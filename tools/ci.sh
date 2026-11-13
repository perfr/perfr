#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# The Linux checks every change should pass, as one script. The `ci`
# workflow calls it step by step; a developer runs the same thing when
# Actions is not available. No network beyond what cargo needs for crates
# it has not fetched yet, no privileges, nothing written outside the cargo
# target directory.
#
#   tools/ci.sh                every step, in the order below
#   tools/ci.sh test wasm      only the named steps
#
#   fmt      cargo fmt --check over the workspace            (report-only)
#   clippy   cargo clippy over the workspace, all targets, no warnings
#   test     cargo test over the workspace, under a memory cap
#   musl     the static release build of the service, as shipped
#   wasm     the full viewer page and the slim viewer core, compiled for wasm32
#   docs     crates/README.md names every crate directory, and the Unreal
#            plugin's copy of perfr.h is the header it was copied from
#
# clippy fails on any warning: the tree is clean under it, and the lints the
# project has decided against are switched off where that was decided
# ([workspace.lints] in Cargo.toml, or an `allow` with its reason on the
# item). fmt counts what it finds and passes anyway: the tree is not clean
# under it yet, and a gate that starts red only teaches people to ignore
# it. PERFR_CI_STRICT=1 turns it into a failure; make that the default in
# the commit that brings the count to zero.
#
# The test step runs every crate, perfr-hook included: without the Frida
# devkits its build script links a stub (crates/perfr-frida-build), and the
# hook's own tests do not inject.
#
# PERFR_CI_MEMORY_MAX (default 24G) caps the test run through a systemd user
# scope. A test here has sampled itself into 70 GB before; the cap makes the
# next one of those a failed step instead of a dead machine. Where no user
# manager is reachable (a container, most CI runners) the tests run uncapped
# and the script says so.

# The step functions are called by name ("step_$step"), which shellcheck
# cannot see.
# shellcheck disable=SC2329

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
command -v cargo >/dev/null 2>&1 || export PATH="$HOME/.cargo/bin:$PATH"

ALL_STEPS=(fmt clippy test musl wasm docs)
MUSL_TARGET=x86_64-unknown-linux-musl
TARGET_DIR=${CARGO_TARGET_DIR:-target}
STRICT=${PERFR_CI_STRICT:-0}
MEMORY_MAX=${PERFR_CI_MEMORY_MAX:-24G}

log=$(mktemp "${TMPDIR:-/tmp}/perfr-ci.XXXXXX")
trap 'rm -f "$log"' EXIT

note() { printf '\n== %s\n' "$*"; }

# Runs the command inside a memory-capped scope when systemd can make one.
capped() {
  if command -v systemd-run >/dev/null 2>&1 \
      && systemd-run --user --scope -q true >/dev/null 2>&1; then
    systemd-run --user --scope -q -p "MemoryMax=$MEMORY_MAX" -p MemorySwapMax=0 -- "$@"
  else
    echo "ci.sh: no systemd user scope available, running without a memory cap" >&2
    "$@"
  fi
}

# Prints "<what>: <count>". A count that gates (third argument 1) fails the
# step when it is not zero; a report-only one fails only when strict.
report() {
  local count=$1 what=$2 gates=${3:-$STRICT}
  if [ "$count" -eq 0 ]; then
    echo "$what: none"
    return 0
  fi
  echo "$what: $count"
  if [ "$gates" = 1 ]; then
    return 1
  fi
  echo "(report-only; PERFR_CI_STRICT=1 makes this a failure)"
  return 0
}

step_fmt() {
  # -l prints one path per file that would change; the exit status is 1 then.
  cargo fmt --all -- --check -l >"$log" 2>&1
  local status=$?
  if [ "$status" -ne 0 ] && ! grep -q '\.rs$' "$log"; then
    cat "$log"
    echo "cargo fmt did not run (exit $status)"
    return 1
  fi
  sed "s#^$PWD/##" "$log"
  report "$(grep -c '\.rs$' "$log")" "files rustfmt would change"
}

