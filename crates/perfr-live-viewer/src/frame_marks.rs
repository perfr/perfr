// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Frames, for a program that marks them (`perfr_frame_mark`).
//!
//! A frame is not a row of the timeline. It is drawn twice, and both times
//! as context for everything else:
//!
//! - a strip under the ruler with one bar per frame, as tall as the frame
//!   was long. The strip always shows the whole capture, whatever the
//!   timeline is zoomed to, so a slow frame can be seen from anywhere; a
//!   click on a bar puts that frame in the timeline. The time the timeline
//!   is showing is a window on the strip: dragging it pans the timeline,
//!   and dragging the handle on either edge of it moves that edge;
//! - bands behind the tracks of the process that marked the frames, every
//!   other frame a little darker, so that where one frame ends and the next
//!   begins can be read off any of its threads. The scheduler and every
//!   other process keep their plain ground: the frames are not theirs.
//!
//! What a draw costs does not grow with the capture: the strip's bars are
//! worked out when the frames or the strip's width change and kept until
//! they change again, and there is at most one bar per pixel column. The
//! bands only ever look at the frames in view.

use eframe::egui::{self, Color32, FontId, Pos2, Rect, RichText, Stroke, Ui};
use perfr_live_event::{kind, InternTable, LaneKey, LiveEvent, DROP_MARK};
use perfr_live_render::{Lane, TrackIndex};

use crate::rect_select::fmt_dur;
use crate::tracks::{RowId, TrackRow};
use crate::{fonts, theme};

/// Height of the strip, in points.
pub const STRIP_H: f32 = 34.0;

/// How much of the timeline's width a frame takes after a click on its bar.
const ZOOM_FILL: f64 = 0.9;

/// The strip's full height, in median frames. A capture with one frame of
/// half a second in it would otherwise draw every other frame one pixel
/// tall; past this a bar is as tall as the strip and its colour says the
/// rest.
const CEILING_MEDIANS: f64 = 3.0;

/// How many frames the median is taken over, evenly spaced along the
/// series. It only sets the strip's scale and where the colours change.
const MEDIAN_SAMPLE: usize = 4096;

/// A frame this many medians long is slow, and this many a hitch.
const SLOW_MEDIANS: f64 = 1.5;
const HITCH_MEDIANS: f64 = 2.5;

/// What the main series of frames is called; the service names it.
const MAIN_SERIES: &str = "frame";

/// The series of frames in `index`, in the order they are offered: those
/// of process `prefer_pid` first (the one the capture is of, when several
/// programs on the machine mark frames), a main series before a named one,
/// and then the one with the most frames.
pub fn series(index: &TrackIndex, intern: &InternTable, prefer_pid: Option<u32>) -> Vec<LaneKey> {
    let mut all: Vec<(bool, bool, usize, LaneKey)> = index
        .lanes()
        .filter(|(key, lane)| key.kind == kind::FRAME && !lane.is_empty())
        .map(|(key, lane)| (Some(key.pid) != prefer_pid, series_name(lane, intern) != MAIN_SERIES, lane.len(), key))
        .collect();
    all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.2.cmp(&a.2)).then(a.3.cmp(&b.3)));
    all.into_iter().map(|(_, _, _, key)| key).collect()
}

/// The series each process's bands are drawn from, one a process: the one
/// the strip is `showing` for its process, and for every other process the
/// first of its series in `all` (the order of [`series`]).
pub fn band_series(all: &[LaneKey], showing: Option<LaneKey>) -> Vec<LaneKey> {
    let mut out: Vec<LaneKey> = showing.into_iter().collect();
    for key in all {
        if out.iter().all(|taken| taken.pid != key.pid) {
            out.push(*key);
        }
    }
    out
}

/// What a series is called.
pub fn series_name<'a>(lane: &Lane, intern: &'a InternTable) -> &'a str {
    lane.events().first().and_then(|e| intern.get(e.name_id)).unwrap_or(MAIN_SERIES)
}

/// The timeline window that shows a frame at [`ZOOM_FILL`] of its width,
/// centred.
pub fn frame_window(frame: &LiveEvent) -> (f64, f64) {
    let start = frame.start_ns as f64;
    let length = frame.duration_ns.max(1) as f64;
    let span = length / ZOOM_FILL;
    let t0 = start + length / 2.0 - span / 2.0;
    (t0, t0 + span)
}

/// What the strip measures a frame against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub median_ns: u64,
    /// The frame time a bar of the strip's full height stands for.
    pub ceiling_ns: u64,
}

impl Default for Scale {
    fn default() -> Self {
        Scale { median_ns: 1, ceiling_ns: 1 }
    }
}

pub fn scale(frames: &[LiveEvent]) -> Scale {
    if frames.is_empty() {
        return Scale::default();
    }
    let step = frames.len().div_ceil(MEDIAN_SAMPLE).max(1);
    let mut sample: Vec<u64> = frames.iter().step_by(step).map(|e| e.duration_ns).collect();
    let mid = sample.len() / 2;
    let median_ns = (*sample.select_nth_unstable(mid).1).max(1);
    let longest = frames.iter().map(|e| e.duration_ns).max().unwrap_or(1).max(1);
    let ceiling_ns = longest.min((median_ns as f64 * CEILING_MEDIANS) as u64).max(1);
    Scale { median_ns, ceiling_ns }
}

