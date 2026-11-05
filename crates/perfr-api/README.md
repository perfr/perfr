# perfr-api

Manual instrumentation for [perfr](https://github.com/perfr/perfr):
scopes, async spans, instants, links and values that a running
`perfr` draws on its live timeline.

```rust
let _frame = perfr_api::scope("frame");
perfr_api::value("fps", 59.9);
```

Every call is safe from any thread at any time and is a single predictable
branch until a service is capturing the process. The producer is a lock-free
shared-memory ring: one `fetch_add`, a few plain stores and one release store,
about fifteen nanoseconds with the clock read.

This crate is also the one implementation behind every other language:

- **C and C++** include `include/perfr.h`, a single header that links nothing
  and loads `libperfr` at run time (the shared library `crates/perfr-lib`
  builds from this crate). The installers copy it to `~/.local/include`
  (`-I~/.local/include`; the sibling `include` of `PERFR_INSTALL_DIR` when
  that is set) or `<install dir>\include` on Windows. It still finds
  `libperfr` beside `perfr` on `PATH`.
- **Python** uses the `perfr-api` package on PyPI, pure Python over the same
  library.

See the header for the C API and its search order, and the project manual
for what reaches the timeline.
