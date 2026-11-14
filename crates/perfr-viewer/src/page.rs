// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The viewer's page: open a `.perfr.stream` and draw it (`start_viewer`).
//!
//! It does not talk to the capture service, import Chrome or Perfetto
//! traces, or draw the client's chrome. `perfr-client` is the page that
//! does; it builds on this crate with the `page` feature off, so its pack
//! carries none of this module.

use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use perfr_live_render::{choose_lod, ThreadFocus, INSTANCE_MIN_PX};
use crate::dev::DevFrame;
use crate::open::{open_capture, OpenedCapture};
use crate::theme;
use crate::timeline::{
    paint_callback, quant_px, TimelineGpu, TimelineGpuSlot, TimelinePayload, ViewUniforms,
};
use crate::tracks::{RowId, TrackStrip};

/// Width of the column of track names left of the timeline, in points.
const LABEL_W: f32 = 160.0;
/// Empty time shown on each side of a fitted capture, as a share of its span.
const FIT_PAD: f64 = 0.05;
/// The narrowest window the view zooms in to.
const MIN_SPAN_NS: f64 = 100.0;
/// One notch of a mouse wheel scales the window by this.
const ZOOM_PER_NOTCH: f64 = 0.85;
/// The most one frame's wheel input may zoom, in notches. A flick of a free
/// wheel delivers dozens in a frame.
const MAX_NOTCHES_PER_FRAME: f32 = 4.0;
/// What a browser reports for one notch in each wheel unit: Chrome sends 100
/// pixels, Firefox three lines.
const NOTCH_POINTS: f32 = 100.0;
const NOTCH_LINES: f32 = 3.0;

type Incoming = Arc<Mutex<Option<Result<Vec<u8>, String>>>>;

/// A capture on a canvas. Wasm boots it from [`start_viewer`]; native tests
/// drive [`Self::load_bytes`].
pub struct CaptureViewer {
    opened: OpenedCapture,
    tracks: TrackStrip,
    /// The visible window in capture nanoseconds, always inside
    /// [`view_limits`] of the open capture.
    t0: f64,
    t1: f64,
    scroll: f32,
    has_gpu: bool,
    backend: &'static str,
    status: String,
    incoming: Incoming,
    /// Counts captures opened, so a second capture with as many events as
    /// the first is still a new picture.
    capture_gen: u64,
    /// What the buffers on the GPU were built from, and where they were
    /// drawn. While the key holds, a frame reuses both.
    drawn: Option<(DrawKey, ViewUniforms)>,
}

/// Everything the timeline's GPU buffers depend on. Equal keys mean the
/// buffers of the previous frame are this frame's picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DrawKey {
    capture_gen: u64,
    layout_gen: u64,
    t0: u64,
    t1: u64,
    scroll_q: i32,
    body_q: [i32; 4],
    ppp_bits: u32,
    screen_q: [i32; 2],
}

/// Latin subset of Hack (U+0020–U+007E and U+00A0–U+00FF). The four egui
/// default fonts are about 1.4 MB of TTF; this page only draws track labels
/// and a status line. A name outside Latin-1 shows as `?`; the `full-fonts`
/// feature links egui's fonts instead. License: `fonts/LICENSE-Hack.txt`.
#[cfg(not(feature = "full-fonts"))]
fn install_latin_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();
    fonts.font_data.insert(
        "HackLatin".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../fonts/HackLatin.ttf"
        ))),
    );
    fonts
        .families
        .insert(egui::FontFamily::Proportional, vec!["HackLatin".to_owned()]);
    fonts
        .families
        .insert(egui::FontFamily::Monospace, vec!["HackLatin".to_owned()]);
    ctx.set_fonts(fonts);
}

