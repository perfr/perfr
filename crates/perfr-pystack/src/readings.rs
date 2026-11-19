// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the reader has read lately: every thread's Python stack by time, who
//! held the GIL, and what Python calls each thread.
//!
//! One thread writes ([`Readings::record`]) and the capture loop reads, a
//! few thousand times a second each, under one mutex held for a lookup or an
//! append. Stacks are kept for [`KEEP_NS`]: long enough for a sample that
//! waited out the capture's reorder window to still find its neighbours, and
//! a sample that must wait longer (for symbols) takes its neighbours with it
//! ([`Near`] holds them).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

/// How long a reading is kept.
const KEEP_NS: u64 = 2_000_000_000;

/// The deepest Python stack kept, innermost frames first. The capture keeps
/// 64 frames of a merged stack; this leaves room for a merge to choose.
const MAX_FRAMES: usize = 128;

/// Distinct Python functions named in one capture. A torn read can produce a
/// name that never existed; this is what bounds how many of those are kept.
const MAX_SYMBOLS: usize = 1 << 18;

/// How sure the readings can get of a thread's outermost frame, in readings
/// that agreed; see [`Inner::is_torn`].
const ROOT_TRUST: i8 = 16;

/// GIL spans held back while only one thread has been seen holding it.
const MAX_PENDING_SPANS: usize = 4096;

/// One Python function, as the reports show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// `function (file.py:line)`, the line being where the function is
    /// defined: two functions of one name in one file are two rows, and the
    /// row says where to look.
    pub name: String,
    /// The source file, relative to the `sys.path` entry it was imported
    /// from when py-spy could tell (`json/encoder.py`).
    pub file: String,
}

/// One frame of a Python stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Index into the symbols, see [`Readings::symbol`].
    pub symbol: u32,
    /// The outermost frame of a run: the one the interpreter's eval function
    /// was entered for. See [`merge`](crate::merge).
    pub entry: bool,
}

/// A frame as it was read, before it has an id.
#[derive(Clone, Copy, Debug)]
pub struct RawFrame<'a> {
    pub name: &'a str,
    pub file: &'a str,
    /// The line the function is defined at; 0 or less when unknown.
    pub line: i32,
    pub entry: bool,
}

/// One thread of one reading.
#[derive(Clone, Debug)]
pub struct ThreadReading<'a> {
    /// The kernel's thread id.
    pub tid: u32,
    /// `threading.Thread.name`, when it could be read.
    pub name: Option<&'a str>,
    pub holds_gil: bool,
    /// Innermost first.
    pub frames: Vec<RawFrame<'a>>,
}

/// A stretch of time one thread was seen holding the GIL on every reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GilSpan {
    pub tid: u32,
    pub start_ns: u64,
    pub end_ns: u64,
}

/// The readings of one thread on either side of an instant.
#[derive(Clone, Debug, Default)]
pub struct Near {
    /// The closer of the two in time.
    pub nearest: Option<Arc<[Frame]>>,
    /// The one on the other side, when there is one close enough.
    pub other: Option<Arc<[Frame]>>,
    /// Both sides are there and are the same stack: the thread was, as far
    /// as two looks can tell, in the same Python function throughout.
    pub steady: bool,
}

impl Near {
    pub fn is_empty(&self) -> bool {
        self.nearest.is_none()
    }
}

/// Where the reader is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Still finding the interpreter in the process.
    Looking,
    /// Reading stacks of this Python version.
    Reading { version: String },
    /// Not reading, and why.
    Stopped { why: String },
}

/// Counts for the capture's summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Readings that gave every thread's stack.
    pub readings: u64,
    /// Readings that failed part-way: the target changed what was being read.
    pub failed: u64,
    /// Stacks of single threads, in readings that otherwise succeeded, that
    /// stopped short of the thread's outermost frame and were dropped.
    pub torn: u64,
    /// Time between readings now. It grows when one reading costs more than
    /// a fifth of it, so the reader never takes more than a fifth of a core.
    pub period_ns: u64,
    /// The first and last successful reading.
    pub first_ns: u64,
    pub last_ns: u64,
    /// Python threads in the last reading.
    pub threads: u32,
}

