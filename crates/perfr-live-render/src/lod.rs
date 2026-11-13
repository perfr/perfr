// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Timeline LOD: pixel columns when scopes are sub-pixel, instanced
//! rounded-rect primitives when a visible scope is wider than a few pixels.

use perfr_live_event::{kind, InternTable, LaneKey, LiveEvent};

use std::collections::HashMap;

use crate::{event_span_ns, gallop, par, Lane, TrackIndex};

pub const INSTANCE_MIN_PX: f32 = 4.0;
/// Extra stack pixels above/below the clip so scroll does not pop lanes.
pub const Y_CULL_PAD: f32 = 48.0;
const LOD_SAMPLE_LANES: usize = 8;

/// Visible stack-Y window, already including pad.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct YCull {
    pub y0: f32,
    pub y1: f32,
}

impl YCull {
    pub fn new(y0: f32, y1: f32) -> Self {
        Self {
            y0: y0.min(y1),
            y1: y0.max(y1),
        }
    }

    pub fn padded(scroll: f32, view_h: f32, pad: f32) -> Self {
        Self::new(scroll - pad, scroll + view_h + pad)
    }

    /// Content-space window from a scroll clip. `content_min_y` is the full
    /// strip top (same origin as `tracks.layout()` / VALUE rail Y).
    pub fn from_clip(content_min_y: f32, clip_min_y: f32, clip_h: f32, pad: f32) -> Self {
        Self::padded(clip_min_y - content_min_y, clip_h, pad)
    }

    pub fn hits(self, y: f32, h: f32) -> bool {
        y + h >= self.y0 && y <= self.y1
    }
}

/// Collect knobs. Default: no Y-cull (full stack), walked on the pool.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CollectOpts {
    pub y_cull: Option<YCull>,
    /// Has no effect. It used to stop a lane's walk after a scope that
    /// reaches the right edge of the window, which is where the walk stops
    /// anyway: the next event starts at or after the window's end. The
    /// field stays until the callers that set it stop doing so.
    pub early_out: bool,
    /// Walk the lanes on the calling thread instead of the pool. The caller
    /// decides from its own measurements: on a small window the hand-off to
    /// the workers and the join cost far more than the walk they parallelise.
    pub inline: bool,
    /// Time each lane's walk separately and name its self-profile span after
    /// the lane, instead of one span for the whole worker chunk. Costs two
    /// clock reads per lane, so it is only set when the Self pane is open to
    /// show which lane was slow.
    pub per_lane_spans: bool,
    /// One instance for a run of events that are each under a pixel wide
    /// and start in the same pixel column, instead of one each. It carries
    /// the identity and colour of the longest of them, which is the event
    /// the pixel-column LOD gives that column to.
    ///
    /// Off, every event in the window is an instance. That is what the
    /// listing did, and on a live capture of a busy process it was the
    /// frame: a view goes instanced as soon as one scope in it is a few
    /// pixels wide, and the other half a million events in the window, all
    /// far under a pixel, were each listed, sorted and uploaded every frame.
    pub merge_subpixel: bool,
}

impl Default for CollectOpts {
    fn default() -> Self {
        Self {
            y_cull: None,
            early_out: true,
            inline: false,
            per_lane_spans: false,
            merge_subpixel: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineLod {
    PixelColumns,
    Instanced,
}

impl TimelineLod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PixelColumns => "pixel_columns",
            Self::Instanced => "instanced",
        }
    }
}

/// Per-instance highlight. Packed in the unused `extra.y` vertex float.
pub const FLAG_NONE: f32 = 0.0;
pub const FLAG_HOVER: f32 = 1.0;
pub const FLAG_SELECTED: f32 = 2.0;
pub const FLAG_SIBLING: f32 = 3.0;
pub const FLAG_DIMMED: f32 = 4.0;
/// Not the selected thread (or not the target process): the flat grey C++
/// perfr paints every timer outside the selection with.
pub const FLAG_INACTIVE: f32 = 5.0;
/// A scheduler slice of another thread of the selected thread's process: a
/// lighter grey, so the process's other threads still read on the cores.
pub const FLAG_SAME_PID: f32 = 6.0;

/// Which threads are drawn in colour. This is C++ perfr's rule
/// (`SchedulerTrack::IsTimerActive`, `ThreadTrack::GetTimerColor`): with a
/// thread selected only that thread is active; with none, every thread of
/// the target process is -- or every thread, when the capture has no target.
/// The viewer's own tracks are always active; they are not part of the
/// capture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadFocus {
    /// `(pid, tid)` of the selected thread.
    pub selected: Option<(u32, u32)>,
    pub target_pid: Option<u32>,
}

impl ThreadFocus {
    /// Whether a thread track's scopes draw in colour: only the selected
    /// thread's when one is selected, everyone's otherwise. The target
    /// process plays no part here -- C++ perfr shows only the target's
    /// threads, so its "active" test never met another process's track;
    /// this viewer shows the service and every instrumented process too,
    /// and greying them whenever a capture has a target left most of the
    /// screen grey with nothing selected.
    pub fn active_on_track(&self, pid: u32, tid: u32) -> bool {
        if perfr_live_event::dev::is_self_pid(pid) {
            return true;
        }
        self.selected.is_none_or(|(_, t)| t == tid)
    }

    /// Whether a scheduler slice draws in colour: `SchedulerTrack::IsTimerActive`
    /// -- the selected thread's slices, or with none selected the target
    /// process's (every process's when the capture has no target).
    pub fn active_on_scheduler(&self, pid: u32, tid: u32) -> bool {
        if perfr_live_event::dev::is_self_pid(pid) {
            return true;
        }
        match self.selected {
            Some((_, t)) => tid == t,
            None => self.target_pid.is_none_or(|p| p == pid),
        }
    }

    /// Another thread of the selected thread's process.
    pub fn same_pid(&self, pid: u32) -> bool {
        self.selected.is_some_and(|(p, _)| p == pid)
    }

    /// Nothing is narrowed: every thread active, as when no capture target
    /// and no selection exist.
    pub fn is_all(&self) -> bool {
        self.selected.is_none() && self.target_pid.is_none()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeInstance {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: u32,
    pub radius: f32,
    pub name_id: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
    pub pid: u32,
    pub tid: u32,
    pub kind: u8,
    pub depth: u8,
    pub extra: u8,
    pub event_flags: u8,
    pub flags: f32,
}

/// Click / keyboard identity for a scope. CPU-only; the shader sees `flags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScopePick {
    pub name_id: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
    pub pid: u32,
    pub tid: u32,
    pub kind: u8,
    pub depth: u8,
    pub extra: u8,
    pub event_flags: u8,
}

impl ScopePick {
    pub fn from_event(e: LiveEvent) -> Self {
        Self {
            name_id: e.name_id,
            start_ns: e.start_ns,
            duration_ns: e.duration_ns,
            pid: e.pid,
            tid: e.tid,
            kind: e.kind,
            depth: e.depth,
            extra: e.extra,
            event_flags: e._pad,
        }
    }

    pub fn lane_key(self) -> LaneKey {
        LiveEvent {
            start_ns: self.start_ns,
            duration_ns: self.duration_ns,
            tid: self.tid,
            pid: self.pid,
            kind: self.kind,
            depth: self.depth,
            extra: self.extra,
            _pad: 0,
            name_id: self.name_id,
        }
        .lane_key()
    }

    pub fn from_instance(i: &ScopeInstance) -> Self {
        Self {
            name_id: i.name_id,
            start_ns: i.start_ns,
            duration_ns: i.duration_ns,
            pid: i.pid,
            tid: i.tid,
            kind: i.kind,
            depth: i.depth,
            extra: i.extra,
            event_flags: i.event_flags,
        }
    }

    pub fn matches_instance(self, i: &ScopeInstance) -> bool {
        i.start_ns == self.start_ns && i.name_id == self.name_id
    }
}

/// Clock readings around the sequential parts of a listing, so the viewer
/// can name them in its self-profile. All on `perfr_live_event::dev::now_ns`,
/// the same clock as the worker spans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListingTiming {
    pub dispatch_t0_ns: u64,
    pub dispatch_t1_ns: u64,
    pub flatten_t0_ns: u64,
    pub flatten_t1_ns: u64,
    pub sort_t0_ns: u64,
    pub sort_t1_ns: u64,
}

#[derive(Clone, Debug)]
pub struct InstanceFrame {
    pub timing: ListingTiming,
    pub width: f32,
    pub height: f32,
    pub lanes: Vec<LaneKey>,
    pub instances: Vec<ScopeInstance>,
    pub worker_spans: Vec<crate::WorkerSpan>,
    pub lanes_kept: u32,
    /// Lanes whose row came out of the [`ListingCache`] instead of a walk.
    pub lanes_reused: u32,
    /// Lanes whose cached row was kept, where it was or moved along with
    /// the window, and only the events that arrived since were walked.
    pub lanes_extended: u32,
}

/// What one lane's listing depended on besides the lane's own events. Same
/// key and same events, same instances.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowKey {
    t0: u64,
    t1: u64,
    width_bits: u32,
    /// [`InternTable::color_epoch`]: the colours of names that were known.
    names: (u64, u64),
    merge: bool,
}

/// The lane as a row was listed from it.
#[derive(Clone, Copy, Debug)]
struct RowState {
    lane: u64,
    version: u64,
    reshaped: u64,
    /// The event the row's last instance begins at, and that instance's
    /// place in the row. Everything before it is final while the lane only
    /// gains events at its end; the last instance is not, since an event
    /// that arrives next may start in the same pixel column and join it.
    /// `None` for a row that is listed whole every time: an empty one, or
    /// one whose instances are not in the order of the events.
    resume: Option<(usize, usize)>,
    /// The size of the name table, when the row holds an event whose name
    /// the table did not have yet. Such a row is good until a name arrives,
    /// which may be the one it is waiting for.
    unnamed_at: Option<usize>,
}

struct CachedRow {
    key: RowKey,
    state: RowState,
    /// Where the row was listed. Not part of the key: a lane appearing
    /// above moves every row below it, which a live capture does many times
    /// a second while threads and stack depths are still turning up, and a
    /// row that only moved is the same row at another height.
    y_bits: u32,
    row: Vec<ScopeInstance>,
    /// The [`ListingCache::frame`] that last listed or reused the row.
    seen: u64,
}

