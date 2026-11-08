// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#if canImport(Glibc)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif
import CPerfrShim
import Foundation

/// A scope that stops when `stop()` runs, and again from `deinit` if it
/// has not been stopped. Zero does nothing.
public final class PerfrScope {
    public let handle: UInt64
    private var stopped = false

    init(handle: UInt64) {
        self.handle = handle
    }

    public func stop() {
        if stopped { return }
        stopped = true
        Perfr.stop(handle)
    }

    deinit {
        stop()
    }
}

/// Manual instrumentation for perfr. Loads `libperfr` the way `perfr.h` does.
/// Calls before a successful `init` return immediately. After that, the
/// library writes nothing unless a service is capturing.
public enum Perfr {
    public static let abiVersion: UInt32 = 1
    public static let eNoLib: Int32 = -1000
    public static let eAbi: Int32 = -1001

    // Only initialize() takes this lock. The loaded library is published
    // once through an acquire/release slot (CPerfrShim) and every other call
    // reads that slot: a lock on the hot path would serialise every
    // instrumented thread on one cache line and time the lock, not the code.
    private static let lock = NSLock()

    /// The published library, or nil. The object is retained for the life of
    /// the process, like the mapping it owns.
    private static var state: Library? {
        guard let raw = perfr_shim_load() else { return nil }
        return Unmanaged<Library>.fromOpaque(raw).takeUnretainedValue()
    }

    public static func initialize() -> Int32 {
        if let state = state { return state.initialize() }
        lock.lock()
        defer { lock.unlock() }
        if let state = state { return state.initialize() }
        switch load() {
        case .err(let code):
            return code
        case .ok(let library):
            perfr_shim_store(UnsafeRawPointer(Unmanaged.passRetained(library).toOpaque()))
            return library.initialize()
        }
    }

    public static func shutdown() {
        state?.shutdown()
    }

    public static var available: Bool {
        state != nil
    }

    public static var libraryPath: String? {
        state?.path
    }

    public static func start(_ name: String) -> UInt64 { call(name) { $0.start } }
    public static func startAsync(_ name: String) -> UInt64 { call(name) { $0.startAsync } }

    public static func stop(_ handle: UInt64) {
        if handle == 0 { return }
        state?.stop(handle)
    }

    public static func instant(_ name: String) -> UInt64 { call(name) { $0.instant } }

    public static func link(from: UInt64, to: UInt64) {
        if from == 0 || to == 0 { return }
        state?.link(from, to)
    }

    public static func value(_ name: String, _ sample: Double) {
        state?.value(name, sample)
    }

    public static func nowNs() -> UInt64 {
        state?.nowNs() ?? 0
    }

    public static func span(_ name: String, startNs: UInt64, endNs: UInt64) {
        state?.span(name, startNs, endNs, async: false)
    }

    public static func spanAsync(_ name: String, startNs: UInt64, endNs: UInt64) {
        state?.span(name, startNs, endNs, async: true)
    }

    public static func scope(_ name: String) -> PerfrScope {
        PerfrScope(handle: start(name))
    }

    public static func scopeAsync(_ name: String) -> PerfrScope {
        PerfrScope(handle: startAsync(name))
    }

    public static func withScope(_ name: String, _ body: () throws -> Void) rethrows {
        let opened = scope(name)
        defer { opened.stop() }
        try body()
    }

    /// OS thread name. `libperfr` has no thread-name call. Linux keeps 15 bytes.
    public static func nameThread(_ name: String) -> Bool {
        let bytes = utf8Prefix(name, 15)
        return bytes.withUnsafeBufferPointer { raw in
            guard let base = raw.baseAddress else { return false }
            return perfr_shim_set_thread_name(base) == 0
        }
    }

    private static func call(_ name: String, _ pick: (Library) -> StartFn) -> UInt64 {
        guard let library = state else { return 0 }
        return name.withCString { ptr in
            pick(library)(ptr, name.utf8.count)
        }
    }

    private static func utf8Prefix(_ name: String, _ max: Int) -> [CChar] {
        let raw = Array(name.utf8)
        var count = min(raw.count, max)
        while count > 0 && count < raw.count && raw[count] & 0xC0 == 0x80 {
            count -= 1
        }
        var out = raw.prefix(count).map { CChar(bitPattern: $0) }
        out.append(0)
        return out
    }

    private static func load() -> LoadResult {
        if let forced = getenvString("PERFR_LIB"), !forced.isEmpty {
            guard fileExists(forced), let handle = openLib(forced) else { return .err(eNoLib) }
            guard let library = Library(handle: handle, path: forced) else {
                dlclose(handle)
                return .err(eAbi)
            }
            return .ok(library)
        }
        for path in candidates() {
            if !fileExists(path) && path.contains("/") { continue }
            guard let handle = openLib(path) else { continue }
            guard let library = Library(handle: handle, path: path) else {
                dlclose(handle)
                return .err(eAbi)
            }
            return .ok(library)
        }
        return .err(eNoLib)
    }

    private static func candidates() -> [String] {
        var list: [String] = []
        if let service = which(serviceFile()), let dir = parent(service) {
            list.append(dir + "/" + libFile())
        }
        if let home = getenvString(homeKey()) {
            if isWindows {
                list.append(home + "/.perfr/bin/" + libFile())
            } else {
                list.append(home + "/.local/bin/" + libFile())
                list.append(home + "/.perfr/bin/" + libFile())
            }
        }
        if let exe = exePath(), let dir = parent(exe) {
            list.append(dir + "/" + libFile())
        }
        list.append(libFile())
        return list
    }