step_clippy() {
  # --keep-going: a deny-level lint in one crate must not hide the others.
  cargo clippy --locked --workspace --all-targets --keep-going --message-format=short \
    2>&1 | tee "$log"
  local status=${PIPESTATUS[0]}
  # One line per diagnostic in the short format: "path:line:col: level: text".
  # The lib and its test build repeat each other; count a location once.
  local errors warnings
  errors=$(grep -E '^[^ ]+:[0-9]+:[0-9]+: error' "$log" | sort -u | wc -l)
  warnings=$(grep -E '^[^ ]+:[0-9]+:[0-9]+: warning' "$log" | sort -u | wc -l)
  if [ "$status" -ne 0 ] && [ "$errors" -eq 0 ]; then
    echo "cargo clippy failed without a lint error (exit $status): a crate does not compile"
    return 1
  fi
  local failed=0
  report "$errors" "clippy errors" 1 || failed=1
  report "$warnings" "clippy warnings" 1 || failed=1
  return "$failed"
}

step_test() {
  # Compile outside the cap: it is there for a runaway test, not for rustc.
  cargo test --locked --workspace --no-run || return 1
  capped cargo test --locked --workspace
}

step_musl() {
  cargo build --locked --release --target "$MUSL_TARGET" -p perfr || return 1
  local bin="$TARGET_DIR/$MUSL_TARGET/release/perfr"
  # The same assertion the release makes: rustc 1.88's musl target emits a
  # static PIE, which file(1) reports as "static-pie linked".
  if ! file "$bin" | grep -qE 'static(ally|-pie) linked'; then
    file "$bin"
    echo "$bin is not statically linked"
    return 1
  fi
  echo "static: $bin ($(du -h "$bin" | cut -f1))"
}

step_wasm() {
  cargo check --locked -p perfr-live-viewer --target wasm32-unknown-unknown || return 1
  # The embeddable viewer: renderer and stream parser, not the full page.
  cargo check --locked -p perfr-viewer --target wasm32-unknown-unknown || return 1
  python3 crates/perfr-viewer/check_boundary.py
}

step_docs() {
  # crates/README.md is the map contributors are sent to. It once described
  # sixteen fewer crates than this directory held.
  local missing=0 dir name
  for dir in crates/*/; do
    name=$(basename "$dir")
    if ! grep -qF "\`$name\`" crates/README.md; then
      echo "crates/README.md does not mention \`$name\`"
      missing=1
    fi
  done
  [ "$missing" -eq 0 ] && echo "crates/README.md names every crate"
  # The Unreal plugin carries its own perfr.h so it can be dropped into a
  # project as is. Nothing else notices when the two drift apart.
  local header=crates/perfr-api/include/perfr.h
  local copy=integrations/unreal/PerfrProfiler/Source/PerfrProfiler/Public/perfr.h
  if cmp -s "$header" "$copy"; then
    echo "the Unreal plugin's perfr.h matches $header"
  else
    echo "$copy differs from $header: run integrations/unreal/sync-perfr-header.sh"
    missing=1
  fi
  return "$missing"
}

steps=("$@")
[ "${#steps[@]}" -gt 0 ] || steps=("${ALL_STEPS[@]}")
for step in "${steps[@]}"; do
  case " ${ALL_STEPS[*]} " in
    *" $step "*) ;;
    *) echo "ci.sh: unknown step '$step' (steps: ${ALL_STEPS[*]})" >&2; exit 2 ;;
  esac
done

# Every requested step runs even after a failure, so one run reports all of
# them; the exit status is the worst.
results=()
failed=0
for step in "${steps[@]}"; do
  note "$step"
  started=$SECONDS
  if "step_$step"; then
    results+=("ok    $step ($((SECONDS - started)) s)")
  else
    results+=("FAIL  $step ($((SECONDS - started)) s)")
    failed=1
  fi
done

note "summary"
printf '%s\n' "${results[@]}"
exit "$failed"
