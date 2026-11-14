// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The perfr viewer: the page that shows a capture.
//!
//! The interface is **eframe WebRunner**. Chrome is egui widgets; the
//! timeline is one `PaintCallback` (hybrid wgpu) over the pixel-column raster
//! and the instanced scope quads in `perfr-live-render`. Around it are the
//! track rail, the reports, search, the log window, stories and the import
//! of Chrome and Perfetto traces.
//!
//! Nothing of the capture pipeline is in here, and no connection to the
//! capture service either. What is behind the page is a [`net::Backend`]:
//! this crate has the one that reads a capture file, which is all its own
//! page (`start_viewer`) needs. `perfr-client` is the layer on top. It
//! supplies the backend that talks to the service, and with it capture
//! control and sharing ([`Host`]), and turns the `page` feature off so its
//! pack starts from its own entry point.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(all(target_arch = "wasm32", feature = "wasm-threads"))]
pub use wasm_bindgen_rayon::init_thread_pool;

mod app;
mod chrome_load;
pub mod code;
pub mod dev;
mod fonts;
mod frame_marks;
pub mod frames;
mod live;
mod local_report;
pub mod logging;
mod logs;
pub mod net;
mod presets;
mod rect_select;
mod report_copy;
mod self_pane;
pub mod share;
mod story;
pub mod theme;
pub mod timeline;
mod track_order;
pub mod tracks;
pub mod url;
pub mod url_params;
pub mod util;
mod vscroll;

pub use app::PerfrLiveApp;
pub use net::Host;

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
pub fn install_wasm_clock() {
    perfr_live_event::dev::set_now_hook(wasm_now_ns);
}

/// Called after JS `initThreadPool` resolves. `n == 1` keeps collect/raster
/// sequential (SAB missing / init failed).
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = markWasmPoolReady)]
pub fn mark_wasm_pool_ready(n: u32) {
    perfr_live_render::set_wasm_pool_threads(n as usize);
}

/// Starts the page on `canvas` over `host`: the logger, the clock, the
/// graphics backend and the eframe WebRunner. JS must call `initThreadPool`
/// (when present) *before* this, then `markWasmPoolReady`.
///
/// The viewer's own entry point ([`start_viewer`]) passes [`Host::viewer`];
/// `perfr-client` passes the host with the capture service in it.
#[cfg(target_arch = "wasm32")]
pub async fn run(canvas: web_sys::HtmlCanvasElement, host: Host) -> Result<(), JsValue> {
    // Console, plus a relay to the service's log file once a backend says
    // there is a service (logging.rs). The page's own lines at info;
    // eframe's ("event handlers installed.") and the rest at warn and above.
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
            Box::new(move |cc| Ok(Box::new(PerfrLiveApp::new(cc, host)))),
        )
        .await
}

/// The viewer's browser entry point: the page on `canvas`, with capture
/// files behind it and nothing else. It is the `page` feature, which
/// `perfr-client` turns off: an entry point compiled into a crate is
/// exported from every pack that links it.
#[cfg(all(feature = "page", target_arch = "wasm32"))]
#[wasm_bindgen]
pub async fn start_viewer(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    run(canvas, Host::viewer()).await
}