/// How a frame compares with the median of its series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    Even,
    Slow,
    Hitch,
}

pub fn pace(duration_ns: u64, median_ns: u64) -> Pace {
    let medians = duration_ns as f64 / median_ns.max(1) as f64;
    if medians > HITCH_MEDIANS {
        Pace::Hitch
    } else if medians > SLOW_MEDIANS {
        Pace::Slow
    } else {
        Pace::Even
    }
}

fn pace_color(pace: Pace) -> Color32 {
    match pace {
        Pace::Even => theme::ACCENT().gamma_multiply(0.8),
        Pace::Slow => Color32::from_rgb(0xFF, 0xC1, 0x07),
        Pace::Hitch => argb(DROP_MARK),
    }
}

fn argb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// One bar of the strip: a run of pixel columns and the frame they show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bar {
    /// Left and right edge, in points from the strip's left.
    pub x0: f32,
    pub x1: f32,
    /// Index into the series: the longest frame the columns hold.
    pub frame: usize,
}

/// The strip's bars for `frames` (sorted by start, not overlapping) laid
/// across `width` points that stand for `cap0..cap1`.
///
/// A frame wider than a point is one bar. Where several frames share a
/// column the column shows the longest of them, since a slow frame among
/// fast ones is what the strip is looked at for; a click there goes to it.
pub fn bars(frames: &[LiveEvent], cap0: f64, cap1: f64, width: f32) -> Vec<Bar> {
    let columns = width.ceil() as usize;
    if columns == 0 || cap1 <= cap0 {
        return Vec::new();
    }
    let per_ns = f64::from(width) / (cap1 - cap0);
    let mut longest: Vec<Option<usize>> = vec![None; columns];
    for (i, frame) in frames.iter().enumerate() {
        let x0 = (frame.start_ns as f64 - cap0) * per_ns;
        let x1 = (frame.end_ns() as f64 - cap0) * per_ns;
        if x1 <= 0.0 || x0 >= columns as f64 {
            continue;
        }
        let first = x0.max(0.0).floor() as usize;
        let last = ((x1.ceil() as usize).max(first + 1) - 1).min(columns - 1);
        for slot in &mut longest[first.min(columns - 1)..=last] {
            if slot.is_none_or(|held| frames[held].duration_ns < frame.duration_ns) {
                *slot = Some(i);
            }
        }
    }
    let mut out: Vec<Bar> = Vec::new();
    for (column, frame) in longest.into_iter().enumerate() {
        let Some(frame) = frame else { continue };
        match out.last_mut() {
            Some(bar) if bar.frame == frame && bar.x1 == column as f32 => bar.x1 += 1.0,
            _ => out.push(Bar { x0: column as f32, x1: column as f32 + 1.0, frame }),
        }
    }
    out
}

/// The bar under `x`, in points from the strip's left.
pub fn bar_at(bars: &[Bar], x: f32) -> Option<&Bar> {
    let at = bars.partition_point(|bar| bar.x1 <= x);
    bars.get(at).filter(|bar| bar.x0 <= x)
}

/// The narrowest the timeline's window is drawn on the strip, in points.
/// Zoomed to one frame of a long capture it would be a hairline, with
/// nothing to see and nothing to take hold of.
const WINDOW_MIN_PT: f32 = 10.0;

/// How far outside the window's edge its handle can be taken hold of, and
/// the most of the window's inside it claims. The inside share shrinks with
/// the window, so a narrow one keeps a middle to drag it by.
const HANDLE_OUT_PT: f32 = 7.0;
const HANDLE_IN_PT: f32 = 4.0;

/// Where the timeline's window `view` is drawn on a strip `width` points
/// wide that stands for `cap`: its left and right edge, in points from the
/// strip's left. `None` when none of it is on the strip.
pub fn window_edges(view: (f64, f64), cap: (f64, f64), width: f32) -> Option<(f32, f32)> {
    if cap.1 <= cap.0 || view.1 <= view.0 || width <= 0.0 {
        return None;
    }
    let x = |t: f64| ((t - cap.0) / (cap.1 - cap.0)) as f32 * width;
    let (left, right) = (x(view.0).max(0.0), x(view.1).min(width));
    if right <= 0.0 || left >= width || right < left {
        return None;
    }
    if right - left >= WINDOW_MIN_PT {
        return Some((left, right));
    }
    // Widened about its middle, and kept on the strip.
    let half = WINDOW_MIN_PT.min(width) / 2.0;
    let middle = ((left + right) / 2.0).clamp(half, width - half);
    Some((middle - half, middle + half))
}

/// What of the window is under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowPart {
    LeftHandle,
    RightHandle,
    Body,
}

