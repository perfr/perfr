# Perfr

perfr manual instrumentation for Swift. The package `dlopen`s `libperfr`
with the same search order as `perfr.h`. It does not link the library.

```swift
import Perfr

_ = Perfr.initialize()
_ = Perfr.nameThread("sim")
Perfr.withScope("frame") {
    Perfr.value("fps", 59.9)
}
```

`Perfr.scope` returns a class whose `stop()` and `deinit` end the scope.
`nameThread` sets the OS thread name. `libperfr` has no thread-name call;
on Linux the name is the 15-byte `comm` perfr reads from `/proc`.

`initialize()` returns `0`, `Perfr.eNoLib` (`-1000`), `Perfr.eAbi`
(`-1001`), or a negative errno. Handles are `UInt64`. Zero means "no event".

## Build

```sh
swift test
PERFR_LIB=/path/to/libperfr.so swift test
swift run PerfrSample
```

The package tools version is 5.10 so a Swift 6 compiler builds it in Swift 5
mode. `CPerfrShim` is a twenty-line C target: the acquire/release slot that
publishes the loaded library (so a scope is a load and an indirect call, with
no lock on the hot path; Swift 5 has no atomics without a dependency) and
`pthread_setname_np`, whose signature differs between Linux and macOS and
which the Swift Glibc overlay does not re-export.

## What was tested

On Linux x86-64 with Swift 6.1.2 and `libperfr.so` from
`cargo build -p perfr-lib`:

- `PERFR_LIB` pointing at a missing file: `initialize` returns `eNoLib` and
  calls are no-ops.
- With the library: `initialize` returns 0, `nowNs` advances, and
  `swift-probe` / `swift-fps` are in `/dev/shm/perfr-scopes-<pid>` only after
  the capturing word at byte 28 is set. `nameThread` matches
  `/proc/thread-self/comm`, including the 15-byte cut.
- `swift run PerfrSample` prints the library path.

macOS was not run. The capture check knows the internal segment header.
