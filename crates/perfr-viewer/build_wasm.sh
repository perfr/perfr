#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# The viewer's wasm pack: a page that opens one .perfr.stream and draws it
# (the WebGPU timeline and the stream parser, no service client). Unlike
# crates/perfr-client/build_wasm.sh this does not enable wasm threads
# (no nightly, no SharedArrayBuffer), so a blog page can embed the pack
# without cross-origin isolation.
#
# Profile `wasm-viewer` (workspace Cargo.toml): opt-level z, fat LTO, one
# codegen unit, symbol names stripped. The client stays on profile `wasm`.
# wasm-opt is not run: on this profile it shrinks the raw module and makes
# gzip and brotli larger.
#
#   ./crates/perfr-viewer/build_wasm.sh [cargo build arguments]
#
# The default pack draws with WebGPU only and carries a Latin subset of Hack.
# A browser without a WebGPU adapter gets a sentence saying so. Arguments go
# to `cargo build`, which is how the two larger variants are asked for:
#
#   --features webgl        link the GLES backend: the page then falls back to
#                           WebGL without WebGPU, and `?webgl` asks for it
#   --features full-fonts   egui's default fonts, for names outside Latin-1
#
# Writes crates/perfr-viewer/viewer-dist/ (gitignored). The service
# embeds the client from perfr-client/viewer-dist, not this pack.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

if ! command -v rustup >/dev/null; then
  echo "rustup is required to add wasm32-unknown-unknown" >&2
  exit 1
fi

rustup target add wasm32-unknown-unknown

BINDGEN_VER="0.2.100"
if ! command -v wasm-bindgen >/dev/null || ! wasm-bindgen --version | grep -q "$BINDGEN_VER"; then
  echo "Installing wasm-bindgen-cli ${BINDGEN_VER}"
  cargo install wasm-bindgen-cli --version "$BINDGEN_VER" --locked
fi

WORKSPACE="$(cd "$ROOT/../.." && pwd)"
# Do not inherit the client's atomics / shared-memory RUSTFLAGS.
unset RUSTFLAGS || true

echo "Building perfr-viewer (profile wasm-viewer, no wasm-threads) $*"
cargo build \
  --manifest-path "$WORKSPACE/Cargo.toml" \
  --target wasm32-unknown-unknown \
  --profile wasm-viewer \
  -p perfr-viewer \
  "$@"

OUT="$ROOT/viewer-dist"
rm -rf "$OUT"
mkdir -p "$OUT"

wasm-bindgen \
  --target web \
  --out-dir "$OUT" \
  --out-name perfr_viewer \
  "$WORKSPACE/target/wasm32-unknown-unknown/wasm-viewer/perfr_viewer.wasm"

cp "$ROOT/viewer.html" "$OUT/index.html"

echo "Wrote $OUT/perfr_viewer.js, perfr_viewer_bg.wasm, and index.html"
ls -lh "$OUT/perfr_viewer_bg.wasm"
