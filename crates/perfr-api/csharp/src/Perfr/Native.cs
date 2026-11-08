/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace Perfr
{
    /// <summary>
    /// The same search <c>perfr.h</c> uses, then the versioned function table.
    /// A miss leaves every call as one null check.
    /// </summary>
    internal static class Native
    {
        static readonly object Gate = new object();
        static NativeApi? _api;

        public static bool Available => Volatile.Read(ref _api) != null;

        public static string? LibraryPath => Volatile.Read(ref _api)?.Path;

        public static int Init()
        {
            var api = Volatile.Read(ref _api);
            if (api != null) return api.Init();
            lock (Gate)
            {
                api = _api;
                if (api != null) return api.Init();
                api = TryLoad(out var error);
                if (api == null) return error;
                Volatile.Write(ref _api, api);
                return api.Init();
            }
        }

        public static void Shutdown()
        {
            Volatile.Read(ref _api)?.Shutdown();
        }

        public static ulong Start(string? name, bool async)
        {
            var api = Volatile.Read(ref _api);
            return api == null ? 0 : api.Start(name, async);
        }

        public static ulong Start(ReadOnlySpan<byte> utf8Name, bool async)
        {
            var api = Volatile.Read(ref _api);
            return api == null ? 0 : api.Start(utf8Name, async);
        }

        public static void Stop(ulong handle)
        {
            if (handle == 0) return;
            Volatile.Read(ref _api)?.Stop(handle);
        }

        public static ulong Instant(string? name)
        {
            var api = Volatile.Read(ref _api);
            return api == null ? 0 : api.Instant(name);
        }

        public static ulong Instant(ReadOnlySpan<byte> utf8Name)
        {
            var api = Volatile.Read(ref _api);
            return api == null ? 0 : api.Instant(utf8Name);
        }

        public static void Link(ulong from, ulong to)
        {
            if (from == 0 || to == 0) return;
            Volatile.Read(ref _api)?.Link(from, to);
        }

        public static void Value(string? name, double value)
        {
            Volatile.Read(ref _api)?.Value(name, value);
        }

        public static void Value(ReadOnlySpan<byte> utf8Name, double value)
        {
            Volatile.Read(ref _api)?.Value(utf8Name, value);
        }

        public static ulong NowNs()
        {
            var api = Volatile.Read(ref _api);
            return api == null ? 0 : api.NowNs();
        }

        public static void Span(string? name, ulong startNs, ulong endNs, bool async)
        {
            Volatile.Read(ref _api)?.Span(name, startNs, endNs, async);
        }

        public static void Span(ReadOnlySpan<byte> utf8Name, ulong startNs, ulong endNs, bool async)
        {
            Volatile.Read(ref _api)?.Span(utf8Name, startNs, endNs, async);
        }

        static NativeApi? TryLoad(out int error)
        {
            foreach (var path in Candidates(out var exclusive))
            {
                if (exclusive && !File.Exists(path))
                {
                    error = Profiler.ENoLib;
                    return null;
                }
                if (Path.IsPathRooted(path) && !File.Exists(path)) continue;
                var handle = DynLib.Open(path);
                if (handle == IntPtr.Zero)
                {
                    if (exclusive)
                    {
                        error = Profiler.ENoLib;
                        return null;
                    }
                    continue;
                }

                if (!NativeApi.TryBind(handle, path, out var api))
                {
                    DynLib.Close(handle);
                    error = Profiler.EAbi;
                    return null;
                }
                error = 0;
                return api;
            }
            error = Profiler.ENoLib;
            return null;
        }

        static List<string> Candidates(out bool exclusive)
        {
            var list = new List<string>();
            var env = Environment.GetEnvironmentVariable("PERFR_LIB");
            if (!string.IsNullOrEmpty(env))
            {
                exclusive = true;
                list.Add(env);
                return list;
            }
            exclusive = false;

            var service = Which(ServiceFile);
            if (service != null)
            {
                var dir = Path.GetDirectoryName(service);
                if (!string.IsNullOrEmpty(dir)) list.Add(Path.Combine(dir, LibFile));
            }

            var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            if (!string.IsNullOrEmpty(home))
            {
                if (IsWindows)
                    list.Add(Path.Combine(home, ".perfr", "bin", LibFile));
                else
                {
                    list.Add(Path.Combine(home, ".local", "bin", LibFile));
                    list.Add(Path.Combine(home, ".perfr", "bin", LibFile));
                }
            }

            var exeDir = ExeDir();
            if (!string.IsNullOrEmpty(exeDir)) list.Add(Path.Combine(exeDir, LibFile));
            list.Add(LibFile);
            return list;
        }

        static string? Which(string file)
        {
            var path = Environment.GetEnvironmentVariable("PATH");
            if (string.IsNullOrEmpty(path)) return null;
            foreach (var dir in path.Split(Path.PathSeparator))
            {
                if (dir.Length == 0) continue;
                var candidate = Path.Combine(dir, file);
                if (File.Exists(candidate)) return candidate;
            }
            return null;
        }

        static string? ExeDir()
        {
            try
            {
                var main = Process.GetCurrentProcess().MainModule?.FileName;
                if (!string.IsNullOrEmpty(main)) return Path.GetDirectoryName(main);
            }
            catch (Exception)
            {
                // A host can refuse the module path. Rule 3 is then skipped.
            }
            return null;
        }

        static bool IsWindows => RuntimeInformation.IsOSPlatform(OSPlatform.Windows);

        static bool IsMac => RuntimeInformation.IsOSPlatform(OSPlatform.OSX);

        static string LibFile => IsWindows ? "libperfr.dll" : IsMac ? "libperfr.dylib" : "libperfr.so";

        static string ServiceFile => IsWindows ? "perfr.exe" : "perfr";
    }

    internal sealed class NativeApi
    {
        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate int InitDelegate();

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate void ShutdownDelegate();

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        unsafe delegate ulong StartDelegate(byte* name, nuint nameLen);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate void StopDelegate(ulong handle);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate void LinkDelegate(ulong from, ulong to);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        unsafe delegate void ValueDelegate(byte* name, nuint nameLen, double value);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate ulong NowDelegate();

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        unsafe delegate void SpanDelegate(byte* name, nuint nameLen, ulong startNs, ulong endNs);

        [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
        delegate IntPtr TableDelegate(uint abiVersion);

        [StructLayout(LayoutKind.Sequential)]
        struct ApiTable
        {
            public uint AbiVersion;
            public uint Size;
            public IntPtr Init;
            public IntPtr Shutdown;
            public IntPtr Start;
            public IntPtr StartDynamic;
            public IntPtr StartAsync;
            public IntPtr Stop;
            public IntPtr Instant;
            public IntPtr Link;
            public IntPtr Value;
            public IntPtr NowNs;
            public IntPtr Span;
            public IntPtr SpanAsync;
        }

        readonly InitDelegate _init;
        readonly ShutdownDelegate _shutdown;
        readonly StartDelegate _start;
        readonly StartDelegate _startAsync;
        readonly StopDelegate _stop;
        readonly StartDelegate _instant;
        readonly LinkDelegate _link;
        readonly ValueDelegate _value;
        readonly NowDelegate _nowNs;
        readonly SpanDelegate _span;
        readonly SpanDelegate _spanAsync;

        NativeApi(
            string path,
            InitDelegate init,
            ShutdownDelegate shutdown,
            StartDelegate start,
            StartDelegate startAsync,
            StopDelegate stop,
            StartDelegate instant,
            LinkDelegate link,
            ValueDelegate value,
            NowDelegate nowNs,
            SpanDelegate span,
            SpanDelegate spanAsync)
        {
            Path = path;
            _init = init;
            _shutdown = shutdown;
            _start = start;
            _startAsync = startAsync;
            _stop = stop;
            _instant = instant;
            _link = link;
            _value = value;
            _nowNs = nowNs;
            _span = span;
            _spanAsync = spanAsync;
        }

        public string Path { get; }

        public static bool TryBind(IntPtr handle, string path, out NativeApi? api)
        {
            api = null;
            var tableExport = DynLib.Symbol(handle, "perfr_api_table_v1");
            if (tableExport == IntPtr.Zero) return false;
            var get = Marshal.GetDelegateForFunctionPointer<TableDelegate>(tableExport);
            var tablePtr = get((uint)Profiler.AbiVersion);
            if (tablePtr == IntPtr.Zero) return false;
            var table = Marshal.PtrToStructure<ApiTable>(tablePtr);
            var need = Marshal.SizeOf<ApiTable>();
            if (table.AbiVersion != Profiler.AbiVersion || table.Size < (uint)need) return false;
            if (IntPtr.Size == 8 && table.Size < 8 + 12 * 8) return false;

            api = new NativeApi(
                path,
                Fn<InitDelegate>(table.Init),
                Fn<ShutdownDelegate>(table.Shutdown),
                Fn<StartDelegate>(table.Start),
                Fn<StartDelegate>(table.StartAsync),
                Fn<StopDelegate>(table.Stop),
                Fn<StartDelegate>(table.Instant),
                Fn<LinkDelegate>(table.Link),
                Fn<ValueDelegate>(table.Value),
                Fn<NowDelegate>(table.NowNs),
                Fn<SpanDelegate>(table.Span),
                Fn<SpanDelegate>(table.SpanAsync));
            return true;
        }

        static T Fn<T>(IntPtr ptr) where T : Delegate => Marshal.GetDelegateForFunctionPointer<T>(ptr);

        public int Init() => _init();

        public void Shutdown() => _shutdown();

        public ulong Start(string? name, bool async) => CallNamed(async ? _startAsync : _start, name);

        public ulong Start(ReadOnlySpan<byte> utf8Name, bool async) => Call(async ? _startAsync : _start, utf8Name);

        public void Stop(ulong handle) => _stop(handle);

        public ulong Instant(string? name) => CallNamed(_instant, name);

        public ulong Instant(ReadOnlySpan<byte> utf8Name) => Call(_instant, utf8Name);

        public void Link(ulong from, ulong to) => _link(from, to);

        public void Value(string? name, double value) => CallNamed(_value, name, value);

        public void Value(ReadOnlySpan<byte> utf8Name, double value) => Call(_value, utf8Name, value);

        public ulong NowNs() => _nowNs();

        public void Span(string? name, ulong startNs, ulong endNs, bool async) =>
            CallNamed(async ? _spanAsync : _span, name, startNs, endNs);

        public void Span(ReadOnlySpan<byte> utf8Name, ulong startNs, ulong endNs, bool async) =>
            Call(async ? _spanAsync : _span, utf8Name, startNs, endNs);

        // A string name is encoded into a stack buffer. These three are the
        // hot path, so there is no helper taking a delegate: a closure per
        // scope would be an allocation per scope.
        static ulong CallNamed(StartDelegate fn, string? name)
        {
            if (string.IsNullOrEmpty(name)) return Call(fn, ReadOnlySpan<byte>.Empty);
            if (Encoding.UTF8.GetMaxByteCount(name.Length) <= StackBytes)
            {
                Span<byte> buf = stackalloc byte[StackBytes];
                return Call(fn, buf.Slice(0, Encoding.UTF8.GetBytes(name, buf)));
            }
            return Call(fn, Encoding.UTF8.GetBytes(name));
        }

        static void CallNamed(ValueDelegate fn, string? name, double value)
        {
            if (string.IsNullOrEmpty(name))
            {
                Call(fn, ReadOnlySpan<byte>.Empty, value);
                return;
            }
            if (Encoding.UTF8.GetMaxByteCount(name.Length) <= StackBytes)
            {
                Span<byte> buf = stackalloc byte[StackBytes];
                Call(fn, buf.Slice(0, Encoding.UTF8.GetBytes(name, buf)), value);
                return;
            }
            Call(fn, Encoding.UTF8.GetBytes(name), value);
        }

        static void CallNamed(SpanDelegate fn, string? name, ulong startNs, ulong endNs)
        {
            if (string.IsNullOrEmpty(name))
            {
                Call(fn, ReadOnlySpan<byte>.Empty, startNs, endNs);
                return;
            }
            if (Encoding.UTF8.GetMaxByteCount(name.Length) <= StackBytes)
            {
                Span<byte> buf = stackalloc byte[StackBytes];
                Call(fn, buf.Slice(0, Encoding.UTF8.GetBytes(name, buf)), startNs, endNs);
                return;
            }
            Call(fn, Encoding.UTF8.GetBytes(name), startNs, endNs);
        }

        const int StackBytes = 256;

        static unsafe ulong Call(StartDelegate fn, ReadOnlySpan<byte> name)
        {
            if (name.IsEmpty) return fn(null, 0);
            fixed (byte* p = name) return fn(p, (nuint)name.Length);
        }

        static unsafe void Call(ValueDelegate fn, ReadOnlySpan<byte> name, double value)
        {
            if (name.IsEmpty)
            {
                fn(null, 0, value);
                return;
            }
            fixed (byte* p = name) fn(p, (nuint)name.Length, value);
        }

        static unsafe void Call(SpanDelegate fn, ReadOnlySpan<byte> name, ulong startNs, ulong endNs)
        {
            if (name.IsEmpty)
            {
                fn(null, 0, startNs, endNs);
                return;
            }
            fixed (byte* p = name) fn(p, (nuint)name.Length, startNs, endNs);
        }
    }

    /// <summary>
    /// dlopen / LoadLibrary. On .NET 6 and later this is <c>NativeLibrary</c>.
    /// The netstandard2.1 target has no <c>NativeLibrary</c> and calls the
    /// loader directly; a runtime without <c>libdl.so.2</c> (musl, as on
    /// Alpine) makes that P/Invoke throw, which is caught here so
    /// <c>Init</c> reports <see cref="Profiler.ENoLib"/> instead.
    /// </summary>
    internal static class DynLib
    {
#if NET6_0_OR_GREATER
        public static IntPtr Open(string path) =>
            NativeLibrary.TryLoad(path, out var handle) ? handle : IntPtr.Zero;

        public static IntPtr Symbol(IntPtr handle, string name) =>
            handle != IntPtr.Zero && NativeLibrary.TryGetExport(handle, name, out var export) ? export : IntPtr.Zero;

        public static void Close(IntPtr handle)
        {
            if (handle != IntPtr.Zero) NativeLibrary.Free(handle);
        }
#else
        public static IntPtr Open(string path)
        {
            try
            {
                if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
                    return LoadLibraryW(path);
                if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX))
                    return MacDlopen(path, 6); // RTLD_NOW | RTLD_LOCAL
                try
                {
                    return LinuxDlopen(path, 2); // RTLD_NOW
                }
                catch (DllNotFoundException)
                {
                    // No libdl.so.2: musl keeps dlopen in libc itself.
                    return LibcDlopen(path, 2);
                }
            }
            catch (DllNotFoundException)
            {
                return IntPtr.Zero;
            }
            catch (EntryPointNotFoundException)
            {
                return IntPtr.Zero;
            }
        }

        public static IntPtr Symbol(IntPtr handle, string name)
        {
            if (handle == IntPtr.Zero) return IntPtr.Zero;
            try
            {
                if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
                    return GetProcAddress(handle, name);
                if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX))
                    return MacDlsym(handle, name);
                try
                {
                    return LinuxDlsym(handle, name);
                }
                catch (DllNotFoundException)
                {
                    return LibcDlsym(handle, name);
                }
            }
            catch (DllNotFoundException)
            {
                return IntPtr.Zero;
            }
            catch (EntryPointNotFoundException)
            {
                return IntPtr.Zero;
            }
        }

        public static void Close(IntPtr handle)
        {
            if (handle == IntPtr.Zero) return;
            try
            {
                if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
                    FreeLibrary(handle);
                else if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX))
                    MacDlclose(handle);
                else
                {
                    try
                    {
                        LinuxDlclose(handle);
                    }
                    catch (DllNotFoundException)
                    {
                        LibcDlclose(handle);
                    }
                }
            }
            catch (DllNotFoundException)
            {
                // The handle came from a loader this process does not have.
            }
            catch (EntryPointNotFoundException)
            {
            }
        }

        [DllImport("libdl.so.2", EntryPoint = "dlopen")]
        static extern IntPtr LinuxDlopen(string file, int mode);

        [DllImport("libdl.so.2", EntryPoint = "dlsym")]
        static extern IntPtr LinuxDlsym(IntPtr handle, string name);

        [DllImport("libdl.so.2", EntryPoint = "dlclose")]
        static extern int LinuxDlclose(IntPtr handle);

        [DllImport("libc", EntryPoint = "dlopen")]
        static extern IntPtr LibcDlopen(string file, int mode);

        [DllImport("libc", EntryPoint = "dlsym")]
        static extern IntPtr LibcDlsym(IntPtr handle, string name);

        [DllImport("libc", EntryPoint = "dlclose")]
        static extern int LibcDlclose(IntPtr handle);

        [DllImport("libSystem.B.dylib", EntryPoint = "dlopen")]
        static extern IntPtr MacDlopen(string file, int mode);

        [DllImport("libSystem.B.dylib", EntryPoint = "dlsym")]
        static extern IntPtr MacDlsym(IntPtr handle, string name);

        [DllImport("libSystem.B.dylib", EntryPoint = "dlclose")]
        static extern int MacDlclose(IntPtr handle);

        [DllImport("kernel32", CharSet = CharSet.Unicode, EntryPoint = "LoadLibraryW")]
        static extern IntPtr LoadLibraryW(string path);

        [DllImport("kernel32", CharSet = CharSet.Ansi)]
        static extern IntPtr GetProcAddress(IntPtr module, string name);

        [DllImport("kernel32")]
        static extern bool FreeLibrary(IntPtr module);
#endif
    }
}
