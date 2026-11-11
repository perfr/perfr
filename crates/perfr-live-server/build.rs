// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Embed files under viewer-dist/ (including wasm-bindgen snippets/).
//!
//! Assets are copied into OUT_DIR and `include_bytes!`d relative to the
//! generated file, so the generated code holds no absolute source paths.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn mime_for(name: &str) -> &'static str {
    if name.ends_with(".png") {
        "image/png"
    } else if name.ends_with(".ico") {
        "image/x-icon"
    } else if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if name.ends_with(".wasm") {
        "application/wasm"
    } else if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if name.ends_with(".json") || name.ends_with(".story") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_files(&path, &rel, out);
        } else if path.is_file() {
            out.push((rel, path));
        }
    }
}

/// The pack is `viewer-dist/` in the viewer crate beside this one
/// (`crates/perfr-live-viewer/viewer-dist`, written by its `build_wasm.sh`).
/// `PERFR_VIEWER_DIST` overrides it; failing both, walk up from the manifest
/// dir so a vendored or relocated crate with the pack above it still finds it.
fn find_viewer_dist() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("PERFR_VIEWER_DIST") {
        let path = PathBuf::from(explicit);
        return path.is_dir().then_some(path);
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").ok()?);
    let sibling = manifest.join("../perfr-live-viewer/viewer-dist");
    if sibling.is_dir() {
        return Some(sibling);
    }
    let mut dir = manifest;
    loop {
        let candidate = dir.join("viewer-dist");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The `source=` line of the pack's `VERSION`: the id of the viewer sources
/// it was built from, as `source_id.py` computed it then.
fn pack_source_id(dist: &Path) -> Option<String> {
    let version = fs::read_to_string(dist.join("VERSION")).ok()?;
    version.lines().find_map(|line| line.strip_prefix("source=")).map(|id| id.trim().to_string())
}

/// The id of the viewer sources as they are now, and the files it covers
/// (relative to the checkout), from the viewer crate's `source_id.py`. `None`
/// when that cannot be asked: no script beside the pack (a pack from
/// elsewhere), no Python, not a git checkout. Then nothing is checked.
fn checkout_source_id(viewer: &Path) -> Option<(String, Vec<String>)> {
    let script = viewer.join("source_id.py");
    if !script.is_file() {
        return None;
    }
    let output = ["python3", "python"]
        .iter()
        .find_map(|python| Command::new(python).arg(&script).arg("--files").output().ok().filter(|o| o.status.success()))?;
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let id = lines.next()?.to_string();
    Some((id, lines.map(str::to_string).collect()))
}

/// Says so when the pack about to be embedded was not built from the viewer
/// sources of this checkout. Nothing did: a viewer change followed by a
/// build of the service produced a binary serving the previous viewer, and
/// the only sign was the change not showing up.
fn warn_if_pack_is_stale(dist: &Path) {
    // Resolved first: the pack is found as `<this crate>/../perfr-live-viewer/
    // viewer-dist`, and the parent of a path ending in `..` is not the
    // directory above it.
    let Ok(dist) = dist.canonicalize() else {
        return;
    };
    let Some(viewer) = dist.parent() else {
        return;
    };
    let Some((now, files)) = checkout_source_id(viewer) else {
        return;
    };
    // The script is the viewer crate's, two below the checkout's root.
    if let Some(root) = viewer.parent().and_then(Path::parent) {
        // Without these cargo would not run this script again after a
        // viewer source changed, and the warning would never be seen.
        for file in &files {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }
    let Some(built) = pack_source_id(&dist) else {
        return;
    };
    if built != now {
        println!(
            "cargo:warning=the viewer pack in {} is stale: it was built from viewer sources {built}, this checkout's are {now}. \
             The service will serve the old viewer; run crates/perfr-live-viewer/build_wasm.sh and build again.",
            dist.display()
        );
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=PERFR_VIEWER_DIST");
    let dist = find_viewer_dist();
    if let Some(dist) = &dist {
        println!("cargo:rerun-if-changed={}", dist.display());
        warn_if_pack_is_stale(dist);
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let assets_dir = out_dir.join("viewer_assets");

    let mut code = String::from(
        "pub fn get(path: &str) -> Option<(&'static str, &'static [u8])> {\n    match path {\n",
    );
    let mut files = Vec::new();
    if let Some(dist) = &dist {
        collect_files(dist, "", &mut files);
    }
    for (index, (rel, path)) in files.into_iter().enumerate() {
        println!("cargo:rerun-if-changed={}", path.display());
        // Flat, index-based names: asset paths carry subdirectories (wasm-bindgen
        // snippets/) and would otherwise need mkdir -p under OUT_DIR.
        let copied = format!("viewer_assets/{index}.bin");
        fs::create_dir_all(&assets_dir).unwrap();
        fs::copy(&path, out_dir.join(&copied)).unwrap();
        let mime = mime_for(&rel);
        code.push_str(&format!(
            "        \"{}\" => Some((\"{}\", include_bytes!(\"{}\"))),\n",
            rel, mime, copied
        ));
    }
    code.push_str("        _ => None,\n    }\n}\n");

    fs::write(out_dir.join("embedded_assets.rs"), code).unwrap();
}
