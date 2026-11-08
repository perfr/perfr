# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""perfr manual instrumentation for Mojo.

Loads libperfr at run time with `OwnedDLHandle` (std.ffi's owner of a dlopen
handle) and the search order of perfr.h, then calls its entry points through
`get_function`. libc (`getenv`, `access`, `pthread_setname_np`) is reached
with `external_call`. Written for and compiled with Mojo 1.0.

```mojo
from perfr import Perfr, name_thread

def main() raises:
    var perfr = Perfr()
    _ = perfr.init()
    with perfr.scope(String("frame")) as handle:
        _ = handle
        perfr.value(String("fps"), 60.0)
```

Every call is a no-op until `init` finds a library, and the library writes
nothing unless a service is capturing. Handles are `UInt64`; zero means
"no event".
"""

from std.ffi import (
    OwnedDLHandle,
    c_char,
    c_double,
    c_int,
    c_size_t,
    c_ulong,
    external_call,
)
from std.sys.info import CompilationTarget, platform_map

comptime LIB_FILE = platform_map[
    "libperfr.so", linux="libperfr.so", macos="libperfr.dylib"
]()
comptime SERVICE = platform_map["perfr", linux="perfr", macos="perfr"]()

comptime E_NOLIB = -1000
comptime E_ABI = -1001
comptime ABI_VERSION = 1

# The table's `size` for a 64-bit target: two UInt32 then twelve pointers.
comptime TABLE_SIZE = 8 + 12 * 8


# libc wants NUL-terminated names. `as_c_string_slice` appends the NUL in
# place, so these two take a copy. libperfr takes pointer and length, so the
# instrumentation calls below pass `as_bytes()` of the caller's string as is.
def _env(name: String) -> String:
    var key = String(name)
    var found = external_call[
        "getenv", Optional[Pointer[c_char, MutAnyOrigin]]
    ](key.as_c_string_slice())
    if not found:
        return String("")
    return String(unsafe_from_utf8_ptr=found.value())


def _exists(path: String) -> Bool:
    var text = String(path)
    return external_call["access", c_int](text.as_c_string_slice(), c_int(0)) == 0


struct Scope[origin: MutOrigin]:
    """A scope that stops when the `with` block ends.

    `with perfr.scope(name) as handle:` yields the handle, which may be
    passed to `link`. The block must not move `perfr`: the scope points at
    it to call `stop`.
    """

    var owner: Pointer[Perfr, Self.origin]
    var handle: UInt64

    def __init__(out self, owner: Pointer[Perfr, Self.origin], handle: UInt64):
        self.owner = owner
        self.handle = handle

    def __enter__(self) -> UInt64:
        return self.handle

    def __exit__(self):
        if self.handle == 0:
            return
        try:
            self.owner[].stop(self.handle)
        except:
            pass


struct Perfr:
    """The loaded library, or nothing. One per process is the intent."""

    var lib: Optional[OwnedDLHandle]
    var path: String
    var status: Int

    def __init__(out self):
        self.lib = None
        self.path = String("")
        self.status = E_NOLIB

    def init(mut self) raises -> Int:
        """Finds and loads libperfr, then creates this process's segment.

        Idempotent. Returns 0, E_NOLIB, E_ABI, or a negative errno. Calls stay
        valid no-ops in every failure case.
        """
        if not self.lib:
            var found = self._find()
            if found == "":
                self.status = E_NOLIB
                return E_NOLIB
            try:
                var loaded = OwnedDLHandle(found)
                # A library without the getter speaks an older ABI by definition.
                if not loaded.check_symbol("perfr_api_table_v1"):
                    self.status = E_ABI
                    return E_ABI
                var get = loaded.get_function[
                    Optional[Pointer[UInt32, MutAnyOrigin]]
                ]("perfr_api_table_v1")
                var table = get(UInt32(ABI_VERSION))
                if not table:
                    self.status = E_ABI
                    return E_ABI
                var head = table.value()
                if (
                    head[unsafe_offset=0] != UInt32(ABI_VERSION)
                    or head[unsafe_offset=1] < UInt32(TABLE_SIZE)
                ):
                    self.status = E_ABI
                    return E_ABI
                self.lib = loaded^
                self.path = found
            except:
                self.status = E_NOLIB
                return E_NOLIB
        var rc = Int(self.lib.value().get_function[c_int]("perfr_init")())
        self.status = rc
        return rc

    def shutdown(mut self) raises:
        """Removes the segment's name. The library stays loaded."""
        if not self.lib:
            return
        self.lib.value().get_function[NoneType]("perfr_shutdown")()

    def available(self) -> Bool:
        """Whether `init` loaded a library."""
        return Bool(self.lib)

    def library_path(self) -> String:
        """The library `init` loaded, or an empty string."""
        return self.path

    def start(mut self, name: String) raises -> UInt64:
        """Begins a scope on the calling thread. Returns its handle."""
        return self._named("perfr_start", name)

    def start_async(mut self, name: String) raises -> UInt64:
        """Begins a scope that may be stopped from any thread, on its own track."""
        return self._named("perfr_start_async", name)

    def stop(mut self, handle: UInt64) raises:
        """Ends a scope from `start` or `start_async`. Zero is a no-op."""
        if not self.lib or handle == 0:
            return
        self.lib.value().get_function[NoneType]("perfr_stop")(handle)

    def instant(mut self, name: String) raises -> UInt64:
        """A point in time with a name and no duration."""
        return self._named("perfr_instant", name)

    def link(mut self, src: UInt64, dst: UInt64) raises:
        """An arrow from one event to another. Either handle being zero is a no-op."""
        if not self.lib or src == 0 or dst == 0:
            return
        self.lib.value().get_function[NoneType]("perfr_link")(src, dst)

    def value(mut self, name: String, sample: Float64) raises:
        """A value to graph over time, on a track named `name`."""
        if not self.lib:
            return
        self.lib.value().get_function[NoneType]("perfr_value")(
            name.as_bytes().unsafe_ptr(), c_size_t(name.byte_length()), c_double(sample)
        )

    def now_ns(mut self) raises -> UInt64:
        """CLOCK_MONOTONIC nanoseconds, the clock of every timestamp. 0 while unloaded."""
        if not self.lib:
            return 0
        return self.lib.value().get_function[UInt64]("perfr_now_ns")()

    def span(mut self, name: String, start_ns: UInt64, end_ns: UInt64) raises:
        """A complete scope at timestamps from `now_ns`."""
        self._span("perfr_span", name, start_ns, end_ns)

    def span_async(mut self, name: String, start_ns: UInt64, end_ns: UInt64) raises:
        """A complete async span at supplied timestamps, on its own track."""
        self._span("perfr_span_async", name, start_ns, end_ns)

    def scope(mut self, name: String) raises -> Scope[origin_of(self)]:
        """`with perfr.scope(name) as handle:` stops the scope after the block."""
        var handle = self.start(name)
        return Scope(Pointer(to=self), handle)

    def _named(mut self, symbol: String, name: String) raises -> UInt64:
        if not self.lib:
            return 0
        return self.lib.value().get_function[UInt64](symbol)(
            name.as_bytes().unsafe_ptr(), c_size_t(name.byte_length())
        )

    def _span(
        mut self, symbol: String, name: String, start_ns: UInt64, end_ns: UInt64
    ) raises:
        if not self.lib:
            return
        self.lib.value().get_function[NoneType](symbol)(
            name.as_bytes().unsafe_ptr(), c_size_t(name.byte_length()), start_ns, end_ns
        )

    def _find(self) -> String:
        """The search order of perfr.h: $PERFR_LIB (exclusive), beside a perfr
        on PATH, ~/.local/bin and ~/.perfr/bin, then the loader's own search."""
        var forced = _env(String("PERFR_LIB"))
        if forced != "":
            if _exists(forced):
                return forced
            return String("")
        var path = _env(String("PATH"))
        if path != "":
            for dir in path.split(":"):
                if dir.byte_length() == 0:
                    continue
                var prefix = String(dir) + "/"
                if _exists(prefix + SERVICE) and _exists(prefix + LIB_FILE):
                    return prefix + LIB_FILE
        var home = _env(String("HOME"))
        if home != "":
            var local = home + "/.local/bin/" + LIB_FILE
            if _exists(local):
                return local
            var dot = home + "/.perfr/bin/" + LIB_FILE
            if _exists(dot):
                return dot
        return String(LIB_FILE)


def name_thread(name: String) raises -> Bool:
    """OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes."""
    var text = String(name)
    if text.byte_length() > 15:
        var head = text.as_bytes()[:15]
        var cut = String(head)
        text = cut^
    var bytes = text.as_c_string_slice()
    comptime if CompilationTarget.is_linux():
        var tid = external_call["pthread_self", c_ulong]()
        return external_call["pthread_setname_np", c_int](tid, bytes) == 0
    else:
        return external_call["pthread_setname_np", c_int](bytes) == 0
