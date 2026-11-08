// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-page profiling for a program that is already running in the browser.
//!
//! A JS, Rust or C caller records scopes, async spans, values and thread
//! names into a ring. [`Session::flush`] and [`Session::snapshot`] encode
//! that ring with [`perfr_live_protocol`], the same `.perfr.stream` frames
//! the live viewer reads from a service or a `?capture=` URL.
//!
//! This is not a sampler. The browser does not let a page read another
//! thread's stack or the scheduler, so the only events are the ones the
//! page records and the timings the browser itself reports.

mod clock;
mod session;

#[cfg(feature = "bindgen")]
mod bindgen;
mod capi;

pub use session::{decode_all, stream_magic, Session, Stats, DEFAULT_CAPACITY};

/// Synthetic pid for the page. It is not an operating-system pid. It sits
/// above real pids and away from the service's agent pid (`0xA6E7_0000`).
pub const PAGE_PID: u32 = 0xB10B_0001;

/// Thread ids the JS package uses for built-in browser tracks. User scopes
/// on the main thread use [`tid::MAIN`]. Workers start at [`tid::WORKER_BASE`].
pub mod tid {
    pub const MAIN: u32 = 1;
    pub const FRAMES: u32 = 2;
    pub const GC: u32 = 3;
    pub const LONG_TASKS: u32 = 4;
    pub const LOAF: u32 = 5;
    pub const INPUT: u32 = 6;
    pub const LAYOUT: u32 = 7;
    pub const PAINT: u32 = 8;
    pub const RESOURCES: u32 = 9;
    pub const FETCH: u32 = 10;
    pub const NAVIGATION: u32 = 11;
    pub const USER_TIMING: u32 = 12;
    pub const VISIBILITY: u32 = 13;
    pub const WORKER_MSG: u32 = 14;
    pub const GPU: u32 = 15;
    pub const WORKER_BASE: u32 = 32;
}

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};

fn global() -> &'static Mutex<Session> {
    static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(Session::new(DEFAULT_CAPACITY)))
}

fn with_session<R>(f: impl FnOnce(&mut Session) -> R) -> R {
    let mut guard = global().lock().unwrap_or_else(|p| p.into_inner());
    f(&mut guard)
}

thread_local! {
    static CURRENT_TID: Cell<u32> = const { Cell::new(tid::MAIN) };
}

/// Thread id stamped on scopes that do not pass one. Defaults to [`tid::MAIN`].
pub fn set_tid(tid: u32) {
    CURRENT_TID.with(|c| c.set(tid));
}

pub fn current_tid() -> u32 {
    CURRENT_TID.with(|c| c.get())
}

pub fn now_ns() -> u64 {
    clock::now_ns()
}

/// Test hook. `None` restores the real clock.
pub fn set_manual_now(ns: Option<u64>) {
    clock::set_manual_now(ns);
}

pub fn scope<R>(name: &str, f: impl FnOnce() -> R) -> R {
    let handle = begin(name);
    let out = f();
    end(handle);
    out
}

pub fn scope_async<R>(name: &str, f: impl FnOnce() -> R) -> R {
    let handle = begin_async(name);
    let out = f();
    end_async(handle);
    out
}

pub fn begin(name: &str) -> u64 {
    let tid = current_tid();
    let now = now_ns();
    with_session(|s| s.begin(name, tid, now))
}

pub fn end(handle: u64) {
    let now = now_ns();
    with_session(|s| s.end(handle, now));
}

pub fn begin_async(name: &str) -> u64 {
    let tid = current_tid();
    let now = now_ns();
    with_session(|s| s.begin_async(name, tid, now))
}

pub fn end_async(handle: u64) {
    let now = now_ns();
    with_session(|s| s.end_async(handle, now));
}

pub fn span(name: &str, start_ns: u64, end_ns: u64) {
    let tid = current_tid();
    with_session(|s| s.span(name, tid, start_ns, end_ns));
}

pub fn span_async(name: &str, start_ns: u64, end_ns: u64) {
    let tid = current_tid();
    with_session(|s| s.span_async(name, tid, start_ns, end_ns));
}

pub fn value(name: &str, value: f64) {
    let tid = current_tid();
    let now = now_ns();
    with_session(|s| s.value(name, tid, value, now));
}

pub fn mark(name: &str) {
    let tid = current_tid();
    let now = now_ns();
    with_session(|s| s.mark(name, tid, now));
}

/// A zero-duration marker named `frame`, unless `name` is passed to [`mark`].
pub fn frame() {
    mark("frame");
}

pub fn name_thread(tid: u32, name: &str) {
    with_session(|s| s.name_thread(tid, name));
}

pub fn name_process(name: &str) {
    with_session(|s| s.name_process(name));
}

pub fn stop() {
    let now = now_ns();
    with_session(|s| s.stop(now));
}

pub fn clear() {
    with_session(|s| s.clear());
}

pub fn resync() {
    with_session(|s| s.resync());
}

/// Live frames since the previous flush. Empty when nothing changed.
pub fn flush() -> Vec<u8> {
    with_session(|s| s.flush())
}

/// The whole ring as one `.perfr.stream`.
pub fn snapshot() -> Vec<u8> {
    let now = now_ns();
    with_session(|s| s.snapshot(now))
}

pub fn stats() -> Stats {
    with_session(|s| s.stats())
}

#[cfg(test)]
pub(crate) fn lock_global_test() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_api_and_rust_api_share_one_ring() {
        // One test owns the process-global session. Other tests use Session.
        let _guard = lock_global_test();
        let _ = with_session(|s| s.clear());
        set_manual_now(Some(1_000));
        set_tid(tid::MAIN);
        let handle = begin("rust-scope");
        set_manual_now(Some(1_500));
        end(handle);
        set_manual_now(Some(2_000));
        value("n", 3.0);
        let bytes = flush();
        let frames = decode_all(&bytes).unwrap();
        assert!(frames.iter().any(|f| matches!(
            f,
            perfr_live_protocol::LiveFrame::EventBatch { events } if events.len() == 2
        )));
        clear();
        set_manual_now(None);
    }
}
