// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Python in a capture: the target's Python functions in its stack samples,
//! what Python calls its threads, and who holds the GIL.
//!
//! `perfr-pystack` reads the Python stacks (with py-spy, without pausing the
//! target) and knows how one joins a native stack. This module is that
//! crate's place in the capture loop:
//!
//! - **Samples.** When a sample is unwound, the Python stacks its thread had
//!   just before and just after are fetched ([`PythonStacks::near`]); when it
//!   is named, the one that fits takes the place of the interpreter's eval
//!   frames ([`PythonStacks::merge`]). A Python function is a frame like any
//!   other from there on: a row in the reports, a line of the callstack, a
//!   name in a saved capture. Its module is its source file and it has no
//!   function id, so nothing offers to hook it. The interpreter's own frames
//!   between one Python function and the next are left out of a merged
//!   stack; what it does under the innermost one stays.
//! - **Thread names.** `threading.Thread(name=...)` is the name a Python
//!   program gives a thread, and before 3.14 the kernel never hears of it:
//!   every thread reads `python3`. [`PythonStacks::pump`] hands Python's
//!   names to the capture's name sync, which prefers them to a kernel name
//!   that says nothing more than the process's.
//! - **The GIL.** Each reading says which thread holds it. Once it has
//!   changed hands, the spans go out as scopes on one row of the process,
//!   `GIL`, each named after the thread that held it: the row shows the
//!   hand-overs, and the scope statistics of the row are how long each
//!   thread held it and how often.

use std::sync::Arc;

use perfr_live_event::python::{GIL_ROW_NAME, GIL_TID};
use perfr_live_event::{kind, LiveEvent};
use perfr_live_server::LiveService;
use perfr_pystack::{Merged, Native, Near, Readings, State};

use crate::names::NameSync;
use crate::serve::FrameNames;
use crate::symbolize::ResolvedFrame;

/// How a capture's samples met its Python stacks, for the summary.
#[derive(Default)]
struct Counters {
    /// Samples that now carry Python frames.
    merged: u64,
    /// Of those, the ones whose thread was in the same Python stack on the
    /// reading before and the reading after.
    steady: u64,
    /// Of those, the ones whose Python stack was a call deeper or shallower
    /// than the native stack: the reading and the sample were on either
    /// side of a call from C into Python, or of its return.
    apart: u64,
    /// Of the merged ones, those whose innermost call into Python was not in
    /// either reading beside the sample and was found in one a little
    /// further away, under the same callers.
    completed: u64,
    /// Samples with interpreter frames and no reading close enough.
    no_reading: u64,
    /// Samples in the interpreter that no reading had a Python frame for.
    unplaced: u64,
}

pub(crate) struct PythonStacks {
    /// The thread that reads. `None` once it is stopped.
    reader: Option<perfr_pystack::Reader>,
    readings: Arc<Readings>,
    pid: i32,
    /// The capture's frame id of each Python symbol; 0 until it has one.
    name_ids: Vec<u32>,
    counters: Counters,
    /// The reader's state as last logged.
    logged: State,
    gil_row_named: bool,
    // Reused across samples.
    kinds: Vec<Native>,
    merged: Vec<Merged>,
    candidate: Vec<Merged>,
    around: Vec<Arc<[perfr_pystack::Frame]>>,
    ids: Vec<u32>,
    spans: Vec<perfr_pystack::GilSpan>,
}

/// Whether `pid` has a Python interpreter mapped.
fn is_python(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .is_ok_and(|maps| perfr_pystack::maps_show_python(&maps))
}