/// Per-lane listing rows from the previous frame (TODO item 21). During a
/// live capture the listing window holds still for many frames -- Follow
/// lists a window wider than the view and pans inside it -- and what changes
/// from one frame to the next is the handful of events each lane received.
/// A lane whose events, window, width and y are unchanged hands back the row
/// it produced last time. One that only gained events at its end keeps its
/// row and walks the new events alone. Only a lane that changed some other
/// way, or a window that moved, is listed from the start.
#[derive(Default)]
pub struct ListingCache {
    rows: HashMap<LaneKey, CachedRow>,
    /// Counts the listings, so the rows the last one did not touch can go.
    frame: u64,
}

impl ListingCache {
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

enum Row {
    /// Culled, absent from the index, or a lane that draws no instances.
    Skipped,
    /// The cached row stands as it is, `len` instances.
    Reused { len: usize },
    /// The cached row's first `keep` instances stand; `tail` follows them.
    Extended { keep: usize, tail: Vec<ScopeInstance>, state: RowState },
    /// The cached row moved to a window that starts a whole number of
    /// pixel columns later, and listed on from where it ended.
    Slid { row: Vec<ScopeInstance>, state: RowState },
    Fresh { row: Vec<ScopeInstance>, state: RowState },
}

impl Row {
    fn len(&self) -> usize {
        match self {
            Row::Skipped => 0,
            Row::Reused { len } => *len,
            Row::Extended { keep, tail, .. } => keep + tail.len(),
            Row::Slid { row, .. } | Row::Fresh { row, .. } => row.len(),
        }
    }
}

/// How many whole pixel columns later `to`'s window starts than `from`'s,
/// when that is all the two differ by: the same span over the same width,
/// begun a whole number of columns on and before the old window's end.
///
/// A view that follows a live capture is listed for a window somewhat wider
/// than itself and panned inside it; when it reaches the window's end the
/// next window is this one, the last moved along. The instances a row holds
/// are then the same instances that many pixels to the left.
fn window_slide(from: &RowKey, to: &RowKey) -> Option<u32> {
    let same_scale = from.width_bits == to.width_bits
        && from.t1.saturating_sub(from.t0) == to.t1.saturating_sub(to.t0)
        && from.names == to.names
        && from.merge == to.merge;
    if !same_scale || to.t0 <= from.t0 || to.t0 >= from.t1 {
        return None;
    }
    let ns_per_px = (to.t1 - to.t0) as f64 / f32::from_bits(to.width_bits) as f64;
    let shift = (to.t0 - from.t0) as f64;
    let columns = (shift / ns_per_px).round();
    // To the nanosecond the window's start was rounded to.
    (columns >= 1.0 && (columns * ns_per_px - shift).abs() <= 1.0).then_some(columns as u32)
}

/// `row` as it stands in a window that starts `columns` pixels later: what
/// has left the window dropped, the rest moved left. `width` is the
/// window's. `None` when an instance was cut short at the old window's end
/// and so is not what the new window would list.
fn slide_row(row: &[ScopeInstance], columns: u32, width: f32, y: f32) -> Option<Vec<ScopeInstance>> {
    if row.last().is_some_and(|last| last.x + last.w >= width - 0.5) {
        return None;
    }
    let dx = columns as f32;
    // Starts are in order and so are ends, in a row listed by column.
    let gone = row.partition_point(|i| i.x + i.w <= dx);
    Some(
        row[gone..]
            .iter()
            .map(|i| {
                let x = (i.x - dx).max(0.0);
                ScopeInstance { x, w: (i.x + i.w - dx - x).max(1.0), y, ..*i }
            })
            .collect(),
    )
}

/// The window a listing is for, and how much time one pixel of it holds.
struct ListWindow {
    t0: u64,
    t1: u64,
    span: f64,
    width: f32,
    ns_per_px: f64,
}

pub fn lane_height(key: LaneKey) -> f32 {
    match key.kind {
        kind::THREAD_STATE => 10.0,
        // A tick strip, not a lane of boxes: it carries no text, so it only
        // needs to be tall enough to read as a bar of marks.
        kind::SAMPLE => 8.0,
        kind::LOG => 8.0,
        kind::DROP => 6.0,
        kind::SCHEDULING_SLICE => 12.0,
        kind::VALUE => 38.0,
        _ => 20.0,
    }
}

pub fn lane_gap(key: LaneKey) -> f32 {
    match key.kind {
        kind::THREAD_STATE => 3.0,
        _ => 1.0,
    }
}

/// Sort leaf lanes under one thread: state, cpu, then scopes by depth.
pub fn sort_thread_leaves(lanes: &mut [LaneKey]) {
    lanes.sort_by_key(|k| (leaf_rank(k.kind), k.depth, k.extra, k.tid));
}

fn leaf_rank(kind_id: u8) -> u8 {
    match kind_id {
        kind::THREAD_STATE => 0,
        // Directly under the state bar: both answer "what was this thread
        // doing", at a glance, before any stack detail.
        kind::SAMPLE => 1,
        kind::LOG => 1,
        kind::DROP => 1,
        kind::SCHEDULING_SLICE => 2,
        kind::API_SCOPE => 2,
        kind::FUNCTION_CALL => 3,
        kind::API_TRACK => 4,
        kind::VALUE => 6,
        _ => 5,
    }
}

pub fn leaf_label(key: LaneKey) -> String {
    match key.kind {
        kind::THREAD_STATE => "state".into(),
        kind::SAMPLE => "samples".into(),
        kind::LOG => "logs".into(),
        kind::DROP => "drops".into(),
        kind::SCHEDULING_SLICE => format!("Core {}", key.extra),
        kind::FUNCTION_CALL => "calls".into(),
        kind::API_TRACK => "async".into(),
        kind::API_SCOPE if key.depth == 0 => "scopes".into(),
        kind::API_SCOPE => format!("d{}", key.depth),
        kind::VALUE => "graph".into(),
        _ => "lane".into(),
    }
}

pub fn stack_height(index: &TrackIndex) -> f32 {
    stack_height_keys(index.lanes().map(|(k, _)| k))
}

pub fn stack_height_keys(keys: impl IntoIterator<Item = LaneKey>) -> f32 {
    keys.into_iter().map(|k| lane_height(k) + lane_gap(k)).sum()
}

/// Top-to-bottom y of each lane, starting at `y0`.
pub fn stacked_layout(keys: &[LaneKey], y0: f32) -> Vec<(LaneKey, f32)> {
    let mut y = y0;
    let mut out = Vec::with_capacity(keys.len());
    for &key in keys {
        out.push((key, y));
        y += lane_height(key) + lane_gap(key);
    }
    out
}

/// Lanes whose events are marks, not spans: `duration_ns` holds something
/// else (a value's bits, a sampling period, a count of lost events, nothing)
/// and the event draws one pixel wide at every zoom. They never make a view
/// instanced, whatever that field would come to in pixels.
fn is_tick_lane(key: LaneKey) -> bool {
    matches!(key.kind, kind::VALUE | kind::SAMPLE | kind::LOG | kind::DROP)
}

/// Sample a few lanes. If any overlapping event is ≥ `min_wide_px`, use
/// instanced SDF quads. Does not walk all scopes.
///
/// Samples the densest lanes (by event count) plus an optional cursor
/// hint. Lanes of marks are never asked; see [`is_tick_lane`].
pub fn choose_lod(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: usize,
    min_wide_px: f32,
) -> TimelineLod {
    choose_lod_hint(index, t0, t1, width, min_wide_px, None)
}

/// Same as [`choose_lod`], always including `hint` (cursor / hover lane).
pub fn choose_lod_hint(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: usize,
    min_wide_px: f32,
    hint: Option<LaneKey>,
) -> TimelineLod {
    choose_lod_from_keys(
        index,
        t0,
        t1,
        width,
        min_wide_px,
        &sample_lod_lanes(index, hint),
    )
}

/// Densest lanes of spans (by `Lane::len`) plus `hint`. O(lanes), not O(scopes).
pub fn sample_lod_lanes(index: &TrackIndex, hint: Option<LaneKey>) -> Vec<LaneKey> {
    let mut scored: Vec<(usize, LaneKey)> = index
        .lanes()
        .filter(|(k, _)| !is_tick_lane(*k))
        .map(|(k, lane)| (lane.len(), k))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = Vec::with_capacity(LOD_SAMPLE_LANES + 1);
    let mut hinted = None;
    if let Some(h) = hint {
        if !is_tick_lane(h) && index.lane(h).is_some() {
            out.push(h);
            hinted = Some(h);
        }
    }
    // The hint is extra, not a replacement. Counting it against the budget
    // dropped the least dense of the sampled lanes, so moving the cursor could
    // evict the one lane holding a wide scope.
    let budget = LOD_SAMPLE_LANES + usize::from(hinted.is_some());
    for (_, k) in scored {
        if out.len() >= budget {
            break;
        }
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

fn choose_lod_from_keys(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: usize,
    min_wide_px: f32,
    keys: &[LaneKey],
) -> TimelineLod {
    if width == 0 || t1 <= t0 {
        return TimelineLod::PixelColumns;
    }
    let ns_per_px = (t1 - t0) as f64 / width as f64;
    if ns_per_px <= 0.0 {
        return TimelineLod::PixelColumns;
    }
    let wide = |duration_ns: u64| (duration_ns as f64 / ns_per_px) >= min_wide_px as f64;
    // Fast path. A hit here is conclusive: one wide scope in view is enough.
    for key in keys {
        if is_tick_lane(*key) {
            continue;
        }
        let Some(lane) = index.lane(*key) else {
            continue;
        };
        if lane.any_duration(t0, t1, wide) {
            return TimelineLod::Instanced;
        }
    }
    // A miss is not conclusive, so finish the question over every lane.
    //
    // Answering PixelColumns straight from a sample miss made the verdict
    // depend on which lanes happened to be sampled, and the sample moves: it
    // is ranked by lifetime event count and takes the hovered lane as a hint.
    // Moving the cursor, changing nothing else, could flip instanced to blit.
    // The cost is two binary searches per lane and a comparison per block
    // of its events in view, paid only when the sample already failed to
    // find anything wide.
    for (key, lane) in index.lanes() {
        if is_tick_lane(key) {
            continue;
        }
        if lane.any_duration(t0, t1, wide) {
            return TimelineLod::Instanced;
        }
    }
    TimelineLod::PixelColumns
}

#[cfg(test)]
mod value_lod_tests {
    use super::*;
    use crate::TrackIndex;
    use perfr_live_event::{LaneKey, LiveEvent};

    #[test]
    fn value_bits_do_not_force_instanced_lod() {
        let mut idx = TrackIndex::default();
        idx.insert(LiveEvent::from_value(0, 1, 1, 1, 1.0e20));
        idx.insert(LiveEvent {
            start_ns: 0,
            duration_ns: 8,
            tid: 2,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: 2,
        });
        assert_eq!(
            choose_lod(&idx, 0, 1_000_000, 200, INSTANCE_MIN_PX),
            TimelineLod::PixelColumns
        );
    }

    fn scope(start: u64, dur: u64, tid: u32, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: dur,
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    /// A sample tick's `duration_ns` is the sampling period and a drop
    /// marker's is a count of lost events. Both draw as a one-pixel tick at
    /// every zoom, so neither is a wide scope, whatever the field says.
    #[test]
    fn ticks_do_not_force_instanced_lod_with_the_number_in_their_duration_field() {
        for tick in [kind::SAMPLE, kind::DROP] {
            let mut idx = TrackIndex::default();
            for i in 0..64u64 {
                idx.insert(scope(i * 20, 8, 1, 1));
            }
            // 1 ms of "duration" in a 1 ms window: 200 px, were it a span.
            idx.insert(LiveEvent { kind: tick, ..scope(500, 1_000_000, 2, 2) });
            assert_eq!(
                choose_lod(&idx, 0, 1_000_000, 200, INSTANCE_MIN_PX),
                TimelineLod::PixelColumns,
                "kind {tick}"
            );
        }
    }

    #[test]
    fn density_sample_hits_busy_lane_past_first_eight() {
        let mut idx = TrackIndex::default();
        for tid in 1..=8u32 {
            idx.insert(scope(0, 8, tid, tid));
        }
        for i in 0..24u32 {
            idx.insert(scope(u64::from(i) * 5_000, 4_800, 99, 100 + i));
        }
        // The first eight lanes in key order do not include tid 99, which
        // is why the sample ranks by density instead.
        let first8: Vec<LaneKey> = idx
            .lanes()
            .map(|(k, _)| k)
            .filter(|k| k.kind != kind::VALUE)
            .take(8)
            .collect();
        assert!(
            !first8.iter().any(|k| k.tid == 99),
            "fixture is only meaningful while first-8 misses the busy lane"
        );
        assert_eq!(
            choose_lod(&idx, 0, 100_000, 100, INSTANCE_MIN_PX),
            TimelineLod::Instanced
        );
        let hint = idx
            .lanes()
            .find(|(k, _)| k.tid == 99)
            .map(|(k, _)| k)
            .unwrap();
        assert_eq!(
            choose_lod_hint(&idx, 0, 100_000, 100, INSTANCE_MIN_PX, Some(hint)),
            TimelineLod::Instanced
        );
        let sampled = sample_lod_lanes(&idx, None);
        assert!(sampled.iter().any(|k| k.tid == 99));
        assert!(sampled.iter().all(|k| k.kind != kind::VALUE));
    }

    #[test]
    fn y_cull_skips_offscreen_lanes() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 50, 1, 1));
        idx.insert(scope(0, 50, 2, 2));
        let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        let layout = stacked_layout(&keys, 0.0);
        assert_eq!(layout.len(), 2);
        let all = collect_instances_layout(&idx, 0, 50, 100.0, &layout, None);
        assert_eq!(all.instances.len(), 2);
        let y0 = layout[0].1;
        let h0 = lane_height(layout[0].0);
        let culled = collect_instances_layout_opts(
            &idx,
            0,
            50,
            100.0,
            &layout,
            None,
            CollectOpts { y_cull: Some(YCull::new(y0, y0 + h0)), ..CollectOpts::default() },
        );
        assert_eq!(culled.instances.len(), 1);
        assert_eq!(culled.instances[0].name_id, all.instances[0].name_id);
    }

    #[test]
    fn a_scope_that_fills_the_window_is_its_lanes_only_instance() {
        let mut idx = TrackIndex::default();
        idx.insert(scope(0, 10_000, 1, 1));
        for i in 0..64u64 {
            idx.insert(scope(10_000 + i * 10, 8, 1, 10 + i as u32));
        }
        let wide = collect_instances_layout_opts(
            &idx,
            100,
            400,
            200.0,
            &stacked_layout(&idx.lanes().map(|(k, _)| k).collect::<Vec<_>>(), 0.0),
            None,
            CollectOpts::default(),
        );
        assert_eq!(wide.instances.len(), 1);
        assert!(wide.instances[0].w >= 199.0);
        let later = collect_instances_layout_opts(
            &idx,
            10_000,
            10_080,
            200.0,
            &stacked_layout(&idx.lanes().map(|(k, _)| k).collect::<Vec<_>>(), 0.0),
            None,
            CollectOpts::default(),
        );
        assert!(
            later.instances.len() > 1,
            "must still emit later scopes that start before t1"
        );
    }

    #[test]
    fn value_lanes_in_view_follow_y_cull_not_gpu() {
        let vk = LaneKey {
            pid: 1,
            tid: 1,
            kind: kind::VALUE,
            depth: 0,
            extra: 0,
        };
        let sk = LaneKey {
            pid: 1,
            tid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
        };
        let layout = [(sk, 0.0), (vk, 100.0)];
        let all = value_lanes_in_view(&layout, 1.0, None);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, vk);
        assert!((all[0].2 - lane_height(vk)).abs() < 0.01);
        let kept = value_lanes_in_view(&layout, 1.0, Some(YCull::new(80.0, 200.0)));
        assert_eq!(kept.len(), 1);
        let skipped = value_lanes_in_view(&layout, 1.0, Some(YCull::new(0.0, 50.0)));
        assert!(skipped.is_empty());
        let compact = value_lanes_in_view(&layout, 0.72, Some(YCull::new(90.0, 140.0)));
        assert_eq!(compact.len(), 1);
        let inst = collect_instances_layout_opts(
            &TrackIndex::default(),
            0,
            10,
            40.0,
            &layout,
            None,
            CollectOpts { y_cull: Some(YCull::new(80.0, 200.0)), ..CollectOpts::default() },
        );
        assert!(inst.instances.iter().all(|i| i.kind != kind::VALUE));
    }

