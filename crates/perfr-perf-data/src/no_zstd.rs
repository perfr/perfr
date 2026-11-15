// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What stands in for the zstd decoder in a build without the `zstd`
//! feature: a recording made with `perf record -z` is refused, saying why.

#[derive(Default)]
pub struct Decoder {}

impl Decoder {
    // The signature of the decoder this stands in for.
    #[allow(clippy::ptr_arg)]
    pub fn push(&mut self, _piece: &[u8], _out: &mut Vec<u8>) -> Result<(), String> {
        Err("perf.data: recorded with `perf record -z`, and this build does not read zstd. Record without -z.".into())
    }
}
