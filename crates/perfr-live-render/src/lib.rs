// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pixel-column rasterizer for the live trace view.
//!
//! # Why not one quad per scope
//!
//! The owner's hunch was that a live view of millions of scopes must be
//! painted from the **pixels**, not by walking every scope every frame.
//! That hunch is correct for perfr's cheap live events:
//!
//! * `FunctionCall`, paired `ApiScopeStart`/`Stop`, `SchedulingSlice`, and
//!   `ThreadStateSlice` are **non-overlapping per lane** (`(kind, tid, depth)`
//!   or `(kind, core)`).
//! * A per-lane vector sorted by `start_ns` can answer "which interval covers
//!   this pixel column?" with a binary search on `end_ns`.
//! * Hot path: **O(lanes × width × log n_lane)**, plus one comparison per 64
//!   events under a column: a column that covers many events shows the
//!   longest of them, found through each lane's per-block longest spans
//!   rather than by visiting them. GPU then draws one textured quad.
//!
//! That is the zoomed-out level of detail. Zoomed in, when some visible
//! scope is a few pixels wide, the view is a list of instances instead, one
//! per visible event (see `lod`), and costs what is in view.
//!
//! Alternatives considered (and left as benches, not the default):
//!
//! * **Naive instancing** (`rasterize_naive`): one fill per event in view.
//!   O(n) or worse O(n × pixels_touched). Falls over at millions; a lane
//!   uses it only while it has fewer events than pixels, or has overlap.
//! * **GPU compute binning**: still O(n) visits per dirty frame; more moving
//!   parts; the CPU column walk is already cheaper than uploading n quads.
//! * **Mip / tile pyramid**: O(width) lookup after ingest, extra memory and
//!   rebuild on ring wrap. Attractive later; not required once the column
//!   walk is O(width log n).
//!
//! Numbers come from `cargo bench -p perfr-live-render`. This file does not
//! invent timings.

use std::collections::BTreeMap;

use perfr_live_event::{chrome, kind, InternTable, LaneKey, LiveEvent};

/// Visible `[start, end)` span. VALUE stores f32 bits in `duration_ns`.
fn event_span_ns(e: &LiveEvent) -> u64 {
    e.end_ns().saturating_sub(e.start_ns).max(1)
}

mod lod;
mod par;
mod shaders;
pub use lod::{
    collect_instances_cached, ListingCache,ThreadFocus, 
    apply_highlight_flags, choose_lod, choose_lod_hint, collect_instances,
    collect_instances_layout, collect_instances_layout_opts,
    instance_for_event, lane_gap, lane_height, leaf_label, pick_column_event, pick_instance_at,
    sample_lod_lanes, sort_thread_leaves, stack_height, stack_height_keys,
    stacked_layout, value_lanes_in_view, CollectOpts, InstanceFrame,
    ScopeInstance, ScopePick, TimelineLod, YCull, FLAG_DIMMED, FLAG_HOVER, FLAG_NONE,
    FLAG_SELECTED, FLAG_SIBLING, INSTANCE_MIN_PX, Y_CULL_PAD, FLAG_INACTIVE, FLAG_SAME_PID};
pub use par::{is_parallel, parallelism, set_wasm_pool_threads, WorkerSpan};
pub use shaders::{BLIT_RECT_WGSL, INSTANCE_WGSL};

/// Events per entry of [`Lane::block_span`].
const SPAN_BLOCK: usize = 64;

/// `from + s[from..].partition_point(pred)`, found by doubling steps from
/// `from`: a handful of comparisons next to where the search starts when the
/// answer is near it, and never worse than a binary search over the rest.
/// `pred` must be true for a prefix of `s[from..]` and false after it.
fn gallop<T>(s: &[T], from: usize, pred: impl Fn(&T) -> bool) -> usize {
    if from >= s.len() || !pred(&s[from]) {
        return from.min(s.len());
    }
    // `pred(s[lo])` holds throughout.
    let mut lo = from;
    let mut step = 1usize;
    loop {
        let hi = lo.saturating_add(step);
        if hi >= s.len() {
            return lo + 1 + s[lo + 1..].partition_point(&pred);
        }
        if !pred(&s[hi]) {
            return lo + 1 + s[lo + 1..hi].partition_point(&pred);
        }
        lo = hi;
        step <<= 1;
    }
}

/// One horizontal lane of intervals, sorted by `start_ns`.
///
/// perfr live scopes are non-overlapping per lane, so `end_ns` is also
/// non-decreasing and [`Self::first_ending_after`] is a binary search.
/// Chrome ingest also places 1 ns instants / marks on the same tid as B/E/X
/// slices; those break the end order. `ends_sorted` tracks that, and a lane
/// that lost it searches `latest_end` instead.
#[derive(Clone, Debug)]
pub struct Lane {
    events: Vec<LiveEvent>,
    ends_sorted: bool,
    /// For a lane whose ends are out of order: the latest end among
    /// `events[..=k]`, for each `k`. Empty while `ends_sorted`.
    ///
    /// It never decreases, so the first event ending after `t` is a binary
    /// search on it. Without it that was a scan from the lane's first
    /// event, on every listing, pick and raster, and one instant inside a
    /// slice anywhere in an imported trace's lane was enough to need it.
    latest_end: Vec<u64>,
    /// Bumped by every insert and every retain that removed something: the
    /// listing cache's way to know a lane is exactly what it listed before.
    version: u64,
    /// The longest span among each [`SPAN_BLOCK`] consecutive events.
    ///
    /// "The longest event in this range" is asked once per pixel column by
    /// the raster and once per lane, over the whole window, by the LOD
    /// decision on every painted frame. Walking the range made both linear
    /// in the events in view; with this a block that cannot hold the answer
    /// is passed in one comparison.
    block_span: Vec<u64>,
    /// The longest span in the lane, 0 when it is empty. When that one is
    /// too short to be drawn as a box, nothing in any window of the lane is,
    /// and the LOD decision needs no search at all: zoomed out, that is
    /// every lane on every frame.
    longest_span: u64,
}

impl Default for Lane {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            ends_sorted: true,
            latest_end: Vec::new(),
            version: 0,
            block_span: Vec::new(),
            longest_span: 0,
        }
    }
}

