#!/usr/bin/env bash
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Builds the pip artifacts for one or more platforms:
#
#   perfr-api        the pure-Python instrumentation package with a prebuilt
#                    the API-only libperfr bundled in (a platform wheel).
#   perfr            the perfr binary, the full libperfr, perfr-hook and perfr.h in a
#                    platform wheel, with a `perfr` console command.
#
# Both wheels for a platform are cut from ONE build so the ring protocol
# version matches between the library that writes and the service that reads.
#
#   tools/release/build_wheels.sh linux-x86_64 [linux-aarch64 macos-arm64 ...]
#
# Wheels land in dist/wheels/. Needs, on PATH or in a venv: cargo, rustup,
# cargo-zigbuild, ziglang, python3 with `build` and `wheel`. zig cross-links
# every target from a Linux host; on a macOS host the mac targets use Apple's
# own toolchain, which cross-links arm64 and x86_64 natively.
#
# The library is built against glibc 2.17 (manylinux2014) so the wheel installs
# on any modern Linux; the service binary is a fully static musl build.
#
# The perfr wheel always includes the Frida companions. The service
# loads libperfr and spawns perfr-hook from the directory
# that contains the perfr executable. This script builds those for the
# wheel's target (glibc or macOS, not the musl service triple). Set
# PERFR_COMPANIONS_DIR to skip that build and copy a directory already laid
# out the way tools/frida/build.sh <dir> writes it.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)

API_PKG="$ROOT/crates/perfr-api/python"
SVC_PKG="$ROOT/crates/perfr/python"
HEADER="$ROOT/crates/perfr-api/include/perfr.h"
OUT="$ROOT/dist/wheels"
GLIBC="2.17" # manylinux2014 floor

# Ship stripped: the workspace release profile keeps debuginfo (useful for dev,
# 8x the size for a wheel). Strip at link time so no per-arch strip tool is
# needed for cross builds.
export CARGO_PROFILE_RELEASE_STRIP=symbols

command -v cargo-zigbuild >/dev/null || { echo "need cargo-zigbuild (pip install cargo-zigbuild ziglang)" >&2; exit 1; }
python3 - <<'PY' || exit 1
import sys
try:
    import build, wheel, setuptools
except ImportError as exc:
    sys.stderr.write(f"need python build, wheel, and setuptools>=77 ({exc})\n")
    sys.stderr.write("pip install 'setuptools>=77' build wheel\n")
    sys.exit(1)
parts = []
for piece in setuptools.__version__.split("."):
    digits = "".join(ch for ch in piece if ch.isdigit())
    if not digits:
        break
    parts.append(int(digits))
if tuple(parts[:2]) < (77, 0):
    sys.stderr.write(
        f"need setuptools>=77 (SPDX license field), found {setuptools.__version__}\n"
    )
    sys.exit(1)
PY

# One release version, stamped in five places. Refuse to build if they drift:
# perfr pins perfr-api to the exact version because the two must
# speak the same ring protocol, and a stale pin would install a mismatch.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/crates/perfr-api/Cargo.toml" | head -1)
drift() { echo "version drift: $1 (perfr-api Cargo.toml says $VERSION)" >&2; exit 1; }
grep -q "^version = \"$VERSION\"" "$API_PKG/pyproject.toml" || drift "$API_PKG/pyproject.toml"
grep -q "^version = \"$VERSION\"" "$SVC_PKG/pyproject.toml" || drift "$SVC_PKG/pyproject.toml"
grep -q "\"perfr-api==$VERSION\"" "$SVC_PKG/pyproject.toml" || drift "perfr must pin perfr-api==$VERSION"
grep -q "__version__ = \"$VERSION\"" "$API_PKG/perfr_api/__init__.py" || drift "$API_PKG/perfr_api/__init__.py"
grep -q "__version__ = \"$VERSION\"" "$SVC_PKG/perfr/__init__.py" || drift "$SVC_PKG/perfr/__init__.py"
echo "release version $VERSION"

# key -> "lib_target service_target platform_tag lib_file"
target_spec() {
  case "$1" in
    linux-x86_64)  echo "x86_64-unknown-linux-gnu  x86_64-unknown-linux-musl  manylinux_2_17_x86_64   libperfr.so" ;;
    linux-aarch64) echo "aarch64-unknown-linux-gnu aarch64-unknown-linux-musl manylinux_2_17_aarch64  libperfr.so" ;;
    macos-x86_64)  echo "x86_64-apple-darwin       x86_64-apple-darwin        macosx_11_0_x86_64      libperfr.dylib" ;;
    macos-arm64)   echo "aarch64-apple-darwin      aarch64-apple-darwin       macosx_11_0_arm64       libperfr.dylib" ;;
    windows-x86_64) echo "x86_64-pc-windows-msvc   x86_64-pc-windows-msvc     win_amd64               libperfr.dll" ;;
    *) echo "unknown platform key '$1' (use linux-x86_64 linux-aarch64 macos-x86_64 macos-arm64 windows-x86_64)" >&2; return 1 ;;
  esac
}

