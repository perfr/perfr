/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace Perfr
{
    /// <summary>
    /// The OS thread name. perfr reads it from the kernel; it is not a
    /// <c>libperfr</c> event. Linux <c>comm</c> is 16 bytes including the NUL.
    /// </summary>
    internal static class ThreadNames
    {
        const int LinuxMax = 15;

        public static bool Set(string? name)
        {
            if (string.IsNullOrEmpty(name)) return false;
            var shown = Truncate(name);
            var ok = SetOs(shown);
            try
            {
                if (Thread.CurrentThread.Name == null)
                    Thread.CurrentThread.Name = shown;
            }
            catch (InvalidOperationException)
            {
                // Already named. The OS name above is the one perfr reads.
            }
            return ok;
        }

        static string Truncate(string name)
        {
            if (!RuntimeInformation.IsOSPlatform(OSPlatform.Linux)) return name;
            var bytes = Encoding.UTF8.GetBytes(name);
            if (bytes.Length <= LinuxMax) return name;
            var n = LinuxMax;
            while (n > 0 && (bytes[n] & 0xC0) == 0x80) n--;
            return Encoding.UTF8.GetString(bytes, 0, n);
        }

        static bool SetOs(string name)
        {
            if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
                return SetWindows(name);
            if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX))
                return SetMac(name);
            if (RuntimeInformation.IsOSPlatform(OSPlatform.Linux))
                return SetLinux(name);
            return false;
        }

        static bool SetLinux(string name)
        {
            var bytes = Encoding.UTF8.GetBytes(name + "\0");
            if (!Linux.Bind(out var self, out var setName)) return false;
            unsafe
            {
                fixed (byte* p = bytes)
                    return setName(self(), p) == 0;
            }
        }

        static bool SetMac(string name)
        {
            var bytes = Encoding.UTF8.GetBytes(name + "\0");
            unsafe
            {
                fixed (byte* p = bytes)
                    return MacSetName(p) == 0;
            }
        }

        static bool SetWindows(string name)
        {
            var kernel = DynLib.Open("kernel32.dll");
            if (kernel == IntPtr.Zero) return false;
            var export = DynLib.Symbol(kernel, "SetThreadDescription");
            if (export == IntPtr.Zero) return false;
            var set = Marshal.GetDelegateForFunctionPointer<SetThreadDescriptionDelegate>(export);
            return set(GetCurrentThread(), name) >= 0;
        }

        [UnmanagedFunctionPointer(CallingConvention.Winapi)]
        delegate int SetThreadDescriptionDelegate(IntPtr thread, [MarshalAs(UnmanagedType.LPWStr)] string name);

        [DllImport("kernel32", EntryPoint = "GetCurrentThread")]
        static extern IntPtr GetCurrentThread();

        [DllImport("libSystem.B.dylib", EntryPoint = "pthread_setname_np")]
        static extern unsafe int MacSetName(byte* name);

        static class Linux
        {
            [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
            internal delegate ulong SelfDelegate();

            [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
            internal unsafe delegate int SetDelegate(ulong thread, byte* name);

            static SelfDelegate? _self;
            static SetDelegate? _set;
            static bool _failed;

            internal static bool Bind(out SelfDelegate self, out SetDelegate set)
            {
                self = _self!;
                set = _set!;
                if (_self != null && _set != null) return true;
                if (_failed) return false;
                foreach (var soname in new[] { "libc.so.6", "libpthread.so.0" })
                {
                    var lib = DynLib.Open(soname);
                    if (lib == IntPtr.Zero) continue;
                    var selfPtr = DynLib.Symbol(lib, "pthread_self");
                    var setPtr = DynLib.Symbol(lib, "pthread_setname_np");
                    if (selfPtr == IntPtr.Zero || setPtr == IntPtr.Zero) continue;
                    _self = Marshal.GetDelegateForFunctionPointer<SelfDelegate>(selfPtr);
                    _set = Marshal.GetDelegateForFunctionPointer<SetDelegate>(setPtr);
                    self = _self;
                    set = _set;
                    return true;
                }
                _failed = true;
                return false;
            }
        }
    }
}
