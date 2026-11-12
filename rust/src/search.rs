use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::Lossy;
use grep_searcher::Searcher;
use rayon::prelude::*;
use tantivy::collector::DocSetCollector;
use tantivy::query::EnableScoring;
use tantivy::query::{AllQuery, BooleanQuery, Occur, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{Executor, IndexReader, TantivyDocument, Term};

use crate::index::{FIELD_BODY, FIELD_PATH};
use crate::query::{Prefilter, Query};
use crate::tokenizer::query_trigrams;
use crate::types::SearchResult;

/// Cap on snippet bytes sent to Lua. Long lines (minified bundles, lockfiles)
/// crater the picker because snacks runs treesitter on every visible snippet.
/// Ripgrep applies the same cap via --max-columns=500 --max-columns-preview.
const MAX_SNIPPET: usize = 500;

pub struct SearchParams<'a> {
    pub reader: &'a IndexReader,
    pub schema: &'a Schema,
    pub project_root: &'a Path,
    pub query_str: &'a str,
    pub cancelled: &'a Arc<AtomicBool>,
    pub executor: &'a Executor,
}

pub fn search_streaming<F>(
    params: &SearchParams,
    limit: usize,
    mut on_batch: F,
) -> Result<(), String>
where
    F: FnMut(Vec<SearchResult>),
{
    let Some(query) = Query::parse(params.query_str, params.project_root)? else {
        return Ok(());
    };

    let body_field = params.schema.get_field(FIELD_BODY).unwrap();
    let path_field = params.schema.get_field(FIELD_PATH).unwrap();
    // No filename field: a filename is a suffix of its path.
    let tantivy_query = to_tantivy(&query.prefilter, &[body_field, path_field]);

    let searcher = params.reader.searcher();
    if searcher.num_docs() == 0 {
        return Ok(());
    }

    let candidates = searcher
        .search_with_executor(
            &tantivy_query,
            &DocSetCollector,
            params.executor,
            EnableScoring::disabled_from_searcher(&searcher),
        )
        .map_err(|e| format!("Search failed: {}", e))?;

    if params.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }

    let total_emitted = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<SearchResult>>(64);

    let cancelled = Arc::clone(params.cancelled);
    let total_clone = Arc::clone(&total_emitted);
    let project_root = params.project_root.to_path_buf();

    let scan_handle = std::thread::Builder::new()
        .name("sakuin-scan".into())
        .spawn(move || {
            candidates
                .into_par_iter()
                .for_each_init(Searcher::new, |grep, doc_address| {
                    if cancelled.load(Ordering::Relaxed)
                        || total_clone.load(Ordering::Relaxed) >= limit
                    {
                        return;
                    }

                    let doc: TantivyDocument = match searcher.doc(doc_address) {
                        Ok(d) => d,
                        Err(_) => return,
                    };
                    let rel_path = match doc.get_first(path_field).and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => return,
                    };
                    if !query.path_allowed(&rel_path) {
                        return;
                    }
                    let abs_path = project_root.join(&rel_path);

                    let mut results =
                        find_matching_lines(grep, &abs_path, &rel_path, &query.matcher, &cancelled);

                    if results.is_empty() {
                        if !abs_path.exists()
                            || !query.matcher.is_match(rel_path.as_bytes()).unwrap_or(false)
                        {
                            return;
                        }
                        results.push(SearchResult {
                            path: rel_path,
                            line: 0,
                            col: 0,
                            snippet: String::new(),
                            score: 0.0,
                            matches: Vec::new(),
                        });
                    } else {
                        let density = (results.len() as f32).log2().max(0.0) / 10.0;
                        results.iter_mut().for_each(|r| r.score += density);
                    }
                    total_clone.fetch_add(results.len(), Ordering::Relaxed);
                    let _ = tx.send(results);
                });
        })
        .expect("failed to spawn scan thread");

    for batch in rx {
        if params.cancelled.load(Ordering::Relaxed)
            || total_emitted.load(Ordering::Relaxed) >= limit
        {
            break;
        }
        on_batch(batch);
    }

    let _ = scan_handle.join();

    Ok(())
}

