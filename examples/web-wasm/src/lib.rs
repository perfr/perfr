// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A wasm-bindgen app recording into the same ring as the C API.
//! The page pulls `.perfr.stream` bytes with `take_stream` and can hand
//! them to an embedded viewer, or ignore them and use `@perfr/web` instead.

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn tick(dt_ms: f64) {
    perfr_web::scope("frame", || {
        perfr_web::scope("simulate", || {
            perfr_web::value("dt", dt_ms);
        });
        perfr_web::frame();
    });
}

#[wasm_bindgen]
pub fn take_stream() -> Vec<u8> {
    perfr_web::flush()
}

#[wasm_bindgen]
pub fn snapshot() -> Vec<u8> {
    perfr_web::snapshot()
}
