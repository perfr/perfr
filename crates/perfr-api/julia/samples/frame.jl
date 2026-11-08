# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

include(joinpath(@__DIR__, "..", "src", "Perfr.jl"))
using .Perfr

rc = init()
name_thread("perfr-jl")
println("sample init=$rc library=$(library_path())")
scope("frame") do
    instant("vsync")
    scope("update") do
    end
    value("fps", 60)
    now = now_ns()
    if now != 0
        span_async("gpu: shadow pass", now - UInt64(2_000_000), now - UInt64(500_000))
    end
end
shutdown()
