#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build the perfr-web encoder for the browser package. This is the same
# .perfr.stream framing as the live viewer (perfr-live-protocol), compiled
# to WASM. It does not rebuild the viewer.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"
OUT="$ROOT/pkg"

if ! rustup target list --installed | grep -q '^wasm32-unknown-unknown$'; then
  rustup target add wasm32-unknown-unknown
fi

if ! command -v wasm-bindgen >/dev/null || ! wasm-bindgen --version | grep -q '0.2.100'; then
  cargo install wasm-bindgen-cli --version 0.2.100 --locked
fi

cargo build \
  --manifest-path "$WORKSPACE/Cargo.toml" \
  -p perfr-web \
  --target wasm32-unknown-unknown \
  --profile wasm \
  --features bindgen

mkdir -p "$OUT"
wasm-bindgen \
  --target web \
  --out-dir "$OUT" \
  --out-name perfr_web \
  "$WORKSPACE/target/wasm32-unknown-unknown/wasm/perfr_web.wasm"

stamp() {
  local f="$1"
  if [[ ! -f "$f" ]]; then
    return
  fi
  if grep -q 'Copyright (c) [0-9]\{4\} The perfr Authors' "$f"; then
    return
  fi
  local tmp
  tmp="$(mktemp)"
  {
    echo "// Copyright (c) 2026 The perfr Authors. All rights reserved."
    echo "// Use of this source code is governed by a BSD-style license that can be"
    echo "// found in the LICENSE file."
    echo ""
    cat "$f"
  } > "$tmp"
  mv "$tmp" "$f"
}

stamp "$OUT/perfr_web.js"
echo "Wrote $OUT/perfr_web.js"
