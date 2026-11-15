// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perf.data`, the file Linux `perf record` writes, as live events.
//!
//! | In the file | Becomes |
//! |---|---|
//! | `SAMPLE` | a tick on the thread's sample bar and one sampled frame per level of its call chain, the events a capture's own samples are |
//! | `COMM`, `FORK` | thread and process names |
//! | `MMAP`, `MMAP2` | the module each frame is named after |
//! | `SWITCH`, `SWITCH_CPU_WIDE` (`perf record --switch-events`) | running and runnable thread states, and scheduling slices when the records say which CPU |
//!
//! A `perf.data` holds addresses and the files they are in, not symbols:
//! `perf report` reads those from the binaries on the machine. Nothing
//! here can, so a frame is `module+0xoffset`, the offset being the one in
//! the file (what `addr2line` or a symbol table is asked). The perfr
//! service, which can read the binaries, names them: see `perf_data` in
//! the service.
//!
//! A sample without a call chain (`perf record` with no `-g`) is its
//! sampled pc alone. One taken with `--call-graph dwarf` carries a copy of
//! the stack instead of a chain, which takes the binaries to unwind: here
//! it is again the pc, with whatever kernel frames the chain has.
//!
//! Every event of the recording is drawn on one sample bar per thread:
//! `perf record -e cycles -e cache-misses` is two profiles in one file,
//! and they are not told apart here.

use std::collections::{HashMap, HashSet};

use perfr_live_event::{extra, kind, thread_state, LiveEvent};
use perfr_perf_data::{Event, Processes, Sample, Session, Span, Switches};

use crate::ingest::ChromeIngestor;

/// Events handed out by one step, so that a round of a large recording
/// (everything a pass over the rings wrote, released at once) is spread
/// over the frames that draw it.
const EVENTS_PER_STEP: usize = 64;
/// The width of a sampled frame when the recording does not say how far
/// apart its samples are (an event counted in cycles or misses, not time).
const UNKNOWN_PERIOD_NS: u64 = 1_000;
/// A frame's depth is a byte; deeper stacks lose their outermost frames.
const MAX_DEPTH: usize = 255;
/// The key of the kernel's frames in the name cache: no process has it.
const KERNEL: u32 = u32::MAX;

pub struct PerfDataState {
    session: Session,
    processes: Processes,
    /// Address to interned label, per process: asked once per distinct
    /// address, and forgotten when the process maps something new.
    names: HashMap<u32, HashMap<u64, u32>>,
    /// Threads that have an event on the timeline, and so a row to name.
    shown: HashSet<(u32, u32)>,
    switches: Switches,
    last_ns: u64,
    finished: bool,
}

impl Default for PerfDataState {
    fn default() -> Self {
        PerfDataState {
            session: Session::new(false),
            processes: Processes::default(),
            names: HashMap::new(),
            shown: HashSet::new(),
            switches: Switches::default(),
            last_ns: 0,
            finished: false,
        }
    }
}

impl PerfDataState {
    /// Records read so far, for the progress line.
    pub fn records(&self) -> u64 {
        self.session.stats.records
    }

    /// Every event of the file has been handed out.
    pub fn is_done(&self) -> bool {
        self.finished
    }

    /// One step over `buf`, the bytes not consumed yet: hands out events
    /// that are ready, or reads one more record. Returns the bytes
    /// consumed, `None` when more are needed first.
    pub fn step(
        &mut self,
        buf: &[u8],
        ended: bool,
        ing: &mut ChromeIngestor,
        out: &mut Vec<LiveEvent>,
    ) -> Result<Option<usize>, String> {
        let before = out.len();
        let mut handed = 0;
        while handed < EVENTS_PER_STEP {
            let Some((time, event)) = self.session.next_event() else {
                break;
            };
            self.on_event(time, event, ing, out);
            handed += 1;
        }
        let used = if handed > 0 {
            Some(0)
        } else if self.session.is_done() {
            if !self.finished {
                // What was running when the recording ended ran until then.
                out.extend(
                    self.switches
                        .finish(self.last_ns)
                        .iter()
                        .flat_map(span_events),
                );
                self.finished = true;
            }
            Some(buf.len())
        } else {
            let used = self.session.step(buf, ended)?;
            if self.session.stats.untimed_samples > 0 {
                return Err("perf.data: its samples carry no time, so there is no timeline to put them on. Record with `perf record -T`.".into());
            }
            used
        };
        ing.note_direct_events(&out[before..]);
        ing.stats.sample = self.session.stats.samples;
        Ok(used)
    }