impl PythonStacks {
    /// Starts reading the Python stacks of `pid`, if it is a Python program.
    ///
    /// `wanted` is the capture request's `python_stacks`: `Some(false)` is
    /// never, `Some(true)` is to try even when no interpreter is mapped
    /// under its own name (one linked statically into another program), and
    /// absent is to read when there is one.
    pub(crate) fn start(pid: i32, period_ns: u64, wanted: Option<bool>) -> Option<PythonStacks> {
        if pid <= 0 || wanted == Some(false) || (wanted.is_none() && !is_python(pid)) {
            return None;
        }
        let reader = match perfr_pystack::Reader::start(pid, period_ns, crate::now_monotonic_ns) {
            Ok(reader) => reader,
            Err(error) => {
                log::warn!("the Python stack reader could not be started: {error}");
                return None;
            }
        };
        if wanted == Some(true) && !is_python(pid) {
            log::info!(
                "pid {pid} has no Python interpreter mapped under its own name; \
                 looking for one in it all the same, as the capture asked"
            );
        } else {
            log::info!("pid {pid} runs Python: reading its Python stacks alongside the samples, without pausing it");
        }
        let readings = reader.readings().clone();
        Some(PythonStacks::over(pid, Some(reader), readings))
    }

    fn over(
        pid: i32,
        reader: Option<perfr_pystack::Reader>,
        readings: Arc<Readings>,
    ) -> PythonStacks {
        PythonStacks {
            reader,
            readings,
            pid,
            name_ids: Vec::new(),
            counters: Counters::default(),
            logged: State::Looking,
            gil_row_named: false,
            kinds: Vec::new(),
            merged: Vec::new(),
            candidate: Vec::new(),
            around: Vec::new(),
            ids: Vec::new(),
            spans: Vec::new(),
        }
    }

    /// The Python stacks of `tid` around `time_ns`. Asked when the sample is
    /// unwound: the readings are kept for two seconds, and a sample may wait
    /// longer than that for its symbols.
    pub(crate) fn near(&self, tid: u32, time_ns: u64) -> Near {
        self.readings.near(tid, time_ns)
    }

    /// Samples newer than this wait a pass or two: the reading that follows
    /// a sample is as likely to be the nearer one as the reading before it,
    /// and it has not been taken yet when the sample is.
    pub(crate) fn horizon_ns(&self, now_ns: u64) -> u64 {
        if !matches!(self.logged, State::Reading { .. }) {
            return u64::MAX;
        }
        let period = self.readings.stats().period_ns;
        now_ns.saturating_sub((2 * period).clamp(2_000_000, 50_000_000))
    }

    fn name_id(&mut self, symbol: u32, names: &mut FrameNames) -> u32 {
        let index = symbol as usize;
        if self.name_ids.len() <= index {
            self.name_ids.resize(index + 1, 0);
        }
        if self.name_ids[index] == 0 {
            let symbol = self
                .readings
                .symbol(symbol)
                .unwrap_or(perfr_pystack::Symbol {
                    name: "(python)".to_string(),
                    file: String::new(),
                });
            self.name_ids[index] = names.id_for_frame(&ResolvedFrame {
                name: symbol.name,
                module: symbol.file,
                address: 0,
                function_id: 0,
            });
        }
        self.name_ids[index]
    }

