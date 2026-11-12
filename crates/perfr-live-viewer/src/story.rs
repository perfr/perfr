// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Playback and a small editor for a capture story.
//!
//! A story is a [`perfr_story::Story`]. Play moves the camera between
//! steps. Overlays are drawn from capture coordinates, so they stay on
//! the same timestamp and track when the page is resized or zoomed.

use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, Context, FontId, Key, Pos2, Rect, RichText, Sense,
    Stroke, TextFormat, Ui, Vec2,
};
use perfr_live_event::{kind, InternTable, LaneKey};
use perfr_live_render::{lane_height, TrackIndex};
use perfr_story::{
    ease_smoothstep, overlay_reveal, Anchor, Camera, Fit, Highlight, Overlay, Player, Scroll,
    Selection, Step, StepKind, Story, TrackRef, View, FORMAT,
};

use crate::fonts;
use crate::theme;
use crate::tracks::{RowId, TrackStrip};

/// One rail row the story can anchor to. `y` and `bottom` are in rail points,
/// the same space as [`TrackStrip::rows`].
#[derive(Clone)]
struct Band {
    track: TrackRef,
    y: f32,
    /// Following lane rows folded into this band, so a thread highlight
    /// covers its scopes and not only the header.
    bottom: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Arrow,
    Rect,
    Callout,
    Highlight,
    Code,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select",
            Tool::Arrow => "Arrow",
            Tool::Rect => "Rect",
            Tool::Callout => "Text",
            Tool::Highlight => "Highlight",
            Tool::Code => "Code",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub time_ns: u64,
    pub track: TrackRef,
    pub y: f32,
}

struct Drag {
    start: Hit,
    current: Hit,
}

/// What the bar asked the viewer to do with its own state.
#[derive(Clone, Debug)]
pub enum StoryCmd {
    AddStep,
    /// Open the Share dialog. Only from a compact bar, which has no
    /// transport bar above it to carry the button.
    Share,
    /// Enter (`true`) or leave fullscreen. Compact bar only, as `Share`.
    Fullscreen(bool),
}

/// The two controls a compact page (`?chrome=compact`, an iframe) has lost
/// with its transport bar, which the story bar draws at its right end. The
/// viewer owns their state and gets a [`StoryCmd`] back for a click.
#[derive(Clone, Copy, Default)]
pub struct BarExtras<'a> {
    /// `(enabled, hover text)`. `None` where there is nothing to post to.
    pub share: Option<(bool, &'a str)>,
    /// `(on, hover text)`. `None` hides the button.
    pub fullscreen: Option<(bool, &'a str)>,
}

/// What a bar control looks like: a glyph painted from lines and triangles,
/// so it does not depend on what the embedded font happens to carry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Glyph {
    Play,
    Pause,
    Prev,
    Next,
    Expand,
    Collapse,
}

/// Camera pose the viewer should apply this frame, if the story still owns it.
pub struct Drive {
    pub camera: Option<Camera>,
    /// Collapse (`true`) or expand (`false`) these tracks. Present only on
    /// the frame a step is entered.
    pub tracks: Option<Vec<(TrackRef, bool)>>,
}

pub struct StorySession {
    pub doc: Option<Story>,
    player: Player,
    pub error: String,
    pub open: bool,
    pub compact: bool,
    editing: bool,
    tool: Tool,
    list_open: bool,
    export_open: bool,
    export_text: String,
    import_text: String,
    callout_text: String,
    code_file: String,
    code_line: String,
    code_snippet: String,
    drag: Option<Drag>,
    pending_overlay: Option<Overlay>,
    pending_fetch: Option<String>,
    /// Apply the next step's camera without a move. Used for the first
    /// frame, before there is a view to ease out of.
    snap: bool,
    retarget: bool,
    /// The user panned or zoomed. The story stops driving until the next step.
    released: bool,
    hold: bool,
    reveal: f32,
    posed: Option<Camera>,
    applied: Option<(usize, u64, u64)>,
    capture_fetched: bool,
}

impl StorySession {
    pub fn from_query(story: Option<&str>, compact: bool) -> Self {
        let mut session = Self {
            doc: None,
            player: Player::default(),
            error: String::new(),
            open: compact,
            compact,
            editing: false,
            tool: Tool::Select,
            list_open: false,
            export_open: false,
            export_text: String::new(),
            import_text: String::new(),
            callout_text: String::new(),
            code_file: String::new(),
            code_line: "1".into(),
            code_snippet: String::new(),
            drag: None,
            pending_overlay: None,
            pending_fetch: None,
            snap: true,
            retarget: false,
            released: false,
            hold: false,
            reveal: 1.0,
            posed: None,
            applied: None,
            capture_fetched: false,
        };
        match story.map(str::trim).filter(|s| !s.is_empty()) {
            Some(url) if is_embedded(url) => session.load_demo(),
            Some(url) => {
                session.pending_fetch = Some(url.to_string());
                session.open = true;
            }
            None => {}
        }
        session
    }

    pub fn take_fetch(&mut self) -> Option<String> {
        self.pending_fetch.take()
    }

    pub fn visible(&self) -> bool {
        self.open || self.doc.is_some() || !self.error.is_empty()
    }

    pub fn needs_frames(&self) -> bool {
        self.player.playing || self.player.transitioning()
    }

    pub fn captures_pointer(&self) -> bool {
        self.editing && self.tool != Tool::Select && self.doc.is_some()
    }

    pub fn locks_layout(&self) -> bool {
        self.doc.is_some() && !self.released
    }

    /// The story's `capture` field, when the page did not already name one
    /// and this session has not asked for it yet.
    pub fn take_capture_url(&mut self) -> Option<String> {
        if self.capture_fetched {
            return None;
        }
        let url = self
            .doc
            .as_ref()
            .map(|d| d.capture.trim().to_string())
            .filter(|s| !s.is_empty())?;
        self.capture_fetched = true;
        Some(url)
    }

    pub fn note_capture_replaced(&mut self) {
        if self.doc.is_some() {
            self.retarget = true;
            self.released = false;
            self.hold = true;
            self.applied = None;
        }
    }

    pub fn load_text(&mut self, text: &str) -> Result<(), String> {
        let story = perfr_story::parse_story(text)?;
        self.install(story);
        Ok(())
    }

    pub fn load_demo(&mut self) {
        self.install(perfr_story::demo_box3d());
    }

    fn install(&mut self, story: Story) {
        self.error.clear();
        self.doc = Some(story);
        self.player = Player::default();
        self.open = true;
        self.snap = true;
        self.retarget = true;
        self.released = false;
        self.hold = true;
        self.reveal = 1.0;
        self.posed = None;
        self.applied = None;
        self.capture_fetched = false;
    }

    pub fn play(&mut self) {
        self.player.playing = true;
        self.released = false;
        self.hold = true;
        self.open = true;
    }

    pub fn open_editor(&mut self) {
        self.open = true;
        self.editing = true;
        self.list_open = true;
        self.player.pause();
    }

    pub fn begin_edit(&mut self) {
        self.open = true;
        self.editing = true;
    }

    pub fn ensure_blank(&mut self) {
        if self.doc.is_some() {
            self.open = true;
            self.editing = true;
            return;
        }
        self.install(Story {
            title: "Untitled story".into(),
            summary: String::new(),
            ..Story::default()
        });
        self.editing = true;
    }

    pub fn release_camera(&mut self) {
        if self.hold || self.player.playing || self.player.transitioning() {
            self.player.pause();
            self.player.clear_transition();
            self.hold = false;
            self.released = true;
            self.retarget = false;
        }
    }

    pub fn tick(&mut self, dt_ms: f32) {
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        if doc.steps.is_empty() {
            return;
        }
        let durations: Vec<u32> = doc.steps.iter().map(|s| s.duration_ms).collect();
        let tick = self.player.tick(dt_ms, &durations);
        self.reveal = if self.player.transitioning() || tick.transition_t < 1.0 {
            overlay_reveal(tick.transition_t)
        } else {
            1.0
        };
        if let Some(camera) = tick.camera {
            self.posed = Some(camera);
            self.hold = true;
        }
        if tick.entered_step.is_some() {
            self.retarget = true;
            self.released = false;
            self.hold = true;
            self.snap = false;
            self.reveal = 0.0;
        }
    }