    #[test]
    fn y_cull_from_clip_tracks_viewport_height() {
        let a = YCull::from_clip(0.0, 10.0, 100.0, 0.0);
        let b = YCull::from_clip(0.0, 10.0, 200.0, 0.0);
        assert_ne!(a, b);
        assert!((a.y1 - a.y0 - 100.0).abs() < 0.01);
        let scrolled = YCull::from_clip(-80.0, 10.0, 100.0, 0.0);
        assert!((scrolled.y0 - 90.0).abs() < 0.01);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn parallel_collect_emits_distinct_worker_tids() {
        if crate::parallelism() < 2 {
            return;
        }
        let run = || {
            let mut idx = TrackIndex::default();
            for i in 0..64u32 {
                idx.insert(scope(0, 50, i, i + 1));
            }
            let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
            let layout = stacked_layout(&keys, 0.0);
            collect_instances_layout(&idx, 0, 50, 100.0, &layout, None)
        };
        #[cfg(feature = "parallel")]
        let frame = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .expect("rayon pool")
            .install(run);
        #[cfg(not(feature = "parallel"))]
        let frame = run();
        let tids: std::collections::HashSet<u32> =
            frame.worker_spans.iter().map(|s| s.tid).collect();
        assert!(
            tids.len() >= 2,
            "expected distinct render-worker tids, got {tids:?}"
        );
        assert!(tids
            .iter()
            .all(|t| perfr_live_event::dev::is_render_worker_tid(*t)));
    }
}

/// Visible events only: per-lane binary search then walk while `start < t1`.
pub fn collect_instances(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: f32,
    y0: f32,
    intern: Option<&InternTable>,
) -> InstanceFrame {
    let keys: Vec<LaneKey> = index.lanes().map(|(k, _)| k).collect();
    let layout = stacked_layout(&keys, y0);
    collect_instances_layout(index, t0, t1, width, &layout, intern)
}

/// Same as [`collect_instances`] with an explicit (lane, y) remap — session
/// track order / drag animation. Still walks only visible events.
pub fn collect_instances_layout(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: f32,
    layout: &[(LaneKey, f32)],
    intern: Option<&InternTable>,
) -> InstanceFrame {
    collect_instances_layout_opts(index, t0, t1, width, layout, intern, CollectOpts::default())
}

/// [`collect_instances_layout`] with Y-cull / early-out knobs (benches, viewer).
pub fn collect_instances_layout_opts(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: f32,
    layout: &[(LaneKey, f32)],
    intern: Option<&InternTable>,
    opts: CollectOpts,
) -> InstanceFrame {
    collect_instances_cached(index, t0, t1, width, layout, intern, opts, None)
}

/// [`collect_instances_layout_opts`] reusing last frame's rows for the lanes
/// that did not change; see [`ListingCache`].
#[allow(clippy::too_many_arguments)]
pub fn collect_instances_cached(
    index: &TrackIndex,
    t0: u64,
    t1: u64,
    width: f32,
    layout: &[(LaneKey, f32)],
    intern: Option<&InternTable>,
    opts: CollectOpts,
    cache: Option<&mut ListingCache>,
) -> InstanceFrame {
    let keys: Vec<LaneKey> = layout.iter().map(|(k, _)| *k).collect();
    let height = layout
        .last()
        .map(|(k, y)| *y + lane_height(*k) + lane_gap(*k))
        .unwrap_or(0.0);
    if width <= 0.0 || t1 <= t0 {
        return InstanceFrame {
            timing: ListingTiming::default(),
            width,
            height,
            lanes: keys,
            instances: Vec::new(),
            worker_spans: Vec::new(),
            lanes_kept: 0,
            lanes_reused: 0,
            lanes_extended: 0,
        };
    }
    let span = (t1 - t0) as f64;
    let win = ListWindow { t0, t1, span, width, ns_per_px: span / width as f64 };
    let merge = opts.merge_subpixel;
    let now = perfr_live_event::dev::now_ns;
    let mut timing = ListingTiming { dispatch_t0_ns: now(), ..ListingTiming::default() };
    let intern_len = intern.map(InternTable::len).unwrap_or(0);
    let names = intern.map(InternTable::color_epoch).unwrap_or_default();
    let row_key = RowKey { t0, t1, width_bits: width.to_bits(), names, merge };
    // The walk reads the cache; the writes wait until it is over.
    let reads: Option<&ListingCache> = cache.as_deref();
    let walk = |&(key, y): &(LaneKey, f32)| {
        if key.kind == kind::VALUE {
            return Row::Skipped;
        }
        let h = lane_height(key);
        if let Some(cull) = opts.y_cull {
            if !cull.hits(y, h + lane_gap(key)) {
                return Row::Skipped;
            }
        }
        let Some(lane) = index.lane(key) else {
            return Row::Skipped;
        };
        let state = |resume, unnamed: bool| RowState {
            lane: lane.id(),
            version: lane.version(),
            reshaped: lane.reshaped(),
            resume,
            unnamed_at: unnamed.then_some(intern_len),
        };
        // A row listed column by column is in the order of the events and
        // can be picked up again where it ended.
        let by_column = merge && lane.ends_sorted();
        let cached = reads
            .and_then(|c| c.rows.get(&key))
            .filter(|c| c.state.lane == lane.id() && c.state.unnamed_at.is_none_or(|len| len == intern_len));
        // The window moved along by whole columns under a lane that only
        // gained events: the row's instances move with it, all but the last,
        // which is listed again along with what is new.
        if let Some((cached, (event, at))) = cached
            .filter(|c| by_column && c.state.reshaped == lane.reshaped())
            .and_then(|c| Some((c, c.state.resume?)))
        {
            let slid = window_slide(&cached.key, &row_key).and_then(|columns| slide_row(&cached.row[..at], columns, width, y));
            if let Some(mut row) = slid {
                let mut unnamed = cached.state.unnamed_at.is_some();
                let resume = list_lane_columns(lane, event, &win, y, h, intern, &mut row, &mut unnamed);
                return Row::Slid { row, state: state(resume, unnamed) };
            }
        }
        if let Some(cached) = cached.filter(|c| c.key == row_key) {
            if cached.state.version == lane.version() {
                return Row::Reused { len: cached.row.len() };
            }
            if let (true, Some((event, keep))) =
                (by_column && cached.state.reshaped == lane.reshaped(), cached.state.resume)
            {
                let mut tail = Vec::new();
                let mut unnamed = cached.state.unnamed_at.is_some();
                let resume = list_lane_columns(lane, event, &win, y, h, intern, &mut tail, &mut unnamed);
                return Row::Extended { keep, tail, state: state(resume.map(|(e, at)| (e, keep + at)), unnamed) };
            }
        }
        let mut row = Vec::new();
        let mut unnamed = false;
        let resume = if by_column {
            list_lane_columns(lane, lane.first_ending_after(t0), &win, y, h, intern, &mut row, &mut unnamed)
        } else {
            list_lane_events(lane, &win, y, h, intern, merge, &mut row, &mut unnamed);
            None
        };
        Row::Fresh { row, state: state(resume, unnamed) }
    };
    let (parts, worker_spans): (Vec<Row>, Vec<par::WorkerSpan>) = if opts.inline {
        (layout.iter().map(walk).collect(), Vec::new())
    } else if opts.per_lane_spans {
        // Name each lane's span after its thread (tid), resolvable on the Self
        // pane through the shared intern table.
        par::map_collect_lanes_labeled(layout, |(k, _): &(LaneKey, f32)| k.tid, walk)
    } else {
        par::map_collect_lanes(layout, walk)
    };
    timing.dispatch_t1_ns = now();
    let lanes_kept = parts.iter().filter(|p| p.len() > 0).count() as u32;
    let lanes_reused = parts.iter().filter(|p| matches!(p, Row::Reused { .. })).count() as u32;
    let lanes_extended = parts.iter().filter(|p| matches!(p, Row::Extended { .. } | Row::Slid { .. })).count() as u32;
    timing.flatten_t0_ns = now();
    let total: usize = parts.iter().map(Row::len).sum();
    // The flatten is a copy into a fresh buffer either way: the frame's
    // instances leave for the GPU upload and are not seen again. What the
    // cache saves is the walk, which the bench times on its own.
    let mut instances: Vec<ScopeInstance> = Vec::with_capacity(total);
    match cache {
        Some(cache) => {
            cache.frame = cache.frame.wrapping_add(1);
            let frame = cache.frame;
            // A kept row that was listed at another height moves to this one.
            let move_to = |cached: &mut CachedRow, y: f32| {
                if cached.y_bits != y.to_bits() {
                    cached.y_bits = y.to_bits();
                    for inst in &mut cached.row {
                        inst.y = y;
                    }
                }
            };
            for ((key, y), part) in layout.iter().zip(parts) {
                match part {
                    Row::Skipped => {}
                    Row::Reused { .. } => {
                        if let Some(cached) = cache.rows.get_mut(key) {
                            move_to(cached, *y);
                            cached.seen = frame;
                            instances.extend_from_slice(&cached.row);
                        }
                    }
                    Row::Extended { keep, tail, state } => {
                        if let Some(cached) = cache.rows.get_mut(key) {
                            cached.row.truncate(keep);
                            move_to(cached, *y);
                            cached.row.extend_from_slice(&tail);
                            cached.state = state;
                            cached.seen = frame;
                            instances.extend_from_slice(&cached.row);
                        }
                    }
                    Row::Slid { row, state } | Row::Fresh { row, state } => {
                        instances.extend_from_slice(&row);
                        let cached = CachedRow { key: row_key, state, y_bits: y.to_bits(), row, seen: frame };
                        cache.rows.insert(*key, cached);
                    }
                }
            }
            // Rows of lanes this listing did not reach go: a lane no longer
            // laid out, and a culled one, which is not worth a slot either.
            // Told by the stamp; asking the layout for each cached row was a
            // walk of the layout per row.
            cache.rows.retain(|_, cached| cached.seen == frame);
        }
        None => {
            for part in parts {
                if let Row::Slid { row, .. } | Row::Fresh { row, .. } = part {
                    instances.extend_from_slice(&row);
                }
            }
        }
    }
    timing.flatten_t1_ns = now();
    // Nothing left to sort globally; the timing stays so the self-profile
    // shows the step at its new cost.
    timing.sort_t0_ns = now();
    timing.sort_t1_ns = now();
    InstanceFrame {
        timing,
        width,
        height,
        lanes: keys,
        instances,
        worker_spans,
        lanes_kept,
        lanes_reused,
        lanes_extended,
    }
}

/// VALUE graph lanes whose `[y, y+h]` intersects `y_cull` (or all if `None`).
/// Not an SDF/instance path — egui polylines only.
pub fn value_lanes_in_view(
    layout: &[(LaneKey, f32)],
    scale: f32,
    y_cull: Option<YCull>,
) -> Vec<(LaneKey, f32, f32)> {
    let s = scale.max(0.01);
    layout
        .iter()
        .filter_map(|&(k, y)| {
            if k.kind != kind::VALUE {
                return None;
            }
            let h = lane_height(k) * s;
            if y_cull.is_some_and(|c| !c.hits(y, h)) {
                return None;
            }
            Some((k, y, h))
        })
        .collect()
}

/// Instance whose rect contains `(x, y)` in the same space as `x/y/w/h`.
///
/// Longest true `duration_ns` wins so a 1 px tick cannot steal the hit-test
/// from a longer same-lane scope under that pixel. Equal duration keeps
/// last-in-list (painter's top among siblings).
pub fn pick_instance_at(instances: &[ScopeInstance], x: f32, y: f32) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, inst) in instances.iter().enumerate() {
        if x < inst.x || x > inst.x + inst.w || y < inst.y || y > inst.y + inst.h {
            continue;
        }
        match best {
            None => best = Some(i),
            Some(j) => {
                if inst.duration_ns >= instances[j].duration_ns {
                    best = Some(i);
                }
            }
        }
    }
    best
}

