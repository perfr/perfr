# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Manual instrumentation for perfr. `ccall` into `libperfr`, found the same
way `perfr.h` finds it. Calls are no-ops until `init` loads a library, and
the library writes nothing unless a service is capturing.

```julia
Perfr.init()
Perfr.scope("frame") do
    Perfr.value("fps", 59.9)
end
```
"""
module Perfr

using Libdl

export ABI_VERSION, E_NOLIB, E_ABI
export init, shutdown, available, library_path
export start, start_async, stop, instant, link, value, now_ns, span, span_async
export scope, scope_async, name_thread

const ABI_VERSION = 1
const E_NOLIB = Int32(-1000)
const E_ABI = Int32(-1001)

mutable struct Api
    lib::Ptr{Cvoid}
    path::String
    fn::Dict{Symbol,Ptr{Cvoid}}
end

const STATE = Ref{Union{Api,Nothing}}(nothing)
const LOAD_ERROR = Ref{Int32}(E_NOLIB)

function _libname()
    Sys.iswindows() ? "libperfr.dll" : Sys.isapple() ? "libperfr.dylib" : "libperfr.so"
end

function _exe_dir()
    buf = Vector{UInt8}(undef, 4096)
    n = ccall(:readlink, Cssize_t, (Cstring, Ptr{UInt8}, Csize_t), "/proc/self/exe", buf, length(buf))
    n > 0 || return nothing
    path = String(buf[1:n])
    return dirname(path)
end

function _candidates()
    env = get(ENV, "PERFR_LIB", "")
    if !isempty(env)
        return String[env], true
    end
    list = String[]
    lib = _libname()
    service = Sys.iswindows() ? "perfr.exe" : "perfr"
    exe = Sys.which(service)
    if exe !== nothing
        push!(list, joinpath(dirname(exe), lib))
    end
    home = homedir()
    if Sys.iswindows()
        push!(list, joinpath(home, ".perfr", "bin", lib))
    else
        push!(list, joinpath(home, ".local", "bin", lib))
        push!(list, joinpath(home, ".perfr", "bin", lib))
    end
    exedir = _exe_dir()
    if exedir !== nothing
        push!(list, joinpath(exedir, lib))
    end
    push!(list, lib)
    return list, false
end

function _symbol(api::Api, name::Symbol)
    get!(api.fn, name) do
        dlsym(api.lib, name)
    end
end

function _load()
    STATE[] !== nothing && return STATE[]
    list, exclusive = _candidates()
    for path in list
        if exclusive && !isfile(path)
            LOAD_ERROR[] = E_NOLIB
            return nothing
        end
        if !exclusive && occursin("/", path) && !isfile(path)
            continue
        end
        lib = try
            dlopen(path, RTLD_NOW)
        catch
            exclusive && return nothing
            continue
        end
        table = ccall(dlsym(lib, :perfr_api_table_v1), Ptr{Cvoid}, (UInt32,), UInt32(ABI_VERSION))
        if table == C_NULL
            dlclose(lib)
            LOAD_ERROR[] = E_ABI
            return nothing
        end
        abi = unsafe_load(Ptr{UInt32}(table))
        size = unsafe_load(Ptr{UInt32}(table) + 1)
        if abi != ABI_VERSION || size < 104
            dlclose(lib)
            LOAD_ERROR[] = E_ABI
            return nothing
        end
        STATE[] = Api(lib, path, Dict{Symbol,Ptr{Cvoid}}())
        return STATE[]
    end
    return nothing
end

function init()
    api = _load()
    api === nothing && return LOAD_ERROR[]
    ccall(_symbol(api, :perfr_init), Cint, ())
end

function shutdown()
    api = STATE[]
    api === nothing && return
    ccall(_symbol(api, :perfr_shutdown), Cvoid, ())
end

available() = STATE[] !== nothing
library_path() = (api = STATE[]; api === nothing ? nothing : api.path)

function start(name::AbstractString)
    api = STATE[]
    api === nothing && return UInt64(0)
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_start), UInt64, (Ptr{UInt8}, Csize_t), pointer(s), sizeof(s))
end

function start_async(name::AbstractString)
    api = STATE[]
    api === nothing && return UInt64(0)
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_start_async), UInt64, (Ptr{UInt8}, Csize_t), pointer(s), sizeof(s))
end

function stop(handle::UInt64)
    handle == 0 && return
    api = STATE[]
    api === nothing && return
    ccall(_symbol(api, :perfr_stop), Cvoid, (UInt64,), handle)
end

function instant(name::AbstractString)
    api = STATE[]
    api === nothing && return UInt64(0)
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_instant), UInt64, (Ptr{UInt8}, Csize_t), pointer(s), sizeof(s))
end

function link(from::UInt64, to::UInt64)
    (from == 0 || to == 0) && return
    api = STATE[]
    api === nothing && return
    ccall(_symbol(api, :perfr_link), Cvoid, (UInt64, UInt64), from, to)
end

function value(name::AbstractString, sample::Real)
    api = STATE[]
    api === nothing && return
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_value), Cvoid, (Ptr{UInt8}, Csize_t, Cdouble), pointer(s), sizeof(s), Float64(sample))
end

function now_ns()
    api = STATE[]
    api === nothing && return UInt64(0)
    ccall(_symbol(api, :perfr_now_ns), UInt64, ())
end

function span(name::AbstractString, start_ns::UInt64, end_ns::UInt64)
    api = STATE[]
    api === nothing && return
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_span), Cvoid, (Ptr{UInt8}, Csize_t, UInt64, UInt64), pointer(s), sizeof(s), start_ns, end_ns)
end

function span_async(name::AbstractString, start_ns::UInt64, end_ns::UInt64)
    api = STATE[]
    api === nothing && return
    s = String(name)
    GC.@preserve s ccall(_symbol(api, :perfr_span_async), Cvoid, (Ptr{UInt8}, Csize_t, UInt64, UInt64), pointer(s), sizeof(s), start_ns, end_ns)
end

"""`scope("frame") do ... end` stops the scope when the block returns."""
function scope(f::Function, name::AbstractString)
    handle = start(name)
    try
        return f()
    finally
        stop(handle)
    end
end

function scope_async(f::Function, name::AbstractString)
    handle = start_async(name)
    try
        return f()
    finally
        stop(handle)
    end
end

"""OS thread name. libperfr has no thread-name call. Linux keeps 15 bytes."""
function name_thread(name::AbstractString)
    s = String(name)
    nbytes = sizeof(s)
    if nbytes > 15
        raw = codeunits(s)
        n = 15
        while n > 0 && n < length(raw) && (raw[n + 1] & 0xc0) == 0x80
            n -= 1
        end
        s = String(raw[1:n])
    end
    buf = Vector{UInt8}(codeunits(s))
    push!(buf, 0)
    GC.@preserve buf begin
        rc = ccall(:pthread_setname_np, Cint, (Culong, Ptr{UInt8}),
                   ccall(:pthread_self, Culong, ()), pointer(buf))
        return rc == 0
    end
end

end # module
