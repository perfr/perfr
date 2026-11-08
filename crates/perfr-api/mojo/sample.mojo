# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from perfr import Perfr, name_thread


def main() raises:
    var perfr = Perfr()
    var rc = perfr.init()
    _ = name_thread(String("perfr-mojo"))
    print("perfr mojo init=", rc, "library=", perfr.library_path())
    with perfr.scope(String("frame")) as handle:
        _ = handle
        _ = perfr.instant(String("vsync"))
        _ = perfr.value(String("fps"), 60.0)
    perfr.shutdown()