/// Painter's algorithm: shorter first so longer scopes cover 1 px ticks.
/// Applied per lane, on whichever thread walked the lane: instances only
/// ever overlap within one lane row, so a global sort over every instance
/// of the frame bought nothing a per-lane sort does not. Picking does not
/// depend on order, it takes the longest instance under the cursor.
fn sort_instances_longer_on_top(instances: &mut [ScopeInstance]) {
    instances.sort_unstable_by_key(|i| i.duration_ns);
}

/// Pixel-column pick: lane under `y`, then the overlapping event at `x`.
pub fn pick_column_event(
    index: &TrackIndex,
    layout: &[(LaneKey, f32)],
    t0: u64,
    t1: u64,
    width: f32,
    x: f32,
    y: f32,
    scale: f32,
) -> Option<LiveEvent> {
    if width <= 0.0 || t1 <= t0 {
        return None;
    }
    let key = layout.iter().find_map(|(k, ly)| {
        let h = (lane_height(*k) + lane_gap(*k)) * scale.max(0.01);
        if y >= *ly && y < *ly + h {
            Some(*k)
        } else {
            None
        }
    })?;
    let span = (t1 - t0) as f64;
    let col0 = t0.saturating_add((x.max(0.0) as f64 / width as f64 * span) as u64);
    let col1 = t0
        .saturating_add(((x.max(0.0) as f64 + 1.0) / width as f64 * span) as u64)
        .max(col0.saturating_add(1));
    index.lane(key)?.overlapping(col0, col1).copied()
}

pub fn apply_highlight_flags(
    instances: &mut [ScopeInstance],
    selected: Option<ScopePick>,
    hover: Option<ScopePick>,
    search: Option<&std::collections::HashSet<u32>>,
    focus: ThreadFocus,
) {
    for inst in instances.iter_mut() {
        let mut f = FLAG_NONE;
        if inst.kind == kind::SCHEDULING_SLICE {
            let active = focus.active_on_scheduler(inst.pid, inst.tid);
            // The scheduler track, as C++ perfr's SchedulerTrack::GetTimerColor
            // orders it: hover, then the selected timer, then everything
            // outside the selection in grey -- a lighter grey for the
            // selected process's other threads -- then the thread colour.
            if hover.is_some_and(|h| h.matches_instance(inst)) {
                f = FLAG_HOVER;
            } else if selected.is_some_and(|s| s.matches_instance(inst)) {
                f = FLAG_SELECTED;
            } else if !active {
                f = if focus.same_pid(inst.pid) { FLAG_SAME_PID } else { FLAG_INACTIVE };
            } else if search.is_some_and(|ids| !ids.contains(&inst.name_id)) {
                f = FLAG_DIMMED;
            }
            inst.flags = f;
            continue;
        }
        // Thread tracks keep their colours whatever thread is selected: the
        // selection is read off the scheduler, where the selected thread's
        // slices stay in colour and every other thread's go grey. Greying
        // the other threads' scopes too hid the very thing a selection is
        // for, comparing what this thread did against the others.
        if let Some(ids) = search {
            if !ids.contains(&inst.name_id) {
                f = FLAG_DIMMED;
            }
        }
        if let Some(h) = hover {
            if h.matches_instance(inst) {
                f = FLAG_HOVER;
            }
        }
        if let Some(s) = selected {
            if s.matches_instance(inst) {
                f = FLAG_SELECTED;
            } else if s.name_id != 0 && inst.name_id == s.name_id {
                f = FLAG_SIBLING;
            }
        }
        inst.flags = f;
    }
}

