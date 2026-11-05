# Cutting a Rust release

The Rust capture service is published by
[`.github/workflows/release-rust.yml`](../.github/workflows/release-rust.yml).
Nothing in that workflow runs until you push a tag or start it by hand.
This document does not publish a release.

## Tag scheme

Tags look like `perfr-v0.1.0`. An optional suffix (`perfr-v0.1.0-rc.1`) is
allowed.

Do not use `v1.*`. Those tags, and the `nightly-*` tags, are the legacy C++
Qt UI and C++ `PerfrService`. A Rust tag on that line would collide with
`git describe --match 'v1.*'` in the C++ workflow and with the old wheel
trigger.

| Tag | GitHub Release |
|---|---|
| `perfr-vX.Y.Z` | Stable. This becomes **Latest**, which perfr.dev serves and `install.sh` downloads. |
| `perfr-vX.Y.Z-suffix` | Prerelease. It is not Latest, so the installer ignores it. |

`/releases/latest` stays empty until the first stable `perfr-vX.Y.Z`. The
three existing C++ nightlies are already prereleases, so they are not Latest.
They were not deleted or retagged.

## What the workflow builds

Each stable tag publishes, per platform, the archive the installer
downloads, a debug-symbol archive, and a symbol-store archive. Platforms: static Linux
x86-64, universal macOS (Apple silicon and Intel), and Windows x86-64.
Linux arm64 is not in the release.

Release builds compile **full** debug info (`debug = true` in the release
profile, `strip = "none"` so cargo does not delete it). Dependencies stay
without debug info: that is a compile-time choice, and their line tables
alone were 35 MB. `tools/release/split_debug.py` then splits the shipped
binaries:

| Platform | Split | What stays in the binary |
|---|---|---|
| Linux | `objcopy --only-keep-debug`, `--strip-debug`, `--add-gnu-debuglink` | symbol table and the GNU build-id note. Link with `-Wl,--build-id=sha1` (the workflow sets `RUSTFLAGS`). |
| macOS | `dsymutil`, then `strip -S`, then ad-hoc `codesign` | the Mach-O UUID that matches the `.dSYM` |
| Windows | PDB already beside the image | the RSDS pointer. The PDB is not in the zip. |

The symbol table is kept on purpose. perfr symbolizes its own binary, and
the disassembly example finds functions from that table. Source lines come
from `.debug_line` in the detached file, not from the release binary. The
example reads `.gnu_debuglink` (next to the binary, `.debug/`, or
`/usr/lib/debug`), then `/usr/lib/debug/.build-id`, then
`PERFR_SYMBOL_PATH/buildid/<id>/debuginfo` or the debuginfod client cache.
A local `cargo build --release` still has the line table inside the binary,
because only the release packager splits it. macOS has no live disassembly.
Windows disassembly does not read source lines from a PDB yet.

Debug archive names look like `perfr-v0.1.0-linux-x86_64-debug.tar.gz`.
The symbol-store archive is `perfr-v0.1.0-linux-x86_64-symstore.tar.gz`
with this layout:

- Linux: `buildid/<hex-build-id>/debuginfo` (the debuginfod URL path, so the
  tree can be the document root of a static debuginfod server)
- Windows: `<filename>/<GUIDAGE>/<filename>` (SymSrv). `GUIDAGE` is the
  CodeView GUID in mixed endian plus the age in unpadded lowercase hex.
- macOS: `dsym/<uuid>/<name>.dSYM/`

If `PERFR_SYMBOL_STORE_BUCKET` and an access key are set
(`PERFR_SYMBOL_STORE_ACCESS_KEY_ID` or `AWS_ACCESS_KEY_ID`, and the matching
secret), the publish job syncs that tree to the bucket with the AWS CLI.
`PERFR_SYMBOL_STORE_ENDPOINT` is the R2 S3 endpoint
(`https://<account>.r2.cloudflarestorage.com`). `PERFR_SYMBOL_STORE_PREFIX`
is an optional key prefix. `PERFR_SYMBOL_STORE_REGION` defaults to `auto`.
With no bucket or no credentials the step prints why and exits 0. The
symstore archives are on the GitHub release either way. Unpack them and
`aws s3 sync` later; `tools/release/upload-symbols.sh` is that step.

`python3 tools/release/test_split_debug.py` checks the path layout and, when
gcc is installed, that objcopy keeps the build-id and moves `.debug_line`
into the detached file.

### The archive

Linux x86-64, on `ubuntu-22.04`, `perfr-linux-x86_64.tar.gz`:

- `perfr`, static musl (`x86_64-unknown-linux-musl`), DWARF removed. The
  WASM viewer in `crates/perfr-live-viewer/viewer-dist` is embedded by
  `perfr-live-server` at compile time. The job checks that the binary is
  statically linked, that `.gnu_debuglink` is present, and that the build-id
  survived.
- `perfr-frida-helper` and `libperfr_frida_agent.so`, the function-hook
  engine, stripped the same way, and `licenses/` for the Frida devkits.
  They are glibc binaries: the helper hosts Frida Core and the agent is
  loaded into ordinary glibc programs.
