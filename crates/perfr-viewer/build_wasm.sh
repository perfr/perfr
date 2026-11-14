#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the viewer's pack: the page that shows a capture, with no capture
# service behind it. It opens a `.perfr.stream` named in its address
# (`?capture=<url>`, with `&story=<url>` to play a story over it) or one the
# reader opens or drops on it, and Chrome and Perfetto traces the same way.
# This is the pack to host on a website or put in an iframe.
#
# Written to viewer-dist/ beside this script (gitignored): the wasm and its
# glue, the page (web/index.html), the icon and the demo story.
#
# The recipe is wasm_pack.sh, shared with the client's pack
# (crates/perfr-client/build_wasm.sh): the same interface, built the same
# way. With the isolation headers the page uses a pool of workers; without
# them, as in an iframe on someone else's site, it draws on one thread.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"
OUT="$ROOT/viewer-dist"

rm -rf "$OUT"
"$ROOT/wasm_pack.sh" "$ROOT" perfr_viewer "$OUT"
cp -r "$ROOT/web/." "$OUT/"

if [[ -z "${PERFR_GIT_HASH:-}" ]]; then
  PERFR_GIT_HASH="$(git -C "$WORKSPACE" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
fi
if [[ -z "${PERFR_RELEASE_TAG:-}" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
  PERFR_RELEASE_TAG="${GITHUB_REF_NAME}"
fi
# What this pack was built from, for anything that serves it.
{
  echo "hash=${PERFR_GIT_HASH}"
  echo "tag=${PERFR_RELEASE_TAG:-dev}"
  echo "date=$(date -u -d "@${SOURCE_DATE_EPOCH:-$(date +%s)}" +%Y-%m-%d)"
} > "$OUT/VERSION"
echo "viewer-dist/VERSION: $(tr '\n' ' ' < "$OUT/VERSION")"
ls -lh "$OUT/perfr_viewer_bg.wasm"
