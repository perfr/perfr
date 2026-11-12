<!--
Copyright (c) 2026 The perfr Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# perfr

perfr profiles a running program with no code change. This directory is the
release for one platform. The manual, and the installers that normally put
these files in place, are at <https://perfr.dev/>.

## Start

```
perfr
```

(`perfr.exe` on Windows), then open <http://127.0.0.1:44766/> in a browser.
The viewer is inside the binary; nothing else has to be installed or served.
The service listens on this machine only unless it is started with `--host`.
`perfr --help` lists the rest.

## What is here

Keep the files together: the service looks for the library and the helper in
its own directory.

| File | What it is |
|---|---|
| `perfr` (`perfr.exe`) | The capture service, with the viewer embedded. |
| `libperfr.dylib` (`libperfr.dll`, with `libperfr.dll.lib`) | The library profiled programs load: the manual instrumentation API, and the agent behind function hooks. |
| `perfr-hook` (`perfr-hook.exe`) | Attaches to a process and loads `libperfr` into it, for function hooks. |
| `include/perfr.h` | The C and C++ header for manual instrumentation. It links nothing: it finds `libperfr` beside the `perfr` on `PATH`. |
| `libperfr_api.a` (`perfr_api.lib`) | The same API as a static library, for programs that define `PERFR_STATIC`. |
| `perfr_api.py` | The Python module; it loads `libperfr` the same way the header does. |
| `PerfrTestRust.exe` | Windows only: an instrumented program to try a first capture on. |
| `licenses/` | Notices of the third-party code linked into these binaries. |

## On this platform

**macOS** (Intel and Apple silicon, one universal binary): manual scopes and
function hooks. Attaching to a process needs a task port, which code signing,
the hardened runtime or SIP can refuse. No call-stack sampling, scheduler,
thread states or GPU lanes. The binaries are ad-hoc signed, not notarized.

**Windows** (x64): manual scopes and function hooks need no privilege.
Call-stack sampling, the scheduler and thread states come from the kernel
logger (ETW): start `perfr.exe` from a terminal opened with *Run as
administrator* to record them. The binaries are not code-signed.
