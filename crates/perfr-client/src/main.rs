// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut service = None;
    let mut offline = false;
    let mut paths = Vec::new();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional && (arg == "--help" || arg == "-h") {
            println!("Usage: perfr-client [--service URL | --offline] [--] [CAPTURE_OR_TRACE] [STORY]\n\nWith no files, connect to http://127.0.0.1:44766.\nFiles open offline unless --service is supplied. Start the capture service separately.");
            return Ok(());
        } else if !positional && arg == "--service" {
            service = Some(
                args.next()
                    .ok_or("--service needs an HTTP(S) URL")?
                    .into_string()
                    .map_err(|_| "service URL must be UTF-8")?,
            );
        } else if !positional && arg == "--offline" {
            offline = true;
        } else if !positional && arg == "--" {
            positional = true;
        } else if !positional && arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()).into());
        } else {
            paths.push(std::path::PathBuf::from(arg));
        }
    }
    if offline && service.is_some() {
        return Err("--offline and --service cannot be combined".into());
    }
    let host = if offline || (!paths.is_empty() && service.is_none()) {
        perfr_viewer::Host::viewer()
    } else {
        perfr_client::native_host(service.as_deref().unwrap_or("http://127.0.0.1:44766"))?
    };
    perfr_viewer::run_native(host, paths).map_err(|e| e.to_string().into())
}