    fn on_event(
        &mut self,
        time: u64,
        event: Event,
        ing: &mut ChromeIngestor,
        out: &mut Vec<LiveEvent>,
    ) {
        self.last_ns = self.last_ns.max(time);
        self.processes.apply(&event);
        match event {
            Event::Mmap(m) => {
                self.names.remove(&if m.kernel { KERNEL } else { m.pid });
            }
            Event::Comm { pid, tid, exec, .. } => {
                if exec {
                    self.names.remove(&pid);
                }
                // A thread already on the timeline was renamed.
                if self.shown.contains(&(pid, tid)) {
                    self.name(pid, tid, ing);
                }
            }
            Event::Fork { pid, .. } => {
                self.names.remove(&pid);
            }
            Event::Exit { .. } => {}
            Event::Sample(sample) => self.on_sample(time, &sample, ing, out),
            Event::Switch {
                pid,
                tid,
                cpu,
                out: leaving,
                preempt,
            } => {
                if tid != 0 {
                    self.show(pid, tid, ing);
                }
                out.extend(
                    self.switches
                        .switch(time, pid, tid, cpu, leaving, preempt)
                        .iter()
                        .flat_map(span_events),
                );
            }
        }
    }

    fn on_sample(
        &mut self,
        time: u64,
        sample: &Sample,
        ing: &mut ChromeIngestor,
        out: &mut Vec<LiveEvent>,
    ) {
        if sample.frames.is_empty() {
            return;
        }
        self.show(sample.pid, sample.tid, ing);
        let period = self
            .session
            .attrs()
            .get(sample.attr)
            .and_then(|a| a.period_ns())
            .unwrap_or(UNKNOWN_PERIOD_NS);
        // Innermost first, as the file has them.
        let processes = &self.processes;
        let names: Vec<u32> = sample
            .frames
            .iter()
            .take(MAX_DEPTH)
            .map(|frame| {
                let cache = self
                    .names
                    .entry(if frame.kernel { KERNEL } else { sample.pid })
                    .or_default();
                *cache
                    .entry(frame.address)
                    .or_insert_with(|| ing.intern.intern(&processes.label(sample.pid, *frame)))
            })
            .collect();
        let event = LiveEvent {
            start_ns: time,
            duration_ns: period,
            tid: sample.tid,
            pid: sample.pid,
            ..LiveEvent::default()
        };
        out.push(LiveEvent {
            kind: kind::SAMPLE,
            name_id: names[0],
            ..event
        });
        for (depth, name_id) in names.iter().rev().enumerate() {
            out.push(LiveEvent {
                kind: kind::FUNCTION_CALL,
                depth: depth as u8,
                extra: extra::SAMPLED_FRAME,
                name_id: *name_id,
                ..event
            });
        }
    }

    fn show(&mut self, pid: u32, tid: u32, ing: &mut ChromeIngestor) {
        if self.shown.insert((pid, tid)) {
            self.name(pid, tid, ing);
        }
    }

    /// Names a row from the comms the recording holds. A thread it has no
    /// comm for keeps the `tid N` the ingestor gives it.
    fn name(&self, pid: u32, tid: u32, ing: &mut ChromeIngestor) {
        // `perf` writes no comm for the idle threads; it calls them this.
        let idle = (pid == 0).then_some("swapper");
        if let Some(name) = self.processes.process_name(pid).or(idle) {
            ing.name_process(pid, name);
        }
        if let Some(name) = self.processes.thread_name(tid).or(idle) {
            ing.name_thread(pid, tid, name);
        }
    }
}

/// A span of a thread's time as the timeline draws it: a state on the
/// thread's bar, and under a running one a slice on its CPU's row when the
/// recording says which CPU that was.
fn span_events(span: &Span) -> Vec<LiveEvent> {
    let event = LiveEvent {
        start_ns: span.start,
        duration_ns: span.end - span.start,
        tid: span.tid,
        pid: span.pid,
        ..LiveEvent::default()
    };
    let state = if span.running {
        thread_state::RUNNING
    } else {
        thread_state::RUNNABLE
    };
    let mut events = vec![LiveEvent {
        kind: kind::THREAD_STATE,
        extra: state,
        ..event
    }];
    if let (true, Some(cpu)) = (span.running, span.cpu) {
        events.push(LiveEvent {
            kind: kind::SCHEDULING_SLICE,
            extra: cpu.min(255) as u8,
            ..event
        });
    }
    events
}

