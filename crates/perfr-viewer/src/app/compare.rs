// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Compare tab: what changed between the capture on screen and
//! another one, as the service works it out (`POST /api/diff`).
//!
//! The viewer picks the other capture and how to count, sends the file,
//! and draws the answer. It computes nothing of it: the figures, the
//! intervals and the verdicts are `perfr-diff`'s, the same ones
//! `perfr diff` prints.

use std::sync::Arc;

use perfr_diff::text::{duration, headline, percent, q_value, tree_lines, Units};
use perfr_diff::{Diff, FunctionRow, Metric, ScopeRow, SpanKind, TreeNode, Verdict};

use super::*;

/// What time is counted against, as the tab offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ComparePer {
    Second,
    Capture,
    Frame,
    Sample,
}

impl ComparePer {
    const ALL: [ComparePer; 4] = [ComparePer::Second, ComparePer::Capture, ComparePer::Frame, ComparePer::Sample];

    fn label(self) -> &'static str {
        match self {
            ComparePer::Second => "per second",
            ComparePer::Capture => "per capture",
            ComparePer::Frame => "per frame",
            ComparePer::Sample => "shares",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            ComparePer::Second => "Time per second of capture: for a program that does the same work every second in both.",
            ComparePer::Capture => "Time per capture: for two captures of the same amount of work, start to end.",
            ComparePer::Frame => "Time per frame, for a program that marks its frames.",
            ComparePer::Sample => "Each function as a share of all samples. Shares add up to 100%: what takes more makes the rest look smaller.",
        }
    }

    /// What the service is told.
    fn query(self) -> &'static str {
        match self {
            ComparePer::Second => "second",
            ComparePer::Capture => "capture",
            ComparePer::Frame => "frame",
            ComparePer::Sample => "sample",
        }
    }
}

/// The capture this one is compared with, kept so that the question can
/// be asked again another way.
#[derive(Clone)]
pub(super) enum Baseline {
    /// Read from disk by the desktop build. A browser hands over a `File`.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Bytes(Arc<Vec<u8>>),
    /// In a browser the file is read when it is sent.
    #[cfg(target_arch = "wasm32")]
    File(web_sys::File),
}

pub(super) struct CompareState {
    pub(super) baseline: Option<(String, Baseline)>,
    pub(super) per: ComparePer,
    /// The capture on screen is the base, and the file is what is judged.
    pub(super) this_is_base: bool,
    /// Rows without a verdict are left out.
    pub(super) changed_only: bool,
    /// A question is with the service.
    pub(super) pending: bool,
    pub(super) error: String,
    pub(super) diff: Option<Diff>,
    /// The file the browser's picker gave, until the next frame takes it.
    #[cfg(target_arch = "wasm32")]
    picked: chrome_load::PendingFile,
    /// The queries asked, in order.
    #[cfg(test)]
    pub(super) asked: Vec<String>,
}

impl Default for CompareState {
    fn default() -> CompareState {
        CompareState {
            baseline: None,
            per: ComparePer::Second,
            this_is_base: false,
            changed_only: true,
            pending: false,
            error: String::new(),
            diff: None,
            #[cfg(target_arch = "wasm32")]
            picked: chrome_load::new_pending_file(),
            #[cfg(test)]
            asked: Vec::new(),
        }
    }
}

