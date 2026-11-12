#!/bin/sh
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Upload symbol-store directories to the Perfr R2/S3 bucket.
#
# This step is meant to be skipped. With no bucket, no credentials or no
# aws CLI it prints why and exits 0. The release already carries the same tree as
# perfr-<version>-<target>-symstore.tar.gz.
#
#   PERFR_SYMBOL_STORE_BUCKET            bucket name (required to upload)
#   PERFR_SYMBOL_STORE_ACCESS_KEY_ID     or AWS_ACCESS_KEY_ID
#   PERFR_SYMBOL_STORE_SECRET_ACCESS_KEY or AWS_SECRET_ACCESS_KEY
#   PERFR_SYMBOL_STORE_ENDPOINT          R2: https://<account>.r2.cloudflarestorage.com
#   PERFR_SYMBOL_STORE_REGION            default: auto
#   PERFR_SYMBOL_STORE_PREFIX            key prefix inside the bucket
#
# Arguments are directories in the symbol-store layout (buildid/... and
# <name>/<GUIDAGE>/...). They are synced as-is, so build-ids from Linux and
# SymSrv keys from Windows land next to each other.

set -eu

bucket="${PERFR_SYMBOL_STORE_BUCKET:-}"
key="${PERFR_SYMBOL_STORE_ACCESS_KEY_ID:-${AWS_ACCESS_KEY_ID:-}}"
secret="${PERFR_SYMBOL_STORE_SECRET_ACCESS_KEY:-${AWS_SECRET_ACCESS_KEY:-}}"

if [ -z "$bucket" ] || [ -z "$key" ] || [ -z "$secret" ]; then
    cat <<'EOF'
symbol store upload: skipped.
PERFR_SYMBOL_STORE_BUCKET or the access key is not set.
The symbol-store layout is attached to this release as
perfr-<version>-<target>-symstore.tar.gz. To upload later, unpack those
archives and sync them to the bucket. docs/releasing.md names the variables.
EOF
    exit 0
fi

if [ "$#" -lt 1 ]; then
    echo "symbol store upload: no directories were passed" >&2
    exit 2
fi

export AWS_ACCESS_KEY_ID="$key"
export AWS_SECRET_ACCESS_KEY="$secret"
export AWS_DEFAULT_REGION="${PERFR_SYMBOL_STORE_REGION:-auto}"

# No aws CLI is a skip too. This used to pip-install whatever awscli was
# newest, in the one job that holds the bucket keys and the token that
# publishes the release; an unpinned package from the network does not
# belong there. GitHub's Ubuntu runners ship the CLI.
if ! command -v aws >/dev/null 2>&1; then
    message="symbol store upload: skipped, there is no aws CLI on this machine. The symstore archives are still release assets; unpack and sync them to upload later."
    if [ -n "${GITHUB_ACTIONS:-}" ]; then
        echo "::warning title=Symbol store upload skipped::${message}"
    else
        echo "$message" >&2
    fi
    exit 0
fi

prefix="${PERFR_SYMBOL_STORE_PREFIX:-}"
prefix="${prefix#/}"
prefix="${prefix%/}"
dest="s3://${bucket}"
if [ -n "$prefix" ]; then
    dest="${dest}/${prefix}"
fi

for dir do
    if [ ! -d "$dir" ]; then
        echo "symbol store upload: $dir is not a directory" >&2
        exit 2
    fi
    if [ -n "${PERFR_SYMBOL_STORE_ENDPOINT:-}" ]; then
        aws s3 sync "$dir" "$dest" --endpoint-url "$PERFR_SYMBOL_STORE_ENDPOINT" --only-show-errors
    else
        aws s3 sync "$dir" "$dest" --only-show-errors
    fi
done

echo "symbol store upload: synced to ${dest}"