impl Lane {
    pub fn events(&self) -> &[LiveEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Changes whenever the events do.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn insert(&mut self, event: LiveEvent) {
        self.version = self.version.wrapping_add(1);
        let i = if self
            .events
            .last()
            .map(|e| e.start_ns <= event.start_ns)
            .unwrap_or(true)
        {
            let i = self.events.len();
            self.events.push(event);
            i
        } else {
            let i = self
                .events
                .partition_point(|e| e.start_ns <= event.start_ns);
            self.events.insert(i, event);
            i
        };
        let end = self.events[i].end_ns();
        if self.ends_sorted {
            let prev_ok = i == 0 || self.events[i - 1].end_ns() <= end;
            let next_ok = i + 1 >= self.events.len() || end <= self.events[i + 1].end_ns();
            self.ends_sorted = prev_ok && next_ok;
            if !self.ends_sorted {
                self.rebuild_latest_end();
            }
        } else {
            let before = if i == 0 { 0 } else { self.latest_end[i - 1] };
            self.latest_end.insert(i, before.max(end));
            // Every later entry now covers the new event too. They only
            // grow, so the first that is already as late ends the fix-up.
            for latest in &mut self.latest_end[i + 1..] {
                if *latest >= end {
                    break;
                }
                *latest = end;
            }
        }
        // An insert can only raise it; a retain recomputes it.
        self.longest_span = self.longest_span.max(event_span_ns(&self.events[i]));
        if i + 1 == self.events.len() {
            let span = event_span_ns(&self.events[i]);
            match self.block_span.get_mut(i / SPAN_BLOCK) {
                Some(longest) => *longest = (*longest).max(span),
                None => self.block_span.push(span),
            }
        } else {
            // An insert in the middle moved every later event one place
            // along, across the block boundaries. It already cost a move of
            // that tail; reading the tail once more keeps this simple.
            self.rebuild_block_span(i / SPAN_BLOCK);
        }
    }

    fn rebuild_latest_end(&mut self) {
        self.latest_end.clear();
        if self.ends_sorted {
            return;
        }
        let mut latest = 0;
        self.latest_end.extend(self.events.iter().map(|e| {
            latest = latest.max(e.end_ns());
            latest
        }));
    }

    fn rebuild_block_span(&mut self, from_block: usize) {
        self.block_span.truncate(from_block);
        self.block_span.extend(
            self.events[(from_block * SPAN_BLOCK).min(self.events.len())..]
                .chunks(SPAN_BLOCK)
                .map(|block| block.iter().map(event_span_ns).max().unwrap_or(0)),
        );
    }

    pub fn extend<I: IntoIterator<Item = LiveEvent>>(&mut self, events: I) {
        for e in events {
            self.insert(e);
        }
    }

    /// Drop every event the predicate rejects, keeping start order.
    pub fn retain<F: FnMut(&LiveEvent) -> bool>(&mut self, mut f: F) {
        let before = self.events.len();
        self.events.retain(|e| f(e));
        if self.events.len() != before {
            self.version = self.version.wrapping_add(1);
            // Removing events can only restore end order, never break it, but
            // a lane that was already unsorted may now be sorted -- recheck so
            // it does not stay on the linear fallback forever.
            self.ends_sorted = self.ends_are_sorted();
            self.rebuild_latest_end();
            self.rebuild_block_span(0);
            self.longest_span = self.block_span.iter().copied().max().unwrap_or(0);
        }
    }

    /// First event with `end_ns > t`.
    ///
    /// Non-overlapping start-sorted lanes have non-decreasing `end_ns`
    /// (`e[i].end <= e[i+1].start <= e[i+1].end`), so this is `partition_point`
    /// on `end_ns() <= t`. Chrome 1 ns instants on the same lane break that
    /// order; then the search is on the latest end so far, which finds the
    /// same event: where that first exceeds `t`, it is that event's own
    /// end. A long parent that still covers `t` is not skipped.
    pub fn first_ending_after(&self, t: u64) -> usize {
        if self.ends_sorted {
            self.events.partition_point(|e| e.end_ns() <= t)
        } else {
            self.latest_end.partition_point(|&latest| latest <= t)
        }
    }

    /// `end_ns` non-decreasing. True for non-overlapping start-sorted lanes.
    pub fn ends_are_sorted(&self) -> bool {
        self.events
            .windows(2)
            .all(|w| w[0].end_ns() <= w[1].end_ns())
    }

    /// The event that owns `[col0, col1)`, if any overlaps it; see
    /// [`Self::last_overlapping`].
    pub fn overlapping(&self, col0: u64, col1: u64) -> Option<&LiveEvent> {
        self.last_overlapping(col0, col1)
    }

    /// Event that owns `[col0, col1)`.
    ///
    /// Longest true `[start, end)` wins so a 1 ns instant cannot steal a
    /// pixel (or pick) from a longer same-lane scope that occupies the
    /// column. Equal duration keeps last-in-start-order (the previous
    /// last-write-wins tie-break).
    ///
    /// Two binary searches, then about one comparison per [`SPAN_BLOCK`]
    /// events in the range. A lane whose ends are out of order is walked
    /// event by event: there the range also holds events that ended before
    /// `col0`, which a block's longest span says nothing about.
    pub fn last_overlapping(&self, col0: u64, col1: u64) -> Option<&LiveEvent> {
        let i = self.first_ending_after(col0);
        let j = self.events.partition_point(|e| e.start_ns < col1);
        if self.ends_sorted {
            return self.longest_in(i, j).map(|k| &self.events[k]);
        }
        let mut best: Option<usize> = None;
        let mut best_dur = 0u64;
        for k in i..j {
            let e = &self.events[k];
            if e.end_ns() <= col0 {
                continue;
            }
            let d = event_span_ns(e);
            if best.is_none() || d >= best_dur {
                best = Some(k);
                best_dur = d;
            }
        }
        best.map(|k| &self.events[k])
    }

    /// Index of the longest of `events[i..j]`, the last of them when several
    /// are as long.
    fn longest_in(&self, i: usize, j: usize) -> Option<usize> {
        if i >= j {
            return None;
        }
        let scan = |from: usize, to: usize, best: &mut Option<(u64, usize)>| {
            for k in from..to {
                let d = event_span_ns(&self.events[k]);
                if best.is_none_or(|(longest, _)| d >= longest) {
                    *best = Some((d, k));
                }
            }
        };
        let mut best = None;
        // Whole blocks are `first..last`; the ragged ends are walked.
        let first = i.div_ceil(SPAN_BLOCK);
        let last = j / SPAN_BLOCK;
        if first >= last {
            scan(i, j, &mut best);
            return best.map(|(_, k)| k);
        }
        scan(i, first * SPAN_BLOCK, &mut best);
        let mut block: Option<(u64, usize)> = None;
        for b in first..last {
            let d = self.block_span[b];
            if block.is_none_or(|(longest, _)| d >= longest) {
                block = Some((d, b));
            }
        }
        if let Some((d, b)) = block {
            if best.is_none_or(|(longest, _)| d >= longest) {
                // Later than anything before it, so it wins a tie; within
                // the block the last event that long is the one.
                let k = (b * SPAN_BLOCK..(b + 1) * SPAN_BLOCK)
                    .rev()
                    .find(|&k| event_span_ns(&self.events[k]) == d);
                best = k.map(|k| (d, k)).or(best);
            }
        }
        scan(last * SPAN_BLOCK, j, &mut best);
        best.map(|(_, k)| k)
    }

    /// Whether an event overlapping `[t0, t1)` has a duration `wide` accepts.
    /// `wide` must be monotonic: true for a duration, true for any longer.
    ///
    /// The LOD decision asks this of every lane on every painted frame, so
    /// it stops at the first such event and passes whole blocks that have
    /// none.
    pub(crate) fn any_duration(&self, t0: u64, t1: u64, wide: impl Fn(u64) -> bool) -> bool {
        // Before the two searches below, each a dozen cold reads of the
        // lane: a lane whose longest span is not wide has no wide duration
        // anywhere.
        if !wide(self.longest_span) {
            return false;
        }
        let i = self.first_ending_after(t0);
        let j = self.events.partition_point(|e| e.start_ns < t1);
        let hit = |k: usize| {
            let e = &self.events[k];
            e.end_ns() > t0 && wide(e.duration_ns)
        };
        let mut k = i;
        while k < j {
            if k % SPAN_BLOCK == 0 && k + SPAN_BLOCK <= j {
                // A span is the duration (1 ns for a duration of zero):
                // if the longest span is not wide, no duration here is.
                if !wide(self.block_span[k / SPAN_BLOCK]) {
                    k += SPAN_BLOCK;
                    continue;
                }
            }
            if hit(k) {
                return true;
            }
            k += 1;
        }
        false
    }

    /// Production path: walk events when there are fewer of them than pixels,
    /// otherwise one binary search per column. Benches of the two raw paths
    /// (`cargo bench -p perfr-live-render`) show the column walk is the one
    /// that stays sub-linear in n; naive wins only while `n ≤ width`.
    pub fn rasterize(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        out: &mut [u32],
        intern: Option<&InternTable>,
    ) {
        // Mixed 1 ns + long scopes break the end order; the column walk
        // would then visit, for each column, every event since the last
        // long one. One pass over the events in view stays linear in them.
        if !self.ends_sorted || self.events.len() <= width {
            self.rasterize_naive(t0, t1, width, out, intern);
        } else {
            self.rasterize_pixel_columns(t0, t1, width, out, intern);
        }
    }

    /// Rasterize `width` columns covering `[t0, t1)` into `out`.
    ///
    /// Each column is one binary search. This is the live-view hot path
    /// once `n > width`.
    pub fn rasterize_pixel_columns(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        out: &mut [u32],
        intern: Option<&InternTable>,
    ) {
        assert!(out.len() >= width);
        if width == 0 || t1 <= t0 {
            out[..width].fill(chrome::TRACK);
            return;
        }
        let dt = (t1 - t0) as f64 / width as f64;
        let column = |x: usize| {
            let col0 = t0.saturating_add((x as f64 * dt) as u64);
            let col1 = t0
                .saturating_add(((x + 1) as f64 * dt) as u64)
                .max(col0.saturating_add(1));
            (col0, col1)
        };
        if !self.ends_sorted {
            // No order to walk along: each column asks from scratch.
            for (x, px) in out[..width].iter_mut().enumerate() {
                let (col0, col1) = column(x);
                *px = self.last_overlapping(col0, col1).map(|e| e.color_for(intern)).unwrap_or(chrome::TRACK);
            }
            return;
        }
        // The columns go left to right and the events are in order of start
        // and, here, of end: the range of events a column covers begins and
        // ends at or after the one before it. Two cursors that only move
        // forward find it in a few steps from where they were. Asked afresh
        // per column, the same two bounds were two binary searches over the
        // whole lane, each a dozen cache misses, and that was most of what a
        // zoomed-out frame cost.
        let (mut i, mut j) = (0usize, 0usize);
        // The owner of the column before, and its colour: a scope a few
        // columns wide is hashed to a colour once, not once per column.
        let mut last: Option<(usize, u32)> = None;
        for (x, px) in out[..width].iter_mut().enumerate() {
            let (col0, col1) = column(x);
            i = gallop(&self.events, i, |e| e.end_ns() <= col0);
            j = gallop(&self.events, j.max(i), |e| e.start_ns < col1);
            *px = match self.longest_in(i, j) {
                None => chrome::TRACK,
                Some(k) => match last {
                    Some((owner, color)) if owner == k => color,
                    _ => {
                        let color = self.events[k].color_for(intern);
                        last = Some((k, color));
                        color
                    }
                },
            };
        }
    }

    /// Walk the events in view and fill their pixels: O(events in view).
    /// [`Self::rasterize`] uses it for a lane with fewer events than pixels
    /// and for a lane with overlap; the benches use it as the baseline the
    /// column walk must beat on a dense lane.
    pub fn rasterize_naive(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        out: &mut [u32],
        intern: Option<&InternTable>,
    ) {
        assert!(out.len() >= width);
        out[..width].fill(chrome::TRACK);
        if width == 0 || t1 <= t0 {
            return;
        }
        let span = (t1 - t0) as f64;
        let mut owner = vec![None; width];
        let first = self.first_ending_after(t0);
        let past = self.events.partition_point(|e| e.start_ns < t1).max(first);
        for (ei, e) in self.events[first..past].iter().enumerate() {
            let ei = first + ei;
            if e.end_ns() <= t0 || e.start_ns >= t1 {
                continue;
            }
            let x0 = (((e.start_ns.max(t0) - t0) as f64 / span) * width as f64).floor() as usize;
            let x1 = (((e.end_ns().min(t1) - t0) as f64 / span) * width as f64).ceil() as usize;
            let x1 = x1.max(x0 + 1).min(width);
            let color = e.color_for(intern);
            let new_d = event_span_ns(e);
            for x in x0.min(width)..x1 {
                let overwrite = match owner[x] {
                    None => true,
                    Some(prev) => new_d >= event_span_ns(&self.events[prev]),
                };
                if overwrite {
                    owner[x] = Some(ei);
                    out[x] = color;
                }
            }
        }
    }
}

/// Time-ordered per-lane index. Insert is append-mostly (live stream).
#[derive(Clone, Debug, Default)]
pub struct TrackIndex {
    lanes: BTreeMap<LaneKey, Lane>,
    /// Bumped whenever the *set* of lanes changes -- a lane appears, or
    /// lanes are dropped -- and not on every event. The track strip keys its
    /// per-thread lane catalogue on this, so a live stream of events into
    /// existing lanes costs it nothing.
    lane_gen: u64,
    /// Running total, so `event_count` is a read rather than a walk over every
    /// lane (it is consulted several times per frame).
    events: usize,
    /// Running time bounds, maintained on insert, so `time_bounds` is a read.
    /// It used to walk every event of every lane, and was called once per
    /// WebSocket batch -- on a busy live capture that was most of the drain.
    bounds: Bounds,
    /// See [`TrackIndex::set_extent_floor`]. 0 when there is none.
    extent_floor: u64,
    /// See [`TrackIndex::revision`].
    revision: u64,
}

/// Min/max over all events and over "real" ones -- a zero-width mark at t=0
/// (Chrome metadata leftovers, missing-`ts` instants) must not stretch the
/// capture to the origin when a later cluster exists.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    min_all: u64,
    max_all: u64,
    min_real: u64,
    max_real: u64,
    any_real: bool,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds { min_all: u64::MAX, max_all: 0, min_real: u64::MAX, max_real: 0, any_real: false }
    }
}

