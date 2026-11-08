#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../../.. && pwd)"
LIB="${1:-${PERFR_LIB:-$ROOT/target/debug/libperfr.so}}"
make test LIB="$LIB"