    private static var isWindows: Bool {
        #if os(Windows)
        return true
        #else
        return false
        #endif
    }

    private static func libFile() -> String {
        #if os(Windows)
        return "libperfr.dll"
        #elseif os(macOS)
        return "libperfr.dylib"
        #else
        return "libperfr.so"
        #endif
    }

    private static func serviceFile() -> String {
        #if os(Windows)
        return "perfr.exe"
        #else
        return "perfr"
        #endif
    }

    private static func homeKey() -> String {
        #if os(Windows)
        return "USERPROFILE"
        #else
        return "HOME"
        #endif
    }
}

private enum LoadResult {
    case ok(Library)
    case err(Int32)
}

private typealias StartFn = @convention(c) (UnsafePointer<CChar>?, Int) -> UInt64
private typealias StopFn = @convention(c) (UInt64) -> Void
private typealias ValueFn = @convention(c) (UnsafePointer<CChar>?, Int, Double) -> Void
private typealias SpanFn = @convention(c) (UnsafePointer<CChar>?, Int, UInt64, UInt64) -> Void
private typealias InitFn = @convention(c) () -> Int32
private typealias ShutdownFn = @convention(c) () -> Void
private typealias NowFn = @convention(c) () -> UInt64
private typealias LinkFn = @convention(c) (UInt64, UInt64) -> Void
private typealias TableFn = @convention(c) (UInt32) -> UnsafeRawPointer?

private final class Library {
    let path: String
    let handle: UnsafeMutableRawPointer
    let initFn: InitFn
    let shutdownFn: ShutdownFn
    let start: StartFn
    let startAsync: StartFn
    let stopFn: StopFn
    let instant: StartFn
    let linkFn: LinkFn
    let valueFn: ValueFn
    let nowFn: NowFn
    let spanFn: SpanFn
    let spanAsyncFn: SpanFn

    init?(handle: UnsafeMutableRawPointer, path: String) {
        guard let get: TableFn = bind(handle, "perfr_api_table_v1"),
              let table = get(1) else { return nil }
        // Two UInt32 then twelve pointers: the table's size at this word size.
        let tableSize = UInt32(8 + 12 * MemoryLayout<UnsafeRawPointer>.size)
        let head = table.assumingMemoryBound(to: UInt32.self)
        if head[0] != Perfr.abiVersion || head[1] < tableSize { return nil }
        guard let initFn: InitFn = bind(handle, "perfr_init"),
              let shutdownFn: ShutdownFn = bind(handle, "perfr_shutdown"),
              let start: StartFn = bind(handle, "perfr_start"),
              let startAsync: StartFn = bind(handle, "perfr_start_async"),
              let stopFn: StopFn = bind(handle, "perfr_stop"),
              let instant: StartFn = bind(handle, "perfr_instant"),
              let linkFn: LinkFn = bind(handle, "perfr_link"),
              let valueFn: ValueFn = bind(handle, "perfr_value"),
              let nowFn: NowFn = bind(handle, "perfr_now_ns"),
              let spanFn: SpanFn = bind(handle, "perfr_span"),
              let spanAsyncFn: SpanFn = bind(handle, "perfr_span_async") else { return nil }
        self.handle = handle
        self.path = path
        self.initFn = initFn
        self.shutdownFn = shutdownFn
        self.start = start
        self.startAsync = startAsync
        self.stopFn = stopFn
        self.instant = instant
        self.linkFn = linkFn
        self.valueFn = valueFn
        self.nowFn = nowFn
        self.spanFn = spanFn
        self.spanAsyncFn = spanAsyncFn
    }

    func initialize() -> Int32 { initFn() }
    func shutdown() { shutdownFn() }
    func stop(_ handle: UInt64) { stopFn(handle) }
    func link(_ from: UInt64, _ to: UInt64) { linkFn(from, to) }

    func value(_ name: String, _ sample: Double) {
        name.withCString { ptr in valueFn(ptr, name.utf8.count, sample) }
    }

    func nowNs() -> UInt64 { nowFn() }

    func span(_ name: String, _ startNs: UInt64, _ endNs: UInt64, async: Bool) {
        name.withCString { ptr in
            let fn = async ? spanAsyncFn : spanFn
            fn(ptr, name.utf8.count, startNs, endNs)
        }
    }
}

private func bind<T>(_ handle: UnsafeMutableRawPointer, _ name: String) -> T? {
    guard let symbol = dlsym(handle, name) else { return nil }
    return unsafeBitCast(symbol, to: T.self)
}

private func openLib(_ path: String) -> UnsafeMutableRawPointer? {
    path.withCString { dlopen($0, RTLD_NOW) }
}

private func fileExists(_ path: String) -> Bool {
    path.withCString { access($0, F_OK) == 0 }
}

private func getenvString(_ name: String) -> String? {
    guard let raw = getenv(name) else { return nil }
    return String(cString: raw)
}

private func which(_ file: String) -> String? {
    guard let path = getenvString("PATH") else { return nil }
    for dir in path.split(separator: ":", omittingEmptySubsequences: true) {
        let candidate = dir + "/" + file
        if fileExists(String(candidate)) { return String(candidate) }
    }
    return nil
}

private func parent(_ path: String) -> String? {
    guard let slash = path.lastIndex(of: "/") else { return nil }
    return String(path[..<slash])
}

private func exePath() -> String? {
    #if os(Linux)
    var buf = [CChar](repeating: 0, count: 4096)
    let n = readlink("/proc/self/exe", &buf, buf.count - 1)
    if n <= 0 { return nil }
    return String(cString: buf)
    #else
    return nil
    #endif
}
