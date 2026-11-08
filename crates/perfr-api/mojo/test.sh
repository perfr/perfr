#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build the sample and the test, then run the test with a missing library
# and, when one is built, with the real one. `mojo` comes from PATH; CI
# installs it with pip.
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"
OUT="${OUT:-$PWD/build}"

if ! command -v mojo >/dev/null 2>&1; then
  echo "mojo is not installed; the Mojo package was not compiled or run" >&2
  exit 1
fi
echo "mojo $(mojo --version 2>/dev/null || true)"
mkdir -p "$OUT"
mojo build sample.mojo -o "$OUT/sample"
mojo build -I . test/test_perfr.mojo -o "$OUT/test_perfr"

echo "no library"
PERFR_LIB=/no/such/libperfr.so "$OUT/test_perfr"
if [[ -f "$LIB" ]]; then
  echo "library $LIB"
  PERFR_LIB="$LIB" "$OUT/test_perfr"
  PERFR_LIB="$LIB" "$OUT/sample"
else
  echo "no libperfr at $LIB; skipped the library run"
fi
