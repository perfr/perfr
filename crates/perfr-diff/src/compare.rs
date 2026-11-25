// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Two sides, each one capture or several, turned into a [`Diff`].
//!
//! A capture is first reduced to tables of units (`Summary`): its time is
//! cut into [`TIME_BATCHES`] equal stretches, and for every function and
//! scope the table says how much of it fell in each. Calls of a scope are
//! batched in the order they were made. The figures and their errors are
//! then ratios of sums over those units ([`crate::stats`]), or over whole
//! captures when each side has at least three.

use std::collections::HashMap;
use std::hash::Hash;

mod tree;

use crate::input::{CaptureInput, SpanKind};
use crate::model::{Diff, FunctionRow, Method, Metric, Outcome, Quantile, ScopeRow, Side, Verdict};
use crate::stats::{benjamini_hochberg, compare, quantile_change_ci, ratio_estimate, Rng, UnitValues};

/// Stretches a capture's time is cut into. Thirty-odd is the usual choice
/// for batch means: enough for the t distribution to be narrow, few enough
/// that a batch is long against how long a program stays in one phase.
pub const TIME_BATCHES: usize = 32;
/// Batches the calls of one scope are cut into, once there are two a batch.
const CALL_BATCHES: usize = 32;
/// Units a side needs for a claim, when the units are batches.
const MIN_BATCH_UNITS: u32 = 5;
/// And when they are captures.
const MIN_CAPTURE_UNITS: u32 = 3;
/// Captures a side for the captures to be the units.
const CAPTURES_AS_UNITS: usize = 3;
/// Durations of one scope a side that the bootstrap of its quantiles walks.
const QUANTILE_VALUES: usize = 4096;
/// Samples a function needs on a side to be tested.
const MIN_SAMPLES: u64 = 10;
/// Call paths the tree is folded into before the rare ones are dropped.
const MAX_TREE_NODES: usize = 2_000_000;
/// Deep enough for any stack worth reading, and shallow enough that the
/// nested document stays within what a JSON reader will descend into.
const MAX_TREE_DEPTH: usize = 48;

/// What sampled time and scope totals are counted against.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Per {
    /// Functions as shares of all samples, scopes per second: the figures
    /// of a sampling report. Shares add up to 100%, so one function taking
    /// more makes every other take less, and a function that got faster
    /// makes the rest look slower.
    Sample,
    /// Time per second of capture: for a program that does the same work
    /// every second on both sides (a game at its frame rate, a server
    /// under the same load).
    #[default]
    Second,
    /// Time per capture: for a program that does a fixed amount of work
    /// from the start of its capture to the end.
    Capture,
    /// Time per call of this scope or frame: the unit of work is named.
    Scope(String),
}

impl Per {
    pub fn parse(text: &str) -> Per {
        match text {
            "sample" | "share" => Per::Sample,
            "" | "second" => Per::Second,
            "capture" => Per::Capture,
            name => Per::Scope(name.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Per::Sample => "sample",
            Per::Second => "second",
            Per::Capture => "capture",
            Per::Scope(name) => name,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub per: Per,
    /// The confidence of every interval; one minus it is the rate of false
    /// findings the verdicts are held to.
    pub confidence: f64,
    /// The relative change a verdict needs, as a fraction.
    pub threshold: f64,
    /// The share of a side (of its samples for a function, of its time for
    /// a scope) a row needs to be tested.
    pub min_share: f64,
    pub seed: u64,
    /// Redraws of the quantile bootstrap.
    pub resamples: usize,
    /// Rows a table keeps beyond the tested ones.
    pub max_rows: usize,
    pub tree: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            per: Per::Second,
            confidence: 0.95,
            threshold: 0.05,
            min_share: 0.005,
            seed: 0x70_65_72_66_72,
            resamples: 600,
            max_rows: 200,
            tree: true,
        }
    }
}

struct Interner<K> {
    index: HashMap<K, u32>,
    keys: Vec<K>,
}

impl<K: Hash + Eq + Clone> Interner<K> {
    fn new() -> Interner<K> {
        Interner { index: HashMap::new(), keys: Vec::new() }
    }

