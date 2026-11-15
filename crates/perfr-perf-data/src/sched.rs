// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! When each thread was on a CPU, from the switch records of a recording
//! made with `perf record --switch-events`.
//!
//! A record says a thread got on a CPU or left one, and, leaving, whether
//! it was pushed off while it could still run. The time between an "in"
//! and the next "out" is a running span; from a preempted "out" to the
//! next "in" the thread is runnable. When it left of its own accord the
//! records do not say what it waited for, and that time is no span.

use std::collections::HashMap;

/// A stretch of one thread's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub pid: u32,
    pub tid: u32,
    pub start: u64,
    pub end: u64,
    /// On a CPU; otherwise waiting for one.
    pub running: bool,
    /// Which CPU, when running and when the records say: a recording of
    /// one process does not.
    pub cpu: Option<u32>,
}

#[derive(Default)]
pub struct Switches {
    /// Per thread: on a CPU since, its process, the CPU.
    on_cpu: HashMap<u32, (u64, u32, Option<u32>)>,
    /// Per thread: preempted at, its process.
    runnable: HashMap<u32, (u64, u32)>,
}

impl Switches {
    /// Takes one switch record, in time order, and returns the span it
    /// closes, if it closes one.
    pub fn switch(
        &mut self,
        time: u64,
        pid: u32,
        tid: u32,
        cpu: Option<u32>,
        out: bool,
        preempt: bool,
    ) -> Option<Span> {
        // The idle thread is on a CPU whenever nothing else is.
        if tid == 0 {
            return None;
        }
        if !out {
            let waited = self.runnable.remove(&tid);
            self.on_cpu.insert(tid, (time, pid, cpu));
            return waited.and_then(|(since, pid)| span(pid, tid, since, time, false, None));
        }
        if preempt {
            self.runnable.insert(tid, (time, pid));
        }
        let (since, pid, cpu) = self.on_cpu.remove(&tid)?;
        span(pid, tid, since, time, true, cpu)
    }

    /// The end of the recording: what was running or waiting then was so
    /// until `end`.
    pub fn finish(&mut self, end: u64) -> Vec<Span> {
        let running = std::mem::take(&mut self.on_cpu)
            .into_iter()
            .filter_map(|(tid, (since, pid, cpu))| span(pid, tid, since, end, true, cpu));
        let waiting = std::mem::take(&mut self.runnable)
            .into_iter()
            .filter_map(|(tid, (since, pid))| span(pid, tid, since, end, false, None));
        let mut spans: Vec<Span> = running.chain(waiting).collect();
        spans.sort_by_key(|s| (s.start, s.tid));
        spans
    }
}

fn span(pid: u32, tid: u32, start: u64, end: u64, running: bool, cpu: Option<u32>) -> Option<Span> {
    (end > start).then_some(Span {
        pid,
        tid,
        start,
        end,
        running,
        cpu,
    })
}
