// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Invocation history, not a map from a trampoline to its latest caller.
//!
//! What a sample gets when the history is short of something, from least
//! lost to most:
//!
//! - A call the journal has no entry for (dropped when the journal was
//!   full, or begun under an earlier capture): its return slot keeps the
//!   trampoline, which is a registered one, and the walk stops there. The
//!   frames below it are right; the ones above are missing.
//! - A thread that lost a leave: its open entries are closed at the last
//!   instant the journal is known to have been complete, and from then on
//!   are calls with no entry, as above. Its later calls are journaled as
//!   usual. Other threads are not touched.
//! - A thread inside a hook whose trampoline nobody could identify: that
//!   thread's samples are the sampled pc alone until the call returns.
//! - The journal can no longer be read, or a taint could not be written:
//!   every sample is the sampled pc alone if an unidentified trampoline
//!   may be on a stack, and stops at the registered ones otherwise.
use perfr_frida_transport::returns::{Journal, Kind, Record};
use std::{cell::Cell, collections::HashMap};

/// How long one record may stay reserved and unwritten before the journal
/// is given up on. Counted from when the reader first met that record, not
/// from when it last read anything: a capture's first pass comes long after
/// its journal was made.
const STALL_LIMIT_NS: u64 = 250_000_000;
/// Entries kept at once. Calls that never return (a `longjmp` over the
/// hook) and samples nobody consumes both grow the history.
const MAX_FRAMES: usize = 262_144;

struct Frame {
    invocation: u64,
    start: u64,
    end: u64,
    sp: u64,
    original: u64,
    trampoline: u64,
}
struct Taint {
    token: u64,
    start: u64,
    end: u64,
}
#[derive(Default)]
struct Thread {
    /// The thread's id as the agent wrote it, which is what a lost leave is
    /// reported by.
    agent_tid: u32,
    /// In the order the calls were made: a caller before what it called.
    frames: Vec<Frame>,
    taints: Vec<Taint>,
}

