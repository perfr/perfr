// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The files in `testdata/`, for the tests of every module.
//!
//! They are the ones the C++ `ObjectUtils` suite was judged against (see
//! the `cpp` branch of the Orbit repository), so both implementations read
//! the same bytes. `PERFR_TESTDATA` points elsewhere if set.

use std::path::PathBuf;

pub(crate) fn dir() -> PathBuf {
    PathBuf::from(
        std::env::var("PERFR_TESTDATA").unwrap_or_else(|_| format!("{}/testdata", env!("CARGO_MANIFEST_DIR"))),
    )
}

pub(crate) fn path(name: &str) -> PathBuf {
    dir().join(name)
}

pub(crate) fn read(name: &str) -> Vec<u8> {
    let path = path(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}
