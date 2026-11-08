// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import Foundation
import Perfr

let rc = Perfr.initialize()
_ = Perfr.nameThread("perfr-swift")
let path = Perfr.libraryPath ?? "(none)"
print("perfr swift init=\(rc) library=\(path)")
Perfr.withScope("frame") {
    _ = Perfr.instant("vsync")
    Perfr.withScope("update") {}
    Perfr.value("fps", 60)
    let now = Perfr.nowNs()
    if now != 0 {
        Perfr.spanAsync("gpu: shadow pass", startNs: now - 2_000_000, endNs: now - 500_000)
    }
}
Perfr.shutdown()
