#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds a wasm pack of the viewer's interface: the viewer's own
# (build_wasm.sh beside this file) or the client's
# (crates/perfr-client/build_wasm.sh), which is the same interface with the
# capture service behind it. One recipe, so the two packs are built alike.
#
#   wasm_pack.sh <crate directory> <module name> <output directory> [profile [wasm-bindgen arguments]]
#
# Compiles the crate for wasm32 with the pinned nightly, a rebuilt std and
# `--features wasm-threads`, runs wasm-bindgen into the output directory as
# <module name>.js and <module name>_bg.wasm, and stamps the generated
# JavaScript. The caller adds its page and its VERSION. The profile is
# `wasm` unless one is named: the viewer's pack names `wasm-viewer`, and
# passes wasm-bindgen the flag that leaves the function names out.

set -euo pipefail
if [[ $# -lt 3 ]]; then
  echo "usage: $0 <crate directory> <module name> <output directory> [profile [wasm-bindgen arguments]]" >&2
  exit 2
fi
CRATE="$(cd "$1" && pwd)"
MODULE="$2"
OUT="$3"
PROFILE="${4:-wasm}"
BINDGEN_ARGS=("${@:5}")
WORKSPACE="$(cd "$(dirname "$0")/../.." && pwd)"

if ! command -v rustup >/dev/null; then
  echo "rustup is required to add wasm32-unknown-unknown" >&2
  exit 1
fi

# wasm-bindgen-rayon needs a rebuilt std with atomics (not the rustc 1.88
# sysroot). Pin matches the crate's tested nightly. rust-toolchain.toml
# stays 1.88 for native cargo test.
NIGHTLY="${PERFR_WASM_NIGHTLY:-nightly-2025-11-15}"
rustup toolchain install "$NIGHTLY" --profile minimal --component rust-src
rustup target add wasm32-unknown-unknown --toolchain "$NIGHTLY"

BINDGEN_VER="0.2.100"
if ! command -v wasm-bindgen >/dev/null || ! wasm-bindgen --version | grep -q "$BINDGEN_VER"; then
  echo "Installing wasm-bindgen-cli ${BINDGEN_VER}"
  cargo install wasm-bindgen-cli --version "$BINDGEN_VER" --locked
fi

# Atomics + shared memory so SharedArrayBuffer / rayon workers work.
# +mutable-globals is required by older rustc; harmless on 1.87+.
# --import-memory lets wasm-bindgen share one SAB across workers.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+atomics,+bulk-memory,+mutable-globals \
  -C link-arg=--shared-memory -C link-arg=--max-memory=2147483648 \
  -C link-arg=--import-memory \
  -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size \
  -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base"
export CARGO_PROFILE_RELEASE_PANIC=abort
# Same stamp as `perfr --version` (crates/perfr-build-info). A release sets
# PERFR_RELEASE_TAG=perfr-vX.Y.Z (the release workflow does). With no tag the
# About window and `perfr --version` say `dev` plus the short hash.
# SOURCE_DATE_EPOCH, when set, pins the UTC build date.
if [[ -z "${PERFR_GIT_HASH:-}" ]]; then
  PERFR_GIT_HASH="$(git -C "$WORKSPACE" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
fi
export PERFR_GIT_HASH
if [[ -z "${PERFR_RELEASE_TAG:-}" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
  PERFR_RELEASE_TAG="${GITHUB_REF_NAME}"
fi
if [[ -n "${PERFR_RELEASE_TAG:-}" ]]; then
  export PERFR_RELEASE_TAG
fi
echo "pack stamp: hash=${PERFR_GIT_HASH} tag=${PERFR_RELEASE_TAG:-dev} date=${SOURCE_DATE_EPOCH:-clock}"

echo "Building $MODULE with ${NIGHTLY} -Z build-std + --features wasm-threads (profile $PROFILE)"
rustup run "$NIGHTLY" cargo build \
  --manifest-path "$CRATE/Cargo.toml" \
  --target wasm32-unknown-unknown \
  --profile "$PROFILE" \
  --features wasm-threads \
  -Z build-std=panic_abort,std

# The snippet directories are named after a hash of the crate that owns
# them. When that hash changes wasm-bindgen writes a new directory and leaves
# the old one, which whatever serves the pack would go on serving.
mkdir -p "$OUT"
rm -rf "$OUT/snippets"

wasm-bindgen \
  --target web \
  --out-dir "$OUT" \
  --out-name "$MODULE" \
  "${BINDGEN_ARGS[@]}" \
  "$WORKSPACE/target/wasm32-unknown-unknown/$PROFILE/$MODULE.wasm"

# wasm-bindgen overwrites the glue. CI requires the perfr header on *.js.
stamp_perfr_js_header() {
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

stamp_perfr_js_header "$OUT/$MODULE.js"
while IFS= read -r -d '' f; do
  stamp_perfr_js_header "$f"
  # wasm-bindgen-rayon's worker helper still calls the module's init with
  # positional arguments, which the generated `__wbg_init` warns about on
  # every worker start (one line per pool thread in the console). The
  # object form is what it wants.
  sed -i 's/await pkg\.default(data\.module, data\.memory);/await pkg.default({ module_or_path: data.module, memory: data.memory });/' "$f"
done < <(find "$OUT/snippets" -name '*.js' -print0 2>/dev/null || true)

echo "Wrote $OUT/$MODULE.js and .wasm"
if [[ -d "$OUT/snippets" ]]; then
  echo "Worker snippets: $OUT/snippets"
fi
