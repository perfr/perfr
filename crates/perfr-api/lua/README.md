# perfr for Lua

Manual instrumentation for Lua. `require("perfr")` picks the implementation:

- **LuaJIT** loads `perfr_ffi.lua` and calls `libperfr` through the FFI.
- **Lua 5.4** loads `perfr_native.so`, a C module that includes `perfr.h`
  and does not link `libperfr`. The header loads the library at run time.

```lua
local perfr = require("perfr")
perfr.init()
perfr.name_thread("sim")
perfr.with_scope("frame", function()
  perfr.value("fps", 59.9)
end)
```

On Lua 5.4 a scope is also a to-be-closed variable:

```lua
do
  local _ <close> = perfr.scope("frame")
end
```

LuaJIT has no `<close>`. Use `with_scope`, or `scope(name):stop()`.

`name_thread` sets the Linux thread `comm` (15 bytes). `libperfr` has no
thread-name call. perfr reads that name from `/proc`.

## Build and test

```sh
make
PERFR_LIB=/no/such/libperfr.so lua5.4 test/test_perfr.lua
PERFR_LIB=/path/to/libperfr.so lua5.4 test/test_perfr.lua
PERFR_LIB=/path/to/libperfr.so luajit test/test_perfr.lua
lua5.4 samples/sample.lua 3
```

`test.sh` runs both interpreters, first with a missing library and then
against `target/debug/libperfr.so` when that file exists. LuaJIT does not
need the `.so`; Lua 5.4 does.

The search order matches `perfr.h`. The FFI module implements it itself.
The C module calls `perfr_init` from the header, so the header does the
search. `$PERFR_LIB` is exclusive.

## What was tested

On Linux x86-64, against `libperfr.so` from `cargo build -p perfr-lib`:

- Lua 5.4.6 (`lua5.4`), including `<close>`, and LuaJIT 2.1.0
  (`luajit`, Ubuntu 2.1.0+git20231223).
- A missing `PERFR_LIB` returns `E_NOLIB` and every call is a no-op.
- With the library, `init` returns 0, `now_ns` advances, and a scope is
  written into `/dev/shm/perfr-scopes-<pid>` only after the segment's
  capturing word is set. `name_thread` shows up in `/proc/thread-self/comm`.
- This LuaJIT build has no `string.pack`; the test does not use it.
- macOS and Windows were not run. The C module's `name_thread` is compiled
  for Linux and Apple and returns false on Windows.

Handles from the C module are Lua integers (or a hex string if the high bit
is set). LuaJIT keeps them as `uint64_t` cdata so a large tid does not lose
bits in a double.
