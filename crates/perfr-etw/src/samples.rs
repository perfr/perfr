// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stack samples from `StackWalk` events.
//!
//! The profiler interrupt logs a `SampledProfile` tick and, because stack
//! tracing is enabled for it, a `StackWalk` carrying the frames of that
//! instant. Two things make this more than a one-to-one translation:
//!
//! - The walk is split. When the tick lands in the kernel, the kernel
//!   frames are logged at once and the user frames arrive in a second
//!   `StackWalk` once the thread is back in user mode -- same thread, same
//!   `EventTimeStamp`, possibly milliseconds later. Both halves must become
//!   one sample.
//! - The service profiles user code. Kernel frames (canonical upper half)
//!   are dropped; what remains is innermost first, the user pc at the
//!   syscall or the interrupted instruction at index 0, which is the order
//!   the Linux unwinder produces and the report counts.
//!
//! So samples are keyed by `(tid, event_timestamp)` and held until either
//! the stream has moved [`SampleAssembler::SETTLE_NS`] past them or the
//! capture stops. A sample whose user half never came (a thread that was
//! in the kernel and stayed there) is emitted with a single marker frame,
//! [`KERNEL_FRAME`], so the tick still counts and the tree says where the
//! thread was. A user half that comes after its tick was released (a
//! low-priority thread kept off its core for longer than the window) is
//! counted and dropped: the tick has been reported once already.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::clock::QpcClock;

/// The frame a sample gets when all of its frames were kernel ones:
/// resolved by the service as `[kernel]`.
pub const KERNEL_FRAME: u64 = u64::MAX;

/// One sample, ready for the symbolizer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Raw counter ticks of the profiler interrupt.
    pub timestamp: i64,
    pub pid: u32,
    pub tid: u32,
    /// User-mode program counters, innermost first; never empty.
    pub frames: Vec<u64>,
}

#[derive(Debug)]
struct Pending {
    pid: u32,
    frames: Vec<u64>,
    /// Whether a user-mode frame has been seen; a sample without one waits
    /// for the deferred user walk.
    has_user: bool,
}

/// Merges split stack walks into samples and releases them in order.
#[derive(Debug)]
pub struct SampleAssembler {
    pending: HashMap<(u32, i64), Pending>,
    /// Arrival order, for in-order release.
    order: VecDeque<(u32, i64)>,
    /// The keys released in the last [`Self::REMEMBER_NS`], in release
    /// order and as a set: a walk for one of them is the late half of a
    /// tick already reported, not a new tick.
    released_order: VecDeque<(u32, i64)>,
    released: HashSet<(u32, i64)>,
    newest: i64,
    /// [`Self::SETTLE_NS`] and [`Self::REMEMBER_NS`] in the counter's ticks.
    settle_ticks: i64,
    remember_ticks: i64,
    pub stacks_seen: u64,
    pub samples_emitted: u64,
    pub kernel_only: u64,
    /// Walks for processes the capture does not follow.
    pub filtered: u64,
    /// Walks that arrived after the sample they belong to was released.
    pub late_halves: u64,
}

impl SampleAssembler {
    /// How far the stream must move past a sample's instant before it is
    /// released. A deferred user walk comes within a scheduler quantum;
    /// this is an order of magnitude more. A time, not a tick count: the
    /// counter is 10 MHz on current Windows and about 3 MHz where it is
    /// the TSC divided down, and a fixed million ticks was 330 ms there.
    pub const SETTLE_NS: u64 = 100_000_000;
    /// How long a released sample's key is remembered, to recognise a
    /// half that comes too late. Bounds the memory: at 1 kHz on 64 cores
    /// this is 128,000 keys.
    pub const REMEMBER_NS: u64 = 2_000_000_000;

    /// An assembler for timestamps of `clock`, the session's.
    pub fn new(clock: QpcClock) -> SampleAssembler {
        let ticks = |ns: u64| i64::try_from(clock.to_ticks(ns)).unwrap_or(i64::MAX).max(1);
        SampleAssembler {
            pending: HashMap::new(),
            order: VecDeque::new(),
            released_order: VecDeque::new(),
            released: HashSet::new(),
            newest: 0,
            settle_ticks: ticks(Self::SETTLE_NS),
            remember_ticks: ticks(Self::REMEMBER_NS),
            stacks_seen: 0,
            samples_emitted: 0,
            kernel_only: 0,
            filtered: 0,
            late_halves: 0,
        }
    }

