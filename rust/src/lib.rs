//! sakuin — Indexed full-text search engine for Neovim.
//!
//! This crate produces a C-compatible shared library (cdylib) that Neovim's
//! LuaJIT FFI loads at runtime.

mod ffi;
mod index;
mod search;
mod state;
mod tokenizer;
mod types;
mod walker;
mod watcher;

use std::os::raw::{c_char, c_void};

// FFI functions receive raw pointers from the caller (LuaJIT FFI). The
// `not_unsafe_ptr_arg_deref` lint fires because these `extern "C"` functions
// dereference raw pointers without being marked `unsafe`. Marking them
// `unsafe` is incorrect — they ARE the safe boundary that validates pointers
// before use. The actual dereferences are wrapped in `unsafe {}` blocks inside.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
mod ffi_exports {
    use super::*;

    // ========================================================================
    // Lifecycle
    // ========================================================================

    /// Initialize the sakuin engine.
    ///
    /// - `project_root`: absolute path to the project directory (e.g., `vim.fn.getcwd()`)
    /// - `index_dir`: absolute path to the index directory (e.g., `{project_root}/.sakuin`)
    /// - `config_json`: JSON-serialized `SakuinConfig` (may be NULL — uses defaults)
    ///
    /// Returns 0 on success, -1 on error (retrieve message with `sakuin_last_error`).
    #[no_mangle]
    pub extern "C" fn sakuin_init(
        project_root: *const c_char,
        index_dir: *const c_char,
        config_json: *const c_char,
    ) -> i32 {
        ffi::ffi_try(|| {
            let root = unsafe { ffi::cstr_to_str(project_root)? };
            let idx_dir = unsafe { ffi::cstr_to_str(index_dir)? };
            let config_str = if config_json.is_null() {
                None
            } else {
                Some(unsafe { ffi::cstr_to_str(config_json)? })
            };
            state::init(root, idx_dir, config_str)
        })
    }

    /// Shut down the engine: stop watcher, stop worker, commit pending writes, free resources.
    #[no_mangle]
    pub extern "C" fn sakuin_shutdown() {
        state::shutdown();
    }

    // ========================================================================
    // Indexing
    // ========================================================================