impl CaptureViewer {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(not(feature = "full-fonts"))]
        install_latin_font(&cc.egui_ctx);
        let mut has_gpu = false;
        let mut backend = "no GPU device";
        if let Some(rs) = &cc.wgpu_render_state {
            let mut renderer = rs.renderer.write();
            renderer.callback_resources.insert(TimelineGpuSlot::new(
                TimelineGpu::init(&rs.device, rs.target_format),
                TimelineGpu::init(&rs.device, rs.target_format),
            ));
            has_gpu = true;
            backend = match rs.adapter.get_info().backend {
                eframe::wgpu::Backend::BrowserWebGpu => "WebGPU",
                eframe::wgpu::Backend::Gl => "WebGL",
                other => other.to_str(),
            };
        }
        let incoming: Incoming = Arc::new(Mutex::new(None));
        // `status` is only rewritten when a wasm page has `?capture=`.
        #[cfg_attr(not(target_arch = "wasm32"), allow(unused_mut))]
        let mut view = Self {
            opened: OpenedCapture::default(),
            tracks: TrackStrip::default(),
            t0: 0.0,
            t1: 1.0,
            scroll: 0.0,
            has_gpu,
            backend,
            status: "Drop a .perfr.stream, or open ?capture=<url>".into(),
            incoming: Arc::clone(&incoming),
            capture_gen: 0,
            drawn: None,
        };
        #[cfg(target_arch = "wasm32")]
        if let Some(url) = capture_url_from_location() {
            view.status = format!("Loading {url}");
            spawn_fetch(url, incoming, cc.egui_ctx.clone());
        }
        view
    }

    /// Replace the open capture with `bytes`. Returns the parse fault, when
    /// the bytes were not a whole stream.
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Option<String> {
        self.opened = open_capture(bytes);
        self.tracks = TrackStrip::default();
        // The capture does not change once it is open, so the rail is laid
        // out here and not again on every frame.
        self.tracks.sync(&self.opened.index, None);
        self.tracks.tick(0.0, &self.opened.index, None);
        self.scroll = 0.0;
        self.capture_gen += 1;
        self.drawn = None;
        self.fit();
        let n = self.opened.index.event_count();
        self.status = match &self.opened.fault {
            Some(fault) => format!("{n} events, then {fault}"),
            None if n == 0 => "That file has no events.".into(),
            None => format!("{n} events"),
        };
        self.opened.fault.clone()
    }

    /// The widest window this capture is shown in: `(lo, hi)`.
    fn limits(&self) -> Option<(f64, f64)> {
        self.opened.index.time_bounds().map(view_limits)
    }

    fn fit(&mut self) {
        if let Some((lo, hi)) = self.limits() {
            (self.t0, self.t1) = clamp_window(lo, hi, lo, hi);
        }
    }

    fn take_incoming(&mut self) {
        let next = self.incoming.lock().ok().and_then(|mut slot| slot.take());
        if let Some(result) = next {
            match result {
                Ok(bytes) => {
                    self.load_bytes(&bytes);
                }
                Err(err) => self.status = err,
            }
        }
    }

    fn label(&self, row: RowId) -> String {
        match row {
            RowId::Scheduler => "scheduler".into(),
            RowId::Machine(_) => "machine".into(),
            RowId::Process(pid) => self
                .opened
                .processes
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| format!("pid {pid}")),
            RowId::Thread(t) => self
                .opened
                .threads
                .get(&(t.pid, t.tid))
                .cloned()
                .unwrap_or_else(|| format!("{}", t.tid)),
            RowId::Async(t) => {
                let name = self
                    .opened
                    .threads
                    .get(&(t.pid, t.tid))
                    .map(String::as_str)
                    .unwrap_or("");
                if name.is_empty() {
                    "async".into()
                } else {
                    format!("async {name}")
                }
            }
            RowId::Lane(_) => String::new(),
        }
    }

    /// Pan, zoom and scroll from this frame's input. The window stays inside
    /// the capture's limits whatever the gesture.
    fn navigate(&mut self, ui: &egui::Ui, resp: &egui::Response, body: Rect, max_scroll: f32) {
        let Some((lo, hi)) = self.limits() else {
            return;
        };
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let over_labels = pointer.is_some_and(|p| p.x < body.min.x && p.y >= body.min.y);
        let (scroll_pts, notches, pinch) =
            ui.input(|i| (i.raw_scroll_delta, wheel_notches(&i.events), i.zoom_delta()));

        if over_labels {
            self.scroll -= scroll_pts.y;
        }
        let span = self.t1 - self.t0;
        let per_point = span / f64::from(body.width().max(1.0));
        if resp.double_clicked() {
            self.fit();
        } else if resp.dragged() {
            let drag = resp.drag_delta();
            let shift = -f64::from(drag.x) * per_point;
            (self.t0, self.t1) = clamp_window(self.t0 + shift, self.t1 + shift, lo, hi);
            self.scroll -= drag.y;
        } else if resp.hovered() {
            // A sideways wheel or two-finger swipe pans.
            if scroll_pts.x != 0.0 {
                let shift = -f64::from(scroll_pts.x) * per_point;
                (self.t0, self.t1) = clamp_window(self.t0 + shift, self.t1 + shift, lo, hi);
            }
            let scale = zoom_scale(notches, pinch);
            if scale != 1.0 {
                let frac = pointer
                    .map(|p| f64::from(((p.x - body.min.x) / body.width().max(1.0)).clamp(0.0, 1.0)))
                    .unwrap_or(0.5);
                (self.t0, self.t1) = zoom_window(self.t0, self.t1, scale, frac, lo, hi);
            }
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
    }

    /// This frame's payload and where to draw it. An unchanged view reuses
    /// the buffers already on the GPU: a pointer move over the canvas repaints
    /// the page, and rebuilding the timeline for it re-walked every visible
    /// lane and re-sent the whole texture or instance buffer.
    fn payload(&mut self, body: Rect, ppp: f32, screen: Rect) -> (TimelinePayload, ViewUniforms) {
        let screen_px = [screen.width() * ppp, screen.height() * ppp];
        // `navigate` and `fit` keep the window at or after zero and at least
        // `MIN_SPAN_NS` wide, so these casts do not move it.
        let t0 = self.t0.max(0.0) as u64;
        let t1 = (self.t1 as u64).max(t0 + 1);
        let key = DrawKey {
            capture_gen: self.capture_gen,
            layout_gen: self.tracks.layout_gen(),
            t0,
            t1,
            scroll_q: quant_px(self.scroll),
            body_q: [
                quant_px(body.min.x),
                quant_px(body.min.y),
                quant_px(body.width()),
                quant_px(body.height()),
            ],
            ppp_bits: ppp.to_bits(),
            screen_q: [quant_px(screen_px[0]), quant_px(screen_px[1])],
        };
        if let Some((drawn, view)) = self.drawn {
            if drawn == key {
                return (TimelinePayload::Keep, view);
            }
        }

        let layout = visible_layout(&self.tracks, self.scroll, body.height());
        let width_px = (body.width() * ppp).round().max(1.0) as usize;
        let lod = choose_lod(&self.opened.index, t0, t1, width_px, INSTANCE_MIN_PX);
        let dev = DevFrame::begin(false);
        let (payload, _) = TimelinePayload::from_index(
            &self.opened.index,
            t0,
            t1,
            body.width(),
            lod,
            ppp,
            &layout,
            &[],
            None,
            None,
            ThreadFocus::default(),
            Some(&self.opened.intern),
            self.tracks.scale,
            None,
            &dev,
        );
        let view = ViewUniforms::from_rect(blit_rect(body, &payload), ppp, screen_px);
        self.drawn = Some((key, view));
        (payload, view)
    }
}

