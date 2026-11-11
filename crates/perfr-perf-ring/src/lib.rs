// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The kernel side of a capture: `perf_event_open`, the event attributes
//! ([`attr`]), and the mmap ring buffer the kernel writes records into
//! ([`ring`]).
//!
//! Unsafe code is the syscalls in `sys` and the shared-memory accesses in
//! `ring`; the wrap arithmetic lives in the safe [`protocol`] module. What
//! checks it: the reader's tests run over a memfd laid out like a perf
//! mapping and written the way the kernel writes one (records wrapping at
//! every offset, lost records, records that cannot be right), and one test
//! watches its own thread through a real ring where the kernel permits.
//! Nothing compares it with another implementation any more: this and its
//! sibling crates began as a port of the C++ tracer, which is kept on the
//! `cpp` branch, and comments here that name a C++ function are saying
//! where a rule came from, not that something still checks against it.

pub mod attr;
pub mod protocol;
pub mod ring;
mod sys;

pub use ring::RingBuffer;