struct Reading {
    time_ns: u64,
    stack: Arc<[Frame]>,
}

#[derive(Default)]
struct Gil {
    /// Who holds it, and since when.
    holder: Option<(u32, u64)>,
    /// The reading before this one.
    last_ns: u64,
    pending: VecDeque<GilSpan>,
    /// The first two distinct holders: whether the GIL ever changed hands.
    holders: Vec<u32>,
}

impl Gil {
    fn push(&mut self, span: GilSpan) {
        if span.end_ns <= span.start_ns {
            return;
        }
        if self.pending.len() >= MAX_PENDING_SPANS {
            self.pending.pop_front();
        }
        self.pending.push_back(span);
    }

    /// One reading: `now` holds the GIL at `time_ns`, or nobody does.
    ///
    /// Two readings are a period apart and the GIL changed hands somewhere
    /// between them, so a span ends, and the next begins, half-way. Readings
    /// further apart than `stall_ns` say nothing about the time between: the
    /// span ends a period after the last look that saw it held.
    fn note(&mut self, time_ns: u64, now: Option<u32>, period_ns: u64, stall_ns: u64) {
        let first = self.last_ns == 0;
        let stalled = !first && time_ns.saturating_sub(self.last_ns) > stall_ns;
        let boundary = if first || stalled {
            time_ns
        } else {
            self.last_ns + (time_ns - self.last_ns) / 2
        };
        if let Some((tid, since)) = self.holder {
            if stalled || now != Some(tid) {
                let end_ns = if stalled {
                    self.last_ns + period_ns.min(stall_ns)
                } else {
                    boundary
                };
                self.push(GilSpan {
                    tid,
                    start_ns: since,
                    end_ns,
                });
                self.holder = None;
            }
        }
        if self.holder.is_none() {
            if let Some(tid) = now {
                self.holder = Some((tid, boundary));
                if self.holders.len() < 2 && !self.holders.contains(&tid) {
                    self.holders.push(tid);
                }
            }
        }
        self.last_ns = time_ns;
    }

    fn close(&mut self) {
        if let Some((tid, since)) = self.holder.take() {
            self.push(GilSpan {
                tid,
                start_ns: since,
                end_ns: self.last_ns,
            });
        }
    }
}

struct Inner {
    symbols: Vec<Symbol>,
    /// file -> function -> (line, id). Three levels so that a frame seen
    /// before is found by its borrowed strings, with no allocation: that is
    /// nearly every frame of every reading.
    symbol_ids: HashMap<Box<str>, HashMap<Box<str>, Vec<(i32, u32)>>>,
    threads: HashMap<u32, VecDeque<Reading>>,
    /// Each thread's outermost frame and how many readings in a row, up to
    /// [`ROOT_TRUST`], have had it there.
    roots: HashMap<u32, (u32, i8)>,
    gil: Gil,
    names: HashMap<u32, String>,
    renamed: Vec<u32>,
    state: State,
    stats: Stats,
    scratch: Vec<Frame>,
}

/// See the module documentation.
pub struct Readings {
    inner: Mutex<Inner>,
}

