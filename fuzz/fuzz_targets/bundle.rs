// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! A `.perfr.zip` bundle: the zip, its manifest and its Parquet tables. A
//! bundle that opens is turned into the stream a viewer is sent and sliced
//! to its first half, which are the two things the service does with one.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(bundle) = perfr_capture::CaptureBundle::from_zip(data) {
        let _ = bundle.to_stream();
        if let Some((start, end)) = bundle.time_bounds() {
            let _ = bundle.slice(start, start + (end - start) / 2);
        }
    }
});
