// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tests that hold a whole `PerfrLiveApp` and drive it the way a page does:
//! stream frames through `ingest`, status answers through `apply_status`,
//! and whole frames through `update` with synthetic input. No GPU and no
//! service: eframe's creation context for tests has no render state, and
//! the native `Net` sends nothing.

use super::*;
use eframe::egui::{Event, Modifiers, RawInput};
use perfr_live_protocol::encode_frame;

/// The app as a page with a service would build it.
pub(super) fn app() -> PerfrLiveApp {
    PerfrLiveApp::new(&eframe::CreationContext::_new_kittest(egui::Context::default()))
}

/// The app as the static site builds it: opened on `?capture=<url>`.
pub(super) fn static_app() -> PerfrLiveApp {
    let mut app = app();
    app.static_capture = Some("capture.perfr.stream".into());
    app
}

/// An app with a context and a clock, for tests that need whole frames.
pub(super) struct Page {
    pub app: PerfrLiveApp,
    pub ctx: egui::Context,
    frame: eframe::Frame,
    time: f64,
    size: Vec2,
    /// Every piece of text the last frame painted.
    texts: Vec<String>,
}

impl Page {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    /// `setup` runs before the first frame.
    pub fn with(setup: impl FnOnce(&mut PerfrLiveApp)) -> Self {
        let ctx = egui::Context::default();
        let mut app = PerfrLiveApp::new(&eframe::CreationContext::_new_kittest(ctx.clone()));
        setup(&mut app);
        Page {
            app,
            ctx,
            frame: eframe::Frame::_new_kittest(),
            time: 0.0,
            size: Vec2::new(1280.0, 800.0),
            texts: Vec::new(),
        }
    }

    pub fn sized(mut self, w: f32, h: f32) -> Self {
        self.size = Vec2::new(w, h);
        self
    }

    /// One `update` with these input events, a sixtieth of a second on.
    pub fn frame(&mut self, events: Vec<Event>) -> egui::FullOutput {
        self.frame_with(events, Modifiers::NONE)
    }

    pub fn frame_with(&mut self, events: Vec<Event>, modifiers: Modifiers) -> egui::FullOutput {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.time),
            modifiers,
            events,
            ..Default::default()
        };
        let (app, frame) = (&mut self.app, &mut self.frame);
        let out = self.ctx.run(input, |ctx| eframe::App::update(app, ctx, frame));
        self.texts.clear();
        for clipped in &out.shapes {
            collect_texts(&clipped.shape, &mut self.texts);
        }
        out
    }

    /// Whether the last frame painted exactly this text somewhere: a menu
    /// item or a label the `__perfr_ui` readout does not name.
    pub fn shows(&self, text: &str) -> bool {
        self.texts.iter().any(|t| t == text)
    }

    pub fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.frame(Vec::new());
        }
    }

    pub fn key(&mut self, key: Key) {
        self.frame(vec![key_event(key, true)]);
        self.frame(vec![key_event(key, false)]);
    }

    /// Where the last frame drew the widget the readout names `label`.
    pub fn rect_of(&self, label: &str) -> Option<Rect> {
        let rects: Vec<serde_json::Value> = serde_json::from_str(&self.app.ui_readout).ok()?;
        rects.iter().find_map(|r| {
            let r = r.as_array()?;
            if r.first()?.as_str()? != label {
                return None;
            }
            let n = |i: usize| r.get(i).and_then(|v| v.as_f64()).map(|v| v as f32);
            Some(Rect::from_min_size(Pos2::new(n(1)?, n(2)?), Vec2::new(n(3)?, n(4)?)))
        })
    }

    pub fn labels(&self) -> Vec<String> {
        let rects: Vec<serde_json::Value> = serde_json::from_str(&self.app.ui_readout).unwrap_or_default();
        rects
            .iter()
            .filter_map(|r| r.as_array()?.first()?.as_str().map(str::to_string))
            .collect()
    }

    pub fn press(&mut self, at: Pos2) {
        self.frame(vec![Event::PointerMoved(at)]);
        self.frame(vec![pointer_button(at, true)]);
    }

    pub fn release(&mut self, at: Pos2) {
        self.frame(vec![pointer_button(at, false)]);
    }

    pub fn click(&mut self, at: Pos2) {
        self.press(at);
        self.release(at);
        self.frame(Vec::new());
    }
}

fn collect_texts(shape: &Shape, out: &mut Vec<String>) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_texts(s, out)),
        Shape::Text(text) => out.push(text.galley.text().to_string()),
        _ => {}
    }
}

pub(super) fn key_event(key: Key, pressed: bool) -> Event {
    Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE }
}

pub(super) fn pointer_button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
}

pub(super) fn scope(pid: u32, tid: u32, start_ns: u64, duration_ns: u64, name_id: u32) -> LiveEvent {
    LiveEvent { start_ns, duration_ns, tid, pid, kind: kind::API_SCOPE, depth: 0, extra: 0, _pad: 0, name_id }
}

pub(super) fn sample(pid: u32, tid: u32, at_ns: u64) -> LiveEvent {
    LiveEvent { start_ns: at_ns, duration_ns: 1_000_000, tid, pid, kind: kind::SAMPLE, depth: 0, extra: 0, _pad: 0, name_id: 0 }
}

pub(super) fn hello() -> LiveFrame {
    LiveFrame::Hello { version: perfr_live_protocol::VERSION, event_size: std::mem::size_of::<LiveEvent>() as u16 }
}

pub(super) fn status_frame(capturing: bool, demo: bool) -> LiveFrame {
    LiveFrame::Status {
        capturing,
        demo,
        events_live: 0,
        events_capacity: 0,
        dropped: 0,
        spilled: 0,
        produced: 0,
        oldest_start_ns: 0,
        newest_end_ns: 0,
        ring_bytes: 1 << 26,
    }
}

