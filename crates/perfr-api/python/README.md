# perfr-api

perfr manual instrumentation for Python. Pure Python, no dependencies.

```python
import perfr_api as perfr

perfr.init()                      # finds libperfr, or stays quiet

with perfr.scope("update"):
    ...

@perfr.scope                      # or @perfr.scope("a name")
def render(frame):
    ...

perfr.value("fps", 59.9)
perfr.instant("level loaded")
perfr.log(perfr.INFO, "tick")
perfr.frame_mark()                # once per turn of the loop
```

Run `perfr`, open its viewer, pick the Python process, press Record.

## How it finds the library

The producer is the same `libperfr` that C and C++ load through
`perfr.h`. The package looks, in order, at `$PERFR_LIB`, beside the
`perfr` on `PATH` (then `~/.local/bin`, `~/.perfr/bin`), the copy
bundled in this wheel, then the system loader. The service comes first
because it is the authority on the ring protocol version: the library that
writes must match the service that reads, and a running service's own
library is the one guaranteed to. Without any library every call is a no-op
and `init()` returns `perfr_api.E_NOLIB`.

## API

`init() -> int`, `shutdown()`, `available()`, `library_path()`,
`start(name) -> handle`, `start_async(name)`, `stop(handle)`,
`instant(name) -> handle`, `link(src, dst)`, `value(name, v)`,
`now_ns()`, `span(name, start_ns, end_ns)`, `span_async(...)`,
`log(level, message)` with `TRACE` .. `FATAL`, `frame_mark(name="")`,
`scope(name)` and `scope_async(name)` as context managers or decorators.

`frame_mark()` says where one frame ends and the next begins; the viewer
draws the frames as a strip of frame times and as bands behind this
process's tracks. A
name marks a series of its own, for a program with more than one loop. With
a `libperfr` from before frame marks, or before logging, that call is a
no-op and the rest work.

Names may be `str` or `bytes`; `bytes` skips an encode on the hot path.
Handles are ints; `0` means "no event" and every function accepts it.