    /// Puts the Python frames into `frames`, a sample's frame ids innermost
    /// first, where the interpreter's eval frames are. `whole` is whether
    /// the native unwind reached the root. Of the two readings around the
    /// sample the nearer is used, unless the other is closer in shape to
    /// the native stack. A sample with nothing to place is left as it was
    /// and counted. The result is cut to `max_frames`, which
    /// loses outermost frames like a native stack deeper than that does.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn merge(
        &mut self,
        tid: u32,
        time_ns: u64,
        frames: &mut Vec<u32>,
        whole: bool,
        near: &Near,
        names: &mut FrameNames,
        max_frames: usize,
    ) {
        self.kinds.clear();
        self.kinds
            .extend(frames.iter().map(|id| names.python_kind(*id)));
        let has_eval = self.kinds.contains(&Native::Eval);
        if near.is_empty() {
            // A thread Python does not know, or one the reader was not
            // looking at then.
            self.counters.no_reading += u64::from(has_eval);
            return;
        }
        // The nearer reading, unless the one on the other side is closer
        // in shape.
        let mut best: Option<usize> = None;
        let mut any_frames = false;
        for stack in [&near.nearest, &near.other].into_iter().flatten() {
            any_frames |= !stack.is_empty();
            if let Some(apart) =
                perfr_pystack::merge(&self.kinds, whole, stack, &mut self.candidate)
            {
                if best.is_none_or(|best| apart < best) {
                    best = Some(apart);
                    std::mem::swap(&mut self.merged, &mut self.candidate);
                }
                if apart == 0 {
                    break;
                }
            }
        }
        let Some(mut apart) = best else {
            // A thread with no Python frames that is not in the interpreter
            // either is simply not running Python.
            self.counters.unplaced += u64::from(has_eval || any_frames);
            return;
        };
        if apart > 0 {
            if let Some(nearest) = &near.nearest {
                if self.complete(tid, time_ns, whole, nearest) {
                    apart = 0;
                    self.counters.completed += 1;
                }
            }
        }
        let merged = std::mem::take(&mut self.merged);
        self.ids.clear();
        for frame in merged.iter().take(max_frames) {
            let id = match *frame {
                Merged::Native(index) => frames[index],
                Merged::Python(symbol) => self.name_id(symbol, names),
            };
            self.ids.push(id);
        }
        self.merged = merged;
        std::mem::swap(frames, &mut self.ids);
        self.counters.merged += 1;
        self.counters.steady += u64::from(near.steady);
        self.counters.apart += u64::from(apart > 0);
    }

    /// For a sample whose native stack is in a call from C into Python that
    /// neither reading beside it caught: looks a little further for a
    /// reading that is `nearest` with more Python called under it, and fits
    /// the native stack exactly. A generator being drained by `sum`, or a
    /// key function called by `sorted`, is entered and left thousands of
    /// times a second, so half the readings have it and half do not; under
    /// the same callers, within milliseconds, it is the same call. Leaves
    /// the merge in `self.merged` and returns true when one is found.
    fn complete(
        &mut self,
        tid: u32,
        time_ns: u64,
        whole: bool,
        nearest: &Arc<[perfr_pystack::Frame]>,
    ) -> bool {
        let mut around = std::mem::take(&mut self.around);
        self.readings.around(tid, time_ns, 12, &mut around);
        let found = around.iter().any(|stack| {
            stack.len() > nearest.len()
                && stack[stack.len() - nearest.len()..] == nearest[..]
                && perfr_pystack::merge(&self.kinds, whole, stack, &mut self.candidate) == Some(0)
        });
        if found {
            std::mem::swap(&mut self.merged, &mut self.candidate);
        }
        around.clear();
        self.around = around;
        found
    }

    /// Once a pass: says what the reader found, hands Python's thread names
    /// to the name sync, and pushes the GIL spans that have ended.
    pub(crate) fn pump(
        &mut self,
        service: &LiveService,
        frame_names: &mut FrameNames,
        names: &mut NameSync,
        batch: &mut Vec<LiveEvent>,
    ) {
        let state = self.readings.state();
        if state != self.logged {
            match &state {
                State::Reading { version } => {
                    log::info!(
                        "Python {version} found in pid {}: samples now carry its Python frames",
                        self.pid
                    )
                }
                // A target that exited is the capture's own news, said by
                // the capture loop.
                State::Stopped { .. } if !crate::hook_journal::process_alive(self.pid) => {}
                State::Stopped { why } => {
                    let hint = if why.starts_with("this process may not read") {
                        " Reading another process's memory needs the permission attaching to it does: \
                         see `perfr doctor`."
                    } else {
                        ""
                    };
                    log::warn!(
                        "the Python stacks of pid {} are not being read: {why}. \
                         Samples keep the interpreter's own frames.{hint}",
                        self.pid
                    )
                }
                State::Looking => {}
            }
            self.logged = state;
        }
        for (tid, name) in self.readings.take_thread_names() {
            names.prefer_thread_name(self.pid as u32, tid, name);
        }
        let mut spans = std::mem::take(&mut self.spans);
        self.readings.take_gil_spans(&mut spans);
        for span in spans.drain(..) {
            if !self.gil_row_named {
                self.gil_row_named = true;
                service.set_thread_name(self.pid as u32, GIL_TID, GIL_ROW_NAME);
            }
            let label = self
                .readings
                .thread_name(span.tid)
                .or_else(|| crate::names::thread_comm(self.pid as u32, span.tid))
                .unwrap_or_else(|| format!("thread {}", span.tid));
            // A name in the frames' range of ids, as a hooked function's is:
            // the service's sequential ids belong to other tables.
            let name_id = frame_names.id_for(&label);
            batch.push(LiveEvent {
                start_ns: span.start_ns,
                duration_ns: span.end_ns - span.start_ns,
                tid: GIL_TID,
                pid: self.pid as u32,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                _pad: 0,
                name_id,
            });
        }
        self.spans = spans;
    }

    /// The capture is over: stops the reader, pushes what is left, and logs
    /// how the samples and the readings met.
    pub(crate) fn finish(
        &mut self,
        service: &LiveService,
        frame_names: &mut FrameNames,
        names: &mut NameSync,
        batch: &mut Vec<LiveEvent>,
    ) {
        // Stopping it ends the GIL span that was still open.
        match self.reader.take() {
            Some(mut reader) => reader.stop(),
            None => self.readings.finish(),
        }
        self.pump(service, frame_names, names, batch);
        let stats = self.readings.stats();
        if stats.readings == 0 {
            if stats.failed > 0 {
                log::warn!(
                    "none of {} readings of the Python stacks succeeded",
                    stats.failed
                );
            }
            return;
        }
        let seconds = stats.last_ns.saturating_sub(stats.first_ns) as f64 / 1e9;
        let rate = if seconds > 0.0 {
            (stats.readings - 1) as f64 / seconds
        } else {
            0.0
        };
        log::info!(
            "Python stacks: {} readings of {} thread(s) at {rate:.0} per second; \
             {} failed and {} stack(s) were read torn, and were dropped",
            stats.readings,
            stats.threads,
            stats.failed,
            stats.torn
        );
        let c = &self.counters;
        log::info!(
            "{} sample(s) carry Python frames: {} between two readings of the same stack, \
             {} with their innermost call into Python taken from a reading a little further away, \
             {} with a Python stack a call deeper or shallower than the native one. \
             {} kept the interpreter's frames: {} had no reading close enough, {} no Python frame to place",
            c.merged,
            c.steady,
            c.completed,
            c.apart,
            c.no_reading + c.unplaced,
            c.no_reading,
            c.unplaced
        );
        if c.merged == 0 && c.no_reading + c.unplaced > 0 {
            log::warn!(
                "no sample could be joined to a Python stack. If the interpreter's `_PyEval_EvalFrameDefault` \
                 does not appear by name in the report, its symbols were not found and the frames cannot be placed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::SampleStore;
    use perfr_live_server::ServerConfig;
    use perfr_pystack::{RawFrame, ThreadReading};

    const MS: u64 = 1_000_000;
    const PID: i32 = 4000;

    struct Rig {
        service: Arc<LiveService>,
        store: Arc<SampleStore>,
        names: FrameNames,
        readings: Arc<Readings>,
        python: PythonStacks,
    }

    impl Rig {
        fn new() -> Rig {
            let service = LiveService::new(ServerConfig {
                ring_buffer_bytes: 1 << 20,
                ..Default::default()
            })
            .unwrap();
            let store = Arc::new(SampleStore::new());
            let names = FrameNames::new(service.clone(), store.clone());
            let readings = Arc::new(Readings::new(MS));
            let python = PythonStacks::over(PID, None, readings.clone());
            Rig {
                service,
                store,
                names,
                readings,
                python,
            }
        }

        /// The frame ids of a native stack, innermost first, each frame
        /// `module!function`.
        fn native(&mut self, frames: &[&str]) -> Vec<u32> {
            frames
                .iter()
                .map(|frame| {
                    let (module, name) = frame.split_once('!').unwrap();
                    self.names.id_for_frame(&ResolvedFrame {
                        name: name.to_string(),
                        module: module.to_string(),
                        address: 1,
                        function_id: 9,
                    })
                })
                .collect()
        }

        /// One reading of thread `tid`: `(function, line, ends a run)`.
        fn read(&self, at_ms: u64, tid: u32, frames: &[(&str, i32, bool)]) {
            let frames = frames
                .iter()
                .map(|(name, line, entry)| RawFrame {
                    name,
                    file: "w.py",
                    line: *line,
                    entry: *entry,
                })
                .collect();
            self.readings.record(
                at_ms * MS,
                &[ThreadReading {
                    tid,
                    name: None,
                    holds_gil: false,
                    frames,
                }],
            );
        }

        fn merged(&mut self, tid: u32, at_ns: u64, whole: bool, frames: &[&str]) -> Vec<String> {
            let mut ids = self.native(frames);
            let near = self.python.near(tid, at_ns);
            self.python
                .merge(tid, at_ns, &mut ids, whole, &near, &mut self.names, 64);
            self.named(&ids)
        }

        fn named(&self, ids: &[u32]) -> Vec<String> {
            let frames = self.store.export_rows().1;
            ids.iter()
                .map(|id| {
                    frames
                        .iter()
                        .find(|(known, _)| known == id)
                        .unwrap()
                        .1
                        .name
                        .clone()
                })
                .collect()
        }
    }

    const NATIVE: [&str; 5] = [
        "python3.14!PyLong_FromLong",
        "python3.14!_PyEval_EvalFrameDefault",
        "python3.14!_PyEval_Vector",
        "python3.14!thread_run",
        "libc.so.6!start_thread",
    ];

    #[test]
    fn a_sample_carries_the_python_functions_its_thread_was_in() {
        let mut rig = Rig::new();
        rig.read(10, 7, &[("crunch", 3, false), ("worker", 9, true)]);
        assert_eq!(
            rig.merged(7, 10 * MS + 200_000, true, &NATIVE),
            [
                "PyLong_FromLong",
                "crunch (w.py:3)",
                "worker (w.py:9)",
                "start_thread"
            ]
        );
        // A Python function is a frame like another: its file is its module,
        // and with no function id nothing can offer to hook it.
        let frames = rig.store.export_rows().1;
        let crunch = &frames
            .iter()
            .find(|(_, info)| info.name == "crunch (w.py:3)")
            .unwrap()
            .1;
        assert_eq!(
            (crunch.module.as_str(), crunch.function_id, crunch.address),
            ("w.py", 0, 0)
        );
        assert_eq!(
            (rig.python.counters.merged, rig.python.counters.steady),
            (1, 0)
        );
    }

    #[test]
    fn a_sample_between_two_readings_of_one_stack_is_counted_as_steady() {
        let mut rig = Rig::new();
        rig.read(10, 7, &[("crunch", 3, true)]);
        rig.read(11, 7, &[("crunch", 3, true)]);
        rig.merged(7, 10 * MS + 500_000, true, &NATIVE);
        assert_eq!(
            (rig.python.counters.merged, rig.python.counters.steady),
            (1, 1)
        );
    }

    #[test]
    fn the_reading_on_the_other_side_is_taken_when_it_is_closer_in_shape() {
        let mut rig = Rig::new();
        // Nearest: two runs, against a native stack with one eval frame.
        // The other side: one run, a different function.
        rig.read(
            10,
            7,
            &[("key", 5, true), ("rank", 3, false), ("main", 1, true)],
        );
        rig.read(11, 7, &[("crunch", 3, false), ("main", 1, true)]);
        assert_eq!(
            rig.merged(7, 10 * MS + 100_000, true, &NATIVE),
            [
                "PyLong_FromLong",
                "crunch (w.py:3)",
                "main (w.py:1)",
                "start_thread"
            ]
        );
        assert_eq!(rig.python.counters.apart, 0);
    }

    #[test]
    fn a_reading_one_call_deeper_than_the_sample_gives_its_outer_frames() {
        let mut rig = Rig::new();
        // Both readings caught the key function `sorted` was calling; the
        // sample fell between two calls of it.
        rig.read(10, 7, &[("key", 5, true), ("rank", 3, true)]);
        rig.read(11, 7, &[("key", 5, true), ("rank", 3, true)]);
        assert_eq!(
            rig.merged(7, 10 * MS + 100_000, true, &NATIVE),
            ["PyLong_FromLong", "rank (w.py:3)", "start_thread"]
        );
        assert_eq!(
            (rig.python.counters.merged, rig.python.counters.apart),
            (1, 1)
        );
    }

    #[test]
    fn a_call_into_python_both_neighbours_missed_is_taken_from_a_reading_a_little_further_away() {
        let mut rig = Rig::new();
        // sum() draining a generator: the generator is on the stack in some
        // readings and not in others. The sample is in it; the two readings
        // beside the sample are not.
        let native = [
            "python3.14!_PyEval_EvalFrameDefault",
            "python3.14!gen_iternext",
            "python3.14!builtin_sum",
            "python3.14!_PyEval_EvalFrameDefault",
            "libc.so.6!__libc_start_main",
        ];
        rig.read(8, 7, &[("genexpr", 2, true), ("squares", 1, true)]);
        rig.read(10, 7, &[("squares", 1, true)]);
        rig.read(11, 7, &[("squares", 1, true)]);
        assert_eq!(
            rig.merged(7, 10 * MS + 400_000, true, &native),
            ["genexpr (w.py:2)", "squares (w.py:1)", "__libc_start_main"]
        );
        let c = &rig.python.counters;
        assert_eq!((c.merged, c.completed, c.apart), (1, 1, 0));
    }

    #[test]
    fn a_call_into_python_seen_only_under_other_callers_is_not_borrowed() {
        let mut rig = Rig::new();
        let native = [
            "python3.14!_PyEval_EvalFrameDefault",
            "python3.14!gen_iternext",
            "python3.14!_PyEval_EvalFrameDefault",
            "libc.so.6!__libc_start_main",
        ];
        // A generator, but under another function than the one the thread
        // is in around the sample.
        rig.read(
            8,
            7,
            &[("genexpr", 2, true), ("pairs", 5, false), ("main", 9, true)],
        );
        rig.read(10, 7, &[("squares", 1, false), ("main", 9, true)]);
        rig.read(11, 7, &[("squares", 1, false), ("main", 9, true)]);
        assert_eq!(
            rig.merged(7, 10 * MS + 400_000, true, &native),
            [
                "_PyEval_EvalFrameDefault",
                "gen_iternext",
                "squares (w.py:1)",
                "main (w.py:9)",
                "__libc_start_main"
            ]
        );
        let c = &rig.python.counters;
        assert_eq!((c.merged, c.completed, c.apart), (1, 0, 1));
    }

    #[test]
    fn a_sample_no_reading_fits_keeps_its_native_frames_and_is_counted() {
        let mut rig = Rig::new();
        let native: Vec<String> = NATIVE
            .iter()
            .map(|f| f.split_once('!').unwrap().1.to_string())
            .collect();
        // No reading of this thread at all.
        assert_eq!(rig.merged(7, 10 * MS, true, &NATIVE), native);
        // A reading, far from the sample.
        rig.read(10, 7, &[("crunch", 3, true)]);
        assert_eq!(rig.merged(7, 30 * MS, true, &NATIVE), native);
        assert_eq!(rig.python.counters.no_reading, 2);
        // A reading close by in which the thread has no Python frames.
        rig.read(40, 7, &[]);
        assert_eq!(rig.merged(7, 40 * MS, true, &NATIVE), native);
        assert_eq!(
            (rig.python.counters.unplaced, rig.python.counters.merged),
            (1, 0)
        );
    }

    #[test]
    fn a_thread_that_runs_no_python_is_not_a_disagreement() {
        let mut rig = Rig::new();
        // Python knows the thread, and it has no Python frames.
        rig.read(10, 7, &[]);
        assert_eq!(
            rig.merged(
                7,
                10 * MS,
                true,
                &["libblas.so!dgemm_kernel", "libc.so.6!start_thread"]
            ),
            ["dgemm_kernel", "start_thread"]
        );
        // A thread Python does not know.
        assert_eq!(
            rig.merged(
                8,
                10 * MS,
                true,
                &["libblas.so!dgemm_kernel", "libc.so.6!start_thread"]
            ),
            ["dgemm_kernel", "start_thread"]
        );
        let c = &rig.python.counters;
        assert_eq!((c.merged, c.no_reading, c.unplaced), (0, 0, 0));
    }

    #[test]
    fn a_merged_stack_deeper_than_the_limit_loses_its_outermost_frames() {
        let mut rig = Rig::new();
        rig.read(10, 7, &[("c", 3, false), ("b", 2, false), ("a", 1, true)]);
        let mut ids = rig.native(&NATIVE);
        let near = rig.python.near(7, 10 * MS);
        rig.python
            .merge(7, 10 * MS, &mut ids, true, &near, &mut rig.names, 3);
        assert_eq!(
            rig.named(&ids),
            ["PyLong_FromLong", "c (w.py:3)", "b (w.py:2)"]
        );
    }

    #[test]
    fn the_gil_becomes_scopes_named_after_their_holder_on_a_row_of_the_process() {
        let mut rig = Rig::new();
        let mut names = NameSync::default();
        let mut batch = Vec::new();
        let holder = |at_ms: u64, holder: u32| {
            let threads: Vec<ThreadReading<'_>> = [(11, "worker-0"), (12, "worker-1")]
                .iter()
                .map(|(tid, name)| ThreadReading {
                    tid: *tid,
                    name: Some(name),
                    holds_gil: *tid == holder,
                    frames: Vec::new(),
                })
                .collect();
            rig.readings.record(at_ms * MS, &threads);
        };
        holder(10, 11);
        holder(11, 11);
        rig.python
            .pump(&rig.service, &mut rig.names, &mut names, &mut batch);
        assert!(batch.is_empty(), "one holder so far: no row");
        holder(12, 12);
        holder(13, 11);
        rig.python
            .finish(&rig.service, &mut rig.names, &mut names, &mut batch);
        let spans: Vec<(String, u64, u64)> = batch
            .iter()
            .map(|e| {
                assert_eq!(
                    (e.pid, e.tid, e.kind, e.depth),
                    (PID as u32, GIL_TID, kind::API_SCOPE, 0)
                );
                (
                    rig.named(&[e.name_id]).remove(0),
                    e.start_ns / 100_000,
                    e.duration_ns / 100_000,
                )
            })
            .collect();
        // Start and length, in tenths of a millisecond.
        assert_eq!(
            spans,
            [
                ("worker-0".to_string(), 100, 15),
                ("worker-1".to_string(), 115, 10),
                ("worker-0".to_string(), 125, 5)
            ]
        );
        let (threads, _) = rig.service.capture_names();
        assert!(threads
            .iter()
            .any(|((pid, tid), name)| (*pid, *tid, name.as_str())
                == (PID as u32, GIL_TID, GIL_ROW_NAME)));
    }

    #[test]
    fn the_names_python_gives_its_threads_reach_the_name_sync() {
        let mut rig = Rig::new();
        let mut names = NameSync::default();
        let named = [ThreadReading {
            tid: 11,
            name: Some("ThreadPoolExecutor-0_3"),
            holds_gil: false,
            frames: Vec::new(),
        }];
        rig.readings.record(10 * MS, &named);
        rig.python
            .pump(&rig.service, &mut rig.names, &mut names, &mut Vec::new());
        let mut sent = Vec::new();
        names.refresh_with(
            &[PID as u32],
            |_| Some("python3".to_string()),
            |_| vec![(11, "python3".to_string()), (12, "python3".to_string())],
            |_, _| {},
            |_, tid, name| sent.push((tid, name.to_string())),
        );
        assert_eq!(
            sent,
            [
                (11, "ThreadPoolExecutor-0_3".to_string()),
                (12, "python3".to_string())
            ]
        );
    }
}
