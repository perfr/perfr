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
//! A channel filter keeps the scopes recorded with that channel, and therefore
//! only the lines inside those scopes. A line itself has no channel.

use eframe::egui::{self, Color32, FontId, RichText, ScrollArea, Sense, Ui};
use perfr_live_event::logs::{aggregate, logs_in_spans, ScopeSpan};
use perfr_live_event::{kind, InternTable, LiveEvent};
use perfr_live_render::{Lane, TrackIndex};

use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeFocus {
    pub pid: u32,
    pub tid: u32,
    pub name_id: u32,
}

#[derive(Clone, Debug)]
pub struct LogUi {
    pub open: bool,
    pub thread_filter: Option<u32>,
    pub channel_filter: Option<u8>,
    pub scope: Option<ScopeFocus>,
    pub highlight_ns: Option<u64>,
    pub channels: Vec<(u8, String)>,
}

impl Default for LogUi {
    fn default() -> Self {
        Self {
            open: true,
            thread_filter: None,
            channel_filter: None,
            scope: None,
            highlight_ns: None,
            channels: Vec::new(),
        }
    }
}

impl LogUi {
    pub fn note_channel(&mut self, id: u8, name: String) {
        if let Some((_, existing)) = self.channels.iter_mut().find(|(i, _)| *i == id) {
            *existing = name;
        } else {
            self.channels.push((id, name));
            self.channels.sort_by_key(|(i, _)| *i);
        }
    }

    pub fn channel_name(&self, id: u8) -> String {
        self.channels
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| if id == 0 { "default".into() } else { format!("channel {id}") })
    }
}

pub enum LogAction {
    /// Center the timeline on this timestamp and draw the highlight.
    Focus(u64),
}

fn level_name(level: u8) -> &'static str {
    match level {
        0 => "TRACE",
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARN",
        4 => "ERROR",
        _ => "FATAL",
    }
}

fn level_color(level: u8) -> Color32 {
    match level {
        0 | 1 => Color32::from_rgb(0x90, 0xA4, 0xAE),
        2 => Color32::from_rgb(0x66, 0xBB, 0x6A),
        3 => Color32::from_rgb(0xFF, 0xC1, 0x07),
        _ => Color32::from_rgb(0xF4, 0x43, 0x36),
    }
}

fn message<'a>(intern: &'a InternTable, id: u32) -> &'a str {
    intern.get(id).unwrap_or("")
}

/// Events of `kind_id` on `lane` whose start is inside `[t0, t1]`.
fn starts_in(lane: &Lane, t0: u64, t1: u64) -> impl Iterator<Item = LiveEvent> + '_ {
    let ev = lane.events();
    let begin = ev.partition_point(|e| e.start_ns < t0);
    let end = ev.partition_point(|e| e.start_ns <= t1);
    ev[begin..end].iter().copied()
}

/// Scopes that overlap `[t0, t1]`. A scope lane does not overlap itself, so
/// walking back from the first start at or after `t0` finds the one still open.
fn scopes_overlapping(lane: &Lane, t0: u64, t1: u64) -> Vec<ScopeSpan> {
    let ev = lane.events();
    if ev.is_empty() {
        return Vec::new();
    }
    let mut i = ev.partition_point(|e| e.start_ns < t0);
    if i > 0 && ev[i - 1].end_ns() >= t0 {
        i -= 1;
    }
    let mut out = Vec::new();
    while i < ev.len() && ev[i].start_ns <= t1 {
        if ev[i].end_ns() >= t0 {
            if let Some(span) = ScopeSpan::from_event(&ev[i]) {
                out.push(span);
            }
        }
        i += 1;
    }
    out
}