    /// Resolve the current step against the rail and report the pose to apply.
    pub fn drive<F, G>(
        &mut self,
        tracks: &TrackStrip,
        content: Option<(u64, u64)>,
        current: Camera,
        thread_name: F,
        process_name: G,
    ) -> Drive
    where
        F: Fn(u32, u32) -> String,
        G: Fn(u32) -> String,
    {
        let Some(doc) = self.doc.as_ref() else {
            return Drive {
                camera: None,
                tracks: None,
            };
        };
        if doc.steps.is_empty() || self.released {
            return Drive {
                camera: None,
                tracks: None,
            };
        }
        let step_index = self.player.step.min(doc.steps.len() - 1);
        let (c0, c1) = content.unwrap_or((0, 1));
        let key = (step_index, c0, c1);
        let step_changed = self.applied.map(|(s, _, _)| s) != Some(step_index);
        let content_changed = self.applied.map(|(_, a, b)| (a, b)) != Some((c0, c1));
        let mut tracks_out = None;
        if self.retarget || (step_changed && self.hold) {
            let step = &doc.steps[step_index];
            let target = camera_for(&step.view, tracks, content, &thread_name, &process_name);
            tracks_out = Some(track_ops(&step.view));
            let ms = doc.transition_ms as f32;
            if self.snap {
                self.player.clear_transition();
                self.posed = Some(target);
                self.reveal = 1.0;
                self.snap = false;
            } else if !self.player.transitioning() {
                self.player.begin_transition(current, target, ms);
                self.reveal = 0.0;
            }
            self.applied = Some(key);
            self.retarget = false;
            self.hold = true;
        } else if content_changed
            && self.hold
            && !self.player.transitioning()
            && !self.player.playing
        {
            let step = &doc.steps[step_index];
            self.posed = Some(camera_for(
                &step.view,
                tracks,
                content,
                &thread_name,
                &process_name,
            ));
            self.applied = Some(key);
            self.reveal = 1.0;
        }
        Drive {
            camera: if self.hold { self.posed } else { None },
            tracks: tracks_out,
        }
    }

    pub fn on_key(&mut self, ctx: &Context) -> bool {
        if self.doc.is_none() || ctx.wants_keyboard_input() {
            return false;
        }
        let (prev, next, play) = ctx.input(|i| {
            let alt = i.modifiers.alt;
            (
                i.key_pressed(Key::OpenBracket) || (alt && i.key_pressed(Key::ArrowLeft)),
                i.key_pressed(Key::CloseBracket) || (alt && i.key_pressed(Key::ArrowRight)),
                alt && i.key_pressed(Key::Space),
            )
        });
        if play {
            self.player.toggle_play();
            if self.player.playing {
                self.released = false;
                self.hold = true;
            }
            return true;
        }
        if next {
            self.step_by(1);
            return true;
        }
        if prev {
            self.step_by(-1);
            return true;
        }
        false
    }

    fn step_by(&mut self, delta: i32) {
        let len = self.doc.as_ref().map(|d| d.steps.len()).unwrap_or(0);
        let moved = if delta > 0 {
            self.player.next(len)
        } else {
            self.player.prev()
        };
        if moved.is_some() {
            self.retarget = true;
            self.released = false;
            self.hold = true;
            self.snap = false;
        }
    }

    pub fn pointer(&mut self, hit: Option<Hit>, down: bool, released: bool) -> Option<StoryCmd> {
        if !self.captures_pointer() {
            self.drag = None;
            return None;
        }
        let Some(hit) = hit else {
            if released {
                self.drag = None;
            }
            return None;
        };
        match self.tool {
            Tool::Arrow | Tool::Rect | Tool::Highlight => {
                if down && self.drag.is_none() {
                    self.drag = Some(Drag {
                        start: hit.clone(),
                        current: hit,
                    });
                } else if let Some(drag) = self.drag.as_mut() {
                    drag.current = hit;
                }
                if released {
                    if let Some(drag) = self.drag.take() {
                        let id = self.fresh_overlay_id();
                        if let Some(overlay) = overlay_from_drag(self.tool, &drag, id) {
                            return self.push_overlay(overlay);
                        }
                    }
                }
            }
            Tool::Callout | Tool::Code => {
                if released {
                    let id = self.fresh_overlay_id();
                    if let Some(overlay) = overlay_from_click(
                        self.tool,
                        &hit,
                        id,
                        &self.callout_text,
                        &self.code_file,
                        &self.code_line,
                        &self.code_snippet,
                    ) {
                        return self.push_overlay(overlay);
                    }
                    self.error = match self.tool {
                        Tool::Callout => "Write the callout text, then click the timeline.".into(),
                        Tool::Code => {
                            "A code reference needs a file and a line, then a click.".into()
                        }
                        _ => String::new(),
                    };
                }
            }
            Tool::Select => {}
        }
        None
    }

    /// An overlay id no overlay on the current step uses. A step drawn with
    /// the editor gets `o1`, `o2`, ... in the order they were added.
    fn fresh_overlay_id(&self) -> String {
        let step = self.doc.as_ref().and_then(|doc| {
            let index = self.player.step.min(doc.steps.len().checked_sub(1)?);
            doc.steps.get(index)
        });
        let taken: Vec<&str> = step
            .map(|s| s.overlays.iter().map(Overlay::id).collect())
            .unwrap_or_default();
        let mut n = taken.len() + 1;
        loop {
            let id = format!("o{n}");
            if !taken.contains(&id.as_str()) {
                return id;
            }
            n += 1;
        }
    }

    fn push_overlay(&mut self, overlay: Overlay) -> Option<StoryCmd> {
        let doc = self.doc.as_mut()?;
        if doc.steps.is_empty() {
            self.pending_overlay = Some(overlay);
            return Some(StoryCmd::AddStep);
        }
        let index = self.player.step.min(doc.steps.len() - 1);
        let step_id = doc.steps[index].id.clone();
        match doc.add_overlay(&step_id, overlay) {
            Ok(()) => self.error.clear(),
            Err(message) => self.error = message,
        }
        None
    }