pub struct Returns {
    pub journal: Journal,
    /// By the thread id the sampler reports.
    threads: HashMap<u32, Thread>,
    frames: usize,
    /// Sorted, for `ProcessUnwinder::unwind_into_avoiding`.
    pub trampolines: Vec<u64>,
    horizon: u64,
    /// When the last pass over the journal began.
    last_cutoff: u64,
    /// The record a pass is waiting on, and since when.
    stalled: Option<(u64, u64)>,
    /// Why there is no history to patch with any more.
    unreadable: Option<&'static str>,
    /// Why no stack can be walked past its sampled pc.
    leaf_only: Option<&'static str>,
    tids: Tids,
    records: Vec<Record>,
    pub patched_slots: Cell<u64>,
    pub incomplete_samples: Cell<u64>,
    /// Times a thread's open entries were closed for a lost leave.
    pub histories_cut: u64,
}
impl Returns {
    pub fn new(journal: Journal, target_pid: u32) -> Self {
        let now = crate::now_monotonic_ns();
        Self {
            journal,
            threads: HashMap::new(),
            frames: 0,
            trampolines: Vec::new(),
            horizon: now,
            last_cutoff: now,
            stalled: None,
            unreadable: None,
            leaf_only: None,
            tids: Tids::of(target_pid),
            records: Vec::new(),
            patched_slots: Cell::new(0),
            incomplete_samples: Cell::new(0),
            histories_cut: 0,
        }
    }
    /// Samples up to here can be read against the history. Without a
    /// history nothing is gained by holding them.
    pub fn horizon(&self) -> u64 {
        if self.unreadable.is_some() || self.leaf_only.is_some() {
            u64::MAX
        } else {
            self.horizon
        }
    }
    /// Why the history stopped being used, when it has.
    pub fn trouble(&self) -> Option<&'static str> {
        self.leaf_only.or(self.unreadable)
    }
    pub fn is_leaf_only(&self) -> bool {
        self.leaf_only.is_some()
    }
    /// No more history will be read: what is on the stacks now stays there
    /// with nothing to say when it leaves.
    pub fn give_up(&mut self, why: &'static str) {
        self.unreadable.get_or_insert(why);
        self.threads.clear();
        self.frames = 0;
    }
    pub fn poll(&mut self) {
        let complete_through = self.last_cutoff;
        let mut records = std::mem::take(&mut self.records);
        records.clear();
        let drained = self.journal.drain(&mut records);
        self.last_cutoff = drained.cutoff;
        self.trampolines = self.journal.trampolines();
        self.trampolines.sort_unstable();
        match drained.blocked {
            None => self.stalled = None,
            Some(at) => {
                let since = match self.stalled {
                    Some((position, since)) if position == at => since,
                    _ => drained.cutoff,
                };
                self.stalled = Some((at, since));
                if drained.cutoff.saturating_sub(since) > STALL_LIMIT_NS {
                    self.give_up("a thread of the target stopped while writing a journal record");
                }
            }
        }
        if self.unreadable.is_none() {
            for record in &records {
                self.apply(record);
            }
            if drained.lost_threads != 0 {
                self.close_open(drained.lost_threads, complete_through);
            }
            if self.frames > MAX_FRAMES {
                self.give_up("more hooked calls were in flight than are kept");
            }
            if drained.blocked.is_none() && self.unreadable.is_none() {
                self.horizon = drained.cutoff;
            }
        }
        self.records = records;
        if self.journal.unknown_return() {
            self.leaf_only
                .get_or_insert("a call through an unidentified trampoline could not be journaled");
        } else if self.unreadable.is_some() && self.journal.tainted() {
            self.leaf_only
                .get_or_insert("the journal can no longer say which threads are inside a hook with an unidentified trampoline");
        }
    }
    fn apply(&mut self, record: &Record) {
        let Some(tid) = self.tids.host(record.tid) else {
            // An entry or a leave nobody can place is a call with no entry.
            if record.kind == Kind::Taint {
                self.leaf_only
                    .get_or_insert("a thread inside a hook with an unidentified trampoline could not be matched to a sampled thread");
            }
            return;
        };
        match record.kind {
            Kind::Enter => {
                let thread = self.threads.entry(tid).or_default();
                thread.agent_tid = record.tid;
                thread.frames.push(Frame {
                    invocation: record.invocation,
                    start: record.time,
                    end: u64::MAX,
                    sp: record.sp,
                    original: record.original,
                    trampoline: record.trampoline,
                });
                self.frames += 1;
            }
            Kind::Leave => {
                // The entry may be gone: closed for a lost leave and
                // retired since, or never written.
                if let Some(frame) = self
                    .threads
                    .get_mut(&tid)
                    .and_then(|t| t.frames.iter_mut().rev().find(|f| f.invocation == record.invocation))
                {
                    frame.end = record.time;
                }
            }
            Kind::Taint => {
                let thread = self.threads.entry(tid).or_default();
                thread.agent_tid = record.tid;
                thread.taints.push(Taint {
                    token: record.invocation,
                    // One carried over from before this journal existed
                    // covers the capture's first samples too.
                    start: if record.sp != 0 { 0 } else { record.time },
                    end: u64::MAX,
                });
            }
            Kind::Untaint => {
                if let Some(taint) = self
                    .threads
                    .get_mut(&tid)
                    .and_then(|t| t.taints.iter_mut().rev().find(|t| t.token == record.invocation))
                {
                    taint.end = record.time;
                }
            }
        }
    }
    /// Some thread whose id has one of these bits lost a leave after
    /// `complete_through`. Which of its open entries it would have closed
    /// is not known, so none of them is believed past that instant: a
    /// later call at the same depth must not be given an earlier caller.
    fn close_open(&mut self, lost_threads: u64, complete_through: u64) {
        for thread in self.threads.values_mut() {
            if lost_threads & (1 << (thread.agent_tid % 64)) == 0 {
                continue;
            }
            let mut cut = false;
            for frame in thread.frames.iter_mut().filter(|f| f.end == u64::MAX) {
                frame.end = complete_through.max(frame.start);
                cut = true;
            }
            self.histories_cut += u64::from(cut);
        }
    }
    /// Restores the return addresses of the calls `tid` was inside at
    /// `time`, in a copy of its stack that starts at `sp`. False when the
    /// stack must not be walked at all.
    pub fn patch(&self, tid: u32, sp: u64, time: u64, stack: &mut [u8]) -> bool {
        if self.leaf_only.is_some() {
            return false;
        }
        let Some(thread) = self.threads.get(&tid) else {
            return true;
        };
        if thread.taints.iter().any(|t| time >= t.start && time < t.end) {
            return false;
        }
        // Reverse invocation order also peels a tail-call chain: the newer
        // hook's saved original may itself be the older hook's trampoline.
        for frame in thread.frames.iter().rev() {
            if time < frame.start || time >= frame.end {
                continue;
            }
            let Some(offset) = frame.sp.checked_sub(sp).and_then(|v| usize::try_from(v).ok()) else {
                continue;
            };
            let Some(end) = offset.checked_add(8) else {
                continue;
            };
            let Some(slot) = stack.get_mut(offset..end) else {
                continue;
            };
            // A nonlocal exit can skip on_leave. Never rewrite a reused
            // slot merely because an old invocation is still in history.
            if u64::from_le_bytes(slot.try_into().unwrap()) == frame.trampoline {
                slot.copy_from_slice(&frame.original.to_le_bytes());
                self.patched_slots.set(self.patched_slots.get() + 1);
            }
        }
        true
    }
    pub fn retire(&mut self, horizon: u64) {
        let mut frames = 0;
        self.threads.retain(|_, thread| {
            thread.frames.retain(|f| f.end > horizon);
            thread.taints.retain(|t| t.end > horizon);
            frames += thread.frames.len();
            !thread.frames.is_empty() || !thread.taints.is_empty()
        });
        self.frames = frames;
    }
}

