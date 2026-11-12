// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The log window.
//!
//! Lines are events on the same ring as scopes. The window shows whichever
//! of them fall in the visible time range, every thread interleaved, or one
//! thread when the filter says so. Clicking a scope restricts the window to
//! lines inside that scope's interval on that thread; a name that recurs
//! (one scope per frame) is grouped so the same line is one row with a count.
//! A scope is the only grouping there is: a line carries no category.
//!
//! What a frame costs here does not grow with the capture. The rows are
//! computed when the question changes (the window moved, a scope was
//! clicked, events arrived) and kept until it changes again; only the rows
//! scrolled into view are laid out; and the tick strip draws at most one
//! mark per pixel column of each lane on screen.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use eframe::egui::{self, Color32, FontId, RichText, ScrollArea, Sense, Ui};
use perfr_live_event::logs::{aggregate, level_name, LogGroup, ScopeSpan, SpanSet};
use perfr_live_event::{kind, log_level_color, InternTable, LaneKey, LiveEvent, DROP_MARK};
use perfr_live_render::{lane_height, Lane, TrackIndex, YCull};

use crate::theme;

/// The most lines the window lists at once. A zoomed-out view of a busy
/// capture can hold millions; nobody reads that many, and merging them every
/// time the view pans is the whole frame. The header says when the list was
/// cut, and zooming in brings the rest back.
const MAX_LINES: usize = 20_000;

/// While a capture is streaming, a list that only changed because events
/// arrived is refreshed this often rather than every frame.
const LIVE_REFRESH_S: f64 = 0.25;

/// How many lines one pixel column of the tick strip looks at to pick its
/// colour. A column holding more shows the loudest of the first few.
const TICK_SAMPLE: usize = 16;

const LINE_FONT: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeFocus {
    pub pid: u32,
    pub tid: u32,
    pub name_id: u32,
}

#[derive(Default)]
pub struct LogUi {
    pub open: bool,
    pub thread_filter: Option<u32>,
    pub scope: Option<ScopeFocus>,
    pub highlight_ns: Option<u64>,
    /// Whether the window has been opened for this capture's first log line.
    /// Once is enough: after that, open and shut is the user's call.
    auto_opened: bool,
    /// The lane set `auto_opened` was last checked against.
    lanes_checked: Option<u64>,
    cache: Option<Cache>,
}

impl LogUi {
    /// Forgets everything about the capture that was on screen.
    pub fn reset_capture(&mut self) {
        self.scope = None;
        self.highlight_ns = None;
        self.thread_filter = None;
        self.auto_opened = false;
        self.lanes_checked = None;
        self.cache = None;
    }

    /// Opens the window the first time the capture turns out to have log
    /// lines. A capture without any never shows it unasked.
    pub fn open_on_first_log(&mut self, index: &TrackIndex) {
        if self.auto_opened || self.lanes_checked == Some(index.lane_gen()) {
            return;
        }
        // The set of lanes changes rarely, so this walk is not per frame.
        self.lanes_checked = Some(index.lane_gen());
        if index.lanes().any(|(k, _)| k.kind == kind::LOG) {
            self.auto_opened = true;
            self.open = true;
        }
    }
}

pub enum LogAction {
    /// Center the timeline on this timestamp and draw the highlight.
    Focus(u64),
}

/// What the list was computed for. Equal keys mean the same rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Query {
    /// Zero while a scope is focused: its lines do not depend on the view.
    t0: u64,
    t1: u64,
    thread: Option<u32>,
    scope: Option<ScopeFocus>,
}

enum Rows {
    Lines(Vec<LiveEvent>),
    Groups(Vec<LogGroup>),
}

struct Cache {
    query: Query,
    /// The index the rows were computed from: its event count and lane set.
    stamp: (usize, u64),
    computed_at: f64,
    rows: Rows,
    /// Lines the query matched, which is more than `rows` holds past
    /// [`MAX_LINES`].
    matched: usize,
    /// Instances of the focused scope, when one is.
    instances: usize,
    threads: Vec<u32>,
    dropped: u64,
}

fn color32(argb: u32) -> Color32 {
    Color32::from_rgb((argb >> 16) as u8, (argb >> 8) as u8, argb as u8)
}

