#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the Emscripten example when emcc is on PATH. The JS shim forwards
# perfr_web_* to Module.perfr (@perfr/web). Without emcc this script exits 0
# after saying so: the C ABI itself is covered by the perfr-web crate tests.
set -euo pipefail
cd "$(dirname "$0")"

if ! command -v emcc >/dev/null 2>&1; then
  echo "emcc not installed; skipping the Emscripten link. See main.c."
  exit 0
fi

emcc main.c -include perfr_web_js.h -o perfr_web_example.html -s WASM=1
echo "Wrote perfr_web_example.html. Set Module.perfr to the @perfr/web profiler."
