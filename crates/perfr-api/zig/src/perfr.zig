// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! perfr manual instrumentation for Zig. Loads libperfr at run time with
//! the same search as perfr.h. Every call is a no-op until init finds a
//! library, and libperfr itself writes nothing until a service is capturing.
//!
//! A scope ends with defer, which is the Zig shape of a C++ destructor:
//!
//! ```
//! const frame = perfr.scope("frame");
//! defer frame.end();
//! ```

const std = @import("std");
const builtin = @import("builtin");
const c = std.c;

pub const abi_version: u32 = 1;
pub const e_nolib: i32 = -1000;
pub const e_abi: i32 = -1001;

const StartFn = *const fn (?[*]const u8, usize) callconv(.c) u64;
const StopFn = *const fn (u64) callconv(.c) void;
const ValueFn = *const fn (?[*]const u8, usize, f64) callconv(.c) void;
const SpanFn = *const fn (?[*]const u8, usize, u64, u64) callconv(.c) void;
const InitFn = *const fn () callconv(.c) i32;
const ShutdownFn = *const fn () callconv(.c) void;
const NowFn = *const fn () callconv(.c) u64;
const LinkFn = *const fn (u64, u64) callconv(.c) void;
const TableFn = *const fn (u32) callconv(.c) ?[*]const u32;

const Api = struct {
    init_fn: InitFn,
    shutdown_fn: ShutdownFn,
    start: StartFn,
    start_async: StartFn,
    stop: StopFn,
    instant: StartFn,
    link: LinkFn,
    value: ValueFn,
    now_ns: NowFn,
    span: SpanFn,
    span_async: SpanFn,
};

// The table is published the way perfr.h publishes its own: filled in
// completely, then one pointer stored with release and loaded with acquire
// on every call. A by-value `?Api` read without synchronisation could show
// a thread the "present" tag before the eleven function pointers behind it.
var api_storage: Api = undefined;
var loaded = std.atomic.Value(?*const Api).init(null);
var loaded_path: [4096]u8 = undefined;
var loaded_path_len: usize = 0;
var gate: std.atomic.Mutex = .unlocked;

/// The table's `size` for this word size: two u32 then twelve pointers.
const table_size: u32 = 8 + 12 * @sizeOf(usize);

fn lock() void {
    while (!gate.tryLock()) {}
}

inline fn current() ?*const Api {
    return loaded.load(.acquire);
}

/// A scope that ends when `end` is called. Pair it with `defer`.
pub const Scope = struct {
    handle: u64,

    pub fn end(self: Scope) void {
        stop(self.handle);
    }
};

pub fn init() i32 {
    if (current()) |api| return api.init_fn();
    lock();
    defer gate.unlock();
    if (current()) |api| return api.init_fn();
    api_storage = bind() catch |err| switch (err) {
        error.NoLib => return e_nolib,
        error.Abi => return e_abi,
    };
    loaded.store(&api_storage, .release);
    return api_storage.init_fn();
}

pub fn shutdown() void {
    if (current()) |api| api.shutdown_fn();
}

pub fn available() bool {
    return current() != null;
}

pub fn libraryPath() ?[]const u8 {
    if (current() == null or loaded_path_len == 0) return null;
    return loaded_path[0..loaded_path_len];
}

pub fn start(name: []const u8) u64 {
    const api = current() orelse return 0;
    if (name.len == 0) return api.start(null, 0);
    return api.start(name.ptr, name.len);
}

pub fn startAsync(name: []const u8) u64 {
    const api = current() orelse return 0;
    if (name.len == 0) return api.start_async(null, 0);
    return api.start_async(name.ptr, name.len);
}

pub fn stop(handle: u64) void {
    if (handle == 0) return;
    const api = current() orelse return;
    api.stop(handle);
}

pub fn instant(name: []const u8) u64 {
    const api = current() orelse return 0;
    if (name.len == 0) return api.instant(null, 0);
    return api.instant(name.ptr, name.len);
}

pub fn link(from: u64, to: u64) void {
    if (from == 0 or to == 0) return;
    const api = current() orelse return;
    api.link(from, to);
}

