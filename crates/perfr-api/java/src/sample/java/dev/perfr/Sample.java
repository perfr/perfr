/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr;

public final class Sample {
    public static void main(String[] args) {
        int rc = Perfr.init();
        Perfr.nameThread("perfr-java");
        System.out.println("sample init=" + rc + " library=" + Perfr.libraryPath());
        try (Perfr.Scope frame = Perfr.scope("frame")) {
            Perfr.instant("vsync");
            try (Perfr.Scope ignored = Perfr.scope("update")) { }
            Perfr.value("fps", 60);
            long now = Perfr.nowNs();
            if (now != 0) Perfr.spanAsync("gpu: shadow pass", now - 2_000_000, now - 500_000);
        }
        Perfr.shutdown();
    }
}
