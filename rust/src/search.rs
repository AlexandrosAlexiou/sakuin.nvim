use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, RegexQuery};
use tantivy::schema::{Field, Schema, Value};
use tantivy::{IndexReader, TantivyDocument};

use crate::index::{FIELD_BODY, FIELD_FILENAME, FIELD_PATH};
use crate::types::SearchResult;

/// Bundles the common, always-required parameters for a search call.
pub struct SearchParams<'a> {
    pub reader: &'a IndexReader,
    pub schema: &'a Schema,
    pub project_root: &'a Path,
    pub query_str: &'a str,
    pub cancelled: &'a Arc<AtomicBool>,
}

/// Build a substring-match query for a single literal chunk across multiple fields.
///
/// Creates a `RegexQuery` with pattern `.*<chunk>.*` for each field so that
/// the chunk matches anywhere inside an indexed token. The per-field queries
/// are combined with `Occur::Should` (OR).
fn build_substring_query_for_chunk(
    chunk: &str,
    fields: &[Field],
) -> Result<Box<dyn tantivy::query::Query>, String> {
    let escaped = regex_escape(&chunk.to_lowercase());
    let pattern = format!(".*{}.*", escaped);

    let field_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = fields
        .iter()
        .filter_map(|&field| {
            RegexQuery::from_pattern(&pattern, field).ok().map(|rq| {
                (
                    Occur::Should,
                    Box::new(rq) as Box<dyn tantivy::query::Query>,
                )
            })
        })
        .collect();

    if field_queries.is_empty() {
        return Err(format!("Failed to build regex query for chunk '{}'", chunk));
    }

    Ok(Box::new(BooleanQuery::new(field_queries)))
}

/// Build a query for a single user term.
///
/// The term is split into alphanumeric chunks (matching how Tantivy's default
/// tokenizer works). Each chunk must match (AND). This handles terms like
/// `thread_pool` (→ chunks `thread`, `pool`) and `std::vector` (→ `std`, `vector`).
///
/// For terms that are purely alphanumeric (e.g. `threadPool`), the whole term
/// is a single chunk and searched as a literal substring.
fn build_query_for_term(
    term: &str,
    fields: &[Field],
) -> Result<Box<dyn tantivy::query::Query>, String> {
    let chunks = split_alphanumeric(term);
    if chunks.is_empty() {
        return Err(format!("No alphanumeric content in term '{}'", term));
    }

    if chunks.len() == 1 {
        return build_substring_query_for_chunk(&chunks[0], fields);
    }

    // Multiple chunks — all must match (AND semantics) for document retrieval.
    // Line-level matching will later require the full literal term.
    let chunk_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = chunks
        .iter()
        .map(|c| build_substring_query_for_chunk(c, fields).map(|q| (Occur::Must, q)))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Box::new(BooleanQuery::new(chunk_queries)))
}

