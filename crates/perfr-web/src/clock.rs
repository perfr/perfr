// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Session clock. Nanoseconds relative to a local origin, not wall time:
//! `performance.now()` on wasm (so a page and a worker can be aligned by
//! the caller), `Instant` on native. A manual override exists so tests do
//! not depend on the real clock.

use std::sync::atomic::{AtomicU64, Ordering};

/// `u64::MAX` means "use the real clock".
static MANUAL_NS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Force [`now_ns`] to return `ns`. `None` restores the real clock.
pub fn set_manual_now(ns: Option<u64>) {
    MANUAL_NS.store(ns.unwrap_or(u64::MAX), Ordering::Relaxed);
}

pub fn now_ns() -> u64 {
    let manual = MANUAL_NS.load(Ordering::Relaxed);
    if manual != u64::MAX {
        return manual;
    }
    platform_now_ns()
}

#[cfg(target_arch = "wasm32")]
fn platform_now_ns() -> u64 {
    use wasm_bindgen::JsCast;
    let global = js_sys::global();
    let Ok(perf) = js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("performance"))
    else {
        return 0;
    };
    if perf.is_undefined() || perf.is_null() {
        return 0;
    }
    let Ok(now_fn) = js_sys::Reflect::get(&perf, &wasm_bindgen::JsValue::from_str("now")) else {
        return 0;
    };
    let Ok(now_fn) = now_fn.dyn_into::<js_sys::Function>() else {
        return 0;
    };
    let Some(now_ms) = now_fn.call0(&perf).ok().and_then(|v| v.as_f64()) else {
        return 0;
    };
    if !now_ms.is_finite() || now_ms < 0.0 {
        return 0;
    }
    // `now()` is milliseconds since this context's timeOrigin. A worker's
    // origin differs from the page's; callers that mix the two should pass
    // an adjusted timestamp into the session instead of using this clock.
    (now_ms * 1_000_000.0) as u64
}

#[cfg(not(target_arch = "wasm32"))]
fn platform_now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}
