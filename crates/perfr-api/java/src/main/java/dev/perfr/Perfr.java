/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr;

import java.nio.charset.StandardCharsets;

/**
 * Manual instrumentation for perfr. The JNI library includes {@code perfr.h},
 * which loads {@code libperfr} at run time. Calls are no-ops until
 * {@link #init()} finds that library, and the library writes nothing unless
 * a service is capturing. They are also no-ops when the JNI library itself
 * ({@code libperfr_jni}, found through {@code $PERFR_JNI} or
 * {@code java.library.path}) is missing: that is reported by {@link #init()}
 * as {@link #E_NOLIB}, not thrown.
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

    /**
     * Whether the JNI shim ({@code libperfr_jni}) loaded. When it did not,
     * every call here is a no-op and {@link #init()} returns {@link #E_NOLIB},
     * the same contract as a missing {@code libperfr}: shipping these classes
     * without the shim for a platform must not take the application down.
     */
    private static final boolean LOADED = loadShim();

    private static boolean loadShim() {
        try {
            String path = System.getenv("PERFR_JNI");
            if (path != null && !path.isEmpty()) System.load(path);
            else System.loadLibrary("perfr_jni");
            return true;
        } catch (UnsatisfiedLinkError | SecurityException e) {
            return false;
        }
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

    /** Whether the JNI shim is loaded. False means every call is a no-op. */
    public static boolean shimLoaded() { return LOADED; }

    public static int init() { return LOADED ? nativeInit() : E_NOLIB; }
    public static void shutdown() { if (LOADED) nativeShutdown(); }
    public static boolean available() { return LOADED && nativeAvailable(); }
    public static String libraryPath() { return LOADED ? nativeLibraryPath() : null; }

    public static long start(String name) { return LOADED ? nativeStart(utf8(name)) : 0; }
    public static long startAsync(String name) { return LOADED ? nativeStartAsync(utf8(name)) : 0; }
    public static void stop(long handle) { if (LOADED && handle != 0) nativeStop(handle); }
    public static long instant(String name) { return LOADED ? nativeInstant(utf8(name)) : 0; }
    public static void link(long from, long to) { if (LOADED && from != 0 && to != 0) nativeLink(from, to); }
    public static void value(String name, double sample) { if (LOADED) nativeValue(utf8(name), sample); }
    public static long nowNs() { return LOADED ? nativeNowNs() : 0; }
    public static void span(String name, long startNs, long endNs) { if (LOADED) nativeSpan(utf8(name), startNs, endNs); }
    public static void spanAsync(String name, long startNs, long endNs) { if (LOADED) nativeSpanAsync(utf8(name), startNs, endNs); }

    public static Scope scope(String name) { return new Scope(start(name)); }
    public static Scope scopeAsync(String name) { return new Scope(startAsync(name)); }

    /** OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes. */
    public static boolean nameThread(String name) { return LOADED && name != null && nativeNameThread(name); }

    private static byte[] utf8(String name) {
        if (name == null || name.isEmpty()) return new byte[0];
        return name.getBytes(StandardCharsets.UTF_8);
    }
}
