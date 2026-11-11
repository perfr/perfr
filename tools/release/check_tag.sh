#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Refuses a release tag that does not name the version stamped in the tree.
#
#   tools/release/check_tag.sh perfr-v0.1.3
#
# The binaries take their version from crates/perfr/Cargo.toml, not from the
# tag, and `perfr update` compares that version with the latest release tag.
# perfr-v0.1.2 was pushed by hand onto a tree still stamped 0.1.1: every
# install of it believed it was 0.1.1 and fetched 0.1.2 again on each update.
# release.sh cannot produce such a tag; the release workflows run this first
# so that a tag made any other way cannot publish one either.
#
# A second argument names another checkout to read (the tests use it).

set -euo pipefail

tag=${1:-}
root=${2:-$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)}
if [ -z "$tag" ]; then
  echo "usage: tools/release/check_tag.sh perfr-vX.Y.Z [checkout]" >&2
  exit 2
fi

manifest=$root/crates/perfr/Cargo.toml
stamped=$(sed -n 's/^version = "\(.*\)"/\1/p' "$manifest" | head -1)
if [ -z "$stamped" ]; then
  echo "error: no package version in $manifest" >&2
  exit 1
fi
if [ "$tag" != "perfr-v$stamped" ]; then
  echo "error: tag $tag does not match the stamped version $stamped (expected perfr-v$stamped)." >&2
  echo "Stamp the tree first; tools/release/release.sh does both. See docs/releasing.md." >&2
  exit 1
fi
echo "tag $tag matches the stamped version $stamped"
