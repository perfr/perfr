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
    /// And where each was painted.
    text_rects: Vec<Rect>,
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
            text_rects: Vec::new(),
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
        self.run(RawInput { modifiers, events, ..Default::default() })
    }

    /// A file dropped on the page, as the browser hands it over: a name
    /// and its bytes.
    pub fn drop_file(&mut self, name: &str, bytes: &[u8]) {
        let file = egui::DroppedFile { name: name.into(), bytes: Some(bytes.into()), ..Default::default() };
        self.run(RawInput { dropped_files: vec![file], ..Default::default() });
    }

    fn run(&mut self, input: RawInput) -> egui::FullOutput {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.time),
            ..input
        };
        let (app, frame) = (&mut self.app, &mut self.frame);
        let out = self.ctx.run(input, |ctx| eframe::App::update(app, ctx, frame));
        self.texts.clear();
        self.text_rects.clear();
        for clipped in &out.shapes {
            collect_texts(&clipped.shape, &mut self.texts, &mut self.text_rects);
        }
        out
    }

    /// Where the last frame painted exactly this text.
    pub fn text_rect(&self, text: &str) -> Option<Rect> {
        self.texts.iter().position(|t| t == text).map(|i| self.text_rects[i])
    }

    /// Types into whatever has the keyboard focus.
    pub fn type_text(&mut self, text: &str) {
        self.frame(vec![Event::Text(text.into())]);
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

fn collect_texts(shape: &Shape, texts: &mut Vec<String>, rects: &mut Vec<Rect>) {
    match shape {
        Shape::Vec(shapes) => shapes.iter().for_each(|s| collect_texts(s, texts, rects)),
        Shape::Text(text) => {
            texts.push(text.galley.text().to_string());
            rects.push(text.visual_bounding_rect());
        }
        _ => {}
    }
}

/// Takes the path a page with a GPU takes: the instances are listed and a
/// paint callback is queued for them, which nothing here ever renders. The
/// branch without a GPU draws a message in place of the lanes and skips
/// part of the frame (the lifted row of a track drag among it).
pub(super) fn draw_as_with_a_gpu(app: &mut PerfrLiveApp) {
    app.has_gpu = true;
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

/// A page whose service has answered and is idle.
fn connected_app() -> PerfrLiveApp {
    let mut app = app();
    app.apply_status(service_status(false));
    app
}

#[test]
fn record_stays_pressed_while_the_service_is_still_starting_the_capture() {
    let mut app = connected_app();
    app.start_record();
    // Attaching can take seconds, and the status says "not capturing"
    // for all of them.
    for _ in 0..40 {
        app.apply_status(service_status(false));
    }
    assert!(app.recording);
    app.apply_status(service_status(true));
    assert!(app.recording);
}

#[test]
fn a_capture_ended_by_someone_else_releases_record() {
    let mut app = connected_app();
    app.start_record();
    app.apply_status(service_status(true));
    // Another viewer, the HTTP API or an agent stopped it.
    app.apply_status(service_status(false));
    assert!(!app.recording, "Record stays pressed with nothing capturing");
    assert!(!app.wants_live_repaint());
}

#[test]
fn a_demo_ended_by_someone_else_releases_record() {
    let mut app = app();
    app.apply_status(StatusJson::default());
    app.start_record();
    app.apply_status(StatusJson { demo: true, capturing: true, ..StatusJson::default() });
    assert!(app.recording);
    app.apply_status(StatusJson::default());
    assert!(!app.recording);
}

#[test]
fn a_capture_ended_by_someone_else_opens_the_report_like_any_finished_capture() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    let record = page.rect_of("Record").expect("the wide bar has Record");
    page.click(record.center());
    assert!(page.app.recording);
    page.app.apply_status(service_status(true));
    page.frames(2);
    assert!(!page.app.report_open);
    page.app.apply_status(service_status(false));
    page.frames(2);
    assert!(!page.app.recording);
    assert!(page.app.report_open, "the capture ended and the report did not open");
    assert_eq!(page.app.report_tab, ReportTab::TopDown);
}

#[test]
fn a_refused_start_releases_record_and_is_not_a_finished_capture() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    page.app.start_record();
    page.frames(2);
    assert!(page.app.capture_timer.running);
    page.app.apply_net_error("/api/capture/start: 409 cannot attach to pid 4242: Operation not permitted".into());
    assert!(!page.app.recording, "the service refused and Record stays pressed");
    page.frames(2);
    assert!(!page.app.report_open, "a capture that never started has no report to open");
    assert!(!page.app.capture_timer.running);
    assert_eq!(page.app.capture_timer.shown_s, 0.0);
    assert!(page.app.error.contains("Operation not permitted"));
}

#[test]
fn an_error_of_another_request_leaves_a_starting_capture_alone() {
    let mut app = connected_app();
    app.start_record();
    app.apply_net_error("/api/processes: 500 busy".into());
    assert!(app.recording);
    // And once the capture runs, a refused benchmark is not its end.
    app.apply_status(service_status(true));
    app.apply_net_error("/api/bench/start: 409 the demo is running; stop it first".into());
    assert!(app.recording);
}

#[test]
fn a_dropped_file_that_is_not_a_trace_says_so_where_it_can_be_seen() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    page.drop_file("notes.txt", b"hello");
    // The service keeps answering, four times a second.
    page.app.apply_status(service_status(false));
    page.frames(2);
    let message = "Not a Chrome or Perfetto trace: notes.txt";
    assert!(!page.app.report_open, "the Inspector tab is not where this test looks");
    assert!(page.shows(message), "nothing on screen says the drop failed: {:?}", page.texts);
    // It stays until dismissed.
    for _ in 0..20 {
        page.app.apply_status(service_status(false));
        page.frames(30);
    }
    assert!(page.shows(message));
    let close = page.rect_of("notice:dismiss").expect("the notice has a dismiss button");
    assert!(Rect::from_min_size(Pos2::ZERO, page.size).contains_rect(close));
    page.click(close.center());
    page.frames(2);
    assert!(!page.shows(message));
    assert!(page.app.notice.is_none());
}

#[test]
fn a_stream_file_that_cannot_be_read_says_so_and_the_next_file_clears_it() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    let mut cut = capture_stream(status_frame(false, false), vec![scope(100, 101, 5_000, 1_000, 7)]);
    cut.truncate(cut.len() - 3);
    page.drop_file("cut.perfr.stream", &cut);
    page.app.apply_status(service_status(false));
    page.frames(2);
    let notice = page.app.notice.clone().expect("a truncated stream is reported");
    assert!(page.shows(&notice), "{notice:?} is not on screen: {:?}", page.texts);
    page.drop_file("whole.perfr.stream", &capture_stream(status_frame(false, false), vec![scope(100, 101, 5_000, 1_000, 7)]));
    page.frames(2);
    assert!(page.app.notice.is_none(), "{:?}", page.app.notice);
}

#[test]
fn a_refused_start_tells_the_reader_why() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    page.app.start_record();
    page.app.apply_net_error("/api/capture/start: 409 cannot attach to pid 4242: Operation not permitted".into());
    page.app.apply_status(service_status(false));
    page.frames(2);
    assert!(
        page.texts.iter().any(|t| t.contains("cannot attach to pid 4242")),
        "the refusal is not on screen: {:?}",
        page.texts
    );
    // The next Record is a new attempt.
    page.app.start_record();
    page.frames(2);
    assert!(page.app.notice.is_none());
}

#[test]
fn a_failing_status_poll_is_not_a_notice() {
    let mut app = connected_app();
    app.apply_net_error("/api/status: 502 Bad Gateway".into());
    assert!(app.notice.is_none());
    assert_eq!(app.error, "/api/status: 502 Bad Gateway");
}

#[test]
fn a_long_notice_wraps_inside_a_phone_sized_page() {
    let mut page = Page::new().sized(390.0, 700.0);
    page.frames(2);
    page.app.fail(format!("{}: could not read the capture", "a-very-long-file-name-".repeat(12)));
    page.frames(3);
    let screen = Rect::from_min_size(Pos2::ZERO, page.size);
    for label in ["notice", "notice:dismiss"] {
        let rect = page.rect_of(label).expect("the notice is drawn");
        assert!(screen.contains_rect(rect), "{label} at {rect:?} leaves the page");
    }
}

#[test]
fn the_reason_of_a_refused_start_is_what_follows_the_status_code() {
    assert_eq!(refused_start("/api/capture/start: 409 no such process"), Some("no such process"));
    assert_eq!(refused_start("/api/demo/start: 409 demo producer is already running"), Some("demo producer is already running"));
    assert_eq!(refused_start("/api/bench/start: 409 "), Some(""));
    assert_eq!(refused_start("/api/capture/start: TypeError: Failed to fetch"), Some("TypeError: Failed to fetch"));
    assert_eq!(refused_start("/api/capture/stop: 409 not capturing"), None);
    assert_eq!(refused_start("/api/status: 500 x"), None);
}

/// A stream of `n` samples, as a file on disk would hold it.
fn sampled_stream(n: u64) -> Vec<u8> {
    capture_stream(status_frame(false, false), (0..n).map(|i| sample(100, 101, 5_000_000 + i * 1_000_000)).collect())
}

/// What the service last said about its own capture.
fn service_report() -> crate::net::SamplingReport {
    crate::net::SamplingReport { samples: 999, ..Default::default() }
}

#[test]
fn a_stream_opened_on_a_page_with_a_service_reports_its_own_samples() {
    let mut app = connected_app();
    app.sampling = Some(service_report());
    app.accept_stream(sampled_stream(3));
    assert!(app.capture_is_local());
    assert_eq!(
        app.sampling.as_ref().map(|r| r.samples),
        Some(3),
        "the panel still holds the report of the service's capture"
    );
    assert!(app.tree.is_some(), "and the call tree is computed with it");
}