/// `/api/status` as a service that can attach to processes answers it.
pub(super) fn service_status(capturing: bool) -> StatusJson {
    StatusJson { capturing, hooks: true, ring_bytes: 1 << 26, ..StatusJson::default() }
}

pub(super) fn stream(frames: &[LiveFrame]) -> Vec<u8> {
    frames.iter().flat_map(encode_frame).collect()
}

/// A capture as the service exports it: names, a status, events, the end.
pub(super) fn capture_stream(status: LiveFrame, events: Vec<LiveEvent>) -> Vec<u8> {
    stream(&[
        hello(),
        LiveFrame::InternedString { id: 7, text: "work".into() },
        LiveFrame::ProcessName { pid: 100, name: "target".into() },
        LiveFrame::ThreadName { pid: 100, tid: 101, name: "main".into() },
        status,
        LiveFrame::EventBatch { events },
        LiveFrame::CaptureFinished,
    ])
}

pub(super) fn feed(app: &mut PerfrLiveApp, frames: &[LiveFrame]) {
    app.ingest(&stream(frames)).expect("the test's own frames decode");
}

#[test]
fn the_app_is_built_without_a_browser_or_a_gpu() {
    let app = app();
    assert!(!app.has_gpu);
    assert_eq!(app.gpu_backend, "none");
    assert!(app.static_capture.is_none());
    assert_eq!(app.index.event_count(), 0);
}

#[test]
fn a_live_stream_lands_in_the_index_with_its_names() {
    let mut app = app();
    feed(
        &mut app,
        &[
            hello(),
            LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000 },
            LiveFrame::InternedString { id: 7, text: "work".into() },
            LiveFrame::ThreadName { pid: 100, tid: 101, name: "main".into() },
            LiveFrame::EventBatch { events: vec![scope(100, 101, 2_000, 500, 7), scope(100, 101, 3_000, 500, 7)] },
        ],
    );
    assert_eq!(app.hello_count, 1);
    assert_eq!(app.index.event_count(), 2);
    assert_eq!(app.intern.get(7), Some("work"));
    assert_eq!(app.thread_display_name(100, 101), "main");
    assert_eq!(app.capture_start_ns, 1_000);
    assert_eq!(app.live_edge_ns, 3_500);
    assert_eq!(app.selected_pid, Some(100), "a capture started elsewhere names its target");
}

#[test]
fn whole_frames_run_headless_and_publish_the_readouts() {
    let mut page = Page::new();
    feed(
        &mut page.app,
        &[
            hello(),
            LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000 },
            LiveFrame::ThreadName { pid: 100, tid: 101, name: "main".into() },
            LiveFrame::EventBatch { events: vec![scope(100, 101, 2_000, 500, 7)] },
        ],
    );
    page.frames(3);
    let labels = page.labels();
    for expected in ["Record", "More", "row:process:100", "row:thread:100:101"] {
        assert!(labels.iter().any(|l| l == expected), "{expected} is missing from {labels:?}");
    }
    assert!(page.rect_of("Record").is_some_and(|r| r.width() > 0.0));
    let sel: serde_json::Value = serde_json::from_str(&page.app.sel_readout).expect("__perfr_sel is JSON");
    assert_eq!(sel["events"], 1);
    assert_eq!(sel["hellos"], 1);
}

/// A static page that finished loading its `?capture=` stream.
fn loaded_static_app() -> PerfrLiveApp {
    let mut app = static_app();
    app.accept_stream(capture_stream(
        status_frame(false, false),
        vec![scope(100, 101, 5_000_000_000, 1_000_000, 7), scope(100, 101, 6_000_000_000, 1_000_000, 7)],
    ));
    app
}

#[test]
fn a_page_without_a_service_does_not_start_a_capture_over_its_file() {
    for start in [PerfrLiveApp::start_record, PerfrLiveApp::start_demo_path, PerfrLiveApp::start_bench] {
        let mut app = loaded_static_app();
        let view = (app.t0, app.t1);
        assert!(app.can_share());
        start(&mut app);
        assert!(!app.recording, "nothing answers a start, so nothing ever ends it");
        assert!(app.can_share(), "the stream Share posts was dropped");
        assert!(!app.follow, "the view left the file for a live edge that does not exist");
        assert_eq!((app.t0, app.t1), view);
        assert!(app.content_span().is_some());
        assert!(!app.wants_live_repaint());
    }
}

#[test]
fn a_narrow_static_page_has_no_record_demo_or_benchmark_control() {
    let mut page = Page::with(|app| *app = loaded_static_app()).sized(390.0, 800.0);
    page.frames(3);
    assert!(page.app.was_narrow);
    assert!(page.rect_of("Rec").is_none(), "Rec is drawn on a page with no service: {:?}", page.labels());
    let more = page.rect_of("More").expect("the narrow bar keeps More");
    page.click(more.center());
    page.frames(2);
    let labels = page.labels();
    assert!(labels.iter().any(|l| l == "Report"), "the More menu is open: {labels:?}");
    assert!(!labels.iter().any(|l| l == "Benchmark"), "{labels:?}");
    assert!(!page.shows("Demo"), "{:?}", page.texts);
}

#[test]
fn a_narrow_page_with_a_service_keeps_record_and_benchmark() {
    let mut page = Page::new().sized(390.0, 800.0);
    page.frames(3);
    assert!(page.rect_of("Rec").is_some(), "{:?}", page.labels());
    let more = page.rect_of("More").expect("the narrow bar keeps More");
    page.click(more.center());
    page.frames(2);
    let labels = page.labels();
    assert!(labels.iter().any(|l| l == "Benchmark"), "{labels:?}");
    assert!(page.shows("Demo"), "{:?}", page.texts);
}
