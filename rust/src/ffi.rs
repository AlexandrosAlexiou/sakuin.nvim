//! FFI helper utilities for the sakuin Neovim plugin.
//!
//! Handles conversion between Rust and C types at the FFI boundary.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::OnceLock;

use parking_lot::Mutex;

fn last_error() -> &'static Mutex<Option<String>> {
    static LAST_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    LAST_ERROR.get_or_init(|| Mutex::new(None))
}

pub fn set_last_error(msg: String) {
    log::error!("{}", msg);
    *last_error().lock() = Some(msg);
}

pub fn take_last_error() -> Option<String> {
    last_error().lock().take()
}

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
pub fn str_to_c(s: &str) -> *const c_char {
    match CString::new(s) {
        Ok(cs) => cs.into_raw() as *const c_char,
        Err(_) => {
            set_last_error("String contains null byte, cannot convert to C string".into());
            std::ptr::null()
        }
    }
}

/// # Safety
/// The pointer must have been returned by `CString::into_raw`.
pub unsafe fn free_c_string(ptr: *const c_char) {
    if !ptr.is_null() {
        drop(CString::from_raw(ptr as *mut c_char));
    }
}

/// Returns 0 on success, -1 on error (error message stored in LAST_ERROR).
pub fn ffi_try<F>(f: F) -> i32
where
    F: FnOnce() -> Result<(), String> + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(f) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            set_last_error(e);
            -1
        }
        Err(_) => {
            set_last_error("Rust panic caught at FFI boundary".into());
            -1
        }
    }
}
