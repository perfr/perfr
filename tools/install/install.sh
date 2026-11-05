#!/bin/sh
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# perfr installer:
#
#     curl -fsSL https://perfr.dev/install.sh | sh
#
# It downloads the Rust perfr release for this machine. Linux
# x86-64 and macOS (one universal binary, Intel and Apple silicon) are
# both published. The binary serves the web viewer. The script checks
# SHA-256 and installs the Frida files the service looks for in the same
# directory. No compiler and no package manager.
#
# Download host for the release archives. perfr.dev serves the assets of the
# latest stable GitHub Release under /download/ (perfr-linux-x86_64.tar.gz,
# perfr-macos-universal.tar.gz, and the matching .sha256 files); its deploy
# fetches them, since the repository is private and GitHub will not hand
# them to an anonymous download.
DEFAULT_BASE="https://perfr.dev/download"
#
# Overridable:
#   PERFR_INSTALL_BASE   full prefix of the asset URLs (overrides DEFAULT_BASE
#                        and PERFR_VERSION)
#   PERFR_VERSION        tag to download from GitHub, e.g. perfr-v0.1.0
#                        (default: latest, from perfr.dev). GitHub serves it
#                        only once the repository is public.
#   PERFR_INSTALL_DIR    where to install (default: ~/.local/bin)

set -eu

