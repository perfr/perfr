// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Version line shared by `--version`, `perfr version`, and `perfr update`.

pub fn package_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn git_hash() -> &'static str {
    env!("PERFR_GIT_HASH")
}

pub fn build_date() -> &'static str {
    env!("PERFR_BUILD_DATE")
}

/// `0.1.0 (abc1234, 2026-11-04)`. The hash and date come from `build.rs`.
pub fn long_version() -> String {
    format!("{} ({}, {})", package_version(), git_hash(), build_date())
}

/// What `perfr --version` prints.
pub fn version_line() -> String {
    format!("perfr {}", long_version())
}