/// A small recording for the tests that feed every format through the
/// stream: two threads, call chains, switch records.
#[cfg(test)]
pub(crate) fn test_recording() -> Vec<u8> {
    tests::recording(perfr_perf_data::testing::CALLCHAIN_SAMPLE, true)
}

#[cfg(test)]
mod tests {
    use perfr_live_event::{extra, kind, thread_state, LiveEvent};
    use perfr_perf_data::sample_type::CPU;
    use perfr_perf_data::testing::*;

    use crate::{ingest_collect, ChromeIngestor, ChromeStream, Format};

    fn name(ing: &ChromeIngestor, e: &LiveEvent) -> String {
        ing.intern.get(e.name_id).unwrap_or("").to_string()
    }

    /// A process with two threads, sampled at 1000 Hz with call chains,
    /// the second thread's records after the first's as `perf` writes
    /// them.
    pub(super) fn recording(sample_type: u64, switches: bool) -> Vec<u8> {
        let e = Event1 { sample_type, id: 7 };
        let mut records = vec![
            e.kernel_mmap(0xffff_ffff_8000_0000, 0x100_0000, "[kernel.kallsyms]_text"),
            e.comm(10, 10, "app", true, 100),
            e.mmap2(10, 0x40_0000, 0x1000, 0x1000, "/opt/app", 110),
            e.sample(
                10,
                10,
                300,
                0x40_0010,
                &[CONTEXT_USER, 0x40_0010, 0x40_0200],
            ),
            e.sample(
                10,
                10,
                500,
                0xffff_ffff_8000_0123,
                &[
                    CONTEXT_KERNEL,
                    0xffff_ffff_8000_0123,
                    CONTEXT_USER,
                    0x40_0010,
                    0x40_0200,
                ],
            ),
            e.fork(10, 10, 11, 10, 200),
            e.comm(10, 11, "worker", false, 210),
            e.sample(10, 11, 400, 0x40_0300, &[CONTEXT_USER, 0x40_0300]),
        ];
        if switches {
            records.extend([
                e.switch(10, 11, 220, false, false),
                e.switch(10, 11, 420, true, true),
                e.switch(10, 11, 460, false, false),
                e.switch(10, 11, 480, true, false),
                e.switch(10, 11, 490, false, false),
            ]);
        }
        records.push(finished_round());
        file(
            &[(attr(sample_type, SAMPLE_ID_ALL | FREQ, 1000), vec![7])],
            &records,
            &[],
        )
    }

