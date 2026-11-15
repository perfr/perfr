// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The perfr client: the viewer with the capture service behind it.
//!
//! This is the page the service serves. Its interface is `perfr-viewer`'s:
//! the timeline, the reports, search, stories. What this crate adds is what
//! needs the service: the backend that talks to it ([`service`]), and a
//! [`host`] that tells the viewer a service is there, which is when it shows
//! recording, the process picker, hooks, the service's settings and Share.

use perfr_viewer::net::{Host, Net};

#[cfg(any(target_arch = "wasm32", feature = "native"))]
pub mod service;
#[cfg(any(target_arch = "wasm32", feature = "native"))]
mod transport;

/// A native client connected to the given HTTP(S) service origin.
#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
pub fn native_host(url: &str) -> Result<Host, String> {
    let http = transport::Http::new(url)?;
    Ok(Host::with_service(move |_| Box::new(service::ServiceNet::native(http.clone()))))
}

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// The browser page's host: a capture service, and sharing.
/// Desktop applications use `native_host` with an explicit service origin.
pub fn host() -> Host {
    Host::with_service(connect)
}

/// The service that served the page, or, when the page's address names a
/// capture file (`?capture=`), that file with no socket opened.
fn connect(capture: Option<&str>) -> Net {
    #[cfg(target_arch = "wasm32")]
    {
        match capture {
            Some(url) => Box::new(service::ServiceNet::from_capture_url(url)),
            None => Box::new(service::ServiceNet::connect()),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // This browser host is also used by headless tests. Desktop
        // connections are constructed by native_host instead.
        let _ = capture;
        Box::new(perfr_viewer::net::NoBackend)
    }
}

/// Browser entry: the client on the given canvas. JS must call
/// `initThreadPool` (when present) *before* this, then `markWasmPoolReady`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn start_eframe(canvas: web_sys::HtmlCanvasElement) -> Result<(), JsValue> {
    perfr_viewer::run(canvas, host()).await
}

#[cfg(test)]
mod host_tests {
    #[test]
    fn the_client_has_a_service_and_shares() {
        let host = super::host();
        assert!(host.service());
        assert!(host.share());
    }
}

#[cfg(test)]
mod share_contract_tests {
    use perfr_viewer::share::*;

    /// The sign-in contract in `SHARE_AUTH.md`, the constants the viewer's
    /// Share dialog works from, and the script in this pack's page that
    /// receives the login all have to say the same thing.
    #[test]
    fn the_auth_doc_and_the_popup_page_use_this_contract() {
        let doc = include_str!("../SHARE_AUTH.md");
        for (name, _) in SHARE_CORS {
            assert!(doc.contains(name), "{name} missing from SHARE_AUTH.md");
        }
        assert!(doc.contains(SIGN_IN_TITLE));
        assert!(doc.contains(SHARE_TOKEN_FRAGMENT));
        assert!(doc.contains("SameSite=None"));
        assert!(doc.contains(ME_PATH));
        assert!(doc.contains(&format!("`Content-Type: {SHARE_CONTENT_TYPE}`")));
        assert_eq!(LOGIN_RESUME_KEY, "perfr.share.resume");
        assert_eq!(LOGIN_MESSAGE_TYPE, "login");
        assert!(ShareFailure::login("Your session expired. Sign in again to share.").needs_login);
        assert!(!ShareFailure::message("Nothing to share.").needs_login);
        let html = include_str!("../viewer-dist/index.html");
        assert!(html.contains(LOGIN_WINDOW_NAME));
        assert!(html.contains(LOGIN_SIGNAL_KEY));
        assert!(html.contains(SHARE_TOKEN_KEY));
        assert!(html.contains(LOGIN_PENDING_KEY));
        assert!(html.contains(SHARE_TOKEN_FRAGMENT));
        assert!(html.contains(LOGIN_MESSAGE_SOURCE));
    }
}

#[cfg(test)]
mod page_tests {
    #[test]
    fn web_page_background_matches_the_default_canvas() {
        // The HTML behind the canvas paints before wasm. It has to be the
        // default canvas, or the first paint flashes another palette. The
        // viewer's page is checked in `perfr-viewer`.
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
