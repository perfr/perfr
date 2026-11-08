// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Version line shared by `--version`, `perfr version`, and the viewer's About window.
//!
//! `package_version` is the `crates/perfr` Cargo version. `perfr update` compares
//! that. The line people read is `version_line`: a release build is
//! `perfr 0.1.3 (abc1234def56, 2026-11-08, perfr-v0.1.3)`; a local build falls
//! back to `dev` plus the hash, `perfr dev+abc1234def56 (abc1234def56, 2026-11-08)`.
//! The stamp lives in `perfr-build-info`, which the viewer links too.

pub use perfr_build_info::{long_version, package_version, version_line};