    pub fn push_step(&mut self, step: Step) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        let step_id = step.id.clone();
        if let Err(message) = doc.add_step(step, None) {
            self.error = message;
            return;
        }
        self.error.clear();
        if let Some(overlay) = self.pending_overlay.take() {
            if let Err(message) = doc.add_overlay(&step_id, overlay) {
                self.error = message;
            }
        }
        let len = doc.steps.len();
        let _ = self.player.go(len - 1, len);
        self.retarget = true;
        self.released = false;
        self.hold = true;
        self.snap = true;
    }

    /// Drop the story. The capture stays as it is. In an iframe the bar
    /// stays, so Play can bring a story back.
    pub fn close(&mut self) {
        self.doc = None;
        self.player = Player::default();
        self.error.clear();
        self.open = self.compact;
        self.editing = false;
        self.list_open = false;
        self.export_open = false;
        self.drag = None;
        self.pending_overlay = None;
        self.retarget = false;
        self.released = false;
        self.hold = false;
        self.posed = None;
        self.applied = None;
    }

    pub fn bar(&mut self, ui: &mut Ui, extras: BarExtras<'_>) -> Vec<StoryCmd> {
        let mut cmds = Vec::new();
        if !self.visible() {
            return cmds;
        }
        let narrow = ui.ctx().screen_rect().width() < BAR_NARROW;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let playing = self.player.playing;
            let play = glyph_button(ui, if playing { Glyph::Pause } else { Glyph::Play }, playing);
            crate::app::note_ui_rect("story:play", play.rect);
            if play.on_hover_text("Play or pause (Alt+Space)").clicked() {
                self.player.toggle_play();
                if self.player.playing {
                    self.released = false;
                    self.hold = true;
                }
            }
            let prev = glyph_button(ui, Glyph::Prev, false);
            crate::app::note_ui_rect("story:prev", prev.rect);
            if prev.on_hover_text("Previous step ([ or Alt+Left)").clicked() {
                self.step_by(-1);
            }
            let next = glyph_button(ui, Glyph::Next, false);
            crate::app::note_ui_rect("story:next", next.rect);
            if next.on_hover_text("Next step (] or Alt+Right)").clicked() {
                self.step_by(1);
            }
            let (index, count, title, kind) = self.step_label();
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{} / {}", index, count.max(1)))
                    .font(FontId::monospace(12.0))
                    .color(theme::MUTED()),
            );
            // The right end first: what is left between it and the counter
            // is the title's, which is cut with an ellipsis when it is long.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if let Some((on, tip)) = extras.fullscreen {
                    let button = glyph_button(ui, if on { Glyph::Collapse } else { Glyph::Expand }, on);
                    crate::app::note_ui_rect("story:fullscreen", button.rect);
                    if button.on_hover_text(tip).clicked() {
                        cmds.push(StoryCmd::Fullscreen(!on));
                    }
                }
                if let Some((enabled, tip)) = extras.share {
                    let button = ui
                        .add_enabled(enabled, text_button("Share", false))
                        .on_hover_text(tip)
                        .on_disabled_hover_text(tip);
                    crate::app::note_ui_rect("Share", button.rect);
                    if button.clicked() {
                        cmds.push(StoryCmd::Share);
                    }
                }
                if !self.compact
                    && ui
                        .add(text_button("Close", false))
                        .on_hover_text("Close the story. The capture stays open.")
                        .clicked()
                {
                    self.close();
                }
                // A compact page is for reading: the editor is a click away
                // on the full viewer, and its second row would take the
                // height an iframe does not have.
                if !self.compact && ui.add(text_button("Edit", self.editing)).clicked() {
                    self.editing = !self.editing;
                    self.open = true;
                    if self.doc.is_none() {
                        cmds.push(StoryCmd::AddStep);
                    }
                }
                let steps = ui.add(text_button("Steps", self.list_open));
                crate::app::note_ui_rect("story:steps", steps.rect);
                if steps.on_hover_text("Every step of the story, with its text").clicked() {
                    self.list_open = !self.list_open;
                }
                if !narrow {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        if !kind.is_empty() {
                            kind_chip(ui, &kind);
                        }
                        ui.add(egui::Label::new(RichText::new(title).size(12.5).color(theme::TEXT())).truncate());
                    });
                }
            });
        });
        if self.editing {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if ui
                    .button("Add step")
                    .on_hover_text("A step from the current view")
                    .clicked()
                {
                    cmds.push(StoryCmd::AddStep);
                }
                for tool in [
                    Tool::Select,
                    Tool::Arrow,
                    Tool::Rect,
                    Tool::Callout,
                    Tool::Highlight,
                    Tool::Code,
                ] {
                    if ui
                        .selectable_label(self.tool == tool, tool.label())
                        .clicked()
                    {
                        self.tool = tool;
                    }
                }
                if ui.button("Delete").clicked() {
                    self.delete_current();
                }
                if ui.button("Up").clicked() {
                    self.move_current(-1);
                }
                if ui.button("Down").clicked() {
                    self.move_current(1);
                }
                if ui.button("Export").clicked() {
                    self.export_text = self
                        .doc
                        .as_ref()
                        .and_then(|d| d.to_json_pretty().ok())
                        .unwrap_or_default();
                    self.export_open = true;
                }
                if ui.button("Import").clicked() {
                    self.import_text.clear();
                    self.export_open = true;
                    self.export_text.clear();
                }
            });
            if self.tool == Tool::Callout {
                ui.add(
                    egui::TextEdit::singleline(&mut self.callout_text)
                        .hint_text("Callout text, then click the timeline")
                        .desired_width(ui.available_width().min(420.0)),
                );
            }
            if self.tool == Tool::Code {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.code_file)
                            .hint_text("file")
                            .desired_width(220.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.code_line)
                            .hint_text("line")
                            .desired_width(48.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.code_snippet)
                            .hint_text("snippet")
                            .desired_width(220.0),
                    );
                });
            }
        }
        if let Some(index) = self.progress(ui) {
            self.go_to(index);
        }
        if !self.error.is_empty() {
            ui.label(
                RichText::new(&self.error)
                    .size(11.0)
                    .color(Color32::from_rgb(0xF4, 0x43, 0x36)),
            );
        }
        cmds
    }

    /// One segment per step, filled as far as the story has played. A
    /// click on a segment asks for that step, which is what the return is.
    fn progress(&self, ui: &mut Ui) -> Option<usize> {
        let doc = self.doc.as_ref()?;
        let count = doc.steps.len();
        if count == 0 {
            return None;
        }
        let width = ui.available_width().max(1.0);
        // Taller to the pointer than to the eye: three pixels are not a
        // target anyone hits.
        let (hit, resp) = ui.allocate_exact_size(Vec2::new(width, 9.0), Sense::click());
        let bar = Rect::from_center_size(hit.center(), Vec2::new(hit.width(), if resp.hovered() { 5.0 } else { 3.0 }));
        let current = self.player.step.min(count - 1);
        let hold = doc.steps[current].duration_ms.max(1) as f32;
        let into = (self.player.elapsed_ms / hold).clamp(0.0, 1.0);
        let hovered = resp.hover_pos().map(|p| segment_at(bar, count, p.x));
        for i in 0..count {
            let seg = segment_rect(bar, count, i);
            // Dim, so what has played stands out from what has not on a
            // scheme whose hairline is as light as its accent.
            let base = if hovered == Some(i) { theme::MUTED() } else { theme::HAIR().gamma_multiply(0.45) };
            ui.painter().rect_filled(seg, 1.5, base);
            let fill = match i.cmp(&current) {
                std::cmp::Ordering::Less => 1.0,
                std::cmp::Ordering::Equal if self.player.playing || into > 0.0 => into.max(0.04),
                std::cmp::Ordering::Equal => 1.0,
                std::cmp::Ordering::Greater => 0.0,
            };
            if fill > 0.0 {
                let done = Rect::from_min_max(seg.min, Pos2::new(seg.left() + seg.width() * fill, seg.bottom()));
                ui.painter().rect_filled(done, 1.5, theme::ACCENT());
            }
        }
        if let Some(i) = hovered {
            let step = &doc.steps[i];
            resp.clone().on_hover_text_at_pointer(format!("{}. {}", i + 1, step.title));
        }
        if resp.clicked() {
            return resp.interact_pointer_pos().map(|p| segment_at(bar, count, p.x));
        }
        None
    }

    fn go_to(&mut self, index: usize) {
        let len = self.doc.as_ref().map(|d| d.steps.len()).unwrap_or(0);
        if self.player.go(index, len).is_some() {
            self.retarget = true;
            self.released = false;
            self.hold = true;
            self.snap = false;
        }
    }

    /// The current step's title and text, under the timeline. Without it a
    /// reader of an embedded story sees the camera move and the overlays
    /// appear, and the sentences that say why only by opening the step list.
    pub fn caption(&self, ctx: &Context) {
        if self.editing {
            return;
        }
        // The bar names the step when it has the width to; under that the
        // caption does, so the title is on screen once either way.
        let titled = ctx.screen_rect().width() < BAR_NARROW;
        let Some(doc) = self.doc.as_ref() else {
            return;
        };
        let Some(step) = doc.steps.get(self.player.step.min(doc.steps.len().saturating_sub(1))) else {
            return;
        };
        if step.body.trim().is_empty() {
            return;
        }
        egui::TopBottomPanel::bottom("perfr_story_caption")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL())
                    .inner_margin(egui::Margin { left: 14, right: 14, top: 8, bottom: 10 })
                    .stroke(Stroke::NONE),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                // A measure a sentence reads well at, however wide the page.
                let measure = ui.available_width().min(760.0);
                if titled {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        kind_chip(ui, step.kind.label());
                        ui.add(egui::Label::new(RichText::new(&step.title).size(13.0).strong().color(theme::TEXT())).truncate());
                    });
                    ui.add_space(2.0);
                }
                ui.label(lite_job_wrapped(&step.body, 12.5, theme::TEXT(), measure));
            });
    }

    fn step_label(&self) -> (usize, usize, String, String) {
        let Some(doc) = &self.doc else {
            return (0, 0, "No story".into(), String::new());
        };
        let count = doc.steps.len();
        if count == 0 {
            return (0, 0, doc.title.clone(), "draft".into());
        }
        let index = self.player.step.min(count - 1);
        let step = &doc.steps[index];
        (
            index + 1,
            count,
            step.title.clone(),
            step.kind.label().to_string(),
        )
    }

    fn delete_current(&mut self) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if doc.steps.is_empty() {
            return;
        }
        let index = self.player.step.min(doc.steps.len() - 1);
        let id = doc.steps[index].id.clone();
        let _ = doc.remove_step(&id);
        let empty = doc.steps.is_empty();
        let len = doc.steps.len();
        if empty {
            self.player.step = 0;
        } else if self.player.step >= len {
            self.player.step = len - 1;
        }
        self.retarget = !empty;
        self.snap = true;
        self.released = false;
        self.hold = !empty;
        self.applied = None;
    }

    fn move_current(&mut self, delta: i32) {
        let Some(doc) = self.doc.as_mut() else {
            return;
        };
        if doc.steps.is_empty() {
            return;
        }
        let index = self.player.step.min(doc.steps.len() - 1);
        let to = index as i32 + delta;
        if to < 0 || to >= doc.steps.len() as i32 {
            return;
        }
        let id = doc.steps[index].id.clone();
        let moved = doc.reorder(&id, to as usize).is_ok();
        if moved {
            self.player.step = to as usize;
        }
    }

    pub fn windows(&mut self, ctx: &Context) {
        self.step_window(ctx);
        self.io_window(ctx);
    }

    fn step_window(&mut self, ctx: &Context) {
        if !self.list_open {
            return;
        }
        let mut open = true;
        let mut goto = None;
        egui::Window::new("Story steps")
            .open(&mut open)
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                let Some(doc) = self.doc.as_ref() else {
                    ui.label("No story yet.");
                    return;
                };
                ui.label(RichText::new(&doc.title).size(13.0).color(theme::TEXT()));
                if !doc.summary.is_empty() {
                    ui.label(RichText::new(&doc.summary).size(11.0).color(theme::MUTED()));
                }
                ui.add_space(4.0);
                let current = self.player.step;
                for (i, step) in doc.steps.iter().enumerate() {
                    let label = format!("{}. {}  {}", i + 1, step.kind.label(), step.title);
                    if ui.selectable_label(i == current, label).clicked() {
                        goto = Some(i);
                    }
                    if i == current && !step.body.is_empty() {
                        ui.add_space(2.0);
                        lite_label(ui, &step.body);
                        ui.add_space(4.0);
                    }
                }
            });
        self.list_open = open;
        if let Some(index) = goto {
            self.go_to(index);
        }
    }

    fn io_window(&mut self, ctx: &Context) {
        if !self.export_open {
            return;
        }
        let importing = self.export_text.is_empty();
        let mut open = true;
        let mut imported = None;
        egui::Window::new(if importing {
            "Import story"
        } else {
            "Story JSON"
        })
        .open(&mut open)
        .resizable(true)
        .default_width(480.0)
        .show(ctx, |ui| {
            if importing {
                ui.label("Paste a perfr.story document.");
                ui.add(
                    egui::TextEdit::multiline(&mut self.import_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .font(FontId::monospace(11.0)),
                );
                if ui.button("Import").clicked() {
                    imported = Some(self.import_text.clone());
                }
            } else {
                ui.label(format!(
                    "{FORMAT} JSON. Copy it, or save it next to the capture."
                ));
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(self.export_text.clone());
                }
                ui.add(
                    egui::TextEdit::multiline(&mut self.export_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(14)
                        .font(FontId::monospace(11.0)),
                );
            }
        });
        self.export_open = open;
        if let Some(text) = imported {
            if let Err(message) = self.load_text(&text) {
                self.error = message;
                self.export_open = true;
            } else {
                self.export_open = false;
            }
        }
    }

    pub fn paint<F, G>(
        &self,
        ui: &Ui,
        body: Rect,
        tracks: &TrackStrip,
        index: &TrackIndex,
        intern: &InternTable,
        content: Option<(u64, u64)>,
        t0: f64,
        t1: f64,
        thread_name: F,
        process_name: G,
    ) where
        F: Fn(u32, u32) -> String,
        G: Fn(u32) -> String,
    {
        let Some(doc) = &self.doc else {
            return;
        };
        if doc.steps.is_empty() || t1 <= t0 {
            return;
        }
        let step = &doc.steps[self.player.step.min(doc.steps.len() - 1)];
        let reveal = self.reveal.clamp(0.0, 1.0);
        let painter = ui.painter();
        for overlay in &step.overlays {
            paint_overlay(
                ui,
                painter,
                overlay,
                body,
                tracks,
                index,
                intern,
                content,
                t0,
                t1,
                reveal,
                &thread_name,
                &process_name,
            );
        }
        if let Some(Selection::Scope { scope_id }) = &step.view.selected {
            let spans = scope_spans(index, tracks, intern, *scope_id, None, t0, t1);
            paint_scope_spans(ui, body, &spans, t0, t1, reveal);
        }
        if let Some(drag) = &self.drag {
            if let Some(overlay) = overlay_from_drag(self.tool, drag, "preview".into()) {
                paint_overlay(
                    ui,
                    painter,
                    &overlay,
                    body,
                    tracks,
                    index,
                    intern,
                    content,
                    t0,
                    t1,
                    1.0,
                    &thread_name,
                    &process_name,
                );
            }
        }
    }

    pub fn bar_height(&self) -> f32 {
        if !self.visible() {
            return 0.0;
        }
        let mut h = 32.0;
        if self.editing {
            h += 28.0;
            if matches!(self.tool, Tool::Callout | Tool::Code) {
                h += 24.0;
            }
        }
        if !self.error.is_empty() {
            h += 16.0;
        }
        h + 10.0
    }
}

