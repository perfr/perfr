#!/bin/bash
#
# Copyright (c) 2022 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Fail on any error.
set -euo pipefail

cd $GITHUB_WORKSPACE
# Use origin/main as reference branch, if not specified by Github
REFERENCE="origin/${GITHUB_BASE_REF:-main}"
MERGE_BASE="$(git merge-base $REFERENCE HEAD)" # Merge base is the commit on main this PR was branched from.
LICENSE_HEADER_MISSED=""
# The built viewer pack: wasm-bindgen's glue and the snippets it copies out of
# crates, one of which (wasm-bindgen-rayon's worker helper) is Google's, under
# Apache-2.0. Requiring our header on these made the pack build write
# "Copyright The perfr Authors" above another holder's notice.
GENERATED='^crates/perfr-client/viewer-dist/(perfr_client|snippets/)'

echo -e "The following files are missing a license/copyright header:\n"
while read file; do
  if [ ! -f "$file" ]; then
    continue
  fi

  if ! grep -E 'Copyright \(c\) [0-9]{4} The perfr Authors\. All rights reserved\.' "$file" > /dev/null; then
    echo "$file"
    # An annotation on the file's first line, printed by this job. It used
    # to take a second, privileged workflow that downloaded a list this job
    # uploaded; a workflow command needs neither a token nor an artifact.
    # %, comma and colon are the characters a property value must escape.
    escaped=${file//%/%25}
    escaped=${escaped//,/%2C}
    escaped=${escaped//:/%3A}
    echo "::error file=${escaped},line=1,title=Missing license header::The source file must start with a license header."
    LICENSE_HEADER_MISSED=1
  fi

done <<< $(git diff -U0 --no-color --relative --name-only --diff-filter=r $MERGE_BASE \
| egrep '\.rs$|\.cpp$|\.c$|\.h$|\.js$|\.py$|\.sh$|\.ps1$' \
| egrep -v "$GENERATED")

if [ -n "$LICENSE_HEADER_MISSED" ]; then
  exit 1
fi