/// Lists a lane whose events do not overlap, from event `from` on: one
/// instance for each event at least a pixel wide, and one for each run of
/// narrower events that start in the same pixel column.
///
/// The instances are pushed in the order of the events. That is also
/// painter's order here: only an instance drawn wider than its event (the
/// one-pixel minimum) reaches into the next, and the next is drawn over it.
///
/// A run is found, not walked: one gallop to the first event that starts
/// past the column and a block search for the longest event before it. A
/// lane that holds a thousand events to the pixel costs what one that holds
/// two does.
///
/// Returns the event the last instance begins at and that instance's index
/// in `out`, for [`RowState::resume`].
fn list_lane_columns(
    lane: &Lane,
    from: usize,
    win: &ListWindow,
    y: f32,
    h: f32,
    intern: Option<&InternTable>,
    out: &mut Vec<ScopeInstance>,
    unnamed: &mut bool,
) -> Option<(usize, usize)> {
    let events = lane.events();
    let radius = (h * 0.14).clamp(2.0, 3.0);
    let narrow = |e: &LiveEvent| (event_span_ns(e) as f64) < win.ns_per_px;
    let mut instance = |e: &LiveEvent| {
        let (inst, named) = instance_named(e, win.t0, win.t1, win.span, win.width, y, h, radius, intern);
        *unnamed |= !named;
        inst
    };
    let px = |ns: u64| (ns as f64 / win.span) * win.width as f64;
    let mut last = None;
    let mut i = from;
    while let Some(e) = events.get(i) {
        if e.start_ns >= win.t1 {
            break;
        }
        if e.end_ns() <= win.t0 {
            i += 1;
            continue;
        }
        last = Some((i, out.len()));
        if !narrow(e) {
            out.push(instance(e));
            i += 1;
            continue;
        }
        // The column `e` starts in ends at `col_end`; `events[i..j]` start
        // in it.
        let start = e.start_ns.max(win.t0);
        let col = ((start - win.t0) as f64 / win.ns_per_px) as u64;
        let col_end = win
            .t0
            .saturating_add(((col + 1) as f64 * win.ns_per_px).ceil() as u64)
            .max(start.saturating_add(1))
            .min(win.t1);
        let j = gallop(events, i + 1, |x| x.start_ns < col_end);
        // All of them but the last end inside the column, since nothing
        // overlaps, and so are narrow. The last may run on past it; then it
        // is listed on its own, next. The walk is for a lane whose ends are
        // in order though its events overlap.
        let all_narrow = |a: usize, b: usize| lane.longest_in(a, b).is_none_or(|k| narrow(&events[k]));
        let run_end = if all_narrow(i, j) {
            j
        } else if all_narrow(i, j - 1) {
            j - 1
        } else {
            (i + 1..j).find(|&k| !narrow(&events[k])).unwrap_or(j)
        };
        if run_end - i == 1 {
            out.push(instance(e));
        } else {
            // The run draws as its longest event, over what its events
            // would cover drawn one by one: each is a pixel wide from where
            // it starts, so from the first start to a pixel past the last.
            // Stopping where the last one ends drew a busy lane in slivers,
            // with the gaps between them that the pixel each used to fill.
            let longest = lane.longest_in(i, run_end).unwrap_or(i);
            let mut inst = instance(&events[longest]);
            let x0 = px(start - win.t0);
            let x1 = px(events[run_end - 1].start_ns.max(start) - win.t0) + 1.0;
            inst.x = x0 as f32;
            inst.w = (x1 - x0) as f32;
            out.push(inst);
        }
        i = run_end;
    }
    last
}

/// Lists a lane event by event, for one whose events overlap (an instant
/// inside a scope, an imported trace's marks): there a pixel column's events
/// are not a run of neighbours, and a column cannot be skipped to its end.
/// With `merge`, the narrow events that start in one column still share an
/// instance, the longest of them.
fn list_lane_events(
    lane: &Lane,
    win: &ListWindow,
    y: f32,
    h: f32,
    intern: Option<&InternTable>,
    merge: bool,
    out: &mut Vec<ScopeInstance>,
    unnamed: &mut bool,
) {
    let listed = out.len();
    let radius = (h * 0.14).clamp(2.0, 3.0);
    let mut instance = |e: &LiveEvent| {
        let (inst, named) = instance_named(e, win.t0, win.t1, win.span, win.width, y, h, radius, intern);
        *unnamed |= !named;
        inst
    };
    // The column of the narrow events being gathered, their instance's
    // place in `out` and the span of the longest so far.
    let mut run: Option<(u64, usize, u64)> = None;
    let mut i = lane.first_ending_after(win.t0);
    while let Some(e) = lane.events().get(i) {
        if e.start_ns >= win.t1 {
            break;
        }
        i += 1;
        if e.end_ns() <= win.t0 {
            continue;
        }
        let span = event_span_ns(e);
        if !merge || span as f64 >= win.ns_per_px {
            out.push(instance(e));
            continue;
        }
        let col = ((e.start_ns.max(win.t0) - win.t0) as f64 / win.ns_per_px) as u64;
        match run {
            Some((c, at, longest)) if c == col => {
                // From where the first of them starts to where the one
                // that reaches furthest ends, as the longest of them.
                let joining = instance(e);
                let mut inst = out[at];
                let (x, right) = (inst.x, (inst.x + inst.w).max(joining.x + joining.w));
                if span >= longest {
                    inst = joining;
                    run = Some((c, at, span));
                }
                inst.x = x;
                inst.w = (right - x).max(1.0);
                out[at] = inst;
            }
            _ => {
                run = Some((col, out.len(), span));
                out.push(instance(e));
            }
        }
    }
    sort_instances_longer_on_top(&mut out[listed..]);
}

pub fn instance_for_event(
    e: &LiveEvent,
    t0: u64,
    t1: u64,
    span: f64,
    width: f32,
    y: f32,
    h: f32,
    radius: f32,
    intern: Option<&InternTable>,
) -> ScopeInstance {
    instance_named(e, t0, t1, span, width, y, h, radius, intern).0
}

/// [`instance_for_event`], and whether the table had the event's name
/// ([`LiveEvent::color_and_named`]).
fn instance_named(
    e: &LiveEvent,
    t0: u64,
    t1: u64,
    span: f64,
    width: f32,
    y: f32,
    h: f32,
    radius: f32,
    intern: Option<&InternTable>,
) -> (ScopeInstance, bool) {
    let px = |ns: u64| (ns as f64 / span) * width as f64;
    let (x0, x1) = if e.end_ns() <= t0 {
        // Ended before the window. The listing never asks for such an
        // event; the viewer does, for a selection that was panned out of
        // view. `end - t0` wrapped then, into a bar across the whole row
        // (and a panic in a debug build). Its place is left of the view:
        // where it ended, and from there as wide as it is.
        let x1 = -px(t0 - e.end_ns());
        let x0 = -px(t0.saturating_sub(e.start_ns));
        (x0.min(x1 - 1.0), x1)
    } else {
        (px(e.start_ns.max(t0) - t0), px(e.end_ns().min(t1) - t0))
    };
    let (color, named) = e.color_and_named(intern);
    let instance = ScopeInstance {
        x: x0 as f32,
        y,
        w: (x1 - x0).max(1.0) as f32,
        h,
        color,
        radius,
        name_id: e.name_id,
        start_ns: e.start_ns,
        duration_ns: e.duration_ns,
        pid: e.pid,
        tid: e.tid,
        kind: e.kind,
        depth: e.depth,
        extra: e.extra,
        event_flags: e._pad,
        flags: FLAG_NONE,
    };
    (instance, named)
}

#[cfg(test)]
mod sparse_lod_tests {
    use super::*;
    use crate::TrackIndex;
    use perfr_live_event::LiveEvent;