/// The part of the window with edges `left` and `right` under `x`.
pub fn window_part_at(x: f32, left: f32, right: f32) -> Option<WindowPart> {
    let inside = HANDLE_IN_PT.min((right - left) / 4.0);
    if x >= left - HANDLE_OUT_PT && x < left + inside {
        Some(WindowPart::LeftHandle)
    } else if x > right - inside && x <= right + HANDLE_OUT_PT {
        Some(WindowPart::RightHandle)
    } else if x >= left && x <= right {
        Some(WindowPart::Body)
    } else {
        None
    }
}

/// A drag of the window in progress. Each holds how far the pointer's time
/// was from the time it moves when the drag began, so that nothing jumps
/// under the pointer: not when the handle was taken a few points off the
/// edge, and not when the window is drawn wider than it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Grab {
    /// The whole window moves; the offset is from its start.
    Pan { offset_ns: f64 },
    /// One edge moves and the other stays; the offset is from that edge.
    Left { offset_ns: f64 },
    Right { offset_ns: f64 },
}

impl Grab {
    /// The drag that begins with the pointer at time `pointer_ns` over
    /// `part` of the window `view`. A drag that begins off the window takes
    /// the window with it, by its middle, as the slider at the bottom does.
    pub fn begin(part: Option<WindowPart>, pointer_ns: f64, view: (f64, f64)) -> Grab {
        match part {
            Some(WindowPart::LeftHandle) => Grab::Left { offset_ns: pointer_ns - view.0 },
            Some(WindowPart::RightHandle) => Grab::Right { offset_ns: pointer_ns - view.1 },
            Some(WindowPart::Body) => Grab::Pan { offset_ns: pointer_ns - view.0 },
            None => Grab::Pan { offset_ns: (view.1 - view.0) / 2.0 },
        }
    }

    /// The window `view` becomes, with the pointer now at `pointer_ns`. It
    /// stays inside `cap`, an edge never passes the other, and the window
    /// is never narrower than `min_span_ns`.
    pub fn window(self, pointer_ns: f64, view: (f64, f64), cap: (f64, f64), min_span_ns: f64) -> (f64, f64) {
        match self {
            Grab::Pan { offset_ns } => {
                let span = view.1 - view.0;
                let t0 = (pointer_ns - offset_ns).min(cap.1 - span).max(cap.0);
                (t0, t0 + span)
            }
            Grab::Left { offset_ns } => {
                let t0 = (pointer_ns - offset_ns).max(cap.0).min(view.1 - min_span_ns);
                (t0, view.1)
            }
            Grab::Right { offset_ns } => {
                let t1 = (pointer_ns - offset_ns).min(cap.1).max(view.0 + min_span_ns);
                (view.0, t1)
            }
        }
    }

    /// Whether the drag changes how much time the window holds.
    pub fn resizes(self) -> bool {
        !matches!(self, Grab::Pan { .. })
    }
}

/// What the user did on the strip this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StripAction {
    /// Clicked a bar: show this frame.
    ShowFrame(LiveEvent),
    /// Dragged the window: show this time, of the same length as before.
    Pan(f64, f64),
    /// Dragged a handle: show this time.
    Resize(f64, f64),
}

/// The strip's bars and scale, kept until the frames, the capture's extent
/// or the strip's width change.
#[derive(Default)]
pub struct StripMemo {
    key: Option<(LaneKey, u64, u64, u64, u32)>,
    bars: Vec<Bar>,
    scale: Scale,
}

impl StripMemo {
    pub fn get(&mut self, series: LaneKey, lane: &Lane, cap0: f64, cap1: f64, width: f32) -> (&[Bar], Scale) {
        let key = (series, lane.version(), cap0.to_bits(), cap1.to_bits(), width.to_bits());
        if self.key != Some(key) {
            self.key = Some(key);
            self.bars = bars(lane.events(), cap0, cap1, width);
            self.scale = scale(lane.events());
        }
        (&self.bars, self.scale)
    }
}