fn to_tantivy(p: &Prefilter, fields: &[Field]) -> Box<dyn tantivy::query::Query> {
    let all = |occur: Occur, ps: &[Prefilter]| -> Box<dyn tantivy::query::Query> {
        Box::new(BooleanQuery::new(
            ps.iter().map(|p| (occur, to_tantivy(p, fields))).collect(),
        ))
    };
    match p {
        Prefilter::All => Box::new(AllQuery),
        Prefilter::And(ps) => all(Occur::Must, ps),
        Prefilter::Or(ps) => all(Occur::Should, ps),
        // Every trigram must appear, each in either the body or the path.
        Prefilter::Literal(s) => Box::new(BooleanQuery::new(
            query_trigrams(s)
                .iter()
                .map(|tg| {
                    let per_field = fields
                        .iter()
                        .map(|&f| {
                            let term = Term::from_field_text(f, tg);
                            let q: Box<dyn tantivy::query::Query> =
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                            (Occur::Should, q)
                        })
                        .collect();
                    let q: Box<dyn tantivy::query::Query> = Box::new(BooleanQuery::new(per_field));
                    (Occur::Must, q)
                })
                .collect(),
        )),
    }
}

fn find_matching_lines(
    grep: &mut Searcher,
    file_path: &Path,
    rel_path: &str,
    matcher: &RegexMatcher,
    cancelled: &AtomicBool,
) -> Vec<SearchResult> {
    let mut hits = Vec::new();
    let mut found = Vec::new();
    let sink = Lossy(|line_num, line: &str| {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(false);
        }
        found.clear();
        let _ = matcher.find_iter(line.as_bytes(), |m| {
            found.push((m.start(), m.end()));
            true
        });
        hits.push(line_hit(rel_path, line_num as u32, line, &found));
        Ok(true)
    });
    let _ = grep.search_path(matcher, file_path, sink);
    hits
}

