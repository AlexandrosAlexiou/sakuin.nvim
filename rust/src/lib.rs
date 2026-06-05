mod binary_detect;
mod bridge;
pub mod ffi;
pub(crate) mod git;
mod index;
mod indexing;
mod logging;
mod path_filter;
mod project_config;
mod search;
mod search_worker;
mod state;
mod tokenizer;
mod types;
mod walker;
mod watcher;

use std::os::raw::{c_char, c_void};

// These extern "C" fns validate raw ptrs internally; `unsafe` on the fn itself is incorrect.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
mod ffi_exports {
    use super::*;

    /// `config_json` may be NULL to use defaults. On error, the returned
    /// `CStatus.err` is a heap-allocated message the caller frees with
    /// `sakuin_free_string`.
    #[no_mangle]
    pub extern "C" fn sakuin_init(
        project_root: *const c_char,
        index_dir: *const c_char,
        config_json: *const c_char,
    ) -> ffi::CStatus {
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

    #[no_mangle]
    pub extern "C" fn sakuin_shutdown() {
        state::shutdown();
    }

    /// Run an indexing job on a background thread. Completion/error is pushed
    /// via `uv_async_send` → `sakuin_indexing_take_event`.
    fn spawn_indexing<F>(f: F)
    where
        F: FnOnce() -> Result<u64, String> + Send + 'static,
    {
        use std::sync::atomic::Ordering;
        std::thread::spawn(move || {
            let prog = indexing::progress();
            match f() {
                Ok(done) => bridge::push_indexing_event(bridge::IndexingEvent {
                    status: "done",
                    total: prog.total.load(Ordering::Relaxed),
                    done,
                    error: None,
                    message: None,
                }),
                Err(e) => {
                    prog.status
                        .store(indexing::PROGRESS_ERROR, Ordering::SeqCst);
                    bridge::push_indexing_event(bridge::IndexingEvent {
                        status: "error",
                        total: prog.total.load(Ordering::Relaxed),
                        done: prog.done.load(Ordering::Relaxed),
                        error: Some(e),
                        message: None,
                    });
                }
            }
        });
    }

    #[no_mangle]
    pub extern "C" fn sakuin_build_index_async() -> ffi::CStatus {
        spawn_indexing(indexing::build_index);
        ffi::CStatus::ok()
    }

    #[no_mangle]
    pub extern "C" fn sakuin_update_index_async() -> ffi::CStatus {
        spawn_indexing(|| indexing::update_index().map(|(a, u, r)| a + u + r));
        ffi::CStatus::ok()
    }

    /// Returns NULL if no event is pending. Caller MUST free with `sakuin_free_indexing_event`.
    #[no_mangle]
    pub extern "C" fn sakuin_indexing_take_event() -> *mut ffi::CIndexingEvent {
        match bridge::indexing_take_event() {
            Some(ev) => Box::into_raw(Box::new(ffi::CIndexingEvent::from_internal(ev))),
            None => std::ptr::null_mut(),
        }
    }

    #[no_mangle]
    pub extern "C" fn sakuin_free_indexing_event(ptr: *mut ffi::CIndexingEvent) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let ev = Box::from_raw(ptr);
            ffi::free_c_string(ev.error);
            ffi::free_c_string(ev.message);
        }
    }

    #[no_mangle]
    pub extern "C" fn sakuin_start_watcher() -> ffi::CStatus {
        ffi::ffi_try(state::start_watcher)
    }

    #[no_mangle]
    pub extern "C" fn sakuin_stop_watcher() {
        state::stop_watcher();
    }

    /// Register the libuv async handle (raw `uv_async_t*`) and the address of
    /// `uv_async_send` — the only libuv function safe to call off-thread.
    /// Must be called once before `sakuin_search_submit`.
    #[no_mangle]
    pub extern "C" fn sakuin_register_async_notifier(
        handle_ptr: *mut c_void,
        send_fn_ptr: unsafe extern "C" fn(*mut c_void) -> i32,
    ) {
        bridge::register_async_notifier(handle_ptr, send_fn_ptr);
    }

    /// Returns NULL if the queue is empty. Caller MUST free with `sakuin_free_search_message`.
    #[no_mangle]
    pub extern "C" fn sakuin_search_take_result() -> *mut ffi::CSearchMessage {
        match bridge::search_take_result() {
            Some(msg) => Box::into_raw(Box::new(ffi::CSearchMessage::from_internal(msg))),
            None => std::ptr::null_mut(),
        }
    }

    #[no_mangle]
    pub extern "C" fn sakuin_free_search_message(ptr: *mut ffi::CSearchMessage) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let msg = Box::from_raw(ptr);
            ffi::free_c_string(msg.error);
            if !msg.results.is_null() && msg.results_len > 0 {
                let results = Vec::from_raw_parts(
                    msg.results,
                    msg.results_len as usize,
                    msg.results_len as usize,
                );
                for r in results {
                    ffi::free_c_string(r.path);
                    ffi::free_c_string(r.snippet);
                }
            }
        }
    }

    /// `generation` is echoed back in result messages so the Lua side can
    /// discard stale batches. `limit = 0` means unlimited. Any in-flight
    /// search is automatically cancelled.
    #[no_mangle]
    pub extern "C" fn sakuin_search_submit(
        query: *const c_char,
        generation: u64,
        limit: u64,
    ) -> ffi::CStatus {
        ffi::ffi_try(|| {
            let query_str = unsafe { ffi::cstr_to_str(query)? };
            let lim = if limit == 0 {
                usize::MAX
            } else {
                limit as usize
            };
            search_worker::search_submit(query_str, generation, lim)
        })
    }

    #[no_mangle]
    pub extern "C" fn sakuin_search_cancel() {
        search_worker::search_cancel();
    }

    /// On success, writes a heap-allocated stats pointer to `*out` (caller
    /// frees with `sakuin_free_stats`). On error, leaves `*out` as NULL and
    /// returns the message in `CStatus`.
    #[no_mangle]
    pub extern "C" fn sakuin_stats(out: *mut *mut ffi::CIndexStats) -> ffi::CStatus {
        if out.is_null() {
            return ffi::CStatus::err("sakuin_stats: out pointer is null");
        }
        unsafe { *out = std::ptr::null_mut() };
        match state::stats() {
            Ok(stats) => {
                let ptr = Box::into_raw(Box::new(ffi::CIndexStats::from_internal(stats)));
                unsafe { *out = ptr };
                ffi::CStatus::ok()
            }
            Err(e) => ffi::CStatus::err(e),
        }
    }

    #[no_mangle]
    pub extern "C" fn sakuin_free_stats(ptr: *mut ffi::CIndexStats) {
        if ptr.is_null() {
            return;
        }
        unsafe {
            let stats = Box::from_raw(ptr);
            ffi::free_c_string(stats.project_root);
        }
    }

    /// Passing NULL is a no-op.
    #[no_mangle]
    pub extern "C" fn sakuin_free_string(ptr: *const c_char) {
        unsafe {
            ffi::free_c_string(ptr);
        }
    }

    /// `level`: "error", "warn", "info", "debug", "trace", or "off".
    #[no_mangle]
    pub extern "C" fn sakuin_set_log_level(level: *const c_char) -> ffi::CStatus {
        ffi::ffi_try(|| {
            let level_str = unsafe { ffi::cstr_to_str(level)? };
            let filter = logging::parse_level(level_str)
                .ok_or_else(|| format!("Unknown log level: '{}'", level_str))?;
            logging::set_level(filter);
            log::info!("Log level changed to {}", level_str);
            Ok(())
        })
    }

    #[no_mangle]
    pub extern "C" fn sakuin_clear_logs() {
        logging::clear();
    }
}

pub use ffi_exports::*;

/// Internal API for the debug CLI binary and integration tests.
#[doc(hidden)]
pub mod internal {
    pub use crate::indexing::{
        build_index, progress, update_index, PROGRESS_DONE, PROGRESS_ERROR, PROGRESS_RUNNING,
    };
    pub use crate::state::{do_search_streaming, init, shutdown, stats};
    pub use crate::types::{IndexStats, SakuinConfig, SearchResult};
}