    /// Full index rebuild: clear existing index and re-index all files.
    ///
    /// Returns 0 on success, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_build_index() -> i32 {
        ffi::ffi_try(|| state::build_index().map(|_| ()))
    }

    /// Incremental index update: re-index changed files, remove deleted files.
    ///
    /// Returns 0 on success, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_update_index() -> i32 {
        ffi::ffi_try(|| state::update_index().map(|_| ()))
    }

    /// Spawn a full index rebuild on a background thread.
    ///
    /// Returns immediately. Progress is available via `sakuin_get_progress`.
    /// Completion/error is pushed via `uv_async_send` → `sakuin_indexing_take_event`.
    /// Returns 0 if the background job was spawned, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_build_index_async() -> i32 {
        ffi::ffi_try(|| {
            std::thread::spawn(|| {
                let prog = state::progress();
                match state::build_index() {
                    Ok(count) => {
                        state::push_indexing_event(state::IndexingEvent {
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: count,
                            status: "done",
                            error: None,
                            message: None,
                        });
                    }
                    Err(e) => {
                        prog.status.store(3, std::sync::atomic::Ordering::SeqCst);
                        ffi::set_last_error(e.clone());
                        state::push_indexing_event(state::IndexingEvent {
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: prog.done.load(std::sync::atomic::Ordering::Relaxed),
                            status: "error",
                            error: Some(e),
                            message: None,
                        });
                    }
                }
            });
            Ok(())
        })
    }

    /// Spawn an incremental index update on a background thread.
    ///
    /// Returns immediately. Progress is available via `sakuin_get_progress`.
    /// Completion/error is pushed via `uv_async_send` → `sakuin_indexing_take_event`.
    /// Returns 0 if the background job was spawned, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_update_index_async() -> i32 {
        ffi::ffi_try(|| {
            std::thread::spawn(|| {
                let prog = state::progress();
                match state::update_index() {
                    Ok((added, updated, removed)) => {
                        state::push_indexing_event(state::IndexingEvent {
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: added + updated + removed,
                            status: "done",
                            error: None,
                            message: None,
                        });
                    }
                    Err(e) => {
                        prog.status.store(3, std::sync::atomic::Ordering::SeqCst);
                        ffi::set_last_error(e.clone());
                        state::push_indexing_event(state::IndexingEvent {
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: prog.done.load(std::sync::atomic::Ordering::Relaxed),
                            status: "error",
                            error: Some(e),
                            message: None,
                        });
                    }
                }
            });
            Ok(())
        })
    }

    /// Take the latest indexing completion event from the shared slot.
    ///
    /// Called from `on_async_notification` (the uv_async callback). Returns a JSON string:
    /// ```json
    /// {"status":"done","total":500,"done":500}
    /// {"status":"error","total":500,"done":200,"error":"..."}
    /// ```
    /// Returns NULL if no event is pending. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_indexing_take_event() -> *const c_char {
        match state::indexing_take_event() {
            Some(ev) => {
                let mut json = format!(
                    r#"{{"status":"{}","total":{},"done":{}"#,
                    ev.status, ev.total, ev.done
                );
                if let Some(msg) = ev.message {
                    json.push_str(&format!(r#","message":"{}""#, msg));
                }
                if let Some(err) = ev.error {
                    let escaped = err
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"")
                        .replace('\n', "\\n");
                    json.push_str(&format!(r#","error":"{}""#, escaped));
                }
                json.push('}');
                ffi::string_to_c(json)
            }
            None => std::ptr::null(),
        }
    }

    /// Get the progress of the current async build/update operation.
    ///
    /// Returns a JSON string:
    /// ```json
    /// {"total":1000,"done":500,"status":"running"}
    /// ```
    ///
    /// Status values: "idle", "running", "done", "error"
    ///
    /// Returns NULL on error. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_get_progress() -> *const c_char {
        let prog = state::progress();
        let total = prog.total.load(std::sync::atomic::Ordering::Relaxed);
        let done = prog.done.load(std::sync::atomic::Ordering::Relaxed);
        let status_code = prog.status.load(std::sync::atomic::Ordering::SeqCst);
        let status = match status_code {
            1 => "running",
            2 => "done",
            3 => "error",
            _ => "idle",
        };
        let json = format!(
            r#"{{"total":{},"done":{},"status":"{}"}}"#,
            total, done, status
        );
        ffi::string_to_c(json)
    }

    /// Start the background filesystem watcher.
    ///
    /// Returns 0 on success, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_start_watcher() -> i32 {
        ffi::ffi_try(state::start_watcher)
    }

    /// Stop the background filesystem watcher.
    #[no_mangle]
    pub extern "C" fn sakuin_stop_watcher() {
        state::stop_watcher();
    }

    // ========================================================================
    // Search — result-slot + uv_async notification
    // ========================================================================

    /// Register the libuv async handle used to notify the main Neovim thread
    /// when search results are ready.
    ///
    /// - `handle_ptr`: raw `uv_async_t*` pointer from `vim.uv.new_async()`
    /// - `send_fn_ptr`: address of `uv_async_send` (the only libuv function
    ///   that is safe to call from any thread)
    ///
    /// Must be called once before any `sakuin_search_submit` calls.
    #[no_mangle]
    pub extern "C" fn sakuin_register_async_notifier(
        handle_ptr: *mut c_void,
        send_fn_ptr: unsafe extern "C" fn(*mut c_void) -> i32,
    ) {
        state::register_async_notifier(handle_ptr, send_fn_ptr);
    }

    /// Take the next search result message from the queue.
    ///
    /// Messages are one of:
    /// - Batch: `{"type":"batch","generation":N,"results":[...],"total_so_far":M}`
    /// - Done:  `{"type":"done","generation":N,"total":M}`
    /// - Error: `{"type":"error","generation":N,"error":"..."}`
    ///
    /// Returns NULL if the queue is empty. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_search_take_result() -> *const c_char {
        match state::search_take_result() {
            Some(msg) => {
                let json = match msg {
                    state::SearchResultMessage::Batch {
                        generation,
                        results_json,
                        total_so_far,
                    } => {
                        format!(
                            r#"{{"type":"batch","generation":{},"results":{},"total_so_far":{}}}"#,
                            generation, results_json, total_so_far
                        )
                    }
                    state::SearchResultMessage::Done { generation, total } => {
                        format!(
                            r#"{{"type":"done","generation":{},"total":{}}}"#,
                            generation, total
                        )
                    }
                    state::SearchResultMessage::Error { generation, error } => {
                        let escaped = error
                            .replace('\\', "\\\\")
                            .replace('"', "\\\"")
                            .replace('\n', "\\n");
                        format!(
                            r#"{{"type":"error","generation":{},"error":"{}"}}"#,
                            generation, escaped
                        )
                    }
                };
                ffi::string_to_c(json)
            }
            None => std::ptr::null(),
        }
    }

    /// Submit a search query to the persistent worker thread.
    ///
    /// - `query`: the search query string (Tantivy query syntax)
    /// - `generation`: a monotonically-increasing counter from the Lua side.
    ///   The same value is echoed back in result messages so the Lua side can
    ///   discard stale results.
    /// - `batch_size`: number of results per batch (0 = default 500)
    /// - `limit`: maximum total results (0 = unlimited)
    ///
    /// Any in-flight search is automatically cancelled.
    /// Returns 0 on success, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_search_submit(
        query: *const c_char,
        generation: u64,
        batch_size: u64,
        limit: u64,
    ) -> i32 {
        ffi::ffi_try(|| {
            let query_str = unsafe { ffi::cstr_to_str(query)? };
            let bs = if batch_size == 0 {
                500
            } else {
                batch_size as usize
            };
            let lim = if limit == 0 {
                usize::MAX
            } else {
                limit as usize
            };
            state::search_submit(query_str, generation, bs, lim)
        })
    }

    /// Cancel any in-flight search on the worker thread.
    #[no_mangle]
    pub extern "C" fn sakuin_search_cancel() {
        state::search_cancel();
    }

    /// Execute a full-text search query (synchronous — blocks the caller).
    ///
    /// - `query`: the search query string (Tantivy query syntax).
    ///
    /// Returns a JSON string of results.
    /// Returns NULL on error (retrieve message with `sakuin_last_error`).
    /// The caller MUST free the returned string with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_search(query: *const c_char) -> *const c_char {
        let result = std::panic::catch_unwind(|| {
            let query_str = unsafe { ffi::cstr_to_str(query) };
            match query_str {
                Err(e) => {
                    ffi::set_last_error(e);
                    std::ptr::null()
                }
                Ok(q) => match state::do_search(q) {
                    Ok(results) => match serde_json::to_string(&results) {
                        Ok(json) => ffi::string_to_c(json),
                        Err(e) => {
                            ffi::set_last_error(format!("JSON serialization failed: {}", e));
                            std::ptr::null()
                        }
                    },
                    Err(e) => {
                        ffi::set_last_error(e);
                        std::ptr::null()
                    }
                },
            }
        });

        match result {
            Ok(ptr) => ptr,
            Err(_) => {
                ffi::set_last_error("Panic during search".into());
                std::ptr::null()
            }
        }
    }

    // ========================================================================
    // Info
    // ========================================================================

    /// Get index statistics as JSON.
    ///
    /// Returns NULL on error. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_stats() -> *const c_char {
        match state::stats() {
            Ok(stats) => match serde_json::to_string(&stats) {
                Ok(json) => ffi::string_to_c(json),
                Err(e) => {
                    ffi::set_last_error(format!("JSON serialization failed: {}", e));
                    std::ptr::null()
                }
            },
            Err(e) => {
                ffi::set_last_error(e);
                std::ptr::null()
            }
        }
    }

    /// Get the last error message.
    ///
    /// Returns NULL if no error has occurred since the last call.
    /// Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_last_error() -> *const c_char {
        match ffi::take_last_error() {
            Some(msg) => ffi::string_to_c(msg),
            None => std::ptr::null(),
        }
    }

    // ========================================================================
    // Memory management
    // ========================================================================

    /// Free a string previously returned by any sakuin function.
    ///
    /// Passing NULL is a no-op.
    #[no_mangle]
    pub extern "C" fn sakuin_free_string(ptr: *const c_char) {
        unsafe {
            ffi::free_c_string(ptr);
        }
    }
}

// Re-export FFI functions at crate root for integration tests.
pub use ffi_exports::*;