impl Bounds {
    /// `floor` is the index's extent floor: an event that begins before it
    /// counts like one of the viewer's own rows, in the fallback only.
    fn add(&mut self, e: &LiveEvent, floor: u64) {
        let start = e.start_ns;
        let end = e.end_ns();
        self.min_all = self.min_all.min(start);
        self.max_all = self.max_all.max(end);
        // The viewer's own rows (its self-profile, the server's) are placed
        // on the capture clock wherever the live edge happens to be, and
        // they keep arriving between captures. They are not the capture:
        // letting them into the real bounds stretched the navigable range
        // seconds before a capture's first event.
        let own = perfr_live_event::dev::is_self_pid(e.pid);
        if !own && start >= floor && (start > 0 || e.duration_ns > 1) {
            self.any_real = true;
            self.min_real = self.min_real.min(start);
            self.max_real = self.max_real.max(end);
        }
    }
}

impl TrackIndex {
    pub fn insert(&mut self, event: LiveEvent) {
        use std::collections::btree_map::Entry;
        let lane = match self.lanes.entry(event.lane_key()) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                self.lane_gen = self.lane_gen.wrapping_add(1);
                e.insert(Lane::default())
            }
        };
        lane.insert(event);
        self.events += 1;
        self.revision = self.revision.wrapping_add(1);
        self.bounds.add(&event, self.extent_floor);
    }

    /// Changes whenever the events held change: an insert, a clear, a
    /// retain. Two reads that agree bracket an index that answers every
    /// question as it did, so a caller can keep an answer that cost a walk
    /// (the level of detail of a view) for as long as this stands still.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Where the capture begins, when that is known apart from its events.
    /// An event that begins before this is still held and drawn, but does
    /// not widen [`Self::time_bounds`] (unless nothing else is held).
    ///
    /// A capture cut to a window keeps every event that overlaps the window
    /// whole. One that spans it -- a frame loop's outer scope, a thread
    /// asleep throughout -- begins before the window and ends after it, and
    /// counted in the bounds it made a two-second cut open as wide as the
    /// capture it was cut from, with data in the middle only.
    ///
    /// [`Self::clear`] removes the floor: what is held next is another
    /// capture, with its own beginning.
    pub fn set_extent_floor(&mut self, start_ns: u64) {
        if self.extent_floor != start_ns {
            self.extent_floor = start_ns;
            self.rescan_bounds();
        }
    }

    fn rescan_bounds(&mut self) {
        let mut b = Bounds::default();
        for lane in self.lanes.values() {
            for e in &lane.events {
                b.add(e, self.extent_floor);
            }
        }
        self.bounds = b;
    }

    /// Changes only when a lane is added or removed. See the field.
    pub fn lane_gen(&self) -> u64 {
        self.lane_gen
    }

    pub fn extend<I: IntoIterator<Item = LiveEvent>>(&mut self, events: I) {
        for e in events {
            self.insert(e);
        }
    }

    pub fn clear(&mut self) {
        self.lanes.clear();
        self.events = 0;
        self.bounds = Bounds::default();
        self.extent_floor = 0;
        self.lane_gen = self.lane_gen.wrapping_add(1);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Drop every event the predicate rejects, and any lane left empty.
    ///
    /// An empty lane is not a lane: `TrackStrip` builds the thread rows from
    /// [`Self::lanes`], so leaving one behind keeps a row (and its height) on
    /// screen with nothing in it.
    pub fn retain<F: FnMut(&LiveEvent) -> bool>(&mut self, mut f: F) {
        for lane in self.lanes.values_mut() {
            lane.retain(&mut f);
        }
        let before = self.lanes.len();
        self.lanes.retain(|_, lane| !lane.is_empty());
        if self.lanes.len() != before {
            self.lane_gen = self.lane_gen.wrapping_add(1);
        }
        self.events = self.lanes.values().map(Lane::len).sum();
        self.revision = self.revision.wrapping_add(1);
        // Dropping events can move either bound; rescan (retain is rare).
        self.rescan_bounds();
    }

    pub fn lane_count(&self) -> usize {
        self.lanes.len()
    }

    pub fn event_count(&self) -> usize {
        self.events
    }

    pub fn lanes(&self) -> impl Iterator<Item = (LaneKey, &Lane)> {
        self.lanes.iter().map(|(k, v)| (*k, v))
    }

    pub fn lane(&self, key: LaneKey) -> Option<&Lane> {
        self.lanes.get(&key)
    }

    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let b = &self.bounds;
        // Asked of the count, not of `min_all` still being at its initial
        // `u64::MAX`: an event can start there (a JSON `ts` of 1e30
        // saturates to it), and the `+ 1` below must not overflow for it.
        if self.events == 0 {
            None
        } else if b.any_real {
            Some((b.min_real, b.max_real.max(b.min_real.saturating_add(1))))
        } else {
            Some((b.min_all, b.max_all.max(b.min_all.saturating_add(1))))
        }
    }

    /// Flattened `lanes × width` RGBA8 pixels (row-major, one row per lane).
    pub fn rasterize_pixel(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        intern: Option<&InternTable>,
    ) -> RasterizedFrame {
        let keys: Vec<LaneKey> = self.lanes.keys().copied().collect();
        self.rasterize_pixel_layout(t0, t1, width, &keys, None, None, intern, false)
    }

    /// Pixel-column raster with optional stack-Y cull. `ys` is `(lane, y)` in
    /// the same space as [`lod::stacked_layout`]; when `y_cull` is set, off-
    /// screen lanes are skipped before the column walk.
    #[allow(clippy::too_many_arguments)]
    pub fn rasterize_pixel_layout(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        order: &[LaneKey],
        ys: Option<&[(LaneKey, f32)]>,
        y_cull: Option<YCull>,
        intern: Option<&InternTable>,
        // Time each lane's raster separately and name its span after the lane
        // (its tid), instead of one span per worker chunk. Only when the Self
        // pane is open -- it costs two clock reads per lane.
        per_lane_spans: bool,
    ) -> RasterizedFrame {
        let ymap: Option<std::collections::BTreeMap<LaneKey, f32>> =
            ys.map(|l| l.iter().copied().collect());
        let keys: Vec<LaneKey> = if order.is_empty() {
            match ys {
                // Legacy / index-only: no layout means every non-VALUE lane.
                None => self
                    .lanes
                    .keys()
                    .copied()
                    .filter(|k| k.kind != kind::VALUE)
                    .collect(),
                // Explicit empty layout (collapsed-all): paint nothing.
                Some(_) => Vec::new(),
            }
        } else {
            order
                .iter()
                .copied()
                .filter(|k| k.kind != kind::VALUE && self.lanes.contains_key(k))
                .collect()
        };
        let keys: Vec<LaneKey> = if let (Some(cull), Some(ym)) = (y_cull, ymap.as_ref()) {
            keys.into_iter()
                .filter(|k| {
                    let y = ym.get(k).copied().unwrap_or(0.0);
                    cull.hits(y, lane_height(*k) + lane_gap(*k))
                })
                .collect()
        } else {
            keys
        };
        let mut pixels = vec![0u32; keys.len() * width];
        let worker_spans = if per_lane_spans {
            // Name each lane's span after its thread (tid), resolvable on the
            // Self pane through the shared intern table.
            par::for_each_row_lanes_labeled(
                &keys,
                &mut pixels,
                width,
                |k: &LaneKey| k.tid,
                |key, dest| {
                    self.lanes[key].rasterize(t0, t1, width, dest, intern);
                },
            )
        } else {
            par::for_each_row_lanes(&keys, &mut pixels, width, |key, dest| {
                self.lanes[key].rasterize(t0, t1, width, dest, intern);
            })
        };
        RasterizedFrame {
            width,
            lanes: keys,
            pixels,
            worker_spans,
        }
    }

    /// Every lane through [`Lane::rasterize_naive`], on the calling thread.
    /// Not a renderer: the baseline the benches and the scaling test hold
    /// [`Self::rasterize_pixel`] against.
    pub fn rasterize_naive(
        &self,
        t0: u64,
        t1: u64,
        width: usize,
        intern: Option<&InternTable>,
    ) -> RasterizedFrame {
        let keys: Vec<LaneKey> = self.lanes.keys().copied().collect();
        let mut pixels = vec![0u32; keys.len() * width];
        for (row, key) in keys.iter().enumerate() {
            let dest = &mut pixels[row * width..(row + 1) * width];
            self.lanes[key].rasterize_naive(t0, t1, width, dest, intern);
        }
        RasterizedFrame {
            width,
            lanes: keys,
            pixels,
            worker_spans: Vec::new(),
        }
    }
}

