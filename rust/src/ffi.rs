use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// # Safety
/// The pointer must be a valid, non-null, null-terminated C string.
pub unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("null pointer passed to cstr_to_str".into());
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map_err(|e| format!("Invalid UTF-8 in C string: {}", e))
}

/// The caller is responsible for freeing this with `sakuin_free_string`.
///
/// Interior NUL bytes are stripped: snippets pulled from text files can
/// contain `\0`, and returning a null pointer would crash `ffi.string` on
/// the Lua side.
pub fn str_to_c(s: &str) -> *const c_char {
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != 0).collect();
    let cs = unsafe { CString::from_vec_unchecked(bytes) };
    cs.into_raw() as *const c_char
}

/// # Safety
/// The pointer must have been returned by `CString::into_raw`.
pub unsafe fn free_c_string(ptr: *const c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr as *mut c_char));
    }
}

/// Result of a fallible FFI call. `err` is null on success, or a heap-allocated
/// C string on failure. The caller frees the error with `sakuin_free_string`.
#[repr(C)]
pub struct CStatus {
    pub err: *const c_char,
}

impl CStatus {
    pub fn ok() -> Self {
        Self {
            err: std::ptr::null(),
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        log::error!("{}", msg);
        Self {
            err: str_to_c(&msg),
        }
    }

    /// Consume the status, freeing the error string if present. Useful for
    /// Rust callers (tests, the CLI) that want a normal `Result`.
    pub fn into_result(self) -> Result<(), String> {
        if self.err.is_null() {
            Ok(())
        } else {
            let msg = unsafe { CStr::from_ptr(self.err) }
                .to_string_lossy()
                .into_owned();
            unsafe { free_c_string(self.err) };
            Err(msg)
        }
    }
}

/// Run a fallible closure and translate the result into a `CStatus`. Catches
/// panics so they cross the FFI boundary as errors instead of UB.
pub fn ffi_try<F>(f: F) -> CStatus
where
    F: FnOnce() -> Result<(), String> + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(Ok(())) => CStatus::ok(),
        Ok(Err(e)) => CStatus::err(e),
        Err(_) => CStatus::err("Rust panic caught at FFI boundary"),
    }
}

// ─── C-ABI structs ──────────────────────────────────────────────────────────

/// A single search result, C-compatible layout.
/// All string pointers are owned and must be freed via `sakuin_free_search_message`.
#[repr(C)]
pub struct CSearchResult {
    pub path: *const c_char,
    pub snippet: *const c_char,
    pub line: u32,
    pub col: u32,
    pub score: f32,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CSearchMessageType {
    Batch = 0,
    Done = 1,
    Error = 2,
}

/// A search result message returned from the queue.
/// - For `Batch`: `results` and `results_len` are populated, `error` is null.
/// - For `Done`: `results` is null, `results_len` is 0, `error` is null.
/// - For `Error`: `results` is null, `error` is non-null.
#[repr(C)]
pub struct CSearchMessage {
    pub msg_type: CSearchMessageType,
    pub generation: u64,
    pub total: u64,
    pub results: *mut CSearchResult,
    pub results_len: u32,
    pub error: *const c_char,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CIndexingStatus {
    Done = 0,
    Error = 1,
    Progress = 2,
}

#[repr(C)]
pub struct CIndexingEvent {
    pub status: CIndexingStatus,
    pub total: u64,
    pub done: u64,
    pub error: *const c_char,   // null if no error
    pub message: *const c_char, // null if no message
}

#[repr(C)]
pub struct CIndexStats {
    pub num_docs: u64,
    pub num_segments: u32,
    pub index_size_bytes: u64,
    pub project_root: *const c_char,
}

impl CSearchMessage {
    pub fn from_internal(msg: crate::state::SearchResultMessage) -> Self {
        use crate::state::SearchResultMessage;
        match msg {
            SearchResultMessage::Batch {
                generation,
                results,
                total_so_far,
            } => {
                let len = results.len() as u32;
                let c_results: Vec<CSearchResult> = results
                    .into_iter()
                    .map(|r| CSearchResult {
                        path: str_to_c(&r.path),
                        snippet: str_to_c(&r.snippet),
                        line: r.line,
                        col: r.col,
                        score: r.score,
                    })
                    .collect();
                let ptr = if c_results.is_empty() {
                    std::ptr::null_mut()
                } else {
                    let mut boxed = c_results.into_boxed_slice();
                    let ptr = boxed.as_mut_ptr();
                    std::mem::forget(boxed);
                    ptr
                };
                CSearchMessage {
                    msg_type: CSearchMessageType::Batch,
                    generation,
                    total: total_so_far,
                    results: ptr,
                    results_len: len,
                    error: std::ptr::null(),
                }
            }
            SearchResultMessage::Done { generation, total } => CSearchMessage {
                msg_type: CSearchMessageType::Done,
                generation,
                total,
                results: std::ptr::null_mut(),
                results_len: 0,
                error: std::ptr::null(),
            },
            SearchResultMessage::Error { generation, error } => CSearchMessage {
                msg_type: CSearchMessageType::Error,
                generation,
                total: 0,
                results: std::ptr::null_mut(),
                results_len: 0,
                error: str_to_c(&error),
            },
        }
    }
}

impl CIndexingEvent {
    pub fn from_internal(ev: crate::state::IndexingEvent) -> Self {
        let status = match ev.status {
            "done" => CIndexingStatus::Done,
            "error" => CIndexingStatus::Error,
            _ => CIndexingStatus::Progress,
        };
        CIndexingEvent {
            status,
            total: ev.total,
            done: ev.done,
            error: ev.error.map_or(std::ptr::null(), |e| str_to_c(&e)),
            message: ev.message.map_or(std::ptr::null(), str_to_c),
        }
    }
}

impl CIndexStats {
    pub fn from_internal(stats: crate::types::IndexStats) -> Self {
        CIndexStats {
            num_docs: stats.num_docs,
            num_segments: stats.num_segments as u32,
            index_size_bytes: stats.index_size_bytes,
            project_root: str_to_c(&stats.project_root),
        }
    }
}
