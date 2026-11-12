# The crates

perfr is one Cargo workspace at the repository root, and every crate is a
directory here. `cargo build -p perfr` compiles the capture service and
nothing of the viewer's eframe/wgpu tree; `cargo test --workspace` covers
all of it.

- Run: `./run.sh` at the repository root builds the service with the Frida
  helper and agent beside it and serves the viewer; `./run.sh --help` lists
  the flags.
- Use: the manual is in [`docs/manual`](../docs/manual/index.md) and on
  [perfr.dev](https://perfr.dev/).
- Background: [the dev blog](https://perfr.dev/blog/) is the running account
  of how each part was built and what it cost.

## What each crate is

The service, and what a capture is stored as:

| Crate | What it is |
|---|---|
| `perfr` | The capture service, one binary: the command line, the capture loop, symbolization and demangling (`cpp_demangle`, `msvc-demangler`, `rustc-demangle`), function hooks, auto-profiling, the MCP server, `perfr update`. It serves the viewer through `perfr-live-server`. |
| `perfr-build-info` | Version, commit, build date and release tag, stamped once by its build script and shown by `perfr --version` and the viewer's About window. |
| `perfr-capture` | A capture on disk: Arrow in memory, Parquet in the `.perfr.zip` bundle, and slicing a capture to a time range. |
| `perfr-wire` | The pod wire format: a one-byte tag and fixed little-endian fields per event, no dependencies. |
| `perfr-pod-dump` | Reads a pod capture and prints the machine, the event counts and the hottest callstacks. |

Where the events come from on Linux:

| Crate | What it is |
|---|---|
| `perfr-perf-records` | Packed layouts of the perf ring-buffer records the service reads. The kernel's `linux/perf_event.h` is the specification. No dependencies. |
| `perfr-perf-ring` | `perf_event_open`, the event attributes and the mmap ring-buffer protocol. The unsafe code is confined to its `sys` module; `libc` is the one dependency. |
| `perfr-unwind` | DWARF callstack unwinding over sampled stack copies, with `framehop` and `object`. |
| `perfr-maps` | Parsing of `/proc/<pid>/maps`. No dependencies. |
| `perfr-thread-states` | The per-thread state machine over the scheduler tracepoints. |
| `perfr-tracing-state` | Bookkeeping that needs no kernel: pairing context switches, hooked calls, GPU samples. |
| `perfr-gpu` | The NVIDIA telemetry helper, a separate process (see Distributing). |

On Windows:

| Crate | What it is |
|---|---|
| `perfr-etw` | Event Tracing for Windows: the kernel logger as the source of scheduling, thread states and samples. |

Symbols:

| Crate | What it is |
|---|---|
| `perfr-object` | ELF, PE/COFF and PDB: symbols, line tables, unwind tables and debug-file lookup, on `object`, `gimli`, `pdb` and `msvc-demangler`. Its `testdata/` holds the binaries the tests parse. |

What runs inside a profiled program, and what puts it there:

| Crate | What it is |
|---|---|
| `perfr-scope-ring` | The per-core lock-free rings in shared memory that an instrumented process writes and the service drains. |
| `perfr-api` | Manual instrumentation: the C ABI behind `include/perfr.h`, the Rust API, the Python package, and the bindings for other languages in its subdirectories. Its own [README](perfr-api/README.md) is the user guide. |
| `perfr-lib` | `libperfr`, the one library loaded into profiled processes: the manual API and, with the `native` feature, the Frida Gum hook agent. |
| `perfr-lib-windows` | `perfr-lib` under the file name Windows needs (`libperfr.dll`), so its PDB does not collide with `perfr.exe`'s. |
| `perfr-hook` | The Frida Core controller the service spawns to attach to a target and load `libperfr` into it. |
| `perfr-frida-transport` | The capture lease and the counters shared by the service, the helper and the agent. |
| `perfr-frida-build` | Build-script helper that compiles the two small C adapters against the Frida devkits (`tools/frida/devkits.sh` downloads them). A debug build without devkits links a stub, so workspace-wide commands work on a fresh checkout. |
| `perfr-test-rust` | `PerfrTestRust`, a program that makes every manual-instrumentation call. Its C, C++ and Python siblings are in `tools/test-programs`. |

The viewer and its server:

| Crate | What it is |
|---|---|
| `perfr-live-event` | The 32-byte packed events the viewer's whole model is made of. |
| `perfr-live-protocol` | Binary framing of the WebSocket stream between service and viewer. |
| `perfr-live-ring` | The service's in-process ring of live events, with optional spill to disk. |
| `perfr-live-server` | The HTTP and WebSocket server inside `perfr`. Its build script embeds the viewer pack (`perfr-live-viewer/viewer-dist`) into the binary, so a rebuilt pack needs a rebuilt service. |
| `perfr-live-chrome` | Import of Chrome Trace Event JSON and Perfetto proto traces into live events. |
| `perfr-live-render` | The pixel-column rasterizer of the timeline. |
| `perfr-live-viewer` | The viewer: an eframe/egui application compiled to WebAssembly by `build_wasm.sh`. The built pack is committed in `viewer-dist/`. See its [README](perfr-live-viewer/README.md). |
| `perfr-story` | The versioned story document (camera targets and overlays) shared by the viewer and the MCP server. |
| `perfr-web` | The recorder behind `@perfr/web` (`web/perfr`): a ring of live events encoded as a `.perfr.stream`, compiled to WebAssembly for a page that profiles itself, with a C ABI for Emscripten and native callers. |

The `docs` step of `tools/ci.sh` fails when a directory here is missing from
these tables.

## Distributing

Three binaries and one library, and only the first is ever required.

| File | Build | Size, stripped | Needs on the target | Required? |
|---|---|---|---|---|
| `perfr` | `tools/build/build-service-musl.sh` | ~25 MB | nothing (static, 0 `DT_NEEDED`) | always |
| `libperfr.so` | `cargo build --release -p perfr-lib --features native` | ~3.5 MB | glibc; loaded into the profiled program | manual instrumentation and function hooks |
| `perfr-hook` | `cargo build --release -p perfr-hook` | ~15 MB | glibc >= 2.34 | only for function hooks |
| `perfr-gpu` | `cargo build --release -p perfr-gpu` | ~450 KB | glibc >= 2.34, libgcc_s; NVIDIA driver at runtime | only for NVIDIA GPU telemetry |

Sizes are of a build on 2026-11-11 with debug info removed; they move, and
the notes of every release end with the sizes measured in that run.

`perfr` is statically linked and runs on any x86-64 Linux with no runtime
dependencies at all. About 10 MB of it is the embedded viewer pack; the
rest is the service, the HTTP stack and the Arrow/Parquet writer. It never
links or loads a GPU library, so it behaves identically on NVIDIA, AMD, and
machines with no GPU.

`perfr-gpu` is dynamically linked *on purpose*: static musl cannot
`dlopen`, and NVML must be loaded at runtime. It does **not** link
`libnvidia-ml` at build time (it `dlopen`s it), so the same helper binary runs
on machines without an NVIDIA driver -- it just exits quietly and the capture
proceeds without GPU telemetry.

The NVIDIA driver itself is never shipped with these binaries: `libnvidia-ml.so.1`
comes from the driver installed on the target and must match its kernel module.

The glibc >= 2.34 floor (Ubuntu 22.04+, Debian 12+, RHEL 9+) comes from the
build host. To support older distributions, build the helper on the oldest one
you need; the service is unaffected, which is the point of the split.

Running:

```
# no arguments: serve the WASM live viewer and drive captures from the UI
perfr                      # -> http://127.0.0.1:44766/
perfr --serve 44768        # a different port


# CPU sampling + scheduling only (no GPU library anywhere)
perfr --pid <tid> --duration-ms 5000 --out capture.pod

# ...plus NVIDIA GPU telemetry
perfr --pid <tid> --duration-ms 5000 \
              --gpu-helper ./perfr-gpu --out capture.pod

# read the capture back
perfr-pod-dump capture.pod            # machine, counts, hottest callstacks
perfr-pod-dump capture.pod --top 20   # more stacks
perfr-pod-dump capture.pod --events   # every event, for debugging
```

`perfr-pod-dump` is how you inspect a capture: it prints the machine it was
taken on, the GPUs, the event counts and time span, and the hottest
callstacks by sample count (addresses only -- symbolization is a later
stage). With `--out` and no `--pid`, the service profiles its own busy
worker threads, which is the quickest way to confirm a build works.

Privileges: stack sampling needs `perf_event_paranoid <= 2`; system-wide
scheduling needs `<= 0`; tracing another user's process also needs
`CAP_SYS_PTRACE`. Root, `CAP_PERFMON` (Linux 5.8+) or `CAP_SYS_ADMIN` bypass
the sysctl entirely.

**The service never exits over missing privileges.** It drops the features it
cannot read, prints exactly which ones and the commands that would enable
them (`setcap`, `sysctl`, or `sudo`, with this binary's real path filled in),
and still writes a valid capture -- machine metadata and GPU telemetry need no
privileges at all.

## Logs

Every run keeps a log: what goes to stderr also goes to
`~/.perfr/logs/perfr-<utc time>-<pid>.log`, and files older
than a week are removed when a new one opens. Under `sudo` the directory is
the invoking user's and the file is chowned to them, so `./run.sh --sudo`
leaves a log you can read and delete. The banner and `GET /api/status`
(`log_path`) say where it is.

```
[2026-09-26T14:03:22.123456] [INFO ] [serve.rs:773] 3 sampling ring(s) at 1000 Hz, unwinder ready
[2026-09-26T14:03:24.010775] [WARN ] [viewer 7c1e perfr_live_viewer::net] WebSocket closed
[2026-09-26T14:03:24.100031] [INFO ] [perfr-hook] agent injected into pid 4242
```

UTC time to the microsecond, the level, where the line came from, the
message. The origin is `file:line` for the service's own lines, the helper's
name for a line a child process (`perfr-hook`, `perfr-gpu`)
wrote to its stderr, and `viewer <page> <module>` for a line the browser
viewer relayed through `POST /api/log` -- the viewer sends its own log
(everything it prints to the console, and a panic) so one file has both
ends of a session. Two open tabs get different page ids.

`--log-dir <dir>` or `PERFR_LOG_DIR` puts the file elsewhere; `PERFR_LOG=debug`
(or `trace`) says more. `--help`, `--slice` and the other one-shot tools leave
no log behind.

## Testing

```
cargo test --workspace
```

works on a fresh checkout: `perfr-hook` is the one crate that needs the
Frida devkits, and its debug build links a stub when they have not been
downloaded.

`tools/ci.sh` is what the `ci` workflow runs and what to run before pushing:
rustfmt (report-only until the tree is clean under it), clippy (any warning
fails), the workspace tests under a memory cap, the static musl build of the
service, the viewer checked for wasm32, and a check that this file names
every crate and that the Unreal plugin's copy of `perfr.h` matches the
original. `tools/ci.sh test wasm` runs only the named steps.

The end-to-end suite that drives a real service and a headless browser is
`tools/e2e` ([README](../tools/e2e/README.md)); it is run by hand.

## Building

`rust-toolchain.toml` pins the toolchain; rustup installs it on first use.
`cargo` lives in `~/.cargo/bin` and may not be on `PATH`:

```
PATH="$HOME/.cargo/bin:$PATH" cargo test --workspace
```

The viewer pack is built separately, with a nightly toolchain, by
`crates/perfr-live-viewer/build_wasm.sh`; a service build embeds whatever
pack is in `viewer-dist/` at that moment.

## History

perfr began as a port of Orbit's C++ capture service, one module at a time,
each Rust crate checked against the C++ it replaced before that C++ was
removed. That is why some module docs and test comments name C++ files
(`ElfFile.cpp`, `ThreadStateManager.cpp`, ...): they were the specification.
None of that C++ is in this repository, and nothing here reads a
`PERFR_*_BACKEND` variable any more. The C++ tree, the switches that ran both
implementations side by side and the differential tools are on the `cpp`
branch of the Orbit fork this project grew out of; the
plan and the per-step write-ups are at
[perfr.dev/rust-port-plan.html](https://perfr.dev/rust-port-plan.html) and
on [the blog](https://perfr.dev/blog/).