pub fn value(name: []const u8, sample: f64) void {
    const api = current() orelse return;
    if (name.len == 0) {
        api.value(null, 0, sample);
        return;
    }
    api.value(name.ptr, name.len, sample);
}

pub fn nowNs() u64 {
    const api = current() orelse return 0;
    return api.now_ns();
}

pub fn span(name: []const u8, start_ns: u64, end_ns: u64) void {
    const api = current() orelse return;
    if (name.len == 0) {
        api.span(null, 0, start_ns, end_ns);
        return;
    }
    api.span(name.ptr, name.len, start_ns, end_ns);
}

pub fn spanAsync(name: []const u8, start_ns: u64, end_ns: u64) void {
    const api = current() orelse return;
    if (name.len == 0) {
        api.span_async(null, 0, start_ns, end_ns);
        return;
    }
    api.span_async(name.ptr, name.len, start_ns, end_ns);
}

pub fn scope(name: []const u8) Scope {
    return .{ .handle = start(name) };
}

pub fn scopeAsync(name: []const u8) Scope {
    return .{ .handle = startAsync(name) };
}

/// OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes,
/// which is what perfr reads from /proc.
pub fn nameThread(name: []const u8) bool {
    if (name.len == 0) return false;
    var buf: [16]u8 = undefined;
    const n = @min(name.len, 15);
    @memcpy(buf[0..n], name[0..n]);
    buf[n] = 0;
    const z: [*:0]const u8 = buf[0..n :0].ptr;
    switch (builtin.os.tag) {
        .macos, .ios, .watchos, .tvos, .visionos => return c.pthread_setname_np(z) == 0,
        else => return c.pthread_setname_np(c.pthread_self(), z) == 0,
    }
}

fn bind() error{ NoLib, Abi }!Api {
    var path_buf: [4096]u8 = undefined;
    const path = find(&path_buf) orelse return error.NoLib;
    var lib = std.DynLib.open(path) catch return error.NoLib;
    const get = lib.lookup(TableFn, "perfr_api_table_v1") orelse {
        lib.close();
        return error.Abi;
    };
    const table = get(abi_version) orelse {
        lib.close();
        return error.Abi;
    };
    if (table[0] != abi_version or table[1] < table_size) {
        lib.close();
        return error.Abi;
    }
    const api = Api{
        .init_fn = lib.lookup(InitFn, "perfr_init") orelse return closeAbi(&lib),
        .shutdown_fn = lib.lookup(ShutdownFn, "perfr_shutdown") orelse return closeAbi(&lib),
        .start = lib.lookup(StartFn, "perfr_start") orelse return closeAbi(&lib),
        .start_async = lib.lookup(StartFn, "perfr_start_async") orelse return closeAbi(&lib),
        .stop = lib.lookup(StopFn, "perfr_stop") orelse return closeAbi(&lib),
        .instant = lib.lookup(StartFn, "perfr_instant") orelse return closeAbi(&lib),
        .link = lib.lookup(LinkFn, "perfr_link") orelse return closeAbi(&lib),
        .value = lib.lookup(ValueFn, "perfr_value") orelse return closeAbi(&lib),
        .now_ns = lib.lookup(NowFn, "perfr_now_ns") orelse return closeAbi(&lib),
        .span = lib.lookup(SpanFn, "perfr_span") orelse return closeAbi(&lib),
        .span_async = lib.lookup(SpanFn, "perfr_span_async") orelse return closeAbi(&lib),
    };
    // The mapping stays for the process. DynLib has no destructor.
    if (path.len < loaded_path.len) {
        @memcpy(loaded_path[0..path.len], path);
        loaded_path_len = path.len;
    }
    return api;
}

fn closeAbi(lib: *std.DynLib) error{Abi} {
    lib.close();
    return error.Abi;
}