/// A raster pixel (`0xAARRGGBB`) as the four bytes a texture wants.
fn argb_to_rgba8(p: u32) -> [u8; 4] {
    [(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]
}

#[derive(Clone, Debug)]
pub struct RasterizedFrame {
    pub width: usize,
    pub lanes: Vec<LaneKey>,
    pub pixels: Vec<u32>,
    pub worker_spans: Vec<WorkerSpan>,
}

impl RasterizedFrame {
    pub fn row(&self, i: usize) -> &[u32] {
        let w = self.width;
        &self.pixels[i * w..(i + 1) * w]
    }

    /// Pack as RGBA8 bytes for a WebGPU / Canvas texture upload.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len() * 4);
        for p in &self.pixels {
            out.extend_from_slice(&argb_to_rgba8(*p));
        }
        out
    }

    /// Lanes this frame actually rasterized, paired with their layout Y and
    /// sorted top-down. Falls back to a self-stacked layout when none of the
    /// raster's lanes appear in `layout`.
    fn placed_ys(&self, layout: &[(LaneKey, f32)], s: f32) -> Vec<(LaneKey, f32)> {
        if layout.is_empty() {
            return Vec::new();
        }
        let ys: BTreeMap<LaneKey, f32> = layout.iter().copied().collect();
        let mut placed: Vec<(LaneKey, f32)> = self
            .lanes
            .iter()
            .filter_map(|k| ys.get(k).copied().map(|y| (*k, y)))
            .collect();
        if placed.is_empty() {
            let mut y = 0.0;
            placed = self
                .lanes
                .iter()
                .map(|k| {
                    let at = y;
                    y += (lod::lane_height(*k) + lod::lane_gap(*k)) * s;
                    (*k, at)
                })
                .collect();
        }
        placed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        placed
    }

    fn extent_of(placed: &[(LaneKey, f32)], s: f32) -> (f32, u32) {
        let Some(&(_, origin)) = placed.first() else {
            return (0.0, 1);
        };
        let bot = placed
            .iter()
            .map(|(k, y)| *y + (lod::lane_height(*k) + lod::lane_gap(*k)) * s)
            .fold(origin, f32::max);
        (origin, (bot - origin).round().max(1.0) as u32)
    }

    /// Layout-space Y of the first rasterized row, and the row count
    /// [`Self::to_rgba8_placed`] will emit. The pixel-column blit's dest rect
    /// must be built from these — recomputing the extent from the full layout
    /// disagrees whenever the rasterizer dropped lanes (VALUE kinds, lanes with
    /// no events, anything `y_cull` cut), which stretches and offsets the blit.
    pub fn placed_extent(&self, layout: &[(LaneKey, f32)], scale: f32) -> (f32, u32) {
        let s = scale.max(0.01);
        if self.lanes.is_empty() || self.width == 0 {
            return (0.0, 1);
        }
        Self::extent_of(&self.placed_ys(layout, s), s)
    }

    /// Place each raster row at `layout` Y (already strip-scaled). Gaps between
    /// lanes stay transparent so header washes show through; dest height is
    /// last_y + lane_h − first_y, matching instanced clip space.
    pub fn to_rgba8_placed(&self, layout: &[(LaneKey, f32)], scale: f32) -> (Vec<u8>, u32) {
        let s = scale.max(0.01);
        if self.lanes.is_empty() || self.width == 0 {
            return (Vec::new(), 1);
        }
        let ys: BTreeMap<LaneKey, f32> = layout.iter().copied().collect();
        let placed = self.placed_ys(layout, s);
        let (origin, height) = Self::extent_of(&placed, s);
        let mut out = vec![0u8; self.width.saturating_mul(height as usize).saturating_mul(4)];
        for (row, key) in self.lanes.iter().enumerate() {
            let Some(&y) = ys
                .get(key)
                .or_else(|| placed.iter().find(|(k, _)| *k == *key).map(|(_, y)| y))
            else {
                continue;
            };
            let h = (lod::lane_height(*key) * s).round().max(1.0) as u32;
            let y0 = ((y - origin).round()).max(0.0) as u32;
            // The rows this lane covers, cut at the bottom of the image.
            let rows = h.min(height.saturating_sub(y0)) as usize;
            if rows == 0 {
                continue;
            }
            let stride = self.width * 4;
            let first = y0 as usize * stride;
            // A lane is one row of colours drawn `rows` pixels tall: convert
            // it once and copy the bytes down. Converting every pixel again
            // for each row of its height was a tenth of a zoomed-out frame.
            // The track colour is written too, as transparent: where two
            // lanes' rows meet, the later lane's gap is what shows.
            for (px, p) in out[first..first + stride].chunks_exact_mut(4).zip(self.row(row)) {
                px.copy_from_slice(&if *p == chrome::TRACK { [0; 4] } else { argb_to_rgba8(*p) });
            }
            for dy in 1..rows {
                out.copy_within(first..first + stride, first + dy * stride);
            }
        }
        (out, height)
    }
}

pub fn kind_label(kind_id: u8) -> &'static str {
    match kind_id {
        kind::API_SCOPE => "api",
        kind::FUNCTION_CALL => "fn",
        kind::SCHEDULING_SLICE => "sched",
        kind::THREAD_STATE => "state",
        kind::API_TRACK => "track",
        kind::VALUE => "value",
        _ => "other",
    }
}

