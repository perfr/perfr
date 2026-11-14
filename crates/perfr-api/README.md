# perfr-api

Manual instrumentation for [perfr](https://github.com/perfr/perfr):
scopes, async spans, instants, links and values that a running
`perfr` draws on its live timeline.

```rust
let _update = perfr_api::scope("update");
perfr_api::value("entities", 1024.0);
perfr_api::log_info("tick");
perfr_api::frame_mark(); // once per turn of the loop
```

`frame_mark` says where one frame ends and the next begins. The viewer draws
the time between two marks as one frame: a bar in a strip of frame times
that always shows the whole capture, where a click on a bar shows that frame
in the timeline, and a band behind this process's tracks, every other frame
a little darker. `frame_mark_named("physics")` marks a series of its own for a
program with more than one loop. In C it is `perfr_frame_mark(name, len)`,
`PERFR_FRAME_MARK()` and `PERFR_FRAME_MARK_NAMED("physics")`; in Python
`perfr.frame_mark()`.

`log` is a no-op until a service is capturing, then the line is another event
on the same ring as scopes. A line carries its level, thread and timestamp.
The viewer files it under every scope open on that thread at that time, so a
scope is the way to group lines: click `frame` to read what was logged in it.

Every call is safe from any thread at any time and is a single predictable
branch until a service is capturing the process. The producer is a lock-free
shared-memory ring: one `fetch_add`, one compare-exchange, a few plain stores
and one release store, about twenty nanoseconds with the clock read.

This crate is also the one implementation behind every other language:

- **C and C++** include `include/perfr.h`, a single header that links nothing
  and loads `libperfr` at run time (the shared library `crates/perfr-lib`
  builds from this crate). The installers copy it to `~/.local/include`
  (`-I~/.local/include`; the sibling `include` of `PERFR_INSTALL_DIR` when
  that is set) or `<install dir>\include` on Windows. It still finds
  `libperfr` beside `perfr` on `PATH`.
- **Python** uses the `perfr-api` package on PyPI, pure Python over the same
  library.
- **C#** is `csharp/`, package id `Perfr`. **Lua** is `lua/` (LuaJIT FFI and
  a Lua 5.4 module). **Zig** is `zig/`. **Swift** is `swift/`. **Java and
  Kotlin** are `java/` (JNI plus a Kotlin `scope` block). **Julia** is
  `julia/`. **Mojo** is `mojo/` and was not compiled here. **GDScript** is
  a design note in `gdscript/`, not an extension. Each directory's README
  says what was run on Linux.

See the header for the C API and its search order, and the project manual
for what reaches the timeline.