# On a macOS host, Apple's toolchain links both mac architectures; zig is for
# cross-linking from Linux (and for musl, where it needs no musl toolchain).
native_mac() { [[ $(uname -s) == Darwin && "$1" == macos-* ]]; }
# Windows wheels are built on Windows (Git Bash) with MSVC: no zig, no cross.
native_windows() { [[ "$1" == windows-* ]]; }
# The service binary's file name on the platform being built.
service_file() { if [[ "$1" == windows-* ]]; then echo perfr.exe; else echo perfr; fi; }
# The crate that produces libperfr: on Windows a wrapper, so the DLL is
# libperfr.dll and its PDB does not collide with perfr.exe's.
lib_crate() { if [[ "$1" == windows-* ]]; then echo perfr-lib-windows; else echo perfr-lib; fi; }

# Frida's devkits and the helper are per-target glibc/macOS binaries, not musl.
companion_triple() {
  case "$1" in
    linux-x86_64)  echo "x86_64-unknown-linux-gnu" ;;
    linux-aarch64) echo "aarch64-unknown-linux-gnu" ;;
    macos-x86_64)  echo "x86_64-apple-darwin" ;;
    macos-arm64)   echo "aarch64-apple-darwin" ;;
    windows-x86_64) echo "x86_64-pc-windows-msvc" ;;
    *) echo "unknown platform key '$1'" >&2; return 1 ;;
  esac
}

# build_frida_companions <key> <dest>
# Writes the full libperfr.* (API + hook agent), perfr-hook, and licenses/ into dest.
build_frida_companions() {
  local key=$1 dest=$2 triple lib host
  triple=$(companion_triple "$key")
  "$ROOT/tools/frida/devkits.sh" "$triple"
  rustup target add "$triple" >/dev/null 2>&1 || true
  host=$(rustc -vV | sed -n 's/^host: //p')
  echo "-- Frida companions ($triple)"
  local -a cargo
  if [[ "$triple" != "$host" ]] && ! native_mac "$key" && ! native_windows "$key"; then
    cargo=(cargo zigbuild --locked --release --target "$triple")
  else
    cargo=(cargo build --locked --release --target "$triple")
  fi
  "${cargo[@]}" --manifest-path "$ROOT/Cargo.toml" -p perfr-hook >/dev/null
  local helper=perfr-hook
  case "$key" in
    macos-*) lib=libperfr.dylib; "${cargo[@]}" --manifest-path "$ROOT/Cargo.toml" -p perfr-lib --features native >/dev/null ;;
    windows-*) lib=libperfr.dll; helper=perfr-hook.exe; "${cargo[@]}" --manifest-path "$ROOT/Cargo.toml" -p perfr-lib-windows --features native >/dev/null ;;
    *) lib=libperfr.so; "${cargo[@]}" --manifest-path "$ROOT/Cargo.toml" -p perfr-lib --features native >/dev/null ;;
  esac
  mkdir -p "$dest/licenses"
  cp "$ROOT/target/$triple/release/$lib" "$dest/"
  cp "$ROOT/target/$triple/release/$helper" "$dest/"
  chmod 755 "$dest/$helper"
  cp "$ROOT/tools/frida/licenses/"* "$dest/licenses/"
}

# --locked everywhere, as in release-rust.yml: the wheel and the GitHub release
# for one tag must resolve the same dependency versions, and a lock that is
# out of date should stop the build, not be rewritten by it.
#
# cargo_for <key> <manifest> <target> [extra cargo args]
cargo_for() {
  local key=$1 manifest=$2 target=$3; shift 3
  if native_mac "$key" || native_windows "$key"; then
    cargo build --locked --release --manifest-path "$manifest" --target "$target" "$@" >/dev/null
  else
    cargo zigbuild --locked --release --manifest-path "$manifest" --target "$target" "$@" >/dev/null
  fi
}

