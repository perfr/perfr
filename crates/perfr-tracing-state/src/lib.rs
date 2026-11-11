// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure-logic bookkeeping shared by the capture backends, one module each:
//!
//!  - [`context_switches`]: pairs switch-in and switch-out per core into
//!    scheduling slices. Fed by Linux perf records and by Windows ETW.
//!  - [`function_calls`]: per-thread stacks of entered instrumented functions,
//!    matched with their exits. Used by the uprobe engine.
//!  - [`nvml`]: sampling cadence and record shapes for the NVIDIA helper.
//!  - [`gpu`], [`cupti`]: GPU job correlation. Nothing feeds them yet.

#![deny(unsafe_code)]

use std::hash::{BuildHasherDefault, Hasher};

/// The FxHash-style hasher shared by the modules here. `std`'s default
/// SipHash is designed for untrusted keys; these are core numbers and thread
/// ids on the capture hot path, so its DoS resistance buys nothing and the
/// cycles matter.
#[derive(Default)]
pub struct FxHasher {
    state: u64,
}

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state = (self.state ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> u64 {
        self.state
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;

pub mod context_switches;
pub mod function_calls;
pub mod cupti;
pub mod gpu;
pub mod nvml;
