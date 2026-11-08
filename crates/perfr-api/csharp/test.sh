#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Build and test the C# binding. Two processes: one with PERFR_LIB pointed
# at a missing file, one at a real libperfr when ROOT has been built.
set -euo pipefail

cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"

dotnet build -c Release --nologo
echo "no library"
PERFR_LIB=/no/such/libperfr.so dotnet test -c Release --nologo --no-build

if [[ -f "$LIB" ]]; then
  echo "library $LIB"
  PERFR_LIB="$LIB" dotnet test -c Release --nologo --no-build
  PERFR_LIB="$LIB" dotnet run -c Release --no-build --project samples/PerfrSample -- --seconds 1
else
  echo "no libperfr at $LIB; skipped the capture test and the sample"
fi