#[test]
fn a_selection_on_an_opened_stream_is_reported_from_the_file() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.app.accept_stream(sampled_stream(4));
    page.frames(2);
    // Select the first two samples: nothing but this page can answer for
    // them, and the service is never asked, so its report would never come.
    page.app.sample_sels.push(TimeMeasure { start_ns: 4_000_000, stop_ns: 6_500_000, label_y: 0.0, sample_tid: None });
    page.frames(2);
    assert_eq!(page.app.sampling_ranges.len(), 1);
    assert_eq!(page.app.sampling.as_ref().map(|r| r.samples), Some(2));
}

#[test]
fn a_late_answer_about_the_services_capture_does_not_replace_the_files_report() {
    let mut app = connected_app();
    app.accept_stream(sampled_stream(3));
    app.apply_service_reports(Some(service_report()), None);
    assert_eq!(app.sampling.as_ref().map(|r| r.samples), Some(3));
    // Back on the service's capture, its answers count again.
    app.apply_frame(LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000 });
    assert!(!app.capture_is_local());
    app.apply_service_reports(Some(service_report()), None);
    assert_eq!(app.sampling.as_ref().map(|r| r.samples), Some(999));
}

#[test]
fn save_is_off_while_a_file_is_on_screen() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    let save = page.rect_of("Save").expect("the wide bar has Save");
    page.click(save.center());
    page.frames(2);
    assert!(page.shows("Capture (.perfr.zip)"), "Save opens its menu on a live page: {:?}", page.texts);
    page.key(Key::Escape);
    page.frames(2);
    assert!(!page.shows("Capture (.perfr.zip)"));

    // What Save downloads is the service's capture, not the file shown.
    page.app.accept_stream(sampled_stream(3));
    page.frames(2);
    let save = page.rect_of("Save").expect("Save keeps its place");
    page.click(save.center());
    page.frames(2);
    assert!(!page.shows("Capture (.perfr.zip)"), "Save offers the service's capture under a file's timeline");
}

/// A stream exported while its capture (or the demo) was still running:
/// the status frame inside says so.
fn stream_saved_mid_capture(demo: bool) -> Vec<u8> {
    capture_stream(status_frame(true, demo), vec![scope(100, 101, 5_000_000_000, 1_000_000, 7)])
}

#[test]
fn a_stream_saved_mid_capture_is_a_finished_capture_on_a_static_page() {
    for demo in [false, true] {
        let mut page = Page::with(|app| {
            *app = static_app();
            app.accept_stream(stream_saved_mid_capture(demo));
        });
        page.frames(10);
        let app = &page.app;
        assert!(!app.status.capturing && !app.status.demo, "the file's status still says a capture runs");
        assert!(!app.wants_live_repaint(), "the page repaints at display rate for a capture that is over");
        assert!(!app.capture_timer.running, "the timer counts on");
        let sel: serde_json::Value = serde_json::from_str(&app.sel_readout).unwrap();
        assert_eq!(sel["recording"], false);
    }
}

#[test]
fn a_stream_saved_mid_capture_is_not_a_capture_ending_on_a_live_page() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    page.drop_file("mid.perfr.stream", &stream_saved_mid_capture(false));
    page.frames(2);
    // The service's next answer: it never captured.
    page.app.apply_status(service_status(false));
    page.frames(2);
    assert!(!page.app.report_open, "the file's stale status read as a capture that just stopped");
}

/// A connected page with the Self pane open and its timeline drawing.
fn page_with_self_pane() -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.self_pane_open = true;
    });
    page.frames(6);
    assert!(page.app.self_tl.index.event_count() > 0, "the Self pane has frames of its own to draw");
    page
}

#[test]
fn x_starts_a_capture_with_the_self_pane_open() {
    let mut page = page_with_self_pane();
    let self_follow = page.app.self_tl.follow;
    let self_edge = page.app.self_tl.live_edge_ns;
    page.key(Key::X);
    assert!(page.app.recording, "the second timeline of the frame saw the same key press and stopped it");
    assert_eq!(page.app.live_edge_ns, DEMO_ORIGIN_NS, "the capture's timeline is the one that starts");
    assert_eq!(
        (page.app.self_tl.follow, page.app.self_tl.live_edge_ns >= self_edge),
        (self_follow, true),
        "Record ran against the Self pane's timeline"
    );
    page.key(Key::X);
    assert!(!page.app.recording);
}

#[test]
fn a_compact_story_page_has_fullscreen_on_its_bar_and_the_steps_text_on_screen() {
    let labels = |page: &Page| -> Vec<String> {
        let rects: Vec<serde_json::Value> = serde_json::from_str(&page.app.ui_readout).expect("the readout is JSON");
        rects.iter().filter_map(|r| r[0].as_str().map(str::to_string)).collect()
    };
    // What an iframe gets: `?story=demo&chrome=compact`.
    let mut page = Page::with(|app| {
        app.story = crate::story::StorySession::from_query(Some("demo"), true);
    });
    page.frames(3);
    let on_bar = labels(&page);
    for label in ["story:play", "story:prev", "story:next", "story:steps", "story:fullscreen"] {
        assert!(on_bar.iter().any(|l| l == label), "{label} is not on the bar: {on_bar:?}");
    }
    // One row: the fullscreen button is level with Play, at the right end.
    let rect = |label: &str| {
        let rects: Vec<serde_json::Value> = serde_json::from_str(&page.app.ui_readout).unwrap();
        let r = rects.iter().find(|r| r[0] == label).unwrap().clone();
        (r[1].as_f64().unwrap(), r[2].as_f64().unwrap())
    };
    let (play, full) = (rect("story:play"), rect("story:fullscreen"));
    assert_eq!(play.1, full.1, "the two are on one row");
    assert!(full.0 > play.0 + 200.0, "fullscreen is at the far end: {play:?} {full:?}");
    // A page for reading: no editor, no transport bar.
    assert!(!page.texts.iter().any(|t| t == "Edit" || t == "Close"), "{:?}", page.texts);
    assert!(!on_bar.iter().any(|l| l == "Record"), "{on_bar:?}");
    // The step's sentences are on screen without opening anything.
    assert!(
        page.texts.iter().any(|t| t.starts_with("This is the box3d demo capture")),
        "the current step's text is not shown: {:?}",
        page.texts
    );
    // The button asks for fullscreen; on the full viewer the story bar
    // leaves that to the transport bar and keeps its editor.
    let mut full_viewer = Page::with(|app| app.story.load_demo());
    full_viewer.frames(3);
    assert!(!labels(&full_viewer).iter().any(|l| l == "story:fullscreen"));
    assert!(full_viewer.texts.iter().any(|t| t == "Edit"));
}

#[test]
fn a_story_key_moves_one_step_with_the_self_pane_open() {
    let mut page = page_with_self_pane();
    page.app.story.load_demo();
    page.frames(3);
    assert!(page.texts.iter().any(|t| t.starts_with("1 / ")), "the story bar shows step 1: {:?}", page.texts);
    page.key(Key::CloseBracket);
    page.frames(2);
    assert!(page.texts.iter().any(|t| t.starts_with("2 / ")), "one press of ] is one step: {:?}", page.texts);
}

#[test]
fn a_story_drives_the_captures_timeline_not_the_self_panes() {
    let mut page = page_with_self_pane();
    feed(
        &mut page.app,
        &[
            LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000_000_000 },
            LiveFrame::EventBatch { events: vec![scope(100, 101, 1_000_000_000, 4_000_000_000, 7)] },
            LiveFrame::CaptureFinished,
        ],
    );
    page.frames(2);
    assert!(page.app.self_tl.follow);
    page.app.story.load_demo();
    page.frames(4);
    assert!(page.app.self_tl.follow, "the story's camera was written into the Self pane's window");
    assert!(!page.app.follow, "and the capture's timeline is the one a story poses");
}

/// `n` functions as `/api/functions` lists them for a large binary.
fn function_list(n: usize) -> Vec<FunctionHit> {
    (0..n)
        .map(|i| FunctionHit {
            function_id: i as u64 + 1,
            name: format!("engine::subsystem_{:03}::Component::update_stage_{i:06}", i % 311),
            module: format!("libengine_{}.so", i % 17),
            size: (i as u64 * 37) % 4096,
            safety: String::new(),
            safety_reason: String::new(),
        })
        .collect()
}

/// A connected page showing the Functions tab over `n` listed functions.
fn functions_page(n: usize) -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.selected_pid = Some(100);
        app.loaded_symbol_pid = Some(100);
        app.symbols.status = "ready".into();
        app.functions_pid = Some(100);
        app.set_functions(function_list(n), n);
        app.open_right_tab(ReportTab::Functions);
    });
    page.frames(3);
    assert!(page.texts.iter().any(|t| t.contains(&format!("{n} functions"))), "the list is drawn: {:?}", page.texts);
    page
}

fn ms_per_frame(page: &mut Page, frames: usize) -> f64 {
    let start = std::time::Instant::now();
    page.frames(frames);
    start.elapsed().as_secs_f64() * 1000.0 / frames as f64
}

