/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

using System;
using System.Buffers.Binary;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Xunit;

namespace Perfr.Tests
{
    public class PerfrTests
    {
        // The producer writes nothing until a reader sets the capturing word
        // in the segment header (magic, then version, pid, capturing at byte
        // 28). This is the layout of perfr-scope-ring's Header, probed here
        // so the test can see a record without a running service.
        const ulong Magic = 0x4F524249545F5347UL;

        [Fact]
        public void NameThreadSetsTheLinuxComm()
        {
            if (!RuntimeInformation.IsOSPlatform(OSPlatform.Linux))
            {
                Perfr.NameThread("perfr-cs");
                return;
            }

            Assert.True(Perfr.NameThread("perfr-cs"));
            Assert.Equal("perfr-cs", Comm());

            Assert.True(Perfr.NameThread("0123456789abcdefEXTRA"));
            Assert.Equal("0123456789abcde", Comm());
        }

        [Fact]
        public void CallsFollowTheLibrary()
        {
            var configured = Environment.GetEnvironmentVariable("PERFR_LIB");
            if (!string.IsNullOrEmpty(configured) && File.Exists(configured))
                WithLibrary(configured);
            else
                WithoutLibrary(configured);
        }

        static void WithoutLibrary(string? configured)
        {
            Assert.False(Perfr.Available);
            Assert.Equal(0UL, Perfr.Start("quiet"));
            Assert.Equal(0UL, Perfr.NowNs());
            using (Perfr.Scope("quiet")) { }
            Perfr.Stop(0);
            Perfr.Stop(1);
            Perfr.Link(0, 1);
            Perfr.Value("quiet", 1.0);
            Perfr.Span("quiet", 1, 2);
            Perfr.SpanAsync("quiet", 1, 2);

            if (string.IsNullOrEmpty(configured))
                Environment.SetEnvironmentVariable("PERFR_LIB", "/no/such/libperfr.so");

            Assert.Equal(Perfr.ENoLib, Perfr.Init());
            Assert.False(Perfr.Available);
            Assert.Null(Perfr.LibraryPath);
            Assert.Equal(0UL, Perfr.Start("still-quiet"));
            Assert.Equal(0UL, Perfr.Instant("still-quiet"));
            Assert.Equal(0UL, Perfr.NowNs());
        }

        static void WithLibrary(string path)
        {
            Assert.Equal(0, Perfr.Init());
            Assert.Equal(0, Perfr.Init());
            Assert.True(Perfr.Available);
            Assert.Equal(path, Perfr.LibraryPath);

            var t0 = Perfr.NowNs();
            Assert.True(t0 > 0);
            Assert.True(Perfr.NowNs() >= t0);
            Assert.Equal(0UL, Perfr.Start("before-capture"));
            using (Perfr.Scope("before-capture")) { }

            var segment = "/dev/shm/perfr-scopes-" + Environment.ProcessId;
            Assert.True(File.Exists(segment), segment);
            // The producer holds an exclusive flock, so FileStream's share
            // emulation cannot open the segment. open/pread can.
            var fd = libc_open(segment, 2);
            Assert.True(fd >= 0, segment);
            try
            {
                var head = new byte[40];
                Assert.Equal(head.Length, (int)libc_pread(fd, head, (ulong)head.Length, 0));
                Assert.Equal(Magic, BinaryPrimitives.ReadUInt64LittleEndian(head));
                Assert.Equal(2u, BinaryPrimitives.ReadUInt32LittleEndian(head.AsSpan(8)));
                Assert.Equal((uint)Environment.ProcessId, BinaryPrimitives.ReadUInt32LittleEndian(head.AsSpan(24)));
                Assert.Equal(0u, BinaryPrimitives.ReadUInt32LittleEndian(head.AsSpan(28)));

                WriteU32(fd, 28, 1);
                ulong handle;
                using (var scope = Perfr.Scope("csharp-probe"))
                {
                    handle = scope.Handle;
                    Assert.NotEqual(0UL, handle);
                    Perfr.Value("csharp-fps", 59.9);
                    var mark = Perfr.Instant("csharp-mark");
                    Assert.NotEqual(0UL, mark);
                    Perfr.Link(mark, handle);
                    var asyncHandle = Perfr.StartAsync("csharp-job");
                    Assert.NotEqual(0UL, asyncHandle);
                    Perfr.Stop(asyncHandle);
                }
                var now = Perfr.NowNs();
                Perfr.Span("csharp-span", now - 1000, now);
                Perfr.SpanAsync("csharp-gpu", now - 5000, now - 1000);
                Perfr.Start("csharp-\u03c0");

                var length = libc_lseek(fd, 0, 2);
                Assert.True(length > 0);
                var all = new byte[length];
                long got = 0;
                while (got < length)
                {
                    var n = PreadAt(fd, all, got, length - got);
                    Assert.True(n > 0);
                    got += n;
                }
                Assert.True(IndexOf(all, Encoding.UTF8.GetBytes("csharp-probe")) >= 0);
                Assert.True(IndexOf(all, Encoding.UTF8.GetBytes("csharp-fps")) >= 0);
                Assert.True(IndexOf(all, Encoding.UTF8.GetBytes("csharp-\u03c0")) >= 0);

                WriteU32(fd, 28, 0);
            }
            finally
            {
                libc_close(fd);
            }

            Assert.Equal(0UL, Perfr.Start("after-capture"));
            Perfr.Shutdown();
            Assert.False(File.Exists(segment));
            Assert.True(Perfr.Available);
        }

        static void WriteU32(int fd, long offset, uint value)
        {
            var bytes = new byte[4];
            BinaryPrimitives.WriteUInt32LittleEndian(bytes, value);
            Assert.Equal(4L, libc_pwrite(fd, bytes, 4, offset));
        }

        static unsafe long PreadAt(int fd, byte[] dest, long offset, long count)
        {
            fixed (byte* p = dest)
                return libc_pread_ptr(fd, p + offset, (ulong)count, offset);
        }

        [DllImport("libc", EntryPoint = "open")]
        static extern int libc_open(string path, int flags);

        [DllImport("libc", EntryPoint = "pread")]
        static extern long libc_pread(int fd, byte[] buf, ulong count, long offset);

        [DllImport("libc", EntryPoint = "pread")]
        static extern unsafe long libc_pread_ptr(int fd, byte* buf, ulong count, long offset);

        [DllImport("libc", EntryPoint = "pwrite")]
        static extern long libc_pwrite(int fd, byte[] buf, ulong count, long offset);

        [DllImport("libc", EntryPoint = "lseek")]
        static extern long libc_lseek(int fd, long offset, int whence);

        [DllImport("libc", EntryPoint = "close")]
        static extern int libc_close(int fd);

        static int IndexOf(byte[] haystack, byte[] needle)
        {
            for (var i = 0; i + needle.Length <= haystack.Length; i++)
            {
                var match = true;
                for (var j = 0; j < needle.Length; j++)
                {
                    if (haystack[i + j] != needle[j])
                    {
                        match = false;
                        break;
                    }
                }
                if (match) return i;
            }
            return -1;
        }

        static string Comm()
        {
            return File.ReadAllText("/proc/thread-self/comm").Trim();
        }
    }
}
