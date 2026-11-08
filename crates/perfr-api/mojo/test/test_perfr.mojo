# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Self-checking main. Exit 0 on success. PERFR_LIB must be set: a missing
# file checks the no-op contract, an existing one checks a real load.

from std.ffi import c_char, c_int, external_call
from std.sys import exit

from perfr import E_NOLIB, Perfr, name_thread


def expect(cond: Bool, msg: String):
    if not cond:
        print("perfr mojo test:", msg)
        exit(1)


def env(name: String) -> String:
    var key = String(name)
    var found = external_call[
        "getenv", Optional[Pointer[c_char, MutAnyOrigin]]
    ](key.as_c_string_slice())
    if not found:
        return String("")
    return String(unsafe_from_utf8_ptr=found.value())


def exists(path: String) -> Bool:
    var text = String(path)
    return external_call["access", c_int](text.as_c_string_slice(), c_int(0)) == 0


def main() raises:
    var configured = env(String("PERFR_LIB"))
    expect(configured != "", String("set PERFR_LIB"))
    expect(name_thread(String("perfr-mojo")), String("name_thread"))

    var perfr = Perfr()
    expect(not perfr.available(), String("available before init"))
    expect(perfr.start(String("quiet")) == 0, String("start before init"))
    expect(perfr.now_ns() == 0, String("now before init"))
    perfr.stop(0)
    perfr.value(String("quiet"), 1.0)
    with perfr.scope(String("quiet")) as handle:
        expect(handle == 0, String("scope before init"))

    if not exists(configured):
        var rc = perfr.init()
        expect(rc == E_NOLIB, String("init without library: ") + String(rc))
        expect(not perfr.available(), String("available after miss"))
        expect(perfr.start(String("still")) == 0, String("start after miss"))
        print("perfr mojo ok (no library)")
        return

    expect(perfr.init() == 0, String("init"))
    expect(perfr.init() == 0, String("init again"))
    expect(perfr.available(), String("available"))
    expect(perfr.library_path() == configured, String("library_path ") + perfr.library_path())
    var t0 = perfr.now_ns()
    expect(t0 > 0, String("now_ns"))
    expect(perfr.now_ns() >= t0, String("now_ns advances"))
    # Nobody is capturing, so handles stay zero even with the library loaded.
    expect(perfr.start(String("before-capture")) == 0, String("start while idle"))
    with perfr.scope(String("idle")) as handle:
        expect(handle == 0, String("scope while idle"))
        _ = perfr.instant(String("vsync"))
        perfr.value(String("fps"), 60.0)
    var now = perfr.now_ns()
    perfr.span(String("span"), now - 1000, now)
    perfr.span_async(String("gpu"), now - 5000, now - 1000)
    perfr.shutdown()
    print("perfr mojo ok")