/// Synthetic non-overlapping scopes for benches / the demo producer.
pub fn generate_nested_scopes(
    count: usize,
    threads: u32,
    max_depth: u8,
    t0: u64,
    span_ns: u64,
) -> Vec<LiveEvent> {
    let mut out = Vec::with_capacity(count);
    if count == 0 || threads == 0 {
        return out;
    }
    let per_thread = (count / threads as usize).max(1);
    for t in 0..threads {
        let mut i = 0usize;
        while i < per_thread && out.len() < count {
            let depth = (i as u8) % max_depth.max(1);
            let start = t0 + (i as u64) * (span_ns / per_thread.max(1) as u64).max(1);
            let duration = (span_ns / per_thread.max(1) as u64 / 2).max(1);
            out.push(LiveEvent {
                start_ns: start,
                duration_ns: duration,
                tid: 100 + t,
                pid: 1,
                kind: kind::API_SCOPE,
                depth,
                extra: 0,
                _pad: 0,
                name_id: (t as u32) * 1000 + i as u32,
            });
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{FLAG_INACTIVE, FLAG_SAME_PID, ThreadFocus};
    use super::*;
    use perfr_live_event::{named_scope_color, thread_scope_color};
    #[cfg(not(debug_assertions))]
    use std::time::Instant;

    fn scope(start: u64, dur: u64, depth: u8, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: dur,
            tid: 1,
            pid: 1,
            kind: kind::API_SCOPE,
            depth,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    #[test]
    fn binary_search_finds_overlapping_interval() {
        let mut lane = Lane::default();
        lane.extend([scope(0, 10, 0, 1), scope(10, 10, 0, 2), scope(20, 10, 0, 3)]);
        assert_eq!(lane.overlapping(0, 5).unwrap().name_id, 1);
        assert_eq!(lane.overlapping(10, 15).unwrap().name_id, 2);
        assert_eq!(lane.overlapping(25, 30).unwrap().name_id, 3);
        assert!(lane.overlapping(30, 40).is_none());
        assert_eq!(lane.first_ending_after(10), 1);
        assert_eq!(lane.last_overlapping(0, 12).unwrap().name_id, 2);
        assert!(
            lane.ends_are_sorted(),
            "non-overlapping start-sorted lane must have sorted ends"
        );
    }

    #[test]
    fn overlapping_ends_break_the_end_index_invariant() {
        let mut lane = Lane::default();
        lane.insert(scope(0, 100, 0, 1));
        lane.insert(scope(10, 5, 0, 2));
        assert!(
            !lane.ends_are_sorted(),
            "overlapping insert (end 100 then 15) must fail this test — \
             first_ending_after then walks linearly so the long parent \
             is not skipped"
        );
    }

    #[test]
    fn first_ending_after_includes_long_scope_when_instant_breaks_end_order() {
        let mut lane = Lane::default();
        lane.insert(scope(0, 1000, 0, 1));
        lane.insert(scope(100, 1, 0, 2));
        assert!(!lane.ends_are_sorted());
        assert_eq!(lane.first_ending_after(200), 0);
        assert_eq!(lane.last_overlapping(200, 201).unwrap().name_id, 1);
        assert_eq!(
            lane.last_overlapping(100, 101).unwrap().name_id,
            1,
            "1 ns instant must not own a pixel that a longer scope occupies"
        );
    }

    #[test]
    fn last_overlapping_keeps_isolated_instant_in_a_gap() {
        let mut lane = Lane::default();
        lane.insert(scope(0, 50, 0, 1));
        lane.insert(scope(60, 1, 0, 2));
        lane.insert(scope(100, 50, 0, 3));
        assert_eq!(lane.last_overlapping(60, 61).unwrap().name_id, 2);
        assert_eq!(lane.last_overlapping(0, 10).unwrap().name_id, 1);
        assert_eq!(lane.last_overlapping(120, 130).unwrap().name_id, 3);
    }

    #[test]
    fn first_ending_after_is_binary_search_on_sorted_ends() {
        let mut lane = Lane::default();
        for i in 0..64u64 {
            lane.insert(scope(i * 100, 80, 0, i as u32));
        }
        assert!(lane.ends_are_sorted());
        assert_eq!(lane.first_ending_after(0), 0);
        assert_eq!(lane.first_ending_after(80), 1);
        assert_eq!(lane.first_ending_after(79), 0);
        assert_eq!(lane.first_ending_after(6400), 64);
    }

    #[test]
    fn pixel_and_naive_agree_on_non_overlapping_lane() {
        let mut lane = Lane::default();
        for i in 0..64u64 {
            lane.insert(scope(i * 100, 80, 0, i as u32));
        }
        let width = 128usize;
        let mut pixel = vec![0u32; width];
        let mut naive = vec![0u32; width];
        lane.rasterize_pixel_columns(0, 6400, width, &mut pixel, None);
        lane.rasterize_naive(0, 6400, width, &mut naive, None);
        assert_eq!(pixel, naive);
        assert!(pixel.iter().any(|&p| p != chrome::TRACK));
    }

    #[test]
    fn pixel_and_naive_agree_when_instant_overlaps_longer_scope() {
        let mut lane = Lane::default();
        lane.insert(scope(0, 1000, 0, 1));
        lane.insert(scope(100, 1, 0, 2));
        let width = 64usize;
        let mut pixel = vec![0u32; width];
        let mut naive = vec![0u32; width];
        lane.rasterize_pixel_columns(0, 1000, width, &mut pixel, None);
        lane.rasterize_naive(0, 1000, width, &mut naive, None);
        assert_eq!(pixel, naive);
        let long = scope(0, 1000, 0, 1).color_rgba();
        assert!(
            pixel.iter().all(|&p| p == long),
            "1 ns instant must not overwrite the longer scope's pixels"
        );
    }

    #[test]
    fn out_of_order_insert_keeps_sort() {
        let mut lane = Lane::default();
        lane.insert(scope(100, 10, 0, 2));
        lane.insert(scope(0, 10, 0, 1));
        lane.insert(scope(50, 10, 0, 3));
        let starts: Vec<u64> = lane.events().iter().map(|e| e.start_ns).collect();
        assert_eq!(starts, vec![0, 50, 100]);
    }

    #[test]
    fn time_bounds_ignore_zero_width_origin_when_cluster_exists() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 1, 0, 1)); // instant / missing-ts leftover
        idx.insert(scope(122_403_254_982_000, 1_000_000, 0, 2));
        idx.insert(scope(122_411_498_000_000, 936_000, 0, 3));
        let (a, b) = idx.time_bounds().expect("bounds");
        assert_eq!(a, 122_403_254_982_000);
        assert_eq!(b, 122_411_498_936_000);
    }

    #[test]
    fn time_bounds_keep_real_work_at_t_zero() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10_000, 0, 1));
        idx.insert(scope(20_000, 5_000, 0, 2));
        let (a, b) = idx.time_bounds().expect("bounds");
        assert_eq!(a, 0);
        assert_eq!(b, 25_000);
    }

    #[test]
    fn retain_drops_events_and_the_lanes_they_emptied() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10, 0, 1));
        idx.insert(scope(20, 10, 0, 2));
        idx.insert(LiveEvent {
            pid: 2,
            ..scope(40, 10, 0, 3)
        });
        assert_eq!(idx.lane_count(), 2);
        idx.retain(|e| e.pid != 1);
        assert_eq!(idx.event_count(), 1);
        // An empty lane still builds a row in the track strip, so it must go
        // with its last event.
        assert_eq!(idx.lane_count(), 1);
        assert!(idx.lanes().all(|(k, _)| k.pid == 2));
    }

    #[test]
    fn retain_keeps_lane_lookups_answering() {
        let mut idx = TrackIndex::default();
        // A long scope with a 1 ns instant inside it: ends are out of order,
        // so the lane is on the duration-aware linear fallback. Dropping the
        // instant must leave the lane searchable, not stuck mid-state.
        idx.insert(scope(0, 1_000, 1, 1));
        let mut instant = scope(10, 1, 1, 2);
        instant.pid = 2;
        idx.insert(instant);
        let key = idx.lanes().next().expect("lane").0;
        idx.retain(|e| e.pid != 2);
        let lane = idx.lane(key).expect("lane");
        assert!(lane.ends_are_sorted());
        assert_eq!(lane.first_ending_after(10), 0);
        assert_eq!(lane.overlapping(10, 11).map(|e| e.start_ns), Some(0));
    }

    #[test]
    fn track_index_groups_by_lane() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10, 0, 1));
        idx.insert(LiveEvent {
            tid: 2,
            ..scope(0, 10, 0, 2)
        });
        idx.insert(scope(20, 10, 1, 3));
        assert_eq!(idx.lane_count(), 3);
        assert_eq!(idx.event_count(), 3);
    }

    #[test]
    fn scheduling_slices_share_a_core_lane() {
        let mut idx = TrackIndex::default();
        idx.insert(LiveEvent {
            start_ns: 0,
            duration_ns: 10,
            tid: 10,
            pid: 1,
            kind: kind::SCHEDULING_SLICE,
            depth: 0,
            extra: 2,
            _pad: 0,
            name_id: 10,
        });
        idx.insert(LiveEvent {
            start_ns: 10,
            duration_ns: 10,
            tid: 20,
            pid: 4,
            kind: kind::SCHEDULING_SLICE,
            depth: 0,
            extra: 2,
            _pad: 0,
            name_id: 20,
        });
        assert_eq!(idx.lane_count(), 1);
        let lane = idx.lane(LaneKey::scheduler(2)).unwrap();
        assert_eq!(lane.len(), 2);
        assert!(lane.ends_are_sorted());
        assert_eq!(lane.events()[0].tid, 10);
        assert_eq!(lane.events()[1].tid, 20);
        assert_eq!(lane.events()[0].color_rgba(), thread_scope_color(10, 1));
        assert_eq!(lane.events()[1].color_rgba(), thread_scope_color(20, 1));
    }

    #[test]
    fn track_index_scopes_lanes_by_pid() {
        let mut idx = TrackIndex::default();
        idx.insert(LiveEvent {
            pid: 1,
            tid: 7,
            ..scope(0, 10, 0, 1)
        });
        idx.insert(LiveEvent {
            pid: 2,
            tid: 7,
            ..scope(0, 10, 0, 2)
        });
        assert_eq!(idx.lane_count(), 2);
    }

    /// The pixel-column path must not be linear in the number of scopes.
    /// 20× more events should not be ~20× slower (that would be the naive path).
    ///
    /// Release-only: debug codegen makes the same walk look linear on CI
    /// (`time_ratio ≈ n_ratio`) and can make pixel slower than naive.
    #[cfg(not(debug_assertions))]
    #[test]
    fn pixel_prepare_is_not_linear_in_scopes() {
        let width = 1024usize;
        let t0 = 0u64;
        let t1 = 1_000_000u64;

        let small_n = 20_000usize;
        let large_n = 400_000usize;
        let small = generate_nested_scopes(small_n, 4, 4, t0, t1);
        let large = generate_nested_scopes(large_n, 4, 4, t0, t1);

        let mut small_idx = TrackIndex::default();
        small_idx.extend(small);
        let mut large_idx = TrackIndex::default();
        large_idx.extend(large);

        // Warm up.
        let _ = small_idx.rasterize_pixel(t0, t1, width, None);
        let _ = large_idx.rasterize_pixel(t0, t1, width, None);
        let _ = large_idx.rasterize_naive(t0, t1, width, None);

        let start = Instant::now();
        for _ in 0..8 {
            let _ = small_idx.rasterize_pixel(t0, t1, width, None);
        }
        let small_ns = start.elapsed().as_nanos().max(1);

        let start = Instant::now();
        for _ in 0..8 {
            let _ = large_idx.rasterize_pixel(t0, t1, width, None);
        }
        let large_pixel_ns = start.elapsed().as_nanos().max(1);

        let start = Instant::now();
        for _ in 0..8 {
            let _ = large_idx.rasterize_naive(t0, t1, width, None);
        }
        let large_naive_ns = start.elapsed().as_nanos().max(1);

        let n_ratio = large_n as f64 / small_n as f64;
        let time_ratio = large_pixel_ns as f64 / small_ns as f64;
        // log2(400k)/log2(20k) ≈ 1.3. A linear path would be ~20×.
        assert!(
            time_ratio < n_ratio / 3.0,
            "pixel rasterizer looks O(scopes): time_ratio={time_ratio:.2} n_ratio={n_ratio:.2} \
             small_ns={small_ns} large_pixel_ns={large_pixel_ns}. \
             The hot path must be O(width log n), not O(n)."
        );
        assert!(
            large_pixel_ns < large_naive_ns,
            "pixel path ({large_pixel_ns} ns) should be faster than naive ({large_naive_ns} ns) \
             at {large_n} scopes / {width} px. If this fails, the chosen renderer is O(scopes)."
        );
    }

    #[test]
    fn instanced_lod_when_scopes_are_wider_than_four_px() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 100_000, 0, 1));
        assert_eq!(
            choose_lod(&idx, 0, 100_000, 100, INSTANCE_MIN_PX),
            TimelineLod::Instanced
        );
        let frame = collect_instances(&idx, 0, 100_000, 100.0, 0.0, None);
        assert_eq!(frame.instances.len(), 1);
        assert!((frame.instances[0].w - 100.0).abs() < 0.6);
        assert_eq!(
            frame.instances[0].color,
            named_scope_color(&1u32.to_le_bytes(), 0)
        );
    }

    #[test]
    fn pixel_column_lod_when_scopes_are_subpixel() {
        let mut idx = TrackIndex::default();
        for i in 0..64u64 {
            idx.insert(scope(i * 20, 8, 0, i as u32));
        }
        assert_eq!(
            choose_lod(&idx, 0, 1_000_000, 200, INSTANCE_MIN_PX),
            TimelineLod::PixelColumns
        );
    }

    #[test]
    fn collect_instances_walks_only_visible() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10, 0, 1));
        idx.insert(scope(1000, 10, 0, 2));
        idx.insert(scope(2000, 10, 0, 3));
        let frame = collect_instances(&idx, 990, 1020, 100.0, 0.0, None);
        assert_eq!(frame.instances.len(), 1);
    }

    #[test]
    fn shaders_are_present_for_both_lods() {
        assert!(BLIT_RECT_WGSL.contains("uni.dest"));
        assert!(INSTANCE_WGSL.contains("sd_rounded_box"));
        // The drop shadow was removed (87680c10b): the 6px quad expansion
        // it needed was pure overdraw on a 72-core scheduler track.
        assert!(!INSTANCE_WGSL.contains("rounded_box_shadow"));
        assert!(INSTANCE_WGSL.contains("SIBLING_RGB"));
        assert!(INSTANCE_WGSL.contains("SELECTED_RGB"));
        assert!(INSTANCE_WGSL.contains("if sibling"));
        assert!(INSTANCE_WGSL.contains("selected"));
        assert!(INSTANCE_WGSL.contains("dimmed"));
        assert!(INSTANCE_WGSL.contains("uni.time"));
        assert!(INSTANCE_WGSL.contains("PULSE_PERIOD"));
        assert!(INSTANCE_WGSL.contains("FLAG") || INSTANCE_WGSL.contains("selected"));
    }

    #[test]
    fn y_cull_raster_drops_offscreen_rows() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 50, 0, 1));
        idx.insert(LiveEvent {
            tid: 2,
            ..scope(0, 50, 0, 2)
        });
        let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        let layout = stacked_layout(&keys, 0.0);
        let full = idx.rasterize_pixel(0, 50, 8, None);
        assert_eq!(full.lanes.len(), 2);
        let y0 = layout[0].1;
        let h0 = lane_height(layout[0].0);
        let culled = idx.rasterize_pixel_layout(
            0,
            50,
            8,
            &keys,
            Some(&layout),
            Some(YCull::new(y0, y0 + h0 * 0.5)),
            None,
            false,
        );
        assert_eq!(culled.lanes.len(), 1);
        assert_eq!(culled.lanes[0], layout[0].0);
        let none = idx.rasterize_pixel_layout(0, 50, 8, &[], Some(&[]), None, None, false);
        assert!(
            none.lanes.is_empty(),
            "explicit empty layout must not fall back to every index lane"
        );
    }

    #[test]
    fn collect_instances_layout_remaps_lane_y() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 50, 0, 1));
        idx.insert(LiveEvent {
            tid: 2,
            ..scope(0, 50, 0, 2)
        });
        let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        assert_eq!(keys.len(), 2);
        let flipped = vec![(keys[1], 40.0), (keys[0], 0.0)];
        let frame = collect_instances_layout(&idx, 0, 50, 100.0, &flipped, None);
        assert_eq!(frame.instances.len(), 2);
        let a = frame.instances.iter().find(|i| i.name_id == 2).unwrap();
        let b = frame.instances.iter().find(|i| i.name_id == 1).unwrap();
        assert!((a.y - 40.0).abs() < 0.01);
        assert!(b.y.abs() < 0.01);
    }

    #[test]
    fn placed_raster_height_spans_the_layout_gaps() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 50, 0, 1));
        idx.insert(LiveEvent {
            depth: 1,
            name_id: 2,
            ..scope(0, 50, 1, 2)
        });
        let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        let raster = idx.rasterize_pixel(0, 50, 8, None);
        let scale = 0.72_f32;
        // The two lanes stacked with nothing between them.
        let h: f32 = keys
            .iter()
            .map(|k| (lane_height(*k) + lane_gap(*k)) * scale)
            .sum();
        let gapped = vec![(keys[0], 36.0), (keys[1], 80.0)];
        let (_, placed_h) = raster.to_rgba8_placed(&gapped, scale);
        let placed_expect = (80.0 + (lane_height(keys[1]) + lane_gap(keys[1])) * scale) - 36.0;
        assert!(
            (placed_h as f32 - placed_expect).abs() < 1.0,
            "placed h={placed_h} expect={placed_expect}"
        );
        assert!(
            placed_h as f32 > h + 1.0,
            "header gaps must occupy rows, not stretch compact leaves"
        );
    }

    #[test]
    fn self_and_demo_scope_instances_share_height_and_radius() {
        fn ev(pid: u32, depth: u8) -> LiveEvent {
            LiveEvent {
                start_ns: 0,
                duration_ns: 1_000,
                tid: 1,
                pid,
                kind: kind::API_SCOPE,
                depth,
                extra: 0,
                _pad: 0,
                name_id: 1,
            }
        }
        let mut idx = TrackIndex::default();
        idx.insert(ev(1, 0));
        idx.insert(ev(perfr_live_event::dev::VIEWER_PID, 0));
        idx.insert(ev(1, 1));
        idx.insert(ev(perfr_live_event::dev::VIEWER_PID, 1));
        let frame = collect_instances(&idx, 0, 2_000, 100.0, 0.0, None);
        for depth in [0u8, 1] {
            let demo = frame
                .instances
                .iter()
                .find(|i| i.pid == 1 && i.depth == depth)
                .unwrap();
            let slf = frame
                .instances
                .iter()
                .find(|i| i.pid == perfr_live_event::dev::VIEWER_PID && i.depth == depth)
                .unwrap();
            let key = ev(1, depth).lane_key();
            let expect_h = lane_height(key);
            assert_eq!(demo.h, expect_h);
            assert_eq!(slf.h, expect_h);
            assert_eq!(demo.radius, slf.radius);
            assert_eq!(demo.radius, (expect_h * 0.14).clamp(2.0, 3.0));
        }
    }

    #[test]
    fn pick_instance_hits_topmost_and_flags_siblings() {
        let a = ScopeInstance {
            x: 0.0,
            y: 0.0,
            w: 20.0,
            h: 10.0,
            color: 1,
            radius: 2.0,
            name_id: 7,
            start_ns: 10,
            duration_ns: 20,
            pid: 1,
            tid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            event_flags: 0,
            flags: FLAG_NONE,
        };
        let b = ScopeInstance {
            x: 5.0,
            y: 0.0,
            w: 20.0,
            h: 10.0,
            color: 1,
            radius: 2.0,
            name_id: 7,
            start_ns: 40,
            duration_ns: 20,
            pid: 1,
            tid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            event_flags: 0,
            flags: FLAG_NONE,
        };
        assert_eq!(pick_instance_at(&[a, b], 8.0, 4.0), Some(1));
        let mut insts = vec![a, b];
        apply_highlight_flags(
            &mut insts,
            Some(ScopePick {
                name_id: 7,
                start_ns: 40,
                duration_ns: 20,
                pid: 1,
                tid: 1,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                event_flags: 0,
            }),
            None,
            None, ThreadFocus::default());
        assert_eq!(insts[1].flags, FLAG_SELECTED);
        assert_eq!(insts[0].flags, FLAG_SIBLING);
    }

    #[test]
    fn search_dims_non_matching_name_ids() {
        let a = ScopeInstance {
            x: 0.0,
            y: 0.0,
            w: 20.0,
            h: 10.0,
            color: 1,
            radius: 2.0,
            name_id: 7,
            start_ns: 10,
            duration_ns: 20,
            pid: 1,
            tid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            event_flags: 0,
            flags: FLAG_NONE,
        };
        let mut b = a;
        b.name_id = 9;
        b.start_ns = 40;
        let mut insts = vec![a, b];
        let ids = std::collections::HashSet::from([7u32]);
        apply_highlight_flags(&mut insts, None, None, Some(&ids), ThreadFocus::default());
        assert_eq!(insts[0].flags, FLAG_NONE);
        assert_eq!(insts[1].flags, FLAG_DIMMED);
        apply_highlight_flags(
            &mut insts,
            Some(ScopePick {
                name_id: 7,
                start_ns: 10,
                duration_ns: 20,
                pid: 1,
                tid: 1,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                event_flags: 0,
            }),
            None,
            Some(&ids), ThreadFocus::default());
        assert_eq!(insts[0].flags, FLAG_SELECTED);
        assert_eq!(insts[1].flags, FLAG_DIMMED, "other names stay dimmed");
        insts[1].name_id = 7;
        insts[1].start_ns = 40;
        apply_highlight_flags(
            &mut insts,
            Some(ScopePick {
                name_id: 7,
                start_ns: 10,
                duration_ns: 20,
                pid: 1,
                tid: 1,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                event_flags: 0,
            }),
            None,
            Some(&ids), ThreadFocus::default());
        assert_eq!(insts[0].flags, FLAG_SELECTED);
        assert_eq!(insts[1].flags, FLAG_SIBLING);
    }

    #[test]
    fn placed_rgba_uses_event_color_and_leaves_empty_columns_clear() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 50, 1, 1));
        let frame = idx.rasterize_pixel(0, 100, 8, None);
        let expect = named_scope_color(&1u32.to_le_bytes(), 1);
        assert_eq!(frame.pixels[0], expect);
        assert_eq!(frame.pixels[7], chrome::TRACK);
        assert_ne!(expect, chrome::TRACK);
        let key = frame.lanes[0];
        let (bytes, h) = frame.to_rgba8_placed(&[(key, 0.0)], 1.0);
        assert!(h >= 16);
        assert_eq!(bytes[0], ((expect >> 16) & 0xFF) as u8);
        assert_eq!(bytes[1], ((expect >> 8) & 0xFF) as u8);
        assert_eq!(bytes[2], (expect & 0xFF) as u8);
        assert_eq!(bytes[3], 0xFF);
        // A column with no event is transparent, so the row behind shows.
        assert_eq!(bytes[7 * 4..8 * 4], [0, 0, 0, 0]);
    }

    #[test]
    fn rgba8_packing_matches_argb() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10, 0, 1));
        let frame = idx.rasterize_pixel(0, 10, 1, None);
        let bytes = frame.to_rgba8();
        assert_eq!(bytes.len(), 4);
        let p = frame.pixels[0];
        assert_eq!(bytes[0], ((p >> 16) & 0xFF) as u8);
        assert_eq!(bytes[3], ((p >> 24) & 0xFF) as u8);
    }
    #[test]
    fn a_selected_thread_greys_the_rest_and_lightens_its_process_on_the_scheduler() {
        let mk = |pid: u32, tid: u32, kind: u8| ScopeInstance {
            x: 0.0, y: 0.0, w: 10.0, h: 4.0, color: 0xFF80_8080, radius: 1.0,
            name_id: 1, start_ns: 0, duration_ns: 10, pid, tid, kind, depth: 0, extra: 0,
            event_flags: 0,
            flags: FLAG_NONE,
        };
        let mut insts = vec![
            mk(1, 10, kind::SCHEDULING_SLICE), // selected thread, on a core
            mk(1, 11, kind::SCHEDULING_SLICE), // same process, other thread
            mk(5, 50, kind::SCHEDULING_SLICE), // another process (2 and 3 are the viewer's own)
            mk(1, 11, kind::API_SCOPE),        // another thread's scope
            mk(1, 10, kind::API_SCOPE),        // the selected thread's scope
        ];
        let focus = ThreadFocus { selected: Some((1, 10)), target_pid: Some(1) };
        apply_highlight_flags(&mut insts, None, None, None, focus);
        assert_eq!(insts[0].flags, FLAG_NONE, "selected thread keeps its colour");
        assert_eq!(insts[1].flags, FLAG_SAME_PID);
        assert_eq!(insts[2].flags, FLAG_INACTIVE);
        assert_eq!(insts[3].flags, FLAG_NONE, "other threads' scopes keep their colour: only the scheduler greys");
        assert_eq!(insts[4].flags, FLAG_NONE);
        // No selection: the target process is active, others grey.
        let focus = ThreadFocus { selected: None, target_pid: Some(1) };
        apply_highlight_flags(&mut insts, None, None, None, focus);
        assert_eq!(insts[1].flags, FLAG_NONE);
        assert_eq!(insts[2].flags, FLAG_INACTIVE);
        // No target either: everything active.
        apply_highlight_flags(&mut insts, None, None, None, ThreadFocus::default());
        assert!(insts.iter().all(|i| i.flags == FLAG_NONE));
    }

}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    /// A JSON `"ts":1e30` saturates to the last nanosecond there is. One
    /// such event must not take the index down with it.
    #[test]
    fn an_event_at_the_end_of_time_does_not_overflow_the_bounds_or_the_columns() {
        let last = LiveEvent { start_ns: u64::MAX, duration_ns: 5, tid: 7, pid: 7, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 1 };
        let mut index = TrackIndex::default();
        index.insert(last);
        assert_eq!(index.time_bounds(), Some((u64::MAX, u64::MAX)));
        let frame = index.rasterize_pixel(u64::MAX - 10, u64::MAX, 2_000, None);
        assert_eq!(frame.pixels.len(), 2_000);
        let mut lane = Lane::default();
        for k in 0..3_000u64 {
            lane.insert(LiveEvent { start_ns: u64::MAX - 3_000 + k, duration_ns: 1, ..last });
        }
        let mut out = vec![0u32; 64];
        lane.rasterize_pixel_columns(u64::MAX - 1, u64::MAX, 64, &mut out, None);
        let key = last.lane_key();
        let layout = [(key, 0.0)];
        let _ = pick_column_event(&index, &layout, u64::MAX - 1, u64::MAX, 100.0, 99.9, 1.0, 1.0);
        // The viewer's own rows only: the fallback bounds take the same path.
        let mut own = TrackIndex::default();
        own.insert(LiveEvent { pid: perfr_live_event::dev::VIEWER_PID, ..last });
        assert_eq!(own.time_bounds(), Some((u64::MAX, u64::MAX)));
    }

    #[test]
    fn the_viewers_own_rows_do_not_define_the_captures_bounds() {
        let mut index = TrackIndex::default();
        let mut own = LiveEvent { start_ns: 1_000, duration_ns: 500, tid: 1, pid: perfr_live_event::dev::VIEWER_PID, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 1 };
        index.insert(own);
        // Only the viewer's rows: they are the fallback bounds.
        assert_eq!(index.time_bounds(), Some((1_000, 1_500)));
        let real = LiveEvent { start_ns: 5_000_000, duration_ns: 100, tid: 7, pid: 7, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 2 };
        index.insert(real);
        // A real event: the bounds are the capture's, whatever the viewer's
        // rows say, before or after.
        assert_eq!(index.time_bounds(), Some((5_000_000, 5_000_100)));
        own.start_ns = 9_000_000;
        index.insert(own);
        assert_eq!(index.time_bounds(), Some((5_000_000, 5_000_100)));
    }

    #[test]
    fn events_already_running_at_the_floor_are_held_but_do_not_widen_the_bounds() {
        let span = |start_ns: u64, end_ns: u64, tid: u32| LiveEvent { start_ns, duration_ns: end_ns - start_ns, tid, pid: 7, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 1 };
        // A capture cut to [100, 200]: one scope spans the window, one runs
        // into it, one sits inside and one runs out of it.
        let stored = [span(10, 900, 1), span(50, 150, 2), span(120, 130, 2), span(190, 260, 2)];
        let mut index = TrackIndex::default();
        index.set_extent_floor(100);
        index.extend(stored);
        assert_eq!(index.event_count(), 4, "all four are held");
        assert_eq!(index.time_bounds(), Some((120, 260)), "the bounds are those of what begins in the window");
        // The same whichever comes first, the floor or the events.
        let mut late = TrackIndex::default();
        late.extend(stored);
        assert_eq!(late.time_bounds(), Some((10, 900)));
        late.set_extent_floor(100);
        assert_eq!(late.time_bounds(), Some((120, 260)));
        // Dropping events keeps the floor.
        late.retain(|e| e.start_ns != 190);
        assert_eq!(late.time_bounds(), Some((120, 130)));
        // With nothing but events that were already running, they are what there is.
        let mut only = TrackIndex::default();
        only.set_extent_floor(100);
        only.insert(span(10, 900, 1));
        assert_eq!(only.time_bounds(), Some((10, 900)));
        // Another capture starts without the floor of the last.
        late.clear();
        late.insert(span(10, 20, 1));
        assert_eq!(late.time_bounds(), Some((10, 20)));
    }
}

