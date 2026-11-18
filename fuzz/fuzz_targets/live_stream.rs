// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! A `.perfr.stream` file, or what a peer sends over the WebSocket: the
//! framing of `perfr-live-protocol`. Whatever the bytes, decoding returns
//! frames or an error. What decodes is encoded again, which must not panic
//! either: the service re-sends the frames it is given.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(frames) = perfr_live_protocol::decode_all(data) {
        for frame in &frames {
            let _ = perfr_live_protocol::encode_frame(frame);
        }
    }
});