fn log_lanes<'a>(
    index: &'a TrackIndex,
    tid: Option<u32>,
) -> impl Iterator<Item = (u32, u32, &'a Lane)> + 'a {
    index.lanes().filter_map(move |(k, lane)| {
        if k.kind != kind::LOG {
            return None;
        }
        if tid.is_some_and(|t| k.tid != t) {
            return None;
        }
        Some((k.pid, k.tid, lane))
    })
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
    let t0 = t0.max(0.0) as u64;
    let t1 = (t1.max(0.0) as u64).max(t0);

    ui.horizontal(|ui| {
        ui.label(RichText::new("LOGS").size(11.0).color(theme::MUTED()));
        let threads = thread_ids(index);
        let thread_label = match state.thread_filter {
            Some(tid) => format!("thread {tid}"),
            None => "all threads".into(),
        };
        egui::ComboBox::from_id_salt("perfr_log_thread")
            .selected_text(thread_label)
            .width(120.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut state.thread_filter, None, "all threads");
                for tid in threads {
                    ui.selectable_value(&mut state.thread_filter, Some(tid), format!("thread {tid}"));
                }
            });

        let channel_label = match state.channel_filter {
            Some(id) => state.channel_name(id),
            None => "all channels".into(),
        };
        egui::ComboBox::from_id_salt("perfr_log_channel")
            .selected_text(channel_label)
            .width(140.0)
            .show_ui(ui, |ui| {
                let default_name = state.channel_name(0);
                ui.selectable_value(&mut state.channel_filter, None, "all channels");
                ui.selectable_value(&mut state.channel_filter, Some(0), default_name);
                for (id, name) in state.channels.clone() {
                    if id != 0 {
                        ui.selectable_value(&mut state.channel_filter, Some(id), name);
                    }
                }
            });

        if state.scope.is_some() && ui.button("clear scope").clicked() {
            state.scope = None;
        }
        let dropped = drop_total(index);
        if dropped > 0 {
            ui.label(
                RichText::new(format!("{dropped} events dropped"))
                    .color(Color32::from_rgb(0xF4, 0x43, 0x36))
                    .size(12.0),
            );
        }
        if ui.button("hide").clicked() {
            state.open = false;
        }
    });

    if let Some(focus) = state.scope {
        let name = message(intern, focus.name_id);
        let spans = scope_instances(index, focus, state.channel_filter);
        ui.label(
            RichText::new(format!(
                "{}  thread {}  {} instance{}",
                if name.is_empty() { "(unnamed scope)" } else { name },
                focus.tid,
                spans.len(),
                if spans.len() == 1 { "" } else { "s" }
            ))
            .size(11.0)
            .color(theme::TEXT()),
        );
        let logs = logs_on_thread(index, focus.pid, focus.tid);
        let matched = logs_in_spans(&logs, &spans);
        if spans.len() > 1 {
            show_groups(ui, &aggregate(&matched), intern, state.highlight_ns)
        } else {
            show_lines(ui, &matched, intern, state, true)
        }
    } else if let Some(channel) = state.channel_filter {
        let spans = channel_spans(index, state.thread_filter, channel, t0, t1);
        let mut logs = logs_in_window_index(index, t0, t1, state.thread_filter);
        logs = logs_in_spans(&logs, &spans);
        logs.retain(|e| e.start_ns >= t0 && e.start_ns <= t1);
        ui.label(
            RichText::new(format!(
                "{}  ·  {} scope{} in view",
                state.channel_name(channel),
                spans.len(),
                if spans.len() == 1 { "" } else { "s" }
            ))
            .size(11.0)
            .color(theme::MUTED()),
        );
        show_lines(ui, &logs, intern, state, false)
    } else {
        let logs = logs_in_window_index(index, t0, t1, state.thread_filter);
        show_lines(ui, &logs, intern, state, false)
    }
}

fn thread_ids(index: &TrackIndex) -> Vec<u32> {
    let mut tids: Vec<u32> = log_lanes(index, None).map(|(_, tid, _)| tid).collect();
    tids.sort_unstable();
    tids.dedup();
    tids
}

fn drop_total(index: &TrackIndex) -> u64 {
    index
        .lanes()
        .filter(|(k, _)| k.kind == kind::DROP)
        .flat_map(|(_, lane)| lane.events().iter().copied())
        .filter_map(|e| e.drop_count())
        .fold(0u64, u64::saturating_add)
}

fn scope_instances(index: &TrackIndex, focus: ScopeFocus, channel: Option<u8>) -> Vec<ScopeSpan> {
    let mut spans = Vec::new();
    for (k, lane) in index.lanes() {
        if k.kind != kind::API_SCOPE && k.kind != kind::API_TRACK {
            continue;
        }
        if k.pid != focus.pid || k.tid != focus.tid {
            continue;
        }
        for event in lane.events() {
            if event.name_id != focus.name_id {
                continue;
            }
            if let Some(span) = ScopeSpan::from_event(event) {
                if channel.is_none_or(|c| span.channel == c) {
                    spans.push(span);
                }
            }
        }
    }
    spans
}

fn logs_on_thread(index: &TrackIndex, pid: u32, tid: u32) -> Vec<LiveEvent> {
    let mut logs = Vec::new();
    for (p, t, lane) in log_lanes(index, Some(tid)) {
        if p == pid && t == tid {
            logs.extend(lane.events().iter().copied());
        }
    }
    logs
}

