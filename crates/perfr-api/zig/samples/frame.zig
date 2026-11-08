// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

const std = @import("std");
const perfr = @import("perfr");

pub fn main() void {
    const rc = perfr.init();
    _ = perfr.nameThread("perfr-zig");
    std.debug.print("perfr zig init={d} library={?s}\n", .{ rc, perfr.libraryPath() });
    const frame = perfr.scope("frame");
    defer frame.end();
    _ = perfr.instant("vsync");
    const update = perfr.scope("update");
    update.end();
    perfr.value("fps", 60);
    const now = perfr.nowNs();
    if (now != 0) perfr.spanAsync("gpu: shadow pass", now - 2_000_000, now - 500_000);
    perfr.shutdown();
}