/// Under this width of page the bar has no room for the step's title, and
/// the caption carries it.
const BAR_NARROW: f32 = 720.0;

/// The `i`th of `count` equal segments of `bar`, with a gap between two.
fn segment_rect(bar: Rect, count: usize, i: usize) -> Rect {
    let gap = if count > 1 && bar.width() / count as f32 > 6.0 { 2.0 } else { 0.0 };
    let width = (bar.width() - gap * (count.saturating_sub(1)) as f32) / count.max(1) as f32;
    let left = bar.left() + (width + gap) * i as f32;
    Rect::from_min_max(Pos2::new(left, bar.top()), Pos2::new(left + width, bar.bottom()))
}

/// Which segment of `bar` the horizontal position `x` is over. A position
/// in a gap, or past either end, goes to the nearest segment.
fn segment_at(bar: Rect, count: usize, x: f32) -> usize {
    if count == 0 || bar.width() <= 0.0 {
        return 0;
    }
    let frac = ((x - bar.left()) / bar.width()).clamp(0.0, 1.0);
    ((frac * count as f32) as usize).min(count - 1)
}

/// A bar button with a word on it, in the transport bar's own style.
fn text_button(text: &str, on: bool) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).size(12.0).color(if on { theme::CANVAS() } else { theme::TEXT() }))
        .fill(if on { theme::ACCENT() } else { theme::TRACK() })
        .stroke(if on { Stroke::NONE } else { Stroke::new(1.0, theme::HAIR()) })
        .min_size(Vec2::new(0.0, 22.0))
        .corner_radius(4)
}

/// A bar button with a painted glyph on it.
fn glyph_button(ui: &mut Ui, glyph: Glyph, on: bool) -> egui::Response {
    let resp = ui.add(
        egui::Button::new(RichText::new(" ").size(1.0))
            .fill(if on { theme::ACCENT() } else { theme::TRACK() })
            .stroke(if on { Stroke::NONE } else { Stroke::new(1.0, theme::HAIR()) })
            .min_size(Vec2::new(28.0, 22.0))
            .corner_radius(4),
    );
    let color = if on {
        theme::CANVAS()
    } else if resp.hovered() {
        theme::TEXT()
    } else {
        theme::MUTED()
    };
    paint_glyph(ui.painter(), resp.rect.center(), glyph, color);
    resp
}