/// The agent names a thread as the target's own PID namespace does; the
/// sampler names it as the service's does. They are the same number unless
/// the target is in a container.
struct Tids {
    pid: u32,
    same: bool,
    host_of: HashMap<u32, u32>,
    scanned_ns: u64,
}
impl Tids {
    fn of(pid: u32) -> Self {
        let nested = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|status| ns_ids(&status))
            .is_some_and(|ids| ids.len() > 1);
        Self {
            pid,
            same: !nested,
            host_of: HashMap::new(),
            scanned_ns: 0,
        }
    }
    fn host(&mut self, agent_tid: u32) -> Option<u32> {
        if self.same {
            return Some(agent_tid);
        }
        if let Some(&host) = self.host_of.get(&agent_tid) {
            return Some(host);
        }
        // A thread started since the last look. Not on every miss: an id
        // that matches nothing would otherwise list the tasks every time.
        let now = crate::now_monotonic_ns();
        if self.scanned_ns == 0 || now.saturating_sub(self.scanned_ns) > 20_000_000 {
            self.scanned_ns = now;
            let tasks = std::fs::read_dir(format!("/proc/{}/task", self.pid)).ok()?;
            for task in tasks.flatten() {
                let ids = std::fs::read_to_string(task.path().join("status"))
                    .ok()
                    .and_then(|status| ns_ids(&status));
                if let Some((&host, &inner)) = ids.as_ref().and_then(|ids| ids.first().zip(ids.last())) {
                    self.host_of.insert(inner, host);
                }
            }
        }
        self.host_of.get(&agent_tid).copied()
    }
}
/// The `NSpid:` line of a `/proc/<pid>/status`: the task's id in each PID
/// namespace it is in, from the one this `/proc` belongs to inwards.
fn ns_ids(status: &str) -> Option<Vec<u32>> {
    let line = status.lines().find_map(|line| line.strip_prefix("NSpid:"))?;
    line.split_whitespace().map(|id| id.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn history() -> (tempfile::NamedTempFile, Returns) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let j = Journal::create(f.as_file()).unwrap();
        (f, Returns::new(j, std::process::id()))
    }
    #[allow(clippy::too_many_arguments)]
    fn entry(
        h: &mut Returns,
        id: u64,
        tid: u32,
        sp: u64,
        original: u64,
        trampoline: u64,
        start: u64,
        end: u64,
    ) {
        let thread = h.threads.entry(tid).or_default();
        thread.agent_tid = tid;
        thread.frames.push(Frame {
            invocation: id,
            start,
            end,
            sp,
            original,
            trampoline,
        });
        h.frames += 1;
    }
    fn slot(value: u64) -> [u8; 8] {
        value.to_le_bytes()
    }
    #[test]
    fn recursive_calls_and_delayed_samples_keep_their_own_callers() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1010, 0xaaa, 0xccc, 10, 60);
        entry(&mut h, 2, 7, 0x1000, 0xbbb, 0xccc, 20, 40);
        let mut stack = [0; 24];
        stack[..8].copy_from_slice(&0xcccu64.to_le_bytes());
        stack[16..].copy_from_slice(&0xcccu64.to_le_bytes());
        h.patch(7, 0x1000, 30, &mut stack);
        assert_eq!(&stack[..8], &0xbbbu64.to_le_bytes());
        assert_eq!(&stack[16..], &0xaaau64.to_le_bytes());
        let mut later = 0xcccu64.to_le_bytes();
        h.patch(7, 0x1000, 50, &mut later);
        assert_eq!(later, 0xcccu64.to_le_bytes());
        // Identical slot address and trampoline on another thread must
        // restore that thread's own caller, not the latest global caller.
        entry(&mut h, 3, 8, 0x1000, 0xddd, 0xccc, 20, 40);
        let mut other = 0xcccu64.to_le_bytes();
        h.patch(8, 0x1000, 30, &mut other);
        assert_eq!(other, 0xdddu64.to_le_bytes());
        let mut unknown = 0xcccu64.to_le_bytes();
        h.patch(9, 0x1000, 30, &mut unknown);
        assert_eq!(unknown, 0xcccu64.to_le_bytes());
    }
    #[test]
    fn tail_calls_peel_in_reverse_but_reused_slots_are_not_overwritten() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1000, 0xaaa, 0xccc, 10, u64::MAX);
        entry(&mut h, 2, 7, 0x1000, 0xccc, 0xddd, 20, 40);
        let mut slot = 0xdddu64.to_le_bytes();
        h.patch(7, 0x1000, 30, &mut slot);
        assert_eq!(slot, 0xaaau64.to_le_bytes());
        // The old invocation survived a longjmp; the slot now holds normal code.
        let mut reused = 0xeeeeu64.to_le_bytes();
        h.patch(7, 0x1000, 50, &mut reused);
        assert_eq!(reused, 0xeeeeu64.to_le_bytes());
        entry(&mut h, 3, 7, 0x1000, 0xbbb, 0xccc, 60, 90);
        let mut newer = 0xcccu64.to_le_bytes();
        h.patch(7, 0x1000, 70, &mut newer);
        assert_eq!(newer, 0xbbbu64.to_le_bytes());
    }
    #[test]
    fn history_is_kept_until_the_sample_consumers_have_advanced() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1000, 0xaaa, 0xccc, 10, 40);
        // Native return metadata has advanced, but Python enrichment has not.
        h.horizon = 100;
        h.retire(20);
        let mut delayed = 0xcccu64.to_le_bytes();
        assert!(h.patch(7, 0x1000, 30, &mut delayed));
        assert_eq!(delayed, 0xaaau64.to_le_bytes());
        h.retire(100);
        assert!(h.threads.is_empty());
        assert_eq!(h.frames, 0);
    }

    /// The journal as the agent writes it, read as the capture loop does.
    #[test]
    fn calls_journaled_by_the_agent_patch_the_samples_taken_inside_them() {
        let (_f, mut h) = history();
        let tid = 7;
        assert!(h.journal.register(0xccc));
        let outer = h.journal.enter(tid, 0x1010, 0xaaa, 0xccc, false);
        let inner = h.journal.enter(tid, 0x1000, 0xbbb, 0xccc, false);
        let inside = crate::now_monotonic_ns();
        h.journal.leave(tid, inner);
        h.poll();
        assert_eq!(h.trampolines, [0xccc]);
        assert!(h.horizon() >= inside, "samples up to the pass can be read");
        let mut stack = [slot(0xccc), [0; 8], slot(0xccc)].concat();
        assert!(h.patch(tid, 0x1000, inside, &mut stack));
        assert_eq!(stack, [slot(0xbbb), [0; 8], slot(0xaaa)].concat());
        // After the inner call returned only the outer one is patched.
        let after = crate::now_monotonic_ns();
        let mut stack = [slot(0xccc), [0; 8], slot(0xccc)].concat();
        assert!(h.patch(tid, 0x1000, after, &mut stack));
        assert_eq!(stack, [slot(0xccc), [0; 8], slot(0xaaa)].concat());
        h.journal.leave(tid, outer);
        h.poll();
        h.retire(h.horizon());
        assert!(h.threads.is_empty());
        assert_eq!(h.trouble(), None);
    }

    #[test]
    fn a_lost_leave_closes_that_threads_open_calls_and_nobody_elses() {
        let (_f, mut h) = history();
        // Two threads, and a third that shares the first one's ring.
        let (hot, quiet, noisy) = (5, 6, 5 + perfr_frida_transport::returns::RINGS as u32);
        let hot_call = h.journal.enter(hot, 0x1000, 0xaaa, 0xccc, false);
        let quiet_call = h.journal.enter(quiet, 0x2000, 0xbbb, 0xccc, false);
        h.poll();
        let before = h.last_cutoff;
        // Its ring fills, and the hot thread's return finds no room.
        while h.journal.enter(noisy, 8, 16, 24, false) != 0 {}
        h.journal.leave(hot, hot_call);
        h.poll();
        assert_eq!(h.histories_cut, 1);
        assert_eq!(h.trouble(), None, "a loss is not the end of the history");
        let now = crate::now_monotonic_ns();
        // A later call of the hot thread at the same depth, whose entry was
        // dropped: the old caller must not be put in its slot.
        let mut reused = slot(0xccc);
        assert!(h.patch(hot, 0x1000, now, &mut reused));
        assert_eq!(reused, slot(0xccc), "left for the unwinder to stop at");
        // A sample from before the loss is still read against the entry.
        let mut earlier = slot(0xccc);
        assert!(h.patch(hot, 0x1000, before - 1, &mut earlier));
        assert_eq!(earlier, slot(0xaaa));
        // The other thread's call is as open as it was.
        let mut other = slot(0xccc);
        assert!(h.patch(quiet, 0x2000, now, &mut other));
        assert_eq!(other, slot(0xbbb));
        // And the hot thread's next journaled call is patched as usual.
        let next = h.journal.enter(hot, 0x1000, 0xddd, 0xccc, false);
        assert_ne!(next, 0);
        h.poll();
        let mut fresh = slot(0xccc);
        assert!(h.patch(hot, 0x1000, crate::now_monotonic_ns(), &mut fresh));
        assert_eq!(fresh, slot(0xddd));
        h.journal.leave(quiet, quiet_call);
    }

    #[test]
    fn a_thread_inside_an_unidentified_hook_is_leaf_only_and_only_it() {
        let (_f, mut h) = history();
        let other = h.journal.enter(8, 0x2000, 0xbbb, 0xccc, false);
        let token = h.journal.taint(7);
        h.poll();
        let inside = crate::now_monotonic_ns();
        assert!(!h.patch(7, 0x1000, inside, &mut slot(0xccc)));
        let mut stack = slot(0xccc);
        assert!(h.patch(8, 0x2000, inside, &mut stack));
        assert_eq!(stack, slot(0xbbb));
        h.journal.untaint(7, token);
        h.poll();
        assert!(h.patch(7, 0x1000, crate::now_monotonic_ns(), &mut slot(0xccc)));
        assert!(!h.patch(7, 0x1000, inside, &mut slot(0xccc)), "a held sample from inside it");
        assert!(!h.is_leaf_only());
        h.journal.leave(8, other);
    }

    #[test]
    fn a_taint_carried_over_from_an_earlier_capture_covers_the_first_samples() {
        let (_f, mut h) = history();
        let early = crate::now_monotonic_ns();
        h.journal.taint_carried(7);
        h.poll();
        assert!(!h.patch(7, 0x1000, early, &mut slot(0xccc)));
        assert!(h.patch(8, 0x1000, early, &mut slot(0xccc)));
    }

    #[test]
    fn a_record_being_written_holds_the_horizon_and_is_not_a_stall() {
        let (_f, mut h) = history();
        // A long time passes between making the journal and first reading
        // it: helper launch, injection, opening the sampling rings.
        h.horizon = 1;
        h.last_cutoff = 1;
        let call = h.journal.enter(7, 0x1000, 0xaaa, 0xccc, false);
        let reserved = h.journal.reserve_unwritten(7);
        h.poll();
        assert_eq!(h.trouble(), None, "met a moment ago, whenever the journal was made");
        assert_eq!(h.horizon(), 1, "nothing past the unwritten record is promised");
        let mut stack = slot(0xccc);
        assert!(h.patch(7, 0x1000, crate::now_monotonic_ns(), &mut stack));
        assert_eq!(stack, slot(0xaaa), "what was written before it is in use");
        // Still unwritten after the limit: now it is a stall.
        h.stalled = h.stalled.map(|(at, since)| (at, since - STALL_LIMIT_NS - 1));
        h.poll();
        assert!(h.trouble().is_some());
        assert!(!h.is_leaf_only(), "the registered trampolines still stop a walk");
        assert_eq!(h.horizon(), u64::MAX, "and samples are not held for a history that will not come");
        let mut stack = slot(0xccc);
        assert!(h.patch(7, 0x1000, crate::now_monotonic_ns(), &mut stack));
        assert_eq!(stack, slot(0xccc));
        let _ = (call, reserved);
    }

    #[test]
    fn an_unreadable_journal_is_leaf_only_once_an_unidentified_trampoline_is_about() {
        let (_f, mut h) = history();
        h.poll();
        h.give_up("for the test");
        h.poll();
        assert!(!h.is_leaf_only());
        // Nothing reads the record; the journal's note of it is enough.
        h.journal.taint(7);
        h.poll();
        assert!(h.is_leaf_only());
        assert!(!h.patch(8, 0x1000, 30, &mut slot(0xccc)));
    }

    #[test]
    fn a_taint_that_found_no_room_makes_every_sample_leaf_only() {
        let (_f, mut h) = history();
        while h.journal.enter(7 + perfr_frida_transport::returns::RINGS as u32, 8, 16, 24, false) != 0 {}
        assert_eq!(h.journal.taint(7), 0);
        h.poll();
        assert!(h.is_leaf_only());
        assert!(!h.patch(8, 0x1000, 30, &mut slot(0xccc)));
        assert_eq!(h.horizon(), u64::MAX);
    }

    #[test]
    fn a_task_names_itself_in_each_pid_namespace_it_is_in() {
        let status = "Name:\tsleep\nTgid:\t4242\nNSpid:\t4242\t17\nNSpgid:\t4242\t1\n";
        assert_eq!(ns_ids(status), Some(vec![4242, 17]));
        assert_eq!(ns_ids("Name:\tsleep\nNSpid:\t9\n"), Some(vec![9]));
        assert_eq!(ns_ids("Name:\tsleep\n"), None, "a kernel without the line");
        // This process is in the namespace its /proc describes.
        let mut own = Tids::of(std::process::id());
        assert!(own.same);
        assert_eq!(own.host(123), Some(123));
        // One that is not: its inner ids are looked up among its tasks.
        let mut nested = Tids { pid: std::process::id(), same: false, host_of: HashMap::new(), scanned_ns: 0 };
        let me = unsafe { libc::gettid() } as u32;
        assert_eq!(nested.host(me), Some(me), "found by listing the tasks");
        assert_eq!(nested.host(u32::MAX), None);
    }
}
