/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

using System;

namespace Perfr
{
    /// <summary>
    /// Manual instrumentation for perfr: <c>using Perfr; Profiler.Init();</c>.
    /// (The class is not called <c>Perfr</c> because a type with the same
    /// name as its namespace cannot be reached by its simple name from any
    /// other namespace.) Every call is safe from any thread at any time. Until <see cref="Init"/> has loaded <c>libperfr</c>, each
    /// call returns immediately. After that, <c>libperfr</c> itself writes
    /// nothing unless a perfr service is capturing the process.
    /// </summary>
    /// <remarks>
    /// Names are UTF-8. A <see cref="ReadOnlySpan{T}"/> of <see cref="byte"/>
    /// is passed through with no copy and no UTF-8 check, the same as the C
    /// API. Handles are opaque. Zero means "no event", which is what every
    /// call returns while profiling is off, and every function accepts zero.
    /// </remarks>
    public static class Profiler
    {
        /// <summary>The ABI this assembly speaks. <c>libperfr</c> returns a table only for it.</summary>
        public const int AbiVersion = 1;

        /// <summary><see cref="Init"/> found no <c>libperfr</c>.</summary>
        public const int ENoLib = -1000;

        /// <summary>A <c>libperfr</c> was found but it speaks another ABI.</summary>
        public const int EAbi = -1001;

        /// <summary>
        /// Loads <c>libperfr</c> and creates this process's segment.
        /// Idempotent. Returns 0 on success, <see cref="ENoLib"/> or
        /// <see cref="EAbi"/> when no usable library was found, or a negative
        /// errno if the segment could not be created. Later calls stay valid
        /// no-ops in every failure case.
        /// </summary>
        public static int Init() => Native.Init();

        /// <summary>
        /// Removes the segment's name. Safe to skip: the service sweeps a
        /// segment once its pid is gone. The library stays loaded.
        /// </summary>
        public static void Shutdown() => Native.Shutdown();

        /// <summary>Whether <see cref="Init"/> has loaded a library.</summary>
        public static bool Available => Native.Available;

        /// <summary>The library <see cref="Init"/> loaded, or null.</summary>
        public static string? LibraryPath => Native.LibraryPath;

        /// <summary>Begins a scope on the calling thread. Returns its handle.</summary>
        public static ulong Start(string? name) => Native.Start(name, async: false);

        /// <summary>Begins a scope on the calling thread. <paramref name="utf8Name"/> is the name bytes.</summary>
        public static ulong Start(ReadOnlySpan<byte> utf8Name) => Native.Start(utf8Name, async: false);

        /// <summary>Begins a scope that may be stopped from any thread, drawn on its own track.</summary>
        public static ulong StartAsync(string? name) => Native.Start(name, async: true);

        /// <summary>Begins an async scope. <paramref name="utf8Name"/> is the name bytes.</summary>
        public static ulong StartAsync(ReadOnlySpan<byte> utf8Name) => Native.Start(utf8Name, async: true);

        /// <summary>Ends a scope from <see cref="Start"/> or <see cref="StartAsync"/>. Zero is a no-op.</summary>
        public static void Stop(ulong handle) => Native.Stop(handle);

        /// <summary>A point in time with a name and no duration. The handle can be either end of a link.</summary>
        public static ulong Instant(string? name) => Native.Instant(name);

        /// <summary>An instant whose name is already bytes.</summary>
        public static ulong Instant(ReadOnlySpan<byte> utf8Name) => Native.Instant(utf8Name);

        /// <summary>An arrow from one event to another. Either handle being zero is a no-op.</summary>
        public static void Link(ulong from, ulong to) => Native.Link(from, to);

        /// <summary>A value to graph over time, on a track named <paramref name="name"/>.</summary>
        public static void Value(string? name, double value) => Native.Value(name, value);

        /// <summary>A value whose track name is already bytes.</summary>
        public static void Value(ReadOnlySpan<byte> utf8Name, double value) => Native.Value(utf8Name, value);

        /// <summary>
        /// <c>CLOCK_MONOTONIC</c> nanoseconds, the clock of every timestamp.
        /// Zero while no library is loaded. This clock runs even when nobody
        /// is capturing, so a later <see cref="Span"/> can use a time grabbed
        /// at the real site of the event.
        /// </summary>
        public static ulong NowNs() => Native.NowNs();

        /// <summary>A complete scope at timestamps from <see cref="NowNs"/>.</summary>
        public static void Span(string? name, ulong startNs, ulong endNs) => Native.Span(name, startNs, endNs, async: false);

        /// <summary>A complete scope at supplied timestamps. <paramref name="utf8Name"/> is the name bytes.</summary>
        public static void Span(ReadOnlySpan<byte> utf8Name, ulong startNs, ulong endNs) => Native.Span(utf8Name, startNs, endNs, async: false);

        /// <summary>A complete async span at supplied timestamps, on its own track.</summary>
        public static void SpanAsync(string? name, ulong startNs, ulong endNs) => Native.Span(name, startNs, endNs, async: true);

        /// <summary>A complete async span. <paramref name="utf8Name"/> is the name bytes.</summary>
        public static void SpanAsync(ReadOnlySpan<byte> utf8Name, ulong startNs, ulong endNs) => Native.Span(utf8Name, startNs, endNs, async: true);

        /// <summary>
        /// A scope that stops when disposed.
        /// <code>using (Profiler.Scope("frame")) { }</code>
        /// </summary>
        public static ScopeGuard Scope(string? name) => new ScopeGuard(Start(name));

        /// <summary>A disposable scope whose name is already bytes.</summary>
        public static ScopeGuard Scope(ReadOnlySpan<byte> utf8Name) => new ScopeGuard(Start(utf8Name));

        /// <summary>A disposable async scope. Stop it from any thread, including <c>Dispose</c>.</summary>
        public static ScopeGuard ScopeAsync(string? name) => new ScopeGuard(StartAsync(name));

        /// <summary>A disposable async scope whose name is already bytes.</summary>
        public static ScopeGuard ScopeAsync(ReadOnlySpan<byte> utf8Name) => new ScopeGuard(StartAsync(utf8Name));

        /// <summary>
        /// Sets the operating-system name of the calling thread, which perfr
        /// reads from <c>/proc/&lt;pid&gt;/task/&lt;tid&gt;/comm</c> on Linux,
        /// <c>pthread_setname_np</c> on macOS, and <c>SetThreadDescription</c>
        /// on Windows. <c>libperfr</c> has no thread-name call. Linux keeps
        /// 15 bytes. Returns false when the OS refuses the name. Also sets
        /// <see cref="System.Threading.Thread.Name"/> when it is still unset.
        /// </summary>
        public static bool NameThread(string? name) => ThreadNames.Set(name);

        /// <summary>A scope handle that calls <see cref="Stop"/> when disposed. Zero does nothing.</summary>
        public readonly struct ScopeGuard : IDisposable
        {
            /// <summary>The handle <see cref="Start"/> returned. Zero when profiling is off.</summary>
            public ulong Handle { get; }

            internal ScopeGuard(ulong handle) => Handle = handle;

            /// <summary>Ends the scope. A second call stops the same handle again, which the C API also allows.</summary>
            public void Dispose() => Profiler.Stop(Handle);
        }
    }
}