fn paint_glyph(painter: &egui::Painter, c: Pos2, glyph: Glyph, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let tri = |points: [Pos2; 3]| egui::Shape::convex_polygon(points.to_vec(), color, Stroke::NONE);
    match glyph {
        Glyph::Play => {
            painter.add(tri([c + Vec2::new(-3.5, -5.0), c + Vec2::new(-3.5, 5.0), c + Vec2::new(5.0, 0.0)]));
        }
        Glyph::Pause => {
            for dx in [-3.0, 3.0] {
                painter.rect_filled(Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(3.0, 10.0)), 0.5, color);
            }
        }
        Glyph::Prev | Glyph::Next => {
            let d = if glyph == Glyph::Next { 1.0 } else { -1.0 };
            painter.line_segment([c + Vec2::new(-2.0 * d, -4.5), c + Vec2::new(2.5 * d, 0.0)], stroke);
            painter.line_segment([c + Vec2::new(2.5 * d, 0.0), c + Vec2::new(-2.0 * d, 4.5)], stroke);
        }
        Glyph::Expand | Glyph::Collapse => {
            // Four corner brackets: pointing out to enter, in to leave.
            let half = 5.5;
            let arm = 3.4;
            let (inset, dir) = if glyph == Glyph::Collapse { (2.2, -1.0) } else { (0.0, 1.0) };
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let corner = c + Vec2::new(sx * (half - inset), sy * (half - inset));
                let stroke = Stroke::new(1.35, color);
                painter.line_segment([corner, corner - Vec2::new(sx * arm * dir, 0.0)], stroke);
                painter.line_segment([corner, corner - Vec2::new(0.0, sy * arm * dir)], stroke);
            }
        }
    }
}

/// The step's kind as a small tag: `context`, `root cause`.
fn kind_chip(ui: &mut Ui, kind: &str) {
    let accent = theme::ACCENT();
    egui::Frame::new()
        .fill(accent.gamma_multiply(0.16))
        .corner_radius(3)
        .inner_margin(egui::Margin { left: 5, right: 5, top: 1, bottom: 1 })
        .show(ui, |ui| {
            ui.label(RichText::new(kind.replace('_', " ")).size(10.5).color(accent));
        });
}

fn is_embedded(url: &str) -> bool {
    matches!(url, "demo" | "box3d")
}

fn track_ops(view: &View) -> Vec<(TrackRef, bool)> {
    let mut ops = Vec::new();
    for track in &view.expanded {
        ops.push((track.clone(), false));
    }
    for track in &view.collapsed {
        ops.push((track.clone(), true));
    }
    ops
}

fn camera_for<F, G>(
    view: &View,
    tracks: &TrackStrip,
    content: Option<(u64, u64)>,
    thread_name: &F,
    process_name: &G,
) -> Camera
where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let (t0, t1) = view.resolve_window(content).unwrap_or((0, 1_000_000_000));
    let scroll = match &view.scroll {
        Some(Scroll::Y { y }) => *y,
        Some(Scroll::Track { track }) => band_for(tracks, track, thread_name, process_name)
            .map(|b| b.y)
            .unwrap_or(0.0),
        None => view
            .focused
            .first()
            .and_then(|track| band_for(tracks, track, thread_name, process_name))
            .map(|b| b.y)
            .unwrap_or(0.0),
    };
    Camera {
        t0_ns: t0 as f64,
        t1_ns: t1.max(t0 + 1) as f64,
        scroll_y: scroll,
    }
}

fn bands(tracks: &TrackStrip) -> Vec<Band> {
    let rows = tracks.rows();
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let row = &rows[i];
        let Some(track) = row_track(row.id) else {
            i += 1;
            continue;
        };
        let mut bottom = row.y + row.height;
        let mut j = i + 1;
        while j < rows.len() && matches!(rows[j].id, RowId::Lane(_)) {
            bottom = rows[j].y + rows[j].height;
            j += 1;
        }
        out.push(Band {
            track,
            y: row.y,
            bottom,
        });
        i = j;
    }
    out
}

fn row_track(id: RowId) -> Option<TrackRef> {
    match id {
        RowId::Scheduler => Some(TrackRef::Scheduler),
        RowId::Process(pid) => Some(TrackRef::Process { pid }),
        RowId::Thread(t) => Some(TrackRef::Thread {
            pid: t.pid,
            tid: t.tid,
        }),
        RowId::Async(t) => Some(TrackRef::Async {
            pid: t.pid,
            tid: t.tid,
        }),
        RowId::Lane(_) | RowId::Machine(_) => None,
    }
}

fn band_for<F, G>(
    tracks: &TrackStrip,
    track: &TrackRef,
    thread_name: &F,
    process_name: &G,
) -> Option<Band>
where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let bands = bands(tracks);
    match track {
        TrackRef::Name { name } => {
            let needle = name.to_ascii_lowercase();
            if needle.is_empty() {
                return None;
            }
            if matches!(needle.as_str(), "scheduler" | "sched" | "core" | "cores") {
                if let Some(band) = bands
                    .iter()
                    .find(|b| matches!(b.track, TrackRef::Scheduler))
                {
                    return Some(band.clone());
                }
            }
            bands.into_iter().find(|band| match &band.track {
                TrackRef::Thread { pid, tid } => {
                    thread_name(*pid, *tid)
                        .to_ascii_lowercase()
                        .contains(&needle)
                        || process_name(*pid).to_ascii_lowercase().contains(&needle)
                }
                TrackRef::Process { pid } => {
                    process_name(*pid).to_ascii_lowercase().contains(&needle)
                }
                TrackRef::Async { pid, tid } => thread_name(*pid, *tid)
                    .to_ascii_lowercase()
                    .contains(&needle),
                TrackRef::Scheduler => "scheduler".contains(&needle),
                TrackRef::Name { .. } => false,
            })
        }
        other => bands.into_iter().find(|band| &band.track == other),
    }
}

pub fn resolve_row<F, G>(
    tracks: &TrackStrip,
    track: &TrackRef,
    thread_name: F,
    process_name: G,
) -> Option<RowId>
where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let band = band_for(tracks, track, &thread_name, &process_name)?;
    match band.track {
        TrackRef::Scheduler => Some(RowId::Scheduler),
        TrackRef::Process { pid } => Some(RowId::Process(pid)),
        TrackRef::Thread { pid, tid } => Some(RowId::Thread(crate::tracks::ThreadId { pid, tid })),
        TrackRef::Async { pid, tid } => Some(RowId::Async(crate::tracks::ThreadId { pid, tid })),
        TrackRef::Name { .. } => None,
    }
}

fn anchor_pos<F, G>(
    anchor: &Anchor,
    body: Rect,
    tracks: &TrackStrip,
    content: Option<(u64, u64)>,
    t0: f64,
    t1: f64,
    thread_name: &F,
    process_name: &G,
) -> Option<Pos2>
where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let time = anchor.resolve_time(content);
    let x = x_of(time, body, t0, t1);
    let y = if let Some(band) = band_for(tracks, &anchor.track, thread_name, process_name) {
        let span = (band.bottom - band.y).max(1.0);
        body.top() + band.y + anchor.y.clamp(0.0, 1.0) * span
    } else {
        body.top() + body.height() * 0.22
    };
    Some(Pos2::new(x, y))
}

fn x_of(time: u64, body: Rect, t0: f64, t1: f64) -> f32 {
    let span = (t1 - t0).max(1.0);
    let w = body.width().max(0.0);
    body.left() + (((time as f64 - t0) / span) as f32 * w).clamp(0.0, w)
}

fn range_of(
    t0_ns: Option<u64>,
    t1_ns: Option<u64>,
    t0_frac: Option<f64>,
    t1_frac: Option<f64>,
    content: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    let view = View {
        t0_ns,
        t1_ns,
        t0_frac,
        t1_frac,
        fit: Some(Fit::Span),
        ..View::default()
    };
    view.resolve_window(content).ok()
}