/// A name made safe to show: no control characters, and not arbitrarily
/// long. What is read is whatever was in the target's memory at that moment.
fn clean(text: &str, max_chars: usize) -> String {
    let mut out: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

impl Inner {
    fn symbol_id(&mut self, frame: &RawFrame<'_>) -> u32 {
        if let Some(lines) = self
            .symbol_ids
            .get(frame.file)
            .and_then(|names| names.get(frame.name))
        {
            if let Some((_, id)) = lines.iter().find(|(line, _)| *line == frame.line) {
                return *id;
            }
        }
        if self.symbols.len() >= MAX_SYMBOLS {
            // Shared by everything past the limit, like the capture's own
            // "(other)" row. Id 0 is always there by then.
            return 0;
        }
        let id = self.symbols.len() as u32;
        let (name, file) = (clean(frame.name, 160), clean(frame.file, 200));
        let shown = if frame.line > 0 {
            format!("{name} ({file}:{})", frame.line)
        } else {
            format!("{name} ({file})")
        };
        self.symbols.push(Symbol { name: shown, file });
        self.symbol_ids
            .entry(frame.file.into())
            .or_default()
            .entry(frame.name.into())
            .or_default()
            .push((frame.line, id));
        id
    }

    /// Whether a stack whose outermost frame is `root` stops short of the
    /// bottom of its thread's stack.
    ///
    /// The target is not paused, and a frame is linked to its caller a
    /// moment after it becomes the thread's current frame (a generator's is
    /// linked each time it is resumed, and unlinked each time it yields). A
    /// read in that moment follows the chain from the innermost frame and
    /// finds its end at once: a stack of one or two frames that is not the
    /// thread's stack, and would be joined to a sample as if it were.
    ///
    /// What gives it away is where it ends. A thread's outermost Python
    /// frame is the same for as long as the thread lives (`<module>`, or
    /// `Thread._bootstrap`), so a stack that ends anywhere else is torn.
    /// The outermost frame is learnt by majority: each reading that agrees
    /// adds to its standing, each that does not takes away, and a thread
    /// that really has a new one (an id reused by another thread, or a
    /// thread C code calls different Python functions on) is believed once
    /// the old one's standing is spent, within [`ROOT_TRUST`] readings.
    fn is_torn(&mut self, tid: u32, root: u32) -> bool {
        let known = self.roots.entry(tid).or_insert((root, 0));
        if known.0 == root {
            known.1 = (known.1 + 1).min(ROOT_TRUST);
            return false;
        }
        known.1 -= 1;
        if known.1 < 0 {
            *known = (root, 1);
            return false;
        }
        true
    }

    /// How far from a sample a reading may be and still be joined to it, and
    /// how far apart two readings may be and still bracket a GIL hand-over.
    fn tolerance_ns(&self) -> u64 {
        (3 * self.stats.period_ns).max(5_000_000)
    }
}

impl Readings {
    /// `period_ns` is the time between readings the reader starts with.
    pub fn new(period_ns: u64) -> Readings {
        Readings {
            inner: Mutex::new(Inner {
                symbols: Vec::new(),
                symbol_ids: HashMap::new(),
                threads: HashMap::new(),
                roots: HashMap::new(),
                gil: Gil::default(),
                names: HashMap::new(),
                renamed: Vec::new(),
                state: State::Looking,
                stats: Stats {
                    period_ns,
                    ..Stats::default()
                },
                scratch: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while the lock was held leaves plain data behind, none of
        // it half-written in a way a reader could not survive.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One reading of every Python thread, taken at `time_ns` on the
    /// capture's clock.
    pub fn record(&self, time_ns: u64, threads: &[ThreadReading<'_>]) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let mut holder = None;
        for thread in threads {
            if thread.holds_gil {
                holder = Some(thread.tid);
            }
            if let Some(name) = thread.name {
                if inner.names.get(&thread.tid).map(String::as_str) != Some(name) {
                    let name = clean(name, 120);
                    if inner.names.get(&thread.tid) != Some(&name) {
                        inner.names.insert(thread.tid, name);
                        if !inner.renamed.contains(&thread.tid) {
                            inner.renamed.push(thread.tid);
                        }
                    }
                }
            }
            let mut scratch = std::mem::take(&mut inner.scratch);
            scratch.clear();
            for raw in thread.frames.iter().take(MAX_FRAMES) {
                let symbol = inner.symbol_id(raw);
                scratch.push(Frame {
                    symbol,
                    entry: raw.entry,
                });
            }
            // The outermost frame as it was read, not as the depth limit
            // left it.
            if let Some(outermost) = thread.frames.last() {
                let root = inner.symbol_id(outermost);
                if inner.is_torn(thread.tid, root) {
                    inner.stats.torn += 1;
                    inner.scratch = scratch;
                    continue;
                }
            }
            let list = inner.threads.entry(thread.tid).or_default();
            // A thread in the same place as last time, which is most threads
            // most of the time, shares that reading's stack.
            let stack = match list.back() {
                Some(last) if last.stack[..] == scratch[..] => last.stack.clone(),
                _ => Arc::from(&scratch[..]),
            };
            list.push_back(Reading { time_ns, stack });
            inner.scratch = scratch;
        }
        let horizon = time_ns.saturating_sub(KEEP_NS);
        inner.threads.retain(|_, list| {
            while list
                .front()
                .is_some_and(|reading| reading.time_ns < horizon)
            {
                list.pop_front();
            }
            !list.is_empty()
        });
        let threads_kept = &inner.threads;
        inner.roots.retain(|tid, _| threads_kept.contains_key(tid));
        let (period_ns, stall_ns) = (inner.stats.period_ns, inner.tolerance_ns());
        inner.gil.note(time_ns, holder, period_ns, stall_ns);
        if inner.stats.readings == 0 {
            inner.stats.first_ns = time_ns;
        }
        inner.stats.readings += 1;
        inner.stats.last_ns = time_ns;
        inner.stats.threads = threads.len() as u32;
    }

    /// A reading that failed.
    pub fn record_failure(&self) {
        self.lock().stats.failed += 1;
    }

    /// The reader's pace changed.
    pub fn set_period(&self, period_ns: u64) {
        self.lock().stats.period_ns = period_ns;
    }

    pub fn set_state(&self, state: State) {
        self.lock().state = state;
    }

    pub fn state(&self) -> State {
        self.lock().state.clone()
    }

    pub fn stats(&self) -> Stats {
        self.lock().stats
    }

    /// No more readings will come: the GIL span still open ends at the last
    /// reading.
    pub fn finish(&self) {
        self.lock().gil.close();
    }

    /// The readings of `tid` on either side of `time_ns`, those that are
    /// within three reading periods of it (and at least 5 ms: a reader that
    /// was briefly late is still better than no Python frames).
    pub fn near(&self, tid: u32, time_ns: u64) -> Near {
        let inner = self.lock();
        let Some(list) = inner.threads.get(&tid) else {
            return Near::default();
        };
        let tolerance = inner.tolerance_ns();
        let after = list.partition_point(|reading| reading.time_ns < time_ns);
        let close = |index: usize| {
            list.get(index)
                .filter(|reading| reading.time_ns.abs_diff(time_ns) <= tolerance)
        };
        let before = after.checked_sub(1).and_then(close);
        let after = close(after);
        match (before, after) {
            (Some(a), Some(b)) => {
                let steady = Arc::ptr_eq(&a.stack, &b.stack) || a.stack == b.stack;
                let (nearest, other) = if time_ns - a.time_ns <= b.time_ns - time_ns {
                    (a, b)
                } else {
                    (b, a)
                };
                Near {
                    nearest: Some(nearest.stack.clone()),
                    other: (!steady).then(|| other.stack.clone()),
                    steady,
                }
            }
            (Some(one), None) | (None, Some(one)) => Near {
                nearest: Some(one.stack.clone()),
                other: None,
                steady: false,
            },
            (None, None) => Near::default(),
        }
    }

    /// The distinct stacks `tid` was read in within the same reach of
    /// `time_ns` as [`near`](Self::near) has, nearest first, at most `max`.
    /// For a sample the two readings beside it do not explain: a call from C
    /// into Python that happens thousands of times a second (a generator
    /// being drained, a key function) is on the stack in some readings and
    /// not in others, and the two nearest may both have missed it.
    pub fn around(&self, tid: u32, time_ns: u64, max: usize, out: &mut Vec<Arc<[Frame]>>) {
        out.clear();
        let inner = self.lock();
        let Some(list) = inner.threads.get(&tid) else {
            return;
        };
        let tolerance = inner.tolerance_ns();
        let middle = list.partition_point(|reading| reading.time_ns < time_ns);
        // Walk outwards from the sample, taking whichever side is nearer.
        let (mut before, mut after) = (middle, middle);
        while out.len() < max {
            let earlier = before
                .checked_sub(1)
                .and_then(|i| list.get(i))
                .map(|r| time_ns - r.time_ns);
            let later = list.get(after).map(|r| r.time_ns - time_ns);
            let reading = match (earlier, later) {
                (Some(e), Some(l)) if e <= l && e <= tolerance => {
                    before -= 1;
                    &list[before]
                }
                (_, Some(l)) if l <= tolerance => {
                    after += 1;
                    &list[after - 1]
                }
                (Some(e), _) if e <= tolerance => {
                    before -= 1;
                    &list[before]
                }
                _ => break,
            };
            if !out
                .iter()
                .any(|known| Arc::ptr_eq(known, &reading.stack) || known[..] == reading.stack[..])
            {
                out.push(reading.stack.clone());
            }
        }
    }

    /// What a symbol id stands for.
    pub fn symbol(&self, id: u32) -> Option<Symbol> {
        self.lock().symbols.get(id as usize).cloned()
    }

    /// The GIL spans finished since the last call, oldest first. Nothing
    /// until a second thread has held the GIL: one thread holding it
    /// whenever it runs is what its state bar already shows.
    pub fn take_gil_spans(&self, out: &mut Vec<GilSpan>) {
        let mut inner = self.lock();
        if inner.gil.holders.len() >= 2 {
            out.extend(inner.gil.pending.drain(..));
        }
    }

    /// The threads Python has named, or renamed, since the last call.
    pub fn take_thread_names(&self) -> Vec<(u32, String)> {
        let mut inner = self.lock();
        let renamed = std::mem::take(&mut inner.renamed);
        renamed
            .into_iter()
            .filter_map(|tid| Some((tid, inner.names.get(&tid)?.clone())))
            .collect()
    }

    /// What Python calls a thread, if it has said.
    pub fn thread_name(&self, tid: u32) -> Option<String> {
        self.lock().names.get(&tid).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn frame<'a>(name: &'a str, file: &'a str, line: i32) -> RawFrame<'a> {
        RawFrame {
            name,
            file,
            line,
            entry: false,
        }
    }

    fn thread<'a>(tid: u32, holds_gil: bool, frames: Vec<RawFrame<'a>>) -> ThreadReading<'a> {
        ThreadReading {
            tid,
            name: None,
            holds_gil,
            frames,
        }
    }

    fn names(readings: &Readings, stack: &[Frame]) -> Vec<String> {
        stack
            .iter()
            .map(|f| readings.symbol(f.symbol).unwrap().name)
            .collect()
    }

    #[test]
    fn a_function_is_named_with_where_it_is_defined() {
        let readings = Readings::new(MS);
        readings.record(
            10 * MS,
            &[thread(
                7,
                false,
                vec![
                    frame("crunch", "app/work.py", 12),
                    frame("<module>", "app/work.py", 1),
                ],
            )],
        );
        let near = readings.near(7, 10 * MS);
        let stack = near.nearest.unwrap();
        assert_eq!(
            names(&readings, &stack),
            ["crunch (app/work.py:12)", "<module> (app/work.py:1)"]
        );
        assert_eq!(
            readings.symbol(stack[0].symbol).unwrap().file,
            "app/work.py"
        );
    }

    #[test]
    fn two_functions_of_one_name_are_two_symbols_and_one_seen_again_is_one() {
        let readings = Readings::new(MS);
        let lambdas = vec![
            frame("<lambda>", "a.py", 3),
            frame("<lambda>", "a.py", 9),
            frame("<lambda>", "b.py", 3),
        ];
        readings.record(MS, &[thread(1, false, lambdas.clone())]);
        readings.record(2 * MS, &[thread(1, false, lambdas)]);
        let stack = readings.near(1, 2 * MS).nearest.unwrap();
        assert_eq!(
            stack.iter().map(|f| f.symbol).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert!(
            readings.symbol(3).is_none(),
            "the second reading named nothing new"
        );
    }

    #[test]
    fn a_name_read_from_a_torn_string_is_made_safe_to_show() {
        let readings = Readings::new(MS);
        let long = "x".repeat(5000);
        readings.record(
            MS,
            &[thread(
                1,
                false,
                vec![frame(&long, "a\u{0}b\n.py", 0), frame("", "", -4)],
            )],
        );
        let stack = readings.near(1, MS).nearest.unwrap();
        let first = readings.symbol(stack[0].symbol).unwrap();
        assert_eq!(first.file, "ab.py");
        assert_eq!(first.name, format!("{} (ab.py)", "x".repeat(160)));
        assert_eq!(readings.symbol(stack[1].symbol).unwrap().name, "? (?)");
    }

    #[test]
    fn a_sample_gets_the_reading_nearest_to_it_and_the_one_on_the_other_side() {
        let readings = Readings::new(MS);
        let main = frame("main", "f.py", 9);
        readings.record(
            10 * MS,
            &[thread(1, false, vec![frame("a", "f.py", 1), main])],
        );
        readings.record(
            11 * MS,
            &[thread(1, false, vec![frame("b", "f.py", 2), main])],
        );
        let near = readings.near(1, 10 * MS + 300_000);
        assert_eq!(
            names(&readings, &near.nearest.unwrap()),
            ["a (f.py:1)", "main (f.py:9)"]
        );
        assert_eq!(
            names(&readings, &near.other.unwrap()),
            ["b (f.py:2)", "main (f.py:9)"]
        );
        assert!(!near.steady);
        let near = readings.near(1, 10 * MS + 700_000);
        assert_eq!(names(&readings, &near.nearest.unwrap())[0], "b (f.py:2)");
    }

    #[test]
    fn two_equal_readings_around_a_sample_are_steady_and_share_one_stack() {
        let readings = Readings::new(MS);
        for t in 10..13 {
            readings.record(t * MS, &[thread(1, false, vec![frame("a", "f.py", 1)])]);
        }
        let near = readings.near(1, 11 * MS + 1);
        assert!(near.steady);
        assert!(near.other.is_none(), "the other side has nothing to add");
        let (first, last) = (
            readings.near(1, 10 * MS).nearest.unwrap(),
            readings.near(1, 12 * MS).nearest.unwrap(),
        );
        assert!(Arc::ptr_eq(&first, &last));
    }

    #[test]
    fn the_stacks_around_a_sample_come_nearest_first_each_once_and_only_from_close_by() {
        let readings = Readings::new(MS);
        let stack = |name: &'static str| vec![frame(name, "f.py", 1), frame("main", "f.py", 9)];
        for (at, name) in [
            (1, "far"),
            (10, "a"),
            (11, "b"),
            (12, "a"),
            (13, "c"),
            (14, "d"),
        ] {
            readings.record(at * MS, &[thread(1, false, stack(name))]);
        }
        let mut around = Vec::new();
        let named = |around: &Vec<Arc<[Frame]>>| -> Vec<String> {
            around
                .iter()
                .map(|s| readings.symbol(s[0].symbol).unwrap().name[..1].to_string())
                .collect()
        };
        // From 11.9 ms: 12 (a), 11 (b), 13 (c), 10 (a again, not repeated), 14 (d). Not 1 ms.
        readings.around(1, 11 * MS + 900_000, 8, &mut around);
        assert_eq!(named(&around), ["a", "b", "c", "d"]);
        readings.around(1, 11 * MS + 900_000, 2, &mut around);
        assert_eq!(named(&around), ["a", "b"]);
        readings.around(2, 11 * MS, 8, &mut around);
        assert!(around.is_empty());
        readings.around(1, 40 * MS, 8, &mut around);
        assert!(around.is_empty());
    }

    #[test]
    fn a_reading_too_far_from_the_sample_is_not_its_stack() {
        let readings = Readings::new(MS);
        readings.record(10 * MS, &[thread(1, false, vec![frame("a", "f.py", 1)])]);
        assert!(!readings.near(1, 14 * MS).is_empty(), "within 5 ms");
        assert!(readings.near(1, 16 * MS).is_empty());
        assert!(readings.near(2, 10 * MS).is_empty(), "another thread");
        // A slower reader is allowed three of its own periods.
        readings.set_period(4 * MS);
        assert!(!readings.near(1, 21 * MS).is_empty());
        assert!(readings.near(1, 23 * MS).is_empty());
    }

    #[test]
    fn readings_are_kept_for_two_seconds_and_a_thread_that_ended_is_forgotten() {
        let readings = Readings::new(MS);
        readings.record(
            MS,
            &[
                thread(1, false, vec![frame("a", "f.py", 1)]),
                thread(2, false, vec![]),
            ],
        );
        readings.record(1900 * MS, &[thread(1, false, vec![frame("a", "f.py", 1)])]);
        assert!(!readings.near(1, MS).is_empty());
        readings.record(2100 * MS, &[thread(1, false, vec![frame("a", "f.py", 1)])]);
        assert!(readings.near(1, MS).is_empty(), "older than two seconds");
        assert!(readings.near(2, MS).is_empty());
        assert!(!readings.near(1, 1900 * MS).is_empty());
    }

    #[test]
    fn a_stack_that_stops_short_of_the_threads_outermost_frame_is_torn_and_dropped() {
        let readings = Readings::new(MS);
        let whole = || {
            vec![
                frame("genexpr", "g.py", 2),
                frame("squares", "g.py", 1),
                frame("<module>", "g.py", 9),
            ]
        };
        let no_generator = || vec![frame("squares", "g.py", 1), frame("<module>", "g.py", 9)];
        // Read while the generator's frame was not yet linked to its caller.
        let torn = || vec![frame("genexpr", "g.py", 2)];
        readings.record(10 * MS, &[thread(1, false, whole())]);
        readings.record(11 * MS, &[thread(1, false, no_generator())]);
        readings.record(12 * MS, &[thread(1, false, torn())]);
        readings.record(13 * MS, &[thread(1, false, whole())]);
        assert_eq!(readings.stats().torn, 1);
        // The sample at the torn reading's instant gets its neighbours.
        let near = readings.near(1, 12 * MS);
        assert_eq!(near.nearest.unwrap().len(), 2);
        let mut around = Vec::new();
        readings.around(1, 12 * MS, 8, &mut around);
        assert!(
            around.iter().all(|stack| stack.len() >= 2),
            "the torn stack is among those around"
        );
        // A thread with no Python frames is not torn.
        readings.record(14 * MS, &[thread(1, false, vec![])]);
        assert_eq!(readings.stats().torn, 1);
    }

    #[test]
    fn a_new_outermost_frame_is_believed_once_it_has_outlasted_the_old_one() {
        let readings = Readings::new(MS);
        let on = |at: u64, root: &'static str| {
            readings.record(
                at * MS,
                &[thread(
                    1,
                    false,
                    vec![frame("f", "a.py", 3), frame(root, "a.py", 1)],
                )],
            )
        };
        // The interpreter starts in one module and settles in another; or
        // the kernel gave a new thread an old thread's id.
        for at in 0..3 {
            on(at, "site");
        }
        for at in 3..10 {
            on(at, "<module>");
        }
        // Three readings had built `site` a standing of three: three of the
        // new one are dropped, the fourth is believed, and so are the rest.
        assert_eq!(readings.stats().torn, 3);
        assert!(
            readings.near(1, 4 * MS).nearest.is_some(),
            "the old one's readings are still there"
        );
        let stack = readings.near(1, 9 * MS).nearest.unwrap();
        assert_eq!(
            readings.symbol(stack[1].symbol).unwrap().name,
            "<module> (a.py:1)"
        );
        // A first reading that was itself torn is taken for the thread's
        // stack, and costs the one good reading it takes to outvote it.
        let readings = Readings::new(MS);
        readings.record(MS, &[thread(1, false, vec![frame("genexpr", "g.py", 2)])]);
        for at in 2..6 {
            readings.record(
                at * MS,
                &[thread(
                    1,
                    false,
                    vec![frame("f", "g.py", 3), frame("<module>", "g.py", 1)],
                )],
            );
        }
        assert_eq!(readings.stats().torn, 1);
        assert_eq!(readings.near(1, 5 * MS).nearest.unwrap().len(), 2);
    }

    #[test]
    fn a_deep_recursion_keeps_its_innermost_frames() {
        let readings = Readings::new(MS);
        let deep: Vec<RawFrame<'_>> = (0..500).map(|line| frame("f", "r.py", line + 1)).collect();
        readings.record(MS, &[thread(1, false, deep)]);
        let stack = readings.near(1, MS).nearest.unwrap();
        assert_eq!(stack.len(), MAX_FRAMES);
        assert_eq!(readings.symbol(stack[0].symbol).unwrap().name, "f (r.py:1)");
    }

    fn spans(readings: &Readings) -> Vec<(u32, u64, u64)> {
        let mut out = Vec::new();
        readings.take_gil_spans(&mut out);
        out.iter()
            .map(|s| (s.tid, s.start_ns / 100_000, s.end_ns / 100_000))
            .collect()
    }

    fn holders(readings: &Readings, at_ms: u64, holder: Option<u32>) {
        let threads: Vec<ThreadReading<'_>> = [1, 2]
            .iter()
            .map(|tid| thread(*tid, holder == Some(*tid), vec![]))
            .collect();
        readings.record(at_ms * MS, &threads);
    }

    #[test]
    fn the_gil_changes_hands_half_way_between_two_readings() {
        let readings = Readings::new(MS);
        holders(&readings, 10, Some(1));
        holders(&readings, 11, Some(1));
        holders(&readings, 12, Some(2));
        holders(&readings, 13, Some(2));
        holders(&readings, 14, None);
        holders(&readings, 15, Some(1));
        // In tenths of a millisecond.
        assert_eq!(spans(&readings), [(1, 100, 115), (2, 115, 135)]);
        readings.finish();
        assert_eq!(spans(&readings), [(1, 145, 150)]);
    }

    #[test]
    fn one_thread_holding_the_gil_alone_makes_no_lane() {
        let readings = Readings::new(MS);
        for t in 10..20 {
            holders(&readings, t, (t % 3 != 0).then_some(1));
        }
        readings.finish();
        assert_eq!(spans(&readings), []);
        // Until a second thread holds it: then what was held back comes too.
        let readings = Readings::new(MS);
        holders(&readings, 10, Some(1));
        holders(&readings, 11, None);
        assert_eq!(spans(&readings), []);
        holders(&readings, 12, Some(2));
        holders(&readings, 13, None);
        assert_eq!(spans(&readings), [(1, 100, 105), (2, 115, 125)]);
    }

    #[test]
    fn a_reader_that_stalled_does_not_claim_the_time_it_did_not_look() {
        let readings = Readings::new(MS);
        holders(&readings, 10, Some(1));
        holders(&readings, 11, Some(1));
        // Forty milliseconds with no reading, then the same holder.
        holders(&readings, 51, Some(1));
        holders(&readings, 52, Some(2));
        assert_eq!(spans(&readings), [(1, 100, 120), (1, 510, 515)]);
    }

    #[test]
    fn a_thread_python_names_or_renames_is_reported_once() {
        let readings = Readings::new(MS);
        let named = |name: &'static str| ThreadReading {
            tid: 9,
            name: Some(name),
            holds_gil: false,
            frames: vec![],
        };
        readings.record(MS, &[named("worker-0")]);
        readings.record(2 * MS, &[named("worker-0")]);
        assert_eq!(readings.take_thread_names(), [(9, "worker-0".to_string())]);
        assert_eq!(readings.take_thread_names(), []);
        readings.record(3 * MS, &[named("worker-0 (busy)")]);
        assert_eq!(
            readings.take_thread_names(),
            [(9, "worker-0 (busy)".to_string())]
        );
        assert_eq!(readings.thread_name(9).as_deref(), Some("worker-0 (busy)"));
    }

    #[test]
    fn readings_and_failures_are_counted() {
        let readings = Readings::new(MS);
        assert_eq!(readings.state(), State::Looking);
        readings.record(
            5 * MS,
            &[thread(1, false, vec![]), thread(2, false, vec![])],
        );
        readings.record(6 * MS, &[thread(1, false, vec![])]);
        readings.record_failure();
        let stats = readings.stats();
        assert_eq!(
            (
                stats.readings,
                stats.failed,
                stats.first_ns,
                stats.last_ns,
                stats.threads
            ),
            (2, 1, 5 * MS, 6 * MS, 1)
        );
    }
}
