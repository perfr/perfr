// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Thin page around the viewer core: open a `.perfr.stream` and draw it.
//!
//! This is the embeddable shell (`start_embed`). It does not talk to the
//! capture service, import Chrome or Perfetto traces, or draw the live
//! product's chrome. The full page is `perfr-live-viewer`.

use std::sync::{Arc, Mutex};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Vec2};
use perfr_live_render::{choose_lod, ThreadFocus, INSTANCE_MIN_PX};

use crate::dev::DevFrame;
use crate::open::{open_capture, OpenedCapture};
use crate::theme;
use crate::timeline::{
    paint_callback, TimelineGpu, TimelineGpuSlot, TimelinePayload, ViewUniforms,
};
use crate::tracks::{RowId, TrackStrip};

/// A capture on a canvas. Wasm boots it from [`start_embed`]; native tests
/// drive [`Self::load_bytes`].
pub struct CaptureViewer {
    opened: OpenedCapture,
    tracks: TrackStrip,
    t0: f64,
    t1: f64,
    scroll: f32,
    has_gpu: bool,
    backend: &'static str,
    status: String,
    incoming: Arc<Mutex<Option<Result<Vec<u8>, String>>>>,
    waiting: bool,
}

/// Latin subset of Hack (U+0020–U+007E and U+00A0–U+00FF). The four egui
/// default fonts are about 1.4 MB of TTF; this page only draws track labels
/// and a status line. License: `fonts/LICENSE-Hack.txt`.
#[cfg(feature = "slim-fonts")]
fn install_embed_font(ctx: &egui::Context) {
    use std::sync::Arc;
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
        #[cfg(feature = "slim-fonts")]
        install_embed_font(&cc.egui_ctx);
        let mut has_gpu = false;
        let mut backend = "no wgpu device";
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
        let incoming = Arc::new(Mutex::new(None));
        // `waiting` is only written when a wasm page has `?capture=`.
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
            waiting: false,
        };
        #[cfg(target_arch = "wasm32")]
        if let Some(url) = capture_url_from_location() {
            view.waiting = true;
            view.status = format!("Loading {url}");
            spawn_fetch(url, incoming);
        }
        view
    }

    /// Replace the open capture with `bytes`. Returns the parse fault, when
    /// the bytes were not a whole stream.
    pub fn load_bytes(&mut self, bytes: &[u8]) -> Option<String> {
        self.opened = open_capture(bytes);
        self.tracks = TrackStrip::default();
        self.scroll = 0.0;
        self.fit();
        self.tracks.sync(&self.opened.index, None);
        self.tracks.tick(0.0, &self.opened.index, None);
        let n = self.opened.index.event_count();
        self.status = match &self.opened.fault {
            Some(fault) => format!("{n} events, then {fault}"),
            None if n == 0 => "That file has no events.".into(),
            None => format!("{n} events"),
        };
        self.waiting = false;
        self.opened.fault.clone()
    }

    fn fit(&mut self) {
        if let Some((a, b)) = self.opened.index.time_bounds() {
            let span = b.saturating_sub(a).max(1) as f64;
            let pad = span * 0.05;
            self.t0 = a as f64 - pad;
            self.t1 = b as f64 + pad;
        }
    }

    fn take_incoming(&mut self) {
        let next = self.incoming.lock().ok().and_then(|mut slot| slot.take());
        if let Some(result) = next {
            match result {
                Ok(bytes) => {
                    self.load_bytes(&bytes);
                }
                Err(err) => {
                    self.waiting = false;
                    self.status = err;
                }
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
}

impl eframe::App for CaptureViewer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.take_incoming();
        if self.waiting {
            ctx.request_repaint();
        }
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
                ui.label(
                    egui::RichText::new(format!("{} — {}", self.status, self.backend))
                        .color(theme::TEXT()),
                );
                let avail = ui.available_rect_before_wrap();
                let label_w = 160.0;
                let body = Rect::from_min_size(
                    Pos2::new(avail.min.x + label_w, avail.min.y),
                    Vec2::new((avail.width() - label_w).max(1.0), avail.height().max(1.0)),
                );
                self.tracks.sync(&self.opened.index, None);
                self.tracks.tick(0.0, &self.opened.index, None);
                let total_h = self.tracks.total_height();
                let max_scroll = (total_h - body.height()).max(0.0);
                self.scroll = self.scroll.clamp(0.0, max_scroll);

                let pointer = ui.input(|i| i.pointer.hover_pos());
                let over_labels = pointer.is_some_and(|p| p.x < body.min.x);
                let wheel = ui.input(|i| i.raw_scroll_delta.y);
                if wheel != 0.0 && over_labels {
                    self.scroll = (self.scroll - wheel).clamp(0.0, max_scroll);
                }

                let resp = ui.interact(
                    body,
                    ui.id().with("capture-timeline"),
                    Sense::click_and_drag(),
                );
                if resp.double_clicked() {
                    self.fit();
                } else if resp.dragged() {
                    let span = (self.t1 - self.t0).max(1.0);
                    let shift =
                        -(resp.drag_delta().x as f64) / f64::from(body.width().max(1.0)) * span;
                    self.t0 += shift;
                    self.t1 += shift;
                } else if wheel != 0.0 && resp.hovered() {
                    let factor = if wheel > 0.0 { 0.85 } else { 1.0 / 0.85 };
                    let frac = pointer
                        .map(|p| f64::from(((p.x - body.min.x) / body.width()).clamp(0.0, 1.0)))
                        .unwrap_or(0.5);
                    let span = (self.t1 - self.t0).max(1.0);
                    let anchor = self.t0 + span * frac;
                    let new_span = (span * factor).max(1.0);
                    self.t0 = anchor - new_span * frac;
                    self.t1 = anchor + new_span * (1.0 - frac);
                }

                let painter = ui.painter();
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
                        "No WebGPU or WebGL device",
                        FontId::proportional(14.0),
                        Color32::from_rgb(0xBF, 0x61, 0x6A),
                    );
                    return;
                }

                let ppp = ui.ctx().pixels_per_point();
                let screen = ui.ctx().screen_rect();
                let view = ViewUniforms::from_rect(
                    body,
                    ppp,
                    [screen.width() * ppp, screen.height() * ppp],
                );
                let t0 = self.t0.max(0.0) as u64;
                let t1 = (self.t1.max(self.t0 + 1.0)) as u64;
                let layout = visible_layout(&self.tracks, self.scroll, body.height());
                let dev = DevFrame::begin(false);
                let width_px = (body.width() * ppp).round().max(1.0) as usize;
                let lod = choose_lod(&self.opened.index, t0, t1, width_px, INSTANCE_MIN_PX);
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
fn capture_url_from_location() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    query_capture(&search)
}

