# Perfr

perfr manual instrumentation for C#. The package loads `libperfr` at run
time (the same library C and C++ load through `perfr.h`) and is a no-op when
that library is missing or `Init` was not called.

```csharp
using Perfr;

Perfr.Init();
Perfr.NameThread("sim");
using (Perfr.Scope("frame"))
{
    using (Perfr.Scope("update")) { }
    Perfr.Value("fps", 59.9);
}
```

Run `perfr`, open its viewer, pick this process, press Record. With no
`libperfr` the same binary still runs.

## How it finds the library

Same order as `perfr.h`:

1. `$PERFR_LIB`, the full path. When set it is the only place tried.
2. Beside `perfr` on `PATH`, then `~/.local/bin` and `~/.perfr/bin`
   (`%USERPROFILE%\.perfr\bin` on Windows).
3. Beside the running executable.
4. The system loader (`libperfr.so`, `libperfr.dylib`, or `libperfr.dll`).

The service is the authority on the ring protocol, so its own library wins
over a copy beside the application. `Init` returns `0`, `Perfr.ENoLib`
(`-1000`), `Perfr.EAbi` (`-1001`), or a negative errno. Later calls stay
valid no-ops in every failure case. After a library is loaded, `libperfr`
still writes nothing until a perfr service is capturing the process.

## API

`Init`, `Shutdown`, `Available`, `LibraryPath`, `Start`, `StartAsync`,
`Stop`, `Instant`, `Link`, `Value`, `NowNs`, `Span`, `SpanAsync`, `Scope`,
`ScopeAsync`, `NameThread`.

`Scope` and `ScopeAsync` return a `ScopeGuard` for `using`. String names are
encoded as UTF-8. The `ReadOnlySpan<byte>` overloads pass the bytes through
with no copy. Handles are `ulong`; `0` means "no event".

`NameThread` is not a `libperfr` call. `libperfr` has no thread-name entry.
It sets the operating-system thread name, which perfr reads from
`/proc/<pid>/task/<tid>/comm` on Linux (15 bytes), `pthread_setname_np` on
macOS, and `SetThreadDescription` on Windows. It also sets
`Thread.CurrentThread.Name` when that is still unset.

## Build

The library targets `net6.0`, `net8.0`, and `netstandard2.1` (so it loads on
.NET 6 and later). `NativeLibrary` is not in the netstandard2.1 contract, so
the loader calls `dlopen` / `LoadLibrary` directly.

```sh
dotnet test -c Release          # no library: every call is a no-op
PERFR_LIB=/path/to/libperfr.so dotnet test -c Release
dotnet run -c Release --project samples/PerfrSample -- --seconds 3
dotnet pack src/Perfr/Perfr.csproj -c Release
```

`test.sh` does the no-library run, then the `libperfr` run when
`target/debug/libperfr.so` (or the path you pass) exists.

## What was tested

On Linux x86-64, with the .NET 8 SDK (8.0.133) and a `libperfr.so` built
from this tree (`cargo build -p perfr-lib`):

- `net6.0`, `net8.0`, and `netstandard2.1` all compile.
- Tests run on `net8.0`. With `PERFR_LIB` pointed at a missing file, `Init`
  returns `ENoLib` and every call is a no-op.
- With the real library, `Init` returns 0, `NowNs` advances, and calls stay
  zero until the segment's capturing word is set. A scope, a value, and a
  non-ASCII name then show up in `/dev/shm/perfr-scopes-<pid>`. `Shutdown`
  unlinks that file.
- `NameThread` sets `/proc/thread-self/comm`, including the 15-byte cut.
- The sample runs for one second against that library.
- `dotnet pack` produces a nupkg. Nothing is published to NuGet.

macOS and Windows are implemented and were not run. The capture check reads
the shared-memory header (`perfr-scope-ring`'s `Header`: magic, version 2,
pid, capturing at byte 28). That layout is internal; a future ring change
would fail this test rather than silently pass.