#[cfg(test)]
pub(crate) mod test_rng {
    /// xorshift64*: enough to drive randomized comparisons against a plain
    /// reference, the same on every run.
    pub struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }
}

#[cfg(test)]
mod block_span_tests {
    use super::test_rng::Rng;
    use super::*;

    fn ev(start: u64, dur: u64, name: u32) -> LiveEvent {
        LiveEvent { start_ns: start, duration_ns: dur, tid: 1, pid: 1, kind: kind::API_SCOPE, depth: 0, extra: 0, _pad: 0, name_id: name }
    }

    /// What `last_overlapping` did before the blocks: every event in range.
    fn reference_owner(lane: &Lane, col0: u64, col1: u64) -> Option<LiveEvent> {
        let mut best: Option<LiveEvent> = None;
        for e in lane.events() {
            if e.end_ns() <= col0 || e.start_ns >= col1 {
                continue;
            }
            if best.is_none_or(|b| event_span_ns(e) >= event_span_ns(&b)) {
                best = Some(*e);
            }
        }
        best
    }

    fn check_blocks(lane: &Lane) {
        assert_eq!(lane.longest_span, lane.events().iter().map(event_span_ns).max().unwrap_or(0), "the lane's longest span");
        let expect: Vec<u64> = lane
            .events()
            .chunks(SPAN_BLOCK)
            .map(|b| b.iter().map(event_span_ns).max().unwrap())
            .collect();
        assert_eq!(lane.block_span, expect);
        assert_eq!(lane.ends_sorted, lane.ends_are_sorted());
        for t in lane.events().iter().flat_map(|e| [e.start_ns, e.end_ns(), e.end_ns().saturating_sub(1)]) {
            let walked = lane.events().iter().position(|e| e.end_ns() > t).unwrap_or(lane.len());
            assert_eq!(lane.first_ending_after(t), walked, "first event ending after {t}");
        }
        assert!(lane.events().windows(2).all(|w| w[0].start_ns <= w[1].start_ns));
    }