    #[test]
    fn samples_become_a_tick_and_the_frames_of_their_stack() {
        let (ing, events) = ingest_collect(&recording(CALLCHAIN_SAMPLE, false)).unwrap();
        assert_eq!(ing.stats.sample, 3);
        assert_eq!(ing.process_names.get(&10).map(String::as_str), Some("app"));
        assert_eq!(
            ing.thread_names.get(&(10, 10)).map(String::as_str),
            Some("app")
        );
        assert_eq!(
            ing.thread_names.get(&(10, 11)).map(String::as_str),
            Some("worker")
        );
        assert_eq!(
            ing.process_names.len(),
            1,
            "only what has events is listed: {:?}",
            ing.process_names
        );

        let ticks: Vec<_> = events.iter().filter(|e| e.kind == kind::SAMPLE).collect();
        assert_eq!(
            ticks
                .iter()
                .map(|e| (e.start_ns, e.tid, name(&ing, e)))
                .collect::<Vec<_>>(),
            [
                (300, 10, "app+0x1010".to_string()),
                (400, 11, "app+0x1300".to_string()),
                (500, 10, "[kernel]+0x123".to_string()),
            ]
        );
        // 1000 Hz: a millisecond between two samples.
        assert!(ticks.iter().all(|e| e.duration_ns == 1_000_000));

        // The stack of the third sample, outermost at depth 0, marked as
        // sampled so the report counts it and the functions table does not.
        let mut stack: Vec<_> = events
            .iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL && e.start_ns == 500)
            .collect();
        stack.sort_by_key(|e| e.depth);
        assert_eq!(
            stack.iter().map(|e| name(&ing, e)).collect::<Vec<_>>(),
            ["app+0x11ff", "app+0x1010", "[kernel]+0x123"]
        );
        assert!(stack
            .iter()
            .all(|e| e.extra == extra::SAMPLED_FRAME && e.duration_ns == 1_000_000 && e.tid == 10));
        // One name per address, however many samples it is in.
        let first: Vec<_> = events
            .iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL && e.start_ns == 300)
            .collect();
        assert!(first.iter().any(|e| e.name_id == stack[1].name_id));
        assert_eq!(ing.content_time_bounds(), Some((300, 1_000_500)));
    }

    /// `--switch-events`: when a thread was on a CPU, and when it was
    /// pushed off one while it could still run.
    #[test]
    fn switch_records_become_thread_states_and_slices_when_the_cpu_is_known() {
        let states = |events: &[LiveEvent]| -> Vec<(u64, u64, u8)> {
            let mut s: Vec<_> = events
                .iter()
                .filter(|e| e.kind == kind::THREAD_STATE)
                .map(|e| (e.start_ns, e.duration_ns, e.extra))
                .collect();
            s.sort();
            s
        };
        let (_, events) = ingest_collect(&recording(CALLCHAIN_SAMPLE, true)).unwrap();
        assert_eq!(
            states(&events),
            [
                (220, 200, thread_state::RUNNING),
                // Preempted at 420, back at 460.
                (420, 40, thread_state::RUNNABLE),
                (460, 20, thread_state::RUNNING),
                // Left of its own accord at 480: nothing says what for. On
                // again at 490 and still there when the recording ends.
                (490, 10, thread_state::RUNNING),
            ]
        );
        // A recording of one process does not say which CPU.
        assert!(!events.iter().any(|e| e.kind == kind::SCHEDULING_SLICE));

        let (_, events) = ingest_collect(&recording(CALLCHAIN_SAMPLE | CPU, true)).unwrap();
        let slices: Vec<_> = events
            .iter()
            .filter(|e| e.kind == kind::SCHEDULING_SLICE)
            .map(|e| (e.start_ns, e.duration_ns, e.tid, e.extra))
            .collect();
        assert_eq!(slices.len(), 3);
        assert!(slices.contains(&(220, 200, 11, 3)), "{slices:?}");
        assert_eq!(states(&events).len(), 4);
    }

    /// Pushed a byte at a time and pumped an event at a time, the stream
    /// says what it is reading and ends with the same events.
    #[test]
    fn a_recording_is_sniffed_and_read_however_it_arrives() {
        let bytes = recording(CALLCHAIN_SAMPLE, true);
        let (_, whole) = ingest_collect(&bytes).unwrap();
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut events = Vec::new();
        for byte in &bytes {
            stream.push(std::slice::from_ref(byte));
            events.extend(stream.pump(&mut ing, 1));
        }
        stream.finish_input();
        while !stream.is_done() {
            events.extend(stream.pump(&mut ing, 3));
        }
        assert_eq!(stream.error(), None);
        assert_eq!(stream.format(), Format::PerfData);
        assert_eq!(stream.pending_bytes(), 0);
        let key = |e: &LiveEvent| (e.start_ns, e.kind, e.tid, e.depth, e.duration_ns);
        let (mut a, mut b) = (events.clone(), whole.clone());
        a.sort_by_key(key);
        b.sort_by_key(key);
        assert_eq!(a.len(), b.len());
        assert!(a.iter().zip(&b).all(|(x, y)| key(x) == key(y)));
    }

    #[test]
    fn a_recording_without_timestamps_says_so() {
        use perfr_perf_data::sample_type::{IDENTIFIER, IP, TID};
        let sample_type = IDENTIFIER | IP | TID;
        let e = Event1 { sample_type, id: 7 };
        let bytes = file(
            &[(attr(sample_type, SAMPLE_ID_ALL, 0), vec![7])],
            &[e.sample(1, 1, 0, 0x10, &[])],
            &[],
        );
        let error = ingest_collect(&bytes).err().unwrap();
        assert!(error.contains("perf record -T"), "{error}");
    }
}