fn logs_in_window_index(index: &TrackIndex, t0: u64, t1: u64, tid: Option<u32>) -> Vec<LiveEvent> {
    let mut logs = Vec::new();
    for (_, _, lane) in log_lanes(index, tid) {
        logs.extend(starts_in(lane, t0, t1));
    }
    logs.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then(a.tid.cmp(&b.tid)));
    logs
}

fn channel_spans(index: &TrackIndex, tid: Option<u32>, channel: u8, t0: u64, t1: u64) -> Vec<ScopeSpan> {
    let mut spans = Vec::new();
    for (k, lane) in index.lanes() {
        if k.kind != kind::API_SCOPE && k.kind != kind::API_TRACK {
            continue;
        }
        if tid.is_some_and(|t| k.tid != t) {
            continue;
        }
        for span in scopes_overlapping(lane, t0, t1) {
            if span.channel == channel {
                spans.push(span);
            }
        }
    }
    spans
}

fn show_lines(
    ui: &mut Ui,
    logs: &[LiveEvent],
    intern: &InternTable,
    state: &LogUi,
    scoped: bool,
) -> Option<LogAction> {
    if logs.is_empty() {
        let why = if scoped {
            "No log lines inside this scope on this thread."
        } else {
            "No log lines in this time range."
        };
        ui.label(RichText::new(why).size(12.0).color(theme::MUTED()));
        return None;
    }
    let mut action = None;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for log in logs {
            let level = log.log_level().unwrap_or(0);
            let text = message(intern, log.name_id);
            let mark = if log.log_truncated() { "  [truncated]" } else { "" };
            let thread = if state.thread_filter.is_some() || scoped {
                String::new()
            } else {
                format!("{:>8}  ", log.tid)
            };
            let line = format!("{thread}{}{mark}", text);
            let selected = state.highlight_ns == Some(log.start_ns);
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(format!("{:5}  {line}", level_name(level)))
                        .font(FontId::monospace(12.0))
                        .color(if selected { theme::ACCENT() } else { level_color(level) }),
                )
                .sense(Sense::click()),
            );
            if resp.clicked() {
                action = Some(LogAction::Focus(log.start_ns));
            }
            resp.on_hover_text("Show this time on the timeline");
        }
    });
    action
}

fn show_groups(
    ui: &mut Ui,
    groups: &[perfr_live_event::logs::LogGroup],
    intern: &InternTable,
    highlight: Option<u64>,
) -> Option<LogAction> {
    if groups.is_empty() {
        ui.label(RichText::new("No log lines inside this scope on this thread.").size(12.0).color(theme::MUTED()));
        return None;
    }
    ui.label(
        RichText::new("Same line across every instance of this scope, with how many times it occurred.")
            .size(10.0)
            .color(theme::MUTED()),
    );
    let mut action = None;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for group in groups {
            let text = message(intern, group.name_id);
            let mark = if group.truncated { "  [truncated]" } else { "" };
            let selected = highlight == Some(group.first_ns);
            let resp = ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{:5}  {:>6}×  {}{mark}",
                        level_name(group.level),
                        group.count,
                        text
                    ))
                    .font(FontId::monospace(12.0))
                    .color(if selected { theme::ACCENT() } else { level_color(group.level) }),
                )
                .sense(Sense::click()),
            );
            if resp.clicked() {
                action = Some(LogAction::Focus(group.first_ns));
            }
            resp.on_hover_text("Jump to the first time this line occurred");
        }
    });
    action
}

/// Vertical ticks for log lines on each thread's log lane, in the visible window.
pub fn paint_ticks(ui: &Ui, body: egui::Rect, t0: f64, t1: f64, index: &TrackIndex, layout: &[(perfr_live_event::LaneKey, f32)], scale: f32) {
    if t1 <= t0 {
        return;
    }
    let span = t1 - t0;
    let painter = ui.painter_at(body);
    let t0n = t0.max(0.0) as u64;
    let t1n = t1.max(0.0) as u64;
    for (key, y) in layout {
        if key.kind != kind::LOG {
            continue;
        }
        let Some(lane) = index.lane(*key) else { continue };
        let h = perfr_live_render::lane_height(*key) * scale.max(0.01);
        let top = body.top() + *y;
        for event in starts_in(lane, t0n, t1n) {
            let x = body.left() + ((event.start_ns as f64 - t0) / span) as f32 * body.width();
            let color = level_color(event.log_level().unwrap_or(0));
            painter.line_segment(
                [egui::pos2(x, top + 1.0), egui::pos2(x, top + h - 1.0)],
                egui::Stroke::new(1.5, color),
            );
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
