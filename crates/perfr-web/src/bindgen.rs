// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! wasm-bindgen surface for `@perfr/web`. One [`WebProfiler`] owns its ring,
//! separate from the C/Rust global session, so a page that embeds the
//! package does not share state with a Rust app linked into the same module.

use wasm_bindgen::prelude::*;

use crate::session::Session;
use crate::{tid, PAGE_PID};

fn ns(now_ns: f64) -> u64 {
    if !now_ns.is_finite() || now_ns <= 0.0 {
        0
    } else {
        now_ns as u64
    }
}

#[wasm_bindgen]
pub struct WebProfiler {
    session: Session,
}

#[wasm_bindgen]
impl WebProfiler {
    #[wasm_bindgen(constructor)]
    pub fn new(capacity: u32) -> WebProfiler {
        WebProfiler {
            session: Session::new(capacity as usize),
        }
    }

    pub fn page_pid(&self) -> u32 {
        PAGE_PID
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        self.session.intern(name)
    }

    pub fn name_process(&mut self, name: &str) {
        self.session.name_process(name);
    }

    pub fn name_thread(&mut self, tid: u32, name: &str) {
        self.session.name_thread(tid, name);
    }

    pub fn begin(&mut self, name: &str, tid: u32, now_ns: f64) -> f64 {
        self.session.begin(name, tid, ns(now_ns)) as f64
    }

    pub fn end(&mut self, handle: f64, now_ns: f64) {
        self.session.end(handle as u64, ns(now_ns));
    }

    pub fn begin_async(&mut self, name: &str, tid: u32, now_ns: f64) -> f64 {
        self.session.begin_async(name, tid, ns(now_ns)) as f64
    }

    pub fn end_async(&mut self, handle: f64, now_ns: f64) {
        self.session.end_async(handle as u64, ns(now_ns));
    }

    pub fn span(&mut self, name: &str, tid: u32, start_ns: f64, end_ns: f64) {
        self.session.span(name, tid, ns(start_ns), ns(end_ns));
    }

    pub fn span_async(&mut self, name: &str, tid: u32, start_ns: f64, end_ns: f64) {
        self.session.span_async(name, tid, ns(start_ns), ns(end_ns));
    }

    pub fn mark(&mut self, name: &str, tid: u32, now_ns: f64) {
        self.session.mark(name, tid, ns(now_ns));
    }

    pub fn value(&mut self, name: &str, tid: u32, value: f64, now_ns: f64) {
        self.session.value(name, tid, value, ns(now_ns));
    }

    pub fn stop(&mut self, now_ns: f64) {
        self.session.stop(ns(now_ns));
    }

    pub fn clear(&mut self) {
        self.session.clear();
    }

    pub fn resync(&mut self) {
        self.session.resync();
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.session.set_enabled(on);
    }

    pub fn flush(&mut self) -> Vec<u8> {
        self.session.flush()
    }

    pub fn snapshot(&self, now_ns: f64) -> Vec<u8> {
        self.session.snapshot(ns(now_ns))
    }

    pub fn live(&self) -> f64 {
        self.session.stats().live as f64
    }

    pub fn dropped(&self) -> f64 {
        self.session.stats().dropped as f64
    }

    pub fn produced(&self) -> f64 {
        self.session.stats().produced as f64
    }

    pub fn capacity(&self) -> f64 {
        self.session.stats().capacity as f64
    }

    pub fn capturing(&self) -> bool {
        self.session.stats().capturing
    }
}

#[wasm_bindgen]
pub fn page_pid() -> u32 {
    PAGE_PID
}

#[wasm_bindgen]
pub fn tid_main() -> u32 {
    tid::MAIN
}

#[wasm_bindgen]
pub fn tid_worker_base() -> u32 {
    tid::WORKER_BASE
}