/// Split a string into contiguous alphanumeric chunks.
///
/// `"thread_pool"` → `["thread", "pool"]`
/// `"std::vector"` → `["std", "vector"]`
/// `"threadPool"` → `["threadPool"]`
/// `"Vec3<>"` → `["Vec3"]`
fn split_alphanumeric(s: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for c in s.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

/// Escape characters that are special in Tantivy's regex syntax (tantivy-fst).
fn regex_escape(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len() + 8);
    for c in literal.chars() {
        match c {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            | '<' | '>' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Score for path-only matches (no content line). Ranks below any content
/// match (which scores > 0 based on query-to-line ratio and file density).
const PATH_MATCH_SCORE: f32 = 0.0;

/// Search the index and return results with file path, line number, and snippet.
///
/// Each matching line within a file produces a separate result entry.
///
/// For simple queries, each whitespace-separated term is matched as a **literal
/// substring** — `threadPool` finds lines containing `threadPool`, not lines
/// with just `thread` or `pool` separately. This matches VS Code / Chromium
/// search behavior.
pub fn search(
    reader: &IndexReader,
    schema: &Schema,
    project_root: &Path,
    query_str: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<SearchResult>, String> {
    let params = SearchParams {
        reader,
        schema,
        project_root,
        query_str,
        cancelled,
    };
    let mut all_results = Vec::new();
    search_streaming(&params, usize::MAX, usize::MAX, |batch| {
        all_results.extend(batch);
    })?;
    Ok(all_results)
}

/// Streaming search: calls `on_batch` with batches of results as they are produced.
///
/// - `batch_size`: number of results to accumulate before calling `on_batch`.
/// - `limit`: maximum total results to produce.
/// - `on_batch`: called with each batch of results. May be called multiple times.
pub fn search_streaming<F>(
    params: &SearchParams,
    batch_size: usize,
    limit: usize,
    mut on_batch: F,
) -> Result<(), String>
where
    F: FnMut(Vec<SearchResult>),
{
    if params.query_str.trim().is_empty() {
        return Ok(());
    }

    let body_field = params.schema.get_field(FIELD_BODY).unwrap();
    let path_field = params.schema.get_field(FIELD_PATH).unwrap();
    let filename_field = params.schema.get_field(FIELD_FILENAME).unwrap();
    let default_fields = [body_field, path_field, filename_field];

    let terms = extract_literal_terms(params.query_str);
    if terms.is_empty() {
        return Ok(());
    }

    // Lowercased terms for line-level literal matching.
    let search_terms: Arc<Vec<String>> = Arc::new(terms.iter().map(|t| t.to_lowercase()).collect());

    // Build Tantivy query for candidate document retrieval.
    // Terms are split into alphanumeric chunks to match how Tantivy's
    // tokenizer indexes text (punctuation is stripped during indexing).
    // Terms that are purely punctuation (e.g. "::") have no alphanumeric
    // chunks and cannot be queried via the index — skip them here and let
    // line-level literal matching handle them.
    let term_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = terms
        .iter()
        .filter_map(|t| {
            build_query_for_term(t, &default_fields)
                .ok()
                .map(|q| (Occur::Must, q))
        })
        .collect();

    // If no terms produced an index query (all were pure punctuation),
    // fall back to matching every document so that line-level matching
    // can still find them.
    let query: Box<dyn tantivy::query::Query> = if term_queries.is_empty() {
        Box::new(AllQuery)
    } else {
        Box::new(BooleanQuery::new(term_queries))
    };

    let searcher = params.reader.searcher();

    let num_docs = searcher.num_docs() as usize;
    let top_docs = if num_docs == 0 {
        Vec::new()
    } else {
        searcher
            .search(&query, &TopDocs::with_limit(num_docs).order_by_score())
            .map_err(|e| format!("Search failed: {}", e))?
    };

    if params.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    let doc_infos: Vec<(String, std::path::PathBuf)> = top_docs
        .into_iter()
        .filter_map(|(_score, doc_address)| {
            let doc: TantivyDocument = searcher.doc(doc_address).ok()?;
            let rel_path = doc.get_first(path_field)?.as_str()?.to_string();
            let abs_path = params.project_root.join(&rel_path);
            Some((rel_path, abs_path))
        })
        .collect();

    let total_emitted = Arc::new(AtomicUsize::new(0));

    let mut results: Vec<SearchResult> = doc_infos
        .par_iter()
        .flat_map_iter(|(rel_path, abs_path)| {
            if params.cancelled.load(Ordering::Relaxed)
                || total_emitted.load(Ordering::Relaxed) >= limit
            {
                return Vec::new();
            }

            let matches = find_matching_lines(abs_path, &search_terms, params.cancelled);

            if matches.is_empty() {
                // The file may have been deleted from disk but not yet
                // removed from the index. Skip it to avoid ghost results.
                if !abs_path.exists() {
                    return vec![];
                }

                // No content-line matches — check if path matches any term.
                let path_lower = rel_path.to_lowercase();
                let path_match = search_terms.iter().any(|t| path_lower.contains(t.as_str()));
                if path_match {
                    total_emitted.fetch_add(1, Ordering::Relaxed);
                    vec![SearchResult {
                        path: rel_path.clone(),
                        line: 0,
                        col: 0,
                        snippet: String::new(),
                        score: PATH_MATCH_SCORE,
                    }]
                } else {
                    vec![]
                }
            } else {
                // Scoring: rank results so the most "exact" matches appear first.
                //
                // Components (all ≥ 0, combined additively):
                //  1. query_ratio  — how much of the line the query covers.
                //     A line that IS the query scores ~1.0; a 200-char line
                //     where the query is 4 chars scores ~0.02.
                //  2. file_density — log2(match_count) / 10, so a file with
                //     32 matches gets +0.5 and a file with 1 match gets 0.
                //     Keeps file-level signal without drowning per-line signal.
                let query_len: usize = search_terms.iter().map(|t| t.len()).sum();
                let file_density = (matches.len() as f32).log2().max(0.0) / 10.0;

                let file_results: Vec<SearchResult> = matches
                    .into_iter()
                    .map(|(line, col, snippet)| {
                        let line_len = snippet.len().max(1) as f32;
                        let query_ratio = (query_len as f32 / line_len).min(1.0);
                        SearchResult {
                            path: rel_path.clone(),
                            line,
                            col,
                            snippet,
                            score: query_ratio + file_density,
                        }
                    })
                    .collect();

                total_emitted.fetch_add(file_results.len(), Ordering::Relaxed);
                file_results
            }
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut emitted = 0;
    for chunk in results.chunks(batch_size) {
        if params.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let remaining = limit.saturating_sub(emitted);
        if remaining == 0 {
            break;
        }
        let take = chunk.len().min(remaining);
        on_batch(chunk[..take].to_vec());
        emitted += take;
    }

    Ok(())
}

/// Find all lines in a file that contain at least one of the given search terms.
///
/// Each term is matched as a case-insensitive literal substring.
/// Returns `(line_number, col, snippet)` for each matching line.
fn find_matching_lines(
    file_path: &Path,
    terms: &[String],
    cancelled: &Arc<AtomicBool>,
) -> Vec<(u32, u32, String)> {
    let contents = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    if terms.is_empty() {
        return Vec::new();
    }

    // Lowercase the whole file once instead of allocating a new String per line.
    let contents_lower = contents.to_lowercase();
    let mut matches = Vec::new();

    for (line_idx, (line, line_lower)) in contents.lines().zip(contents_lower.lines()).enumerate() {
        if line_idx & 0x1FF == 0 && cancelled.load(Ordering::Relaxed) {
            return matches;
        }

        // A line matches if ANY term appears on it.
        // Find the earliest matching column across all terms.
        let best_col = terms
            .iter()
            .filter_map(|t| line_lower.find(t.as_str()))
            .min();

        if let Some(col) = best_col {
            let snippet = line.trim().to_string();
            matches.push(((line_idx + 1) as u32, (col + 1) as u32, snippet));
        }
    }

    matches
}

/// Extract literal search terms from the query string.
///
/// Splits on whitespace. Each term is preserved as-is (including
/// underscores, colons, angle brackets, etc.).
fn extract_literal_terms(query_str: &str) -> Vec<String> {
    query_str
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::index::{create_reader, create_writer, index_file, open_or_create_index};

    /// Helper: set up an in-memory index with some test files and return
    /// (reader, schema, project_root TempDir).
    fn setup_test_index(files: &[(&str, &str)]) -> (IndexReader, Schema, TempDir) {
        let project_dir = TempDir::new().unwrap();
        let index_dir = project_dir.path().join(".sakuin");
        std::fs::create_dir_all(&index_dir).unwrap();

        let index = open_or_create_index(&index_dir).unwrap();
        let schema = index.schema();
        let mut writer = create_writer(&index).unwrap();

        for (rel_path, contents) in files {
            let file_path = project_dir.path().join(rel_path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&file_path, contents).unwrap();
            index_file(&writer, &schema, project_dir.path(), &file_path).unwrap();
        }

        writer.commit().unwrap();
        let reader = create_reader(&index).unwrap();
        reader.reload().unwrap();

        (reader, schema, project_dir)
    }

    fn do_search(
        reader: &IndexReader,
        schema: &Schema,
        project_root: &Path,
        query: &str,
    ) -> Vec<SearchResult> {
        let cancelled = Arc::new(AtomicBool::new(false));
        search(reader, schema, project_root, query, &cancelled).unwrap()
    }

    // ------------------------------------------------------------------
    // regex_escape
    // ------------------------------------------------------------------

    #[test]
    fn test_regex_escape_plain() {
        assert_eq!(regex_escape("prod"), "prod");
    }

    #[test]
    fn test_regex_escape_special_chars() {
        assert_eq!(regex_escape("a.b"), "a\\.b");
        assert_eq!(regex_escape("a+b"), "a\\+b");
        assert_eq!(regex_escape("a*b"), "a\\*b");
        assert_eq!(regex_escape("(foo)"), "\\(foo\\)");
    }

    #[test]
    fn test_regex_escape_angle_brackets() {
        assert_eq!(regex_escape("Vec3<>"), "Vec3\\<\\>");
        assert_eq!(regex_escape("HashMap<String>"), "HashMap\\<String\\>");
    }

    // ------------------------------------------------------------------
    // extract_literal_terms
    // ------------------------------------------------------------------

    #[test]
    fn test_extract_terms_simple() {
        assert_eq!(extract_literal_terms("foo bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn test_extract_terms_and_is_literal() {
        assert_eq!(
            extract_literal_terms("foo AND bar"),
            vec!["foo", "AND", "bar"]
        );
    }

    #[test]
    fn test_extract_terms_field_prefix_is_literal() {
        assert_eq!(extract_literal_terms("body:hello"), vec!["body:hello"]);
    }

    #[test]
    fn test_extract_terms_preserves_code_colons() {
        assert_eq!(extract_literal_terms("std::vector"), vec!["std::vector"]);
    }

    #[test]
    fn test_extract_terms_preserves_underscores() {
        assert_eq!(extract_literal_terms("thread_pool"), vec!["thread_pool"]);
    }

    #[test]
    fn test_extract_terms_preserves_angle_brackets() {
        assert_eq!(extract_literal_terms("Vec3<>"), vec!["Vec3<>"]);
    }

    #[test]
    fn test_extract_terms_camel_case_preserved() {
        assert_eq!(extract_literal_terms("threadPool"), vec!["threadPool"]);
    }

    // ------------------------------------------------------------------
    // Literal matching integration tests
    // ------------------------------------------------------------------

    #[test]
    fn test_literal_match_exact() {
        let (reader, schema, dir) =
            setup_test_index(&[("config.rs", "let env = ProductionConfig::new();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "ProductionConfig");
        assert!(
            !results.is_empty(),
            "Expected 'ProductionConfig' to match line containing 'ProductionConfig'"
        );
        assert_eq!(results[0].path, "config.rs");
    }

    #[test]
    fn test_literal_match_substring() {
        // "Prod" should match "ProductionConfig" since it's a substring
        let (reader, schema, dir) =
            setup_test_index(&[("config.rs", "let env = ProductionConfig::new();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "Prod");
        assert!(
            !results.is_empty(),
            "Expected 'Prod' to match 'ProductionConfig' via substring"
        );
    }

    #[test]
    fn test_literal_match_case_insensitive() {
        let (reader, schema, dir) = setup_test_index(&[("lower.rs", "let production = true;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "PROD");
        assert!(
            !results.is_empty(),
            "Expected 'PROD' to match 'production' case-insensitively"
        );
    }

    #[test]
    fn test_literal_no_match() {
        let (reader, schema, dir) = setup_test_index(&[("nope.rs", "let staging = true;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "prod");
        assert!(
            results.is_empty(),
            "Expected no results for 'prod' when file only contains 'staging'"
        );
    }

    #[test]
    fn test_threadpool_does_not_match_separate_words() {
        // This is the key test: searching "threadPool" should NOT match a file
        // that only contains "thread" and "pool" as separate words.
        let (reader, schema, dir) = setup_test_index(&[(
            "separate.rs",
            "let thread = spawn();\nlet pool = create();\n",
        )]);

        let results = do_search(&reader, &schema, dir.path(), "threadPool");
        assert!(
            results.is_empty(),
            "Expected 'threadPool' to NOT match file with separate 'thread' and 'pool'"
        );
    }

    #[test]
    fn test_threadpool_matches_contiguous() {
        // "threadPool" should match a line containing "threadPool"
        let (reader, schema, dir) =
            setup_test_index(&[("pool.rs", "let tp = threadPool::new(4);\n")]);

        let results = do_search(&reader, &schema, dir.path(), "threadPool");
        assert!(
            !results.is_empty(),
            "Expected 'threadPool' to match line containing 'threadPool'"
        );
    }

    #[test]
    fn test_snake_case_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("pool.rs", "let thread_pool = ThreadPool::new(4);\n")]);

        let results = do_search(&reader, &schema, dir.path(), "thread_pool");
        assert!(
            !results.is_empty(),
            "Expected 'thread_pool' to match file containing 'thread_pool'"
        );
    }

    #[test]
    fn test_screaming_snake_case_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("consts.rs", "const MAX_FILE_SIZE: usize = 1024;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "MAX_FILE_SIZE");
        assert!(!results.is_empty(), "Expected 'MAX_FILE_SIZE' to match");
    }

    #[test]
    fn test_multi_term_both_must_be_on_line() {
        // "Prod Config" = two terms: both must appear on the SAME line
        let (reader, schema, dir) = setup_test_index(&[
            (
                "both.rs",
                "fn getProductionConfig() {}\nlet staging = false;\n",
            ),
            ("only_prod.rs", "let production = true;\n"),
        ]);

        let results = do_search(&reader, &schema, dir.path(), "Prod Config");
        let paths: Vec<&str> = results.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"both.rs"),
            "Expected both.rs to match both terms on same line"
        );
        assert!(
            !paths.contains(&"only_prod.rs"),
            "Expected only_prod.rs to NOT match (missing 'Config')"
        );
    }

    #[test]
    fn test_empty_query() {
        let (reader, schema, dir) = setup_test_index(&[("a.rs", "hello\n")]);
        let results = do_search(&reader, &schema, dir.path(), "");
        assert!(results.is_empty());
    }

    #[test]
    fn test_whitespace_query() {
        let (reader, schema, dir) = setup_test_index(&[("a.rs", "hello\n")]);
        let results = do_search(&reader, &schema, dir.path(), "   ");
        assert!(results.is_empty());
    }

    #[test]
    fn test_path_substring_match() {
        let (reader, schema, dir) = setup_test_index(&[("src/production/config.rs", "// empty\n")]);

        let results = do_search(&reader, &schema, dir.path(), "production");
        assert!(
            !results.is_empty(),
            "Expected 'production' to match file path containing 'production'"
        );
    }

    #[test]
    fn test_angle_bracket_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("math.rs", "fn normalize(v: Vec3<f32>) -> Vec3<f32> {}\n")]);

        let results = do_search(&reader, &schema, dir.path(), "Vec3");
        assert!(
            !results.is_empty(),
            "Expected 'Vec3' to match file containing 'Vec3<f32>'"
        );
    }

    #[test]
    fn test_double_colon_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("main.cpp", "#include <vector>\nstd::vector<int> v;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "std::vector");
        assert!(
            !results.is_empty(),
            "Expected 'std::vector' to match file containing 'std::vector<int>'"
        );
    }

    #[test]
    fn test_trailing_double_colon_query() {
        // "ffi::" should match lines containing "ffi::" (e.g. "ffi::CString")
        // but should NOT match lines that only contain "ffi" without "::"
        let (reader, schema, dir) =
            setup_test_index(&[("lib.rs", "use std::ffi::CString;\nlet ffi = something();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "ffi::");
        assert!(
            !results.is_empty(),
            "Expected 'ffi::' to match line containing 'ffi::CString'"
        );
        // Should only match the line with "ffi::", not the line with bare "ffi"
        assert_eq!(
            results.len(),
            1,
            "Expected exactly 1 result for 'ffi::' (the line with 'ffi::CString'), got {}",
            results.len()
        );
        assert!(
            results[0].snippet.contains("ffi::CString"),
            "Expected the matching line to contain 'ffi::CString', got: {}",
            results[0].snippet
        );
    }

    #[test]
    fn test_trailing_punctuation_does_not_match_bare_word() {
        // Searching "foo::" should not match a line that only contains "foo"
        let (reader, schema, dir) = setup_test_index(&[("bare.rs", "let foo = 42;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "foo::");
        assert!(
            results.is_empty(),
            "Expected 'foo::' to NOT match line with bare 'foo' (no '::')"
        );
    }

    #[test]
    fn test_exact_match_scores_highest() {
        // A short line that is almost entirely the query should score higher
        // than a long line that merely contains it.
        let (reader, schema, dir) = setup_test_index(&[(
            "mixed.rs",
            concat!(
                "use std::ffi::CString;\n",                             // long line
                "ffi::CString\n",                                       // exact-ish
                "let x = some_very_long_thing(ffi::CString, other);\n", // long line
            ),
        )]);

        let results = do_search(&reader, &schema, dir.path(), "ffi::CString");
        assert!(
            results.len() == 3,
            "Expected 3 results, got {}",
            results.len()
        );
        // The short line "ffi::CString" should be first (highest score).
        assert_eq!(
            results[0].snippet, "ffi::CString",
            "Expected the shortest/most exact match first, got: {}",
            results[0].snippet
        );
    }

    #[test]
    fn test_deleted_file_not_in_results() {
        // A file that has been deleted from disk but is still in the index
        // should not appear in search results.
        let (reader, schema, dir) = setup_test_index(&[
            ("src/config.rs", "let production = true;\n"),
            ("src/utils.rs", "fn helper() {}\n"),
        ]);

        // Delete the file from disk, leaving its document in the index.
        std::fs::remove_file(dir.path().join("src/config.rs")).unwrap();

        // The query matches the path ("config") so without the fix
        // this would return a ghost result via the path-only fallback.
        let results = do_search(&reader, &schema, dir.path(), "config");
        assert!(
            results.is_empty(),
            "Expected no results for a file deleted from disk, got: {:?}",
            results.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_pure_punctuation_query() {
        // Searching "::" should not error and should match lines containing "::".
        let (reader, schema, dir) = setup_test_index(&[
            ("main.rs", "use std::collections::HashMap;\nlet x = 42;\n"),
            ("lib.rs", "fn helper() {}\n"),
        ]);

        let results = do_search(&reader, &schema, dir.path(), "::");
        assert!(
            !results.is_empty(),
            "Expected '::' to match line containing '::'"
        );
        assert_eq!(results[0].path, "main.rs");
        assert!(
            results[0].snippet.contains("::"),
            "Expected snippet to contain '::', got: {}",
            results[0].snippet
        );
        // lib.rs should not appear
        assert!(
            results.iter().all(|r| r.path == "main.rs"),
            "Expected only main.rs in results"
        );
    }
}
