// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#if canImport(Glibc)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif
import Foundation
import XCTest
@testable import Perfr

final class PerfrTests: XCTestCase {
    func testNameThreadSetsLinuxComm() {
        #if os(Linux)
        XCTAssertTrue(Perfr.nameThread("perfr-sw"))
        XCTAssertEqual(comm(), "perfr-sw")
        XCTAssertTrue(Perfr.nameThread("0123456789abcdefEXTRA"))
        XCTAssertEqual(comm(), "0123456789abcde")
        #else
        _ = Perfr.nameThread("perfr-sw")
        #endif
    }

    func testCallsFollowTheLibrary() throws {
        let configured = ProcessInfo.processInfo.environment["PERFR_LIB"] ?? ""
        if !configured.isEmpty && FileManager.default.fileExists(atPath: configured) {
            try withLibrary(configured)
        } else {
            withoutLibrary()
        }
    }

    private func withoutLibrary() {
        XCTAssertFalse(Perfr.available)
        XCTAssertEqual(Perfr.start("quiet"), 0)
        XCTAssertEqual(Perfr.nowNs(), 0)
        Perfr.withScope("quiet") {}
        Perfr.stop(0)
        Perfr.value("quiet", 1)
        let rc = Perfr.initialize()
        XCTAssertEqual(rc, Perfr.eNoLib)
        XCTAssertFalse(Perfr.available)
        XCTAssertEqual(Perfr.start("still"), 0)
    }

    private func withLibrary(_ expected: String) throws {
        XCTAssertEqual(Perfr.initialize(), 0)
        XCTAssertEqual(Perfr.initialize(), 0)
        XCTAssertTrue(Perfr.available)
        XCTAssertEqual(Perfr.libraryPath, expected)
        let t0 = Perfr.nowNs()
        XCTAssertGreaterThan(t0, 0)
        XCTAssertEqual(Perfr.start("before-capture"), 0)

        #if os(Linux)
        let pid = Int(getpid())
        let segment = "/dev/shm/perfr-scopes-\(pid)"
        let fd = open(segment, O_RDWR)
        XCTAssertGreaterThanOrEqual(fd, 0)
        defer { _ = close(fd) }
        var head = [UInt8](repeating: 0, count: 40)
        XCTAssertEqual(head.withUnsafeMutableBytes { pread(fd, $0.baseAddress, 40, 0) }, 40)
        XCTAssertEqual(readU64(head, 0), 0x4F524249545F5347)
        XCTAssertEqual(readU32(head, 8), 2)
        XCTAssertEqual(readU32(head, 24), UInt32(pid))
        XCTAssertEqual(readU32(head, 28), 0)

        var one: UInt32 = 1
        XCTAssertEqual(withUnsafeBytes(of: &one) { pwrite(fd, $0.baseAddress, 4, 28) }, 4)
        let frame = Perfr.scope("swift-probe")
        XCTAssertNotEqual(frame.handle, 0)
        Perfr.value("swift-fps", 59.9)
        let mark = Perfr.instant("swift-mark")
        XCTAssertNotEqual(mark, 0)
        Perfr.link(from: mark, to: frame.handle)
        frame.stop()
        Perfr.withScope("swift-with") {}
        let now = Perfr.nowNs()
        Perfr.span("swift-span", startNs: now - 1000, endNs: now)
        Perfr.spanAsync("swift-gpu", startNs: now - 5000, endNs: now - 1000)

        let length = lseek(fd, 0, SEEK_END)
        XCTAssertGreaterThan(length, 0)
        var all = [UInt8](repeating: 0, count: Int(length))
        var got: Int = 0
        let total = all.count
        while got < total {
            let offset = got
            let n = all.withUnsafeMutableBytes { raw -> Int in
                let base = raw.baseAddress!.advanced(by: offset)
                return pread(fd, base, total - offset, off_t(offset))
            }
            XCTAssertGreaterThan(n, 0)
            got += n
        }
        XCTAssertTrue(contains(all, Array("swift-probe".utf8)))
        XCTAssertTrue(contains(all, Array("swift-fps".utf8)))

        var zero: UInt32 = 0
        _ = withUnsafeBytes(of: &zero) { pwrite(fd, $0.baseAddress, 4, 28) }
        XCTAssertEqual(Perfr.start("after"), 0)
        Perfr.shutdown()
        XCTAssertFalse(FileManager.default.fileExists(atPath: segment))
        #endif
    }
}

#if os(Linux)
private func comm() -> String {
    let fd = open("/proc/thread-self/comm", O_RDONLY)
    var buf = [UInt8](repeating: 0, count: 64)
    let n = buf.withUnsafeMutableBytes { read(fd, $0.baseAddress, 64) }
    _ = close(fd)
    let text = String(decoding: buf.prefix(max(0, n)), as: UTF8.self)
    return text.trimmingCharacters(in: .whitespacesAndNewlines)
}

private func readU32(_ bytes: [UInt8], _ offset: Int) -> UInt32 {
    UInt32(bytes[offset])
        | (UInt32(bytes[offset + 1]) << 8)
        | (UInt32(bytes[offset + 2]) << 16)
        | (UInt32(bytes[offset + 3]) << 24)
}

private func readU64(_ bytes: [UInt8], _ offset: Int) -> UInt64 {
    var value: UInt64 = 0
    for i in 0..<8 {
        value |= UInt64(bytes[offset + i]) << (8 * i)
    }
    return value
}

private func contains(_ haystack: [UInt8], _ needle: [UInt8]) -> Bool {
    if needle.isEmpty || haystack.count < needle.count { return false }
    let last = haystack.count - needle.count
    for i in 0...last {
        var matched = true
        for j in 0..<needle.count where haystack[i + j] != needle[j] {
            matched = false
            break
        }
        if matched { return true }
    }
    return false
}
#endif
