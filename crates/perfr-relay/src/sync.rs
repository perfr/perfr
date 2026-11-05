// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Atomics for the ring. With `--features loom` these are loom's atomics so
//! the seqlock model can explore interleavings; otherwise the standard library.

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{fence, AtomicU64, Ordering};
#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{fence, AtomicU64, Ordering};
