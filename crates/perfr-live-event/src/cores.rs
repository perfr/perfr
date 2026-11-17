// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What each core of the captured machine is.
//!
//! The scheduler track has one lane per logical CPU, and the cores of one
//! machine are not always alike: Intel's hybrid parts have performance and
//! efficiency cores, an X3D Ryzen has the stacked cache on half of them,
//! and a core's top frequency can differ from its neighbour's. Which of
//! them a thread ran on is part of reading a schedule, so the service says
//! what it knows of each core once per capture and the viewer marks the
//! lanes with it.

/// What a core is called by its maker, where the machine tells.
pub mod core_kind {
    /// Not told: every core of the machine is of one kind, or the system
    /// does not say.
    pub const UNKNOWN: u8 = 0;
    /// A performance core (Intel P-core, the big cores of big.LITTLE).
    pub const PERFORMANCE: u8 = 1;
    /// An efficiency core (Intel E-core, the little cores of big.LITTLE).
    pub const EFFICIENCY: u8 = 2;
}

/// One logical CPU of the captured machine. A zero is a fact not known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoreInfo {
    /// The CPU's number: the scheduler lane it is, `Core N`.
    pub cpu: u16,
    /// One of [`core_kind`].
    pub kind: u8,
    /// The highest frequency the core can reach, in kHz.
    pub max_freq_khz: u32,
    /// The size of the level 3 cache the core sees, in KiB.
    pub l3_kb: u32,
}
