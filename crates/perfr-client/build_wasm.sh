#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the client's pack: the viewer's interface with the capture service
# behind it, which is the page the service embeds and serves. Written to
# viewer-dist/ beside this script, and committed.
#
# The recipe is the viewer's (crates/perfr-viewer/wasm_pack.sh), so this pack
# and the viewer's own are built alike.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$ROOT/../.." && pwd)"

"$WORKSPACE/crates/perfr-viewer/wasm_pack.sh" "$ROOT" perfr_client "$ROOT/viewer-dist"
echo "Rebuild perfr so perfr-live-server's build script embeds the pack."

if [[ -z "${PERFR_GIT_HASH:-}" ]]; then
  PERFR_GIT_HASH="$(git -C "$WORKSPACE" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
fi
if [[ -z "${PERFR_RELEASE_TAG:-}" && "${GITHUB_REF_TYPE:-}" == "tag" ]]; then
  PERFR_RELEASE_TAG="${GITHUB_REF_NAME}"
fi

# What this pack was built from, for anything that serves it. `source` is
# source_id.py over the viewer's sources as they are now: perfr-web's
# build_site.py recomputes it over the checkout it deploys and refuses a
# pack that does not match, so a pack built on a branch cannot go live after
# a viewer change merged behind it. Rebuild on main after merging.
SOURCE_ID="$(python3 "$ROOT/source_id.py" "$WORKSPACE")"
{
  echo "hash=${PERFR_GIT_HASH}"
  echo "tag=${PERFR_RELEASE_TAG:-dev}"
  echo "date=$(date -u -d "@${SOURCE_DATE_EPOCH:-$(date +%s)}" +%Y-%m-%d)"
  echo "source=${SOURCE_ID}"
} > "$ROOT/viewer-dist/VERSION"
echo "viewer-dist/VERSION: $(tr '\n' ' ' < "$ROOT/viewer-dist/VERSION")"