    fn intern(&mut self, key: K) -> u32 {
        if let Some(found) = self.index.get(&key) {
            return *found;
        }
        let id = self.keys.len() as u32;
        self.index.insert(key.clone(), id);
        self.keys.push(key);
        id
    }
}

struct Keys {
    /// A function is its name and the file name of its module: the same
    /// library in another directory is the same library.
    functions: Interner<(String, String)>,
    scopes: Interner<(SpanKind, String)>,
}

struct FnUnits {
    own: Vec<u32>,
    inclusive: Vec<u32>,
}

#[derive(Default)]
struct ScopeData {
    /// `(start, duration)`, by start.
    calls: Vec<(u64, u64)>,
    /// Per time batch, by the call's start.
    batch_ns: Vec<f64>,
    batch_calls: Vec<u32>,
}

/// One capture as tables of units.
struct Summary {
    label: String,
    t0: u64,
    span_ns: u64,
    period_ns: Option<u64>,
    /// Samples per time batch.
    totals: Vec<u32>,
    /// What a function's samples are counted against, per time batch.
    norm_fn: Vec<f64>,
    /// And a scope's time.
    norm_scope: Vec<f64>,
    /// What one sample is worth in the function unit.
    fn_scale: f64,
    functions: HashMap<u32, FnUnits>,
    scopes: HashMap<u32, ScopeData>,
    /// Frame id of this capture to function key.
    frame_keys: HashMap<u32, u32>,
    spans: u64,
    samples: u64,
}

impl Summary {
    fn seconds(&self) -> f64 {
        self.span_ns as f64 / 1e9
    }

    fn batch_seconds(&self) -> f64 {
        self.seconds() / TIME_BATCHES as f64
    }

