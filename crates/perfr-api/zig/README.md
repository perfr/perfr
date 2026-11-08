# perfr for Zig

Manual instrumentation for Zig 0.17. The module loads `libperfr` with
`std.DynLib` and the same search order as `perfr.h`. It does not
`@cImport` the header: the header's weak-symbol loader is a C feature, and
the calls are the plain C ABI underneath it.

```zig
const perfr = @import("perfr");

pub fn frame() void {
    const scope = perfr.scope("frame");
    defer scope.end();
    perfr.value("fps", 59.9);
}
```

`nameThread` sets the OS thread name. `libperfr` has no thread-name call.
On Linux the name is the 15-byte `comm` perfr reads from `/proc`.

`init` returns `0`, `perfr.e_nolib` (`-1000`), `perfr.e_abi` (`-1001`), or
a negative errno from the library. Until it succeeds, every call returns
immediately. After that, `libperfr` still writes nothing unless a service
is capturing.

## Build

```sh
zig build test
zig build run
PERFR_LIB=/path/to/libperfr.so zig test src/perfr.zig -lc
```

`zig build test` caches its run step, so a changed `PERFR_LIB` does not
re-run the assertions. `test.sh` uses `zig test` for that reason. It needs
Zig 0.17.1 (`minimum_zig_version` in `build.zig.zon`).

Add the module to another package with a path dependency on this directory,
then `@import("perfr")`.

## What was tested

On Linux x86-64 with Zig 0.17.1 and `libperfr.so` from
`cargo build -p perfr-lib`:

- `PERFR_LIB` pointing at a missing file: `init` returns `e_nolib`, calls
  are no-ops, including `scope` / `end`.
- With the library: `init` returns 0, `nowNs` advances, and `zig-probe` /
  `zig-fps` are in `/dev/shm/perfr-scopes-<pid>` only after the capturing
  word at byte 28 is set. `nameThread` matches `/proc/thread-self/comm`,
  including the 15-byte cut.
- `zig build run` prints the library path.

macOS and Windows were not run. The capture check knows the internal
segment header (magic, version 2, pid, capturing at byte 28).
