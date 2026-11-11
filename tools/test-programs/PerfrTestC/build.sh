#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds PerfrTestC with perfr.h alone: nothing is linked. At run time the
# header loads libperfr.so by the search order in perfr.h; in the source
# tree point PERFR_LIB at target/release/libperfr.so (the e2e
# harness does). PERFR_STATIC=1 ./build.sh links libperfr_api.a instead, the
# shape a musl or otherwise dlopen-less program uses.
set -euo pipefail
cd "$(dirname "$0")"
ROOT=$(git -C . rev-parse --show-toplevel)
INCLUDE="$ROOT/crates/perfr-api/include"
if [[ ${PERFR_STATIC:-0} == 1 ]]; then
  # The static build is the only one that needs a library at build time, and
  # the one it links is perfr-api's archive. The guard used to look for
  # libperfr.so, which is perfr-lib's output: with that present and the
  # archive absent it built nothing and the link failed.
  [ -f "$ROOT/target/release/libperfr_api.a" ] || (cd "$ROOT" && cargo build --release -p perfr-api)
  NATIVE_LIBS=(-lpthread -ldl -lm)
  if [[ $(uname -s) == Darwin ]]; then NATIVE_LIBS=(-lSystem -liconv); fi
  ${CC:-cc} -O2 -g -std=c11 -Wall -Wextra -DPERFR_STATIC -I"$INCLUDE" \
    -o PerfrTestC PerfrTestC.c "$ROOT/target/release/libperfr_api.a" "${NATIVE_LIBS[@]}"
  echo "built $(pwd)/PerfrTestC (static, libperfr_api.a linked in)"
else
  # dlopen is in libSystem on macOS. Keep the array nonempty for Bash 3.2
  # with nounset: expanding an empty array there is an unbound-variable error.
  DL=(-ldl); if [[ $(uname -s) == Darwin ]]; then DL=(-lSystem); fi
  ${CC:-cc} -O2 -g -std=c11 -Wall -Wextra -I"$INCLUDE" -o PerfrTestC PerfrTestC.c -lpthread "${DL[@]}"
  echo "built $(pwd)/PerfrTestC (header-only; loads libperfr at run time)"
fi
