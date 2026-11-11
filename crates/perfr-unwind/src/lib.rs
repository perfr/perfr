// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! DWARF-based callstack unwinding, on framehop and the object crate: the
//! unwinder walks CFI (.eh_frame / .debug_frame) over the slice of stack
//! copied with a sample; the modules come from the profiled process's maps,
//! and follow them as the process maps more code.
//!
//! What checks it: a live test samples its own thread through a real perf
//! ring and requires deep, mostly complete unwinds; the rest (which files
//! become modules and over what range, how a refresh adds and drops them,
//! how a walk ends) is tested against the test process's own maps and
//! synthetic stacks. It is not compared with another unwinder.

#![deny(unsafe_code)]

pub mod modules;
pub mod unwinder;

pub use unwinder::{ModulesChange, ProcessUnwinder, UnwindEnd, UnwindOutcome};
