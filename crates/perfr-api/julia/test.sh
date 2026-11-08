#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"

echo "no library"
PERFR_LIB=/no/such/libperfr.so julia --project=. -e 'include("test/runtests.jl")'
if [[ -f "$LIB" ]]; then
  echo "library $LIB"
  PERFR_LIB="$LIB" julia --project=. -e 'include("test/runtests.jl")'
  PERFR_LIB="$LIB" julia --project=. samples/frame.jl
else
  echo "no libperfr at $LIB; skipped the capture test"
fi