    fn batch_of(&self, time_ns: u64) -> usize {
        let offset = time_ns.saturating_sub(self.t0) as u128;
        ((offset * TIME_BATCHES as u128 / self.span_ns.max(1) as u128) as usize).min(TIME_BATCHES - 1)
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn summarize(input: &CaptureInput, keys: &mut Keys, per: &Per) -> Result<Summary, String> {
    let mut t0 = u64::MAX;
    let mut t1 = 0u64;
    for span in &input.spans {
        t0 = t0.min(span.start_ns);
        t1 = t1.max(span.start_ns.saturating_add(span.duration_ns));
    }
    for sample in &input.samples {
        t0 = t0.min(sample.timestamp_ns);
        t1 = t1.max(sample.timestamp_ns);
    }
    if t0 == u64::MAX {
        return Err(format!("{}: nothing to compare (no scopes, no samples)", input.label));
    }
    let mut summary = Summary {
        label: input.label.clone(),
        t0,
        span_ns: (t1 - t0).max(1),
        period_ns: input.sample_period_ns.filter(|period| *period > 0),
        totals: vec![0; TIME_BATCHES],
        norm_fn: Vec::new(),
        norm_scope: Vec::new(),
        fn_scale: 1.0,
        functions: HashMap::new(),
        scopes: HashMap::new(),
        frame_keys: HashMap::new(),
        spans: input.spans.len() as u64,
        samples: input.samples.len() as u64,
    };

    let mut name_keys: HashMap<(SpanKind, u32), u32> = HashMap::new();
    for span in &input.spans {
        let key = *name_keys.entry((span.kind, span.name)).or_insert_with(|| {
            let name = input.names.get(span.name as usize).cloned().unwrap_or_default();
            keys.scopes.intern((span.kind, name))
        });
        let batch = summary.batch_of(span.start_ns);
        let data = summary.scopes.entry(key).or_insert_with(|| ScopeData {
            calls: Vec::new(),
            batch_ns: vec![0.0; TIME_BATCHES],
            batch_calls: vec![0; TIME_BATCHES],
        });
        data.calls.push((span.start_ns, span.duration_ns));
        data.batch_ns[batch] += span.duration_ns as f64;
        data.batch_calls[batch] += 1;
    }
    for data in summary.scopes.values_mut() {
        data.calls.sort_unstable();
    }

    // The sample a function was last counted in, so that recursion counts
    // it once.
    let mut counted_in: Vec<u64> = Vec::new();
    for (index, sample) in input.samples.iter().enumerate() {
        let batch = summary.batch_of(sample.timestamp_ns);
        summary.totals[batch] += 1;
        for (depth, frame) in sample.frames.iter().enumerate() {
            let key = *summary.frame_keys.entry(*frame).or_insert_with(|| {
                let (name, module) = match input.frames.get(frame) {
                    Some(frame) if !frame.name.is_empty() => (frame.name.clone(), file_name(&frame.module).to_string()),
                    Some(frame) => ("[unnamed]".to_string(), file_name(&frame.module).to_string()),
                    None => ("[unnamed]".to_string(), String::new()),
                };
                keys.functions.intern((name, module))
            });
            if counted_in.len() <= key as usize {
                counted_in.resize(key as usize + 1, 0);
            }
            let units = summary
                .functions
                .entry(key)
                .or_insert_with(|| FnUnits { own: vec![0; TIME_BATCHES], inclusive: vec![0; TIME_BATCHES] });
            if depth == 0 {
                units.own[batch] += 1;
            }
            if counted_in[key as usize] != index as u64 + 1 {
                counted_in[key as usize] = index as u64 + 1;
                units.inclusive[batch] += 1;
            }
        }
    }

    let batch_seconds = summary.batch_seconds();
    let per_batch = |value: f64| vec![value; TIME_BATCHES];
    (summary.norm_fn, summary.norm_scope) = match per {
        Per::Sample => (summary.totals.iter().map(|n| f64::from(*n)).collect(), per_batch(batch_seconds)),
        Per::Second => (per_batch(batch_seconds), per_batch(batch_seconds)),
        Per::Capture => (per_batch(1.0 / TIME_BATCHES as f64), per_batch(1.0 / TIME_BATCHES as f64)),
        Per::Scope(name) => {
            // A frame series first, then a scope, then anything else of
            // that name.
            let found = [SpanKind::Frame, SpanKind::Scope, SpanKind::Function, SpanKind::Track, SpanKind::Gpu]
                .into_iter()
                .filter_map(|kind| keys.scopes.index.get(&(kind, name.clone())))
                .find_map(|key| summary.scopes.get(key));
            let Some(unit) = found else {
                return Err(format!("{}: no scope or frame named {name:?} to count against", input.label));
            };
            let counts: Vec<f64> = unit.batch_calls.iter().map(|n| f64::from(*n)).collect();
            (counts.clone(), counts)
        }
    };
    Ok(summary)
}

/// The name without its line, and the line, of a frame named the way an
/// interpreter's frames are here: `function (file:line)`.
fn without_line(name: &str) -> Option<(&str, u64)> {
    let inner = name.strip_suffix(')')?;
    let colon = inner.rfind(':')?;
    let line = inner[colon + 1..].parse().ok()?;
    inner[..colon].contains(" (").then_some((&inner[..colon], line))
}

/// Functions that are on one side only because their line number changed:
/// the source was edited above them. A name that is gone from the base and
/// new in the new side at another line of the same file is the same
/// function, when the file has as many of that name left over on both
/// sides; several are paired in the order of their lines. The base side is
/// rewritten to the new side's names. Returns how many were matched.
fn match_moved(keys: &Keys, base: &mut [Summary], new: &[Summary]) -> usize {
    let on = |side: &[Summary], key: u32| side.iter().any(|summary| summary.functions.contains_key(&key));
    let mut groups: HashMap<(&str, &str), [Vec<(u64, u32)>; 2]> = HashMap::new();
    for (key, (name, module)) in keys.functions.keys.iter().enumerate() {
        let key = key as u32;
        let side = match (on(base, key), on(new, key)) {
            (true, false) => 0,
            (false, true) => 1,
            _ => continue,
        };
        if let Some((stem, line)) = without_line(name) {
            groups.entry((stem, module.as_str())).or_default()[side].push((line, key));
        }
    }
    let mut moved: HashMap<u32, u32> = HashMap::new();
    for [mut gone, mut came] in groups.into_values() {
        if gone.is_empty() || gone.len() != came.len() {
            continue;
        }
        gone.sort_unstable();
        came.sort_unstable();
        moved.extend(gone.iter().zip(&came).map(|((_, from), (_, to))| (*from, *to)));
    }
    for summary in base.iter_mut() {
        for (from, to) in &moved {
            if let Some(units) = summary.functions.remove(from) {
                summary.functions.insert(*to, units);
            }
        }
        for key in summary.frame_keys.values_mut() {
            if let Some(to) = moved.get(key) {
                *key = *to;
            }
        }
    }
    moved.len()
}

/// What the units of a comparison are, and what a claim needs of them.
struct Design<'a> {
    options: &'a Options,
    by_capture: bool,
    min_units: u32,
}

impl Design<'_> {
    /// `(y, x)` per unit of a side, from per-batch figures of each capture.
    fn units(&self, side: &[Summary], mut batch: impl FnMut(&Summary, usize) -> (f64, f64)) -> Vec<(f64, f64)> {
        self.units_of(side, |_, summary, index| batch(summary, index))
    }