fn find(buf: *[4096]u8) ?[]const u8 {
    if (envZ("PERFR_LIB")) |forced| {
        if (forced.len == 0 or forced.len >= buf.len) return null;
        if (c.access(forced.ptr, 0) != 0) return null;
        return forced;
    }
    if (which(buf, serviceName())) |dir| return dir;
    if (envZ(if (builtin.os.tag == .windows) "USERPROFILE" else "HOME")) |home| {
        if (builtin.os.tag == .windows) {
            if (joinExists(buf, home, "\\.perfr\\bin\\", libName())) |p| return p;
        } else {
            if (joinExists(buf, home, "/.local/bin/", libName())) |p| return p;
            if (joinExists(buf, home, "/.perfr/bin/", libName())) |p| return p;
        }
    }
    if (exeDir(buf)) |dir| {
        if (joinExists(buf, dir, "/", libName())) |p| return p;
    }
    var opened = std.DynLib.open(libName()) catch return null;
    opened.close();
    return libName();
}

fn envZ(name: [*:0]const u8) ?[:0]const u8 {
    const v = c.getenv(name) orelse return null;
    return std.mem.span(v);
}

fn libName() [:0]const u8 {
    return switch (builtin.os.tag) {
        .windows => "libperfr.dll",
        .macos, .ios => "libperfr.dylib",
        else => "libperfr.so",
    };
}

fn serviceName() [:0]const u8 {
    return if (builtin.os.tag == .windows) "perfr.exe" else "perfr";
}

fn which(buf: *[4096]u8, file: [:0]const u8) ?[]const u8 {
    const path = envZ("PATH") orelse return null;
    var it = std.mem.splitScalar(u8, path, if (builtin.os.tag == .windows) ';' else ':');
    while (it.next()) |dir| {
        if (dir.len == 0) continue;
        if (joinExists(buf, dir, "/", file)) |candidate| {
            // candidate lives in buf. Copy the directory out before the next join.
            if (std.mem.lastIndexOfScalar(u8, candidate, '/')) |slash| {
                var dir_copy: [4096]u8 = undefined;
                if (slash >= dir_copy.len) continue;
                @memcpy(dir_copy[0..slash], candidate[0..slash]);
                if (joinExists(buf, dir_copy[0..slash], "/", libName())) |lib| return lib;
            }
        }
    }
    return null;
}

fn joinExists(buf: *[4096]u8, a: []const u8, mid: []const u8, b: []const u8) ?[]const u8 {
    if (a.len + mid.len + b.len + 1 > buf.len) return null;
    @memcpy(buf[0..a.len], a);
    @memcpy(buf[a.len .. a.len + mid.len], mid);
    @memcpy(buf[a.len + mid.len .. a.len + mid.len + b.len], b);
    const n = a.len + mid.len + b.len;
    buf[n] = 0;
    const z: [*:0]const u8 = buf[0..n :0].ptr;
    if (c.access(z, 0) != 0) return null;
    return buf[0..n];
}

fn exeDir(buf: *[4096]u8) ?[]const u8 {
    if (builtin.os.tag != .linux) return null;
    const n = c.readlink("/proc/self/exe", buf, buf.len - 1);
    if (n <= 0) return null;
    const len: usize = @intCast(n);
    const slash = std.mem.lastIndexOfScalar(u8, buf[0..len], '/') orelse return null;
    return buf[0..slash];
}

test "nameThread sets the linux comm" {
    if (builtin.os.tag != .linux) return;
    try std.testing.expect(nameThread("perfr-zig"));
    try expectComm("perfr-zig");
    try std.testing.expect(nameThread("0123456789abcdefEXTRA"));
    try expectComm("0123456789abcde");
}

