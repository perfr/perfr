// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The viewer pack embedded in the binary: its files, their ETags, and the
//! isolation headers every response carries for it.

use axum::http::{header, header::HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/embedded_assets.rs"));
}

pub(super) async fn isolation_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    apply_isolation(next.run(req).await)
}

pub(super) async fn index(headers: axum::http::HeaderMap) -> Response {
    asset_response("index.html", &headers).unwrap_or_else(|| {
        apply_isolation(
            Html("<!doctype html><title>perfr Live</title><p>viewer-dist/index.html missing</p>")
                .into_response(),
        )
    })
}

pub(super) async fn static_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    // Browsers ask for /favicon.ico whatever the page links; the icon is
    // a PNG, and this is the same file.
    let path = if path == "favicon.ico" { "favicon.png" } else { path.as_str() };
    asset_response(path, &headers).unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

/// COOP/COEP so the wasm-bindgen-rayon pool can use SharedArrayBuffer.
/// CORP lets same-origin assets (HTML, js, wasm, and
/// `snippets/wasm-bindgen-rayon-*/…/workerHelpers.no-bundler.js`) load
/// under `require-corp`. Applied to every response, not only viewer-dist.
pub(super) const ISOLATION: [(&'static str, &'static str); 3] = [
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-embedder-policy", "require-corp"),
    ("cross-origin-resource-policy", "same-origin"),
];

pub(super) fn apply_isolation(mut resp: Response) -> Response {
    for (k, v) in ISOLATION {
        if let (Ok(name), Ok(val)) = (
            header::HeaderName::from_bytes(k.as_bytes()),
            header::HeaderValue::from_str(v),
        ) {
            resp.headers_mut().insert(name, val);
        }
    }
    resp
}

/// An embedded viewer asset. Served with `Cache-Control: no-cache` and an
/// ETag of its bytes: the browser keeps its copy but asks every time, and
/// a service restarted with a new pack answers with the new bytes rather
/// than a tab quietly running the old viewer until a hard refresh. Same
/// bytes, and the answer is a 304.
pub(super) fn asset_response(path: &str, headers: &axum::http::HeaderMap) -> Option<Response> {
    let (mime, data) = assets::get(path)?;
    let etag = format!("\"{:016x}\"", asset_etag(data));
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    let mut resp = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, mime)], data).into_response()
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    Some(apply_isolation(resp))
}

/// The hash of an embedded asset, computed the first time it is asked for.
/// The assets are compiled in and never change, the wasm is ten megabytes,
/// and a reload of the page asks about each of them: hashed per request,
/// every "is my copy current?" cost a pass over the file to answer 304.
pub(super) fn asset_etag(data: &'static [u8]) -> u64 {
    static HASHES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<(usize, usize), u64>>> =
        std::sync::OnceLock::new();
    let mut hashes = HASHES.get_or_init(Default::default).lock();
    // By address and length: each asset is one static.
    *hashes.entry((data.as_ptr() as usize, data.len())).or_insert_with(|| asset_hash(data))
}

/// FNV-1a over the asset, for its ETag.
pub(super) fn asset_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
