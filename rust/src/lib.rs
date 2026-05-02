mod ffi;
pub(crate) mod git;
mod index;
mod logging;
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

    /// Spawn a full index rebuild on a background thread.
    ///
    /// Returns immediately. Completion/error is pushed via `uv_async_send` →
    /// `sakuin_indexing_take_event`. Returns 0 if the background job was
    /// spawned, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_build_index_async() -> i32 {
        ffi::ffi_try(|| {
            std::thread::spawn(|| {
                let prog = state::progress();
                match state::build_index() {
                    Ok(count) => {
                        state::push_indexing_event(state::IndexingEvent {
                            status: "done",
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: count,
                            error: None,
                            message: None,
                        });
                    }
                    Err(e) => {
                        prog.status
                            .store(state::PROGRESS_ERROR, std::sync::atomic::Ordering::SeqCst);
                        ffi::set_last_error(e.clone());
                        state::push_indexing_event(state::IndexingEvent {
                            status: "error",
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: prog.done.load(std::sync::atomic::Ordering::Relaxed),
                            error: Some(e),
                            message: None,
                        });
                    }
                }
            });
            Ok(())
        })
    }

    #[no_mangle]
    pub extern "C" fn sakuin_update_index_async() -> i32 {
        ffi::ffi_try(|| {
            std::thread::spawn(|| {
                let prog = state::progress();
                match state::update_index() {
                    Ok((added, updated, removed)) => {
                        state::push_indexing_event(state::IndexingEvent {
                            status: "done",
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: added + updated + removed,
                            error: None,
                            message: None,
                        });
                    }
                    Err(e) => {
                        prog.status
                            .store(state::PROGRESS_ERROR, std::sync::atomic::Ordering::SeqCst);
                        ffi::set_last_error(e.clone());
                        state::push_indexing_event(state::IndexingEvent {
                            status: "error",
                            total: prog.total.load(std::sync::atomic::Ordering::Relaxed),
                            done: prog.done.load(std::sync::atomic::Ordering::Relaxed),
                            error: Some(e),
                            message: None,
                        });
                    }
                }
            });
            Ok(())
        })
    }

    /// Returns NULL if no event is pending. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_indexing_take_event() -> *const c_char {
        match state::indexing_take_event() {
            Some(ev) => match serde_json::to_string(&ev) {
                Ok(json) => ffi::str_to_c(&json),
                Err(_) => std::ptr::null(),
            },
            None => std::ptr::null(),
        }
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

    /// Returns NULL if the queue is empty. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_search_take_result() -> *const c_char {
        match state::search_take_result() {
            Some(msg) => match serde_json::to_string(&msg) {
                Ok(json) => ffi::str_to_c(&json),
                Err(_) => std::ptr::null(),
            },
            None => std::ptr::null(),
        }
    }

    /// Submit a search query to the persistent worker thread.
    ///
    /// - `query`: the search query string
    /// - `generation`: a monotonically-increasing counter from the Lua side.
    ///   The same value is echoed back in result messages so the Lua side can
    ///   discard stale results.
    /// - `limit`: maximum total results (0 = unlimited)
    ///
    /// Any in-flight search is automatically cancelled.
    /// Returns 0 on success, -1 on error.
    #[no_mangle]
    pub extern "C" fn sakuin_search_submit(
        query: *const c_char,
        generation: u64,
        limit: u64,
    ) -> i32 {
        ffi::ffi_try(|| {
            let query_str = unsafe { ffi::cstr_to_str(query)? };
            let lim = if limit == 0 {
                usize::MAX
            } else {
                limit as usize
            };
            state::search_submit(query_str, generation, lim)
        })
    }

    /// Cancel any in-flight search on the worker thread.
    #[no_mangle]
    pub extern "C" fn sakuin_search_cancel() {
        state::search_cancel();
    }

    /// Get index statistics as JSON.
    ///
    /// Returns NULL on error. Caller MUST free with `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_stats() -> *const c_char {
        match state::stats() {
            Ok(stats) => match serde_json::to_string(&stats) {
                Ok(json) => ffi::str_to_c(&json),
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
            Some(msg) => ffi::str_to_c(&msg),
            None => std::ptr::null(),
        }
    }

    /// Free a string previously returned by any sakuin function.
    ///
    /// Passing NULL is a no-op.
    #[no_mangle]
    pub extern "C" fn sakuin_free_string(ptr: *const c_char) {
        unsafe {
            ffi::free_c_string(ptr);
        }
    }

    /// Change the log level at runtime.
    ///
    /// `level`: one of "error", "warn", "info", "debug", "trace", "off".
    /// Returns 0 on success, -1 if the level string is unrecognized.
    #[no_mangle]
    pub extern "C" fn sakuin_set_log_level(level: *const c_char) -> i32 {
        ffi::ffi_try(|| {
            let level_str = unsafe { ffi::cstr_to_str(level)? };
            let filter = logging::parse_level(level_str)
                .ok_or_else(|| format!("Unknown log level: '{}'", level_str))?;
            logging::set_level(filter);
            log::info!("Log level changed to {}", level_str);
            Ok(())
        })
    }

    /// Clear the log file contents.
    #[no_mangle]
    pub extern "C" fn sakuin_clear_logs() {
        logging::clear();
    }
}

// Re-export FFI functions at crate root for integration tests.
pub use ffi_exports::*;

/// Internal API for the debug CLI binary and integration tests.
/// Not part of the C FFI surface exposed to Neovim.
#[doc(hidden)]
pub mod internal {
    pub use crate::state::{
        build_index, do_search_streaming, init, progress, shutdown, stats, update_index,
        PROGRESS_DONE, PROGRESS_ERROR, PROGRESS_RUNNING,
    };
    pub use crate::types::{IndexStats, SakuinConfig, SearchResult};
}
