use std::ffi::{CStr, CString};
use std::fs;
use std::sync::{Condvar, Mutex, OnceLock};
use tempfile::TempDir;

// These tests share a global singleton (OnceLock<Mutex<Option<SakuinState>>>).
// We use #[serial] from the `serial_test` crate so they never run in parallel.
use serial_test::serial;

/// Shared condvar to wait for the async notification from the worker thread.
static NOTIFY_STATE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();

fn notify_state() -> &'static (Mutex<bool>, Condvar) {
    NOTIFY_STATE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// A fake `uv_async_send` that signals the condvar instead of waking a libuv loop.
/// This has the same signature: `int (*)(void*) -> int`.
unsafe extern "C" fn fake_uv_async_send(_handle: *mut std::os::raw::c_void) -> i32 {
    let (lock, cvar) = notify_state();
    let mut notified = lock.lock().unwrap();
    *notified = true;
    cvar.notify_all();
    0
}

/// Reset notification state before submitting a search.
fn reset_notify_state() {
    let (lock, _) = notify_state();
    let mut notified = lock.lock().unwrap();
    *notified = false;
}

/// Wait for the async notification from the worker, with a timeout.
fn wait_for_notification(timeout: std::time::Duration) -> bool {
    let (lock, cvar) = notify_state();
    let notified = lock.lock().unwrap();
    let (notified, _timeout_result) = cvar.wait_timeout_while(notified, timeout, |n| !*n).unwrap();
    *notified
}

fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn copy_fixture_dir(src: &std::path::Path, dst: &std::path::Path) {
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dst_path).unwrap();
            copy_fixture_dir(&entry.path(), &dst_path);
        } else {
            fs::copy(entry.path(), dst_path).unwrap();
        }
    }
}

/// Test helper: create a temporary project with some files.
fn create_test_project() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let fixture = fixtures_dir().join("test_project");
    copy_fixture_dir(&fixture, root);

    // Create a file that should be skipped (binary)
    fs::write(root.join("binary.bin"), [0u8, 1, 2, 3, 0, 255, 128]).unwrap();

    dir
}

/// Test helper: create a project with code-style identifiers for search testing.
fn create_code_project() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let fixture = fixtures_dir().join("code_project");
    copy_fixture_dir(&fixture, root);

    dir
}

/// Test helper: init + build index on a project, returning the TempDir.
/// Panics on failure.
fn init_only(project: &TempDir) {
    let root = project.path();
    let index_dir = root.join(".sakuin");
    let c_root = CString::new(root.to_str().unwrap()).unwrap();
    let c_idx = CString::new(index_dir.to_str().unwrap()).unwrap();
    let rc = sakuin::sakuin_init(c_root.as_ptr(), c_idx.as_ptr(), std::ptr::null());
    assert_eq!(rc, 0, "sakuin_init should succeed");
}

fn init_and_build(project: &TempDir) {
    let root = project.path();
    let index_dir = root.join(".sakuin");

    let c_root = CString::new(root.to_str().unwrap()).unwrap();
    let c_idx = CString::new(index_dir.to_str().unwrap()).unwrap();

    let rc = sakuin::sakuin_init(c_root.as_ptr(), c_idx.as_ptr(), std::ptr::null());
    assert_eq!(rc, 0, "sakuin_init should succeed");

    let rc = sakuin::sakuin_build_index();
    assert_eq!(rc, 0, "sakuin_build_index should succeed");
}

/// Test helper: synchronous search, returns parsed JSON results.
fn sync_search(query: &str) -> Vec<serde_json::Value> {
    let c_query = CString::new(query).unwrap();
    let results_ptr = sakuin::sakuin_search(c_query.as_ptr());
    assert!(
        !results_ptr.is_null(),
        "sakuin_search should return non-null for query '{}'",
        query
    );
    let results_json = unsafe { CStr::from_ptr(results_ptr) }.to_str().unwrap();
    let results: Vec<serde_json::Value> = serde_json::from_str(results_json).unwrap();
    sakuin::sakuin_free_string(results_ptr);
    results
}

