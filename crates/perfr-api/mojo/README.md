# perfr for Mojo

Manual instrumentation for Mojo 1.0. `perfr/__init__.mojo` loads `libperfr`
with `OwnedDLHandle` (std.ffi's owner of a `dlopen` handle) and reaches libc
(`getenv`, `access`, `pthread_setname_np`) with `external_call`.

```mojo
from perfr import Perfr, name_thread

def main() raises:
    var perfr = Perfr()
    var rc = perfr.init()
    _ = name_thread(String("sim"))
    with perfr.scope(String("frame")) as handle:
        _ = handle
        perfr.value(String("fps"), 60.0)
```

`with` uses `__enter__` / `__exit__`. The scope points at the `Perfr` it
came from and calls `stop` when the block ends, so `perfr` must not move
inside the block. `name_thread` sets the OS thread name. `libperfr` has no
thread-name call. Linux keeps 15 bytes.

`init` returns `0`, `-1000` (`E_NOLIB`), `-1001` (`E_ABI`), or a negative
errno. Until it succeeds, calls return immediately. The search is the one in
`perfr.h`: `$PERFR_LIB` (exclusive), beside a `perfr` on `PATH`, then
`~/.local/bin` and `~/.perfr/bin`, then the loader's own search for
`libperfr.so`.

Each call resolves its entry point with `get_function`, that is a `dlsym`
per call. Mojo 1.0 has no way to keep a function pointer from a `dlsym`
result in a struct field (`get_function`'s callable borrows the handle, and
a raw symbol cannot be cast to a function type), so this binding is slower
per scope than the C header. It is correct, and fine for frame-level scopes.

## Build and test

```sh
pip install mojo          # or any Mojo 1.0 install
./test.sh /path/to/libperfr.so
```

`test.sh` builds `sample.mojo` and `test/test_perfr.mojo`, runs the test
with a missing library and then with the real one, and exits non-zero when
`mojo` is not on `PATH`.

## What was tested

On Linux x86-64 with Mojo 1.0.0 and `libperfr.so` from
`cargo build -p perfr-lib`:

- The package and the sample compile without errors.
- A missing `PERFR_LIB`: `init` returns `E_NOLIB` and calls are no-ops.
- With the library: `init` returns 0 twice, `library_path` is the configured
  path, `now_ns` advances, and handles stay zero while nobody is capturing.
- With `PERFR_LIB` unset and a directory holding `perfr` and `libperfr.so`
  first on `PATH`, `init` finds that copy.
- `name_thread` returns true.

The test does not poke the segment's capturing word the way the other
bindings' tests do. macOS was not run.