    /// The same, for figures kept by the capture's place in its side.
    fn units_of(&self, side: &[Summary], mut batch: impl FnMut(usize, &Summary, usize) -> (f64, f64)) -> Vec<(f64, f64)> {
        let mut units = Vec::with_capacity(if self.by_capture { side.len() } else { side.len() * TIME_BATCHES });
        for (capture, summary) in side.iter().enumerate() {
            if self.by_capture {
                let mut whole = (0.0, 0.0);
                for index in 0..TIME_BATCHES {
                    let (y, x) = batch(capture, summary, index);
                    whole.0 += y;
                    whole.1 += x;
                }
                units.push(whole);
            } else {
                units.extend((0..TIME_BATCHES).map(|index| batch(capture, summary, index)));
            }
        }
        units
    }

    fn metric(&self, base: &[(f64, f64)], new: &[(f64, f64)], test: bool) -> Metric {
        let (base, new) = (ratio_estimate(base), ratio_estimate(new));
        let found = if test {
            compare(&base, &new, self.options.confidence, self.min_units)
        } else {
            crate::stats::Comparison {
                delta: new.value - base.value,
                change: (base.value > 0.0).then(|| new.value / base.value - 1.0),
                ..Default::default()
            }
        };
        Metric {
            base: base.value,
            new: new.value,
            delta: found.delta,
            delta_ci: found.delta_ci,
            change: found.change,
            change_ci: found.change_ci,
            p: found.p,
            q: None,
            verdict: Verdict::Untested,
        }
    }

