# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""perfr manual instrumentation for Mojo.

Loads libperfr at run time. `OwnedDLHandle` is the std.ffi owner of a
dlopen handle (earlier Mojo docs call this `DLHandle`). `external_call`
is used for libc: getenv, access, readlink, and pthread_setname_np.

This file targets the std.ffi surface described in Mojo's C-FFI manual
(OwnedDLHandle.get_function parameterized by return type, external_call
parameterized by symbol and return type, `with` via __enter__/__exit__).
It was not compiled in this tree: the Mojo toolchain was not installed.
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

alias E_NOLIB = -1000
alias E_ABI = -1001
alias ABI_VERSION = 1


fn _env(name: String) -> String:
    var key = String(name)
    var found = external_call[
        "getenv", Optional[Pointer[c_char, MutUntrackedOrigin]]
    ](key.as_c_string_span())
    if not found:
        return String("")
    return String(unsafe_from_utf8_ptr=found.value())


fn _exists(path: String) -> Bool:
    var text = String(path)
    return external_call["access", c_int](text.as_c_string_span(), c_int(0)) == 0


struct Scope:
    var handle: UInt64
    var lib_path: String

    def __init__(out self, handle: UInt64, lib_path: String):
        self.handle = handle
        self.lib_path = lib_path

    def __enter__(self) -> UInt64:
        return self.handle

    def __exit__(self):
        if self.handle == 0 or self.lib_path == "":
            return
        try:
            var lib = OwnedDLHandle(self.lib_path)
            var stop = lib.get_function[NoneType]("perfr_stop")
            stop(self.handle)
        except:
            pass


struct Perfr:
    var lib: OwnedDLHandle
    var ready: Bool
    var path: String
    var status: Int

    def __init__(out self) raises:
        # A handle to the current process so the field is always constructed.
        # Real loads replace it. Symbols are only called when `ready` is set.
        self.lib = OwnedDLHandle()
        self.ready = False
        self.path = String("")
        self.status = E_NOLIB

    def init(mut self) raises -> Int:
        if self.ready:
            var again = self.lib.get_function[c_int]("perfr_init")
            var rc = Int(again())
            self.status = rc
            return rc
        var found = self._find()
        if found == "":
            self.status = E_NOLIB
            return E_NOLIB
        try:
            var loaded = OwnedDLHandle(found)
            if not loaded.check_symbol("perfr_api_table_v1"):
                self.status = E_ABI
                return E_ABI
            var get = loaded.get_function[Optional[Pointer[UInt32, MutUntrackedOrigin]]](
                "perfr_api_table_v1"
            )
            var table = get(UInt32(ABI_VERSION))
            if not table:
                self.status = E_ABI
                return E_ABI
            var head = table.value()
            if head[unsafe_offset=0] != UInt32(ABI_VERSION) or head[unsafe_offset=1] < 104:
                self.status = E_ABI
                return E_ABI
            self.lib = loaded
            self.ready = True
            self.path = found
        except:
            self.status = E_NOLIB
            return E_NOLIB
        var start_fn = self.lib.get_function[c_int]("perfr_init")
        var rc = Int(start_fn())
        self.status = rc
        return rc

    def shutdown(mut self) raises:
        if not self.ready:
            return
        var fn = self.lib.get_function[NoneType]("perfr_shutdown")
        fn()

    def available(self) -> Bool:
        return self.ready

    def library_path(self) -> String:
        return self.path

    def start(mut self, name: String) raises -> UInt64:
        return self._named[UInt64]("perfr_start", name)

    def start_async(mut self, name: String) raises -> UInt64:
        return self._named[UInt64]("perfr_start_async", name)

    def stop(mut self, handle: UInt64) raises:
        if not self.ready or handle == 0:
            return
        var fn = self.lib.get_function[NoneType]("perfr_stop")
        fn(handle)

    def instant(mut self, name: String) raises -> UInt64:
        return self._named[UInt64]("perfr_instant", name)

    def link(mut self, src: UInt64, dst: UInt64) raises:
        if not self.ready or src == 0 or dst == 0:
            return
        var fn = self.lib.get_function[NoneType]("perfr_link")
        fn(src, dst)

    def value(mut self, name: String, sample: Float64) raises:
        if not self.ready:
            return
        var fn = self.lib.get_function[NoneType]("perfr_value")
        var text = String(name)
        var n = text.byte_length()
        fn(text.as_c_string_span(), c_size_t(n), c_double(sample))

    def now_ns(mut self) raises -> UInt64:
        if not self.ready:
            return 0
        var fn = self.lib.get_function[UInt64]("perfr_now_ns")
        return fn()

    def span(mut self, name: String, start_ns: UInt64, end_ns: UInt64) raises:
        self._span("perfr_span", name, start_ns, end_ns)

    def span_async(mut self, name: String, start_ns: UInt64, end_ns: UInt64) raises:
        self._span("perfr_span_async", name, start_ns, end_ns)

    def scope(mut self, name: String) raises -> Scope:
        var handle = self.start(name)
        var owned = String(self.path)
        return Scope(handle, owned)

    def _named[R: AnyType](mut self, symbol: String, name: String) raises -> R:
        if not self.ready:
            return rebind[R](0)
        var fn = self.lib.get_function[R](symbol)
        var text = String(name)
        var n = text.byte_length()
        return fn(text.as_c_string_span(), c_size_t(n))

    def _span(mut self, symbol: String, name: String, start_ns: UInt64, end_ns: UInt64) raises:
        if not self.ready:
            return
        var fn = self.lib.get_function[NoneType](symbol)
        var text = String(name)
        var n = text.byte_length()
        fn(text.as_c_string_span(), c_size_t(n), start_ns, end_ns)

    def _find(self) -> String:
        var forced = _env(String("PERFR_LIB"))
        if forced != "":
            if _exists(forced):
                return forced
            return String("")
        var home = _env(String("HOME"))
        if home != "":
            var local = home + "/.local/bin/" + LIB_FILE
            if _exists(local):
                return local
            var dot = home + "/.perfr/bin/" + LIB_FILE
            if _exists(dot):
                return dot
        if _exists(String(LIB_FILE)):
            return String(LIB_FILE)
        return String("")


fn name_thread(name: String) raises -> Bool:
    """OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes."""
    var text = String(name)
    if text.byte_length() > 15:
        text = String(text.as_bytes()[:15])
    var bytes = text.as_c_string_span()
    comptime if CompilationTarget.is_linux():
        var tid = external_call["pthread_self", c_ulong]()
        return external_call["pthread_setname_np", c_int](tid, bytes) == 0
    else:
        return external_call["pthread_setname_np", c_int](bytes) == 0
