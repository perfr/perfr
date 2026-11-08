// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stamps the perfr version, git hash, UTC build date and release tag.
//!
//! No vergen. `git` is optional: a source tarball still builds, and the
//! hash is `unknown`, so the version line says `dev+unknown`.
//! `SOURCE_DATE_EPOCH` wins over the clock so a reproducible build can pin
//! the date. `PERFR_RELEASE_TAG` (or `GITHUB_REF_NAME` when
//! `GITHUB_REF_TYPE=tag`) is kept only when it is a `perfr-v*` tag; otherwise
//! the build is local and the version label is `dev` plus the hash.
//! `build_wasm.sh` exports the same variables before it compiles the viewer.

use std::path::PathBuf;
use std::process::Command;

#[allow(dead_code)]
#[path = "src/stamp.rs"]
mod stamp;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/stamp.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=PERFR_GIT_HASH");
    println!("cargo:rerun-if-env-changed=PERFR_RELEASE_TAG");
    println!("cargo:rerun-if-env-changed=PERFR_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let perfr_manifest = manifest.join("../perfr/Cargo.toml");
    println!("cargo:rerun-if-changed={}", perfr_manifest.display());

    let git_dir = manifest.join("../../.git");
    let head = git_dir.join("HEAD");
    if head.is_file() {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(text) = std::fs::read_to_string(&head) {
            if let Some(rest) = text.trim().strip_prefix("ref:") {
                let ref_path = git_dir.join(rest.trim());
                if ref_path.is_file() {
                    println!("cargo:rerun-if-changed={}", ref_path.display());
                }
            }
        }
    }

    let version = package_version(&perfr_manifest);
    let hash = git_hash();
    let date = build_date();
    let tag = release_tag();
    println!("cargo:rustc-env=PERFR_PKG_VERSION={version}");
    println!("cargo:rustc-env=PERFR_GIT_HASH={hash}");
    println!("cargo:rustc-env=PERFR_BUILD_DATE={date}");
    println!("cargo:rustc-env=PERFR_RELEASE_TAG={tag}");
}

fn package_version(path: &std::path::Path) -> String {
    if let Some(v) = env_nonempty("PERFR_PKG_VERSION") {
        return v;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("version = \"") {
                if let Some(v) = rest.strip_suffix('"') {
                    if !v.is_empty() && !v.contains(['\n', '\r']) {
                        return v.to_string();
                    }
                }
            }
        }
    }
    println!("cargo:warning=perfr package version not found; stamping 0.0.0");
    "0.0.0".to_string()
}

fn git_hash() -> String {
    if let Some(hash) = env_nonempty("PERFR_GIT_HASH") {
        if stamp::is_commit_hash(&hash) {
            return hash;
        }
        println!("cargo:warning=ignoring PERFR_GIT_HASH={hash}: not a hex commit hash");
    }
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stamp::is_commit_hash(&text) {
                text
            } else {
                "unknown".to_string()
            }
        }
        _ => "unknown".to_string(),
    }
}

fn release_tag() -> String {
    if let Some(tag) = env_nonempty("PERFR_RELEASE_TAG") {
        if stamp::is_release_tag(&tag) {
            return tag;
        }
        println!("cargo:warning=ignoring PERFR_RELEASE_TAG={tag}: not a perfr-vX.Y.Z tag");
        return String::new();
    }
    if std::env::var("GITHUB_REF_TYPE").ok().as_deref() == Some("tag") {
        if let Some(tag) = env_nonempty("GITHUB_REF_NAME") {
            if stamp::is_release_tag(&tag) {
                return tag;
            }
        }
    }
    String::new()
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn build_date() -> String {
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH") {
        if let Ok(secs) = epoch.trim().parse::<i64>() {
            return stamp::format_unix_date(secs);
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    stamp::format_unix_date(secs)
}
