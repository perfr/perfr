// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Self-profile frames live in the viewer core. URL parameters stay here,
//! because they name this page (the service client, Share, Chrome import).
//! `app.rs` still reaches both through `crate::dev`.

pub use crate::url_params::*;
pub use perfr_viewer::dev::*;