/// Draws the strip in `rect` and answers with what the user did on it.
/// `resp` is the strip's own widget, sensing clicks and drags; the caller
/// makes it, and gives what this leaves alone (a right-drag) to the
/// timeline's own gestures.
///
/// `cap` is the time the strip's width stands for and `view` the window the
/// timeline shows. The window is drawn on the strip with a handle on each
/// edge: a drag of the window pans the timeline, a drag of a handle moves
/// that edge, and a click on a bar shows that frame. `grab` is the drag in
/// progress, kept by the caller from one draw to the next; `min_span_ns` is
/// the narrowest window the timeline will show.
#[allow(clippy::too_many_arguments)]
pub fn paint_strip(
    ui: &Ui,
    rect: Rect,
    resp: &egui::Response,
    frames: &[LiveEvent],
    bars: &[Bar],
    scale: Scale,
    cap: (f64, f64),
    view: (f64, f64),
    min_span_ns: f64,
    name: &str,
    grab: &mut Option<Grab>,
) -> Option<StripAction> {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::INPUT());
    // The window is dragged with the primary button. A right-drag is the
    // timeline's measure, here as anywhere else.
    let primary = egui::PointerButton::Primary;
    let (cap0, cap1) = cap;
    let time_at = |x: f32| cap0 + f64::from((x - rect.left()) / rect.width().max(1.0)) * (cap1 - cap0);
    let window = window_edges(view, cap, rect.width());
    let part_at = |x: f32| window.and_then(|(left, right)| window_part_at(x - rect.left(), left, right));

    // A drag is decided by where the button went down, not by where the
    // pointer has got to by the time the press is known to be a drag.
    let mut action = None;
    if resp.drag_started_by(primary) {
        let origin = ui.input(|i| i.pointer.press_origin()).or(resp.interact_pointer_pos());
        if let Some(origin) = origin.filter(|_| cap1 > cap0) {
            *grab = Some(Grab::begin(part_at(origin.x), time_at(origin.x), view));
        }
    }
    if !resp.dragged_by(primary) && !resp.drag_started_by(primary) {
        *grab = None;
    }
    if let (Some(held), Some(pointer)) = (*grab, resp.interact_pointer_pos()) {
        let (t0, t1) = held.window(time_at(pointer.x), view, cap, min_span_ns);
        if (t0, t1) != view {
            action = Some(if held.resizes() { StripAction::Resize(t0, t1) } else { StripAction::Pan(t0, t1) });
        }
    }

    let hover_x = resp.hover_pos().map(|p| p.x);
    // What is under the pointer: a part of the window first, since its
    // handles reach a little outside it, then a bar.
    let over_part = match *grab {
        Some(Grab::Left { .. }) => Some(WindowPart::LeftHandle),
        Some(Grab::Right { .. }) => Some(WindowPart::RightHandle),
        Some(Grab::Pan { .. }) => Some(WindowPart::Body),
        None => hover_x.and_then(part_at),
    };
    let over_bar = if grab.is_some() || matches!(over_part, Some(WindowPart::LeftHandle | WindowPart::RightHandle)) {
        None
    } else {
        hover_x.and_then(|x| bar_at(bars, x - rect.left())).copied()
    };

    // A point of headroom, so a bar of full height is not cut by the ruler.
    let room = (rect.height() - 3.0).max(1.0);
    for bar in bars {
        let frame = &frames[bar.frame];
        let tall = (frame.duration_ns as f64 / scale.ceiling_ns as f64).min(1.0) as f32;
        let h = (tall * room).max(1.5);
        // A sliver of the strip's ground between two bars wide enough to
        // spare one, so that neighbours of equal height read as two.
        let gap = if bar.x1 - bar.x0 >= 3.0 { 1.0 } else { 0.0 };
        let shape = Rect::from_min_max(
            Pos2::new(rect.left() + bar.x0, rect.bottom() - h),
            Pos2::new(rect.left() + bar.x1 - gap, rect.bottom()),
        );
        let mut color = pace_color(pace(frame.duration_ns, scale.median_ns));
        if over_bar.is_some_and(|over| over.frame == bar.frame) {
            color = color.gamma_multiply(1.35);
        }
        painter.rect_filled(shape, 0.0, color);
    }

    // The time the timeline is showing: the rest of the strip steps back,
    // and the window has an edge all round and a handle on either side.
    if let Some((left, right)) = window {
        let shown = Rect::from_min_max(
            Pos2::new(rect.left() + left, rect.top()),
            Pos2::new(rect.left() + right, rect.bottom()),
        );
        crate::app::note_ui_rect("frames:window", shown);
        let veil = theme::INPUT().gamma_multiply(0.62);
        painter.rect_filled(rect.with_max_x(shown.left()), 0.0, veil);
        painter.rect_filled(rect.with_min_x(shown.right()), 0.0, veil);
        let ink = theme::TEXT();
        let held = grab.is_some();
        let lit = |part: WindowPart| held || over_part == Some(part);
        painter.rect_filled(shown, 0.0, ink.gamma_multiply(if lit(WindowPart::Body) { 0.12 } else { 0.06 }));
        painter.rect_stroke(shown, 0.0, Stroke::new(1.0, ink.gamma_multiply(0.75)), egui::StrokeKind::Inside);
        for (x, part) in [(shown.left(), WindowPart::LeftHandle), (shown.right(), WindowPart::RightHandle)] {
            let on = over_part == Some(part);
            let grip = Rect::from_center_size(
                Pos2::new(x, rect.center().y),
                egui::vec2(if on { 6.0 } else { 4.0 }, (rect.height() * 0.55).max(8.0)),
            );
            painter.rect_filled(grip, 2.0, if on { ink } else { ink.gamma_multiply(0.8) });
            painter.rect_stroke(grip, 2.0, Stroke::new(1.0, theme::INPUT()), egui::StrokeKind::Outside);
        }
    }
    painter.line_segment([rect.left_bottom(), rect.right_bottom()], theme::hairline());

    match over_part {
        Some(WindowPart::LeftHandle | WindowPart::RightHandle) => {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        Some(WindowPart::Body) if grab.is_some() => ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing),
        Some(WindowPart::Body) => ui.ctx().set_cursor_icon(egui::CursorIcon::Grab),
        None if over_bar.is_some() => ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand),
        None => {}
    }
    if let Some(bar) = over_bar {
        let frame = &frames[bar.frame];
        let on_window = over_part == Some(WindowPart::Body);
        let _ = egui::Tooltip::always_open(
            ui.ctx().clone(),
            ui.layer_id(),
            egui::Id::new("perfr-frame-tip"),
            egui::PopupAnchor::Pointer,
        )
        .at_pointer()
        .gap(8.0)
        .show(|ui| {
            ui.label(RichText::new(name).family(fonts::medium()).size(12.0).color(theme::TEXT()));
            ui.label(
                RichText::new(format!("{}  ·  {}", fmt_dur(frame.duration_ns), fmt_fps(frame.duration_ns)))
                    .font(FontId::monospace(11.0))
                    .color(theme::TEXT()),
            );
            ui.label(
                RichText::new(format!(
                    "{:.2}× the median, {}",
                    frame.duration_ns as f64 / scale.median_ns as f64,
                    fmt_dur(scale.median_ns)
                ))
                .font(FontId::monospace(11.0))
                .color(theme::MUTED()),
            );
            let hint = if on_window { "Click: show this frame · Drag: move the view" } else { "Click: show this frame" };
            ui.label(RichText::new(hint).size(10.5).color(theme::MUTED()));
        });
    }
    if resp.clicked() {
        // A click on a handle is not a click on the bar behind it.
        if let Some(bar) = over_bar {
            return Some(StripAction::ShowFrame(frames[bar.frame]));
        }
    }
    action
}