/// Where a payload is drawn. The column texture holds only the rows the
/// rasterizer placed, from `top` down for `rows` points, so it is drawn
/// there: stretched over the whole body it no longer lines up with the track
/// names. Instances carry their own positions and take the body.
fn blit_rect(body: Rect, payload: &TimelinePayload) -> Rect {
    match payload {
        TimelinePayload::Pixel {
            place: Some((top, rows)),
            ..
        } => Rect::from_min_size(
            Pos2::new(body.left(), body.top() + *top),
            Vec2::new(body.width(), rows.max(1.0)),
        ),
        _ => body,
    }
}

/// The widest window a capture spanning `bounds` is shown in: the capture
/// with [`FIT_PAD`] of empty time each side, and nothing before time zero.
/// The renderer's clock is unsigned; a window that started before zero was
/// drawn from zero across the full width, which stretched the picture.
fn view_limits(bounds: (u64, u64)) -> (f64, f64) {
    let (a, b) = bounds;
    let span = b.saturating_sub(a).max(1) as f64;
    let pad = span * FIT_PAD;
    let lo = (a as f64 - pad).max(0.0);
    (lo, (b as f64 + pad).max(lo + MIN_SPAN_NS))
}

/// `[t0, t1]` moved, and narrowed if it has to be, to sit inside `[lo, hi]`.
/// The span is kept when the window is only moved.
fn clamp_window(t0: f64, t1: f64, lo: f64, hi: f64) -> (f64, f64) {
    let full = (hi - lo).max(MIN_SPAN_NS);
    let span = (t1 - t0).clamp(MIN_SPAN_NS, full);
    let t0 = t0.clamp(lo, lo + full - span);
    (t0, t0 + span)
}

/// The window scaled by `scale` (under 1 zooms in) around the time at
/// `frac` of its width, so the time under the pointer stays under it.
fn zoom_window(t0: f64, t1: f64, scale: f64, frac: f64, lo: f64, hi: f64) -> (f64, f64) {
    let span = t1 - t0;
    let full = (hi - lo).max(MIN_SPAN_NS);
    let new_span = (span * scale).clamp(MIN_SPAN_NS, full);
    let anchor = t0 + span * frac;
    let new_t0 = anchor - new_span * frac;
    clamp_window(new_t0, new_t0 + new_span, lo, hi)
}

