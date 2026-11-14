// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! WASM client for the perfr live stream.
//!
//! Shipped UI is **eframe WebRunner** (`start_eframe`). Chrome is egui
//! widgets; the timeline is one `PaintCallback` (hybrid wgpu).
//! Parsing of ELF/DWARF and protobuf stays on the service.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub use wasm_bindgen_rayon::init_thread_pool;

#[cfg(feature = "egui")]
mod app;
#[cfg(feature = "egui")]
mod chrome_load;
mod code;
#[cfg(feature = "egui")]
mod dev;
mod live;
mod local_report;
mod logging;
#[cfg(feature = "egui")]
mod fonts;
#[cfg(feature = "egui")]
mod frame_marks;
// The stream assembler lives in `perfr-viewer`. This crate depends on that
// one; it does not depend on this crate.
pub(crate) use perfr_viewer::frames;
#[cfg(feature = "egui")]
mod net;
#[cfg(feature = "egui")]
mod presets;
#[cfg(feature = "egui")]
mod logs;
mod track_order;
mod rect_select;
#[cfg(feature = "egui")]
mod url_params;
#[cfg(feature = "egui")]
mod util;
mod report_copy;
#[cfg(feature = "egui")]
mod share;
#[cfg(feature = "egui")]
mod story;
mod self_pane;
#[cfg(feature = "egui")]
pub(crate) use perfr_viewer::theme;
#[cfg(feature = "egui")]
pub(crate) use perfr_viewer::timeline;
#[cfg(feature = "egui")]
pub use perfr_viewer::tracks;
#[cfg(feature = "egui")]
mod vscroll;

#[cfg(feature = "egui")]
pub use app::PerfrLiveApp;

/// `globalThis.performance.timeOrigin + .now()` — works on Window and
/// DedicatedWorker, and is comparable *between* them.
///
/// `performance.now()` alone is not. It is measured from each context's own
/// `timeOrigin`, and a worker is created after the page, so a worker reading is
/// systematically smaller than a main-thread reading taken at the same instant.
/// `absorb_worker_spans` subtracts the main thread's frame origin and saturates
/// at zero, so every pool worker's span collapsed onto rel 0 and stacked on top
/// of the others in its lane. Adding `timeOrigin` makes both epoch-relative.
#[cfg(target_arch = "wasm32")]
fn wasm_now_ns() -> u64 {
    use wasm_bindgen::JsCast;
    let global = js_sys::global();
    let Ok(perf) = js_sys::Reflect::get(&global, &JsValue::from_str("performance")) else {
        return 0;
    };
    if perf.is_undefined() || perf.is_null() {
        return 0;
    }
    let Ok(now_fn) = js_sys::Reflect::get(&perf, &JsValue::from_str("now")) else {
        return 0;
    };
    let Ok(now_fn) = now_fn.dyn_into::<js_sys::Function>() else {
        return 0;
    };
    let Some(now_ms) = now_fn.call0(&perf).ok().and_then(|v| v.as_f64()) else {
        return 0;
    };
    let origin_ms = js_sys::Reflect::get(&perf, &JsValue::from_str("timeOrigin"))
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);
    // Converted separately on purpose. `origin_ms` is ~1.8e12, so folding it
    // into one f64 multiply would round the sum to ~256 ns and throw away the
    // sub-microsecond resolution of `now_ms`, which is the part that actually
    // times a lane chunk.
    ((origin_ms * 1_000_000.0) as u64).saturating_add((now_ms * 1_000_000.0) as u64)
}

#[cfg(target_arch = "wasm32")]
fn install_wasm_clock() {
    perfr_live_event::dev::set_now_hook(wasm_now_ns);
}

/// Called after JS `initThreadPool` resolves. `n == 1` keeps collect/raster
/// sequential (SAB missing / init failed).
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = markWasmPoolReady)]
pub fn mark_wasm_pool_ready(n: u32) {
    perfr_live_render::set_wasm_pool_threads(n as usize);
}

/// Browser entry: eframe WebRunner on the given canvas. Native window is not used.
/// JS must call `initThreadPool` (when present) *before* this, then
/// `markWasmPoolReady`.
#[cfg(all(feature = "egui", target_arch = "wasm32"))]
#[wasm_bindgen]
pub async fn start_eframe(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    // Console plus a relay to the service's log file (logging.rs). The
    // viewer's own lines at info; eframe's ("event handlers installed.")
    // and the rest at warn and above, as before.
    logging::install();
    install_wasm_clock();
    log::info!(
        "{} starting; {}",
        perfr_build_info::version_line(),
        web_sys::window()
            .and_then(|w| w.navigator().user_agent().ok())
            .unwrap_or_else(|| "unknown user agent".into())
    );
    // Default to the WebGL2 backend. WebGPU (Dawn) init fails on some
    // Linux/NVIDIA drivers -- "vkGetSemaphoreFdKHR ... VK_ERROR_INITIALIZATION
    // _FAILED" -- and once picked, wgpu cannot fall back mid-run; WebGL2 draws
    // the instanced timeline fine. `?webgpu` opts back in where it works.
    let mut web_options = eframe::WebOptions::default();
    if !crate::url_params::query_prefers_webgpu_from_location() {
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) =
            &mut web_options.wgpu_options.wgpu_setup
        {
            setup.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
    } else {
        log::info!("?webgpu: asking for the WebGPU backend");
    }
    eframe::WebRunner::new()
        .start(
            canvas,
            web_options,
            Box::new(|cc| Ok(Box::new(PerfrLiveApp::new(cc)))),
        )
        .await
}

#[cfg(test)]
mod page_tests {
    #[test]
    fn web_page_background_matches_the_default_canvas() {
        // The HTML behind the canvas paints before wasm. It has to be the
        // default canvas, or the first paint flashes another palette. The
        // viewer checks its own page in `perfr-viewer`.
        let html = include_str!("../viewer-dist/index.html");
        let canvas = perfr_live_event::theme::default_theme().canvas;
        let css = format!(
            "#{:02X}{:02X}{:02X}",
            (canvas >> 16) & 0xFF,
            (canvas >> 8) & 0xFF,
            canvas & 0xFF
        );
        assert!(
            html.contains(&format!("background: {css};")),
            "page background should be the default canvas {css}"
        );
    }
}
