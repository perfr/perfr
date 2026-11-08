#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# `zig build test` caches the run step and ignores PERFR_LIB, so the
# assertions go through `zig test`, which executes every time.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"
ZIG="${ZIG:-zig}"

echo "no library"
PERFR_LIB=/no/such/libperfr.so "$ZIG" test src/perfr.zig -lc
if [[ -f "$LIB" ]]; then
  echo "library $LIB"
  PERFR_LIB="$LIB" "$ZIG" test src/perfr.zig -lc
  PERFR_LIB="$LIB" "$ZIG" build run
else
  echo "no libperfr at $LIB; skipped the capture test"
fi