/// Not a check: prints what a frame costs with each report tab's worst
/// case on screen. `cargo test --release -- --ignored --nocapture report_tab_frame_times`.
#[test]
#[ignore]
fn report_tab_frame_times() {
    let mut page = functions_page(200_000);
    println!("functions 200k, no filter:      {:8.2} ms/frame", ms_per_frame(&mut page, 20));
    page.app.report_filter = "stage_0001".into();
    page.frames(2);
    println!("functions 200k, filter:         {:8.2} ms/frame", ms_per_frame(&mut page, 20));
    page.app.report_filter.clear();
    page.app.functions_sort = (2, true);
    page.frames(2);
    println!("functions 200k, sorted by size: {:8.2} ms/frame", ms_per_frame(&mut page, 20));
    let mut idle = Page::with(|app| app.apply_status(service_status(false)));
    idle.frames(3);
    println!("no report panel:                {:8.2} ms/frame", ms_per_frame(&mut idle, 20));

    // Flat: a report of 20,000 functions.
    let mut flat = Page::with(|app| {
        app.apply_status(service_status(false));
        app.sampling = Some(crate::net::SamplingReport {
            samples: 1_000_000,
            rows: (0..20_000u64)
                .map(|i| crate::net::SamplingRow {
                    name: format!("engine::subsystem::Component::update_stage_{i:06}"),
                    module: format!("libengine_{}.so", i % 17),
                    self_count: 20_000 - i,
                    inclusive_count: 40_000 - i,
                    self_percent: 0.1,
                    inclusive_percent: 0.2,
                    function_id: i + 1,
                })
                .collect(),
            ..Default::default()
        });
        app.open_right_tab(ReportTab::Flat);
    });
    flat.frames(3);
    println!("flat 20k rows:                  {:8.2} ms/frame", ms_per_frame(&mut flat, 20));

    // Top-down: 200 threads of 250-deep stacks, 50,000 nodes, roots open.
    fn chain(depth: usize, tag: usize) -> crate::net::TreeNodeJson {
        let mut node = crate::net::TreeNodeJson { name: format!("leaf_{tag}"), inclusive: 1, ..Default::default() };
        for d in 0..depth {
            node = crate::net::TreeNodeJson {
                name: format!("frame_{tag}_{d}"),
                module: "libengine.so".into(),
                inclusive: 1,
                children: vec![node],
                ..Default::default()
            };
        }
        node
    }
    let mut tree = Page::with(|app| {
        app.apply_status(service_status(false));
        app.tree = Some(Arc::new(crate::net::SamplingTree {
            mode: "top_down".into(),
            samples: 200,
            roots: (0..200).map(|t| chain(250, t)).collect(),
            ..Default::default()
        }));
        app.open_right_tab(ReportTab::TopDown);
    });
    tree.frames(3);
    println!("top-down 50k nodes, collapsed:  {:8.2} ms/frame", ms_per_frame(&mut tree, 20));

    // Live: 20,000 distinct scope names.
    let mut live = Page::with(|app| {
        app.apply_status(service_status(false));
        let events: Vec<LiveEvent> = (0..20_000u32).map(|i| scope(100, 101, 1_000 + i as u64 * 10, 5, 1_000 + i)).collect();
        let mut frames = vec![hello(), LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000 }];
        frames.extend((0..20_000u32).map(|i| LiveFrame::InternedString { id: 1_000 + i, text: format!("scope_{i:05}") }));
        frames.push(LiveFrame::EventBatch { events });
        feed(app, &frames);
        app.open_right_tab(ReportTab::Live);
    });
    live.frames(3);
    println!("live 20k names:                 {:8.2} ms/frame", ms_per_frame(&mut live, 20));
}

/// The ids of the rows the Functions tab offers to copy, in view order.
fn copy_ids(page: &Page) -> Vec<u64> {
    page.app.report_view.rows.iter().map(|r| r.id).collect()
}

#[test]
fn the_functions_tab_follows_its_filter_sort_list_and_hooks() {
    let mut page = functions_page(40);
    assert_eq!(copy_ids(&page), (1..=40).collect::<Vec<u64>>(), "the list arrives alphabetical and is shown so");
    assert_eq!(page.app.report_view.rows[0].cols.len(), 3);

    // A filter narrows the rows, and clearing it brings them back.
    page.app.report_filter = "stage_00003".into();
    page.frames(2);
    assert_eq!(copy_ids(&page), (31..=40).collect::<Vec<u64>>());
    page.app.report_filter.clear();
    page.frames(2);
    assert_eq!(copy_ids(&page).len(), 40);

    // A sort on another column reorders them.
    page.app.functions_sort = (2, true);
    page.frames(2);
    let sizes: Vec<u64> = copy_ids(&page).iter().map(|id| ((id - 1) * 37) % 4096).collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "largest first: {sizes:?}");

    // Sorted by the hooked column, a newly hooked function moves.
    page.app.functions_sort = (0, true);
    page.frames(2);
    let before = copy_ids(&page);
    let hook = page.app.functions[20].clone();
    page.app.selected_hooks.push(hook);
    page.frames(2);
    assert_eq!(copy_ids(&page)[0], 21, "the hooked function leads");
    assert_ne!(copy_ids(&page), before);

    // More functions from the service's search join the list.
    page.app.functions_sort = (1, false);
    let mut more = function_list(45);
    more.drain(..40);
    page.app.add_functions(more);
    page.frames(2);
    assert_eq!(copy_ids(&page).len(), 45);

    // Another tab's rows replace the copy view, and coming back restores it.
    page.app.sampling = Some(crate::net::SamplingReport {
        samples: 1,
        rows: vec![crate::net::SamplingRow {
            name: "hot".into(),
            module: "m".into(),
            self_count: 1,
            inclusive_count: 1,
            self_percent: 100.0,
            inclusive_percent: 100.0,
            function_id: 777,
        }],
        ..Default::default()
    });
    page.app.report_tab = ReportTab::Flat;
    page.frames(2);
    assert_eq!(copy_ids(&page), vec![777]);
    page.app.report_tab = ReportTab::Functions;
    page.frames(2);
    assert_eq!(copy_ids(&page).len(), 45);

    // A new list for the process replaces the old one.
    page.app.set_functions(function_list(7), 7);
    page.frames(2);
    assert_eq!(copy_ids(&page).len(), 7);
}

/// A top-down tree: two threads, `main` over `work` over a leaf each.
fn small_tree() -> crate::net::SamplingTree {
    let node = |name: &str, id: u64, inclusive: u64, children: Vec<crate::net::TreeNodeJson>| crate::net::TreeNodeJson {
        kind: if id == 0 { "thread".into() } else { "function".into() },
        name: name.into(),
        module: "app".into(),
        function_id: id,
        inclusive,
        exclusive: if children.is_empty() { inclusive } else { 0 },
        inclusive_percent: inclusive as f64,
        of_parent_percent: 100.0,
        children,
        ..Default::default()
    };
    crate::net::SamplingTree {
        mode: "top_down".into(),
        samples: 100,
        roots: vec![
            node("Thread 1", 0, 60, vec![node("main", 1, 60, vec![node("work", 2, 60, vec![node("leaf_a", 3, 60, vec![])])])]),
            node("Thread 2", 0, 40, vec![node("main", 1, 40, vec![node("idle", 4, 40, vec![])])]),
        ],
        ..Default::default()
    }
}

/// The names of the rows the tree tab offers to copy, indentation included.
fn tree_rows(page: &Page) -> Vec<String> {
    page.app.report_view.rows.iter().map(|r| r.cols[3].clone()).collect()
}

#[test]
fn the_tree_tab_walks_what_is_expanded_in_sorted_order() {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.apply_service_reports(None, Some(small_tree()));
        app.open_right_tab(ReportTab::TopDown);
    });
    page.frames(2);
    // The tree arrives open along every path (the threshold is 0).
    assert_eq!(
        tree_rows(&page),
        ["Thread 1", "  main", "    work", "      leaf_a", "Thread 2", "  main", "    idle"]
    );
    // Folding a node hides what is under it and nothing else.
    page.app.tree_expanded.remove("0/0");
    page.frames(2);
    assert_eq!(tree_rows(&page), ["Thread 1", "  main", "Thread 2", "  main", "    idle"]);
    // A filter draws the paths to the matches, open.
    page.app.report_filter = "leaf".into();
    page.frames(2);
    assert_eq!(tree_rows(&page), ["Thread 1", "  main", "    work", "      leaf_a"]);
    // Ascending by inclusive puts the lighter thread first.
    page.app.report_filter.clear();
    page.app.tree_sort = (1, false);
    page.frames(2);
    assert_eq!(tree_rows(&page)[0], "Thread 2");
}

/// A connected page with the Inspector tab open and the service's ring
/// size in its box.
fn inspector_page() -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.open_right_tab(ReportTab::Inspector);
    });
    page.frames(3);
    assert_eq!(page.app.ring_bytes, "67108864");
    page
}

#[test]
fn a_ring_size_being_typed_survives_the_status_polls() {
    let mut page = inspector_page();
    let ring_box = page.text_rect("67108864").expect("the Inspector shows the ring size in its box");
    page.click(ring_box.center());
    page.type_text("0");
    assert_eq!(page.app.ring_bytes.len(), 9, "the click put the caret in the box: {:?}", page.app.ring_bytes);
    let typed = page.app.ring_bytes.clone();
    // The service answers four times a second, and pushes its stats over
    // the socket besides.
    for _ in 0..8 {
        page.app.apply_status(service_status(false));
        page.app.apply_frame(status_frame(false, false));
        page.frames(15);
    }
    assert_eq!(page.app.ring_bytes, typed, "a status answer put the service's value back over what was typed");
}

#[test]
fn a_spill_path_being_typed_survives_the_status_polls() {
    let mut page = inspector_page();
    let hint = page.text_rect("/tmp/perfr-spill").expect("the empty spill path box shows its hint");
    page.click(hint.center());
    page.type_text("/data/spill");
    assert_eq!(page.app.spill_path, "/data/spill");
    page.app.apply_status(StatusJson { spill_path: Some("/var/old".into()), ..service_status(false) });
    page.frames(2);
    assert_eq!(page.app.spill_path, "/data/spill");
}