    /// [`Self::SETTLE_NS`] in the ticks of this assembler's clock.
    pub fn settle_ticks(&self) -> i64 {
        self.settle_ticks
    }

    /// Feeds one `StackWalk`. `pid_filter` keeps only that process's walks
    /// (zero keeps every process).
    pub fn push_stack(&mut self, pid: u32, tid: u32, event_timestamp: i64, frames: &[u64], pid_filter: u32) {
        self.stacks_seen += 1;
        if pid_filter != 0 && pid != pid_filter {
            self.filtered += 1;
            return;
        }
        self.newest = self.newest.max(event_timestamp);
        let key = (tid, event_timestamp);
        if self.released.contains(&key) {
            self.late_halves += 1;
            return;
        }
        let entry = self.pending.entry(key).or_insert_with(|| {
            self.order.push_back(key);
            Pending { pid, frames: Vec::new(), has_user: false }
        });
        for &frame in frames {
            if crate::events::is_kernel_address(frame) {
                continue;
            }
            entry.frames.push(frame);
            entry.has_user = true;
        }
    }

    /// Samples whose instant is settled, oldest first. `now` is the newest
    /// raw timestamp the stream has reached (any event, not only walks).
    pub fn drain_settled(&mut self, now: i64, out: &mut Vec<Sample>) {
        let now = now.max(self.newest);
        let horizon = now.saturating_sub(self.settle_ticks);
        while let Some(&(tid, ts)) = self.order.front() {
            if ts > horizon {
                break;
            }
            self.order.pop_front();
            if let Some(pending) = self.pending.remove(&(tid, ts)) {
                self.released_order.push_back((tid, ts));
                self.released.insert((tid, ts));
                out.push(self.finish(tid, ts, pending));
            }
        }
        let forgotten = now.saturating_sub(self.remember_ticks);
        while let Some(&key) = self.released_order.front() {
            if key.1 > forgotten {
                break;
            }
            self.released_order.pop_front();
            self.released.remove(&key);
        }
    }

    /// Everything still pending, at the end of a capture.
    pub fn drain_all(&mut self, out: &mut Vec<Sample>) {
        while let Some((tid, ts)) = self.order.pop_front() {
            if let Some(pending) = self.pending.remove(&(tid, ts)) {
                out.push(self.finish(tid, ts, pending));
            }
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn finish(&mut self, tid: u32, timestamp: i64, pending: Pending) -> Sample {
        self.samples_emitted += 1;
        let frames = if pending.has_user {
            pending.frames
        } else {
            self.kernel_only += 1;
            vec![KERNEL_FRAME]
        };
        Sample { timestamp, pid: pending.pid, tid, frames }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEN_MHZ: QpcClock = QpcClock::with_frequency(10_000_000);
    /// The settle window at 10 MHz.
    const SETTLE: i64 = 1_000_000;

    const K1: u64 = 0xFFFF_F800_0000_1000;
    const K2: u64 = 0xFFFF_F800_0000_2000;
    const U1: u64 = 0x7ff6_0000_0100;
    const U2: u64 = 0x7ff6_0000_0200;
    const U3: u64 = 0x7ff6_0000_0300;

    #[test]
    fn a_user_mode_sample_is_one_walk_innermost_first() {
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[U1, U2, U3], 0);
        let mut out = Vec::new();
        a.drain_settled(1_000 + SETTLE, &mut out);
        assert_eq!(out, vec![Sample { timestamp: 1_000, pid: 10, tid: 11, frames: vec![U1, U2, U3] }]);
        assert_eq!(a.samples_emitted, 1);
    }

    #[test]
    fn a_split_walk_merges_and_drops_the_kernel_half() {
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[K1, K2], 0);
        // Not settled yet, and not emitted kernel-only: the user half is due.
        let mut out = Vec::new();
        a.drain_settled(1_500, &mut out);
        assert!(out.is_empty());
        a.push_stack(10, 11, 1_000, &[U1, U2], 0);
        a.drain_settled(1_000 + SETTLE, &mut out);
        assert_eq!(out, vec![Sample { timestamp: 1_000, pid: 10, tid: 11, frames: vec![U1, U2] }]);
        assert_eq!(a.kernel_only, 0);
    }

    #[test]
    fn a_walk_that_stays_in_the_kernel_becomes_a_marker_frame() {
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[K1, K2], 0);
        let mut out = Vec::new();
        a.drain_settled(1_000 + SETTLE, &mut out);
        assert_eq!(out[0].frames, vec![KERNEL_FRAME]);
        assert_eq!(a.kernel_only, 1);
    }

