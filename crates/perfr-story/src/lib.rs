// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A capture story: ordered steps that move the perfr viewer and draw
//! overlays on a capture.
//!
//! The document is JSON, `format` = `"perfr.story"`, `version` = `1`.
//! It is saved next to a capture or fetched on its own. It is not part of
//! the `.perfr.stream`. Coordinates are capture coordinates (nanoseconds,
//! process and thread ids, scope name ids), so a step stays put when the
//! page is resized or the time axis is zoomed.
//!
//! perfr-web can store the document as `captures/<id>.perfr.story` beside
//! `captures/<id>.perfr.stream` and open the viewer with
//! `?capture=<stream-url>&story=<story-url>`. `chrome=compact` hides the
//! capture controls for an iframe. A short link can later resolve to those
//! two URLs; this crate does not require that store.

mod camera;
mod demo;
mod format;

pub use camera::{
    ease_smoothstep, interpolate_camera, overlay_reveal, Camera, Player, Tick, TRANSITION_MS,
};
pub use demo::{demo_box3d, demo_box3d_json};
pub use format::{
    is_story_filename, parse_story, validate, Anchor, Fit, Highlight, Overlay, Scroll, Selection,
    Step, StepKind, Story, StoryError, TrackRef, View, FORMAT, VERSION,
};
