/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr

fun main() {
    val configured = System.getenv("PERFR_LIB") ?: ""
    check(configured.isNotEmpty()) { "set PERFR_LIB" }
    val loaded = java.nio.file.Files.exists(java.nio.file.Path.of(configured))
    if (loaded) {
        check(Perfr.init() == 0) { "init" }
        check(Perfr.available())
    }
    val value = scope("kotlin-scope") { 7 }
    check(value == 7)
    val asyncValue = scopeAsync("kotlin-async") { 8 }
    check(asyncValue == 8)
    Perfr.scope("kotlin-use").use { opened ->
        // Nobody is capturing, so the handle is zero even when libperfr is loaded.
        check(opened.handle() == 0L)
    }
    if (!loaded) check(Perfr.init() == Perfr.E_NOLIB)
    println("perfr kotlin ok")
}