/// A frame time as a rate: what the frame rate would be if every frame took
/// this long.
pub fn fmt_fps(duration_ns: u64) -> String {
    let fps = 1e9 / duration_ns.max(1) as f64;
    if fps >= 100.0 {
        format!("{fps:.0} fps")
    } else {
        format!("{fps:.1} fps")
    }
}

/// Bands thinner than this many points are not drawn; they fade in over
/// the next few. Stripes a pixel or two wide are a texture, not a boundary.
const BAND_MIN_PT: f32 = 2.0;
const BAND_FULL_PT: f32 = 8.0;

/// How strongly the bands are drawn, 0 to 1, when the frames in view are
/// `points_per_frame` wide on average.
pub fn band_strength(points_per_frame: f32) -> f32 {
    ((points_per_frame - BAND_MIN_PT) / (BAND_FULL_PT - BAND_MIN_PT)).clamp(0.0, 1.0)
}

/// Where the rows of process `pid` are, top to bottom: `(top, bottom)` in
/// points from the top of the lanes, rows that touch joined into one span.
///
/// These are the rows the frames of that process are drawn behind: its
/// header, its threads with the lanes under them, their async blocks and
/// graphs. A track of it that was dragged under another process is still
/// its own and is found where it is; a guest in its section is not.
pub fn process_spans(rows: &[TrackRow], pid: u32) -> Vec<(f32, f32)> {
    let mut spans: Vec<(f32, f32)> = rows
        .iter()
        .filter(|row| match row.id {
            RowId::Process(p) => p == pid,
            RowId::Thread(t) | RowId::Async(t) => t.pid == pid,
            // A scheduler lane's key has pid 0 and belongs to no process.
            RowId::Lane(key) => key.pid == pid && !key.is_scheduler(),
            RowId::Scheduler | RowId::Machine(_) => false,
        })
        .filter(|row| row.height > 0.0)
        .map(|row| (row.y, row.y + row.height))
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut joined: Vec<(f32, f32)> = Vec::new();
    for (top, bottom) in spans {
        match joined.last_mut() {
            Some(last) if top <= last.1 + 0.5 => last.1 = last.1.max(bottom),
            _ => joined.push((top, bottom)),
        }
    }
    joined
}

