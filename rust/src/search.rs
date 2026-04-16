use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use tantivy::collector::TopDocs;
use tantivy::query::EnableScoring;
use tantivy::query::{AllQuery, BooleanQuery, Occur, RegexQuery};
use tantivy::schema::{Schema, Value};
use tantivy::{Executor, IndexReader, TantivyDocument};

use crate::index::{FIELD_BODY, FIELD_FILENAME, FIELD_PATH};
use crate::types::SearchResult;

pub struct SearchParams<'a> {
    pub reader: &'a IndexReader,
    pub schema: &'a Schema,
    pub project_root: &'a Path,
    pub query_str: &'a str,
    pub cancelled: &'a Arc<AtomicBool>,
    pub executor: &'a Executor,
}

pub fn search(
    reader: &IndexReader,
    schema: &Schema,
    project_root: &Path,
    query_str: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<SearchResult>, String> {
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let executor = Executor::multi_thread(num_threads, "sakuin-search-")
        .unwrap_or_else(|_| Executor::single_thread());
    let params = SearchParams {
        reader,
        schema,
        project_root,
        query_str,
        cancelled,
        executor: &executor,
    };
    let mut all_results = Vec::new();
    search_streaming(&params, usize::MAX, usize::MAX, |batch| {
        all_results.extend(batch);
    })?;
    Ok(all_results)
}

pub fn search_streaming<F>(
    params: &SearchParams,
    batch_size: usize,
    limit: usize,
    mut on_batch: F,
) -> Result<(), String>
where
    F: FnMut(Vec<SearchResult>),
{
    let query_str = params.query_str.trim();
    if query_str.is_empty() {
        return Ok(());
    }

    let needle = query_str.to_lowercase();

    let body_field = params.schema.get_field(FIELD_BODY).unwrap();
    let path_field = params.schema.get_field(FIELD_PATH).unwrap();
    let filename_field = params.schema.get_field(FIELD_FILENAME).unwrap();
    let fields = [body_field, path_field, filename_field];

    // Tantivy candidate retrieval: chunks must all appear in the document (AND).
    // Pure-punctuation queries fall back to AllQuery for line-level scanning.
    let chunks = alphanumeric_chunks(&needle);
    let tantivy_query: Box<dyn tantivy::query::Query> = if chunks.is_empty() {
        Box::new(AllQuery)
    } else {
        let per_chunk: Vec<(Occur, Box<dyn tantivy::query::Query>)> = chunks
            .iter()
            .filter_map(|chunk| {
                let pattern = format!(".*{}.*", regex_escape(chunk));
                let field_qs: Vec<(Occur, Box<dyn tantivy::query::Query>)> = fields
                    .iter()
                    .filter_map(|&f| {
                        RegexQuery::from_pattern(&pattern, f).ok().map(|rq| {
                            (
                                Occur::Should,
                                Box::new(rq) as Box<dyn tantivy::query::Query>,
                            )
                        })
                    })
                    .collect();
                if field_qs.is_empty() {
                    None
                } else {
                    Some((
                        Occur::Must,
                        Box::new(BooleanQuery::new(field_qs)) as Box<dyn tantivy::query::Query>,
                    ))
                }
            })
            .collect();
        Box::new(BooleanQuery::new(per_chunk))
    };

    let searcher = params.reader.searcher();
    let num_docs = searcher.num_docs() as usize;
    if num_docs == 0 {
        return Ok(());
    }
    let top_docs = searcher
        .search_with_executor(
            &tantivy_query,
            &TopDocs::with_limit(num_docs).order_by_score(),
            params.executor,
            EnableScoring::enabled_from_statistics_provider(&searcher, &searcher),
        )
        .map_err(|e| format!("Search failed: {}", e))?;

    if params.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    let doc_infos: Vec<_> = top_docs
        .into_iter()
        .filter_map(|(_score, doc_address)| {
            let doc: TantivyDocument = searcher.doc(doc_address).ok()?;
            let rel_path = doc.get_first(path_field)?.as_str()?.to_string();
            let abs_path = params.project_root.join(&rel_path);
            Some((rel_path, abs_path))
        })
        .collect();

    let total_emitted = Arc::new(AtomicUsize::new(0));

    let mut results: Vec<_> = doc_infos
        .par_iter()
        .flat_map_iter(|(rel_path, abs_path)| {
            if params.cancelled.load(Ordering::Relaxed)
                || total_emitted.load(Ordering::Relaxed) >= limit
            {
                return Vec::new();
            }

            let matches = find_matching_lines(abs_path, &needle, params.cancelled);

            if matches.is_empty() {
                if !abs_path.exists() {
                    return vec![];
                }
                if rel_path.to_lowercase().contains(needle.as_str()) {
                    total_emitted.fetch_add(1, Ordering::Relaxed);
                    vec![SearchResult {
                        path: rel_path.clone(),
                        line: 0,
                        col: 0,
                        snippet: String::new(),
                        score: 0.0,
                    }]
                } else {
                    vec![]
                }
            } else {
                let query_len = needle.len() as f32;
                let density = (matches.len() as f32).log2().max(0.0) / 10.0;
                let out: Vec<_> = matches
                    .into_iter()
                    .map(|(line, col, snippet)| SearchResult {
                        score: (query_len / snippet.len().max(1) as f32).min(1.0) + density,
                        path: rel_path.clone(),
                        line,
                        col,
                        snippet,
                    })
                    .collect();
                total_emitted.fetch_add(out.len(), Ordering::Relaxed);
                out
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

fn find_matching_lines(
    file_path: &Path,
    needle: &str,
    cancelled: &Arc<AtomicBool>,
) -> Vec<(u32, u32, String)> {
    let contents = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut matches = Vec::new();

    for (line_idx, line) in contents.lines().enumerate() {
        if line_idx & 0x1FF == 0 && cancelled.load(Ordering::Relaxed) {
            return matches;
        }
        // Lowercase per line: avoids allocating a second copy of the entire file.
        if let Some(col) = line.to_lowercase().find(needle) {
            matches.push((
                (line_idx + 1) as u32,
                (col + 1) as u32,
                line.trim().to_string(),
            ));
        }
    }

    matches
}

/// Split a string into contiguous alphanumeric chunks (for Tantivy index lookups).
fn alphanumeric_chunks(s: &str) -> Vec<String> {
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

/// Escape characters that are special in Tantivy's regex syntax.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tempfile::TempDir;

    use crate::index::{create_reader, create_writer, index_file, open_or_create_index};

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

    #[test]
    fn test_literal_match_exact() {
        let (reader, schema, dir) =
            setup_test_index(&[("config.rs", "let env = ProductionConfig::new();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "ProductionConfig");
        assert!(!results.is_empty());
        assert_eq!(results[0].path, "config.rs");
    }

    #[test]
    fn test_literal_match_substring() {
        let (reader, schema, dir) =
            setup_test_index(&[("config.rs", "let env = ProductionConfig::new();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "Prod");
        assert!(
            !results.is_empty(),
            "Expected 'Prod' to match 'ProductionConfig'"
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
        assert!(results.is_empty());
    }

    #[test]
    fn test_threadpool_does_not_match_separate_words() {
        let (reader, schema, dir) = setup_test_index(&[(
            "separate.rs",
            "let thread = spawn();\nlet pool = create();\n",
        )]);

        let results = do_search(&reader, &schema, dir.path(), "threadPool");
        assert!(
            results.is_empty(),
            "Expected 'threadPool' to NOT match separate 'thread' and 'pool'"
        );
    }

    #[test]
    fn test_threadpool_matches_contiguous() {
        let (reader, schema, dir) =
            setup_test_index(&[("pool.rs", "let tp = threadPool::new(4);\n")]);

        let results = do_search(&reader, &schema, dir.path(), "threadPool");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_snake_case_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("pool.rs", "let thread_pool = ThreadPool::new(4);\n")]);

        let results = do_search(&reader, &schema, dir.path(), "thread_pool");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_screaming_snake_case_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("consts.rs", "const MAX_FILE_SIZE: usize = 1024;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "MAX_FILE_SIZE");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_phrase_matches_exact_adjacent_words() {
        // "asynchronous programming" must match only lines containing those words adjacent.
        let (reader, schema, dir) = setup_test_index(&[(
            "docs.rs",
            "// asynchronous programming is fun\nlet asynchronous = 1;\nlet programming = 2;\n",
        )]);

        let results = do_search(&reader, &schema, dir.path(), "asynchronous programming");
        assert_eq!(results.len(), 1, "Expected exactly 1 result for the phrase");
        assert!(results[0].snippet.contains("asynchronous programming"));
    }

    #[test]
    fn test_phrase_does_not_match_words_on_separate_lines() {
        // Words present in the file but never adjacent on the same line.
        let (reader, schema, dir) = setup_test_index(&[(
            "separate.rs",
            "let asynchronous = 1;\nlet programming = 2;\n",
        )]);

        let results = do_search(&reader, &schema, dir.path(), "asynchronous programming");
        assert!(
            results.is_empty(),
            "Phrase should not match words on separate lines"
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
            "Expected 'production' to match the file path"
        );
    }

    #[test]
    fn test_angle_bracket_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("math.rs", "fn normalize(v: Vec3<f32>) -> Vec3<f32> {}\n")]);

        let results = do_search(&reader, &schema, dir.path(), "Vec3");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_double_colon_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("main.cpp", "#include <vector>\nstd::vector<int> v;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "std::vector");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_trailing_double_colon_query() {
        let (reader, schema, dir) =
            setup_test_index(&[("lib.rs", "use std::ffi::CString;\nlet ffi = something();\n")]);

        let results = do_search(&reader, &schema, dir.path(), "ffi::");
        assert!(
            !results.is_empty(),
            "Expected 'ffi::' to match line containing 'ffi::CString'"
        );
        assert_eq!(results.len(), 1, "Expected exactly 1 result for 'ffi::'");
        assert!(results[0].snippet.contains("ffi::CString"));
    }

    #[test]
    fn test_trailing_punctuation_does_not_match_bare_word() {
        let (reader, schema, dir) = setup_test_index(&[("bare.rs", "let foo = 42;\n")]);

        let results = do_search(&reader, &schema, dir.path(), "foo::");
        assert!(
            results.is_empty(),
            "Expected 'foo::' to NOT match bare 'foo'"
        );
    }

    #[test]
    fn test_exact_match_scores_highest() {
        let (reader, schema, dir) = setup_test_index(&[(
            "mixed.rs",
            concat!(
                "use std::ffi::CString;\n",
                "ffi::CString\n",
                "let x = some_very_long_thing(ffi::CString, other);\n",
            ),
        )]);

        let results = do_search(&reader, &schema, dir.path(), "ffi::CString");
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].snippet, "ffi::CString");
    }

    #[test]
    fn test_deleted_file_not_in_results() {
        let (reader, schema, dir) = setup_test_index(&[
            ("src/config.rs", "let production = true;\n"),
            ("src/utils.rs", "fn helper() {}\n"),
        ]);

        std::fs::remove_file(dir.path().join("src/config.rs")).unwrap();

        let results = do_search(&reader, &schema, dir.path(), "config");
        assert!(
            results.is_empty(),
            "Expected no results for a deleted file, got: {:?}",
            results.iter().map(|r| &r.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_pure_punctuation_query() {
        let (reader, schema, dir) = setup_test_index(&[
            ("main.rs", "use std::collections::HashMap;\nlet x = 42;\n"),
            ("lib.rs", "fn helper() {}\n"),
        ]);

        let results = do_search(&reader, &schema, dir.path(), "::");
        assert!(
            !results.is_empty(),
            "Expected '::' to match lines containing '::'"
        );
        assert_eq!(results[0].path, "main.rs");
        assert!(results[0].snippet.contains("::"));
        assert!(results.iter().all(|r| r.path == "main.rs"));
    }
}
