/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr

/** `scope("frame") { ... }` — the Kotlin shape of a Java try-with-resources. */
inline fun <R> scope(name: String, block: () -> R): R {
    val opened = Perfr.scope(name)
    try {
        return block()
    } finally {
        opened.close()
    }
}

inline fun <R> scopeAsync(name: String, block: () -> R): R {
    val opened = Perfr.scopeAsync(name)
    try {
        return block()
    } finally {
        opened.close()
    }
}