    fn ev(tid: u32, start: u64, dur: u64, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: dur,
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    /// A window holding only sparse lanes must still choose Instanced when the
    /// scopes in it are wide. The LOD sample is ranked by lifetime event count,
    /// so busy lanes elsewhere crowd the sparse ones out and the sample comes
    /// back empty -- which used to mean "pixel columns", dropping clip labels
    /// however far you zoomed in.
    #[test]
    fn zooming_into_only_sparse_lanes_still_picks_instanced() {
        let mut idx = TrackIndex::default();
        // Busy lanes, all far away from the window under test. More than
        // LOD_SAMPLE_LANES of them so they own the whole sample.
        for tid in 0..(LOD_SAMPLE_LANES as u32 + 4) {
            for i in 0..64u64 {
                idx.insert(ev(tid, i * 10, 5, tid + 1));
            }
        }
        // One sparse lane with a single wide scope, alone in the window.
        idx.insert(ev(9_000, 1_000_000, 10_000, 777));

        let sampled = sample_lod_lanes(&idx, None);
        assert!(
            !sampled.iter().any(|k| k.tid == 9_000),
            "fixture is only meaningful while the sparse lane misses the sample"
        );

        // 100 ns window over 100 px: the scope is far wider than INSTANCE_MIN_PX.
        let lod = choose_lod(&idx, 1_000_000, 1_000_100, 100, INSTANCE_MIN_PX);
        assert_eq!(
            lod,
            TimelineLod::Instanced,
            "wide scope in view must render instanced so its label is drawn"
        );
    }

    /// Hovering must never change the LOD. The sample takes the hovered lane
    /// as a hint and is otherwise ranked by lifetime event count, so a verdict
    /// read off the sample moved as the cursor moved -- instanced flipping to
    /// blit on hover alone, with nothing else changed.
    #[test]
    fn the_hover_hint_never_changes_the_verdict() {
        let mut idx = TrackIndex::default();
        // Busy lanes with sub-pixel scopes, more than the sample holds.
        for tid in 0..(LOD_SAMPLE_LANES as u32 + 6) {
            for i in 0..64u64 {
                idx.insert(ev(tid, i * 100, 20, tid + 1));
            }
        }
        // One quiet lane carrying the only wide scope in the window.
        idx.insert(ev(9_001, 0, 4_000, 999));

        let keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        let base = choose_lod(&idx, 0, 6_400, 100, INSTANCE_MIN_PX);
        assert_eq!(base, TimelineLod::Instanced, "the wide scope is in view");
        for k in keys {
            assert_eq!(
                choose_lod_hint(&idx, 0, 6_400, 100, INSTANCE_MIN_PX, Some(k)),
                base,
                "hovering lane {k:?} changed the LOD"
            );
        }
    }

    /// The wider search must not override a genuine pixel-columns verdict.
    #[test]
    fn dense_narrow_scopes_still_pick_pixel_columns() {
        let mut idx = TrackIndex::default();
        for tid in 0..4u32 {
            for i in 0..256u64 {
                idx.insert(ev(tid, i * 10, 5, tid + 1));
            }
        }
        // 2560 ns over 8 px: every 5 ns scope is far under a pixel.
        let lod = choose_lod(&idx, 0, 2_560, 8, INSTANCE_MIN_PX);
        assert_eq!(lod, TimelineLod::PixelColumns);
    }

    /// Default theverge-style fit (~9 s / 1280 px) turns 1 ns into a 1 px
    /// bar several milliseconds wide. That tick must not paint over or win
    /// the pick against a longer same-lane scope, and duration stays 1 ns.
    #[test]
    fn one_ns_event_does_not_cover_or_steal_longer_same_lane_scope() {
        let t0 = 1_000_000u64;
        let long_dur = 10_000_000u64;
        let mut idx = TrackIndex::default();
        idx.insert(ev(1, t0, long_dur, 1));
        idx.insert(ev(1, t0 + 2_000_000, 1, 2));
        idx.insert(ev(1, t0 + 20_000_000, 1, 3));

        let win_t0 = t0;
        let win_t1 = t0 + 9_000_000_000;
        let width = 1280.0f32;
        let ns_per_px = (win_t1 - win_t0) as f64 / width as f64;
        assert!(
            ns_per_px > 1_000_000.0,
            "fixture is only meaningful while 1 px is milliseconds, got {ns_per_px} ns/px"
        );

        let frame = collect_instances(&idx, win_t0, win_t1, width, 0.0, None);
        let inst_1ns = frame
            .instances
            .iter()
            .find(|i| i.duration_ns == 1 && i.name_id == 2)
            .expect("1 ns event stays 1 ns in the instance");
        let inst_long = frame
            .instances
            .iter()
            .find(|i| i.duration_ns == long_dur)
            .expect("long scope");
        let inst_gap = frame
            .instances
            .iter()
            .find(|i| i.duration_ns == 1 && i.name_id == 3)
            .expect("isolated 1 ns in the gap");

        assert_eq!(inst_1ns.duration_ns, 1);
        assert!(
            inst_1ns.w <= 1.0 + f32::EPSILON,
            "1 ns must stay a 1 px tick, not a merged bar: w={}",
            inst_1ns.w
        );
        assert!(
            inst_long.w > inst_1ns.w,
            "long scope must be wider than the 1 ns tick"
        );

        let long_i = frame
            .instances
            .iter()
            .position(|i| i.duration_ns == long_dur)
            .unwrap();
        let ns_i = frame
            .instances
            .iter()
            .position(|i| i.duration_ns == 1 && i.name_id == 2)
            .unwrap();
        assert!(
            long_i > ns_i,
            "longer scope must draw after the 1 ns tick (z-order)"
        );

        let cx = inst_long.x + inst_long.w * 0.5;
        let cy = inst_long.y + inst_long.h * 0.5;
        let picked = pick_instance_at(&frame.instances, cx, cy).unwrap();
        assert_eq!(frame.instances[picked].duration_ns, long_dur);
        assert_eq!(frame.instances[picked].name_id, 1);

        let ix = inst_1ns.x + inst_1ns.w * 0.5;
        let iy = inst_1ns.y + inst_1ns.h * 0.5;
        let picked_tick = pick_instance_at(&frame.instances, ix, iy).unwrap();
        assert_eq!(
            frame.instances[picked_tick].duration_ns, long_dur,
            "1 px tick overlapping the long scope must not steal the pick"
        );

        let gx = inst_gap.x + inst_gap.w * 0.5;
        let gy = inst_gap.y + inst_gap.h * 0.5;
        let picked_gap = pick_instance_at(&frame.instances, gx, gy).unwrap();
        assert_eq!(
            frame.instances[picked_gap].name_id, 3,
            "isolated 1 ns tick in a gap stays pickable"
        );

        let lane = idx.lanes().next().unwrap().1;
        let col0 = t0 + 2_000_000;
        assert_eq!(
            lane.last_overlapping(col0, col0 + 1).unwrap().name_id,
            1,
            "pixel-column pick must also prefer the longer scope"
        );
    }
}

#[cfg(test)]
mod thread_focus_tests {
    use super::ThreadFocus;

    #[test]
    fn the_target_greys_the_scheduler_but_not_other_processes_thread_tracks() {
        // A capture of pid 7 with nothing selected: every thread track is
        // in colour, the scheduler shows only pid 7's slices in colour.
        let f = ThreadFocus { selected: None, target_pid: Some(7) };
        assert!(f.active_on_track(7, 70) && f.active_on_track(9, 90));
        assert!(f.active_on_scheduler(7, 70) && !f.active_on_scheduler(9, 90));
        // A thread selected: only it, on both.
        let f = ThreadFocus { selected: Some((9, 90)), target_pid: Some(7) };
        assert!(f.active_on_track(9, 90) && !f.active_on_track(7, 70) && !f.active_on_track(9, 91));
        assert!(f.active_on_scheduler(9, 90) && !f.active_on_scheduler(7, 70));
        assert!(f.same_pid(9) && !f.same_pid(7));
        // No target, nothing selected: everything.
        let f = ThreadFocus::default();
        assert!(f.active_on_track(9, 90) && f.active_on_scheduler(9, 90) && f.is_all());
        // The viewer's own rows never grey.
        let f = ThreadFocus { selected: Some((9, 90)), target_pid: Some(7) };
        assert!(f.active_on_track(perfr_live_event::dev::VIEWER_PID, 1));
    }
}

#[cfg(test)]
mod listing_cache_tests {
    use super::*;
    use crate::TrackIndex;