test "calls follow the library" {
    const configured = envZ("PERFR_LIB") orelse {
        // An unset variable would search PATH. Tests set PERFR_LIB.
        return error.SkipZigTest;
    };
    if (c.access(configured.ptr, 0) != 0) {
        try std.testing.expectEqual(e_nolib, init());
        try std.testing.expect(!available());
        try std.testing.expectEqual(@as(u64, 0), start("quiet"));
        try std.testing.expectEqual(@as(u64, 0), nowNs());
        stop(0);
        value("quiet", 1);
        link(0, 1);
        const quiet = scope("quiet");
        quiet.end();
        return;
    }

    try std.testing.expectEqual(@as(i32, 0), init());
    try std.testing.expectEqual(@as(i32, 0), init());
    try std.testing.expect(available());
    const path = libraryPath() orelse return error.MissingPath;
    try std.testing.expect(std.mem.indexOf(u8, path, "libperfr") != null);
    const t0 = nowNs();
    try std.testing.expect(t0 > 0);
    try std.testing.expectEqual(@as(u64, 0), start("before-capture"));

    if (builtin.os.tag != .linux) return;
    var seg_buf: [128]u8 = undefined;
    const prefix = "/dev/shm/perfr-scopes-";
    @memcpy(seg_buf[0..prefix.len], prefix);
    const digits = writePid(seg_buf[prefix.len..], c.getpid());
    const seg_len = prefix.len + digits;
    seg_buf[seg_len] = 0;
    const seg: [*:0]u8 = seg_buf[0..seg_len :0].ptr;
    const fd = c.open(seg, .{ .ACCMODE = .RDWR });
    try std.testing.expect(fd >= 0);
    defer _ = std.os.linux.close(fd);

    var head: [40]u8 = undefined;
    try std.testing.expectEqual(@as(isize, 40), c.pread(fd, &head, head.len, 0));
    try std.testing.expectEqual(@as(u64, 0x4F524249545F5347), std.mem.readInt(u64, head[0..8], .little));
    try std.testing.expectEqual(@as(u32, 2), std.mem.readInt(u32, head[8..12], .little));
    try std.testing.expectEqual(@as(u32, @intCast(c.getpid())), std.mem.readInt(u32, head[24..28], .little));

    var one = [_]u8{ 1, 0, 0, 0 };
    try std.testing.expectEqual(@as(isize, 4), c.pwrite(fd, &one, 4, 28));
    const frame = scope("zig-probe");
    defer frame.end();
    try std.testing.expect(frame.handle != 0);
    value("zig-fps", 59.9);
    const mark = instant("zig-mark");
    try std.testing.expect(mark != 0);
    link(mark, frame.handle);
    const job = scopeAsync("zig-job");
    stop(job.handle);
    const now = nowNs();
    span("zig-span", now - 1000, now);
    spanAsync("zig-gpu", now - 5000, now - 1000);

    const length = c.lseek(fd, 0, 2);
    try std.testing.expect(length > 0);
    const all = try std.testing.allocator.alloc(u8, @intCast(length));
    defer std.testing.allocator.free(all);
    var got: isize = 0;
    while (got < length) {
        const n = c.pread(fd, all[@intCast(got)..].ptr, all.len - @as(usize, @intCast(got)), got);
        try std.testing.expect(n > 0);
        got += n;
    }
    try std.testing.expect(std.mem.indexOf(u8, all, "zig-probe") != null);
    try std.testing.expect(std.mem.indexOf(u8, all, "zig-fps") != null);

    var zero = [_]u8{ 0, 0, 0, 0 };
    _ = c.pwrite(fd, &zero, 4, 28);
    frame.end();
    try std.testing.expectEqual(@as(u64, 0), start("after"));
    shutdown();
}

fn writePid(buf: []u8, pid: c.pid_t) usize {
    var n: u32 = @intCast(pid);
    var tmp: [16]u8 = undefined;
    var i: usize = tmp.len;
    if (n == 0) {
        i -= 1;
        tmp[i] = '0';
    }
    while (n > 0) {
        i -= 1;
        tmp[i] = '0' + @as(u8, @intCast(n % 10));
        n /= 10;
    }
    const digits = tmp[i..];
    @memcpy(buf[0..digits.len], digits);
    return digits.len;
}

fn expectComm(expected: []const u8) !void {
    const fd = c.open("/proc/thread-self/comm", .{});
    if (fd < 0) return error.Open;
    var buf: [64]u8 = undefined;
    const n = c.read(fd, &buf, buf.len);
    _ = std.os.linux.close(fd);
    if (n <= 0) return error.Read;
    var len: usize = @intCast(n);
    if (len > 0 and buf[len - 1] == '\n') len -= 1;
    try std.testing.expectEqualStrings(expected, buf[0..len]);
}