/// How much this frame's wheel and pinch input scale the window. In
/// proportion to how far the wheel moved: a trackpad sends a small delta on
/// nearly every frame, and a fixed step for each of them zoomed from the
/// whole capture to nanoseconds within a second.
fn zoom_scale(notches: f32, pinch: f32) -> f64 {
    let notches = notches.clamp(-MAX_NOTCHES_PER_FRAME, MAX_NOTCHES_PER_FRAME);
    let wheel = ZOOM_PER_NOTCH.powf(f64::from(notches));
    let pinch = if pinch.is_finite() && pinch > 0.0 {
        f64::from(pinch)
    } else {
        1.0
    };
    wheel / pinch
}

/// Vertical wheel travel this frame in mouse-wheel notches, positive away
/// from the user. Read from the events and not from `raw_scroll_delta`,
/// which is in points and makes a Firefox notch a quarter of a Chrome one.
/// Wheel events with the zoom modifier are left out: egui reports those as
/// `zoom_delta`. Shift turns the wheel sideways, which pans.
fn wheel_notches(events: &[egui::Event]) -> f32 {
    let mut sum = 0.0;
    for event in events {
        let egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
        } = event
        else {
            continue;
        };
        if modifiers.command || modifiers.ctrl || modifiers.shift {
            continue;
        }
        sum += match unit {
            egui::MouseWheelUnit::Point => delta.y / NOTCH_POINTS,
            egui::MouseWheelUnit::Line => delta.y / NOTCH_LINES,
            egui::MouseWheelUnit::Page => delta.y,
        };
    }
    sum
}

impl eframe::App for CaptureViewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.take_incoming();
        for file in ctx.input(|i| i.raw.dropped_files.clone()) {
            if let Some(bytes) = file.bytes {
                self.load_bytes(&bytes);
            }
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(theme::CANVAS())
                    .inner_margin(0.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&self.status).color(theme::TEXT()));
                    ui.label(egui::RichText::new(self.backend).color(theme::MUTED()));
                });
                let avail = ui.available_rect_before_wrap();
                let body = Rect::from_min_size(
                    Pos2::new(avail.min.x + LABEL_W, avail.min.y),
                    Vec2::new((avail.width() - LABEL_W).max(1.0), avail.height().max(1.0)),
                );
                let max_scroll = (self.tracks.total_height() - body.height()).max(0.0);

                let resp = ui.interact(
                    body,
                    ui.id().with("capture-timeline"),
                    Sense::click_and_drag(),
                );
                self.navigate(ui, &resp, body, max_scroll);

                let painter = ui.painter().with_clip_rect(avail);
                for row in self.tracks.rows() {
                    if matches!(row.id, RowId::Lane(_)) {
                        continue;
                    }
                    let y = row.y - self.scroll;
                    if y + row.height < 0.0 || y > body.height() {
                        continue;
                    }
                    let text = self.label(row.id);
                    if text.is_empty() {
                        continue;
                    }
                    painter.text(
                        Pos2::new(avail.min.x + 6.0, avail.min.y + y),
                        Align2::LEFT_TOP,
                        text,
                        FontId::proportional(12.0),
                        theme::TEXT(),
                    );
                }

                if self.opened.index.event_count() == 0 {
                    painter.text(
                        body.center(),
                        Align2::CENTER_CENTER,
                        "No capture open",
                        FontId::proportional(14.0),
                        theme::MUTED(),
                    );
                    return;
                }
                if !self.has_gpu {
                    painter.text(
                        body.center(),
                        Align2::CENTER_CENTER,
                        "No GPU device to draw the capture with",
                        FontId::proportional(14.0),
                        Color32::from_rgb(0xBF, 0x61, 0x6A),
                    );
                    return;
                }

                let ppp = ui.ctx().pixels_per_point();
                let (payload, view) = self.payload(body, ppp, ui.ctx().screen_rect());
                painter.add(paint_callback(body, payload, view, 0));
            });
    }
}