    #[test]
    fn samples_are_released_in_order_and_only_once_settled() {
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[U1], 0);
        a.push_stack(10, 12, 2_000, &[U2], 0);
        a.push_stack(10, 11, 3_000, &[U3], 0);
        let mut out = Vec::new();
        a.drain_settled(2_000 + SETTLE, &mut out);
        assert_eq!(out.iter().map(|s| s.timestamp).collect::<Vec<_>>(), vec![1_000, 2_000]);
        assert_eq!(a.pending_count(), 1);
        a.drain_all(&mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(a.pending_count(), 0);
    }

    #[test]
    fn a_user_half_that_comes_after_its_tick_was_released_does_not_count_the_tick_twice() {
        // A thread preempted in the kernel for longer than the settle
        // window: its tick goes out kernel-only, then the user walk lands.
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[K1, K2], 0);
        let mut out = Vec::new();
        a.drain_settled(1_000 + SETTLE, &mut out);
        assert_eq!(out.len(), 1);
        a.push_stack(10, 11, 1_000, &[U1, U2], 0);
        a.drain_all(&mut out);
        assert_eq!(out.len(), 1, "one profiler tick is one sample: {out:?}");
        assert_eq!(a.samples_emitted, 1);
        assert_eq!(a.late_halves, 1);
        assert_eq!(a.pending_count(), 0);
        // The same thread's next tick is a sample like any other.
        a.push_stack(10, 11, 11_000, &[U1], 0);
        a.drain_all(&mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn the_settle_window_is_a_time_whatever_the_counter_frequency() {
        assert_eq!(SampleAssembler::new(TEN_MHZ).settle_ticks(), SETTLE);
        // A counter that is the TSC divided by 1024: 100 ms is 300,000 of
        // its ticks, not the million that fits a 10 MHz one.
        let mut a = SampleAssembler::new(QpcClock::with_frequency(3_000_000));
        assert_eq!(a.settle_ticks(), 300_000);
        a.push_stack(10, 11, 1_000, &[U1], 0);
        let mut out = Vec::new();
        a.drain_settled(1_000 + 299_999, &mut out);
        assert!(out.is_empty());
        a.drain_settled(1_000 + 300_000, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn released_samples_are_remembered_for_a_while_not_forever() {
        // Ten seconds of a 1 kHz profiler on one thread, drained as it goes.
        let mut a = SampleAssembler::new(TEN_MHZ);
        let mut out = Vec::new();
        for tick in 0..10_000i64 {
            let ts = tick * 10_000;
            a.push_stack(10, 11, ts, &[U1], 0);
            a.drain_settled(ts, &mut out);
        }
        assert_eq!(out.len(), 9_900, "everything but the last 100 ms");
        // Two seconds of keys, less the 100 ms not yet released.
        assert_eq!(a.released.len(), 1_900);
        assert_eq!(a.released_order.len(), a.released.len());
    }

    #[test]
    fn other_processes_are_filtered_when_a_target_is_set() {
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(99, 11, 1_000, &[U1], 10);
        a.push_stack(10, 12, 1_000, &[U2], 10);
        let mut out = Vec::new();
        a.drain_all(&mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pid, 10);
        assert_eq!(a.filtered, 1);
        assert_eq!(a.stacks_seen, 2);
    }

    #[test]
    fn the_stream_clock_advances_the_horizon_without_walks() {
        // A thread sampled once then idle: its sample must still come out
        // once the rest of the stream (context switches) moves on.
        let mut a = SampleAssembler::new(TEN_MHZ);
        a.push_stack(10, 11, 1_000, &[U1], 0);
        let mut out = Vec::new();
        a.drain_settled(1_000 + SETTLE - 1, &mut out);
        assert!(out.is_empty());
        a.drain_settled(1_000 + SETTLE, &mut out);
        assert_eq!(out.len(), 1);
    }
}
