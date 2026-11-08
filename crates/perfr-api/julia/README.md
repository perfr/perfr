# Perfr

perfr manual instrumentation for Julia. The module `ccall`s `libperfr`
through `Libdl`, with the same search order as `perfr.h`.

```julia
using Perfr

Perfr.init()
Perfr.name_thread("sim")
Perfr.scope("frame") do
    Perfr.value("fps", 59.9)
end
```

`name_thread` sets the OS thread name. `libperfr` has no thread-name call.
On Linux the name is the 15-byte `comm`.

`init` returns `0`, `E_NOLIB` (`-1000`), `E_ABI` (`-1001`), or a negative
errno. Handles are `UInt64`. Zero means "no event".

## Test

From this directory, with no registry lookup (the package has no dependencies):

```sh
PERFR_LIB=/no/such/libperfr.so julia --project=. -e 'include("test/runtests.jl")'
PERFR_LIB=/path/to/libperfr.so julia --project=. -e 'include("test/runtests.jl")'
julia --project=. samples/frame.jl
```

## What was tested

On Linux x86-64 with Julia 1.13.3 and `libperfr.so` from
`cargo build -p perfr-lib`:

- A missing `PERFR_LIB`: `init` returns `E_NOLIB` and calls are no-ops.
- With the library: `init` returns 0, `now_ns` advances, and `julia-probe` /
  `julia-fps` are in the process segment only after the capturing word at
  byte 28 is set. `name_thread` matches `/proc/thread-self/comm`.
- The sample prints the library path.

`[compat]` declares Julia 1.10, which was not run. macOS and Windows
were not run. The capture check knows the internal segment header.
`Project.toml` is a local package; it is not registered.
