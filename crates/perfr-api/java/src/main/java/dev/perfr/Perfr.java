/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr;

import java.nio.charset.StandardCharsets;

/**
 * Manual instrumentation for perfr. The JNI library includes {@code perfr.h},
 * which loads {@code libperfr} at run time. Calls are no-ops until
 * {@link #init()} finds that library, and the library writes nothing unless
 * a service is capturing.
 *
 * <pre>
 * try (Perfr.Scope frame = Perfr.scope("frame")) {
 *     Perfr.value("fps", 59.9);
 * }
 * </pre>
 *
 * Handles are opaque {@code long}s. Zero means "no event". The bit pattern
 * round-trips through JNI even when the high bit is set.
 */
public final class Perfr {
    public static final int ABI_VERSION = 1;
    public static final int E_NOLIB = -1000;
    public static final int E_ABI = -1001;

    static {
        String path = System.getenv("PERFR_JNI");
        if (path != null && !path.isEmpty()) System.load(path);
        else System.loadLibrary("perfr_jni");
    }

    private Perfr() {}

    public static final class Scope implements AutoCloseable {
        private long handle;

        Scope(long handle) {
            this.handle = handle;
        }

        public long handle() {
            return handle;
        }

        @Override
        public void close() {
            long current = handle;
            handle = 0;
            if (current != 0) stop(current);
        }
    }

    public static native int nativeInit();
    public static native void nativeShutdown();
    public static native boolean nativeAvailable();
    public static native String nativeLibraryPath();
    private static native long nativeStart(byte[] utf8);
    private static native long nativeStartAsync(byte[] utf8);
    private static native void nativeStop(long handle);
    private static native long nativeInstant(byte[] utf8);
    private static native void nativeLink(long from, long to);
    private static native void nativeValue(byte[] utf8, double value);
    private static native long nativeNowNs();
    private static native void nativeSpan(byte[] utf8, long startNs, long endNs);
    private static native void nativeSpanAsync(byte[] utf8, long startNs, long endNs);
    public static native boolean nativeNameThread(String name);

    public static int init() { return nativeInit(); }
    public static void shutdown() { nativeShutdown(); }
    public static boolean available() { return nativeAvailable(); }
    public static String libraryPath() { return nativeLibraryPath(); }

    public static long start(String name) { return nativeStart(utf8(name)); }
    public static long startAsync(String name) { return nativeStartAsync(utf8(name)); }
    public static void stop(long handle) { if (handle != 0) nativeStop(handle); }
    public static long instant(String name) { return nativeInstant(utf8(name)); }
    public static void link(long from, long to) { if (from != 0 && to != 0) nativeLink(from, to); }
    public static void value(String name, double sample) { nativeValue(utf8(name), sample); }
    public static long nowNs() { return nativeNowNs(); }
    public static void span(String name, long startNs, long endNs) { nativeSpan(utf8(name), startNs, endNs); }
    public static void spanAsync(String name, long startNs, long endNs) { nativeSpanAsync(utf8(name), startNs, endNs); }

    public static Scope scope(String name) { return new Scope(start(name)); }
    public static Scope scopeAsync(String name) { return new Scope(startAsync(name)); }

    /** OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes. */
    public static boolean nameThread(String name) { return nativeNameThread(name); }

    private static byte[] utf8(String name) {
        if (name == null || name.isEmpty()) return new byte[0];
        return name.getBytes(StandardCharsets.UTF_8);
    }
}
