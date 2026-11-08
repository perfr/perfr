// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! C ABI for the global session. Emscripten and a native test link these
//! symbols. The pointer returned by flush and snapshot points at a buffer
//! owned by this crate and is invalid after the next call to either.

use std::sync::Mutex;

use crate::{with_session, PAGE_PID};

static OUT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

fn store(bytes: Vec<u8>, out_len: *mut usize) -> *const u8 {
    let mut slot = OUT.lock().unwrap_or_else(|p| p.into_inner());
    *slot = bytes;
    if !out_len.is_null() {
        unsafe { *out_len = slot.len() };
    }
    if slot.is_empty() {
        std::ptr::null()
    } else {
        slot.as_ptr()
    }
}

unsafe fn cstr(ptr: *const libc_char, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// `libc::c_char` without depending on the libc crate.
#[allow(non_camel_case_types)]
type libc_char = i8;

#[no_mangle]
pub extern "C" fn perfr_web_page_pid() -> u32 {
    PAGE_PID
}

#[no_mangle]
pub extern "C" fn perfr_web_now_ns() -> u64 {
    crate::now_ns()
}

#[no_mangle]
pub extern "C" fn perfr_web_set_tid(tid: u32) {
    crate::set_tid(tid);
}

#[no_mangle]
pub extern "C" fn perfr_web_begin(name: *const libc_char, name_len: usize) -> u64 {
    let name = unsafe { cstr(name, name_len) };
    crate::begin(&name)
}

#[no_mangle]
pub extern "C" fn perfr_web_end(handle: u64) {
    crate::end(handle);
}

#[no_mangle]
pub extern "C" fn perfr_web_begin_async(name: *const libc_char, name_len: usize) -> u64 {
    let name = unsafe { cstr(name, name_len) };
    crate::begin_async(&name)
}

#[no_mangle]
pub extern "C" fn perfr_web_end_async(handle: u64) {
    crate::end_async(handle);
}

#[no_mangle]
pub extern "C" fn perfr_web_span(
    name: *const libc_char,
    name_len: usize,
    start_ns: u64,
    end_ns: u64,
) {
    let name = unsafe { cstr(name, name_len) };
    crate::span(&name, start_ns, end_ns);
}

#[no_mangle]
pub extern "C" fn perfr_web_span_async(
    name: *const libc_char,
    name_len: usize,
    start_ns: u64,
    end_ns: u64,
) {
    let name = unsafe { cstr(name, name_len) };
    crate::span_async(&name, start_ns, end_ns);
}

#[no_mangle]
pub extern "C" fn perfr_web_value(name: *const libc_char, name_len: usize, value: f64) {
    let name = unsafe { cstr(name, name_len) };
    crate::value(&name, value);
}

#[no_mangle]
pub extern "C" fn perfr_web_mark(name: *const libc_char, name_len: usize) {
    let name = unsafe { cstr(name, name_len) };
    crate::mark(&name);
}

#[no_mangle]
pub extern "C" fn perfr_web_frame() {
    crate::frame();
}

#[no_mangle]
pub extern "C" fn perfr_web_name_thread(tid: u32, name: *const libc_char, name_len: usize) {
    let name = unsafe { cstr(name, name_len) };
    crate::name_thread(tid, &name);
}

#[no_mangle]
pub extern "C" fn perfr_web_name_process(name: *const libc_char, name_len: usize) {
    let name = unsafe { cstr(name, name_len) };
    crate::name_process(&name);
}

#[no_mangle]
pub extern "C" fn perfr_web_stop() {
    crate::stop();
}

#[no_mangle]
pub extern "C" fn perfr_web_clear() {
    crate::clear();
}

/// Live bytes since the previous flush. `*out_len` is the length. The
/// pointer is null when there is nothing to send.
#[no_mangle]
pub extern "C" fn perfr_web_flush(out_len: *mut usize) -> *const u8 {
    let bytes = with_session(|s| s.flush());
    store(bytes, out_len)
}

/// The whole ring as one `.perfr.stream`.
#[no_mangle]
pub extern "C" fn perfr_web_snapshot(out_len: *mut usize) -> *const u8 {
    let now = crate::now_ns();
    let bytes = with_session(|s| s.snapshot(now));
    store(bytes, out_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_all, set_manual_now};

    #[test]
    fn c_abi_span_decodes() {
        let _guard = crate::lock_global_test();
        perfr_web_clear();
        set_manual_now(Some(5_000));
        perfr_web_set_tid(7);
        let name = b"c-scope";
        let handle = perfr_web_begin(name.as_ptr() as *const libc_char, name.len());
        assert_ne!(handle, 0);
        set_manual_now(Some(5_250));
        perfr_web_end(handle);
        let mut len = 0usize;
        let ptr = perfr_web_flush(&mut len);
        assert!(len > 0);
        assert!(!ptr.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let frames = decode_all(bytes).unwrap();
        assert!(frames.iter().any(|f| matches!(
            f,
            perfr_live_protocol::LiveFrame::ThreadName { tid: 1, .. }
                | perfr_live_protocol::LiveFrame::EventBatch { .. }
        )));
        perfr_web_clear();
        set_manual_now(None);
    }
}