/// Builds the trimmed, capped snippet and maps match ranges into it. `col`
/// points at the first visible match, so `col - 1 - matches[0].0` is the
/// leading whitespace the picker needs to map snippet ranges back to the line.
fn line_hit(path: &str, line: u32, text: &str, found: &[(usize, usize)]) -> SearchResult {
    let lead = text.len() - text.trim_start().len();
    let trimmed = text.trim();
    let mut end = trimmed.len().min(MAX_SNIPPET);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let snippet = if end < trimmed.len() {
        format!("{} …", &trimmed[..end])
    } else {
        trimmed.to_string()
    };
    let matches: Vec<(u32, u32)> = found
        .iter()
        .map(|&(s, e)| (s.max(lead) - lead, e.saturating_sub(lead).min(end)))
        .filter(|(s, e)| s < e)
        .map(|(s, e)| (s as u32, e as u32))
        .collect();
    let col = match matches.first() {
        Some(&(s, _)) => s as usize + lead,
        None => found.first().map_or(0, |&(s, _)| s),
    } + 1;
    let matched: usize = found.iter().map(|(s, e)| e - s).sum();
    SearchResult {
        path: path.to_string(),
        line,
        col: col as u32,
        score: (matched as f32 / snippet.len().max(1) as f32).min(1.0),
        snippet,
        matches,
    }
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
        let executor = Executor::single_thread();
        let mut results = Vec::new();
        search_streaming(
            &SearchParams {
                reader,
                schema,
                project_root,
                query_str: query,
                cancelled: &cancelled,
                executor: &executor,
            },
            usize::MAX,
            |b| results.extend(b),
        )
        .unwrap();
        results
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
    fn test_smart_case() {
        let (reader, schema, dir) = setup_test_index(&[("lower.rs", "let production = true;\n")]);

        assert!(!do_search(&reader, &schema, dir.path(), "prod").is_empty());
        assert!(
            do_search(&reader, &schema, dir.path(), "PROD").is_empty(),
            "uppercase query is case-sensitive, like rg --smart-case"
        );
        assert!(!do_search(&reader, &schema, dir.path(), "PROD -- -i").is_empty());
    }

    #[test]
    fn test_regex_and_flags() {
        let (reader, schema, dir) = setup_test_index(&[
            ("a.rs", "fn load_config() {}\nfn save() {}\n"),
            ("b.py", "def load_config(): pass\n"),
            ("c.rs", "let a.b = 1;\nlet axb = 2;\n"),
        ]);
        let lines = |q: &str| {
            let mut v: Vec<_> = do_search(&reader, &schema, dir.path(), q)
                .into_iter()
                .map(|r| format!("{}:{}", r.path, r.line))
                .collect();
            v.sort();
            v
        };
        assert_eq!(lines(r"fn \w+\(\)"), ["a.rs:1", "a.rs:2"]);
        assert_eq!(lines("load_config|save"), ["a.rs:1", "a.rs:2", "b.py:1"]);
        assert_eq!(lines("load_config -- -g *.py"), ["b.py:1"]);
        assert_eq!(lines("load_config -- -t rust"), ["a.rs:1"]);
        assert_eq!(lines("a.b"), ["c.rs:1", "c.rs:2"]);
        assert_eq!(lines("a.b -- -F"), ["c.rs:1"]);
        assert_eq!(lines("load -- -w"), Vec::<String>::new());
    }

    #[test]
    fn test_match_ranges_and_col() {
        let (reader, schema, dir) = setup_test_index(&[("x.rs", "    foo bar foo\n")]);
        let r = &do_search(&reader, &schema, dir.path(), "foo")[0];
        assert_eq!(r.snippet, "foo bar foo");
        assert_eq!(r.matches, [(0, 3), (8, 11)]);
        assert_eq!(r.col, 5);

        // Match starting in trimmed indentation: col and ranges stay consistent.
        let r = &do_search(&reader, &schema, dir.path(), r"^\s+foo")[0];
        assert_eq!(r.matches, [(0, 3)]);
        assert_eq!(r.col, 5);
    }

    #[test]
    fn test_prefilter_never_drops_a_match() {
        use grep_matcher::Matcher;
        let files = [
            ("a.rs", "fn thread_pool() -> ThreadPool {}\n"),
            ("b.rs", "let x = foo.bar(baz);\nstd::vector<int> v;\n"),
            ("c.rs", "HELLO world\nhello_world\n"),
            ("d.rs", "abc123 xyz\n"),
        ];
        let (reader, schema, dir) = setup_test_index(&files);
        for q in [
            "thread_?pool",
            "(?i)threadpool",
            "foo.*baz",
            "vector<\\w+>",
            "hello|xyz",
            "Hello -- -i",
            "[a-c]{3}\\d+",
            "wor(ld)+",
            "(foo|std)::?\\w",
            "o_w -- -F",
        ] {
            let m = crate::query::Query::parse(q, dir.path())
                .unwrap()
                .unwrap()
                .matcher;
            let mut want: Vec<_> = files
                .iter()
                .filter(|(_, body)| body.lines().any(|l| m.is_match(l.as_bytes()).unwrap()))
                .map(|(p, _)| p.to_string())
                .collect();
            let mut got: Vec<_> = do_search(&reader, &schema, dir.path(), q)
                .into_iter()
                .filter(|r| r.line > 0)
                .map(|r| r.path)
                .collect();
            want.sort();
            got.sort();
            got.dedup();
            assert_eq!(got, want, "query {q:?}");
        }
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
        let best = results
            .iter()
            .max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        assert_eq!(best.snippet, "ffi::CString");
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
    fn test_rare_substring_survives_bm25_candidate_cap() {
        // Regression: the picker passes a non-MAX limit; capping `TopDocs` by
        // that limit drops rare-substring docs (one `return "Resumable"`) under
        // BM25 noise (many `return Result::Ok(...)`) before line scanning.
        let mut files = vec![("needle.rs".into(), "return \"Resumable\";\n".into())];
        for i in 0..50 {
            files.push((
                format!("noise_{i}.rs"),
                "return Result::Ok(());\n".repeat(200),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c): &(String, String)| (p.as_str(), c.as_str()))
            .collect();
        let (reader, schema, dir) = setup_test_index(&refs);

        let cancelled = Arc::new(AtomicBool::new(false));
        let executor = Executor::single_thread();
        let mut results = Vec::new();
        search_streaming(
            &SearchParams {
                reader: &reader,
                schema: &schema,
                project_root: dir.path(),
                query_str: "return \"Resu",
                cancelled: &cancelled,
                executor: &executor,
            },
            30, // < noise count, so a limit-bounded TopDocs would drop the needle
            |b| results.extend(b),
        )
        .unwrap();

        assert!(
            results.iter().any(|r| r.path == "needle.rs" && r.line != 0),
            "needle.rs missing from {:?}",
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
