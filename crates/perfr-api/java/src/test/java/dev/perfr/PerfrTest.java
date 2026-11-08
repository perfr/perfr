/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

package dev.perfr;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/** Self-checking main. Exit 0 on success. No JUnit, so the tree stays free of jars. */
public final class PerfrTest {
    private static final long MAGIC = 0x4F524249545F5347L;

    public static void main(String[] args) throws Exception {
        String configured = System.getenv("PERFR_LIB");
        if (configured == null || configured.isEmpty()) fail("set PERFR_LIB");
        if (!Perfr.shimLoaded()) {
            withoutShim();
            System.out.println("perfr java ok (no shim)");
            return;
        }
        expect(Perfr.nameThread("perfr-java"), "nameThread");
        expect("perfr-java".equals(comm()), "comm " + comm());
        expect(Perfr.nameThread("0123456789abcdefEXTRA"), "long name");
        expect("0123456789abcde".equals(comm()), "truncated " + comm());
        if (Files.exists(Path.of(configured))) withLibrary(configured);
        else withoutLibrary();
        System.out.println("perfr java ok");
    }

    /** PERFR_JNI names a file that does not exist: nothing may throw. */
    private static void withoutShim() {
        expect(Perfr.init() == Perfr.E_NOLIB, "init without shim " + Perfr.init());
        expect(!Perfr.available(), "available without shim");
        expect(Perfr.libraryPath() == null, "path without shim");
        expect(Perfr.start("quiet") == 0, "start without shim");
        expect(Perfr.nowNs() == 0, "now without shim");
        try (Perfr.Scope ignored = Perfr.scope("quiet")) { }
        Perfr.stop(1);
        Perfr.link(1, 2);
        Perfr.value("quiet", 1);
        Perfr.span("quiet", 1, 2);
        expect(!Perfr.nameThread("quiet"), "nameThread without shim");
        Perfr.shutdown();
    }

    private static void withoutLibrary() {
        expect(Perfr.start("quiet") == 0, "start");
        expect(Perfr.nowNs() == 0, "now");
        try (Perfr.Scope ignored = Perfr.scope("quiet")) { }
        Perfr.stop(0);
        Perfr.value("quiet", 1);
        expect(Perfr.init() == Perfr.E_NOLIB, "init " + Perfr.init());
        expect(!Perfr.available(), "available");
        expect(Perfr.start("still") == 0, "still");
    }

    private static void withLibrary(String expected) throws IOException {
        expect(Perfr.init() == 0, "init");
        expect(Perfr.init() == 0, "init again");
        expect(Perfr.available(), "available");
        String path = Perfr.libraryPath();
        expect(path != null && path.contains("libperfr"), "path " + path);
        long t0 = Perfr.nowNs();
        expect(t0 > 0, "now");
        expect(Perfr.start("before-capture") == 0, "idle");

        long pid = ProcessHandle.current().pid();
        Path segment = Path.of("/dev/shm/perfr-scopes-" + pid);
        expect(Files.exists(segment), segment.toString());
        // FileChannel does not flock, so it can open the segment the producer holds.
        try (FileChannel channel = FileChannel.open(segment, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            ByteBuffer head = ByteBuffer.allocate(40).order(ByteOrder.LITTLE_ENDIAN);
            expect(channel.read(head) == 40, "head");
            head.flip();
            expect(head.getLong() == MAGIC, "magic");
            head.position(8);
            expect(head.getInt() == 2, "version");
            head.position(24);
            expect(head.getInt() == (int) pid, "pid");
            head.position(28);
            expect(head.getInt() == 0, "capturing");
            writeU32(channel, 28, 1);

            try (Perfr.Scope frame = Perfr.scope("java-probe")) {
                expect(frame.handle() != 0, "handle");
                Perfr.value("java-fps", 59.9);
                long mark = Perfr.instant("java-mark");
                expect(mark != 0, "instant");
                Perfr.link(mark, frame.handle());
            }
            int returned = scopeReturned();
            expect(returned == 7, "kotlin-less java scope");
            long now = Perfr.nowNs();
            Perfr.span("java-span", now - 1000, now);
            Perfr.spanAsync("java-gpu", now - 5000, now - 1000);

            byte[] all = new byte[(int) channel.size()];
            ByteBuffer buf = ByteBuffer.wrap(all);
            channel.position(0);
            while (buf.hasRemaining()) {
                if (channel.read(buf) < 0) break;
            }
            expect(indexOf(all, "java-probe".getBytes(StandardCharsets.UTF_8)) >= 0, "probe bytes");
            expect(indexOf(all, "java-fps".getBytes(StandardCharsets.UTF_8)) >= 0, "fps bytes");
            writeU32(channel, 28, 0);
        }
        expect(Perfr.start("after") == 0, "idle again");
        Perfr.shutdown();
        expect(!Files.exists(segment), "unlinked");
    }

    private static int scopeReturned() {
        try (Perfr.Scope ignored = Perfr.scope("java-with")) {
            return 7;
        }
    }

    private static void writeU32(FileChannel channel, long offset, int value) throws IOException {
        ByteBuffer buf = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(value);
        buf.flip();
        expect(channel.write(buf, offset) == 4, "pwrite");
    }

    private static int indexOf(byte[] haystack, byte[] needle) {
        for (int i = 0; i + needle.length <= haystack.length; i++) {
            boolean match = true;
            for (int j = 0; j < needle.length; j++) {
                if (haystack[i + j] != needle[j]) { match = false; break; }
            }
            if (match) return i;
        }
        return -1;
    }

    private static String comm() throws IOException {
        return Files.readString(Path.of("/proc/thread-self/comm")).trim();
    }

    private static void expect(boolean cond, String msg) {
        if (!cond) fail(msg);
    }

    private static void fail(String msg) {
        System.err.println("perfr java test: " + msg);
        System.exit(1);
    }
}
