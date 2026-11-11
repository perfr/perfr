// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! WGSL for the hybrid GPU timeline.
//!
//! Pixel-column path: nearest-neighbor blit of the CPU raster into its
//! destination rect.
//! Instanced path: SDF rounded rects, antialiased at the edge. No drop
//! shadow: the quad expansion it needed was overdraw on every scope.
//!
//! Sources live in `src/shaders/*.wgsl` so editors, `wgsl-analyzer`, and the
//! `naga` CLI can see them; `include_str!` bakes them in at compile time.

/// Instanced scope rects: SDF rounded boxes.
pub const INSTANCE_WGSL: &str = include_str!("shaders/instance.wgsl");

/// Textured dest-rect blit for the zoomed-out pixel-column LOD.
pub const BLIT_RECT_WGSL: &str = include_str!("shaders/blit_rect.wgsl");