# Build a platform wheel from a pure-Python package: build py3-none-any, then
# stamp the platform tag onto it. The package's .so/binary rides as data, and
# py3-none-<platform> means one wheel serves every Python 3.
build_and_tag() {
  local pkg=$1 plat=$2
  rm -rf "$pkg/build" "$pkg"/*.egg-info "$pkg/dist"
  # Keep stderr: a setuptools or metadata failure used to vanish into /dev/null
  # and the step exited 1 with no message after the Rust build.
  ( cd "$pkg" && python3 -m build --wheel --no-isolation --outdir dist >/dev/null )
  local anywheel tagged
  anywheel=$(echo "$pkg"/dist/*-py3-none-any.whl)
  python3 -m wheel tags --python-tag py3 --abi-tag none --platform-tag "$plat" --remove "$anywheel" >/dev/null
  tagged=$(echo "$pkg"/dist/*-"$plat".whl)
  mkdir -p "$OUT"
  mv "$tagged" "$OUT/"
  rm -rf "$pkg/build" "$pkg"/*.egg-info "$pkg/dist"
  echo "  $(basename "$tagged")"
}

for key in "$@"; do
  read -r lib_target svc_target plat lib_file <<<"$(target_spec "$key")"
  echo "== $key ($plat) =="

  rustup target add "$lib_target" "$svc_target" >/dev/null 2>&1 || true

  # 1. The API-only libperfr for the perfr-api wheel, against the glibc floor on
  #    Linux (zig picks the version). Copied out before step 3 rebuilds the
  #    same file name with the hook agent in it.
  echo "-- libperfr, API only ($lib_target)"
  if [[ "$key" == linux-* ]]; then
    cargo_for "$key" "$ROOT/Cargo.toml" "$lib_target.$GLIBC" -p "$(lib_crate "$key")"
  else
    cargo_for "$key" "$ROOT/Cargo.toml" "$lib_target" -p "$(lib_crate "$key")"
  fi
  lib="$ROOT/target/$lib_target/release/$lib_file"

  # 2. perfr. The musl target is static and self-contained.
  echo "-- perfr ($svc_target)"
  cargo_for "$key" "$ROOT/Cargo.toml" "$svc_target" -p perfr
  svc="$ROOT/target/$svc_target/release/$(service_file "$key")"

  # 3. Stage the artifacts into each package and build the wheels.
  cp "$lib" "$API_PKG/perfr_api/$lib_file"
  build_and_tag "$API_PKG" "$plat"

  cp "$svc" "$SVC_PKG/perfr/$(service_file "$key")"
  cp "$HEADER" "$SVC_PKG/perfr/perfr.h"
  staged_companions=()
  companions_src=${PERFR_COMPANIONS_DIR:-}
  companions_tmp=
  if [[ -z "$companions_src" ]]; then
    companions_tmp=$(mktemp -d)
    build_frida_companions "$key" "$companions_tmp"
    companions_src=$companions_tmp
  fi
  shopt -s nullglob
  # The full libperfr (API + hook agent) replaces nothing: the API-only one
  # went into the perfr-api wheel above and was never staged here.
  # perfr-hook.exe is only produced on Windows; the other name is only
  # produced elsewhere. A missing literal name is not a glob, so nullglob
  # does not drop it.
  for f in "$companions_src"/libperfr.* "$companions_src"/perfr-hook "$companions_src"/perfr-hook.exe; do
    if [[ ! -e "$f" ]]; then
      continue
    fi
    cp "$f" "$SVC_PKG/perfr/"
    staged_companions+=("$SVC_PKG/perfr/$(basename "$f")")
  done
  shopt -u nullglob
  if [[ -d "$companions_src/licenses" ]]; then
    cp -r "$companions_src/licenses" "$SVC_PKG/perfr/licenses"; staged_companions+=("$SVC_PKG/perfr/licenses")
  fi
  [[ -z "${companions_tmp}" ]] || rm -rf "$companions_tmp"
  if [[ ! -f "$SVC_PKG/perfr/perfr-hook" && ! -f "$SVC_PKG/perfr/perfr-hook.exe" ]] || \
     [[ ! -f "$SVC_PKG/perfr/libperfr.so" && ! -f "$SVC_PKG/perfr/libperfr.dylib" && ! -f "$SVC_PKG/perfr/libperfr.dll" ]]; then
    echo "Frida companions were not staged for $key" >&2
    exit 1
  fi
  echo "-- Frida companions: ${#staged_companions[@]} item(s)"
  build_and_tag "$SVC_PKG" "$plat"

  # Leave the trees clean: the bundled binaries are build products, not source.
  rm -rf "$API_PKG/perfr_api/$lib_file" \
         "$SVC_PKG/perfr/$(service_file "$key")" \
         "$SVC_PKG/perfr/$lib_file" "$SVC_PKG/perfr/perfr.h" \
         "${staged_companions[@]}"
done

echo
echo "wheels in $OUT:"
ls -1 "$OUT"