fn level_color(level: u8) -> Color32 {
    color32(log_level_color(level))
}

fn message(intern: &InternTable, id: u32) -> &str {
    intern.get(id).unwrap_or("")
}

/// The slice of `lane` whose events start inside `[t0, t1]`.
fn starts_in(lane: &Lane, t0: u64, t1: u64) -> &[LiveEvent] {
    let ev = lane.events();
    let begin = ev.partition_point(|e| e.start_ns < t0);
    let end = ev.partition_point(|e| e.start_ns <= t1);
    &ev[begin..end.max(begin)]
}

fn log_lanes(index: &TrackIndex, tid: Option<u32>) -> impl Iterator<Item = (LaneKey, &Lane)> {
    index
        .lanes()
        .filter(move |(k, _)| k.kind == kind::LOG && tid.is_none_or(|t| k.tid == t))
}

/// The first `limit` lines of `slices` in timestamp order, thread id breaking
/// ties, and how many lines there are in all. Each slice is one thread's
/// lane, already in time order, so this is a merge and never a sort of the
/// whole window.
fn merge_lines(slices: &[&[LiveEvent]], limit: usize) -> (Vec<LiveEvent>, usize) {
    let total: usize = slices.iter().map(|s| s.len()).sum();
    let mut out = Vec::with_capacity(total.min(limit));
    // (timestamp, tid, slice, position): the slice index keeps one thread's
    // lines at one timestamp in the order they were written.
    let mut heads: BinaryHeap<Reverse<(u64, u32, usize, usize)>> = slices
        .iter()
        .enumerate()
        .filter_map(|(s, slice)| slice.first().map(|e| Reverse((e.start_ns, e.tid, s, 0))))
        .collect();
    while out.len() < limit {
        let Some(Reverse((_, _, s, i))) = heads.pop() else { break };
        out.push(slices[s][i]);
        if let Some(next) = slices[s].get(i + 1) {
            heads.push(Reverse((next.start_ns, next.tid, s, i + 1)));
        }
    }
    (out, total)
}

fn thread_ids(index: &TrackIndex) -> Vec<u32> {
    let mut tids: Vec<u32> = log_lanes(index, None).map(|(k, _)| k.tid).collect();
    tids.sort_unstable();
    tids.dedup();
    tids
}

fn drop_total(index: &TrackIndex) -> u64 {
    index
        .lanes()
        .filter(|(k, _)| k.kind == kind::DROP)
        .flat_map(|(_, lane)| lane.events().iter())
        .filter_map(|e| e.drop_count())
        .fold(0u64, u64::saturating_add)
}

/// Every instance of the focused scope on its thread.
fn scope_instances(index: &TrackIndex, focus: ScopeFocus) -> Vec<ScopeSpan> {
    index
        .lanes()
        .filter(|(k, _)| (k.kind == kind::API_SCOPE || k.kind == kind::API_TRACK) && k.pid == focus.pid && k.tid == focus.tid)
        .flat_map(|(_, lane)| lane.events().iter())
        .filter(|e| e.name_id == focus.name_id)
        .filter_map(ScopeSpan::from_event)
        .collect()
}

fn compute(index: &TrackIndex, query: Query, now: f64) -> Cache {
    let (rows, matched, instances) = match query.scope {
        Some(focus) => {
            let spans = scope_instances(index, focus);
            let inside = SpanSet::new(&spans);
            let slices: Vec<Vec<LiveEvent>> = log_lanes(index, Some(focus.tid))
                .filter(|(k, _)| k.pid == focus.pid)
                .map(|(_, lane)| lane.events().iter().copied().filter(|e| inside.contains_log(e)).collect())
                .collect();
            let borrowed: Vec<&[LiveEvent]> = slices.iter().map(|s| s.as_slice()).collect();
            if spans.len() > 1 {
                // Grouping needs every line, but its output is one row per
                // distinct line, which is what bounds the list.
                let (lines, matched) = merge_lines(&borrowed, usize::MAX);
                (Rows::Groups(aggregate(&lines)), matched, spans.len())
            } else {
                let (lines, matched) = merge_lines(&borrowed, MAX_LINES);
                (Rows::Lines(lines), matched, spans.len())
            }
        }
        None => {
            let slices: Vec<&[LiveEvent]> = log_lanes(index, query.thread)
                .map(|(_, lane)| starts_in(lane, query.t0, query.t1))
                .collect();
            let (lines, matched) = merge_lines(&slices, MAX_LINES);
            (Rows::Lines(lines), matched, 0)
        }
    };
    Cache {
        query,
        stamp: (index.event_count(), index.lane_gen()),
        computed_at: now,
        rows,
        matched,
        instances,
        threads: thread_ids(index),
        dropped: drop_total(index),
    }
}