#[test]
fn the_ring_and_spill_boxes_follow_the_service_until_they_are_edited_and_again_after_apply() {
    let mut page = inspector_page();
    // Untouched, the boxes show what the service reports.
    page.app.apply_status(StatusJson { ring_bytes: 1 << 20, spill_path: Some("/var/spill".into()), ..service_status(false) });
    page.frames(2);
    assert_eq!((page.app.ring_bytes.as_str(), page.app.spill_path.as_str()), ("1048576", "/var/spill"));

    let ring_box = page.text_rect("1048576").expect("the ring size");
    page.click(ring_box.center());
    page.type_text("0");
    let typed = page.app.ring_bytes.clone();
    page.app.apply_status(StatusJson { ring_bytes: 1 << 20, ..service_status(false) });
    page.frames(2);
    assert_eq!(page.app.ring_bytes, typed);

    // Apply sends it; from then on the box shows what the service made of it.
    let apply = page.rect_of("Apply").expect("the Inspector has Apply");
    page.click(apply.center());
    page.app.apply_status(StatusJson { ring_bytes: 1 << 24, ..service_status(false) });
    page.frames(2);
    assert_eq!(page.app.ring_bytes, "16777216");
}

/// A connected page showing a finished capture of one process, three threads.
fn three_thread_page() -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        draw_as_with_a_gpu(app);
        feed(
            app,
            &[
                hello(),
                LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000_000 },
                LiveFrame::EventBatch {
                    events: (0..3).map(|t| scope(100, 101 + t, 2_000_000 + t as u64 * 1_000, 5_000_000, 7)).collect(),
                },
                LiveFrame::CaptureFinished,
            ],
        );
    });
    page.frames(4);
    page
}

impl Page {
    /// Presses on `from` and drags to `to` in a few steps, button still down.
    fn drag(&mut self, from: Pos2, to: Pos2) {
        self.press(from);
        for step in 1..=6 {
            let at = from + (to - from) * (step as f32 / 6.0);
            self.frame(vec![Event::PointerMoved(at)]);
        }
    }
}

#[test]
fn a_track_dropped_outside_the_lanes_is_dropped() {
    let mut page = three_thread_page();
    let header = page.rect_of("row:thread:100:102").expect("the thread's header");
    let grab = Pos2::new(header.left() + 60.0, header.center().y);
    // Up into the toolbar, a row height and more above the lanes.
    let toolbar = Pos2::new(grab.x, 6.0);
    page.drag(grab, toolbar);
    assert!(page.app.tracks.dragging(), "the header is lifted");
    page.release(toolbar);
    page.frames(2);
    assert!(!page.app.tracks.any_dragging(), "the button is up and the row still follows the pointer");
    assert!(!page.app.wants_live_repaint());
}

#[test]
fn a_track_dropped_on_the_rail_still_lands_where_it_was_dropped() {
    let mut page = three_thread_page();
    let order = |page: &Page| -> Vec<u32> { page.app.tracks.thread_order().iter().map(|t| t.tid).collect() };
    let before = order(&page);
    assert_eq!(before.len(), 3);
    let first = page.rect_of(&format!("row:thread:100:{}", before[0])).expect("first header");
    let last = page.rect_of(&format!("row:thread:100:{}", before[2])).expect("last header");
    let grab = Pos2::new(first.left() + 60.0, first.center().y);
    let below = Pos2::new(grab.x, last.bottom() + 4.0);
    page.drag(grab, below);
    page.release(below);
    page.frames(2);
    assert!(!page.app.tracks.any_dragging());
    assert_eq!(order(&page), vec![before[1], before[2], before[0]], "dragged from the top to under the last");
}

#[test]
fn a_track_drag_ends_on_release_without_a_gpu_too() {
    let mut page = three_thread_page();
    // The lifted row is drawn by the GPU branch of the frame only.
    page.app.has_gpu = false;
    page.frames(2);
    let first = page.rect_of("row:thread:100:101").or(page.rect_of("row:thread:100:102")).expect("a header");
    let grab = Pos2::new(first.left() + 60.0, first.center().y);
    let to = grab + Vec2::new(0.0, 30.0);
    page.drag(grab, to);
    assert!(page.app.tracks.dragging());
    page.release(to);
    page.frames(2);
    assert!(!page.app.tracks.any_dragging());
}

/// The three-thread page with a thread selected and a scope search on.
fn page_with_a_selection_and_a_search() -> Page {
    let mut page = three_thread_page();
    page.app.selected_thread = Some((100, 102));
    page.app.search = "work".into();
    page.frames(2);
    page
}

#[test]
fn escape_in_the_tracks_filter_clears_that_filter_and_nothing_else() {
    let mut page = page_with_a_selection_and_a_search();
    let filter = page.rect_of("filter:tracks").expect("the tracks box");
    page.click(filter.center());
    page.type_text("zz");
    assert_eq!(page.app.track_filter, "zz");
    page.key(Key::Escape);
    assert_eq!(page.app.track_filter, "", "Escape clears the box it was typed in");
    assert_eq!(page.app.selected_thread, Some((100, 102)), "and it took the selection with it");
    assert_eq!(page.app.search, "work", "and the scope search");
}

#[test]
fn escape_in_the_scope_search_clears_the_search_and_keeps_the_selection() {
    let mut page = three_thread_page();
    page.app.selected_thread = Some((100, 102));
    page.frames(2);
    let search = page.text_rect("scope").expect("the empty search box shows its hint");
    page.click(search.center());
    page.type_text("work");
    assert_eq!(page.app.search, "work");
    page.key(Key::Escape);
    assert_eq!(page.app.search, "");
    assert_eq!(page.app.selected_thread, Some((100, 102)));
}

#[test]
fn escape_over_the_timeline_still_clears_the_search_and_the_selection() {
    let mut page = page_with_a_selection_and_a_search();
    page.key(Key::Escape);
    assert_eq!(page.app.search, "");
    assert_eq!(page.app.selected_thread, None);
    // And again after a text box was used and left.
    page.app.selected_thread = Some((100, 102));
    let filter = page.rect_of("filter:tracks").expect("the tracks box");
    page.click(filter.center());
    page.key(Key::Escape);
    assert_eq!(page.app.selected_thread, Some((100, 102)));
    page.key(Key::Escape);
    assert_eq!(page.app.selected_thread, None, "the second Escape is the timeline's");
}

#[test]
fn a_browser_shortcut_on_a_navigation_key_does_not_move_the_view() {
    let mut page = three_thread_page();
    let view = (page.app.t0, page.app.t1);
    for key in [Key::S, Key::W, Key::A, Key::D] {
        let held = Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::CTRL };
        page.frame_with(vec![held], Modifiers::CTRL);
        page.frame_with(Vec::new(), Modifiers::CTRL);
        let up = Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::CTRL };
        page.frame_with(vec![up], Modifiers::CTRL);
        assert_eq!((page.app.t0, page.app.t1), view, "Ctrl+{key:?} panned or zoomed");
    }
    // The bare key still does.
    page.frame(vec![key_event(Key::S, true)]);
    page.frames(3);
    page.frame(vec![key_event(Key::S, false)]);
    assert_ne!((page.app.t0, page.app.t1), view);
}

/// What the last timeline payload decided to upload: "Full", "Flags" or "Skip".
fn upload_mode_of(page: &Page) -> String {
    let draw: serde_json::Value = serde_json::from_str(&page.app.draw_readout).expect("the draw readout is JSON");
    draw["mode"].as_str().expect("a mode").to_string()
}

/// A finished capture of two scopes with different names, drawn as with a GPU.
fn two_name_page() -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        draw_as_with_a_gpu(app);
        feed(
            app,
            &[
                hello(),
                LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000_000 },
                LiveFrame::InternedString { id: 7, text: "alpha".into() },
                LiveFrame::InternedString { id: 8, text: "beta".into() },
                LiveFrame::EventBatch { events: vec![scope(100, 101, 2_000_000, 500_000_000, 7), scope(100, 101, 600_000_000, 500_000_000, 8)] },
                LiveFrame::CaptureFinished,
            ],
        );
    });
    page.frames(6);
    page
}

#[test]
fn a_search_for_another_name_with_as_many_matches_is_drawn() {
    let mut page = two_name_page();
    // A click on a Live row, a flame bar, "Highlight every instance".
    page.app.search = "alpha".into();
    page.frames(4);
    assert_eq!(page.app.search_ids.len(), 1);
    assert_eq!(upload_mode_of(&page), "Skip", "the frame is settled");
    page.app.search = "beta".into();
    page.frame(Vec::new());
    assert_eq!(page.app.search_ids.len(), 1, "one match again");
    assert_ne!(upload_mode_of(&page), "Skip", "the timeline keeps highlighting the previous search");
    page.frames(3);
    assert_eq!(upload_mode_of(&page), "Skip", "and settles again");
}