fn paint_overlay<F, G>(
    ui: &Ui,
    painter: &egui::Painter,
    overlay: &Overlay,
    body: Rect,
    tracks: &TrackStrip,
    index: &TrackIndex,
    intern: &InternTable,
    content: Option<(u64, u64)>,
    t0: f64,
    t1: f64,
    reveal: f32,
    thread_name: &F,
    process_name: &G,
) where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let accent = theme::ACCENT();
    let color =
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), (220.0 * reveal) as u8);
    match overlay {
        Overlay::Arrow { from, to, .. } => {
            let (Some(a), Some(b)) = (
                anchor_pos(
                    from,
                    body,
                    tracks,
                    content,
                    t0,
                    t1,
                    thread_name,
                    process_name,
                ),
                anchor_pos(to, body, tracks, content, t0, t1, thread_name, process_name),
            ) else {
                return;
            };
            paint_arrow(painter, a, b, color, reveal);
        }
        Overlay::Rect {
            t0_ns,
            t1_ns,
            t0_frac,
            t1_frac,
            tracks: named,
            label,
            ..
        } => {
            let Some((a, b)) = range_of(*t0_ns, *t1_ns, *t0_frac, *t1_frac, content) else {
                return;
            };
            let (top, bottom) = union_bands(tracks, named, thread_name, process_name, body);
            let rect = time_rect(body, a, b, top, bottom, t0, t1);
            grow_rect(painter, rect, color, reveal, 6.0);
            if !label.is_empty() && reveal > 0.4 {
                painter.text(
                    rect.left_top() + Vec2::new(6.0, 4.0),
                    Align2::LEFT_TOP,
                    label,
                    FontId::new(11.0, fonts::medium()),
                    color,
                );
            }
        }
        Overlay::Highlight { target, .. } => match target {
            Highlight::Time {
                t0_ns,
                t1_ns,
                t0_frac,
                t1_frac,
            } => {
                if let Some((a, b)) = range_of(*t0_ns, *t1_ns, *t0_frac, *t1_frac, content) {
                    let rect = time_rect(body, a, b, body.top(), body.bottom(), t0, t1);
                    painter.rect_filled(
                        rect,
                        0.0,
                        Color32::from_rgba_unmultiplied(
                            accent.r(),
                            accent.g(),
                            accent.b(),
                            (48.0 * reveal) as u8,
                        ),
                    );
                }
            }
            Highlight::Track { track } => {
                if let Some(band) = band_for(tracks, track, thread_name, process_name) {
                    let rect = Rect::from_min_max(
                        Pos2::new(body.left(), body.top() + band.y),
                        Pos2::new(body.right(), body.top() + band.bottom),
                    );
                    painter.rect_filled(
                        rect,
                        0.0,
                        Color32::from_rgba_unmultiplied(
                            accent.r(),
                            accent.g(),
                            accent.b(),
                            (36.0 * reveal) as u8,
                        ),
                    );
                }
            }
            Highlight::Scope { scope_id, name } => {
                let spans = scope_spans(
                    index,
                    tracks,
                    intern,
                    scope_id.unwrap_or(0),
                    name.as_deref(),
                    t0,
                    t1,
                );
                paint_scope_spans(ui, body, &spans, t0, t1, reveal);
            }
        },
        Overlay::Callout { anchor, text, .. } => {
            let Some(at) = anchor_pos(
                anchor,
                body,
                tracks,
                content,
                t0,
                t1,
                thread_name,
                process_name,
            ) else {
                return;
            };
            paint_callout(ui, at, text, reveal);
        }
        Overlay::Code {
            anchor,
            file,
            line,
            snippet,
            ..
        } => {
            let at = anchor
                .as_ref()
                .and_then(|anchor| {
                    anchor_pos(
                        anchor,
                        body,
                        tracks,
                        content,
                        t0,
                        t1,
                        thread_name,
                        process_name,
                    )
                })
                .unwrap_or(Pos2::new(body.left() + 16.0, body.top() + 16.0));
            let mut text = format!("{file}:{line}");
            if !snippet.is_empty() {
                text.push('\n');
                text.push_str(snippet);
            }
            paint_callout(ui, at, &text, reveal);
        }
    }
}

fn scope_spans(
    index: &TrackIndex,
    tracks: &TrackStrip,
    intern: &InternTable,
    scope_id: u64,
    name: Option<&str>,
    t0: f64,
    t1: f64,
) -> Vec<(u64, u64, f32, f32)> {
    let want = name.unwrap_or("").trim().to_ascii_lowercase();
    if scope_id == 0 && want.is_empty() {
        return Vec::new();
    }
    let ys: std::collections::HashMap<LaneKey, f32> = tracks.layout().iter().copied().collect();
    let t0 = t0.max(0.0) as u64;
    let t1 = t1.max(0.0) as u64;
    let mut out = Vec::new();
    let mut name_matches: crate::tracks::FastMap<u32, bool> = crate::tracks::FastMap::default();
    for (key, lane) in index.lanes() {
        if key.kind != kind::API_SCOPE && key.kind != kind::FUNCTION_CALL {
            continue;
        }
        let Some(&y) = ys.get(&key) else { continue };
        let h = lane_height(key) * tracks.scale.max(0.1);
        // Only the events on screen. This runs every frame a step with a
        // highlight is shown, and it used to walk every event of the
        // capture, testing the name before the window: with few matches
        // or none in view, millions of events per frame. Events are in
        // start order, so the walk begins at the first one that can still
        // reach `t0` and stops at the first that starts after `t1`.
        let events = lane.events();
        let first = match t0.checked_sub(1) {
            Some(before) => lane.first_ending_after(before),
            None => 0,
        };
        for ev in &events[first..] {
            if ev.start_ns > t1 {
                break;
            }
            // A lane whose ends are out of order (an instant inside a
            // long scope) hands back events that ended before the window.
            if ev.end_ns() < t0 {
                continue;
            }
            let id_ok = scope_id != 0 && u64::from(ev.name_id) == scope_id;
            // The name is judged once per id, not once per event: each
            // test lower-cases a copy of it.
            let named = !id_ok
                && !want.is_empty()
                && *name_matches.entry(ev.name_id).or_insert_with(|| {
                    intern.get(ev.name_id).unwrap_or("").to_ascii_lowercase().contains(&want)
                });
            if !named && !id_ok {
                continue;
            }
            out.push((ev.start_ns, ev.end_ns(), y, h));
            if out.len() >= 48 {
                return out;
            }
        }
    }
    out
}

pub fn paint_scope_spans(
    ui: &Ui,
    body: Rect,
    spans: &[(u64, u64, f32, f32)],
    t0: f64,
    t1: f64,
    reveal: f32,
) {
    let accent = theme::ACCENT();
    let painter = ui.painter();
    for &(a, b, y, h) in spans.iter().take(48) {
        let rect = time_rect(body, a, b, body.top() + y, body.top() + y + h, t0, t1);
        painter.rect_filled(
            rect,
            2.0,
            Color32::from_rgba_unmultiplied(
                accent.r(),
                accent.g(),
                accent.b(),
                (70.0 * reveal) as u8,
            ),
        );
    }
}

fn union_bands<F, G>(
    tracks: &TrackStrip,
    named: &[TrackRef],
    thread_name: &F,
    process_name: &G,
    body: Rect,
) -> (f32, f32)
where
    F: Fn(u32, u32) -> String,
    G: Fn(u32) -> String,
{
    let mut top = None;
    let mut bottom = None;
    for track in named {
        if let Some(band) = band_for(tracks, track, thread_name, process_name) {
            let y0 = body.top() + band.y;
            let y1 = body.top() + band.bottom;
            top = Some(top.map_or(y0, |t: f32| t.min(y0)));
            bottom = Some(bottom.map_or(y1, |t: f32| t.max(y1)));
        }
    }
    (top.unwrap_or(body.top()), bottom.unwrap_or(body.bottom()))
}

fn time_rect(body: Rect, a: u64, b: u64, top: f32, bottom: f32, t0: f64, t1: f64) -> Rect {
    let x0 = x_of(a.min(b), body, t0, t1);
    let x1 = x_of(a.max(b), body, t0, t1);
    Rect::from_min_max(
        Pos2::new(x0, top.min(bottom)),
        Pos2::new(x1.max(x0 + 1.0), top.max(bottom).max(top + 1.0)),
    )
}

fn grow_rect(painter: &egui::Painter, rect: Rect, color: Color32, reveal: f32, radius: f32) {
    let center = rect.center();
    let grown = Rect::from_center_size(center, rect.size() * ease_smoothstep(reveal).max(0.04));
    painter.rect_stroke(
        grown,
        radius,
        Stroke::new(1.5, color),
        egui::StrokeKind::Inside,
    );
}