/// The rows for `query`, recomputed only when they can have changed.
fn rows_for<'a>(state: &'a mut LogUi, ui: &Ui, index: &TrackIndex, query: Query) -> &'a Cache {
    let now = ui.input(|i| i.time);
    let stamp = (index.event_count(), index.lane_gen());
    let fresh = match &state.cache {
        Some(cache) if cache.query == query => {
            if cache.stamp == stamp {
                true
            } else if now - cache.computed_at < LIVE_REFRESH_S {
                // Events are still arriving. The same question, asked again
                // a moment later, can wait: come back when the list is due.
                ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(LIVE_REFRESH_S));
                true
            } else {
                false
            }
        }
        _ => false,
    };
    if !fresh {
        state.cache = Some(compute(index, query, now));
    }
    state.cache.as_ref().expect("just filled")
}

/// Draws the log window. Returns a request to move the timeline.
pub fn show(
    ui: &mut Ui,
    state: &mut LogUi,
    index: &TrackIndex,
    intern: &InternTable,
    t0: f64,
    t1: f64,
) -> Option<LogAction> {
    let query = match state.scope {
        Some(focus) => Query { t0: 0, t1: 0, thread: None, scope: Some(focus) },
        None => {
            let t0 = t0.max(0.0) as u64;
            Query { t0, t1: (t1.max(0.0) as u64).max(t0), thread: state.thread_filter, scope: None }
        }
    };
    let highlight = state.highlight_ns;
    let (mut thread_filter, mut clear_scope, mut hide) = (state.thread_filter, false, false);
    let cache = rows_for(state, ui, index, query);

    ui.horizontal(|ui| {
        ui.label(RichText::new("LOGS").size(11.0).color(theme::MUTED()));
        if query.scope.is_none() {
            let label = |tid: Option<u32>| tid.map_or("all threads".to_string(), |t| format!("thread {t}"));
            egui::ComboBox::from_id_salt("perfr_log_thread")
                .selected_text(label(thread_filter))
                .width(120.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut thread_filter, None, label(None));
                    for tid in &cache.threads {
                        ui.selectable_value(&mut thread_filter, Some(*tid), label(Some(*tid)));
                    }
                });
        } else if ui.button("clear scope").on_hover_text("Back to the lines in the visible time range").clicked() {
            clear_scope = true;
        }
        if cache.dropped > 0 {
            ui.label(RichText::new(format!("{} events dropped", cache.dropped)).color(color32(DROP_MARK)).size(12.0));
        }
        if ui.button("hide").clicked() {
            hide = true;
        }
    });

    let listed = match &cache.rows {
        Rows::Lines(lines) => lines.len(),
        Rows::Groups(groups) => groups.len(),
    };
    if let Some(focus) = query.scope {
        let name = message(intern, focus.name_id);
        let mut header = format!(
            "{}  thread {}  {} instance{}",
            if name.is_empty() { "(unnamed scope)" } else { name },
            focus.tid,
            cache.instances,
            if cache.instances == 1 { "" } else { "s" }
        );
        if matches!(cache.rows, Rows::Groups(_)) {
            header.push_str("  ·  the same line across instances is one row, with how many times it occurred");
        }
        ui.label(RichText::new(header).size(11.0).color(theme::TEXT()));
    }
    if matches!(cache.rows, Rows::Lines(_)) && cache.matched > listed {
        ui.label(
            RichText::new(format!("first {listed} of {} lines; zoom in to see the rest", cache.matched))
                .size(11.0)
                .color(theme::MUTED()),
        );
    }

    let action = if listed == 0 {
        let why = if query.scope.is_some() {
            "No log lines inside this scope on this thread."
        } else {
            "No log lines in this time range."
        };
        ui.label(RichText::new(why).size(12.0).color(theme::MUTED()));
        None
    } else {
        let show_thread = query.scope.is_none() && query.thread.is_none();
        match &cache.rows {
            Rows::Lines(lines) => show_rows(ui, lines.len(), |ui, row| {
                let log = &lines[row];
                let level = log.log_level().unwrap_or(0);
                let thread = if show_thread { format!("{:>8}  ", log.tid) } else { String::new() };
                line_row(
                    ui,
                    format!("{:5}  {thread}", level_name(level).to_ascii_uppercase()),
                    message(intern, log.name_id),
                    log.log_truncated(),
                    level,
                    highlight == Some(log.start_ns),
                    "Show this time on the timeline",
                )
                .then_some(log.start_ns)
            }),
            Rows::Groups(groups) => show_rows(ui, groups.len(), |ui, row| {
                let group = &groups[row];
                line_row(
                    ui,
                    format!("{:5}  {:>6}×  ", level_name(group.level).to_ascii_uppercase(), group.count),
                    message(intern, group.name_id),
                    group.truncated,
                    group.level,
                    highlight == Some(group.first_ns),
                    "Jump to the first time this line occurred",
                )
                .then_some(group.first_ns)
            }),
        }
        .map(LogAction::Focus)
    };

    state.thread_filter = thread_filter;
    if clear_scope {
        state.scope = None;
    }
    if hide {
        state.open = false;
    }
    action
}