fn visible_layout(
    tracks: &TrackStrip,
    scroll: f32,
    view_h: f32,
) -> Vec<(perfr_live_event::LaneKey, f32)> {
    tracks
        .layout()
        .iter()
        .filter_map(|(key, y)| {
            let y = y - scroll;
            if (-64.0..view_h + 64.0).contains(&y) {
                Some((*key, y))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
fn location_search() -> Option<String> {
    web_sys::window()?.location().search().ok()
}

#[cfg(target_arch = "wasm32")]
fn capture_url_from_location() -> Option<String> {
    query_capture(&location_search()?)
}

/// Fetches `url` into `slot`, then asks for a frame: the page is idle while
/// the download runs and would not otherwise look at the slot again.
#[cfg(target_arch = "wasm32")]
fn spawn_fetch(url: String, slot: Incoming, ctx: egui::Context) {
    wasm_bindgen_futures::spawn_local(async move {
        let result = fetch_bytes(&url).await;
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(result);
        }
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    use wasm_bindgen::JsCast;
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|err| format!("fetch failed: {err:?}"))?;
    let resp: web_sys::Response = resp_value
        .dyn_into()
        .map_err(|_| "fetch did not return a Response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {} for {url}", resp.status()));
    }
    let buf = resp.array_buffer().map_err(|err| format!("{err:?}"))?;
    let buf = wasm_bindgen_futures::JsFuture::from(buf)
        .await
        .map_err(|err| format!("{err:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// `?capture=<url>` from a query string. The value is percent-decoded and
/// handed to `fetch` as-is: a path next to the page, or an absolute URL.
#[cfg(any(test, target_arch = "wasm32"))]
fn query_capture(search: &str) -> Option<String> {
    for kv in search.trim_start_matches('?').split('&') {
        let mut parts = kv.splitn(2, '=');
        if parts.next() != Some("capture") {
            continue;
        }
        let raw = parts.next().unwrap_or("");
        if raw.is_empty() {
            return None;
        }
        return Some(crate::url::percent_decode(raw));
    }
    None
}

/// `?webgl` asks for the WebGL backend. It is honoured only by a pack built
/// with `--features webgl`; the default pack does not link the GLES backend.
#[cfg(any(test, target_arch = "wasm32"))]
fn query_asks_webgl(search: &str) -> bool {
    search
        .trim_start_matches('?')
        .split('&')
        .any(|kv| kv == "webgl" || kv == "webgl=1" || kv == "webgl=true")
}

#[cfg(any(test, target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    WebGpu,
    WebGl,
}

/// What `start_viewer` rejects with when the page cannot draw. `viewer.html`
/// shows it in place of the canvas.
#[cfg(any(test, target_arch = "wasm32"))]
const NO_WEBGPU: &str = "This viewer draws with WebGPU, and this browser did not provide a WebGPU adapter.";

/// The backend to start on. WebGPU when the browser has an adapter for it.
/// A pack with WebGL linked falls back to it without being asked, and takes
/// it when `?webgl` asks; a pack without it has nothing to fall back to and
/// says so, where it used to leave a blank canvas.
#[cfg(any(test, target_arch = "wasm32"))]
fn pick_backend(has_webgpu: bool, webgl_linked: bool, asked_webgl: bool) -> Result<Backend, &'static str> {
    if webgl_linked && (asked_webgl || !has_webgpu) {
        Ok(Backend::WebGl)
    } else if has_webgpu {
        Ok(Backend::WebGpu)
    } else {
        Err(NO_WEBGPU)
    }
}

/// Whether `navigator.gpu.requestAdapter()` yields an adapter. Firefox
/// without WebGPU has no `navigator.gpu`; a blocklisted GPU has one that
/// resolves to null.
///
/// Asked twice before the answer is no. Chrome resolves the first request
/// after it launches to null when its GPU process is still starting (seen
/// after 1.7 s, with the next request answered in 30 ms), and a page opened
/// then would be told it has no WebGPU.
#[cfg(target_arch = "wasm32")]
async fn has_webgpu_adapter() -> bool {
    for _ in 0..2 {
        if request_webgpu_adapter().await {
            return true;
        }
    }
    false
}

#[cfg(target_arch = "wasm32")]
async fn request_webgpu_adapter() -> bool {
    use wasm_bindgen::{JsCast, JsValue};
    let get = |target: &JsValue, key: &str| {
        js_sys::Reflect::get(target, &JsValue::from_str(key))
            .ok()
            .filter(|v| !v.is_undefined() && !v.is_null())
    };
    let Some(window) = web_sys::window() else {
        return false;
    };
    let Some(gpu) = get(&window, "navigator").and_then(|nav| get(&nav, "gpu")) else {
        return false;
    };
    let Some(request) = get(&gpu, "requestAdapter").and_then(|f| f.dyn_into::<js_sys::Function>().ok())
    else {
        return false;
    };
    let Some(promise) = request
        .call0(&gpu)
        .ok()
        .and_then(|p| p.dyn_into::<js_sys::Promise>().ok())
    else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .is_ok_and(|adapter| !adapter.is_undefined() && !adapter.is_null())
}

/// Browser entry. The page calls this with its canvas after `init()`. It
/// rejects with a sentence for the reader when the page cannot draw.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub async fn start_viewer(canvas: web_sys::HtmlCanvasElement) -> Result<(), wasm_bindgen::JsValue> {
    console_error_panic_hook::set_once();
    let asked_webgl = location_search().is_some_and(|s| query_asks_webgl(&s));
    let backend = pick_backend(
        has_webgpu_adapter().await,
        cfg!(feature = "webgl"),
        asked_webgl,
    )
    .map_err(wasm_bindgen::JsValue::from_str)?;
    let mut web_options = eframe::WebOptions::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut web_options.wgpu_options.wgpu_setup
    {
        setup.instance_descriptor.backends = match backend {
            Backend::WebGpu => eframe::wgpu::Backends::BROWSER_WEBGPU,
            Backend::WebGl => eframe::wgpu::Backends::GL,
        };
    }
    eframe::WebRunner::new()
        .start(
            canvas,
            web_options,
            Box::new(|cc| Ok(Box::new(CaptureViewer::new(cc)))),
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::{LiveEvent, LIVE_EVENT_SIZE};
    use perfr_live_protocol::{encode_frame, LiveFrame, VERSION};

    fn scope(tid: u32, start_ns: u64, duration_ns: u64) -> LiveEvent {
        LiveEvent {
            start_ns,
            duration_ns,
            tid,
            pid: 7,
            kind: perfr_live_event::kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: 1,
        }
    }

    fn stream(events: Vec<LiveEvent>) -> Vec<u8> {
        let mut bytes = encode_frame(&LiveFrame::Hello {
            version: VERSION,
            event_size: LIVE_EVENT_SIZE as u16,
        });
        bytes.extend(encode_frame(&LiveFrame::EventBatch { events }));
        bytes.extend(encode_frame(&LiveFrame::CaptureFinished));
        bytes
    }

    fn viewer() -> CaptureViewer {
        CaptureViewer {
            opened: OpenedCapture::default(),
            tracks: TrackStrip::default(),
            t0: 0.0,
            t1: 1.0,
            scroll: 0.0,
            has_gpu: false,
            backend: "test",
            status: String::new(),
            incoming: Arc::new(Mutex::new(None)),
            capture_gen: 0,
            drawn: None,
        }
    }

    fn body() -> Rect {
        Rect::from_min_size(Pos2::new(160.0, 20.0), Vec2::new(800.0, 400.0))
    }

    fn screen() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(960.0, 420.0))
    }

    #[test]
    fn capture_query_is_decoded() {
        assert_eq!(query_capture(""), None);
        assert_eq!(
            query_capture("?capture=demo.perfr.stream"),
            Some("demo.perfr.stream".into())
        );
        assert_eq!(
            query_capture("?webgl=1&capture=a%20b.perfr.stream"),
            Some("a b.perfr.stream".into())
        );
        // A literal plus is part of the path, not a space.
        assert_eq!(
            query_capture("?capture=runs/build+debug.perfr.stream"),
            Some("runs/build+debug.perfr.stream".into())
        );
    }

    #[test]
    fn webgl_is_asked_for_by_name_only() {
        assert!(query_asks_webgl("?webgl"));
        assert!(query_asks_webgl("?capture=a&webgl=1"));
        assert!(!query_asks_webgl("?capture=webgl"));
        assert!(!query_asks_webgl(""));
    }

    #[test]
    fn a_browser_without_webgpu_gets_webgl_or_a_sentence() {
        // The default pack: WebGPU or nothing, and nothing is an error the
        // page can show, whether or not `?webgl` was asked for.
        assert_eq!(pick_backend(true, false, false), Ok(Backend::WebGpu));
        assert_eq!(pick_backend(true, false, true), Ok(Backend::WebGpu));
        assert_eq!(pick_backend(false, false, false), Err(NO_WEBGPU));
        assert_eq!(pick_backend(false, false, true), Err(NO_WEBGPU));
        // A pack with WebGL linked falls back on its own.
        assert_eq!(pick_backend(false, true, false), Ok(Backend::WebGl));
        assert_eq!(pick_backend(true, true, false), Ok(Backend::WebGpu));
        assert_eq!(pick_backend(true, true, true), Ok(Backend::WebGl));
    }

    #[test]
    fn the_page_behind_the_canvas_is_the_default_canvas_colour() {
        // The HTML paints before wasm; another colour there is a flash.
        let canvas = perfr_live_event::theme::default_theme().canvas;
        let css = format!(
            "#{:02X}{:02X}{:02X}",
            (canvas >> 16) & 0xFF,
            (canvas >> 8) & 0xFF,
            canvas & 0xFF
        );
        assert!(
            include_str!("../viewer.html").contains(&format!("background: {css};")),
            "page background should be the default canvas {css}"
        );
    }

    #[test]
    fn every_character_the_page_writes_itself_is_in_the_latin_font() {
        // The subset covers U+0020-7E and U+00A0-FF. An em dash in the status
        // line was drawn as `?` on every page.
        let source = include_str!("page.rs");
        let shell = source.split("#[cfg(test)]\nmod tests").next().unwrap_or("");
        for (n, line) in shell.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            for ch in code.chars() {
                let c = ch as u32;
                assert!(
                    (0x20..=0x7E).contains(&c) || (0xA0..=0xFF).contains(&c),
                    "line {}: {ch:?} is not in the Latin subset",
                    n + 1
                );
            }
        }
    }

    #[test]
    fn a_capture_near_time_zero_fits_without_a_negative_window() {
        // A browser recording counts from page load, so its first event is
        // close to zero and 5% of the span before it would be negative.
        let mut v = viewer();
        assert_eq!(v.load_bytes(&stream(vec![scope(1, 1_000_000_000, 60_000_000_000)])), None);
        assert_eq!(v.t0, 0.0);
        assert!(v.t1 > 61_000_000_000.0, "the end keeps its pad: {}", v.t1);
        assert_eq!(Some((v.t0, v.t1)), v.limits());

        // Away from zero the pad is on both sides.
        let (lo, hi) = view_limits((100_000, 200_000));
        assert_eq!((lo, hi), (95_000.0, 205_000.0));
    }

    #[test]
    fn a_window_is_moved_back_inside_the_limits_at_the_same_span() {
        let (lo, hi) = (0.0, 1_000.0);
        assert_eq!(clamp_window(-300.0, 200.0, lo, hi), (0.0, 500.0));
        assert_eq!(clamp_window(900.0, 1_400.0, lo, hi), (500.0, 1_000.0));
        assert_eq!(clamp_window(100.0, 600.0, lo, hi), (100.0, 600.0));
        // Wider than the limits: the limits.
        assert_eq!(clamp_window(-5_000.0, 5_000.0, lo, hi), (0.0, 1_000.0));
        // Narrower than the floor: the floor.
        assert_eq!(clamp_window(500.0, 500.0, lo, hi), (500.0, 500.0 + MIN_SPAN_NS));
        // Both edges before zero, which used to reach the renderer as an
        // empty window.
        assert_eq!(clamp_window(-900.0, -400.0, lo, hi), (0.0, 500.0));
    }

    #[test]
    fn zoom_keeps_the_time_under_the_pointer() {
        let (lo, hi) = (0.0, 1_000_000.0);
        let (t0, t1) = (200_000.0, 600_000.0);
        let frac = 0.25;
        let anchor = t0 + (t1 - t0) * frac;
        let (z0, z1) = zoom_window(t0, t1, 0.5, frac, lo, hi);
        assert_eq!(z1 - z0, 200_000.0);
        assert!((z0 + (z1 - z0) * frac - anchor).abs() < 1e-6);
        // Zooming out past the capture stops at its limits.
        assert_eq!(zoom_window(t0, t1, 100.0, frac, lo, hi), (lo, hi));
        // Zooming in stops at the floor.
        let (m0, m1) = zoom_window(t0, t1, 1e-12, frac, lo, hi);
        assert_eq!(m1 - m0, MIN_SPAN_NS);
    }

    #[test]
    fn zoom_follows_how_far_the_wheel_moved() {
        let wheel = |unit, y: f32| egui::Event::MouseWheel {
            unit,
            delta: Vec2::new(0.0, y),
            modifiers: egui::Modifiers::NONE,
        };
        // One notch is one step, in either browser's unit.
        let chrome = wheel_notches(&[wheel(egui::MouseWheelUnit::Point, 100.0)]);
        let firefox = wheel_notches(&[wheel(egui::MouseWheelUnit::Line, 3.0)]);
        assert_eq!(chrome, 1.0);
        assert_eq!(firefox, 1.0);
        assert!((zoom_scale(1.0, 1.0) - ZOOM_PER_NOTCH).abs() < 1e-12);
        assert!((zoom_scale(-1.0, 1.0) - 1.0 / ZOOM_PER_NOTCH).abs() < 1e-12);

        // A second of trackpad scrolling, 2 points on each of 60 frames, is
        // 1.2 notches in all. A fixed 15% step per frame made it 0.85^60, a
        // window 17,000 times narrower.
        let mut span = 1.0;
        for _ in 0..60 {
            span *= zoom_scale(wheel_notches(&[wheel(egui::MouseWheelUnit::Point, 2.0)]), 1.0);
        }
        assert!((span - ZOOM_PER_NOTCH.powf(1.2)).abs() < 1e-6, "{span}");

        // No wheel, no zoom; a flick is capped; a pinch zooms in.
        assert_eq!(zoom_scale(0.0, 1.0), 1.0);
        assert_eq!(zoom_scale(500.0, 1.0), zoom_scale(MAX_NOTCHES_PER_FRAME, 1.0));
        assert_eq!(zoom_scale(0.0, 2.0), 0.5);

        // Ctrl+wheel is egui's zoom gesture and arrives as `zoom_delta`.
        let ctrl = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: Vec2::new(0.0, 100.0),
            modifiers: egui::Modifiers::CTRL,
        };
        assert_eq!(wheel_notches(&[ctrl]), 0.0);
    }

    #[test]
    fn an_unchanged_view_keeps_the_gpu_buffers() {
        let mut v = viewer();
        v.load_bytes(&stream(vec![scope(1, 1_000, 5_000), scope(2, 2_000, 5_000)]));
        let (first, view) = v.payload(body(), 1.0, screen());
        assert!(!matches!(first, TimelinePayload::Keep | TimelinePayload::Empty));

        // The next frame, with nothing moved: nothing is rebuilt, and the
        // picture stays where it was drawn.
        let (second, kept) = v.payload(body(), 1.0, screen());
        assert!(matches!(second, TimelinePayload::Keep));
        assert_eq!(kept.dest, view.dest);

        // A pan, a scroll, a resize, a display scale or another capture each
        // rebuild it.
        v.t0 += 500.0;
        v.t1 += 500.0;
        assert!(!matches!(v.payload(body(), 1.0, screen()).0, TimelinePayload::Keep));
        assert!(matches!(v.payload(body(), 1.0, screen()).0, TimelinePayload::Keep));
        v.scroll += 4.0;
        assert!(!matches!(v.payload(body(), 1.0, screen()).0, TimelinePayload::Keep));
        let wider = Rect::from_min_size(body().min, Vec2::new(900.0, 400.0));
        assert!(!matches!(v.payload(wider, 1.0, screen()).0, TimelinePayload::Keep));
        assert!(!matches!(v.payload(wider, 2.0, screen()).0, TimelinePayload::Keep));
        v.load_bytes(&stream(vec![scope(1, 1_000, 5_000), scope(2, 2_000, 5_000)]));
        assert!(!matches!(v.payload(wider, 2.0, screen()).0, TimelinePayload::Keep));
    }

    #[test]
    fn the_column_texture_is_drawn_over_the_rows_it_holds() {
        // Scopes too narrow to draw one by one, so the view is the
        // pixel-column texture, which holds only the placed rows.
        let events: Vec<LiveEvent> = (0..40_000u64)
            .map(|i| scope(1 + (i % 3) as u32, 1_000 + i * 10, 4))
            .collect();
        let mut v = viewer();
        v.load_bytes(&stream(events));
        let (payload, view) = v.payload(body(), 1.0, screen());
        let TimelinePayload::Pixel {
            place: Some((top, rows)),
            ..
        } = payload
        else {
            panic!("a zoomed-out busy capture is drawn as pixel columns");
        };
        assert!(rows < body().height(), "three lanes do not fill a 400 pt body");
        assert_eq!(view.dest, [body().left(), body().top() + top, body().width(), rows]);
        assert_eq!(blit_rect(body(), &TimelinePayload::Empty), body());
    }

    /// The listing gives a run of events that are each under a pixel wide
    /// one instance per pixel column. The page takes that through
    /// `TimelinePayload::from_index`; listed one each, a busy capture is
    /// hundreds of thousands of instances for every frame of a pan.
    #[test]
    fn events_under_a_pixel_wide_share_an_instance_per_column() {
        const TINY: u64 = 100_000;
        let mut events = vec![scope(1, 0, 1_000_000_000)];
        events.extend((0..TINY).map(|i| scope(2, i * 10_000, 1_000)));
        let mut v = viewer();
        assert_eq!(v.load_bytes(&stream(events)), None);
        let (payload, _) = v.payload(body(), 1.0, screen());
        let TimelinePayload::Instanced { instances } = payload else {
            panic!("a scope as wide as the view is drawn as instances");
        };
        let columns = body().width() as usize;
        assert!(
            instances.len() <= 2 * columns + 1,
            "{} instances for {columns} columns of {TINY} events",
            instances.len()
        );
    }
}
