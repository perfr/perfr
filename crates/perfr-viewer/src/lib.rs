// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The viewer: opens a `.perfr.stream` and draws it.
//!
//! Nothing of the capture pipeline is in here:
//!
//! * [`open`] reads a `.perfr.stream` (the framed event format in
//!   `perfr-live-protocol`) into the timeline index.
//! * [`timeline`] is the WebGPU renderer: one egui paint callback over the
//!   pixel-column raster and the instanced scope quads in `perfr-live-render`.
//! * [`page`] is the viewer's own page, the wasm pack another page embeds
//!   (`build_wasm.sh`). It is the `page` feature, on by default.
//!
//! `perfr-capture` and the service depend on the same format crates. This
//! crate does not depend on them.
//!
//! `perfr-client`, the page the service serves, is built on this crate: it
//! adds the service client, Chrome/Perfetto import and the rest of the
//! product. It turns `page` off, so its pack has the library and none of the
//! page.

pub mod dev;
pub mod frames;
pub mod open;
pub mod tracks;
pub mod url;

#[cfg(feature = "page")]
pub mod page;
#[cfg(feature = "egui")]
pub mod theme;
#[cfg(feature = "egui")]
pub mod timeline;

pub use open::{open_capture, OpenedCapture};