/// The arrows `paint_flow_arrows` draws for `flows` over `layout`: each
/// curve's four control points, in the order painted.
fn flow_curves(layout: &[(LaneKey, f32)], flows: &[FlowEdge]) -> Vec<[Pos2; 4]> {
    let ctx = egui::Context::default();
    let body = Rect::from_min_size(Pos2::new(200.0, 60.0), Vec2::new(1000.0, 600.0));
    let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))), ..Default::default() };
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| paint_flow_arrows(ui, body, 0.0, 1_000_000.0, layout, flows, 1.0));
    });
    fn curves(shape: &Shape, out: &mut Vec<[Pos2; 4]>) {
        match shape {
            Shape::Vec(shapes) => shapes.iter().for_each(|s| curves(s, out)),
            Shape::CubicBezier(curve) => out.push(curve.points),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &out.shapes {
        curves(&clipped.shape, &mut found);
    }
    found
}

fn lane(pid: u32, tid: u32, depth: u8) -> LaneKey {
    LaneKey { pid, tid, kind: kind::API_SCOPE, depth, extra: 0 }
}

fn flow(from: (u32, u32, u64), to: (u32, u32, u64)) -> FlowEdge {
    let end = |(pid, tid, start_ns): (u32, u32, u64)| perfr_live_chrome::FlowEnd { start_ns, pid, tid, name_id: 0 };
    FlowEdge { from: end(from), to: end(to) }
}

#[test]
fn flow_arrows_join_the_first_lane_of_each_thread_and_skip_what_is_off_screen() {
    // Thread 1 has two lanes (depths): an arrow goes to the first listed.
    let layout = [(lane(1, 1, 0), 10.0), (lane(1, 1, 1), 40.0), (lane(1, 2, 0), 90.0)];
    let flows = [
        flow((1, 1, 100_000), (1, 2, 300_000)),
        // Both ends right of the view: not drawn.
        flow((1, 1, 5_000_000), (1, 2, 6_000_000)),
        // A thread with no lane: not drawn.
        flow((1, 1, 100_000), (9, 9, 300_000)),
        flow((1, 2, 500_000), (1, 1, 900_000)),
    ];
    let curves = flow_curves(&layout, &flows);
    assert_eq!(curves.len(), 2);
    let h = lane_height(lane(1, 1, 0)) * 0.5;
    let y1 = 60.0 + 10.0 + h;
    let y2 = 60.0 + 90.0 + lane_height(lane(1, 2, 0)) * 0.5;
    assert_eq!(curves[0][0], Pos2::new(300.0, y1));
    assert_eq!(curves[0][3], Pos2::new(500.0, y2));
    assert_eq!(curves[1][0], Pos2::new(700.0, y2));
    assert_eq!(curves[1][3], Pos2::new(1100.0, y1));
}

/// Not a check: what a frame's flow arrows cost on a trace with many of
/// them in view. `cargo test --release -- --ignored --nocapture flow_arrow_times`.
#[test]
#[ignore]
fn flow_arrow_times() {
    let layout: Vec<(LaneKey, f32)> = (0..500u32).map(|t| (lane(1, t, 0), t as f32 * 20.0)).collect();
    let flows: Vec<FlowEdge> = (0..100_000u64)
        .map(|i| flow((1, (i % 500) as u32, i * 10), (1, ((i * 7 + 250) % 500) as u32, i * 10 + 500)))
        .collect();
    let start = std::time::Instant::now();
    let mut drawn = 0;
    for _ in 0..5 {
        drawn = flow_curves(&layout, &flows).len();
    }
    println!("100k flows over 500 lanes: {:8.2} ms/frame ({drawn} arrows)", start.elapsed().as_secs_f64() * 1000.0 / 5.0);
}

/// A finished capture of `threads` threads in one process, drawn as with a GPU.
fn many_thread_page(threads: u32) -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        draw_as_with_a_gpu(app);
        let mut frames = vec![hello(), LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000_000 }];
        frames.extend((0..threads).map(|t| LiveFrame::ThreadName { pid: 100, tid: 1_000 + t, name: format!("worker-{t}") }));
        frames.push(LiveFrame::EventBatch {
            events: (0..threads).map(|t| scope(100, 1_000 + t, 2_000_000 + t as u64, 5_000_000, 7)).collect(),
        });
        frames.push(LiveFrame::CaptureFinished);
        feed(app, &frames);
    });
    page.frames(6);
    page
}

/// Not a check: an idle frame of the timeline over a capture with many
/// threads. `cargo test --release -- --ignored --nocapture timeline_frame_times`.
#[test]
#[ignore]
fn timeline_frame_times() {
    let mut page = many_thread_page(5_000);
    println!("5,000 threads, idle frame:      {:8.3} ms/frame", ms_per_frame(&mut page, 200));
}

#[test]
fn the_ui_readout_is_the_json_it_always_was() {
    // What `take_ui_rects_json` wrote before it built the text in place:
    // Rust's debug form of the label, and whole-number geometry.
    let reference = |rects: &[(&str, Rect)]| {
        let items: Vec<String> = rects
            .iter()
            .map(|(label, r)| format!("[{:?},{:.0},{:.0},{:.0},{:.0}]", label, r.left(), r.top(), r.width(), r.height()))
            .collect();
        format!("[{}]", items.join(","))
    };
    let rects = [
        ("Record", Rect::from_min_size(Pos2::new(21.4, 7.5), Vec2::new(28.0, 22.0))),
        ("row:thread:100:101", Rect::from_min_size(Pos2::new(0.0, 85.6), Vec2::new(196.0, 21.0))),
        ("hook:ns::f(const char*, \"x\")", Rect::from_min_size(Pos2::new(-3.6, 1e6), Vec2::new(0.4, 0.5))),
        ("tree:a\\b\tc\nd", Rect::NOTHING),
        ("symbols-status:Waiting for symbols…", Rect::from_min_size(Pos2::new(700.0, 9.0), Vec2::new(151.5, 14.0))),
        ("", Rect::ZERO),
    ];
    let _ = take_ui_rects_json();
    assert_eq!(take_ui_rects_json(), "[]");
    for (label, rect) in rects {
        note_ui_rect(label, rect);
    }
    assert_eq!(take_ui_rects_json(), reference(&rects));
    assert_eq!(take_ui_rects_json(), "[]", "taking empties it");
    note_ui_rect("one", Rect::ZERO);
    assert_eq!(take_ui_rects_json(), r#"[["one",0,0,0,0]]"#);
}

#[test]
fn a_name_with_a_control_character_leaves_the_readouts_parseable() {
    // A function name with an escape sequence in it, as a mangled or
    // user-supplied scope name can have.
    let odd = "paint\u{1b}[31m\u{0}red\u{7f}";
    let mut tree = small_tree();
    tree.roots[0].children[0].name = odd.into();
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.apply_service_reports(None, Some(tree));
        app.open_right_tab(ReportTab::TopDown);
        app.report_filter = "\u{1b}".into();
    });
    page.frames(3);
    let rects: Vec<serde_json::Value> =
        serde_json::from_str(&page.app.ui_readout).expect("window.__perfr_ui is JSON whatever the names are");
    assert!(rects.iter().any(|r| r[0] == format!("tree:{odd}")), "the row is named as it is");
    let sel: serde_json::Value =
        serde_json::from_str(&page.app.sel_readout).expect("window.__perfr_sel is JSON whatever was typed");
    assert_eq!(sel["report_filter"], "\u{1b}");
}

/// An app with every piece of per-capture state set: a stream of process
/// 100 on screen and held for Share, names, a selection of every kind,
/// reports, a half-read frame.
fn app_with_everything_set() -> PerfrLiveApp {
    let mut app = connected_app();
    app.accept_stream(capture_stream(status_frame(false, false), vec![scope(100, 101, 5_000, 1_000, 7)]));
    app.tracks.ensure_catalogue(&app.index);
    let pick = ScopePick::from_event(scope(100, 101, 5_000, 1_000, 7));
    let measure = TimeMeasure { start_ns: 5_000, stop_ns: 5_500, label_y: 0.0, sample_tid: None };
    app.selected = Some(pick);
    app.hover = Some(pick);
    app.selected_thread = Some((100, 101));
    app.scope_report = Some((7, "work".into()));
    app.scope_menu = Some((pick, Pos2::ZERO));
    app.sample_sels = vec![measure];
    app.measure = Some(measure);
    app.sampling = Some(service_report());
    app.tree = Some(Arc::new(small_tree()));
    app.sampling_ranges = vec![(5_000, 5_500, None)];
    app.rect_result = Some(RectResult::default());
    app.flame_zoom = vec![0];
    app.report_selection.insert(1);
    app.live_focus = Some(crate::live::LiveKey { pid: 100, name_id: 7, kind: kind::API_SCOPE });
    app.live_order = vec![crate::live::LiveKey { pid: 100, name_id: 7, kind: kind::API_SCOPE }];
    app.leftover = vec![1, 2, 3];
    app.core_util_key = Some((1, 2));
    app.capture_start_ns = 4_000;
    app.user_set_view = true;
    app.t0 = 5_000.0;
    app.t1 = 6_000.0;
    app.notice = Some("an earlier failure".into());
    app
}

/// Which of those are gone.
fn what_was_reset(app: &PerfrLiveApp) -> Vec<&'static str> {
    let old_events = app.index.lanes().any(|(k, _)| k.pid == 100);
    let checks: [(&'static str, bool); 27] = [
        ("index", !old_events),
        ("live_all", app.live_all.row(crate::live::LiveKey { pid: 100, name_id: 7, kind: kind::API_SCOPE }).is_none()),
        ("intern", app.intern.get(7) != Some("work")),
        ("thread_names", !app.thread_names.contains_key(&(100, 101))),
        ("trace_processes", !app.trace_processes.iter().any(|p| p.pid == 100)),
        ("tracks", app.tracks.catalogue_threads().is_empty()),
        ("selected", app.selected.is_none()),
        ("hover", app.hover.is_none()),
        ("selected_thread", app.selected_thread.is_none()),
        ("scope_report", app.scope_report.is_none()),
        ("scope_menu", app.scope_menu.is_none()),
        ("sample_sels", app.sample_sels.is_empty()),
        ("measure", app.measure.is_none()),
        ("sampling", app.sampling.as_ref().map(|r| r.samples) != Some(999)),
        ("tree", app.tree.as_ref().map(|t| t.samples) != Some(100)),
        ("sampling_ranges", app.sampling_ranges.is_empty()),
        ("rect_result", app.rect_result.is_none()),
        ("flame_zoom", app.flame_zoom.is_empty()),
        ("report_selection", app.report_selection.is_empty()),
        ("live_focus", app.live_focus.is_none()),
        ("live_order", app.live_order.is_empty()),
        ("held_stream", !app.held_stream.has_bytes() && !app.held_stream.is_local()),
        ("leftover", app.leftover.is_empty()),
        ("core_util_key", app.core_util_key.is_none()),
        ("capture_start_ns", app.capture_start_ns != 4_000),
        ("view", (app.t0, app.t1) != (5_000.0, 6_000.0)),
        ("notice", app.notice.is_none()),
    ];
    checks.into_iter().filter(|(_, reset)| *reset).map(|(name, _)| name).collect()
}