    fn scope(start: u64, dur: u64, tid: u32, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: dur,
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    fn index(lanes: u32, per_lane: u64) -> TrackIndex {
        let mut idx = TrackIndex::default();
        for tid in 1..=lanes {
            for i in 0..per_lane {
                idx.insert(scope(i * 1_000, 600, tid, 1 + (i % 7) as u32));
            }
        }
        idx
    }

    fn layout_of(idx: &TrackIndex) -> Vec<(LaneKey, f32)> {
        let mut keys: Vec<LaneKey> = idx.lanes().map(|(k, _)| k).collect();
        sort_thread_leaves(&mut keys);
        stacked_layout(&keys, 0.0)
    }

    #[test]
    fn unchanged_lanes_come_back_from_the_cache_and_changed_ones_are_walked() {
        let mut idx = index(6, 200);
        let layout = layout_of(&idx);
        let (t0, t1, w) = (0u64, 200_000u64, 2000.0f32);
        let opts = CollectOpts { inline: true, ..CollectOpts::default() };
        let plain = collect_instances_layout_opts(&idx, t0, t1, w, &layout, None, opts);
        let mut cache = ListingCache::default();
        let first = collect_instances_cached(&idx, t0, t1, w, &layout, None, opts, Some(&mut cache));
        assert_eq!(first.lanes_reused, 0, "nothing to reuse on the first frame");
        assert_eq!(first.instances, plain.instances);
        assert_eq!(cache.len(), 6);
        let second = collect_instances_cached(&idx, t0, t1, w, &layout, None, opts, Some(&mut cache));
        assert_eq!(second.lanes_reused, 6, "a still frame reuses every lane");
        assert_eq!(second.instances, plain.instances);
        // One lane receives an event: that lane is walked, the others are not.
        idx.insert(scope(199_500, 100, 3, 9));
        let plain2 = collect_instances_layout_opts(&idx, t0, t1, w, &layout, None, opts);
        let third = collect_instances_cached(&idx, t0, t1, w, &layout, None, opts, Some(&mut cache));
        assert_eq!(third.lanes_reused, 5);
        assert_eq!(third.instances, plain2.instances);
        assert!(third.instances.len() > second.instances.len());
        // A moved window reuses nothing and is still right.
        let moved = collect_instances_cached(&idx, t0 + 1, t1 + 1, w, &layout, None, opts, Some(&mut cache));
        assert_eq!(moved.lanes_reused, 0);
        let plain3 = collect_instances_layout_opts(&idx, t0 + 1, t1 + 1, w, &layout, None, opts);
        assert_eq!(moved.instances, plain3.instances);
        // A lane leaving the layout leaves the cache.
        let fewer: Vec<(LaneKey, f32)> = layout[..4].to_vec();
        let _ = collect_instances_cached(&idx, t0 + 1, t1 + 1, w, &fewer, None, opts, Some(&mut cache));
        assert_eq!(cache.len(), 4);
    }

    #[test]
    fn a_culled_lane_is_not_cached_and_a_visible_one_is() {
        let idx = index(4, 50);
        let layout = layout_of(&idx);
        let cull = YCull::new(0.0, lane_height(layout[0].0) * 0.5);
        let opts = CollectOpts { y_cull: Some(cull), inline: true, ..CollectOpts::default() };
        let mut cache = ListingCache::default();
        let f = collect_instances_cached(&idx, 0, 60_000, 500.0, &layout, None, opts, Some(&mut cache));
        assert_eq!(f.lanes_kept, 1);
        assert_eq!(cache.len(), 1);
        let g = collect_instances_cached(&idx, 0, 60_000, 500.0, &layout, None, opts, Some(&mut cache));
        assert_eq!(g.lanes_reused, 1);
        assert_eq!(g.instances, f.instances);
    }

    /// Frames of a session in any order of events: scopes arriving (in
    /// order, late, on new lanes), scopes dropped, the window, the width
    /// and the layout changing or not. Each frame's cached listing must be
    /// the plain one, and the pooled walk the inline one; the raster of the
    /// same frame must be each lane's own.
    #[test]
    fn the_cached_and_the_pooled_listing_are_the_plain_one_frame_after_frame() {
        use crate::test_rng::Rng;
        let mut rng = Rng(0x5EED_5EED_5EED_5EED);
        let mut idx = index(12, 40);
        let mut cache = ListingCache::default();
        let (mut t0, mut t1, mut w) = (0u64, 40_000u64, 800.0f32);
        let mut cull = None;
        let mut drop_lane = usize::MAX;
        let mut reused = 0;
        for frame in 0..400 {
            match rng.below(10) {
                0..=2 => {
                    for _ in 0..rng.below(20) {
                        let tid = 1 + rng.below(14) as u32;
                        let start = rng.below(60_000);
                        let dur = [1, 1, 30, 600, 5_000][rng.below(5) as usize];
                        idx.insert(LiveEvent { depth: rng.below(2) as u8, ..scope(start, dur, tid, 1 + rng.below(9) as u32) });
                    }
                }
                3 => idx.retain(|e| e.start_ns % 7 != frame % 7),
                4 => {
                    t0 = rng.below(50_000);
                    t1 = t0 + 1 + rng.below(60_000);
                }
                5 => w = [1.0, 64.0, 800.0, 1917.5][rng.below(4) as usize],
                6 => cull = (rng.below(2) == 0).then(|| YCull::new(rng.below(200) as f32, 60.0 + rng.below(300) as f32)),
                7 => drop_lane = rng.below(16) as usize,
                _ => {}
            }
            let mut layout = layout_of(&idx);
            if drop_lane < layout.len() {
                layout.remove(drop_lane);
            }
            let opts = CollectOpts { y_cull: cull, inline: true, ..CollectOpts::default() };
            let plain = collect_instances_layout_opts(&idx, t0, t1, w, &layout, None, opts);
            let cached = collect_instances_cached(&idx, t0, t1, w, &layout, None, opts, Some(&mut cache));
            assert_eq!(cached.instances, plain.instances, "frame {frame}: cached");
            assert_eq!(cached.lanes_kept, plain.lanes_kept, "frame {frame}");
            reused += cached.lanes_reused;
            let pooled = collect_instances_layout_opts(
                &idx,
                t0,
                t1,
                w,
                &layout,
                None,
                CollectOpts { inline: false, per_lane_spans: frame % 2 == 0, ..opts },
            );
            assert_eq!(pooled.instances, plain.instances, "frame {frame}: pooled");
            // Every instance is an event of its lane, drawn inside the view.
            for i in &plain.instances {
                assert!(i.x >= 0.0 && i.x <= w && i.w >= 1.0, "frame {frame}: {i:?}");
                let lane = idx.lane(ScopePick::from_instance(i).lane_key()).expect("lane");
                assert!(lane.events().iter().any(|e| e.start_ns == i.start_ns && e.name_id == i.name_id));
            }
            let width = w as usize;
            let keys: Vec<LaneKey> = layout.iter().map(|(k, _)| *k).collect();
            let raster = idx.rasterize_pixel_layout(t0, t1, width, &keys, Some(&layout), cull, None, frame % 2 == 0);
            for (row, key) in raster.lanes.iter().enumerate() {
                let mut own = vec![0u32; width];
                idx.lane(*key).expect("lane").rasterize(t0, t1, width, &mut own, None);
                assert_eq!(raster.row(row), own, "frame {frame}: raster row of {key:?}");
            }
        }
        assert!(reused > 0, "the sequence never exercised a reused row");
    }

    fn one_lane(events: impl IntoIterator<Item = LiveEvent>) -> (TrackIndex, Vec<(LaneKey, f32)>) {
        let mut idx = TrackIndex::default();
        idx.extend(events);
        let layout = layout_of(&idx);
        (idx, layout)
    }

    const INLINE: CollectOpts =
        CollectOpts { y_cull: None, early_out: true, inline: true, per_lane_spans: false, merge_subpixel: true };

    /// 1000 ns to the pixel: 400 px over 400 us.
    const PX: u64 = 1_000;

    #[test]
    fn events_under_a_pixel_that_start_in_one_column_are_one_instance() {
        // Column 3 holds forty 20 ns scopes and one of 300 ns, the longest;
        // a 5 px scope starts in the same column after them and runs on.
        let mut events: Vec<LiveEvent> = (0..40).map(|i| scope(3 * PX + i * 15, 10, 1, 100 + i as u32)).collect();
        events.push(scope(3 * PX + 610, 300, 1, 7));
        events.push(scope(3 * PX + 950, 5 * PX, 1, 8));
        // Alone in its column: itself, a pixel wide.
        events.push(scope(20 * PX + 100, 10, 1, 9));
        let (idx, layout) = one_lane(events);
        let frame = collect_instances_layout_opts(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE);
        let names: Vec<u32> = frame.instances.iter().map(|i| i.name_id).collect();
        assert_eq!(names, [7, 8, 9], "the run as its longest event, then the wide scope, then the lone one");
        let run = frame.instances[0];
        assert_eq!((run.start_ns, run.duration_ns), (3 * PX + 610, 300), "the run is picked as its longest event");
        assert!(
            (run.x - 3.0).abs() < 1e-3 && (run.w - 1.61).abs() < 1e-3,
            "from the first start to a pixel past the last, as the events drawn one by one cover: {run:?}"
        );
        assert!((frame.instances[1].w - 5.0).abs() < 1e-3, "a scope a pixel wide or more is never merged");
        assert!((frame.instances[2].x - 20.1).abs() < 1e-3 && frame.instances[2].w == 1.0);

        let each = collect_instances_layout_opts(
            &idx,
            0,
            400 * PX,
            400.0,
            &layout,
            None,
            CollectOpts { merge_subpixel: false, ..INLINE },
        );
        assert_eq!(each.instances.len(), 43, "with merging off every event is an instance");
    }

    #[test]
    fn a_dense_lane_lists_no_more_than_a_few_instances_per_column() {
        // A hundred events to the pixel, across the whole window.
        let (idx, layout) = one_lane((0..40_000u64).map(|i| scope(i * 10, 6, 1, 1 + (i % 5) as u32)));
        let frame = collect_instances_layout_opts(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE);
        assert_eq!(frame.instances.len(), 400, "one per column, where it was a hundred");
        for (column, inst) in frame.instances.iter().enumerate() {
            assert!((inst.x - column as f32).abs() < 0.02 && (inst.w - 1.99).abs() < 0.02, "{inst:?}");
        }
    }

    #[test]
    fn narrow_events_of_a_lane_with_overlap_share_their_columns_instance_too() {
        // A long scope with marks inside it: the ends are out of order.
        let mut events = vec![scope(0, 100 * PX, 1, 1)];
        events.extend((0..30).map(|i| scope(50 * PX + i * 20, 5, 1, 2)));
        events.push(scope(50 * PX + 700, 90, 1, 3));
        let (idx, layout) = one_lane(events);
        assert!(!idx.lanes().next().expect("the lane").1.ends_sorted());
        let frame = collect_instances_layout_opts(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE);
        let names: Vec<u32> = frame.instances.iter().map(|i| i.name_id).collect();
        assert_eq!(names, [3, 1], "the marks as the longest of them, under the scope they are inside");
    }

    /// What a live capture does to a lane: events arrive at its end, a few
    /// a frame, most of them far under a pixel.
    fn arrivals(rng: &mut crate::test_rng::Rng, tid: u32, from: u64, until: u64) -> Vec<LiveEvent> {
        let mut out = Vec::new();
        let mut t = from;
        while t < until {
            let dur = [3, 3, 3, 40, 40, 700, 2_500, 9_000][rng.below(8) as usize];
            out.push(scope(t, dur, tid, 1 + rng.below(9) as u32));
            t += dur + rng.below(60);
        }
        out
    }

    fn assert_same_listing(got: &[ScopeInstance], want: &[ScopeInstance], from_x: f32, what: &str) {
        let (got, want): (Vec<_>, Vec<_>) =
            (got.iter().filter(|i| i.x >= from_x).collect(), want.iter().filter(|i| i.x >= from_x).collect());
        assert_eq!(got.len(), want.len(), "{what}: count");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(ScopePick::from_instance(g), ScopePick::from_instance(w), "{what}");
            assert!((g.x - w.x).abs() < 0.01 && (g.w - w.w).abs() < 0.01 && g.y == w.y && g.color == w.color, "{what}: {g:?} / {w:?}");
        }
    }

    #[test]
    fn a_lane_that_only_gained_events_keeps_its_row_and_walks_what_is_new() {
        use crate::test_rng::Rng;
        let mut rng = Rng(0x0A77_E7D5);
        let mut idx = TrackIndex::default();
        let mut cache = ListingCache::default();
        let mut ends = [0u64; 3];
        let mut extended = 0;
        for frame in 0..120u64 {
            for (lane, end) in ends.iter_mut().enumerate() {
                // The third lane goes quiet now and then: reused whole.
                if lane == 2 && frame % 4 != 0 {
                    continue;
                }
                let until = *end + 1 + rng.below(4 * PX);
                let batch = arrivals(&mut rng, 1 + lane as u32, *end, until);
                *end = batch.last().map(|e| e.end_ns()).unwrap_or(*end);
                idx.extend(batch);
            }
            let layout = layout_of(&idx);
            let plain = collect_instances_layout_opts(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE);
            let cached = collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
            assert_eq!(cached.instances, plain.instances, "frame {frame}");
            if frame > 0 {
                assert_eq!(cached.lanes_extended + cached.lanes_reused, 3, "frame {frame}: no lane is listed from its start");
            }
            extended += cached.lanes_extended;
        }
        assert!(extended > 200, "the lanes that gained events were extended: {extended}");
    }