/// Darkens the ground behind every other frame in view, within `spans`
/// (see [`process_spans`]) and nowhere else. A frame's turn comes from its
/// number (`extra`), not from where it sits in the lane, so the bands do
/// not swap when old frames leave the ring.
pub fn paint_bands(ui: &Ui, body: Rect, t0: f64, t1: f64, lane: &Lane, spans: &[(f32, f32)], light: bool) {
    if t1 <= t0 || body.width() < 1.0 {
        return;
    }
    // Only the spans scrolled into view are worth a rectangle a frame.
    let clip = ui.clip_rect().intersect(body);
    let spans: Vec<(f32, f32)> = spans
        .iter()
        .map(|(top, bottom)| ((body.top() + top).max(clip.top()), (body.top() + bottom).min(clip.bottom())))
        .filter(|(top, bottom)| bottom > top)
        .collect();
    if spans.is_empty() {
        return;
    }
    let frames = lane.events();
    let first = lane.first_ending_after(t0.max(0.0) as u64);
    let end = first + frames[first..].partition_point(|e| (e.start_ns as f64) < t1);
    let in_view = &frames[first..end];
    let (Some(head), Some(tail)) = (in_view.first(), in_view.last()) else { return };
    let per_ns = f64::from(body.width()) / (t1 - t0);
    let covered = (tail.end_ns() - head.start_ns) as f64 * per_ns;
    let strength = band_strength(covered as f32 / in_view.len() as f32);
    if strength <= 0.0 {
        return;
    }
    let shade = Color32::from_black_alpha(((if light { 14.0 } else { 46.0 }) * strength) as u8);
    let painter = ui.painter_at(body);
    let x = |ns: u64| body.left() + ((ns as f64 - t0) * per_ns) as f32;
    for frame in in_view.iter().filter(|e| e.extra & 1 == 1) {
        let (left, right) = (x(frame.start_ns).max(body.left()), x(frame.end_ns()).min(body.right()));
        if right <= left {
            continue;
        }
        for (top, bottom) in &spans {
            painter.rect_filled(Rect::from_min_max(Pos2::new(left, *top), Pos2::new(right, *bottom)), 0.0, shade);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(start_ns: u64, duration_ns: u64, number: u8) -> LiveEvent {
        LiveEvent { start_ns, duration_ns, tid: 7, pid: 1, kind: kind::FRAME, depth: 0, extra: number, _pad: 0, name_id: 5 }
    }

    /// `n` frames of `each` ns, back to back from zero.
    fn even(n: u64, each: u64) -> Vec<LiveEvent> {
        (0..n).map(|i| frame(i * each, each, i as u8)).collect()
    }

    #[test]
    fn a_clicked_frame_fills_nine_tenths_of_the_window_and_sits_in_its_middle() {
        let (t0, t1) = frame_window(&frame(1_000_000, 9_000, 0));
        assert!(((t1 - t0) - 10_000.0).abs() < 1e-6, "9 µs is 90% of {}", t1 - t0);
        assert!((t0 - 999_500.0).abs() < 1e-6 && (t1 - 1_009_500.0).abs() < 1e-6, "{t0}..{t1}");
        // A frame of no length still gives a window that can be shown.
        let (t0, t1) = frame_window(&frame(50, 0, 0));
        assert!(t1 > t0);
    }

    #[test]
    fn a_frame_wider_than_a_point_is_one_bar_over_its_columns() {
        // Four frames of 25 over a strip of 100 points for 0..100.
        let frames = even(4, 25);
        let got = bars(&frames, 0.0, 100.0, 100.0);
        assert_eq!(
            got,
            [
                Bar { x0: 0.0, x1: 25.0, frame: 0 },
                Bar { x0: 25.0, x1: 50.0, frame: 1 },
                Bar { x0: 50.0, x1: 75.0, frame: 2 },
                Bar { x0: 75.0, x1: 100.0, frame: 3 },
            ]
        );
        assert_eq!(bar_at(&got, 24.9).map(|b| b.frame), Some(0));
        assert_eq!(bar_at(&got, 25.0).map(|b| b.frame), Some(1));
        assert_eq!(bar_at(&got, 99.5).map(|b| b.frame), Some(3));
        assert_eq!(bar_at(&got, 100.0), None);
    }

    #[test]
    fn a_column_that_holds_several_frames_shows_the_longest_of_them() {
        // 1000 frames of 10 across 100 points: ten to a column. One of them
        // is slow, and its column must show it and not a neighbour.
        let mut frames = even(1_000, 10);
        frames[437].duration_ns = 9; // shorter: never the one shown
        frames[555].duration_ns = 10; // as its neighbours
        let slow = 123;
        // Lengthen it without overlapping the next frame's start.
        frames[slow].start_ns -= 5;
        frames[slow].duration_ns = 15;
        frames[slow - 1].duration_ns = 5;
        let got = bars(&frames, 0.0, 10_000.0, 100.0);
        assert!(got.len() <= 100, "at most a bar a column: {}", got.len());
        let shown = bar_at(&got, 12.5).expect("a bar in column 12");
        assert_eq!(shown.frame, slow);
        assert!(got.iter().all(|b| b.frame != 437));
        // The strip is covered from end to end: no column without a frame.
        let covered: f32 = got.iter().map(|b| b.x1 - b.x0).sum();
        assert_eq!(covered, 100.0);
    }

    #[test]
    fn frames_outside_the_strip_and_a_strip_of_no_width_draw_nothing() {
        let frames = even(4, 25);
        assert!(bars(&frames, 200.0, 300.0, 100.0).is_empty());
        assert!(bars(&frames, 0.0, 100.0, 0.0).is_empty());
        assert!(bars(&frames, 100.0, 100.0, 100.0).is_empty());
        // Half in: the frames that reach the strip are clipped to it.
        let got = bars(&frames, 50.0, 150.0, 100.0);
        assert_eq!(got.first().map(|b| (b.x0, b.frame)), Some((0.0, 2)));
        assert_eq!(got.last().map(|b| (b.x1, b.frame)), Some((50.0, 3)));
    }

    #[test]
    fn one_long_frame_does_not_flatten_the_strip() {
        let mut frames = even(200, 7_000_000);
        frames[90].duration_ns = 500_000_000;
        let s = scale(&frames);
        assert_eq!(s.median_ns, 7_000_000);
        assert_eq!(s.ceiling_ns, 21_000_000, "three medians, not the half second");
        assert_eq!(pace(7_000_000, s.median_ns), Pace::Even);
        assert_eq!(pace(12_000_000, s.median_ns), Pace::Slow);
        assert_eq!(pace(500_000_000, s.median_ns), Pace::Hitch);
        // A steady capture is not stretched to three medians either.
        assert_eq!(scale(&even(50, 7_000_000)).ceiling_ns, 7_000_000);
        assert_eq!(scale(&[]), Scale::default());
    }

    #[test]
    fn the_strip_is_recomputed_only_when_something_it_shows_changed() {
        let mut lane = Lane::default();
        for f in even(10, 100) {
            lane.insert(f);
        }
        let key = frame(0, 1, 0).lane_key();
        let mut memo = StripMemo::default();
        let first = memo.get(key, &lane, 0.0, 1_000.0, 200.0).0.to_vec();
        assert_eq!(first.len(), 10);
        // Marked so a recomputation would show.
        memo.bars.clear();
        assert!(memo.get(key, &lane, 0.0, 1_000.0, 200.0).0.is_empty(), "the same question was answered again");
        lane.insert(frame(1_000, 100, 10));
        assert_eq!(memo.get(key, &lane, 0.0, 1_100.0, 200.0).0.len(), 11);
    }

    #[test]
    fn the_main_series_comes_first_and_then_the_busiest() {
        let mut intern = InternTable::default();
        intern.insert_id(5, "physics");
        intern.insert_id(6, "frame");
        intern.insert_id(7, "render");
        let mut index = TrackIndex::default();
        for i in 0..9u64 {
            index.insert(LiveEvent { name_id: 5, ..frame(i * 10, 10, i as u8) });
        }
        for i in 0..3u64 {
            index.insert(LiveEvent { name_id: 6, ..frame(i * 30, 30, i as u8) });
        }
        for i in 0..5u64 {
            index.insert(LiveEvent { name_id: 7, ..frame(i * 20, 20, i as u8) });
        }
        // Something that is not a frame.
        index.insert(LiveEvent { kind: kind::API_SCOPE, ..frame(0, 50, 0) });
        fn names<'a>(index: &TrackIndex, intern: &'a InternTable, prefer: Option<u32>) -> Vec<(u32, &'a str)> {
            series(index, intern, prefer)
                .into_iter()
                .map(|key| (key.pid, series_name(index.lane(key).unwrap(), intern)))
                .collect()
        }
        assert_eq!(names(&index, &intern, None), [(1, "frame"), (1, "physics"), (1, "render")]);

        // A second program on the machine that marks more frames than the
        // one being captured: the capture's own series still come first.
        for i in 0..50u64 {
            index.insert(LiveEvent { name_id: 6, pid: 2, ..frame(i * 5, 5, i as u8) });
        }
        assert_eq!(names(&index, &intern, None), [(2, "frame"), (1, "frame"), (1, "physics"), (1, "render")]);
        assert_eq!(names(&index, &intern, Some(1)), [(1, "frame"), (1, "physics"), (1, "render"), (2, "frame")]);

        // Each process's bands come from one series of its own: the one the
        // strip is showing for its process, the first of its series for the
        // others.
        let all = series(&index, &intern, Some(1));
        let pids_and_names = |keys: Vec<LaneKey>| -> Vec<(u32, &str)> {
            keys.into_iter().map(|key| (key.pid, series_name(index.lane(key).unwrap(), &intern))).collect()
        };
        assert_eq!(pids_and_names(band_series(&all, None)), [(1, "frame"), (2, "frame")]);
        assert_eq!(pids_and_names(band_series(&all, Some(all[1]))), [(1, "physics"), (2, "frame")]);
    }

    #[test]
    fn the_window_is_drawn_where_the_timeline_is_and_never_too_narrow_to_hold() {
        // A strip of 1000 points for 0..10 s, the timeline on 2 s..4 s.
        let cap = (0.0, 10e9);
        assert_eq!(window_edges((2e9, 4e9), cap, 1000.0), Some((200.0, 400.0)));
        // One 14 ms frame of it is 1.4 points: drawn ten wide, about its middle.
        let (left, right) = window_edges((5e9, 5.014e9), cap, 1000.0).unwrap();
        assert!((right - left - WINDOW_MIN_PT).abs() < 1e-3);
        assert!(((left + right) / 2.0 - 500.7).abs() < 1e-2);
        // At the capture's ends it stays on the strip.
        assert_eq!(window_edges((0.0, 1e6), cap, 1000.0), Some((0.0, WINDOW_MIN_PT)));
        assert_eq!(window_edges((10e9 - 1e6, 10e9), cap, 1000.0), Some((1000.0 - WINDOW_MIN_PT, 1000.0)));
        // A view that reaches past the capture is cut to it; one wholly
        // outside it, or a strip with nothing to stand for, has no window.
        assert_eq!(window_edges((-5e9, 5e9), cap, 1000.0), Some((0.0, 500.0)));
        assert_eq!(window_edges((11e9, 12e9), cap, 1000.0), None);
        assert_eq!(window_edges((2e9, 4e9), (0.0, 0.0), 1000.0), None);
        assert_eq!(window_edges((2e9, 4e9), cap, 0.0), None);
    }

    #[test]
    fn a_handle_reaches_outside_its_edge_and_leaves_a_narrow_window_a_middle() {
        use WindowPart::*;
        let at = |x: f32| window_part_at(x, 200.0, 400.0);
        assert_eq!(at(192.0), None);
        assert_eq!(at(193.5), Some(LeftHandle));
        assert_eq!(at(203.0), Some(LeftHandle));
        assert_eq!(at(204.5), Some(Body));
        assert_eq!(at(300.0), Some(Body));
        assert_eq!(at(397.0), Some(RightHandle));
        assert_eq!(at(406.5), Some(RightHandle));
        assert_eq!(at(408.0), None);
        // The narrowest window: both handles, and still a middle to drag.
        let narrow = |x: f32| window_part_at(x, 500.0, 510.0);
        assert_eq!(narrow(501.0), Some(LeftHandle));
        assert_eq!(narrow(505.0), Some(Body));
        assert_eq!(narrow(509.0), Some(RightHandle));
    }

    #[test]
    fn dragging_the_window_pans_and_dragging_a_handle_moves_only_its_edge() {
        let cap = (0.0, 10_000.0);
        let view = (2_000.0, 4_000.0);
        // The window taken by a point 500 into it, and moved 1500 later.
        let pan = Grab::begin(Some(WindowPart::Body), 2_500.0, view);
        assert_eq!(pan.window(2_500.0, view, cap, 100.0), view, "nothing moves until the pointer does");
        assert_eq!(pan.window(4_000.0, view, cap, 100.0), (3_500.0, 5_500.0));
        assert!(!pan.resizes());
        // It stops at the capture's ends, keeping its length.
        assert_eq!(pan.window(-900.0, view, cap, 100.0), (0.0, 2_000.0));
        assert_eq!(pan.window(99_000.0, view, cap, 100.0), (8_000.0, 10_000.0));

        // The right handle, taken 30 outside the edge: the edge follows the
        // pointer at that distance and the left edge stays.
        let right = Grab::begin(Some(WindowPart::RightHandle), 4_030.0, view);
        assert!(right.resizes());
        assert_eq!(right.window(4_030.0, view, cap, 100.0), view);
        assert_eq!(right.window(6_030.0, view, cap, 100.0), (2_000.0, 6_000.0));
        assert_eq!(right.window(50_000.0, view, cap, 100.0), (2_000.0, 10_000.0), "not past the capture's end");
        assert_eq!(right.window(0.0, view, cap, 100.0), (2_000.0, 2_100.0), "not past the other edge");

        let left = Grab::begin(Some(WindowPart::LeftHandle), 1_990.0, view);
        assert_eq!(left.window(990.0, view, cap, 100.0), (1_000.0, 4_000.0));
        assert_eq!(left.window(-5_000.0, view, cap, 100.0), (0.0, 4_000.0));
        assert_eq!(left.window(9_000.0, view, cap, 100.0), (3_900.0, 4_000.0));

        // A drag that begins off the window brings the window to the
        // pointer, by its middle, and goes on from there.
        let jump = Grab::begin(None, 7_000.0, view);
        assert_eq!(jump.window(7_000.0, view, cap, 100.0), (6_000.0, 8_000.0));
    }

    #[test]
    fn frames_are_drawn_behind_the_rows_of_their_own_process_and_no_others() {
        use crate::tracks::ThreadId;
        let row = |id: RowId, y: f32, height: f32| TrackRow { id, y, height };
        let thread = |pid: u32, tid: u32| RowId::Thread(ThreadId { pid, tid });
        let graph = |pid: u32| RowId::Lane(LiveEvent { kind: kind::VALUE, pid, ..frame(0, 1, 0) }.lane_key());
        let rows = [
            row(RowId::Scheduler, 0.0, 20.0),
            row(RowId::Lane(perfr_live_event::LaneKey::scheduler(0)), 20.0, 13.0),
            row(RowId::Process(7), 33.0, 18.0),
            row(thread(7, 70), 51.0, 80.0),
            row(RowId::Async(ThreadId { pid: 7, tid: 70 }), 131.0, 21.0),
            // A guest in 7's section: another process's graph.
            row(graph(9), 152.0, 39.0),
            row(thread(7, 71), 191.0, 60.0),
            row(RowId::Process(9), 251.0, 18.0),
            row(thread(9, 90), 269.0, 40.0),
            // A track of 7 that was dragged under process 9.
            row(graph(7), 309.0, 39.0),
        ];
        assert_eq!(process_spans(&rows, 7), [(33.0, 152.0), (191.0, 251.0), (309.0, 348.0)]);
        assert_eq!(process_spans(&rows, 9), [(152.0, 191.0), (251.0, 309.0)]);
        // The scheduler's lanes are keyed with pid 0, and are nobody's.
        assert!(process_spans(&rows, 0).is_empty());
        assert!(process_spans(&rows, 1234).is_empty());
    }

    #[test]
    fn bands_fade_out_before_they_are_too_thin_to_be_boundaries() {
        assert_eq!(band_strength(1.0), 0.0);
        assert_eq!(band_strength(BAND_MIN_PT), 0.0);
        assert!(band_strength(5.0) > 0.0 && band_strength(5.0) < 1.0);
        assert_eq!(band_strength(BAND_FULL_PT), 1.0);
        assert_eq!(band_strength(400.0), 1.0);
    }
}