#[cfg(target_arch = "wasm32")]
fn spawn_fetch(url: String, slot: Arc<Mutex<Option<Result<Vec<u8>, String>>>>) {
    wasm_bindgen_futures::spawn_local(async move {
        let result = fetch_bytes(&url).await;
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(result);
        }
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
pub fn query_capture(search: &str) -> Option<String> {
    for kv in search.trim_start_matches('?').split('&') {
        let mut parts = kv.splitn(2, '=');
        if parts.next() != Some("capture") {
            continue;
        }
        let raw = parts.next().unwrap_or("");
        if raw.is_empty() {
            return None;
        }
        return Some(percent_decode(raw));
    }
    None
}

#[cfg(any(test, target_arch = "wasm32"))]
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `?webgl` forces the WebGL backend. Compiled only with `--features webgl`;
/// the slim pack does not include the GLES backend.
#[cfg(all(target_arch = "wasm32", feature = "webgl"))]
fn query_prefers_webgl() -> bool {
    web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|s| {
            s.trim_start_matches('?')
                .split('&')
                .any(|kv| kv == "webgl" || kv == "webgl=1" || kv == "webgl=true")
        })
}

/// Browser entry. The page calls this with its canvas after `init()`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub async fn start_embed(canvas: web_sys::HtmlCanvasElement) -> Result<(), wasm_bindgen::JsValue> {
    console_error_panic_hook::set_once();
    let mut web_options = eframe::WebOptions::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut web_options.wgpu_options.wgpu_setup
    {
        setup.instance_descriptor.backends = {
            #[cfg(feature = "webgl")]
            {
                if query_prefers_webgl() {
                    eframe::wgpu::Backends::GL
                } else {
                    eframe::wgpu::Backends::BROWSER_WEBGPU
                }
            }
            #[cfg(not(feature = "webgl"))]
            {
                eframe::wgpu::Backends::BROWSER_WEBGPU
            }
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
    }
}