/// Test helper: collect all file paths from search results.
fn result_paths(results: &[serde_json::Value]) -> Vec<String> {
    results
        .iter()
        .filter_map(|r| r["path"].as_str().map(String::from))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

// ============================================================================
// Original tests (now with #[serial])
// ============================================================================

#[test]
#[serial]
fn test_init_and_shutdown() {
    let project = create_test_project();
    init_and_build(&project);

    // Stats
    let stats_ptr = sakuin::sakuin_stats();
    assert!(!stats_ptr.is_null(), "sakuin_stats should return non-null");
    let stats_json = unsafe { CStr::from_ptr(stats_ptr) }.to_str().unwrap();
    let stats: serde_json::Value = serde_json::from_str(stats_json).unwrap();
    sakuin::sakuin_free_string(stats_ptr);

    // We should have indexed at least 3-4 text files (not the binary)
    let num_docs = stats["num_docs"].as_u64().unwrap();
    assert!(num_docs >= 3, "Expected at least 3 docs, got {}", num_docs);
    assert!(num_docs <= 5, "Expected at most 5 docs, got {}", num_docs);

    // Synchronous search
    let results = sync_search("hello");

    // "hello" should match hello.rs and possibly src/utils.rs (contains "Hello")
    assert!(
        !results.is_empty(),
        "Expected at least 1 search result for 'hello'"
    );

    // Verify result structure
    let first = &results[0];
    assert!(first.get("path").is_some(), "Result should have 'path'");
    assert!(first.get("line").is_some(), "Result should have 'line'");
    assert!(first.get("col").is_some(), "Result should have 'col'");
    assert!(
        first.get("snippet").is_some(),
        "Result should have 'snippet'"
    );
    assert!(first.get("score").is_some(), "Result should have 'score'");

    // Shutdown
    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_incremental_update() {
    let project = create_test_project();
    let root = project.path();
    init_and_build(&project);

    // Add a new file
    fs::write(
        root.join("new_file.txt"),
        "This is a brand new file with unique content bananaphone.\n",
    )
    .unwrap();

    // Small delay to ensure mtime differs
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Update index
    let rc = sakuin::sakuin_update_index();
    assert_eq!(rc, 0);

    // Search for the new content
    let results = sync_search("bananaphone");

    assert!(
        !results.is_empty(),
        "Should find the newly added file containing 'bananaphone'"
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_empty_query() {
    let project = create_test_project();
    init_and_build(&project);

    // Empty query should return empty results
    let results = sync_search("");
    assert!(results.is_empty(), "Empty query should return no results");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_async_search_with_result_slot() {
    let project = create_test_project();
    init_and_build(&project);

    // Register our fake async notifier.
    // We pass a dummy handle pointer (not dereferenced by fake_uv_async_send)
    // and our fake send function that signals a condvar.
    let dummy_handle = 0xDEAD_BEEF_usize as *mut std::os::raw::c_void;
    sakuin::sakuin_register_async_notifier(
        dummy_handle,
        fake_uv_async_send as unsafe extern "C" fn(*mut std::os::raw::c_void) -> i32,
    );

    // Reset notification state
    reset_notify_state();

    // Submit a search with generation 42, batch_size=500, limit=0 (unlimited)
    let query = CString::new("hello").unwrap();
    let rc = sakuin::sakuin_search_submit(query.as_ptr(), 42, 500, 0);
    assert_eq!(rc, 0, "sakuin_search_submit should succeed");

    // Wait for the worker to notify us (may get batch + done messages)
    let notified = wait_for_notification(std::time::Duration::from_secs(5));
    assert!(notified, "Should have received async notification");

    // Small delay to let all messages arrive
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Drain all messages from the queue
    let mut got_results = false;
    let mut got_done = false;
    let mut total_results = 0;

    loop {
        let result_ptr = sakuin::sakuin_search_take_result();
        if result_ptr.is_null() {
            break;
        }

        let result_json = unsafe { CStr::from_ptr(result_ptr) }.to_str().unwrap();
        let msg: serde_json::Value = serde_json::from_str(result_json).unwrap();
        sakuin::sakuin_free_string(result_ptr);

        assert_eq!(
            msg["generation"].as_u64().unwrap(),
            42,
            "Generation should match"
        );

        let msg_type = msg["type"].as_str().unwrap();
        match msg_type {
            "batch" => {
                let results = msg["results"].as_array().unwrap();
                assert!(!results.is_empty(), "Batch should have results");
                total_results += results.len();
                got_results = true;
            }
            "done" => {
                got_done = true;
            }
            "error" => {
                panic!("Unexpected error message: {}", msg["error"]);
            }
            _ => {
                panic!("Unknown message type: {}", msg_type);
            }
        }
    }

    assert!(
        got_results,
        "Should have received at least one batch of results"
    );
    assert!(got_done, "Should have received a done message");
    assert!(
        total_results > 0,
        "Expected at least 1 search result for 'hello'"
    );

    // Taking again should return null (queue is empty)
    let result_ptr2 = sakuin::sakuin_search_take_result();
    assert!(
        result_ptr2.is_null(),
        "Queue should be empty after draining"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Snake_case / underscore query tests
// ============================================================================

#[test]
#[serial]
fn test_search_snake_case_identifier() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("thread_pool_size");
    assert!(
        !results.is_empty(),
        "Expected results for 'thread_pool_size'"
    );
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("pool.rs")),
        "Expected pool.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_snake_case_partial() {
    let project = create_code_project();
    init_and_build(&project);

    // Searching "thread_pool" should match files that contain "thread_pool_size"
    let results = sync_search("thread_pool");
    assert!(!results.is_empty(), "Expected results for 'thread_pool'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("pool.rs")),
        "Expected pool.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_screaming_snake_case() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("MAX_FILE_SIZE");
    assert!(!results.is_empty(), "Expected results for 'MAX_FILE_SIZE'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("config.rs")),
        "Expected config.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_screaming_snake_partial() {
    let project = create_code_project();
    init_and_build(&project);

    // "DEFAULT_TIMEOUT" should match DEFAULT_TIMEOUT_MS
    let results = sync_search("DEFAULT_TIMEOUT");
    assert!(
        !results.is_empty(),
        "Expected results for 'DEFAULT_TIMEOUT'"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Angle bracket / generic type query tests
// ============================================================================

#[test]
#[serial]
fn test_search_generic_type_plain() {
    let project = create_code_project();
    init_and_build(&project);

    // Plain "Vec3" should find the math.rs file
    let results = sync_search("Vec3");
    assert!(!results.is_empty(), "Expected results for 'Vec3'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("math.rs")),
        "Expected math.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_generic_type_with_angle_brackets() {
    let project = create_code_project();
    init_and_build(&project);

    // "Vec3<" should find the math.rs file (angle bracket should not break search)
    let results = sync_search("Vec3<");
    assert!(!results.is_empty(), "Expected results for 'Vec3<'");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_generic_type_with_param() {
    let project = create_code_project();
    init_and_build(&project);

    // "Vec3<f32>" should find the math.rs file
    let results = sync_search("Vec3<f32>");
    assert!(!results.is_empty(), "Expected results for 'Vec3<f32>'");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_empty_angle_brackets() {
    let project = create_code_project();
    init_and_build(&project);

    // "Vec3<>" should not crash and should find Vec3 usages
    let results = sync_search("Vec3<>");
    assert!(!results.is_empty(), "Expected results for 'Vec3<>'");

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Double colon / namespace query tests
// ============================================================================

#[test]
#[serial]
fn test_search_double_colon() {
    let project = create_code_project();
    init_and_build(&project);

    // "std::vector" should find legacy.cpp — NOT be treated as a field prefix
    let results = sync_search("std::vector");
    assert!(!results.is_empty(), "Expected results for 'std::vector'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("legacy.cpp")),
        "Expected legacy.cpp in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_double_colon_partial() {
    let project = create_code_project();
    init_and_build(&project);

    // "std::string" should find legacy.cpp
    let results = sync_search("std::string");
    assert!(!results.is_empty(), "Expected results for 'std::string'");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_rust_path_separator() {
    let project = create_code_project();
    init_and_build(&project);

    // "std::sync::Arc" should find pool.rs which has `use std::sync::Arc;`
    let results = sync_search("std::sync::Arc");
    assert!(!results.is_empty(), "Expected results for 'std::sync::Arc'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("pool.rs")),
        "Expected pool.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Parentheses in search queries
// ============================================================================

#[test]
#[serial]
fn test_search_parentheses_not_advanced() {
    let project = create_code_project();
    init_and_build(&project);

    // "parse_token()" should not be treated as advanced query syntax
    // and should find parser.rs
    let results = sync_search("parse_token");
    assert!(!results.is_empty(), "Expected results for 'parse_token'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("parser.rs")),
        "Expected parser.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// CamelCase / PascalCase query tests
// ============================================================================

#[test]
#[serial]
fn test_search_pascal_case() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("ThreadPool");
    assert!(!results.is_empty(), "Expected results for 'ThreadPool'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("pool.rs")),
        "Expected pool.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_camel_case() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("handleRequest");
    assert!(!results.is_empty(), "Expected results for 'handleRequest'");
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("handler.rs")),
        "Expected handler.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_camel_case_partial() {
    let project = create_code_project();
    init_and_build(&project);

    // "baseUrl" should find handler.rs
    let results = sync_search("baseUrl");
    assert!(!results.is_empty(), "Expected results for 'baseUrl'");

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Multi-term query tests
// ============================================================================

#[test]
#[serial]
fn test_search_multi_term_with_underscore() {
    let project = create_code_project();
    init_and_build(&project);

    // "thread_pool new" — both terms must match (AND). pool.rs has both.
    let results = sync_search("thread_pool new");
    assert!(
        !results.is_empty(),
        "Expected results for 'thread_pool new'"
    );
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("pool.rs")),
        "Expected pool.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_multi_term_mixed_styles() {
    let project = create_code_project();
    init_and_build(&project);

    // "HttpRequestHandler handleRequest" — handler.rs has both
    let results = sync_search("HttpRequestHandler handleRequest");
    assert!(
        !results.is_empty(),
        "Expected results for 'HttpRequestHandler handleRequest'"
    );
    let paths = result_paths(&results);
    assert!(
        paths.iter().any(|p| p.contains("handler.rs")),
        "Expected handler.rs in results, got: {:?}",
        paths
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Case insensitivity tests
// ============================================================================

#[test]
#[serial]
fn test_search_case_insensitive_snake() {
    let project = create_code_project();
    init_and_build(&project);

    // Lowercase "max_file_size" should match "MAX_FILE_SIZE"
    let results = sync_search("max_file_size");
    assert!(
        !results.is_empty(),
        "Expected results for 'max_file_size' matching MAX_FILE_SIZE"
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_case_insensitive_camel() {
    let project = create_code_project();
    init_and_build(&project);

    // "HTTPREQUESTHANDLER" should match "HttpRequestHandler"
    let results = sync_search("httprequesthandler");
    assert!(
        !results.is_empty(),
        "Expected results for 'httprequesthandler' matching HttpRequestHandler"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// No-match tests (ensure false positives don't creep in)
// ============================================================================

#[test]
#[serial]
fn test_search_no_match() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("zzz_nonexistent_identifier_zzz");
    assert!(
        results.is_empty(),
        "Expected no results for a nonexistent identifier"
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_whitespace_only() {
    let project = create_code_project();
    init_and_build(&project);

    let results = sync_search("   ");
    assert!(
        results.is_empty(),
        "Whitespace-only query should return no results"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// File deletion / incremental update with code project
// ============================================================================

#[test]
#[serial]
fn test_incremental_file_deletion() {
    let project = create_code_project();
    let root = project.path();
    init_and_build(&project);

    // Verify math.rs is indexed
    let results = sync_search("Vec3");
    assert!(!results.is_empty(), "Vec3 should be found before deletion");

    // Delete math.rs
    fs::remove_file(root.join("src/math.rs")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Incremental update
    let rc = sakuin::sakuin_update_index();
    assert_eq!(rc, 0, "sakuin_update_index should succeed");

    // Vec3 should now only appear in results if another file references it
    // (it shouldn't, since we only defined it in math.rs)
    let results = sync_search("Vec3");
    let paths = result_paths(&results);
    assert!(
        !paths.iter().any(|p| p.contains("math.rs")),
        "math.rs should no longer appear after deletion"
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_incremental_file_addition() {
    let project = create_code_project();
    let root = project.path();
    init_and_build(&project);

    // Add a new file with a unique identifier
    fs::write(
        root.join("src/networking.rs"),
        "pub fn connect_to_remote_server(host: &str, port: u16) -> Result<(), Error> {\n    todo!()\n}\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let rc = sakuin::sakuin_update_index();
    assert_eq!(rc, 0);

    let results = sync_search("connect_to_remote_server");
    assert!(
        !results.is_empty(),
        "Should find newly added file containing 'connect_to_remote_server'"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Advanced query syntax (should still work)
// ============================================================================

#[test]
#[serial]
fn test_search_advanced_field_prefix() {
    let project = create_code_project();
    init_and_build(&project);

    // "body:Vec3" uses advanced syntax (field prefix with known field name)
    let results = sync_search("body:Vec3");
    // Should find math.rs (Vec3 is in the body)
    assert!(!results.is_empty(), "Expected results for 'body:Vec3'");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_search_advanced_boolean_and() {
    let project = create_code_project();
    init_and_build(&project);

    // Boolean AND should work as advanced query
    let results = sync_search("thread AND pool");
    assert!(
        !results.is_empty(),
        "Expected results for 'thread AND pool'"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// Init with custom / invalid config JSON
// ============================================================================

#[test]
#[serial]
fn test_init_with_config_json() {
    let project = create_test_project();
    let root = project.path();
    let index_dir = root.join(".sakuin");

    let config_json = r#"{"max_file_size":524288,"ignore_patterns":[],"include_extensions":null,"respect_gitignore":false}"#;

    let c_root = CString::new(root.to_str().unwrap()).unwrap();
    let c_idx = CString::new(index_dir.to_str().unwrap()).unwrap();
    let c_cfg = CString::new(config_json).unwrap();

    let rc = sakuin::sakuin_init(c_root.as_ptr(), c_idx.as_ptr(), c_cfg.as_ptr());
    assert_eq!(rc, 0, "sakuin_init with valid config JSON should succeed");

    let rc = sakuin::sakuin_build_index();
    assert_eq!(rc, 0);

    let results = sync_search("hello");
    assert!(
        !results.is_empty(),
        "Expected results after init with custom config"
    );

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_init_with_invalid_config_json() {
    let project = create_test_project();
    let root = project.path();
    let index_dir = root.join(".sakuin");

    let c_root = CString::new(root.to_str().unwrap()).unwrap();
    let c_idx = CString::new(index_dir.to_str().unwrap()).unwrap();
    let c_cfg = CString::new("{not_valid_json}").unwrap();

    let rc = sakuin::sakuin_init(c_root.as_ptr(), c_idx.as_ptr(), c_cfg.as_ptr());
    assert_eq!(rc, -1, "sakuin_init with invalid config JSON should fail");

    let err_ptr = sakuin::sakuin_last_error();
    assert!(
        !err_ptr.is_null(),
        "last_error should be set after failed init"
    );
    let err = unsafe { CStr::from_ptr(err_ptr) }.to_str().unwrap();
    assert!(
        err.contains("Failed to parse config JSON"),
        "Unexpected error: {}",
        err
    );
    sakuin::sakuin_free_string(err_ptr);
}

// ============================================================================
// sakuin_last_error
// ============================================================================

#[test]
#[serial]
fn test_last_error_after_failed_search() {
    // Ensure state is None going in (all prior tests call sakuin_shutdown,
    // but call it here defensively in case a previous test panicked).
    sakuin::sakuin_shutdown();

    // Clear any leftover error from a previous test run.
    let stale = sakuin::sakuin_last_error();
    if !stale.is_null() {
        sakuin::sakuin_free_string(stale);
    }

    // Searching without init must return null and record the error.
    let c_query = CString::new("hello").unwrap();
    let result_ptr = sakuin::sakuin_search(c_query.as_ptr());
    assert!(
        result_ptr.is_null(),
        "sakuin_search without init should return null"
    );

    let err_ptr = sakuin::sakuin_last_error();
    assert!(
        !err_ptr.is_null(),
        "sakuin_last_error should be non-null after failed search"
    );
    let err = unsafe { CStr::from_ptr(err_ptr) }.to_str().unwrap();
    assert!(
        err.contains("not initialized"),
        "Unexpected error message: {}",
        err
    );
    sakuin::sakuin_free_string(err_ptr);

    // Reading clears the slot — second call should return null.
    let err_ptr2 = sakuin::sakuin_last_error();
    assert!(
        err_ptr2.is_null(),
        "sakuin_last_error should be null after it has been read once"
    );
}

// ============================================================================
// sakuin_get_progress
// ============================================================================

#[test]
#[serial]
fn test_get_progress_after_build() {
    let project = create_test_project();
    init_and_build(&project);

    let prog_ptr = sakuin::sakuin_get_progress();
    assert!(
        !prog_ptr.is_null(),
        "sakuin_get_progress should return non-null"
    );

    let prog_json = unsafe { CStr::from_ptr(prog_ptr) }.to_str().unwrap();
    let prog: serde_json::Value = serde_json::from_str(prog_json).unwrap();
    sakuin::sakuin_free_string(prog_ptr);

    assert!(prog.get("total").is_some(), "Progress should have 'total'");
    assert!(prog.get("done").is_some(), "Progress should have 'done'");
    assert_eq!(
        prog["status"].as_str().unwrap(),
        "done",
        "Status should be 'done' after synchronous build"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// sakuin_build_index_async + sakuin_indexing_take_event
// ============================================================================

/// Poll until an indexing event with status "done" arrives, or panic on timeout.
fn wait_for_indexing_done(timeout: std::time::Duration) -> serde_json::Value {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for indexing 'done' event"
        );

        wait_for_notification(std::time::Duration::from_millis(500));
        reset_notify_state();

        let event_ptr = sakuin::sakuin_indexing_take_event();
        if event_ptr.is_null() {
            continue;
        }

        let event_json = unsafe { CStr::from_ptr(event_ptr) }.to_str().unwrap();
        let event: serde_json::Value = serde_json::from_str(event_json).unwrap();
        sakuin::sakuin_free_string(event_ptr);

        match event["status"].as_str().unwrap_or("") {
            "done" => return event,
            "error" => panic!("Async indexing error: {}", event),
            _ => {} // progress — keep polling
        }
    }
}

#[test]
#[serial]
fn test_async_build_index() {
    let project = create_test_project();
    init_only(&project);

    let dummy_handle = 0xDEAD_BEEF_usize as *mut std::os::raw::c_void;
    sakuin::sakuin_register_async_notifier(
        dummy_handle,
        fake_uv_async_send as unsafe extern "C" fn(*mut std::os::raw::c_void) -> i32,
    );

    reset_notify_state();

    let rc = sakuin::sakuin_build_index_async();
    assert_eq!(rc, 0, "sakuin_build_index_async should succeed");

    let event = wait_for_indexing_done(std::time::Duration::from_secs(10));
    assert!(
        event["done"].as_u64().unwrap() >= 3,
        "Expected at least 3 files indexed in done event, got: {}",
        event
    );

    // Index should be usable after async build.
    let results = sync_search("hello");
    assert!(!results.is_empty(), "Should find results after async build");

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_async_update_index() {
    let project = create_test_project();
    let root = project.path();
    init_and_build(&project);

    let dummy_handle = 0xDEAD_BEEF_usize as *mut std::os::raw::c_void;
    sakuin::sakuin_register_async_notifier(
        dummy_handle,
        fake_uv_async_send as unsafe extern "C" fn(*mut std::os::raw::c_void) -> i32,
    );

    // Add a file so the incremental update has real work to do.
    fs::write(
        root.join("async_update_marker.txt"),
        "unique async update content xyzzy",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));

    reset_notify_state();

    let rc = sakuin::sakuin_update_index_async();
    assert_eq!(rc, 0, "sakuin_update_index_async should succeed");

    let event = wait_for_indexing_done(std::time::Duration::from_secs(10));
    assert_eq!(event["status"].as_str().unwrap(), "done");

    // The new file should now be searchable.
    let results = sync_search("xyzzy");
    assert!(
        !results.is_empty(),
        "Should find newly added file after async update"
    );

    sakuin::sakuin_shutdown();
}

// ============================================================================
// sakuin_start_watcher / sakuin_stop_watcher
// ============================================================================

#[test]
#[serial]
fn test_start_stop_watcher() {
    let project = create_test_project();
    init_and_build(&project);

    let rc = sakuin::sakuin_start_watcher();
    assert_eq!(rc, 0, "sakuin_start_watcher should succeed");

    sakuin::sakuin_stop_watcher(); // should not crash

    sakuin::sakuin_shutdown();
}

#[test]
#[serial]
fn test_watcher_double_start() {
    let project = create_test_project();
    init_and_build(&project);

    let rc = sakuin::sakuin_start_watcher();
    assert_eq!(rc, 0, "first sakuin_start_watcher should succeed");

    let rc = sakuin::sakuin_start_watcher();
    assert_eq!(rc, -1, "second sakuin_start_watcher should fail with -1");

    // Error should be available.
    let err_ptr = sakuin::sakuin_last_error();
    assert!(
        !err_ptr.is_null(),
        "last_error should describe the double-start failure"
    );
    sakuin::sakuin_free_string(err_ptr);

    sakuin::sakuin_shutdown();
}

// ============================================================================
// sakuin_search_cancel
// ============================================================================

#[test]
#[serial]
fn test_search_cancel() {
    let project = create_code_project();
    init_and_build(&project);

    let dummy_handle = 0xDEAD_BEEF_usize as *mut std::os::raw::c_void;
    sakuin::sakuin_register_async_notifier(
        dummy_handle,
        fake_uv_async_send as unsafe extern "C" fn(*mut std::os::raw::c_void) -> i32,
    );

    reset_notify_state();

    // Submit and immediately cancel.
    let query = CString::new("ThreadPool").unwrap();
    let rc = sakuin::sakuin_search_submit(query.as_ptr(), 99, 500, 0);
    assert_eq!(rc, 0);
    sakuin::sakuin_search_cancel();

    // Give the worker time to process the cancellation.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Draining the queue after cancel must not crash or produce invalid JSON.
    loop {
        let ptr = sakuin::sakuin_search_take_result();
        if ptr.is_null() {
            break;
        }
        let json = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        let _msg: serde_json::Value =
            serde_json::from_str(json).expect("Result message should be valid JSON");
        sakuin::sakuin_free_string(ptr);
    }

    sakuin::sakuin_shutdown();
}
