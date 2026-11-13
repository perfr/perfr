#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Runs install.sh against a made-up release served from file:// URLs: a full
# install, the refusals, and every truncated prefix of the script (what
# `curl | sh` executes when the transfer is cut). No network; everything
# happens in one temporary directory.

set -euo pipefail

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
INSTALL=$ROOT/tools/install/install.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
it() { printf '  %s\n' "$1"; }

case "$(uname -s)" in
  Linux) asset=perfr-linux-x86_64.tar.gz; agent=libperfr.so ;;
  Darwin) asset=perfr-macos-universal.tar.gz; agent=libperfr.dylib ;;
  *) echo "test_install.sh: install.sh does not run on $(uname -s); nothing to test"; exit 0 ;;
esac
if [ "$(uname -s)" = Linux ] && [ "$(uname -m)" != x86_64 ]; then
  echo "test_install.sh: no release asset for Linux $(uname -m); nothing to test"
  exit 0
fi
command -v curl >/dev/null 2>&1 || fail "the test serves the release over file://, which needs curl"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

SITE=$TMP/site
mkdir -p "$SITE" "$TMP/stage/perfr/include" "$TMP/stage/perfr/licenses"
for name in perfr perfr-hook "$agent"; do
  printf 'fake %s\n' "$name" >"$TMP/stage/perfr/$name"
done
printf '/* fake */\n' >"$TMP/stage/perfr/include/perfr.h"
printf 'notice\n' >"$TMP/stage/perfr/licenses/NOTICE"
tar -C "$TMP/stage" -czf "$SITE/$asset" perfr
printf '%s  %s\n' "$(sha256 "$SITE/$asset")" "$asset" >"$SITE/$asset.sha256"

# run_install <script> <prefix>: installs into <prefix>/bin with HOME there.
run_install() {
  local script=$1 prefix=$2
  mkdir -p "$prefix"
  status=0
  output=$(HOME="$prefix" PERFR_INSTALL_BASE="file://$SITE" PERFR_INSTALL_DIR="$prefix/bin" \
    sh <"$script" 2>&1) || status=$?
}

it "a release with a matching checksum is installed"
run_install "$INSTALL" "$TMP/full"
[ "$status" -eq 0 ] || fail "install exited $status"$'\n'"$output"
for name in perfr perfr-hook "$agent"; do
  [ -f "$TMP/full/bin/$name" ] || fail "missing $name"$'\n'"$output"
done
[ -x "$TMP/full/bin/perfr" ] || fail "perfr is not executable"
[ -f "$TMP/full/include/perfr.h" ] || fail "perfr.h is not in the sibling include directory"
[ -f "$TMP/full/bin/licenses/NOTICE" ] || fail "licenses were not installed"

it "a checksum that does not match installs nothing"
cp -R "$SITE" "$TMP/badsum"
printf '%064d  %s\n' 0 "$asset" >"$TMP/badsum/$asset.sha256"
status=0
output=$(HOME="$TMP/bad" PERFR_INSTALL_BASE="file://$TMP/badsum" PERFR_INSTALL_DIR="$TMP/bad/bin" \
  sh <"$INSTALL" 2>&1) || status=$?
[ "$status" -ne 0 ] || fail "a bad checksum was accepted"
case "$output" in *"checksum mismatch"*) ;; *) fail "no mismatch message"$'\n'"$output" ;; esac
[ ! -e "$TMP/bad/bin" ] || fail "something was installed despite the bad checksum"

it "a missing checksum installs nothing"
mkdir -p "$TMP/nosum"
cp "$SITE/$asset" "$TMP/nosum/"
status=0
output=$(HOME="$TMP/no" PERFR_INSTALL_BASE="file://$TMP/nosum" PERFR_INSTALL_DIR="$TMP/no/bin" \
  sh <"$INSTALL" 2>&1) || status=$?