/// What each of the five ways a capture is replaced resets today. They are
/// five hand-written lists, and they differ; this pins them as they are, so
/// that folding them into one is a visible change to this table and not an
/// accident. Not every difference is meant: see the notes on each.
#[test]
fn what_each_way_of_replacing_the_capture_resets() {
    // Clear: the selection and the reports go, as nowhere else. The marquee
    // result, the flame zoom and the report's row selection stay.
    let mut app = app_with_everything_set();
    app.clear_everything();
    assert_eq!(
        what_was_reset(&app),
        [
            "index", "live_all", "intern", "thread_names", "trace_processes", "selected", "selected_thread",
            "scope_report", "scope_menu", "sample_sels", "measure", "sampling", "tree", "sampling_ranges",
            "held_stream", "view", "notice",
        ]
    );

    // A stream file: its own report replaces the old one (the file is
    // local), its bytes are the held stream, a half-read frame is dropped.
    // The selected thread, a scoped report and the open scope menu stay.
    let mut app = app_with_everything_set();
    app.accept_stream(stream(&[
        hello(),
        LiveFrame::InternedString { id: 9, text: "other".into() },
        LiveFrame::ProcessName { pid: 300, name: "another".into() },
        LiveFrame::EventBatch { events: vec![scope(300, 301, 9_000, 1_000, 9)] },
        LiveFrame::CaptureFinished,
    ]));
    assert_eq!(
        what_was_reset(&app),
        [
            "index", "live_all", "intern", "thread_names", "trace_processes", "selected", "hover", "sample_sels",
            "measure", "sampling", "tree", "sampling_ranges", "leftover", "view", "notice",
        ]
    );

    // A Chrome or Perfetto trace: the only one that rebuilds the track
    // strip. The reports of the previous capture stay in the panel.
    let mut app = app_with_everything_set();
    app.begin_trace_load(TraceLoad::from_bytes("t.json".into(), b"[]".to_vec()));
    assert_eq!(
        what_was_reset(&app),
        [
            "index", "live_all", "intern", "thread_names", "trace_processes", "tracks", "selected", "hover",
            "sample_sels", "measure", "held_stream", "view", "notice",
        ]
    );

    // A capture starting on the service: names stay interned (the service
    // sends each once per socket), and so do the selected thread and the
    // reports of the capture before.
    let mut app = app_with_everything_set();
    app.apply_frame(LiveFrame::CaptureStarted { pid: 200, start_ns: 9_000 });
    assert_eq!(
        what_was_reset(&app),
        [
            "index", "live_all", "thread_names", "trace_processes", "selected", "hover", "sample_sels", "measure",
            "held_stream", "capture_start_ns",
        ]
    );

    // A Hello, the start of a full resend: only what the resend brings again.
    let mut app = app_with_everything_set();
    app.apply_frame(hello());
    assert_eq!(what_was_reset(&app), ["index", "live_all", "thread_names", "trace_processes", "hover"]);
}

/// The three-thread capture, zoomed into its middle, with the report panel open.
fn page_with_report_panel() -> Page {
    let mut page = three_thread_page();
    page.app.open_right_tab(ReportTab::Inspector);
    page.app.apply_zoom_window(3_000_000.0, 5_000_000.0);
    page.frames(4);
    assert!(page.rect_of("report_splitter").is_some());
    page
}

#[test]
fn a_splitter_grab_on_the_timeline_side_resizes_the_panel_and_pans_nothing() {
    let mut page = page_with_report_panel();
    let view = (page.app.t0, page.app.t1);
    let width = page.app.report_w_last;
    let handle = page.rect_of("report_splitter").expect("the splitter");
    let lanes = page.rect_of("row:thread:100:101").expect("a lane row");
    // The handle reaches a few points into the timeline: grab it there, at
    // the height of the lanes, and pull left.
    let grab = Pos2::new(handle.left() + 1.5, lanes.center().y);
    let to = grab - Vec2::new(120.0, 0.0);
    page.drag(grab, to);
    page.release(to);
    page.frames(2);
    assert!(page.app.report_w_last > width + 100.0, "the panel follows the pointer: {width} -> {}", page.app.report_w_last);
    assert_eq!((page.app.t0, page.app.t1), view, "the same drag panned the timeline under it");
}

#[test]
fn a_splitter_grab_over_the_time_slider_does_not_jump_the_view() {
    let mut page = page_with_report_panel();
    let view = (page.app.t0, page.app.t1);
    let handle = page.rect_of("report_splitter").expect("the splitter");
    // The time slider is the strip along the bottom of the timeline.
    let grab = Pos2::new(handle.left() + 1.5, page.size.y - TIME_SLIDER_H * 0.5);
    let to = grab - Vec2::new(60.0, 0.0);
    page.drag(grab, to);
    page.release(to);
    page.frames(2);
    assert_eq!((page.app.t0, page.app.t1), view, "the press on the splitter also moved the time slider");
}

#[test]
fn a_drag_on_the_lanes_still_pans() {
    let mut page = page_with_report_panel();
    let view = (page.app.t0, page.app.t1);
    let lanes = page.rect_of("row:thread:100:101").expect("a lane row");
    let grab = Pos2::new(lanes.right() + 300.0, lanes.center().y);
    let to = grab - Vec2::new(120.0, 0.0);
    page.drag(grab, to);
    page.release(to);
    page.frames(2);
    assert!(page.app.t0 > view.0, "dragging the lanes left moves the view later");
}

fn wheel(dy: f32) -> Event {
    Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: Vec2::new(0.0, dy), modifiers: Modifiers::NONE }
}

/// Many threads, so the lanes scroll, and the Benchmark window floating
/// over the middle of them.
fn page_with_a_window_over_the_lanes() -> (Page, Rect) {
    let mut page = many_thread_page(200);
    page.app.bench_open = true;
    page.frames(3);
    assert!(page.app.vscroll_max > 0.0, "the lanes are taller than the view");
    let rate = page.rect_of("bench:rate").expect("the Benchmark window is drawn");
    (page, rate)
}

#[test]
fn the_wheel_over_a_floating_window_leaves_the_lanes_under_it_alone() {
    let (mut page, rate) = page_with_a_window_over_the_lanes();
    let over_window = rate.center() - Vec2::new(0.0, 30.0);
    page.frame(vec![Event::PointerMoved(over_window)]);
    page.frame(vec![wheel(-120.0)]);
    page.frames(3);
    assert_eq!(page.app.lane_scroll, 0.0, "the lanes scrolled under the window the wheel was over");

    // Over the lanes themselves it scrolls them, as before.
    let lanes = Pos2::new(page.size.x - 80.0, page.size.y - 120.0);
    page.frame(vec![Event::PointerMoved(lanes)]);
    page.frame(vec![wheel(-120.0)]);
    page.frames(3);
    assert!(page.app.lane_scroll > 0.0);
}

#[test]
fn a_press_on_a_window_over_the_splitter_is_not_a_splitter_grab() {
    let (mut page, rate) = page_with_a_window_over_the_lanes();
    // The report panel's edge under the window.
    page.app.open_right_tab(ReportTab::Inspector);
    page.app.report_w_override = Some(page.size.x - rate.center().x);
    page.frames(3);
    let handle = page.rect_of("report_splitter").expect("the splitter");
    let rate = page.rect_of("bench:rate").expect("the window is still there");
    assert!(handle.left() > rate.left() - 40.0 && handle.right() < rate.right() + 40.0, "{handle:?} is under {rate:?}");
    let width = page.app.report_w_last;
    let on_window = Pos2::new(handle.center().x, rate.center().y - 30.0);
    page.drag(on_window, on_window - Vec2::new(0.0, 4.0));
    assert!(page.app.report_splitter_grab.is_none(), "a press on the window grabbed the splitter beneath it");
    page.release(on_window);
    page.frames(2);
    assert_eq!(page.app.report_w_last, width);
}

/// The report panel as dragging its splitter to the right edge leaves it:
/// hidden behind its edge tab, with a width of next to nothing remembered.
fn collapse_report_panel(page: &mut Page) {
    page.app.open_right_tab(ReportTab::Flat);
    page.frames(2);
    page.app.report_w_user = Some(12.0);
    page.frames(3);
    assert!(page.app.report_collapsed, "a panel that narrow collapses once the button is up");
}

fn report_panel_is_showing(page: &Page) -> bool {
    page.app.report_open && !page.app.report_collapsed && page.app.report_w_last > REPORT_COLLAPSE_W
}

#[test]
fn the_functions_pill_brings_a_collapsed_report_panel_back() {
    let mut page = three_thread_page();
    page.app.selected_pid = Some(100);
    collapse_report_panel(&mut page);
    page.app.capture_open = true;
    page.frames(3);
    let pill = page.rect_of("Functions").expect("the settings have a Functions pill");
    page.click(pill.center());
    page.frames(4);
    assert_eq!(page.app.report_tab, ReportTab::Functions);
    assert!(report_panel_is_showing(&page), "the panel showed for a frame and collapsed again");
}

