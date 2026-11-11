// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared dynamic hook request model; backend availability is checked at Start.

/// Ceiling on armed probes. With kernel uprobes each hook costs two perf
/// events, so two file descriptors, on every CPU (they share the CPU's one
/// ring), and every process mapping the file takes the trap: a careless
/// selection is a resource problem, not just a slow one.
pub const MAX_HOOKS: usize = 16;

/// One function to hook: where the probe goes, in file terms.
#[derive(Clone, Debug)]
pub struct HookSpec {
    pub function_id: u64,
    pub module_path: String,
    pub file_offset: u64,
    pub name: String,
}
