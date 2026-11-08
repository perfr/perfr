#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"

make
echo "no library"
PERFR_LIB=/no/such/libperfr.so lua5.4 test/test_perfr.lua
PERFR_LIB=/no/such/libperfr.so luajit test/test_perfr.lua

if [[ -f "$LIB" ]]; then
  echo "library $LIB"
  PERFR_LIB="$LIB" lua5.4 test/test_perfr.lua
  PERFR_LIB="$LIB" luajit test/test_perfr.lua
  PERFR_LIB="$LIB" lua5.4 samples/sample.lua 1
  PERFR_LIB="$LIB" luajit samples/sample.lua 1
else
  echo "no libperfr at $LIB; skipped the capture test"
fi
