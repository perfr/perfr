#!/bin/bash
#
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail
cd "$(dirname "$0")"
if ! command -v mojo >/dev/null 2>&1; then
  echo "mojo is not installed; the Mojo package was not compiled or run"
  exit 0
fi
echo "mojo $(mojo --version 2>/dev/null || true)"
mojo build sample.mojo