    /// Adjusts the p-values of `metrics` together and gives each its verdict.
    fn judge(&self, metrics: &mut [&mut Metric]) {
        let tested: Vec<usize> = (0..metrics.len()).filter(|index| metrics[*index].p.is_some()).collect();
        let p: Vec<f64> = tested.iter().map(|index| metrics[*index].p.unwrap_or(1.0)).collect();
        let q = benjamini_hochberg(&p);
        let alpha = 1.0 - self.options.confidence;
        for (index, q) in tested.into_iter().zip(q) {
            let metric = &mut *metrics[index];
            metric.q = Some(q);
            // Something that was not there before has no relative change,
            // and is as large a change as there is.
            let large = metric.change.is_none_or(|change| change.abs() >= self.options.threshold);
            metric.verdict = if q <= alpha && large && metric.delta > 0.0 {
                Verdict::Regressed
            } else if q <= alpha && large && metric.delta < 0.0 {
                Verdict::Improved
            } else {
                Verdict::Unchanged
            };
        }
    }
}

/// The units a scope's calls are, in the order they were made: each call
/// while there are few, then [`CALL_BATCHES`] runs of neighbours.
fn call_unit(index: usize, calls: usize) -> usize {
    if calls < 2 * CALL_BATCHES {
        index
    } else {
        index * CALL_BATCHES / calls
    }
}

fn call_units(calls: usize) -> usize {
    calls.min(if calls < 2 * CALL_BATCHES { calls } else { CALL_BATCHES })
}

struct ScopeSide {
    calls: u64,
    total_ns: f64,
    /// Per call: `(duration, 1)` units, batched.
    mean_units: Vec<(f64, f64)>,
    /// Every duration, for the quantiles shown.
    all: UnitValues,
    /// Some of them with their unit, for the bootstrap.
    drawn: UnitValues,
    total_units: Vec<(f64, f64)>,
}

fn scope_side(design: &Design<'_>, side: &[Summary], key: u32) -> ScopeSide {
    let calls: usize = side.iter().filter_map(|summary| summary.scopes.get(&key)).map(|data| data.calls.len()).sum();
    let step = calls.div_ceil(QUANTILE_VALUES).max(1);
    let mut mean_units = Vec::new();
    let mut all = Vec::with_capacity(calls);
    let mut drawn = Vec::new();
    let mut units = 0u32;
    let mut total_ns = 0.0;
    for (capture, summary) in side.iter().enumerate() {
        let Some(data) = summary.scopes.get(&key) else {
            continue;
        };
        let count = data.calls.len();
        let first_unit = if design.by_capture { capture as u32 } else { units };
        let mut batches = vec![(0.0, 0.0); if design.by_capture { 1 } else { call_units(count) }];
        for (index, (_, duration)) in data.calls.iter().enumerate() {
            let unit = if design.by_capture { 0 } else { call_unit(index, count) };
            batches[unit].0 += *duration as f64;
            batches[unit].1 += 1.0;
            all.push((*duration as f64, 0));
            if index % step == 0 {
                drawn.push((*duration as f64, first_unit + unit as u32));
            }
            total_ns += *duration as f64;
        }
        units = if design.by_capture { capture as u32 + 1 } else { units + batches.len() as u32 };
        mean_units.extend(batches);
    }
    let total_units = design.units(side, |summary, batch| {
        let time = summary.scopes.get(&key).map_or(0.0, |data| data.batch_ns[batch]);
        (time / 1e6, summary.norm_scope[batch])
    });
    ScopeSide {
        calls: calls as u64,
        total_ns,
        mean_units,
        all: UnitValues::new(all, 1),
        drawn: UnitValues::new(drawn, units),
        total_units,
    }
}

fn quantile(design: &Design<'_>, base: &ScopeSide, new: &ScopeSide, q: f64, test: bool, rng: &mut Rng) -> Quantile {
    let (base_value, new_value) = (base.all.quantile(q).unwrap_or(0.0), new.all.quantile(q).unwrap_or(0.0));
    let both = base.calls > 0 && new.calls > 0;
    Quantile {
        base: base_value,
        new: new_value,
        change: (both && base_value > 0.0).then(|| new_value / base_value - 1.0),
        change_ci: (test && both)
            .then(|| {
                quantile_change_ci(
                    &base.drawn,
                    &new.drawn,
                    q,
                    design.options.confidence,
                    design.min_units,
                    design.options.resamples,
                    rng,
                )
            })
            .flatten(),
    }
}

fn side_info(side: &[Summary]) -> Side {
    let periods: Vec<u64> = side.iter().filter_map(|summary| summary.period_ns).collect();
    Side {
        captures: side.iter().map(|summary| summary.label.clone()).collect(),
        seconds: side.iter().map(Summary::seconds).sum(),
        samples: side.iter().map(|summary| summary.samples).sum(),
        sample_period_ns: (periods.len() == side.len() && periods.windows(2).all(|pair| pair[0] == pair[1]))
            .then(|| periods.first().copied())
            .flatten(),
        spans: side.iter().map(|summary| summary.spans).sum(),
    }
}

/// Keeps the rows that were tested and the largest of the others, up to
/// `max_rows` of those; says how many were dropped.
fn keep_rows<R>(rows: &mut Vec<R>, max_rows: usize, tested: impl Fn(&R) -> bool, changed: impl Fn(&R) -> bool, size: impl Fn(&R) -> f64) -> u64 {
    rows.sort_by(|a, b| changed(b).cmp(&changed(a)).then(tested(b).cmp(&tested(a))).then(size(b).total_cmp(&size(a))));
    let keep = rows.iter().filter(|row| tested(row)).count() + max_rows;
    let dropped = rows.len().saturating_sub(keep);
    rows.truncate(keep);
    dropped as u64
}

/// Compares `new` against `base`. Each side is one capture or several of
/// the same thing.
pub fn diff(base: &[CaptureInput], new: &[CaptureInput], options: &Options) -> Result<Diff, String> {
    if base.is_empty() || new.is_empty() {
        return Err("a comparison needs a capture on each side".to_string());
    }
    if !(options.confidence > 0.0 && options.confidence < 1.0) {
        return Err(format!("confidence is a fraction between 0 and 1, not {}", options.confidence));
    }
    let mut keys = Keys { functions: Interner::new(), scopes: Interner::new() };
    let mut summarize_side = |side: &[CaptureInput]| -> Result<Vec<Summary>, String> {
        side.iter().map(|input| summarize(input, &mut keys, &options.per)).collect()
    };
    let mut base_side = summarize_side(base)?;
    let new_side = summarize_side(new)?;
    let moved = match_moved(&keys, &mut base_side, &new_side);
    let mut new_side = new_side;

    let by_capture = base.len() >= CAPTURES_AS_UNITS && new.len() >= CAPTURES_AS_UNITS;
    let design =
        Design { options, by_capture, min_units: if by_capture { MIN_CAPTURE_UNITS } else { MIN_BATCH_UNITS } };

    // Sampled time is in milliseconds when every capture says how far
    // apart its samples are, and in samples when one does not.
    let everything = || base_side.iter().chain(new_side.iter());
    let timed = everything().filter(|summary| summary.samples > 0).all(|summary| summary.period_ns.is_some());
    let sampled = (base_side.iter().any(|s| s.samples > 0), new_side.iter().any(|s| s.samples > 0));
    let share = options.per == Per::Sample;
    let function_unit = if share {
        "share"
    } else if timed {
        "ms"
    } else {
        "samples"
    };
    let period_ms = |summary: &Summary| if timed { summary.period_ns.unwrap_or(0) as f64 / 1e6 } else { 1.0 };
    for summary in base_side.iter_mut().chain(new_side.iter_mut()) {
        summary.fn_scale = if share { 1.0 } else { period_ms(summary) };
    }

    let mut notes = Vec::new();
    if by_capture {
        notes.push("Each capture is one unit: the intervals include what differs from one run to the next.".to_string());
    } else {
        notes.push(
            "Each capture is cut into batches in time, and the intervals are those of these captures. \
             What differs between two runs of the same build is not in them: give three or more captures \
             a side to measure it."
                .to_string(),
        );
    }
    match &options.per {
        Per::Sample => notes.push(
            "Shares add up to 100%: a function that takes more makes every other look smaller, and one \
             that takes less makes the others look slower. Count against time or a unit of work (per) \
             for verdicts that stand on their own."
                .to_string(),
        ),
        Per::Second => notes.push(
            "Time is counted per second of capture, which is right for a program that does the same \
             work every second on both sides. For a program that runs flat out, count per capture \
             (the same work in each) or per scope or frame (per): there, something that got slower \
             leaves less of every second to the rest."
                .to_string(),
        ),
        Per::Capture | Per::Scope(_) => {}
    }
    if moved > 0 {
        notes.push(format!(
            "{moved} function{} matched to the same name at another line of the same file: the source moved under {}.",
            if moved == 1 { " is" } else { "s are" },
            if moved == 1 { "it" } else { "them" },
        ));
    }
    match sampled {
        (true, true) => {}
        (false, false) => notes.push("No samples on either side: functions are not compared.".to_string()),
        _ => notes.push("Only one side has samples: functions are not compared.".to_string()),
    }
    if !share && !timed && sampled == (true, true) {
        notes.push("A capture does not say how far apart its samples are: sampled time is counted in samples.".to_string());
    }
    let (base_info, new_info) = (side_info(&base_side), side_info(&new_side));
    if timed && share && sampled == (true, true) && base_info.sample_period_ns != new_info.sample_period_ns {
        notes.push("The two sides were sampled at different rates; shares are compared all the same.".to_string());
    }

    // All sampled time.
    let mut cpu = (sampled == (true, true)).then(|| {
        let cores = matches!(options.per, Per::Sample | Per::Second);
        let units = |side: &[Summary]| {
            design.units(side, |summary, batch| {
                let samples = f64::from(summary.totals[batch]);
                if cores {
                    (samples * period_ms(summary) / if timed { 1e3 } else { 1.0 }, summary.batch_seconds())
                } else {
                    (samples * period_ms(summary), summary.norm_scope[batch])
                }
            })
        };
        design.metric(&units(&base_side), &units(&new_side), true)
    });

    // Functions.
    let mut functions: Vec<FunctionRow> = Vec::new();
    if sampled == (true, true) {
        let count = |side: &[Summary], key: u32, which: fn(&FnUnits) -> &Vec<u32>| -> u64 {
            side.iter()
                .filter_map(|summary| summary.functions.get(&key))
                .map(|units| which(units).iter().map(|n| u64::from(*n)).sum::<u64>())
                .sum()
        };
        let own: fn(&FnUnits) -> &Vec<u32> = |units| &units.own;
        let inclusive: fn(&FnUnits) -> &Vec<u32> = |units| &units.inclusive;
        let (base_total, new_total) = (base_info.samples.max(1) as f64, new_info.samples.max(1) as f64);
        for key in 0..keys.functions.keys.len() as u32 {
            let self_samples = (count(&base_side, key, own), count(&new_side, key, own));
            let inclusive_samples = (count(&base_side, key, inclusive), count(&new_side, key, inclusive));
            if inclusive_samples == (0, 0) {
                // A name the base side no longer goes by (`match_moved`).
                continue;
            }
            let enough = |samples: (u64, u64)| {
                samples.0.max(samples.1) >= MIN_SAMPLES
                    && (samples.0 as f64 / base_total).max(samples.1 as f64 / new_total) >= options.min_share
            };
            let metric = |which: fn(&FnUnits) -> &Vec<u32>, test: bool| {
                let units = |side: &[Summary]| {
                    design.units(side, |summary, batch| {
                        let samples = summary.functions.get(&key).map_or(0, |units| which(units)[batch]);
                        (f64::from(samples) * summary.fn_scale, summary.norm_fn[batch])
                    })
                };
                design.metric(&units(&base_side), &units(&new_side), test)
            };
            let (name, module) = keys.functions.keys[key as usize].clone();
            functions.push(FunctionRow {
                name,
                module,
                self_samples,
                inclusive_samples,
                self_time: metric(own, enough(self_samples)),
                inclusive: metric(inclusive, enough(inclusive_samples)),
            });
        }
    }

    // Scopes.
    let mut rng = Rng::new(options.seed);
    let mut scopes: Vec<ScopeRow> = Vec::new();
    let (base_ns, new_ns) = (base_info.seconds.max(1e-9) * 1e9, new_info.seconds.max(1e-9) * 1e9);
    for key in 0..keys.scopes.keys.len() as u32 {
        let (base, new) = (scope_side(&design, &base_side, key), scope_side(&design, &new_side, key));
        let test = (base.total_ns / base_ns).max(new.total_ns / new_ns) >= options.min_share;
        let both = base.calls > 0 && new.calls > 0;
        let (kind, name) = keys.scopes.keys[key as usize].clone();
        scopes.push(ScopeRow {
            name,
            kind,
            calls: (base.calls, new.calls),
            mean: design.metric(&base.mean_units, &new.mean_units, test && both),
            median: quantile(&design, &base, &new, 0.5, test, &mut rng),
            p95: quantile(&design, &base, &new, 0.95, test, &mut rng),
            total: design.metric(&base.total_units, &new.total_units, test),
        });
    }

    {
        let mut metrics: Vec<&mut Metric> = Vec::new();
        metrics.extend(cpu.as_mut());
        for row in &mut functions {
            metrics.push(&mut row.self_time);
            metrics.push(&mut row.inclusive);
        }
        for row in &mut scopes {
            metrics.push(&mut row.mean);
            metrics.push(&mut row.total);
        }
        design.judge(&mut metrics);
    }

    let mut outcome = Outcome::default();
    let mut count = |metrics: [&Metric; 2]| {
        if metrics.iter().any(|metric| metric.p.is_some()) {
            outcome.tested += 1;
        }
        if metrics.iter().any(|metric| metric.verdict == Verdict::Regressed) {
            outcome.regressed += 1;
        } else if metrics.iter().any(|metric| metric.verdict == Verdict::Improved) {
            outcome.improved += 1;
        }
    };
    functions.iter().for_each(|row| count([&row.self_time, &row.inclusive]));
    scopes.iter().for_each(|row| count([&row.mean, &row.total]));

    let functions_omitted = keep_rows(
        &mut functions,
        options.max_rows,
        |row| row.self_time.p.is_some() || row.inclusive.p.is_some(),
        |row| row.self_time.changed() || row.inclusive.changed(),
        |row| row.self_time.delta.abs().max(row.inclusive.delta.abs() * 1e-3),
    );
    let scopes_omitted = keep_rows(
        &mut scopes,
        options.max_rows,
        |row| row.mean.p.is_some() || row.total.p.is_some(),
        |row| row.mean.changed() || row.total.changed(),
        |row| row.total.delta.abs(),
    );

    let tree = (options.tree && sampled == (true, true))
        .then(|| tree::build([base, new], [&base_side, &new_side], &keys, &design))
        .flatten();

    Ok(Diff {
        base: base_info,
        new: new_info,
        method: Method {
            per: options.per.label().to_string(),
            function_unit: function_unit.to_string(),
            cpu_unit: match (matches!(options.per, Per::Sample | Per::Second), timed) {
                (true, true) => "cores",
                (true, false) => "samples/s",
                (false, true) => "ms",
                (false, false) => "samples",
            }
            .to_string(),
            confidence: options.confidence,
            threshold: options.threshold,
            min_share: options.min_share,
            units: if by_capture { "captures" } else { "batches" }.to_string(),
            notes,
        },
        outcome,
        cpu,
        functions,
        functions_omitted,
        scopes,
        scopes_omitted,
        tree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_name_with_a_file_and_a_line_has_a_line_to_leave_out() {
        assert_eq!(without_line("tick (game.py:14)"), Some(("tick (game.py", 14)));
        assert_eq!(without_line("<module> (train.py:1)"), Some(("<module> (train.py", 1)));
        for name in ["main", "f (a.py)", "ns::f(int)", "std::vector<int>::size()", "operator() (native)", "f (a.py:x)", "a:1)"] {
            assert_eq!(without_line(name), None, "{name}");
        }
    }

    #[test]
    fn calls_are_their_own_units_until_there_are_two_a_batch() {
        assert_eq!((call_units(0), call_units(1), call_units(63)), (0, 1, 63));
        assert_eq!((call_units(64), call_units(10_000)), (32, 32));
        assert_eq!((call_unit(5, 63), call_unit(0, 64), call_unit(63, 64), call_unit(9_999, 10_000)), (5, 0, 31, 31));
        // Runs of neighbours, all the same length to within one call.
        let sizes = (0..100).fold([0u32; 32], |mut sizes, index| {
            sizes[call_unit(index, 100)] += 1;
            sizes
        });
        assert!(sizes.iter().all(|size| (3..=4).contains(size)), "{sizes:?}");
    }

    #[test]
    fn a_module_is_its_file_name_and_what_time_is_counted_against_has_a_name() {
        assert_eq!(file_name("/usr/lib/libc.so.6"), "libc.so.6");
        assert_eq!(file_name("C:\\game\\bin\\game.exe"), "game.exe");
        assert_eq!(file_name("train.py"), "train.py");
        for (text, per) in [("", Per::Second), ("share", Per::Sample), ("sample", Per::Sample), ("second", Per::Second), ("capture", Per::Capture)] {
            assert_eq!(Per::parse(text), per);
        }
        assert_eq!(Per::parse("frame"), Per::Scope("frame".to_string()));
        assert_eq!(Per::parse("frame").label(), "frame");
    }
}