    #[test]
    fn a_window_moved_on_by_whole_columns_moves_its_rows_with_it() {
        use crate::test_rng::Rng;
        let mut rng = Rng(0x51DE_51DE);
        let mut idx = TrackIndex::default();
        let mut cache = ListingCache::default();
        let mut ends = [0u64; 4];
        let (mut t0, span) = (0u64, 400 * PX);
        let mut slid = 0;
        for frame in 0..400u64 {
            // The live edge moves on about three columns a frame.
            let edge = (frame + 1) * 3 * PX;
            for (lane, end) in ends.iter_mut().enumerate() {
                let batch = arrivals(&mut rng, 1 + lane as u32, *end, edge);
                *end = batch.last().map(|e| e.end_ns()).unwrap_or(*end);
                idx.extend(batch);
            }
            // And the window after it, a whole number of columns at a time,
            // once the edge has gone three quarters of the way across.
            let moved = edge > t0 + span * 3 / 4;
            if moved {
                t0 += (1 + rng.below(90)) * PX;
            }
            let layout = layout_of(&idx);
            let plain = collect_instances_layout_opts(&idx, t0, t0 + span, 400.0, &layout, None, INLINE);
            let cached = collect_instances_cached(&idx, t0, t0 + span, 400.0, &layout, None, INLINE, Some(&mut cache));
            // The window's first columns hold what was cut by its start,
            // which a row that was moved keeps as it was listed.
            assert_same_listing(&cached.instances, &plain.instances, 2.0, &format!("frame {frame}"));
            if moved && frame > 0 {
                assert_eq!(cached.lanes_extended, 4, "frame {frame}: every row moved with the window");
                slid += 1;
            }
        }
        assert!(slid > 10, "the window moved {slid} times");
    }

    #[test]
    fn a_window_that_moved_by_part_of_a_column_or_back_is_listed_again() {
        let (idx, layout) = one_lane((0..4_000u64).map(|i| scope(i * 100, 60, 1, 1)));
        let mut cache = ListingCache::default();
        let mut list = |t0: u64| collect_instances_cached(&idx, t0, t0 + 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
        list(10 * PX);
        assert_eq!(list(20 * PX).lanes_extended, 1, "ten columns on");
        assert_eq!(list(20 * PX + PX / 2).lanes_extended, 0, "half a column on: the columns are other columns");
        assert_eq!(list(15 * PX).lanes_extended, 0, "back: what is before the row was never listed");
    }

    #[test]
    fn a_row_that_only_moved_up_or_down_is_the_same_row_at_its_new_height() {
        let mut idx = TrackIndex::default();
        idx.extend((0..500u64).map(|i| scope(i * 700, 400, 5, 1)));
        let mut cache = ListingCache::default();
        let layout = layout_of(&idx);
        collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
        // A lane appears above it: a thread seen for the first time.
        idx.insert(scope(0, 50 * PX, 2, 9));
        let layout = layout_of(&idx);
        let moved = collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
        let plain = collect_instances_layout_opts(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE);
        assert_eq!(moved.lanes_reused, 1, "the lane below was not walked again");
        assert_eq!(moved.instances, plain.instances);
    }

    #[test]
    fn a_name_that_arrives_relists_only_the_rows_that_were_waiting_for_one() {
        // Two lanes. Lane 1's scopes are named 5, which the table has; lane
        // 2's are named 6, which it does not have yet.
        let mut idx = TrackIndex::default();
        idx.extend((0..50u64).map(|i| scope(i * 5 * PX, 2 * PX, 1, 5)));
        idx.extend((0..50u64).map(|i| scope(i * 5 * PX, 2 * PX, 2, 6)));
        let layout = layout_of(&idx);
        let mut intern = InternTable::default();
        intern.insert_id(5, "update");
        let mut cache = ListingCache::default();
        let mut list = |intern: &InternTable| {
            collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, Some(intern), INLINE, Some(&mut cache))
        };
        let first = list(&intern);
        // A name nothing listed uses: new threads' and functions' names
        // arrive all through a capture. Lane 1 has nothing to wait for.
        intern.insert_id(77, "elsewhere");
        let other = list(&intern);
        assert_eq!(other.lanes_reused, 1, "the row whose names were known stands");
        assert_eq!(other.instances, first.instances);
        // The name lane 2 was drawn without: its colour changes.
        intern.insert_id(6, "render");
        let named = list(&intern);
        assert_eq!(named.lanes_reused, 1, "lane 1 again");
        let colour_of = |f: &InstanceFrame, name: u32| f.instances.iter().find(|i| i.name_id == name).map(|i| i.color);
        assert_eq!(colour_of(&named, 5), colour_of(&first, 5));
        assert_ne!(colour_of(&named, 6), colour_of(&first, 6), "lane 2 was listed again, in its name's colour");
        // Nothing is waiting any more: another new name moves no row.
        intern.insert_id(78, "elsewhere too");
        assert_eq!(list(&intern).lanes_reused, 2);
        // A known id renamed changes its colour under every row.
        intern.insert_id(5, "update_all");
        let renamed = list(&intern);
        assert_eq!(renamed.lanes_reused, 0);
        assert_ne!(colour_of(&renamed, 5), colour_of(&first, 5));
        // And another table is another table, whatever it holds.
        let mut again = InternTable::default();
        again.insert_id(5, "update_all");
        again.insert_id(6, "render");
        assert_eq!(list(&again).lanes_reused, 0);
    }

    #[test]
    fn a_lane_made_again_under_its_key_is_not_the_lane_a_row_was_listed_from() {
        let mut idx = TrackIndex::default();
        idx.extend((0..10u64).map(|i| scope(i * 5 * PX, 2 * PX, 1, 1)));
        let mut cache = ListingCache::default();
        let layout = layout_of(&idx);
        collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
        // Gone and back with as many events, so the same version, elsewhere.
        idx.retain(|_| false);
        idx.extend((0..10u64).map(|i| scope(PX + i * 7 * PX, 3 * PX, 1, 2)));
        let again = collect_instances_cached(&idx, 0, 400 * PX, 400.0, &layout, None, INLINE, Some(&mut cache));
        assert_eq!((again.lanes_reused, again.lanes_extended), (0, 0));
        assert!(again.instances.iter().all(|i| i.name_id == 2), "the new lane's events, not the cached row");
    }

    /// `cargo test --release -p perfr-live-render listing_cache_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn listing_cache_bench() {
        let lanes = 200u32;
        let per_lane = 5_000u64;
        let mut idx = index(lanes, per_lane);
        let layout = layout_of(&idx);
        // Two windows: the tail of the capture at a zoom where scopes are
        // instanced (a few px each), and the whole capture, which is what
        // a zoomed-out still view lists (the viewer would pick the column
        // LOD there; this is the listing's worst case).
        for (label, visible) in [("500 events/lane in view", 500u64), ("every event in view", per_lane)] {
            let (t0, t1, w) = ((per_lane - visible) * 1_000, per_lane * 1_000 + 60_000, 2000.0f32);
            let rounds = 100u64;
            let opts = CollectOpts::default();
            let mut cache = ListingCache::default();
            let mut next = per_lane * 1_000;
            let (mut plain_ns, mut cached_ns) = (0u128, 0u128);
            let (mut plain_walk, mut cached_walk) = (0u64, 0u64);
            let mut reused = 0u64;
            let mut prims = 0usize;
            for _ in 0..rounds {
                // Two hot lanes receive 50 events each between frames.
                for i in 0..50 {
                    idx.insert(scope(next + i * 10, 5, 1, 3));
                    idx.insert(scope(next + i * 10, 5, 2, 4));
                }
                next += 500;
                let a = std::time::Instant::now();
                let p = collect_instances_layout_opts(&idx, t0, t1, w, &layout, None, opts);
                plain_ns += a.elapsed().as_nanos();
                plain_walk += p.timing.dispatch_t1_ns - p.timing.dispatch_t0_ns;
                let b = std::time::Instant::now();
                let c = collect_instances_cached(&idx, t0, t1, w, &layout, None, opts, Some(&mut cache));
                cached_ns += b.elapsed().as_nanos();
                cached_walk += c.timing.dispatch_t1_ns - c.timing.dispatch_t0_ns;
                assert_eq!(p.instances.len(), c.instances.len());
                reused += c.lanes_reused as u64;
                prims = c.instances.len();
            }
            println!(
                "listing_cache_bench [{label}]: {lanes} lanes x {per_lane} events, 2 hot lanes, {prims} instances per frame, {rounds} frames\n  plain  {:.3} ms/frame (walk {:.3} ms)\n  cached {:.3} ms/frame (walk {:.3} ms, {} lanes reused per frame)",
                plain_ns as f64 / rounds as f64 / 1e6,
                plain_walk as f64 / rounds as f64 / 1e6,
                cached_ns as f64 / rounds as f64 / 1e6,
                cached_walk as f64 / rounds as f64 / 1e6,
                reused / rounds
            );
        }
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    /// The viewer builds the instance of the selected scope for whatever
    /// window is on screen, also after the scope has been panned out of it.
    #[test]
    fn an_event_outside_the_window_gets_an_instance_outside_the_view() {
        let event = LiveEvent { start_ns: 100, duration_ns: 50, pid: 1, tid: 2,
            kind: kind::API_SCOPE, depth: 0, extra: 0, _pad: 0, name_id: 7 };
        let width = 800.0;
        let at = |t0: u64, t1: u64| instance_for_event(&event, t0, t1, (t1 - t0) as f64, width, 0.0, 16.0, 2.0, None);
        // Entirely left of the window: nothing of it may reach x = 0.
        for (t0, t1) in [(150, 1_150), (151, 1_151), (10_000, 11_000), (u64::MAX - 10, u64::MAX)] {
            let i = at(t0, t1);
            assert!(i.x.is_finite() && i.w >= 1.0, "[{t0}, {t1}): {i:?}");
            assert!(i.x + i.w <= 0.0, "[{t0}, {t1}): drawn at {}..{}", i.x, i.x + i.w);
        }
        // Entirely right of it: nothing left of the right edge.
        let i = at(0, 100);
        assert!(i.x >= width && i.w >= 1.0, "{i:?}");
        // Inside or across an edge, as before: clamped to the view.
        let i = at(0, 1_000);
        assert_eq!((i.x, i.w), (80.0, 40.0));
        let i = at(120, 1_120);
        assert_eq!((i.x, i.w), (0.0, 24.0));
    }

    #[test]
    fn dynamic_origin_survives_both_scope_picking_paths() {
        let event = LiveEvent { start_ns: 10, duration_ns: 20, pid: 1, tid: 2,
            kind: kind::API_SCOPE, depth: 3, extra: 0,
            _pad: perfr_live_event::event_flags::DYNAMIC, name_id: 7 };
        let pick = ScopePick::from_event(event);
        let instance = instance_for_event(&event, 0, 100, 100.0, 100.0, 0.0, 16.0, 2.0, None);
        assert_eq!(ScopePick::from_instance(&instance), pick);
        assert_eq!(pick.event_flags, perfr_live_event::event_flags::DYNAMIC);
        assert_eq!(pick.lane_key(), event.lane_key());
    }
}
