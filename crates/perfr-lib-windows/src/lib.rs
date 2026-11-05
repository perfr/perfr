// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `libperfr.dll`: every exported symbol comes from `perfr-lib` (the
//! `perfr_*` API and the `perfr_frida_*` agent entry points). A cdylib exports
//! the `#[no_mangle]` functions of the crates linked into it, so this crate
//! only has to link that one.
#![allow(unused_imports)]
pub use perfr::*;
