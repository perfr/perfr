// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! A trace from another tool: Chrome Trace Event JSON or a Perfetto proto,
//! told apart by their first bytes as the viewer tells them.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = perfr_live_chrome::ingest_collect(data);
});
