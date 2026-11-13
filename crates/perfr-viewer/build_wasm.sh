#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Slim viewer-only wasm: the WebGPU timeline and the .perfr.stream parser.
# Unlike crates/perfr-live-viewer/build_wasm.sh this does not enable wasm
# threads (no nightly, no SharedArrayBuffer), so a blog page can embed the
# pack without cross-origin isolation.
#
# Profile `wasm-embed` (workspace Cargo.toml): opt-level z, fat LTO, one
# codegen unit, symbol names stripped. The full page stays on profile `wasm`.
# wasm-opt is not run: on this profile it shrinks the raw module and makes
# gzip and brotli larger. WebGL is off (`--features webgl` puts it back).
# The font is a Latin subset of Hack (`slim-fonts`, on by default).
#
#   ./crates/perfr-viewer/build_wasm.sh
#
# Writes crates/perfr-viewer/viewer-dist/ (gitignored). The service embeds
# the full page from perfr-live-viewer/viewer-dist, not this pack.

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
# Do not inherit the full viewer's atomics / shared-memory RUSTFLAGS.
unset RUSTFLAGS || true

echo "Building perfr-viewer (profile wasm-embed, no wasm-threads, no webgl)"
cargo build \
  --manifest-path "$WORKSPACE/Cargo.toml" \
  --target wasm32-unknown-unknown \
  --profile wasm-embed \
  -p perfr-viewer

OUT="$ROOT/viewer-dist"
rm -rf "$OUT"
mkdir -p "$OUT"

wasm-bindgen \
  --target web \
  --out-dir "$OUT" \
  --out-name perfr_viewer \
  "$WORKSPACE/target/wasm32-unknown-unknown/wasm-embed/perfr_viewer.wasm"

cp "$ROOT/embed.html" "$OUT/index.html"

echo "Wrote $OUT/perfr_viewer.js, perfr_viewer_bg.wasm, and index.html"
ls -lh "$OUT/perfr_viewer_bg.wasm"