    /// Lanes built in order, out of order, with ties, with overlap, and
    /// thinned by `retain`, asked for random windows: the answer through
    /// the blocks is the answer of the walk they replace.
    #[test]
    fn the_longest_event_through_the_blocks_is_the_one_a_walk_finds() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..60 {
            let n = 1 + rng.below(700) as usize;
            let overlap = round % 3 == 0;
            let mut lane = Lane::default();
            let mut t = 0u64;
            let mut pending = Vec::new();
            for k in 0..n {
                let gap = 1 + rng.below(40);
                // Few distinct durations, so ties are common.
                let dur = [0, 1, 1, 5, 5, 30, 400][rng.below(7) as usize];
                let dur = if overlap && rng.below(10) == 0 { dur + 5_000 } else { dur.min(gap) };
                pending.push(ev(t, dur, k as u32));
                t += gap;
            }
            // Mostly in order, with some late arrivals.
            let mut late = Vec::new();
            for e in pending {
                if rng.below(8) == 0 {
                    late.push(e);
                } else {
                    lane.insert(e);
                }
            }
            for e in late {
                lane.insert(e);
            }
            check_blocks(&lane);
            for pass in 0..2 {
                for _ in 0..200 {
                    let a = rng.below(t + 50);
                    let reach = if rng.below(2) == 0 { 60 } else { t + 50 };
                    let b = a + 1 + rng.below(reach);
                    assert_eq!(
                        lane.last_overlapping(a, b).copied(),
                        reference_owner(&lane, a, b),
                        "round {round} pass {pass}: [{a}, {b}) over {} events, overlap {overlap}",
                        lane.len()
                    );
                    let min = [1, 2, 6, 31, 401, 6_000][rng.below(6) as usize];
                    let expect = lane
                        .events()
                        .iter()
                        .any(|e| e.end_ns() > a && e.start_ns < b && e.duration_ns >= min);
                    assert_eq!(lane.any_duration(a, b, |d| d >= min), expect, "round {round}: [{a}, {b}) min {min}");
                }
                lane.retain(|e| e.name_id % 5 != 0);
                check_blocks(&lane);
            }
        }
    }

    /// The walk a lane with overlap is rastered with, over every event of
    /// the lane, as it was before it was bounded to the window.
    fn reference_naive(lane: &Lane, t0: u64, t1: u64, width: usize) -> Vec<u32> {
        let mut out = vec![chrome::TRACK; width];
        let mut owner: Vec<Option<usize>> = vec![None; width];
        let span = (t1 - t0) as f64;
        for (ei, e) in lane.events().iter().enumerate() {
            if e.end_ns() <= t0 || e.start_ns >= t1 {
                continue;
            }
            let x0 = (((e.start_ns.max(t0) - t0) as f64 / span) * width as f64).floor() as usize;
            let x1 = (((e.end_ns().min(t1) - t0) as f64 / span) * width as f64).ceil() as usize;
            for x in x0.min(width)..x1.max(x0 + 1).min(width) {
                if owner[x].is_none_or(|p| event_span_ns(e) >= event_span_ns(&lane.events()[p])) {
                    owner[x] = Some(ei);
                    out[x] = e.color_for(None);
                }
            }
        }
        out
    }

    #[test]
    fn a_lane_with_overlap_is_searched_not_scanned_and_draws_the_same() {
        let mut rng = Rng(7);
        for _ in 0..30 {
            let n = 50 + rng.below(1_500) as usize;
            let mut lane = Lane::default();
            let mut t = 0u64;
            let mut late = Vec::new();
            for k in 0..n {
                let gap = 1 + rng.below(30);
                // Slices, the odd long parent, and instants inside them.
                let dur = match rng.below(12) {
                    0 => 2_000 + rng.below(20_000),
                    1 | 2 => 1,
                    _ => 1 + rng.below(gap),
                };
                let e = ev(t, dur, (k % 251) as u32);
                if rng.below(6) == 0 {
                    late.push(e);
                } else {
                    lane.insert(e);
                }
                t += gap;
            }
            for e in late {
                lane.insert(e);
            }
            assert!(!lane.ends_are_sorted(), "fixture is only meaningful with overlap");
            check_blocks(&lane);
            for width in [1usize, 9, 200, 2_000] {
                let t0 = rng.below(t);
                let t1 = t0 + 1 + rng.below(t);
                let mut got = vec![0u32; width];
                lane.rasterize(t0, t1, width, &mut got, None);
                assert_eq!(got, reference_naive(&lane, t0, t1, width), "{n} events, [{t0}, {t1}) at {width} px");
            }
            // Thinned until the overlap is gone, the lane is a plain one again.
            lane.retain(|e| e.duration_ns < 2_000 && e.duration_ns > 1 && e.name_id % 2 == 0);
            check_blocks(&lane);
        }
    }

    /// The search must not depend on how many events lie before the window.
    #[cfg(not(debug_assertions))]
    #[test]
    fn a_window_at_the_end_of_a_lane_with_overlap_does_not_pay_for_the_lane() {
        let build = |n: u64| {
            let mut lane = Lane::default();
            lane.insert(ev(0, 30, 1));
            lane.insert(ev(5, 1, 2));
            for k in 0..n {
                lane.insert(ev(100 + k * 10, 8, 3));
            }
            lane
        };
        let time = |lane: &Lane, n: u64| {
            let (t0, t1) = (100 + (n - 200) * 10, 100 + n * 10);
            let mut out = vec![0u32; 256];
            let start = std::time::Instant::now();
            for _ in 0..200 {
                lane.rasterize(t0, t1, 256, &mut out, None);
                std::hint::black_box(lane.first_ending_after(t0));
            }
            start.elapsed().as_nanos().max(1)
        };
        let (small, large) = (build(20_000), build(2_000_000));
        let _ = (time(&small, 20_000), time(&large, 2_000_000));
        let ratio = time(&large, 2_000_000) as f64 / time(&small, 20_000) as f64;
        assert!(ratio < 10.0, "100x the events before the window cost {ratio:.1}x");
    }

    #[test]
    fn a_gallop_from_anywhere_before_the_answer_is_the_partition_point() {
        let mut rng = Rng(7);
        for _ in 0..200 {
            let n = rng.below(400) as usize;
            let mut v: Vec<u64> = (0..n).map(|_| rng.below(1_000)).collect();
            v.sort();
            let limit = rng.below(1_100);
            let expect = v.partition_point(|x| *x < limit);
            for from in [0, expect / 2, expect.saturating_sub(1), expect] {
                assert_eq!(gallop(&v, from, |x| *x < limit), expect, "{n} values, limit {limit}, from {from}");
            }
        }
        assert_eq!(gallop::<u64>(&[], 0, |_| true), 0);
        assert_eq!(gallop(&[1u64, 2, 3], 3, |_| true), 3);
        assert_eq!(gallop(&[1u64, 2, 3], 9, |_| true), 3, "a start past the end is the end");
    }

    #[test]
    fn the_column_walk_owns_each_column_as_asking_afresh_would() {
        // What each column held before the walk kept cursors: the answer of
        // `last_overlapping`, asked per column from scratch.
        let afresh = |lane: &Lane, t0: u64, t1: u64, width: usize| -> Vec<u32> {
            let dt = (t1 - t0) as f64 / width as f64;
            (0..width)
                .map(|x| {
                    let col0 = t0.saturating_add((x as f64 * dt) as u64);
                    let col1 = t0.saturating_add(((x + 1) as f64 * dt) as u64).max(col0.saturating_add(1));
                    lane.last_overlapping(col0, col1).map(|e| e.color_for(None)).unwrap_or(chrome::TRACK)
                })
                .collect()
        };
        let mut rng = Rng(1234);
        for round in 0..60 {
            let n = 1 + rng.below(5_000) as usize;
            let mut lane = Lane::default();
            let mut t = rng.below(1_000);
            for k in 0..n {
                let gap = 1 + rng.below(40);
                // Scopes, zero-length marks, and now and then one that runs
                // into the next (which breaks the order of ends).
                let dur = match rng.below(10) {
                    0 => 0,
                    1 if round % 3 == 0 => gap + rng.below(60),
                    _ => 1 + rng.below(gap),
                };
                lane.insert(ev(t, dur, (k % 251) as u32));
                t += gap;
            }
            for width in [1usize, 2, 7, 64, 300, 1_700, 6_000] {
                // Windows inside the lane, hanging off either end, far past
                // it, and narrower than a pixel per nanosecond.
                let windows = [
                    (0, t + 100),
                    (rng.below(t), rng.below(t) + t / 2 + 1),
                    (t / 3, t / 3 + 1 + rng.below(50)),
                    (t.saturating_sub(5), t + 10_000),
                    (t + 50, t + 5_000),
                    (0, 1),
                ];
                for (t0, t1) in windows {
                    let t1 = t1.max(t0 + 1);
                    let mut cols = vec![0u32; width];
                    lane.rasterize_pixel_columns(t0, t1, width, &mut cols, None);
                    assert_eq!(cols, afresh(&lane, t0, t1, width), "round {round}: {n} events, [{t0}, {t1}) at {width} px");
                }
            }
        }
        // At the top of the clock the column bounds saturate and stop moving.
        let mut lane = Lane::default();
        for k in 0..200u64 {
            lane.insert(ev(u64::MAX - 4_000 + k * 20, 10, k as u32));
        }
        let mut cols = vec![0u32; 500];
        lane.rasterize_pixel_columns(u64::MAX - 5_000, u64::MAX, 500, &mut cols, None);
        assert_eq!(cols, afresh(&lane, u64::MAX - 5_000, u64::MAX, 500));
    }

    #[test]
    fn a_placed_frame_is_each_lanes_row_drawn_as_tall_as_the_lane() {
        // The image as it was built pixel by pixel, row by row.
        let reference = |frame: &RasterizedFrame, layout: &[(LaneKey, f32)], scale: f32| -> (Vec<u8>, u32) {
            let s = scale.max(0.01);
            let ys: BTreeMap<LaneKey, f32> = layout.iter().copied().collect();
            let placed = frame.placed_ys(layout, s);
            let (origin, height) = RasterizedFrame::extent_of(&placed, s);
            let mut out = vec![0u8; frame.width * height as usize * 4];
            for (row, key) in frame.lanes.iter().enumerate() {
                let Some(&y) = ys.get(key) else { continue };
                let h = (lod::lane_height(*key) * s).round().max(1.0) as u32;
                let y0 = ((y - origin).round()).max(0.0) as u32;
                for dy in 0..h {
                    let yy = y0 + dy;
                    if yy >= height {
                        break;
                    }
                    for (i, p) in frame.row(row).iter().enumerate() {
                        let o = (yy as usize * frame.width + i) * 4;
                        let rgba = if *p == chrome::TRACK { [0; 4] } else { argb_to_rgba8(*p) };
                        out[o..o + 4].copy_from_slice(&rgba);
                    }
                }
            }
            (out, height)
        };
        let mut rng = Rng(99);
        let mut index = TrackIndex::default();
        let kinds = [kind::API_SCOPE, kind::SCHEDULING_SLICE, kind::THREAD_STATE, kind::FUNCTION_CALL];
        for tid in 0..12u32 {
            let mut t = 0u64;
            for k in 0..(20 + rng.below(200)) {
                let gap = 5 + rng.below(50);
                index.insert(LiveEvent {
                    start_ns: t,
                    duration_ns: 1 + rng.below(gap),
                    tid,
                    pid: 7,
                    kind: kinds[tid as usize % kinds.len()],
                    depth: (tid % 3) as u8,
                    extra: 0,
                    _pad: 0,
                    name_id: k as u32,
                });
                t += gap;
            }
        }
        let keys: Vec<LaneKey> = index.lanes().map(|(k, _)| k).collect();
        for scale in [1.0f32, 0.72, 1.5, 2.0] {
            // Lanes stacked with gaps, and a second layout that leaves the
            // last ones hanging below the image another lane's height sets.
            let mut y = 3.0f32;
            let layout: Vec<(LaneKey, f32)> = keys
                .iter()
                .map(|k| {
                    let at = y;
                    y += lod::lane_height(*k) + lane_gap(*k) + 2.0;
                    (*k, at)
                })
                .collect();
            for width in [1usize, 33, 640] {
                let frame = index.rasterize_pixel_layout(0, 6_000, width, &keys, Some(&layout), None, None, false);
                assert_eq!(frame.to_rgba8_placed(&layout, scale), reference(&frame, &layout, scale), "{width} px at scale {scale}");
            }
        }
    }

    #[test]
    fn the_column_raster_is_the_naive_raster_on_every_lane_without_overlap() {
        let mut rng = Rng(42);
        for _ in 0..20 {
            let n = 300 + rng.below(3_000) as usize;
            let mut lane = Lane::default();
            let mut t = 0u64;
            for k in 0..n {
                let gap = 1 + rng.below(20);
                lane.insert(ev(t, 1 + rng.below(gap), (k % 251) as u32));
                t += gap;
            }
            for width in [1usize, 7, 64, 300] {
                let t0 = rng.below(t / 2);
                let t1 = t0 + 1 + rng.below(t);
                let mut cols = vec![0u32; width];
                let mut naive = vec![0u32; width];
                lane.rasterize_pixel_columns(t0, t1, width, &mut cols, None);
                // The reference for a column is the longest event overlapping it.
                let dt = (t1 - t0) as f64 / width as f64;
                for (x, px) in naive.iter_mut().enumerate() {
                    let col0 = t0.saturating_add((x as f64 * dt) as u64);
                    let col1 = t0.saturating_add(((x + 1) as f64 * dt) as u64).max(col0 + 1);
                    *px = reference_owner(&lane, col0, col1).map(|e| e.color_for(None)).unwrap_or(chrome::TRACK);
                }
                assert_eq!(cols, naive, "{n} events, [{t0}, {t1}) at {width} px");
            }
        }
    }
}