/// One row of the tab's list. Text is made for the rows on screen only.
enum Line<'a> {
    Heading(String),
    Plain(String, Color32),
    Columns,
    Cpu(&'a Metric),
    Function(&'a FunctionRow, bool),
    Scope(&'a ScopeRow),
    Tree(&'a TreeNode, usize, usize),
}

fn verdict_color(verdict: Verdict) -> Color32 {
    match verdict {
        Verdict::Regressed => LINK_RED,
        Verdict::Improved => LINK_GREEN,
        Verdict::Unchanged | Verdict::Untested => theme::MUTED(),
    }
}

/// The mark of a verdict, drawn: the viewer's font has no triangles. Up
/// and red is slower, down and green is faster, a dash is tested and not
/// changed, and nothing is not tested.
fn paint_mark(painter: &egui::Painter, center: Pos2, verdict: Verdict) {
    let color = verdict_color(verdict);
    let (half, rise) = (4.0, 3.5);
    let triangle = |tip: f32| {
        vec![
            Pos2::new(center.x, center.y + tip * rise),
            Pos2::new(center.x - half, center.y - tip * rise),
            Pos2::new(center.x + half, center.y - tip * rise),
        ]
    };
    match verdict {
        Verdict::Regressed => {
            painter.add(egui::Shape::convex_polygon(triangle(-1.0), color, egui::Stroke::NONE));
        }
        Verdict::Improved => {
            painter.add(egui::Shape::convex_polygon(triangle(1.0), color, egui::Stroke::NONE));
        }
        Verdict::Unchanged => {
            painter.line_segment(
                [Pos2::new(center.x - half, center.y), Pos2::new(center.x + half, center.y)],
                egui::Stroke::new(1.5, color),
            );
        }
        Verdict::Untested => {}
    }
}

fn signed(delta: f64, text: String) -> String {
    if delta >= 0.0 {
        format!("+{text}")
    } else {
        text
    }
}

fn interval(ci: Option<(f64, f64)>) -> String {
    ci.map_or(String::new(), |(low, high)| format!("[{}, {}]", percent(low), percent(high)))
}

/// The cells of a row with a metric: base, new, change, interval, q.
fn metric_cells(metric: &Metric, value: impl Fn(f64) -> String) -> [String; 5] {
    let change = match metric.change {
        Some(change) => percent(change),
        None if metric.new > 0.0 => "new".to_string(),
        None => String::new(),
    };
    [value(metric.base), value(metric.new), change, interval(metric.change_ci), q_value(metric.q)]
}

fn scope_label(row: &ScopeRow) -> String {
    match row.kind {
        SpanKind::Scope => row.name.clone(),
        kind => format!("{} [{}]", row.name, kind.label()),
    }
}

impl PerfrLiveApp {
    /// How the service is asked: `POST /api/diff?<this>`.
    fn compare_query(&self) -> String {
        let name = self.compare.baseline.as_ref().map_or("", |(name, _)| name.as_str());
        let mut query = format!("name={}&per={}", crate::net::percent_encode(name), self.compare.per.query());
        if self.compare.this_is_base {
            query.push_str("&current=base");
        }
        query
    }

    /// Sends the baseline to be compared with the capture on screen.
    pub(super) fn request_compare(&mut self) {
        let Some((_, baseline)) = self.compare.baseline.clone() else {
            return;
        };
        let query = self.compare_query();
        self.compare.pending = true;
        self.compare.error.clear();
        #[cfg(test)]
        self.compare.asked.push(query.clone());
        match baseline {
            Baseline::Bytes(bytes) => self.net.diff_capture(bytes, query),
            #[cfg(target_arch = "wasm32")]
            Baseline::File(file) => self.net.diff_capture_file(file, query),
        }
        self.needs_repaint = true;
    }

    pub(super) fn set_baseline(&mut self, name: String, baseline: Baseline) {
        self.compare.baseline = Some((name, baseline));
        self.compare.diff = None;
        self.request_compare();
    }

    /// The service's answer, or why there is none.
    pub(super) fn apply_compare(&mut self, answer: Result<Diff, String>) {
        self.compare.pending = false;
        match answer {
            Ok(diff) => {
                self.compare.error.clear();
                self.compare.diff = Some(diff);
            }
            Err(error) => {
                // What was on screen answered another question.
                self.compare.diff = None;
                self.compare.error = error;
            }
        }
        self.needs_repaint = true;
    }

    fn pick_baseline(&mut self) {
        #[cfg(target_arch = "wasm32")]
        chrome_load::start_baseline_dialog(&self.compare.picked);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = chrome_load::start_baseline_dialog() {
            let name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
            match std::fs::read(&path) {
                Ok(bytes) => self.set_baseline(name, Baseline::Bytes(Arc::new(bytes))),
                Err(error) => self.compare.error = format!("{}: {error}", path.display()),
            }
        }
    }

    /// Takes the file the browser's picker gave, once it has.
    pub(super) fn pump_baseline(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            let file = self.compare.picked.lock().ok().and_then(|mut picked| picked.take());
            if let Some(file) = file {
                self.set_baseline(file.name(), Baseline::File(file));
            }
        }
    }

    /// The tab: its controls, then the comparison as one list.
    pub(super) fn compare_panel(&mut self, ui: &mut Ui) {
        let font = self.ui_tweaks.report_font;
        if !self.has_service() {
            ui.label(RichText::new("Comparing needs the service: it reads both captures.").color(theme::MUTED()).size(font));
            return;
        }
        ui.horizontal_wrapped(|ui| {
            if pill(ui, "Baseline…", false).on_hover_text("Pick the capture to compare this one with: a .perfr.zip").clicked() {
                self.pick_baseline();
            }
            match &self.compare.baseline {
                Some((name, _)) => {
                    ui.label(RichText::new(name).color(theme::TEXT()).size(font));
                    if pill(ui, "Compare again", false).on_hover_text("Ask again: the capture on screen has changed").clicked() {
                        self.request_compare();
                    }
                }
                None => {
                    ui.label(RichText::new("No baseline yet.").color(theme::MUTED()).size(font));
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            for per in ComparePer::ALL {
                if pill(ui, per.label(), self.compare.per == per).on_hover_text(per.hint()).clicked() && self.compare.per != per {
                    self.compare.per = per;
                    self.request_compare();
                }
            }
        });
        ui.horizontal_wrapped(|ui| {
            let swap = pill(ui, "This capture is the base", self.compare.this_is_base)
                .on_hover_text("Judge the file against the capture on screen, instead of the capture on screen against the file");
            if swap.clicked() {
                self.compare.this_is_base = !self.compare.this_is_base;
                self.request_compare();
            }
            if pill(ui, "Changed only", self.compare.changed_only).on_hover_text("Leave out the rows without a verdict").clicked() {
                self.compare.changed_only = !self.compare.changed_only;
            }
        });
        ui.add_space(2.0);
        if !self.compare.error.is_empty() {
            ui.label(RichText::new(&self.compare.error).color(LINK_RED).size(font));
        }
        if self.compare.pending {
            ui.label(RichText::new("Comparing…").color(theme::MUTED()).size(font));
        }
        if self.compare.diff.is_none() {
            if self.compare.baseline.is_none() {
                let help = "Pick a saved capture (.perfr.zip) to see what changed between it and this one: \
                            every sampled function, scope and frame series with its value in both, the interval \
                            of the difference, and a verdict that takes chance into account.";
                ui.label(RichText::new(help).color(theme::MUTED()).size(font));
            }
            return;
        }
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .scroll_source(egui::scroll_area::ScrollSource { drag: false, ..Default::default() })
            .show(ui, |ui| self.compare_rows(ui));
    }

    fn compare_rows(&mut self, ui: &mut Ui) {
        let Some(diff) = self.compare.diff.take() else {
            return;
        };
        let clicked = self.compare_list(ui, &diff);
        self.compare.diff = Some(diff);
        if let Some(name) = clicked {
            // The same function in this capture's own report.
            self.report_filter = name;
            self.open_right_tab(ReportTab::Flat);
        }
    }

    /// Draws the list; returns the function whose row was clicked.
    fn compare_list(&mut self, ui: &mut Ui, diff: &Diff) -> Option<String> {
        let font = self.ui_tweaks.report_font;
        let row_h = font + self.ui_tweaks.report_row_gap + 6.0;
        let units = Units::new(diff);
        let filter = self.report_filter.trim().to_lowercase();
        let named = |name: &str, module: &str| filter.is_empty() || contains_ci(name, &filter) || contains_ci(module, &filter);
        let changed_only = self.compare.changed_only;

        let mut lines: Vec<Line<'_>> = Vec::new();
        let verdict = if diff.regressed() {
            Verdict::Regressed
        } else if diff.outcome.improved > 0 {
            Verdict::Improved
        } else {
            Verdict::Unchanged
        };
        lines.push(Line::Plain(headline(diff), if verdict == Verdict::Unchanged { theme::TEXT() } else { verdict_color(verdict) }));
        for (side, info) in [("base", &diff.base), ("new", &diff.new)] {
            let captures = match info.captures.as_slice() {
                [one] => one.clone(),
                many => format!("{} captures", many.len()),
            };
            lines.push(Line::Plain(
                format!("{side}: {captures}, {:.2} s, {} samples, {} spans", info.seconds, fmt_int(info.samples), fmt_int(info.spans)),
                theme::MUTED(),
            ));
        }
        if let Some(cpu) = &diff.cpu {
            lines.push(Line::Heading("All sampled time".to_string()));
            lines.push(Line::Columns);
            lines.push(Line::Cpu(cpu));
        }
        if !diff.functions.is_empty() {
            lines.push(Line::Heading(format!("Functions, their own time ({})", units.function_heading())));
            lines.push(Line::Columns);
            let before = lines.len();
            lines.extend(
                diff.functions
                    .iter()
                    .filter(|row| named(&row.name, &row.module))
                    .filter(|row| if changed_only { row.self_time.changed() } else { row.self_samples != (0, 0) })
                    .map(|row| Line::Function(row, false)),
            );
            if lines.len() == before {
                lines.push(Line::Plain("No function's own time changed.".to_string(), theme::MUTED()));
            }
            // A caller is listed for what it calls only when nothing
            // changed in the function itself.
            let callers: Vec<Line<'_>> = diff
                .functions
                .iter()
                .filter(|row| named(&row.name, &row.module) && row.inclusive.changed() && !row.self_time.changed())
                .map(|row| Line::Function(row, true))
                .collect();
            if !callers.is_empty() {
                lines.push(Line::Heading(format!("Functions, with what they call ({})", units.function_heading())));
                lines.push(Line::Columns);
                lines.extend(callers);
            }
        }
        if !diff.scopes.is_empty() {
            lines.push(Line::Heading("Scopes (mean time per call)".to_string()));
            lines.push(Line::Columns);
            let before = lines.len();
            lines.extend(
                diff.scopes
                    .iter()
                    .filter(|row| named(&row.name, row.kind.label()))
                    .filter(|row| !changed_only || row.mean.changed() || row.total.changed() || row.kind == SpanKind::Frame)
                    .map(Line::Scope),
            );
            if lines.len() == before {
                lines.push(Line::Plain("No scope changed.".to_string(), theme::MUTED()));
            }
        }
        if let Some(tree) = &diff.tree {
            let (tree, cut) = tree_lines(tree, 400);
            if !tree.is_empty() {
                lines.push(Line::Heading(format!("Where in the call tree ({}, with callees)", units.function_heading())));
                lines.extend(tree.iter().map(|line| Line::Tree(line.node, line.depth, line.folded)));
                if cut {
                    lines.push(Line::Plain("…".to_string(), theme::MUTED()));
                }
            }
        }

        // mark, base, new, change, interval, q, then the name to the edge.
        let widths = [14.0, 74.0, 74.0, 58.0, 116.0, 44.0];
        let gap = self.ui_tweaks.report_col_gap;
        let fixed: f32 = widths.iter().sum::<f32>() + gap * widths.len() as f32;
        // As wide as the panel shows, and scrolled sideways when a name
        // needs more than what the figures leave.
        let clip = ui.clip_rect();
        let avail_w = clip.width().max(fixed + 200.0);
        let top = ui.cursor().top();
        let pitch = RowPitch::new(ui, row_h);
        let (first, last) = pitch.in_view(clip, top, lines.len());
        pitch.skip(ui, avail_w, first);
        let mut clicked = None;
        for (n, line) in lines[first..last].iter().enumerate() {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(avail_w, row_h), Sense::hover());
            let painter = ui.painter();
            let text = |x: f32, align: Align2, text: &str, size: f32, color: Color32| -> f32 {
                painter.text(Pos2::new(x, rect.center().y), align, text, FontId::new(size, FontFamily::Proportional), color).right()
            };
            let mark_at = Pos2::new(rect.left() + 7.0, rect.center().y);
            // The right edge of each fixed column, and where the name starts.
            let mut edges = [0.0; 6];
            let mut x = rect.left();
            for (edge, width) in edges.iter_mut().zip(widths) {
                x += width;
                *edge = x;
                x += gap;
            }
            let name_x = x;
            // A name, and after it something quieter, in what is left of
            // the row.
            let named_row = |name: &str, after: &str| {
                let room = rect.right() - name_x - 6.0;
                let after_w = (after.chars().count() as f32 * (font - 0.5) * 0.58 + 10.0).min(room * 0.4);
                let shown = truncate_to_width(name, (room - after_w).max(60.0), font);
                let end = text(name_x, Align2::LEFT_CENTER, &shown, font, theme::TEXT());
                text(end + 10.0, Align2::LEFT_CENTER, &truncate_to_width(after, after_w, font - 0.5), font - 0.5, theme::MUTED());
            };
            let cells = |metric: &Metric, cells: &[String; 5]| {
                let color = verdict_color(metric.verdict);
                paint_mark(painter, mark_at, metric.verdict);
                text(edges[1], Align2::RIGHT_CENTER, &cells[0], font, theme::TEXT());
                text(edges[2], Align2::RIGHT_CENTER, &cells[1], font, theme::TEXT());
                text(edges[3], Align2::RIGHT_CENTER, &cells[2], font, if metric.changed() { color } else { theme::TEXT() });
                text(edges[3] + gap, Align2::LEFT_CENTER, &cells[3], font - 0.5, theme::MUTED());
                text(edges[5], Align2::RIGHT_CENTER, &cells[4], font - 0.5, theme::MUTED());
            };
            match line {
                Line::Heading(title) => {
                    text(rect.left(), Align2::LEFT_CENTER, title, font + 0.5, theme::TEXT());
                }
                Line::Plain(words, color) => {
                    text(rect.left(), Align2::LEFT_CENTER, words, font, *color);
                }
                Line::Columns => {
                    for (edge, title) in [(edges[1], "base"), (edges[2], "new"), (edges[3], "change"), (edges[5], "q")] {
                        text(edge, Align2::RIGHT_CENTER, title, font - 0.5, theme::MUTED());
                    }
                    text(edges[3] + gap, Align2::LEFT_CENTER, "interval", font - 0.5, theme::MUTED());
                }
                Line::Cpu(cpu) => {
                    cells(cpu, &metric_cells(cpu, |value| units.cpu_value(value)));
                }
                Line::Function(row, inclusive) => {
                    if (first + n) % 2 == 1 {
                        painter.rect_filled(rect, 0.0, theme::REPORT_ROW_ALT());
                    }
                    let metric = if *inclusive { &row.inclusive } else { &row.self_time };
                    cells(metric, &metric_cells(metric, |value| units.function_value(value)));
                    named_row(&row.name, &row.module);
                    note_ui_rect(&format!("compare:function:{}", row.name), rect);
                    let hover = ui.interact(rect, ui.id().with(("compare-row", first + n)), Sense::click());
                    let hover = hover.on_hover_ui(|ui| {
                        ui.label(format!("{}  [{}]", row.name, row.module));
                        ui.label(format!("{} samples of its own in the base, {} in the new", row.self_samples.0, row.self_samples.1));
                        ui.label(format!("{} with what it calls, then {}", row.inclusive_samples.0, row.inclusive_samples.1));
                        for (title, metric) in [("its own", &row.self_time), ("with callees", &row.inclusive)] {
                            ui.label(format!(
                                "{title}: {} to {} ({})",
                                units.function_value(metric.base),
                                units.function_value(metric.new),
                                units.function_delta(metric.delta)
                            ));
                        }
                    });
                    if hover.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        clicked = Some(row.name.clone());
                    }
                }
                Line::Scope(row) => {
                    if (first + n) % 2 == 1 {
                        painter.rect_filled(rect, 0.0, theme::REPORT_ROW_ALT());
                    }
                    let per_call = |ns: f64, calls: u64| if calls == 0 { "-".to_string() } else { duration(ns) };
                    let mut shown = metric_cells(&row.mean, duration);
                    shown[0] = per_call(row.mean.base, row.calls.0);
                    shown[1] = per_call(row.mean.new, row.calls.1);
                    if row.calls.0 == 0 || row.calls.1 == 0 {
                        shown[2] = if row.calls.0 == 0 { "new" } else { "gone" }.to_string();
                    }
                    // A scope that is new, gone or called more often has
                    // its verdict in what it costs in all.
                    let worst = if row.mean.changed() { &row.mean } else { &row.total };
                    cells(&Metric { verdict: worst.verdict, ..row.mean }, &shown);
                    let calls = if row.calls.0 == row.calls.1 {
                        format!("{} calls", fmt_int(row.calls.0))
                    } else {
                        format!("{} then {} calls", fmt_int(row.calls.0), fmt_int(row.calls.1))
                    };
                    named_row(&scope_label(row), &calls);
                    note_ui_rect(&format!("compare:scope:{}", row.name), rect);
                    ui.interact(rect, ui.id().with(("compare-row", first + n)), Sense::hover()).on_hover_ui(|ui| {
                        ui.label(scope_label(row));
                        for (title, quantile) in [("median", &row.median), ("95th percentile", &row.p95)] {
                            ui.label(format!(
                                "{title}: {} to {} {} {}",
                                per_call(quantile.base, row.calls.0),
                                per_call(quantile.new, row.calls.1),
                                quantile.change.map_or(String::new(), percent),
                                interval(quantile.change_ci)
                            ));
                        }
                        let total = &row.total;
                        ui.label(format!(
                            "in all: {} to {} {} {}",
                            units.total_value(total.base),
                            units.total_value(total.new),
                            total.change.map_or(String::new(), percent),
                            interval(total.change_ci)
                        ));
                    });
                }
                Line::Tree(node, depth, folded) => {
                    let metric = &node.inclusive;
                    let color = verdict_color(metric.verdict);
                    paint_mark(painter, mark_at, metric.verdict);
                    let delta = signed(metric.delta, units.function_delta(metric.delta).trim_start_matches('+').to_string());
                    text(edges[2], Align2::RIGHT_CENTER, &delta, font, if metric.changed() { color } else { theme::TEXT() });
                    let indent = edges[2] + gap + *depth as f32 * 10.0;
                    let more = match folded {
                        0 => String::new(),
                        1 => "  … 1 more frame".to_string(),
                        n => format!("  … {n} more frames"),
                    };
                    let width = (rect.right() - indent).max(120.0);
                    text(indent, Align2::LEFT_CENTER, &truncate_to_width(&format!("{}{more}", node.name), width, font), font, theme::TEXT());
                    note_ui_rect(&format!("compare:tree:{}", node.name), rect);
                }
            }
        }
        pitch.skip(ui, avail_w, lines.len() - last);

        ui.add_space(6.0);
        let method = &diff.method;
        let reading = format!(
            "Red and pointing up is slower, green and pointing down is faster, a dash is tested and not changed. \
             Intervals are at {:.0}% confidence; q is the p-value adjusted for every comparison made here, and a \
             verdict needs q at or under {:.2} and a change of at least {:.0}%. Rows under {:.1}% of both captures \
             are not tested.",
            method.confidence * 100.0,
            1.0 - method.confidence,
            method.threshold * 100.0,
            method.min_share * 100.0
        );
        // The list scrolls sideways; the notes wrap at what the panel shows.
        ui.allocate_ui(Vec2::new(clip.width() - 12.0, 0.0), |ui| {
            ui.set_max_width(clip.width() - 12.0);
            for note in std::iter::once(&reading).chain(&method.notes) {
                ui.add(egui::Label::new(RichText::new(note).color(theme::MUTED()).size(font - 0.5)).wrap());
            }
        });
        clicked
    }
}