- `libperfr_api.so` and `perfr.h`, for manual instrumentation.
- `perfr-gpu-helper`, the optional NVIDIA helper. It is a glibc binary on
  purpose (the static service cannot `dlopen`).

macOS, on `macos-15`, via `tools/build/build-service-macos.sh --universal`,
`perfr-macos-universal.tar.gz`:

- One universal `perfr` (Apple silicon and Intel), `perfr-frida-helper`,
  `libperfr_frida_agent.dylib`, `libperfr_api.dylib`, `libperfr_api.a`,
  `perfr.h`, `licenses/`, and the SDK extras the build script writes.
- Ad-hoc signed (`codesign --sign -`) after the strip, not notarized.
- Records manual scopes and Frida function hooks. No call-stack sampling,
  scheduler, live disassembly, or auto-profiling.
- No GPU helper. That binary is Linux-only.

Windows x86-64, on `windows-2022`, via `.\tools\build\build-service-windows.ps1`,
`perfr-windows-x86_64.zip`:

- `perfr.exe` with the viewer embedded, linked against the static C runtime
  (`.cargo/config.toml`); the build script checks that with `dumpbin /dependents`.
- `perfr-frida-helper.exe` and `perfr_frida_agent.dll` (function hooks),
  `licenses/`; `perfr_api.dll`, `perfr_api.dll.lib`, `perfr_api.lib` and
  `include\perfr.h`; `perfr_api.py`; `PerfrTestRust.exe`. No PDBs in this zip.
- Records manual scopes and Frida hooks without privilege. Call-stack
  sampling, the scheduler and thread states come from the NT Kernel Logger
  (ETW) and need an elevated prompt. No auto-profiling, no kernel hook
  engine, no GPU helper.
- Not code-signed.

Linux arm64 is not in the release. The service compiles for arm64; it is not
run in CI, and the static musl build in this workflow is x86-64 only.

The Linux and macOS archives are gzipped tars, the Windows one is a zip;
debug and symstore archives are gzipped tars on every platform. The
installer archives use the top-level directory `perfr/`. Beside the archives the
release has `SHA256SUMS` and a `.sha256` file per archive (`sha256sum`
format: hash, two spaces, file name). `tools/install/install.ps1` is the
Windows installer and reads the zip's `.sha256` the way `install.sh` reads
the tars'. The pip wheels are a separate channel and bundle the same
companion files inside the `perfr` wheel; see `tools/release/README.md`.

## How to cut one

From the commit you want to ship:

```sh
git tag perfr-v0.1.0
git push origin perfr-v0.1.0
```

Pushing the tag starts the workflow. The workflow does not create the tag.
Wait until the Linux, macOS, and Windows jobs and the publish job are green, then check
`https://github.com/perfr/perfr/releases/tag/perfr-v0.1.0`.

`workflow_dispatch` (Actions → release-rust → Run workflow) builds the same
artifacts and uploads them to the run. Leave **tag** empty: that does not
publish and does not create a tag. Set **tag** to a `perfr-v*` tag that
already exists only when you want that run to create the GitHub Release for
it (`gh release create --verify-tag`).

The Cargo version (`0.1.0` today) is independent of the git tag. The pip
wheels are not: they publish from the same `perfr-v*` tags, and their version
has to match across the files listed in
[`tools/release/README.md`](../tools/release/README.md). Pushing `perfr-v*`
also starts `.github/workflows/python-wheels.yml`, which publishes to PyPI
when the `pypi` environment's Trusted Publishing is configured. Do not push
the tag if you do not want that publish attempt.

## Installer host

The installers are fetched from `https://perfr.dev/install.sh` and
`https://perfr.dev/install.ps1`; the website's `build_site.py` (the perfr-web repository) copies them to
the site root. They download the archives from `https://perfr.dev/download/`
(`DEFAULT_BASE` at the top of
[`tools/install/install.sh`](../tools/install/install.sh), `$base` in
`install.ps1`; `PERFR_INSTALL_BASE` overrides it for one run).

The repository is private, so GitHub will not serve release assets to an
anonymous download. The perfr.dev server fetches them instead: every minute
its deploy (`deploy/fetch_release.py` in perfr-web) asks GitHub for the
latest stable release with a read-only token kept on the server, downloads
the assets of a new one, checks them against `SHA256SUMS` and rebuilds the
site with them under `/download/`. A release shows up there a minute or two
after the publish job is green.

Until a stable `perfr-v*` release exists, the script exits with an error that
says so and points at a source build. It does not install a C++ nightly.

## Legacy C++ nightlies

[`.github/workflows/release.yml`](../.github/workflows/release.yml) no longer
has a weekly schedule. A manual run still builds the Qt UI and C++
`PerfrService`, and still uploads a **prerelease** titled as a legacy C++
nightly. The pinned release action has no `make_latest` input; `prerelease:
true` is what keeps GitHub from selecting it as Latest. Existing
`nightly-2026-09-24`, `nightly-2026-10-01`, and `nightly-2026-10-08` releases
are unchanged.