[ "$status" -ne 0 ] || fail "an unverified archive was accepted"
[ ! -e "$TMP/no/bin" ] || fail "something was installed without a checksum"

it "an archive that is not there is reported as a missing release"
mkdir -p "$TMP/empty"
status=0
output=$(HOME="$TMP/none" PERFR_INSTALL_BASE="file://$TMP/empty" PERFR_INSTALL_DIR="$TMP/none/bin" \
  sh <"$INSTALL" 2>&1) || status=$?
[ "$status" -ne 0 ] || fail "an archive that does not exist was installed"
case "$output" in *"No Rust release"*) ;; *) fail "no missing-release message"$'\n'"$output" ;; esac
case "$output" in *"trying again"*) fail "a missing archive was tried again"$'\n'"$output" ;; esac

# A curl that fails its first FAKE_CURL_FAILS calls the way a reset connection
# does and is the real curl after that, and a sleep that does not wait.
REAL_CURL=$(command -v curl)
mkdir -p "$TMP/fakebin"
cat >"$TMP/fakebin/curl" <<'FAKE'
#!/bin/sh
n=$(cat "$FAKE_CURL_COUNT" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" >"$FAKE_CURL_COUNT"
if [ "$n" -le "$FAKE_CURL_FAILS" ]; then
  echo 'curl: (56) Recv failure: Connection reset by peer' >&2
  exit 56
fi
exec "$REAL_CURL" "$@"
FAKE
printf '#!/bin/sh\nexit 0\n' >"$TMP/fakebin/sleep"
chmod +x "$TMP/fakebin/curl" "$TMP/fakebin/sleep"

# run_flaky <fails> <prefix>: an install where curl breaks off <fails> times.
run_flaky() {
  local fails=$1 prefix=$2
  rm -f "$TMP/curl-count"
  status=0
  output=$(HOME="$prefix" PERFR_INSTALL_BASE="file://$SITE" PERFR_INSTALL_DIR="$prefix/bin" \
    PATH="$TMP/fakebin:$PATH" REAL_CURL="$REAL_CURL" \
    FAKE_CURL_COUNT="$TMP/curl-count" FAKE_CURL_FAILS="$fails" \
    sh <"$INSTALL" 2>&1) || status=$?
}

it "a download that breaks off is tried again"
run_flaky 2 "$TMP/flaky"
[ "$status" -eq 0 ] || fail "install exited $status after two broken transfers"$'\n'"$output"
case "$output" in *"trying again (3 of 3)"*) ;; *) fail "no retry message"$'\n'"$output" ;; esac
[ -x "$TMP/flaky/bin/perfr" ] || fail "perfr was not installed after the retries"

it "a download that keeps breaking off is not reported as a missing release"
run_flaky 3 "$TMP/broken"
[ "$status" -ne 0 ] || fail "three broken transfers were accepted"
[ "$(cat "$TMP/curl-count")" -eq 3 ] || fail "curl ran $(cat "$TMP/curl-count") times, not 3"
case "$output" in *"No Rust release"*) fail "a broken transfer was called a missing release"$'\n'"$output" ;; esac
case "$output" in *"Run the installer again"*) ;; *) fail "no advice to run it again"$'\n'"$output" ;; esac
[ ! -e "$TMP/broken/bin" ] || fail "something was installed from a broken transfer"

# `curl | sh` hands the shell whatever arrived. A script the shell runs
# line by line would, cut in the wrong place, install the service and stop
# before the library it must match.
it "a truncated script installs nothing, wherever it is cut"
lines=$(wc -l <"$INSTALL")
n=1
while [ "$n" -lt "$lines" ]; do
  head -n "$n" "$INSTALL" >"$TMP/cut.sh"
  run_install "$TMP/cut.sh" "$TMP/cut"
  if [ -e "$TMP/cut/bin" ]; then
    fail "the first $n of $lines lines installed: $(cd "$TMP/cut/bin" && echo *)"
  fi
  n=$((n + 1))
done

echo "ok"
