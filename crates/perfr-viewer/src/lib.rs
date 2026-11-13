// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Viewer core for a capture.
//!
//! Two pieces, and nothing of the capture pipeline:
//!
//! * [`open`] reads a `.perfr.stream` (the framed event format in
//!   `perfr-live-protocol`) into the timeline index.
//! * [`timeline`] is the WebGPU renderer: one egui paint callback over the
//!   pixel-column raster and the instanced scope quads in `perfr-live-render`.
//!
//! `perfr-capture` and the service depend on the same format crates. This
//! crate does not depend on them, and the slim wasm pack
//! ([`build_wasm.sh`](../build_wasm.sh)) links only this crate. The full
//! product UI lives in `perfr-live-viewer`, which depends on this crate and
//! adds the service client, Chrome/Perfetto import, and the rest of the page.

pub mod dev;
pub mod frames;
pub mod open;
pub mod tracks;

#[cfg(feature = "egui")]
pub mod theme;
#[cfg(feature = "egui")]
pub mod timeline;

#[cfg(feature = "egui")]
mod embed;
#[cfg(feature = "egui")]
pub use embed::CaptureViewer;
#[cfg(feature = "egui")]
pub use open::{open_capture, OpenedCapture};