#[test]
fn a_scope_report_from_the_menu_brings_a_collapsed_report_panel_back() {
    let mut page = three_thread_page();
    collapse_report_panel(&mut page);
    let pick = ScopePick::from_event(scope(100, 101, 2_000_000, 5_000_000, 7));
    page.app.scope_menu = Some((pick, Pos2::new(500.0, 300.0)));
    page.frames(3);
    let item = page.rect_of("menu:report").expect("the scope menu offers its report");
    page.click(item.center());
    page.frames(4);
    assert!(page.app.scope_report.is_some());
    assert!(report_panel_is_showing(&page), "the report loaded into a panel that stayed hidden");
}

#[test]
fn the_settings_item_opens_the_interface_knobs_on_a_static_page() {
    let mut page = Page::with(|app| *app = loaded_static_app());
    page.frames(3);
    let more = page.rect_of("More").expect("More");
    page.click(more.center());
    page.frames(2);
    let item = page.text_rect("Settings").expect("the menu offers Settings");
    page.click(item.center());
    page.frames(3);
    assert!(page.shows("INTERFACE"), "the menu item did nothing: {:?}", page.texts);
    assert!(page.shows("track scale"));
    // What needs a service is not offered: the capture settings, the hook
    // list's pill, the track order file that is kept by the service.
    assert!(page.rect_of("Functions").is_none(), "{:?}", page.labels());
    assert!(!page.shows("Record"));
    // And the knobs work.
    let before = page.app.ui_tweaks;
    page.app.ui_tweaks.report_font = 15.0;
    page.frames(2);
    assert_ne!(page.app.ui_tweaks, before);
}

#[test]
fn the_settings_window_of_a_page_with_a_service_keeps_its_capture_settings() {
    let mut page = three_thread_page();
    page.app.capture_open = true;
    page.frames(3);
    assert!(page.shows("INTERFACE"));
    assert!(page.rect_of("Functions").is_some(), "the capture settings are above the knobs: {:?}", page.labels());
}

#[test]
fn a_capture_that_stops_asks_for_its_report_once() {
    let mut page = Page::with(|app| app.apply_status(service_status(false)));
    page.frames(2);
    page.app.start_record();
    page.app.apply_status(service_status(true));
    page.frames(3);
    page.app.sample_sels.push(TimeMeasure { start_ns: 1, stop_ns: 2, label_y: 0.0, sample_tid: None });
    page.frames(2);
    let asked = page.app.report_requests;
    page.app.stop_record();
    page.app.apply_status(service_status(false));
    page.frames(3);
    assert_eq!(page.app.report_requests - asked, 1, "the service scans the capture once per request");
    assert!(page.app.report_open);
    assert_eq!(page.app.report_tab, ReportTab::TopDown);
    assert!(page.app.sample_sels.is_empty() && page.app.sampling_ranges.is_empty(), "the report is over the whole capture");
}

/// A page holding a stream, with a time range selected and the Share
/// dialog at its confirm step over the held bytes.
fn share_confirm_app(stream_bytes: Vec<u8>) -> PerfrLiveApp {
    let mut app = static_app();
    app.accept_stream(stream_bytes);
    app.sample_sels = vec![TimeMeasure { start_ns: 5_000_000_000, stop_ns: 5_500_000_000, label_y: 0.0, sample_tid: None }];
    app.share_open = true;
    app.share_ui = ShareUi::Confirm;
    app.share_draft = Some(ShareDraft::loaded());
    app
}

fn whole_stream() -> Vec<u8> {
    capture_stream(
        status_frame(false, false),
        (0..2_000).map(|i| scope(100, 101, 5_000_000_000 + i * 1_000_000, 500_000, 7)).collect(),
    )
}

#[test]
fn the_share_dialog_cuts_the_selected_range_once() {
    let mut app = share_confirm_app(whole_stream());
    app.refresh_share_range();
    let draft = app.share_draft.as_ref().unwrap();
    let range = draft.range.as_ref().expect("the selection is cut out of the stream");
    assert_eq!(range.span, (5_000_000_000, 5_500_000_000));
    assert!(!range.bytes.is_empty() && range.bytes.len() < app.held_stream.bytes().unwrap().len());
    assert!(!draft.range_unslicable);
}

#[test]
fn a_range_that_could_not_be_cut_is_not_cut_again_every_frame() {
    let mut cut_short = whole_stream();
    cut_short.truncate(cut_short.len() - 3);
    let mut app = share_confirm_app(cut_short);
    app.refresh_share_range();
    assert!(app.share_draft.as_ref().unwrap().range_unslicable);
    assert!(app.share_draft.as_ref().unwrap().range.is_none());
    // Whether the cut is tried again shows when it would now succeed.
    app.held_stream.loaded(whole_stream());
    app.refresh_share_range();
    assert!(
        app.share_draft.as_ref().unwrap().range.is_none(),
        "the same span was cut again on the next frame"
    );
    // Another span is another attempt.
    app.sample_sels[0].stop_ns = 5_600_000_000;
    app.refresh_share_range();
    assert!(app.share_draft.as_ref().unwrap().range.is_some());
}

/// Not a check: what the Share dialog costs per frame while it shows a
/// range it could not cut. `cargo test --release -- --ignored --nocapture share_range_times`.
#[test]
#[ignore]
fn share_range_times() {
    let mut big = capture_stream(
        status_frame(false, false),
        (0..2_000_000u64).map(|i| scope(100, 101, 5_000_000_000 + i * 1_000, 500, 7)).collect(),
    );
    big.truncate(big.len() - 3);
    let size = big.len();
    let mut app = share_confirm_app(big);
    let start = std::time::Instant::now();
    for _ in 0..20 {
        app.refresh_share_range();
    }
    println!(
        "uncuttable range over a {:.0} MB stream: {:8.3} ms/frame",
        size as f64 / 1e6,
        start.elapsed().as_secs_f64() * 1000.0 / 20.0
    );
}

#[test]
fn a_saved_knob_that_is_not_a_number_falls_back_to_its_default() {
    let defaults = UiTweaks::default();
    // `NaN` parses as a float and passes through `clamp` unchanged: it
    // reached the font size of every report row.
    for text in [r#"{"report_font":NaN}"#, r#"{"report_font":nan,"report_row_gap":4}"#, r#"{"report_font":-NaN}"#] {
        let loaded = UiTweaks::from_json(text);
        assert_eq!(loaded.report_font, defaults.report_font, "{text}");
    }
    assert_eq!(UiTweaks::from_json(r#"{"report_font":nan,"report_row_gap":4}"#).report_row_gap, 4.0);
    // Infinities are numbers out of range, and clamp like any other.
    assert_eq!(UiTweaks::from_json(r#"{"report_font":inf}"#).report_font, 18.0);
    assert_eq!(UiTweaks::from_json(r#"{"report_indent":-inf}"#).report_indent, 0.0);
    for knob in ["report_row_gap", "report_col_gap", "report_font", "report_bar_w", "report_indent", "track_sort_every_s"] {
        let loaded = UiTweaks::from_json(&format!("{{\"{knob}\":NaN}}"));
        assert_eq!(loaded, defaults, "{knob}");
    }
}

/// The times of the ruler's long (major) ticks for a window, to the nearest unit.
fn major_ticks(t0: f64, t1: f64) -> Vec<i64> {
    let ctx = egui::Context::default();
    let ruler = Rect::from_min_size(Pos2::new(100.0, 40.0), Vec2::new(1000.0, 26.0));
    let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 800.0))), ..Default::default() };
    let out = ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| paint_timebar(ui, ruler, t0, t1, 0.0));
    });
    fn lines(shape: &Shape, out: &mut Vec<[Pos2; 2]>) {
        match shape {
            Shape::Vec(shapes) => shapes.iter().for_each(|s| lines(s, out)),
            Shape::LineSegment { points, .. } => out.push(*points),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for clipped in &out.shapes {
        lines(&clipped.shape, &mut found);
    }
    let span = t1 - t0;
    found
        .iter()
        .filter(|p| p[0].x == p[1].x && (p[1].y - p[0].y - 7.0).abs() < 0.01)
        .map(|p| (t0 + (p[0].x - ruler.left()) as f64 / ruler.width() as f64 * span).round() as i64)
        .collect()
}

#[test]
fn the_rulers_long_ticks_stay_on_round_times_left_of_zero() {
    // 1,000 units over 1,000 points: a tick every 10, a long one every 50.
    assert_eq!(major_ticks(0.0, 1000.0).iter().take(3).collect::<Vec<_>>(), [&0, &50, &100]);
    assert_eq!(major_ticks(20.0, 1020.0).iter().take(3).collect::<Vec<_>>(), [&50, &100, &150]);
    // A window that starts before zero, as a zoom out of an empty view gives.
    let ticks = major_ticks(-320.0, 680.0);
    assert!(ticks.contains(&0), "{ticks:?}");
    assert!(ticks.iter().all(|t| t % 50 == 0), "long ticks off the round times: {ticks:?}");
}

#[test]
fn the_shaders_clock_keeps_its_precision_after_a_day() {
    // The selection pulse is sin(2 pi t / period); what matters is that t
    // moves smoothly and stays in phase.
    let period = 1.2f32 as f64;
    assert!(
        perfr_live_render::INSTANCE_WGSL.contains("const PULSE_PERIOD: f32 = 1.2;"),
        "the wrap in shader_time is a whole number of the shader's pulse periods"
    );
    for now in [0.25f64, 59.9, 1_199.9, 1_200.3, 86_400.0 + 0.123, 30.0 * 86_400.0 + 0.456] {
        let phase = |t: f64| (t / period).fract();
        for step in 0..5 {
            let t = now + step as f64 / 120.0;
            let shader = shader_time(t) as f64;
            let off = (phase(shader) - phase(t)).abs();
            let off = off.min(1.0 - off);
            assert!(off < 1e-3, "at {t} s the pulse is {off} of a period out of phase");
        }
    }
}

#[test]
fn hovering_a_flame_bar_names_it_with_its_samples() {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        app.apply_service_reports(None, Some(small_tree()));
        app.open_right_tab(ReportTab::Flame);
    });
    page.frames(3);
    let bar = page
        .labels()
        .iter()
        .find(|l| l.starts_with("flame:") && l.contains("leaf_a"))
        .and_then(|l| page.rect_of(l))
        .expect("the flame graph draws a bar for leaf_a");
    page.frame(vec![Event::PointerMoved(bar.center())]);
    page.frames(2);
    assert!(
        page.shows("leaf_a\n60 samples, 60.0% of the selection"),
        "no tooltip over the hovered bar: {:?}",
        page.texts
    );
}

