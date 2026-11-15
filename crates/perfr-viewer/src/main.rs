// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The desktop shell: all loading, interaction and rendering live in the library.
fn main() -> eframe::Result {
    let mut paths = Vec::new();
    let mut positional = false;
    for arg in std::env::args_os().skip(1) {
        if !positional && (arg == "--help" || arg == "-h") {
            println!("Usage: perfr-viewer [--] [CAPTURE_OR_TRACE] [STORY]\n\nOpen local .perfr.stream, Chrome/Perfetto traces, or .story.json files.\nWith no files, use Open or drag a file into the window.");
            return Ok(());
        }
        if !positional && arg == "--" {
            positional = true;
        } else {
            paths.push(std::path::PathBuf::from(arg));
        }
    }
    perfr_viewer::run_native(perfr_viewer::Host::viewer(), paths)
}
