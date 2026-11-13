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
//! crate does not depend on them. It is a library with no wasm entry point
//! of its own; two pages are built on it:
//!
//! * `perfr-live-viewer`, the full product UI: the service client,
//!   Chrome/Perfetto import, and the rest of the page.
//! * `perfr-viewer-embed`, the slim page that opens one `.perfr.stream` and
//!   draws it. Its pack links this crate and none of the full page.

pub mod dev;
pub mod frames;
pub mod open;
pub mod tracks;
pub mod url;

#[cfg(feature = "egui")]
pub mod theme;
#[cfg(feature = "egui")]
pub mod timeline;

pub use open::{open_capture, OpenedCapture};
