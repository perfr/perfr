/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

using System;
using Perfr;
using System.Diagnostics;
using System.Threading;

namespace PerfrSample
{
    /// <summary>
    /// A short console program that exercises the C# API. Run perfr, open the
    /// viewer, and record this process. With no libperfr the same binary
    /// still runs; every call is a no-op.
    /// </summary>
    static class Program
    {
        static int Main(string[] args)
        {
            var seconds = 3;
            for (var i = 0; i + 1 < args.Length; i++)
            {
                if (args[i] == "--seconds") seconds = int.Parse(args[i + 1]);
            }

            var rc = Profiler.Init();
            Profiler.NameThread("perfr-sample");
            Console.WriteLine("PerfrSample pid={0} init={1} library={2} seconds={3}",
                Environment.ProcessId, rc, Profiler.LibraryPath ?? "(none)", seconds);

            var started = Stopwatch.StartNew();
            var frame = 0u;
            while (started.Elapsed.TotalSeconds < seconds)
            {
                using (Profiler.Scope("frame"))
                {
                    Profiler.Instant("vsync");
                    using (Profiler.Scope("update"))
                    {
                        Thread.SpinWait(10_000);
                    }
                    using (Profiler.Scope("render"))
                    {
                        Thread.SpinWait(20_000);
                    }
                    var now = Profiler.NowNs();
                    if (now != 0)
                        Profiler.SpanAsync("gpu: shadow pass", now - 2_000_000, now - 500_000);
                    Profiler.Value("fps", 60.0);
                    Profiler.Value("frame", frame);
                }
                frame++;
                Thread.Sleep(8);
            }

            Console.WriteLine("PerfrSample done: {0} frames", frame);
            Profiler.Shutdown();
            return 0;
        }
    }
}