BASE="${PERFR_INSTALL_BASE:-$DEFAULT_BASE}"
BASE=${BASE%/}
if [ -z "${PERFR_INSTALL_BASE:-}" ] && [ -n "${PERFR_VERSION:-}" ]; then
    case "${PERFR_VERSION}" in
        */*) printf 'perfr: error: PERFR_VERSION must be a tag such as perfr-v0.1.0\n' >&2; exit 1 ;;
    esac
    BASE="https://github.com/perfr/perfr/releases/download/${PERFR_VERSION}"
fi
VERSION="${PERFR_VERSION:-latest}"
INSTALL_DIR="${PERFR_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'perfr: %s\n' "$1" >&2; }
die() { printf 'perfr: error: %s\n' "$1" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# --- detect platform ---------------------------------------------------------
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
case "$os" in
    Linux)  os=linux ;;
    Darwin) os=macos ;;
    MINGW* | MSYS* | CYGWIN*)
        die "this is a POSIX shell on Windows. Use the PowerShell installer: irm https://perfr.dev/install.ps1 | iex" ;;
    *) die "unsupported OS '$os'. Linux and macOS use this script; Windows uses tools/install/install.ps1." ;;
esac
case "$arch" in
    x86_64 | amd64)        arch=x86_64 ;;
    aarch64 | arm64)       arch=aarch64 ;;
    *) die "unsupported architecture '$arch'" ;;
esac

case "${os}-${arch}" in
    linux-x86_64)
        asset="perfr-linux-x86_64.tar.gz"
        ;;
    macos-x86_64 | macos-aarch64)
        asset="perfr-macos-universal.tar.gz"
        ;;
    linux-aarch64)
        die "no published Linux arm64 build. The release is a static Linux x86-64 binary and a macOS universal binary. Arm64 Linux compiles from source and is not in CI."
        ;;
    *)
        die "no release asset for ${os}/${arch}"
        ;;
esac

url="${BASE}/${asset}"
say "installing perfr ${VERSION} for ${os}/${arch}"
say "from ${url}"

# --- pick a downloader -------------------------------------------------------
if have curl; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif have wget; then
    fetch() { wget -qO "$2" "$1"; }
else
    die "need curl or wget to download"
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/perfr-install.XXXXXX") || die "cannot make a temp dir"
trap 'rm -rf "$tmp"' EXIT INT TERM

archive="$tmp/$asset"
if ! fetch "$url" "$archive"; then
    die "download failed: ${url}

No Rust release is published at that URL yet, or this platform's archive is missing.
perfr.dev serves the latest stable release, tagged perfr-vX.Y.Z (see
docs/releasing.md). A prerelease suffix (perfr-vX.Y.Z-rc.1) is not Latest, so
it is not served there.
The v1.* and nightly-* releases are the legacy C++ build and do not contain perfr.

Build from source:
  Linux x86-64:  ./tools/build/build-service-musl.sh
  macOS:         tools/build/build-service-macos.sh

Or set PERFR_INSTALL_BASE to a URL prefix that already serves the files."
fi

# --- checksum is required ----------------------------------------------------
if ! fetch "${url}.sha256" "$tmp/sum"; then
    die "no checksum at ${url}.sha256
Refusing to install an unverified binary."
fi
want=$(awk '{print $1}' "$tmp/sum")
case "$want" in
    [0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]*)
        ;;
    *) die "checksum file did not contain a SHA-256 hash" ;;
esac
if have sha256sum; then
    got=$(sha256sum "$archive" | awk '{print $1}')
elif have shasum; then
    got=$(shasum -a 256 "$archive" | awk '{print $1}')
else
    die "need sha256sum or shasum to verify the download"
fi
if [ "$got" != "$want" ]; then
    die "checksum mismatch
  expected $want
  got      $got"
fi
say "checksum ok"

# --- unpack and install ------------------------------------------------------
tar -xzf "$archive" -C "$tmp" || die "could not unpack ${asset}"
root="$tmp/perfr"
[ -f "$root/perfr" ] || die "archive has no perfr/perfr"
for need in perfr-hook; do
    [ -f "$root/$need" ] || die "archive is missing $need. Function hooks need it beside perfr."
done
agent=""
for candidate in libperfr.so libperfr.dylib; do
    if [ -f "$root/$candidate" ]; then
        agent=$candidate
        break
    fi
done
[ -n "$agent" ] || die "archive is missing libperfr. perfr.h, the Python package and function hooks need it beside perfr."

mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
install_bin() {
    chmod 755 "$1"
    mv -f "$1" "$2" || die "cannot install to $2"
    say "installed $2"
}
install_file() {
    chmod 644 "$1"
    mv -f "$1" "$2" || die "cannot install to $2"
    say "installed $2"
}

install_bin "$root/perfr" "$INSTALL_DIR/perfr"
install_bin "$root/perfr-hook" "$INSTALL_DIR/perfr-hook"
install_file "$root/$agent" "$INSTALL_DIR/$agent"

if [ -f "$root/perfr.h" ]; then
    install_file "$root/perfr.h" "$INSTALL_DIR/perfr.h"
fi
if [ -f "$root/perfr-gpu" ]; then
    install_bin "$root/perfr-gpu" "$INSTALL_DIR/perfr-gpu"
fi
if [ -d "$root/licenses" ]; then
    mkdir -p "$INSTALL_DIR/licenses"
    cp -R "$root/licenses/." "$INSTALL_DIR/licenses/"
    say "installed Frida license notices in $INSTALL_DIR/licenses"
fi

# Prior installs left the split libraries and helpers in the install dir.
# The same names are deleted by `perfr update` (RETIRED_INSTALL_FILES in
# crates/perfr/src/update.rs) and by install.ps1. perfr_api.lib and
# libperfr_api.a are the static libraries and are not in this list.
for retired in \
    perfr-frida-helper \
    libperfr_frida_agent.so \
    libperfr_frida_agent.dylib \
    libperfr_api.so \
    libperfr_api.dylib \
    perfr-gpu-helper \
    perfr_frida_agent.dll \
    perfr_api.dll \
    perfr-frida-helper.exe \
    perfr-gpu-helper.exe \
    perfr-frida-helper.pdb \
    perfr-frida-helper.lib \
    perfr-gpu-helper.pdb \
    perfr-gpu-helper.lib \
    perfr_frida_agent.pdb \
    perfr_frida_agent.lib \
    perfr_frida_agent.dll.lib \
    perfr_api.pdb \
    perfr_api.dll.lib
do
    stale="$INSTALL_DIR/$retired"
    if [ -f "$stale" ] || [ -L "$stale" ]; then
        rm -f "$stale" || die "cannot remove $stale"
        say "removed $stale"
    fi
done

# --- PATH hint ---------------------------------------------------------------
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say "note: $INSTALL_DIR is not on your PATH. Add it, e.g.:"
        printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR" >&2
        ;;
esac

cat >&2 <<EOF

perfr is installed for this machine (Linux or macOS). Start it
and open the viewer in a browser:

  perfr
  # then open http://127.0.0.1:44766/

Function hooks use libperfr and perfr-hook in the same
directory as perfr, on either OS. They do not need root. On macOS,
attaching needs a task port; code signing, the hardened runtime, or SIP
can refuse it.

On Linux, sampling needs perf_event_paranoid at 2 or below, and the
scheduler needs 0 or below. Root, CAP_PERFMON, or CAP_SYS_ADMIN bypass
that sysctl. Auto-profiling uses the kernel hook engine and needs root
or CAP_SYS_ADMIN. Sampling, the scheduler, and auto-profiling are not
in the macOS build.

To instrument your own code: perfr.h is beside perfr (nothing to
link; it finds libperfr there), or use the perfr-api crate or package.
EOF
