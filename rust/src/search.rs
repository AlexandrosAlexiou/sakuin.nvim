use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, RegexQuery};
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

/// Known Tantivy field names used in this schema.
const KNOWN_FIELDS: &[&str] = &["body", "path", "path_exact", "filename", "extension"];

/// Check whether the query uses advanced Tantivy syntax that should be handled
/// by the built-in `QueryParser` rather than our literal matching.
fn is_advanced_query(query_str: &str) -> bool {
    let trimmed = query_str.trim();

    // Phrase query: contains double-quote
    if trimmed.contains('"') {
        return true;
    }

    for token in trimmed.split_whitespace() {
        let upper = token.to_uppercase();
        // Boolean operators — must be standalone tokens
        if matches!(upper.as_str(), "AND" | "OR" | "NOT") {
            return true;
        }
        // Leading +/- operators (Tantivy boost/exclude)
        if (token.starts_with('+') || token.starts_with('-'))
            && token.len() > 1
            && token[1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric())
        {
            return true;
        }
        // Field prefix — only if the part before the colon is a known field
        if let Some(idx) = token.find(':') {
            let prefix = &token[..idx];
            if KNOWN_FIELDS.iter().any(|&f| prefix.eq_ignore_ascii_case(f)) {
                return true;
            }
        }
    }

    false
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
        // Single chunk — just a direct substring query
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

    // Deduplicate while preserving order
    let mut seen = std::collections::HashSet::new();
    chunks.retain(|c| {
        let key = c.to_lowercase();
        seen.insert(key)
    });

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

/// Score boost for lines containing the exact literal query term (case-insensitive).
const EXACT_MATCH_BOOST: f32 = 1_000_000.0;

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

    let query: Box<dyn tantivy::query::Query> = if is_advanced_query(params.query_str) {
        let query_parser = QueryParser::for_index(
            params.reader.searcher().index(),
            vec![body_field, path_field, filename_field],
        );
        query_parser
            .parse_query(params.query_str)
            .map_err(|e| format!("Failed to parse query '{}': {}", params.query_str, e))?
    } else {
        // Simple query: each whitespace-separated term is matched as a literal
        // substring against indexed tokens. No sub-word decomposition.
        //
        // For a query like "threadPool", we search for the regex .*threadpool.*
        // against the body, path, and filename fields. All terms must match (AND).
        let terms = extract_literal_terms(params.query_str);
        if terms.is_empty() {
            return Ok(());
        }

        let term_queries: Vec<(Occur, Box<dyn tantivy::query::Query>)> = terms
            .iter()
            .map(|t| build_query_for_term(t, &default_fields).map(|q| (Occur::Must, q)))
            .collect::<Result<Vec<_>, _>>()?;

        Box::new(BooleanQuery::new(term_queries))
    };

    let searcher = params.reader.searcher();

    let num_docs = searcher.num_docs() as usize;
    let top_docs = if num_docs == 0 {
        Vec::new()
    } else {
        searcher
            .search(&query, &TopDocs::with_limit(num_docs))
            .map_err(|e| format!("Search failed: {}", e))?
    };

    if params.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    // The literal terms (whitespace-separated) for line-level matching.
    // We also pre-compute lowercased alphanumeric-only versions for fallback.
    let literal_terms: Vec<String> = extract_literal_terms(params.query_str)
        .into_iter()
        .map(|t| t.to_lowercase())
        .collect();

    if literal_terms.is_empty() {
        return Ok(());
    }

    let search_terms: Arc<Vec<SearchTerm>> = Arc::new(
        literal_terms
            .into_iter()
            .map(|lit| {
                let alnum: String = lit.chars().filter(|c| c.is_alphanumeric()).collect();
                SearchTerm {
                    literal: lit.clone(),
                    alphanumeric: if alnum != lit && !alnum.is_empty() {
                        Some(alnum)
                    } else {
                        None
                    },
                }
            })
            .collect(),
    );

    // Collect (score, rel_path, abs_path) for all matching documents.
    let doc_infos: Vec<(f32, String, std::path::PathBuf)> = top_docs
        .into_iter()
        .filter_map(|(score, doc_address)| {
            let doc: TantivyDocument = searcher.doc(doc_address).ok()?;
            let rel_path = doc.get_first(path_field)?.as_str()?.to_string();
            let abs_path = params.project_root.join(&rel_path);
            Some((score, rel_path, abs_path))
        })
        .collect();

    let total_emitted = Arc::new(AtomicUsize::new(0));

    // Parallelize: read each file and scan for lines containing the search terms.
    let mut results: Vec<SearchResult> = doc_infos
        .par_iter()
        .flat_map_iter(|(score, rel_path, abs_path)| {
            if params.cancelled.load(Ordering::Relaxed)
                || total_emitted.load(Ordering::Relaxed) >= limit
            {
                return Vec::new();
            }

            let matches = find_matching_lines(abs_path, &search_terms, params.cancelled);

            if matches.is_empty() {
                // No content-line matches — check if path matches any term.
                let path_lower = rel_path.to_lowercase();
                let path_match = search_terms
                    .iter()
                    .any(|t| term_matches_str(t, &path_lower));
                if path_match {
                    total_emitted.fetch_add(1, Ordering::Relaxed);
                    vec![SearchResult {
                        path: rel_path.clone(),
                        line: 0,
                        col: 0,
                        snippet: String::new(),
                        score: *score + EXACT_MATCH_BOOST,
                    }]
                } else {
                    vec![]
                }
            } else {
                let file_results: Vec<SearchResult> = matches
                    .into_iter()
                    .map(|(line, col, snippet)| SearchResult {
                        path: rel_path.clone(),
                        line,
                        col,
                        snippet,
                        score: *score + EXACT_MATCH_BOOST,
                    })
                    .collect();

                total_emitted.fetch_add(file_results.len(), Ordering::Relaxed);
                file_results
            }
        })
        .collect();

    // Sort by score descending.
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Deliver results in batches.
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

/// A pre-processed search term for line-level matching.
struct SearchTerm {
    /// The original user term, lowercased (e.g. `"vec3<>"`).
    literal: String,
    /// Alphanumeric-only version, if different from literal (e.g. `"vec3"`).
    /// Used as fallback for terms containing symbols.
    alphanumeric: Option<String>,
}

/// Check if a search term matches within a lowercased string.
///
/// First tries the full literal. If the literal contains non-alphanumeric chars
/// and didn't match, falls back to matching just the alphanumeric part.
fn term_matches_str(term: &SearchTerm, haystack: &str) -> bool {
    if haystack.contains(term.literal.as_str()) {
        return true;
    }
    if let Some(ref alnum) = term.alphanumeric {
        return haystack.contains(alnum.as_str());
    }
    false
}

/// Find the column position where a term matches in a lowercased string.
fn term_find_in_str(term: &SearchTerm, haystack: &str) -> Option<usize> {
    if let Some(pos) = haystack.find(term.literal.as_str()) {
        return Some(pos);
    }
    if let Some(ref alnum) = term.alphanumeric {
        return haystack.find(alnum.as_str());
    }
    None
}

/// Find all lines in a file that contain at least one of the given search terms.
///
/// A line matches if ANY term appears as a case-insensitive substring.
/// Returns `(line_number, col, snippet)` for each matching line.
fn find_matching_lines(
    file_path: &Path,
    terms: &[SearchTerm],
    cancelled: &Arc<AtomicBool>,
) -> Vec<(u32, u32, String)> {
    let contents = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    if terms.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();

    for (line_idx, line) in contents.lines().enumerate() {
        if line_idx & 0x1FF == 0 && cancelled.load(Ordering::Relaxed) {
            return matches;
        }

        let line_lower = line.to_lowercase();

        // A line matches if ANY term appears on it.
        // Find the earliest matching column across all terms.
        let best_col = terms
            .iter()
            .filter_map(|t| term_find_in_str(t, &line_lower))
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
/// Splits on whitespace, strips field prefixes for known fields, strips
/// quote characters and boolean operators. Each term is preserved as-is
/// (including underscores, colons, angle brackets, etc.) — no sub-word
/// decomposition.
fn extract_literal_terms(query_str: &str) -> Vec<String> {
    let mut terms = Vec::new();

    for token in query_str.split_whitespace() {
        let token = token.trim_matches(|c: char| c == '"' || c == '\'' || c == '(' || c == ')');

        // Skip boolean operators
        if matches!(token.to_uppercase().as_str(), "AND" | "OR" | "NOT") {
            continue;
        }

        // Strip known field prefix (e.g., "body:term" -> "term")
        let term = if let Some(idx) = token.find(':') {
            let prefix = &token[..idx];
            if KNOWN_FIELDS.iter().any(|&f| prefix.eq_ignore_ascii_case(f)) {
                &token[idx + 1..]
            } else {
                token
            }
        } else {
            token
        };

        let term = term.trim_matches(|c: char| c == '"' || c == '\'');
        if !term.is_empty() {
            terms.push(term.to_string());
        }
    }

    terms
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
    // is_advanced_query
    // ------------------------------------------------------------------

    #[test]
    fn test_is_advanced_simple() {
        assert!(!is_advanced_query("prod"));
        assert!(!is_advanced_query("foo bar"));
        assert!(!is_advanced_query("  hello world  "));
    }

    #[test]
    fn test_is_advanced_operators() {
        assert!(is_advanced_query("foo AND bar"));
        assert!(is_advanced_query("foo OR bar"));
        assert!(is_advanced_query("NOT foo"));
    }

    #[test]
    fn test_is_advanced_field_prefix() {
        assert!(is_advanced_query("body:foo"));
        assert!(is_advanced_query("path:bar"));
    }

    #[test]
    fn test_is_advanced_phrase() {
        assert!(is_advanced_query("\"hello world\""));
    }

    #[test]
    fn test_is_advanced_parens() {
        assert!(!is_advanced_query("(foo)"));
    }

    #[test]
    fn test_is_advanced_code_symbols_not_advanced() {
        assert!(!is_advanced_query("Vec3<>"));
        assert!(!is_advanced_query("std::vector"));
        assert!(!is_advanced_query("thread_pool"));
        assert!(!is_advanced_query("foo()"));
        assert!(!is_advanced_query("HashMap<String>"));
    }

    #[test]
    fn test_is_advanced_plus_minus() {
        assert!(is_advanced_query("+foo"));
        assert!(is_advanced_query("-bar"));
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
    fn test_extract_terms_strips_operators() {
        assert_eq!(extract_literal_terms("foo AND bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn test_extract_terms_strips_field_prefix() {
        assert_eq!(extract_literal_terms("body:hello"), vec!["hello"]);
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
        // threadPool stays as one term — no decomposition
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
}
