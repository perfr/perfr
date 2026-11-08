# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

using Test

include(joinpath(@__DIR__, "..", "src", "Perfr.jl"))
using .Perfr

const MAGIC = UInt8[0x47, 0x53, 0x5f, 0x54, 0x49, 0x42, 0x52, 0x4f]

function u32_at(data::Vector{UInt8}, offset::Int)
    UInt32(data[offset + 1]) |
    UInt32(data[offset + 2]) << 8 |
    UInt32(data[offset + 3]) << 16 |
    UInt32(data[offset + 4]) << 24
end

@testset "name_thread" begin
    @test name_thread("perfr-jl")
    @test readchomp("/proc/thread-self/comm") == "perfr-jl"
    @test name_thread("0123456789abcdefEXTRA")
    @test readchomp("/proc/thread-self/comm") == "0123456789abcde"
end

@testset "calls" begin
    configured = get(ENV, "PERFR_LIB", "")
    @test !isempty(configured)
    if !isfile(configured)
        @test start("quiet") == 0
        @test now_ns() == 0
        scope("quiet") do
        end
        stop(UInt64(0))
        value("quiet", 1)
        @test init() == E_NOLIB
        @test !available()
        @test start("still") == 0
    else
        @test init() == 0
        @test init() == 0
        @test available()
        @test occursin("libperfr", something(library_path(), ""))
        t0 = now_ns()
        @test t0 > 0
        @test start("before-capture") == 0
        segment = "/dev/shm/perfr-scopes-$(getpid())"
        @test isfile(segment)
        open(segment, "r+") do io
            head = read(io, 40)
            @test head[1:8] == MAGIC
            @test u32_at(head, 8) == 2
            @test u32_at(head, 24) == UInt32(getpid())
            seek(io, 28)
            write(io, UInt32(1))
            flush(io)
        end
        handle = start("julia-probe")
        @test handle != 0
        value("julia-fps", 59.9)
        mark = instant("julia-mark")
        @test mark != 0
        link(mark, handle)
        stop(handle)
        got = scope("julia-with") do
            7
        end
        @test got == 7
        now = now_ns()
        span("julia-span", now - UInt64(1000), now)
        span_async("julia-gpu", now - UInt64(5000), now - UInt64(1000))
        data = read(segment)
        @test findfirst(Vector{UInt8}("julia-probe"), data) !== nothing
        @test findfirst(Vector{UInt8}("julia-fps"), data) !== nothing
        open(segment, "r+") do io
            seek(io, 28)
            write(io, UInt32(0))
            flush(io)
        end
        @test start("after") == 0
        shutdown()
        @test !isfile(segment)
    end
end