/// Lays out only the rows scrolled into view. `row` draws one and returns
/// the timestamp to jump to when it was clicked.
fn show_rows(ui: &mut Ui, count: usize, mut row: impl FnMut(&mut Ui, usize) -> Option<u64>) -> Option<u64> {
    let height = ui.fonts(|f| f.row_height(&FontId::monospace(LINE_FONT)));
    let mut clicked = None;
    ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, height, count, |ui, range| {
        for index in range {
            if let Some(ns) = row(ui, index) {
                clicked = Some(ns);
            }
        }
    });
    clicked
}

/// One row: a fixed prefix, then the line's text. Exactly one row high
/// whatever the text holds, which is what lets the list skip the rows that
/// are not on screen. The whole text is in the tooltip. Returns whether the
/// row was clicked.
fn line_row(ui: &mut Ui, prefix: String, text: &str, truncated: bool, level: u8, selected: bool, hint: &str) -> bool {
    // A line break inside a line would make the row two rows.
    let first = text.lines().next().unwrap_or("");
    let more = if first.len() < text.len() { " ⏎" } else { "" };
    let mark = if truncated { "  [truncated]" } else { "" };
    let resp = ui.add(
        egui::Label::new(
            RichText::new(format!("{prefix}{first}{more}{mark}"))
                .font(FontId::monospace(LINE_FONT))
                .color(if selected { theme::ACCENT() } else { level_color(level) }),
        )
        .truncate()
        .selectable(false)
        .sense(Sense::click()),
    );
    let clicked = resp.clicked();
    resp.on_hover_ui(|ui| {
        ui.label(RichText::new(text).font(FontId::monospace(LINE_FONT)));
        ui.label(RichText::new(hint).size(10.0).color(theme::MUTED()));
    });
    clicked
}

/// Marks log lines on each thread's log lane: at most one tick per pixel
/// column, in the colour of the loudest line that column holds, and only for
/// the lanes scrolled into view.
pub fn paint_ticks(
    ui: &Ui,
    body: egui::Rect,
    t0: u64,
    t1: u64,
    index: &TrackIndex,
    layout: &[(LaneKey, f32)],
    scale: f32,
    y_cull: Option<YCull>,
) {
    if t1 <= t0 || body.width() < 1.0 {
        return;
    }
    let ns_per_pt = (t1 - t0) as f64 / body.width() as f64;
    let painter = ui.painter_at(body);
    for (key, y) in layout {
        if key.kind != kind::LOG {
            continue;
        }
        let h = lane_height(*key) * scale.max(0.01);
        if y_cull.is_some_and(|c| !c.hits(*y, h)) {
            continue;
        }
        let Some(lane) = index.lane(*key) else { continue };
        let top = body.top() + *y;
        let mut rest = starts_in(lane, t0, t1);
        while let Some(first) = rest.first() {
            // Everything that lands on this point of the strip is one tick.
            let column = ((first.start_ns - t0) as f64 / ns_per_pt).floor();
            let column_end = t0.saturating_add(((column + 1.0) * ns_per_pt).ceil() as u64);
            let in_column = rest.partition_point(|e| e.start_ns < column_end).max(1);
            let level = rest[..in_column.min(TICK_SAMPLE)]
                .iter()
                .filter_map(|e| e.log_level())
                .max()
                .unwrap_or(0);
            let x = body.left() + column as f32 + 0.5;
            painter.line_segment(
                [egui::pos2(x, top + 1.0), egui::pos2(x, top + h - 1.0)],
                egui::Stroke::new(1.5, level_color(level)),
            );
            rest = &rest[in_column..];
        }
    }
}

