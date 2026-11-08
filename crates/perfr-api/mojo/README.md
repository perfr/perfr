# perfr for Mojo

Manual instrumentation for Mojo. `perfr/__init__.mojo` loads `libperfr`
with `OwnedDLHandle` (the std.ffi owner of a `dlopen` handle; earlier Mojo
docs call this `DLHandle`) and uses `external_call` for libc (`getenv`,
`access`, `pthread_setname_np`).

```mojo
from perfr import Perfr, name_thread

def main() raises:
    var perfr = Perfr()
    var rc = perfr.init()
    _ = name_thread(String("sim"))
    with perfr.scope(String("frame")) as handle:
        _ = handle
        _ = perfr.value(String("fps"), 60.0)
```

`with` uses `__enter__` / `__exit__`. `__exit__` opens the library path
again and calls `perfr_stop`, so the scope does not borrow the handle
across the block. `name_thread` sets the OS thread name. `libperfr` has
no thread-name call. Linux keeps 15 bytes.

`init` returns `0`, `-1000` (no library), or `-1001` (wrong ABI). Until it
succeeds, calls return immediately. The search checks `$PERFR_LIB` (exclusive),
then `~/.local/bin` and `~/.perfr/bin`, then `libperfr.so` on the loader
path. It does not walk `PATH` for the `perfr` binary; that part of the C
search is not in this file.

## What was tested

Nothing. The Mojo compiler was not installed in this environment, and the
Linux CI image used for the other bindings does not have it either. The
workflow runs `mojo build` on `sample.mojo` when `mojo` is on `PATH` and
otherwise prints that it skipped. Treat the package as unchecked source
written against the std.ffi manual (`OwnedDLHandle.get_function` parameterized
by return type, `external_call` parameterized by symbol and return type).

To try it locally, from this directory, with `mojo` on `PATH` and
`PERFR_LIB` pointing at `libperfr.so`:

```sh
mojo build sample.mojo
# or
mojo run sample.mojo
```

If that fails, the compiler's FFI surface has moved. The file's module
docstring names the calls it expects.
