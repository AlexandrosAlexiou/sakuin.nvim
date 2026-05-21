//! Plumbing that wakes the Neovim event loop from Rust threads.
//!
//! Worker threads push batches/terminals onto SEARCH_RESULT_QUEUE (or an
//! indexing event into INDEXING_EVENT) and call `uv_async_send` to wake the
//! Neovim event loop; the registered Lua callback then drains the queue on the
//! main thread.
//!
//! uv_async_send is the only libuv function safe to call off-thread — going
//! through a LuaJIT ffi.cast callback from a non-Lua thread SEGVs.

use std::collections::VecDeque;
use std::os::raw::c_void;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::types::SearchResult;

pub struct IndexingEvent {
    pub status: &'static str,
    pub total: u64,
    pub done: u64,
    pub error: Option<String>,
    pub message: Option<String>,
}

static INDEXING_EVENT: OnceLock<Mutex<Option<IndexingEvent>>> = OnceLock::new();

fn indexing_event_slot() -> &'static Mutex<Option<IndexingEvent>> {
    INDEXING_EVENT.get_or_init(|| Mutex::new(None))
}

pub fn indexing_take_event() -> Option<IndexingEvent> {
    indexing_event_slot().lock().take()
}

pub fn push_indexing_event(event: IndexingEvent) {
    *indexing_event_slot().lock() = Some(event);
    notify_main_thread();
}

pub enum SearchResultMessage {
    Batch {
        generation: u64,
        results: Vec<SearchResult>,
        total_so_far: u64,
    },
    Done {
        generation: u64,
        total: u64,
    },
    Error {
        generation: u64,
        error: String,
    },
}

static SEARCH_RESULT_QUEUE: OnceLock<Mutex<VecDeque<SearchResultMessage>>> = OnceLock::new();

pub(crate) fn search_result_queue() -> &'static Mutex<VecDeque<SearchResultMessage>> {
    SEARCH_RESULT_QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

pub fn search_take_result() -> Option<SearchResultMessage> {
    search_result_queue().lock().pop_front()
}

struct AsyncNotifier {
    handle: *mut c_void,
    send_fn: unsafe extern "C" fn(*mut c_void) -> i32,
}

// Safety: Raw pointers aren't Send, but we need it for the static Mutex.
// The uv_async_t handle lives for the Neovim process lifetime, and
// uv_async_send is the only thread-safe libuv function.
unsafe impl Send for AsyncNotifier {}

static ASYNC_NOTIFIER: OnceLock<Mutex<Option<AsyncNotifier>>> = OnceLock::new();

fn async_notifier() -> &'static Mutex<Option<AsyncNotifier>> {
    ASYNC_NOTIFIER.get_or_init(|| Mutex::new(None))
}

/// Register the libuv async handle and send function used to wake the main thread.
///
/// - `handle_ptr`: raw `uv_async_t*` from `vim.uv.new_async()`
/// - `send_fn_ptr`: address of `uv_async_send`
///
/// Must be called once before any `search_submit` calls.
pub fn register_async_notifier(
    handle_ptr: *mut c_void,
    send_fn_ptr: unsafe extern "C" fn(*mut c_void) -> i32,
) {
    let mut guard = async_notifier().lock();
    *guard = Some(AsyncNotifier {
        handle: handle_ptr,
        send_fn: send_fn_ptr,
    });
}

pub(crate) fn notify_main_thread() {
    let guard = async_notifier().lock();
    if let Some(notifier) = guard.as_ref() {
        unsafe {
            (notifier.send_fn)(notifier.handle);
        }
    }
}