pub fn paint_highlight(ui: &Ui, body: egui::Rect, t0: f64, t1: f64, ns: u64) {
    if t1 <= t0 {
        return;
    }
    let x = body.left() + ((ns as f64 - t0) / (t1 - t0)) as f32 * body.width();
    if x < body.left() || x > body.right() {
        return;
    }
    ui.painter_at(body).line_segment(
        [egui::pos2(x, body.top()), egui::pos2(x, body.bottom())],
        egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xFF, 0xC1, 0x07, 180)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_at(ts: u64, tid: u32, name: u32, level: u8) -> LiveEvent {
        LiveEvent { start_ns: ts, duration_ns: 0, tid, pid: 1, kind: kind::LOG, depth: 0, extra: level, _pad: 0, name_id: name }
    }

    fn scope(start: u64, end: u64, tid: u32, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: end - start,
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    fn lines(cache: &Cache) -> Vec<(u64, u32, u32)> {
        match &cache.rows {
            Rows::Lines(lines) => lines.iter().map(|e| (e.start_ns, e.tid, e.name_id)).collect(),
            Rows::Groups(_) => panic!("expected lines"),
        }
    }

    #[test]
    fn the_window_interleaves_threads_by_time_and_keeps_write_order() {
        let mut index = TrackIndex::default();
        // Thread 7 logs twice at t=20; thread 3 logs at the same instant.
        for e in [log_at(10, 7, 1, 2), log_at(20, 7, 2, 2), log_at(20, 7, 3, 2), log_at(20, 3, 4, 2), log_at(30, 3, 5, 2), log_at(99, 3, 6, 2)] {
            index.insert(e);
        }
        let all = compute(&index, Query { t0: 0, t1: 50, thread: None, scope: None }, 0.0);
        assert_eq!(lines(&all), vec![(10, 7, 1), (20, 3, 4), (20, 7, 2), (20, 7, 3), (30, 3, 5)]);
        assert_eq!(all.matched, 5, "the line at 99 is outside the window");
        assert_eq!(all.threads, vec![3, 7]);
        let one = compute(&index, Query { t0: 0, t1: 50, thread: Some(7), scope: None }, 0.0);
        assert_eq!(lines(&one), vec![(10, 7, 1), (20, 7, 2), (20, 7, 3)]);
    }

    #[test]
    fn a_window_with_more_lines_than_the_list_holds_says_how_many() {
        let mut index = TrackIndex::default();
        for i in 0..(MAX_LINES as u64 + 500) {
            index.insert(log_at(i, 1 + (i % 3) as u32, 9, 2));
        }
        let cache = compute(&index, Query { t0: 0, t1: u64::MAX, thread: None, scope: None }, 0.0);
        let listed = lines(&cache);
        assert_eq!(listed.len(), MAX_LINES, "the list is cut");
        assert_eq!(cache.matched, MAX_LINES + 500, "and the header can say what was left out");
        assert!(listed.windows(2).all(|w| w[0].0 <= w[1].0), "the earliest lines, in order");
        assert_eq!(listed.last().unwrap().0, MAX_LINES as u64 - 1);
    }

    #[test]
    fn a_focused_scope_lists_its_lines_and_groups_them_when_it_recurs() {
        let mut index = TrackIndex::default();
        for i in 0..4u64 {
            index.insert(scope(i * 100, i * 100 + 50, 7, 1));
            index.insert(log_at(i * 100 + 10, 7, 20, 2));
            index.insert(log_at(i * 100 + 70, 7, 21, 3)); // between instances
        }
        index.insert(scope(1000, 1100, 7, 2));
        index.insert(log_at(1050, 7, 22, 4));
        index.insert(log_at(1050, 8, 23, 4)); // another thread

        let focus = |name_id| Query { t0: 0, t1: 0, thread: None, scope: Some(ScopeFocus { pid: 1, tid: 7, name_id }) };
        let recurring = compute(&index, focus(1), 0.0);
        assert_eq!(recurring.instances, 4);
        match &recurring.rows {
            Rows::Groups(groups) => {
                assert_eq!(groups.len(), 1);
                assert_eq!((groups[0].name_id, groups[0].count, groups[0].first_ns), (20, 4, 10));
            }
            Rows::Lines(_) => panic!("a recurring scope groups its lines"),
        }
        let single = compute(&index, focus(2), 0.0);
        assert_eq!(single.instances, 1);
        assert_eq!(lines(&single), vec![(1050, 7, 22)]);
    }

    /// Runs `add` as one headless egui frame in an 800 by 300 point window
    /// and returns how many shapes it painted.
    fn frame(ctx: &egui::Context, mut add: impl FnMut(&mut Ui)) -> usize {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 300.0))),
            ..Default::default()
        };
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| add(ui));
        });
        out.shapes.len()
    }

    #[test]
    fn only_the_rows_scrolled_into_view_are_laid_out() {
        let ctx = egui::Context::default();
        let mut laid_out = 0;
        // Twice: the first frame of a scroll area measures itself.
        for _ in 0..2 {
            laid_out = 0;
            frame(&ctx, |ui| {
                show_rows(ui, 100_000, |ui, row| {
                    laid_out += 1;
                    // A line with a break in it must still be one row high.
                    line_row(ui, format!("{row:>6}  "), "first\nsecond", row % 2 == 0, 3, false, "hint");
                    None
                });
            });
        }
        assert!(laid_out > 5, "something is on screen: {laid_out}");
        assert!(laid_out < 60, "a 300 point window does not lay out 100,000 rows: {laid_out}");
    }

    #[test]
    fn the_window_draws_every_kind_of_list_without_a_capture_behind_it() {
        let mut index = TrackIndex::default();
        let mut intern = InternTable::default();
        intern.insert_id(1, "frame");
        intern.insert_id(20, "tick");
        for i in 0..4u64 {
            index.insert(scope(i * 100, i * 100 + 50, 7, 1));
            index.insert(log_at(i * 100 + 10, 7, 20, 2));
        }
        index.insert(scope(1000, 1100, 7, 2));
        index.insert(LiveEvent { kind: kind::DROP, duration_ns: 12, ..log_at(40, 7, 0, 6) });
        let ctx = egui::Context::default();
        let mut state = LogUi::default();
        let draw = |state: &mut LogUi, t0: f64, t1: f64| {
            for _ in 0..2 {
                frame(&ctx, |ui| {
                    assert!(show(ui, state, &index, &intern, t0, t1).is_none(), "nothing was clicked");
                });
            }
        };
        // Lines in the window, an empty window, a recurring scope (grouped),
        // a scope with one instance and no lines.
        draw(&mut state, 0.0, 1_000.0);
        assert_eq!(state.cache.as_ref().unwrap().matched, 4);
        assert_eq!(state.cache.as_ref().unwrap().dropped, 12);
        draw(&mut state, 5_000.0, 6_000.0);
        assert_eq!(state.cache.as_ref().unwrap().matched, 0);
        state.scope = Some(ScopeFocus { pid: 1, tid: 7, name_id: 1 });
        draw(&mut state, 5_000.0, 6_000.0);
        assert!(matches!(state.cache.as_ref().unwrap().rows, Rows::Groups(ref g) if g.len() == 1 && g[0].count == 4));
        state.scope = Some(ScopeFocus { pid: 1, tid: 7, name_id: 2 });
        draw(&mut state, 5_000.0, 6_000.0);
        assert_eq!(state.cache.as_ref().unwrap().instances, 1);
    }

    #[test]
    fn the_list_is_kept_until_the_question_or_the_capture_changes() {
        let mut index = TrackIndex::default();
        let intern = InternTable::default();
        index.insert(log_at(10, 7, 1, 2));
        let ctx = egui::Context::default();
        let mut state = LogUi::default();
        let computed_at = |state: &LogUi| state.cache.as_ref().unwrap().stamp;
        frame(&ctx, |ui| {
            show(ui, &mut state, &index, &intern, 0.0, 100.0);
        });
        assert_eq!(state.cache.as_ref().unwrap().matched, 1);
        // The same view of the same capture: the rows are reused as they are.
        state.cache.as_mut().unwrap().matched = 777;
        frame(&ctx, |ui| {
            show(ui, &mut state, &index, &intern, 0.0, 100.0);
        });
        assert_eq!(state.cache.as_ref().unwrap().matched, 777, "not recomputed");
        // The view moved: recomputed at once.
        frame(&ctx, |ui| {
            show(ui, &mut state, &index, &intern, 0.0, 50.0);
        });
        assert_eq!(state.cache.as_ref().unwrap().matched, 1);
        // Events arrived a moment after the list was built: the list waits
        // for its refresh interval instead of being rebuilt every frame.
        let before = computed_at(&state);
        index.insert(log_at(20, 7, 1, 2));
        frame(&ctx, |ui| {
            show(ui, &mut state, &index, &intern, 0.0, 50.0);
        });
        assert_eq!(computed_at(&state), before, "inside the refresh interval");
        assert_eq!(state.cache.as_ref().unwrap().matched, 1);
        // ...and is rebuilt once that interval has passed.
        state.cache.as_mut().unwrap().computed_at = -1.0;
        frame(&ctx, |ui| {
            show(ui, &mut state, &index, &intern, 0.0, 50.0);
        });
        assert_eq!(state.cache.as_ref().unwrap().matched, 2);
    }

    #[test]
    fn the_tick_strip_draws_a_tick_per_column_and_only_for_lanes_on_screen() {
        let mut index = TrackIndex::default();
        for i in 0..200_000u64 {
            index.insert(log_at(i * 5, 7, 1, if i % 1000 == 0 { 4 } else { 2 }));
        }
        let key = LaneKey { pid: 1, tid: 7, kind: kind::LOG, depth: 0, extra: 0 };
        assert!(index.lane(key).is_some(), "the lane the strip is drawn on");
        let layout = [(key, 20.0)];
        let ctx = egui::Context::default();
        let bare = frame(&ctx, |_| {});
        let paint = |cull: Option<YCull>, t1: u64| {
            frame(&ctx, |ui| {
                let body = ui.max_rect();
                paint_ticks(ui, body, 0, t1, &index, &layout, 1.0, cull);
            }) - bare
        };
        // 200,000 lines across the whole view: one tick per point of width.
        let all = paint(None, 1_000_000);
        assert!(all > 400 && all <= 800, "{all} ticks for 200,000 lines in an 800 point window");
        // Zoomed in to twenty lines: each is its own tick.
        assert_eq!(paint(None, 99), 20);
        // A lane scrolled out of view costs nothing.
        assert_eq!(paint(Some(YCull::new(500.0, 900.0)), 1_000_000), 0);
        assert_eq!(paint(Some(YCull::new(0.0, 100.0)), 99), 20);
    }

    #[test]
    fn the_window_opens_once_for_a_capture_that_logs_and_never_for_one_that_does_not() {
        let mut ui = LogUi::default();
        let mut index = TrackIndex::default();
        assert!(!ui.open, "closed until there is something to show");
        index.insert(scope(0, 10, 1, 1));
        ui.open_on_first_log(&index);
        assert!(!ui.open, "scopes alone do not open it");
        index.insert(log_at(5, 1, 2, 2));
        ui.open_on_first_log(&index);
        assert!(ui.open);
        // Hidden by hand: more lines, even on new threads, do not reopen it.
        ui.open = false;
        index.insert(log_at(6, 2, 2, 2));
        ui.open_on_first_log(&index);
        assert!(!ui.open);
        ui.reset_capture();
        ui.open_on_first_log(&index);
        assert!(ui.open, "a new capture gets its own first time");
    }
}