fn paint_arrow(painter: &egui::Painter, from: Pos2, to: Pos2, color: Color32, reveal: f32) {
    let delta = to - from;
    let end = from + delta * reveal.clamp(0.0, 1.0);
    painter.line_segment([from, end], Stroke::new(1.7, color));
    if reveal < 0.35 {
        return;
    }
    let len = delta.length().max(0.001);
    let dir = delta / len;
    let normal = Vec2::new(-dir.y, dir.x);
    let back = end - dir * 8.0;
    painter.line_segment([end, back + normal * 4.0], Stroke::new(1.7, color));
    painter.line_segment([end, back - normal * 4.0], Stroke::new(1.7, color));
}

fn paint_callout(ui: &Ui, at: Pos2, text: &str, reveal: f32) {
    if text.is_empty() || reveal <= 0.0 {
        return;
    }
    let color = Color32::from_white_alpha((255.0 * reveal) as u8);
    let job = lite_job(text, 12.0, color);
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    let pad = Vec2::new(8.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let mut origin = at + Vec2::new(10.0, -size.y - 8.0);
    let clip = ui.clip_rect();
    if origin.x + size.x > clip.right() - 4.0 {
        origin.x = (clip.right() - size.x - 4.0).max(clip.left() + 4.0);
    }
    if origin.y < clip.top() + 4.0 {
        origin.y = at.y + 10.0;
    }
    let rect = Rect::from_min_size(origin, size);
    let fill = Color32::from_rgba_unmultiplied(0x19, 0x19, 0x19, (230.0 * reveal) as u8);
    let accent = theme::ACCENT();
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter().rect_stroke(
        rect,
        6.0,
        Stroke::new(
            1.0,
            Color32::from_rgba_unmultiplied(
                accent.r(),
                accent.g(),
                accent.b(),
                (180.0 * reveal) as u8,
            ),
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().circle_filled(
        at,
        3.0,
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), (220.0 * reveal) as u8),
    );
    ui.painter().galley(origin + pad, galley, color);
}

fn lite_label(ui: &mut Ui, text: &str) {
    let job = lite_job(text, 11.5, theme::TEXT());
    ui.label(job);
}

/// Paragraphs, `**bold**`, and `` `code` ``. Anything else is shown as text.
fn lite_job(text: &str, size: f32, color: Color32) -> LayoutJob {
    lite_job_wrapped(text, size, color, 280.0)
}

fn lite_job_wrapped(text: &str, size: f32, color: Color32, max_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = max_width;
    for (i, paragraph) in text.split("\n\n").enumerate() {
        if i > 0 {
            job.append(
                "\n",
                0.0,
                TextFormat {
                    font_id: FontId::proportional(size),
                    color,
                    ..Default::default()
                },
            );
        }
        append_inline(&mut job, paragraph, size, color);
    }
    job
}

fn append_inline(job: &mut LayoutJob, text: &str, size: f32, color: Color32) {
    let plain = |job: &mut LayoutJob, text: &str| {
        if !text.is_empty() {
            job.append(
                text,
                0.0,
                TextFormat {
                    font_id: FontId::proportional(size),
                    color,
                    ..Default::default()
                },
            );
        }
    };
    let mut rest = text;
    while !rest.is_empty() {
        // The nearest marker of either kind. Everything before it is plain.
        let next = match (rest.find('`'), rest.find("**")) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => {
                plain(job, rest);
                return;
            }
        };
        let (head, tail) = rest.split_at(next);
        plain(job, head);
        if let Some(code) = tail.strip_prefix('`') {
            if let Some(end) = code.find('`') {
                job.append(
                    &code[..end],
                    0.0,
                    TextFormat {
                        font_id: FontId::monospace(size),
                        color,
                        background: Color32::from_white_alpha(16),
                        ..Default::default()
                    },
                );
                rest = &code[end + 1..];
                continue;
            }
        } else if let Some(bold) = tail.strip_prefix("**") {
            if let Some(end) = bold.find("**") {
                job.append(
                    &bold[..end],
                    0.0,
                    TextFormat {
                        font_id: FontId::new(size, fonts::medium()),
                        color,
                        ..Default::default()
                    },
                );
                rest = &bold[end + 2..];
                continue;
            }
        }
        // A marker with no closing partner is ordinary text. Consuming it
        // is what keeps this loop finite.
        let marker = if tail.starts_with("**") { 2 } else { 1 };
        plain(job, &tail[..marker]);
        rest = &tail[marker..];
    }
}

fn overlay_from_drag(tool: Tool, drag: &Drag, id: String) -> Option<Overlay> {
    match tool {
        Tool::Arrow => Some(Overlay::Arrow {
            id,
            from: anchor_of(&drag.start),
            to: anchor_of(&drag.current),
        }),
        Tool::Rect => Some(Overlay::Rect {
            id,
            t0_ns: Some(drag.start.time_ns.min(drag.current.time_ns)),
            t1_ns: Some(
                drag.start
                    .time_ns
                    .max(drag.current.time_ns)
                    .max(drag.start.time_ns + 1),
            ),
            t0_frac: None,
            t1_frac: None,
            tracks: vec![drag.start.track.clone()],
            label: String::new(),
        }),
        Tool::Highlight => Some(Overlay::Highlight {
            id,
            target: Highlight::Time {
                t0_ns: Some(drag.start.time_ns.min(drag.current.time_ns)),
                t1_ns: Some(
                    drag.start
                        .time_ns
                        .max(drag.current.time_ns)
                        .max(drag.start.time_ns + 1),
                ),
                t0_frac: None,
                t1_frac: None,
            },
        }),
        _ => None,
    }
}

fn overlay_from_click(
    tool: Tool,
    hit: &Hit,
    id: String,
    callout: &str,
    file: &str,
    line: &str,
    snippet: &str,
) -> Option<Overlay> {
    match tool {
        Tool::Callout if !callout.trim().is_empty() => Some(Overlay::Callout {
            id,
            anchor: anchor_of(hit),
            text: callout.trim().to_string(),
        }),
        Tool::Code => {
            let line_no = line.trim().parse::<u32>().unwrap_or(0);
            if file.trim().is_empty() || line_no == 0 {
                return None;
            }
            Some(Overlay::Code {
                id,
                anchor: Some(anchor_of(hit)),
                file: file.trim().to_string(),
                line: line_no,
                snippet: snippet.trim().to_string(),
            })
        }
        _ => None,
    }
}

fn anchor_of(hit: &Hit) -> Anchor {
    Anchor {
        time_ns: Some(hit.time_ns),
        time_frac: None,
        track: hit.track.clone(),
        y: hit.y.clamp(0.0, 1.0),
    }
}

/// A step that records the view the author is looking at.
pub fn step_from_view(id: &str, view: View) -> Step {
    Step {
        id: id.to_string(),
        kind: StepKind::Evidence,
        title: "Step".into(),
        body: String::new(),
        duration_ms: 4_000,
        view,
        overlays: Vec::new(),
    }
}