/// A finished capture dense enough that the whole of it is drawn as pixel
/// columns: `threads` threads of back-to-back scopes far narrower than a pixel.
fn zoomed_out_page(threads: u32, scopes: u64) -> Page {
    let mut page = Page::with(|app| {
        app.apply_status(service_status(false));
        draw_as_with_a_gpu(app);
        let mut frames = vec![hello(), LiveFrame::CaptureStarted { pid: 100, start_ns: 1_000_000 }];
        for t in 0..threads {
            frames.push(LiveFrame::EventBatch {
                events: (0..scopes).map(|i| scope(100, 200 + t, 2_000_000 + i * 1_000, 900, 7 + (i % 5) as u32)).collect(),
            });
        }
        frames.push(LiveFrame::CaptureFinished);
        feed(app, &frames);
    });
    page.frames(4);
    // Fitted to its content: the capture fills the view, as when one is opened.
    page.key(Key::Home);
    page.frames(4);
    assert!(page.app.t1 - page.app.t0 < 100e6, "the view is the capture's few tens of ms: {} .. {}", page.app.t0, page.app.t1);
    assert_eq!(page.app.last_lod, perfr_live_render::TimelineLod::PixelColumns, "the capture is drawn as columns");
    page
}

#[test]
fn hovering_a_zoomed_out_view_moves_the_marks_and_rasterizes_nothing() {
    let mut page = zoomed_out_page(6, 40_000);
    let drawn = page.app.column_rasters;
    assert!(drawn > 0, "the view was rasterized to begin with");
    page.frames(5);
    assert_eq!(page.app.column_rasters, drawn, "a still view is not drawn again");
    // The pointer crosses the lanes: the scope under it changes on nearly
    // every frame, and each change used to rasterize the whole view.
    let mut hovered = std::collections::HashSet::new();
    for step in 0..40 {
        let at = Pos2::new(420.0 + step as f32 * 17.0, 150.0 + (step % 5) as f32 * 30.0);
        page.frame(vec![Event::PointerMoved(at)]);
        if let Some(pick) = page.app.hover {
            hovered.insert((pick.tid, pick.start_ns));
        }
    }
    assert!(hovered.len() > 4, "the pointer was over {} different scopes", hovered.len());
    assert_eq!(page.app.column_rasters, drawn, "hovering rasterized the view again");
    // What does change the picture still draws it: moving the view. (So
    // does a click: selecting a scope focuses its thread, and that dims the
    // columns of the others.)
    page.frame(vec![Event::PointerMoved(Pos2::new(700.0, 125.0)), key_event(Key::W, true)]);
    page.frames(3);
    page.frame(vec![key_event(Key::W, false)]);
    assert!(page.app.column_rasters > drawn, "a zoom was not drawn");
}

#[test]
fn the_level_of_detail_is_decided_once_for_a_view_that_holds_still() {
    let mut page = zoomed_out_page(4, 20_000);
    let memo = page.app.lod_memo.expect("decided");
    page.frames(5);
    assert_eq!(page.app.lod_memo, Some(memo), "the same window and events: the same answer, kept");
    // New events are a new question, whatever the window.
    feed(&mut page.app, &[LiveFrame::EventBatch { events: vec![scope(100, 200, 2_500_000, 4_000_000, 9)] }]);
    page.frames(2);
    let after = page.app.lod_memo.expect("decided again");
    assert_ne!(after.0, memo.0, "the index changed and the memo did not notice");
    assert_eq!(after.1, perfr_live_render::TimelineLod::Instanced, "a scope wide enough to be a box is in view now");
}

/// Not a check: what a frame costs over a large capture, still and under
/// each kind of input, at three zoom levels. The capture is a `.perfr.stream`
/// named by `PERFR_PROBE_STREAM`; `PERFR_PROBE_ONLY=<scenario>` with
/// `PERFR_PROBE_SECONDS=<n>` loops one scenario for a sampler to look at.
/// `cargo test --release -p perfr-live-viewer --lib -- --ignored --nocapture render_frame_times`.
#[test]
#[ignore]
fn render_frame_times() {
    let Some(path) = std::env::var_os("PERFR_PROBE_STREAM") else {
        println!("PERFR_PROBE_STREAM is not set");
        return;
    };
    let only = std::env::var("PERFR_PROBE_ONLY").ok();
    let seconds: f64 = std::env::var("PERFR_PROBE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let bytes = std::fs::read(&path).expect("the stream file");
    let load = std::time::Instant::now();
    let mut page = Page::with(|app| {
        app.static_capture = Some("capture.perfr.stream".into());
        draw_as_with_a_gpu(app);
    })
    .sized(1920.0, 1080.0);
    page.app.accept_stream(bytes);
    page.frames(10);
    println!(
        "loaded {} events on {} lanes in {:.0} ms; pid {}",
        page.app.index.event_count(),
        page.app.index.lane_count(),
        load.elapsed().as_secs_f64() * 1000.0,
        std::process::id()
    );
    let lanes = Pos2::new(1100.0, 420.0);
    let key = |key: Key, pressed: bool| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE };

    // One scenario: `step(page, i)` is the input of frame `i`.
    let run = |page: &mut Page, name: &str, frames: usize, step: &mut dyn FnMut(&mut Page, usize)| {
        if only.as_deref().is_some_and(|o| o != name) {
            // Still drive the input, so the view is where the next scenario expects it.
            for i in 0..frames {
                step(page, i);
            }
            return;
        }
        let start = std::time::Instant::now();
        let mut done = 0usize;
        loop {
            for i in 0..frames {
                step(page, i);
            }
            done += frames;
            if start.elapsed().as_secs_f64() >= seconds {
                break;
            }
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0 / done as f64;
        println!(
            "{name:<28} {ms:8.3} ms/frame   view {:>10.3} ms wide, {} frames",
            (page.app.t1 - page.app.t0) / 1e6,
            done
        );
    };
    let still = |page: &mut Page, _: usize| {
        page.frame(Vec::new());
    };
    let hover = |page: &mut Page, i: usize| {
        let x = 700.0 + ((i * 7) % 900) as f32;
        page.frame(vec![Event::PointerMoved(Pos2::new(x, 300.0 + (i % 200) as f32))]);
    };
    // A drag that goes back and forth, so a long run stays inside the capture.
    let pan = |page: &mut Page, i: usize| {
        let phase = i % 120;
        if phase == 0 {
            page.frame(vec![Event::PointerMoved(lanes), pointer_button(lanes, true)]);
        }
        let dx = if phase < 60 { phase as f32 } else { (120 - phase) as f32 } * 6.0;
        let at = Pos2::new(lanes.x - dx, lanes.y);
        if phase == 119 {
            page.frame(vec![Event::PointerMoved(at), pointer_button(at, false)]);
        } else {
            page.frame(vec![Event::PointerMoved(at)]);
        }
    };
    let scroll = |page: &mut Page, i: usize| {
        let dy = if (i / 40) % 2 == 0 { -30.0 } else { 30.0 };
        page.frame(vec![Event::PointerMoved(lanes), wheel(dy)]);
    };
    let zoom = |page: &mut Page, frames: usize, key_code: Key| {
        page.frame(vec![Event::PointerMoved(lanes), key(key_code, true)]);
        page.frames(frames);
        page.frame(vec![key(key_code, false)]);
        page.frames(2);
    };

    for level in ["whole", "mid", "deep"] {
        run(&mut page, &format!("{level}: still"), 120, &mut |p, i| still(p, i));
        run(&mut page, &format!("{level}: hover"), 120, &mut |p, i| hover(p, i));
        run(&mut page, &format!("{level}: pan drag"), 120, &mut |p, i| pan(p, i));
        run(&mut page, &format!("{level}: scroll tracks"), 80, &mut |p, i| scroll(p, i));
        if level != "deep" {
            let before = page.app.t1 - page.app.t0;
            let start = std::time::Instant::now();
            let frames = if level == "whole" { 45 } else { 60 };
            zoom(&mut page, frames, Key::W);
            println!(
                "{:<28} {:8.3} ms/frame   view {:.3} -> {:.3} ms",
                format!("{level}: zoom in (W held)"),
                start.elapsed().as_secs_f64() * 1000.0 / (frames + 4) as f64,
                before / 1e6,
                (page.app.t1 - page.app.t0) / 1e6
            );
        }
    }
    let start = std::time::Instant::now();
    zoom(&mut page, 120, Key::S);
    println!(
        "{:<28} {:8.3} ms/frame   view back to {:.3} ms",
        "zoom out (S held)",
        start.elapsed().as_secs_f64() * 1000.0 / 124.0,
        (page.app.t1 - page.app.t0) / 1e6
    );
}
