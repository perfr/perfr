// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! `/proc/<pid>/maps`, which the unwinder and the symbolizer read for every
//! process of a capture.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = perfr_maps::parse_maps(data);
});