pub fn next_step_id(story: &Story) -> String {
    let mut n = story.steps.len() + 1;
    loop {
        let id = format!("step-{n}");
        if story.steps.iter().all(|s| s.id != id) {
            return id;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_on_the_progress_strip_names_the_step_drawn_there() {
        let bar = Rect::from_min_max(Pos2::new(100.0, 0.0), Pos2::new(600.0, 3.0));
        // Five steps over 500 px: each owns a fifth, its gap included.
        for (x, step) in [(100.0, 0), (199.0, 0), (201.0, 1), (350.0, 2), (599.0, 4)] {
            assert_eq!(segment_at(bar, 5, x), step, "x = {x}");
        }
        // Past either end is the nearest step, not a panic or a sixth.
        assert_eq!(segment_at(bar, 5, -40.0), 0);
        assert_eq!(segment_at(bar, 5, 9_000.0), 4);
        assert_eq!(segment_at(bar, 0, 300.0), 0);
        // The segments tile the strip: the first starts at its left edge,
        // the last ends at its right, and the middle of each is its own.
        assert_eq!(segment_rect(bar, 5, 0).left(), bar.left());
        assert!((segment_rect(bar, 5, 4).right() - bar.right()).abs() < 0.01);
        for i in 0..5 {
            assert_eq!(segment_at(bar, 5, segment_rect(bar, 5, i).center().x), i);
        }
        // One step is the whole strip, and many steps on a narrow strip
        // give up their gaps rather than their width.
        assert_eq!(segment_rect(bar, 1, 0), bar);
        let narrow = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(60.0, 3.0));
        assert!(segment_rect(narrow, 30, 7).width() > 1.9);
    }

    fn section_texts(job: &LayoutJob) -> Vec<String> {
        job.sections
            .iter()
            .map(|s| job.text[s.byte_range.clone()].to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    #[test]
    fn lite_markdown_splits_code_and_bold_in_source_order() {
        let job = lite_job("a **b** `c` d", 12.0, Color32::WHITE);
        assert_eq!(job.text, "a b c d");
        assert_eq!(section_texts(&job), ["a ", "b", " ", "c", " d"]);
        let bold = &job.sections[1];
        assert_eq!(bold.format.font_id.family, fonts::medium());
        let code = &job.sections[3];
        assert_eq!(code.format.font_id.family, egui::FontFamily::Monospace);
    }

    #[test]
    fn lite_markdown_keeps_unmatched_markers_as_text() {
        // Each of these used to spin forever: the marker was found at offset
        // 0, nothing was consumed, and the loop never advanced.
        for text in ["**unterminated", "`lonely", "a ** b ` c", "``", "****"] {
            let job = lite_job(text, 12.0, Color32::WHITE);
            let expected = text.replace("``", "").replace("****", "");
            assert_eq!(job.text, expected, "{text:?}");
        }
        let job = lite_job("one\n\ntwo", 12.0, Color32::WHITE);
        assert_eq!(job.text, "one\ntwo");
    }

    #[test]
    fn overlay_ids_from_the_editor_do_not_collide() {
        let mut session = StorySession::from_query(None, false);
        session.ensure_blank();
        let hit = Hit {
            time_ns: 5,
            track: TrackRef::Scheduler,
            y: 0.2,
        };
        session.push_step(step_from_view("s", View::default()));
        for _ in 0..3 {
            let id = session.fresh_overlay_id();
            let overlay = overlay_from_click(Tool::Callout, &hit, id, "note", "", "1", "").unwrap();
            assert!(session.push_overlay(overlay).is_none());
            assert!(session.error.is_empty(), "{}", session.error);
        }
        let ids: Vec<&str> = session.doc.as_ref().unwrap().steps[0]
            .overlays
            .iter()
            .map(Overlay::id)
            .collect();
        assert_eq!(ids, ["o1", "o2", "o3"]);
        session.close();
        assert!(session.doc.is_none());
        assert!(!session.visible());
    }

    fn span_event(pid: u32, tid: u32, start_ns: u64, duration_ns: u64, name_id: u32) -> perfr_live_event::LiveEvent {
        perfr_live_event::LiveEvent {
            start_ns,
            duration_ns,
            tid,
            pid,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id,
        }
    }

    /// The highlight as it was first written: every event of every scope
    /// lane, the name lower-cased per event, the window checked last.
    fn scope_spans_by_full_scan(
        index: &TrackIndex,
        tracks: &TrackStrip,
        intern: &InternTable,
        scope_id: u64,
        name: Option<&str>,
        t0: f64,
        t1: f64,
    ) -> Vec<(u64, u64, f32, f32)> {
        let want = name.unwrap_or("").trim().to_ascii_lowercase();
        if scope_id == 0 && want.is_empty() {
            return Vec::new();
        }
        let ys: std::collections::HashMap<LaneKey, f32> = tracks.layout().iter().copied().collect();
        let t0 = t0.max(0.0) as u64;
        let t1 = t1.max(0.0) as u64;
        let mut out = Vec::new();
        for (key, lane) in index.lanes() {
            if key.kind != kind::API_SCOPE && key.kind != kind::FUNCTION_CALL {
                continue;
            }
            let Some(&y) = ys.get(&key) else { continue };
            let h = lane_height(key) * tracks.scale.max(0.1);
            for ev in lane.events() {
                let named = !want.is_empty()
                    && intern.get(ev.name_id).unwrap_or("").to_ascii_lowercase().contains(&want);
                let id_ok = scope_id != 0 && u64::from(ev.name_id) == scope_id;
                if !named && !id_ok {
                    continue;
                }
                if ev.end_ns() < t0 || ev.start_ns > t1 {
                    continue;
                }
                out.push((ev.start_ns, ev.end_ns(), y, h));
                if out.len() >= 48 {
                    return out;
                }
            }
        }
        out
    }

    #[test]
    fn a_scope_highlight_finds_what_a_scan_of_every_event_finds() {
        let mut intern = InternTable::default();
        intern.insert_id(1, "Physics::Step");
        intern.insert_id(2, "render");
        intern.insert_id(3, "physics_island");
        let mut index = TrackIndex::default();
        // Back to back scopes on one thread: ends in order.
        for i in 0..400u64 {
            index.insert(span_event(10, 1, i * 100, 100, 1 + (i % 3) as u32));
        }
        // A thread with a long scope and instants inside it, which breaks
        // the order of the ends that the window search leans on.
        index.insert(span_event(10, 2, 0, 30_000, 3));
        for i in 0..50u64 {
            index.insert(span_event(10, 2, 500 + i * 400, 1, 1));
        }
        // An id nothing has, and a thread with nothing that matches.
        for i in 0..50u64 {
            index.insert(span_event(11, 1, i * 700, 300, 2));
        }
        let mut tracks = TrackStrip::default();
        tracks.sync(&index, None);
        let windows = [
            (0.0, 40_000.0),
            (0.0, 0.0),
            (100.0, 100.0),
            (199.0, 200.0),
            (10_050.0, 10_950.0),
            (39_900.0, 1e12),
            (-5.0, 350.0),
            (50_000.0, 60_000.0),
        ];
        let asks: [(u64, Option<&str>); 6] =
            [(0, Some("physics")), (0, Some("  STEP ")), (2, None), (3, Some("render")), (99, None), (0, Some("nothing"))];
        let mut found = 0;
        for (t0, t1) in windows {
            for (id, name) in asks {
                let got = scope_spans(&index, &tracks, &intern, id, name, t0, t1);
                let want = scope_spans_by_full_scan(&index, &tracks, &intern, id, name, t0, t1);
                assert_eq!(got, want, "window {t0}..{t1}, scope {id} {name:?}");
                found += got.len();
            }
        }
        assert!(found > 200, "the cases found too little to compare ({found})");
        assert!(scope_spans(&index, &tracks, &intern, 0, None, 0.0, 1e12).is_empty());
    }

    /// Per-frame cost of a story step's scope highlight on a large capture.
    /// Run with `cargo test --release highlight_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn highlight_bench() {
        let mut intern = InternTable::default();
        for id in 1..=64u32 {
            intern.insert_id(id, &format!("namespace::Component{id}::update_step"));
        }
        intern.insert_id(100, "rare_marker");
        let mut index = TrackIndex::default();
        let per_thread = 250_000u64;
        for tid in 1..=8u32 {
            for i in 0..per_thread {
                index.insert(span_event(10, tid, i * 1_000, 900, 1 + (i % 64) as u32));
            }
        }
        // One match, at the very end: the highlight has to look everywhere.
        index.insert(span_event(10, 8, per_thread * 1_000, 900, 100));
        let mut tracks = TrackStrip::default();
        tracks.sync(&index, None);
        let end = (per_thread * 1_000 + 1_000) as f64;
        // The step's camera on 1% of the capture, and on all of it.
        for (label, t0, t1) in [("window_1_percent", end * 0.5, end * 0.51), ("whole_capture", 0.0, end)] {
            for (how, id, name) in [("by_name", 0u64, Some("rare_marker")), ("by_id", 100u64, None)] {
                let iters = 5;
                let t = std::time::Instant::now();
                let mut n = 0;
                for _ in 0..iters {
                    n += scope_spans(&index, &tracks, &intern, id, name, t0, t1).len();
                }
                let ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
                println!("HIGHLIGHT_BENCH events={} {label} {how} ms_per_frame={ms:.3} (found {n})", index.event_count());
            }
        }
    }
}
