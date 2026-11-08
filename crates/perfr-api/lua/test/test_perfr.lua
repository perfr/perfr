-- Copyright (c) 2026 The perfr Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

local src = debug.getinfo(1, "S").source
if src:sub(1, 1) == "@" then src = src:sub(2) end
local dir = src:match("^(.*)/") or "."
local root = dir:match("^(.*)/") or "."
package.path = root .. "/?.lua;" .. package.path

local perfr = require("perfr")

local function zero(h)
  return h == nil or h == 0
end

local function fail(msg)
  io.stderr:write("perfr lua test: " .. msg .. "\n")
  os.exit(1)
end

local function expect(cond, msg)
  if not cond then fail(msg) end
end

local configured = os.getenv("PERFR_LIB")
expect(configured and configured ~= "", "set PERFR_LIB")

local function exists(path)
  local f = io.open(path, "rb")
  if f then f:close() return true end
  return false
end

local function comm()
  local f = assert(io.open("/proc/thread-self/comm"))
  local name = f:read("*l")
  f:close()
  return name
end

expect(perfr.name_thread("perfr-lua"), "name_thread")
expect(comm() == "perfr-lua", "comm " .. comm())
expect(perfr.name_thread("0123456789abcdefEXTRA"), "name_thread long")
expect(comm() == "0123456789abcde", "truncated comm " .. comm())

local function quiet()
  expect(zero(perfr.start("quiet")), "start before init")
  expect(perfr.now_ns() == 0, "now before init")
  perfr.stop(0)
  perfr.value("quiet", 1)
  perfr.link(0, 1)
  perfr.span("quiet", 1, 2)
  perfr.with_scope("quiet", function() end)
  perfr.scope("quiet"):stop()
  local rc = perfr.init()
  expect(rc == perfr.E_NOLIB, "init " .. tostring(rc))
  expect(not perfr.available(), "available")
  expect(zero(perfr.start("still")), "start after miss")
end

local MAGIC = "\x47\x53\x5f\x54\x49\x42\x52\x4f"

local function u32le(n)
  return string.char(
    n % 256,
    math.floor(n / 256) % 256,
    math.floor(n / 65536) % 256,
    math.floor(n / 16777216) % 256)
end

local function u32_at(data, offset)
  local b1, b2, b3, b4 = data:byte(offset + 1, offset + 4)
  return b1 + b2 * 256 + b3 * 65536 + b4 * 16777216
end

local function with_library()
  local rc = perfr.init()
  expect(rc == 0, "init " .. tostring(rc))
  expect(perfr.init() == 0, "init again")
  expect(perfr.available(), "available")
  local path = perfr.library_path()
  expect(path and path:find("libperfr", 1, true), "library_path " .. tostring(path))
  local t0 = perfr.now_ns()
  expect(t0 > 0, "now_ns")
  expect(zero(perfr.start("before-capture")), "start while idle")

  local stat = assert(io.open("/proc/self/stat"))
  local pid = stat:read("*n")
  stat:close()
  local segment = "/dev/shm/perfr-scopes-" .. tostring(pid)
  local seg = assert(io.open(segment, "r+b"))
  local head = seg:read(40)
  expect(head:sub(1, 8) == MAGIC, "magic")
  expect(u32_at(head, 8) == 2, "version")
  expect(u32_at(head, 24) == pid, "pid")
  expect(u32_at(head, 28) == 0, "capturing")

  seg:seek("set", 28)
  seg:write(u32le(1))
  seg:flush()

  local handle = perfr.start("lua-probe")
  expect(not zero(handle), "probe handle")
  perfr.value("lua-fps", 59.9)
  local mark = perfr.instant("lua-mark")
  expect(not zero(mark), "instant")
  perfr.link(mark, handle)
  perfr.stop(handle)
  perfr.with_scope("lua-with", function() end)
  local method = perfr.scope("lua-method")
  method:stop()
  method:stop() -- a second stop is a no-op on both interpreters
  local now = perfr.now_ns()
  perfr.span("lua-span", now - 1000, now)
  perfr.span_async("lua-gpu", now - 5000, now - 1000)
  if type(jit) ~= "table" then
    -- <close> is Lua 5.4 syntax, so LuaJIT must not parse it.
    local chunk = assert(load([[
      do
        local _ <close> = perfr.scope("lua-close")
      end
    ]], "close", "t", { perfr = perfr }))
    chunk()
  end

  seg:seek("set", 0)
  local data = seg:read("*a")
  expect(data:find("lua-probe", 1, true), "probe bytes")
  expect(data:find("lua-fps", 1, true), "value bytes")
  expect(data:find("lua-with", 1, true), "with_scope bytes")
  expect(data:find("lua-method", 1, true), "scope():stop() bytes")
  if type(jit) ~= "table" then
    expect(data:find("lua-close", 1, true), "close bytes")
  end

  seg:seek("set", 28)
  seg:write(u32le(0))
  seg:flush()
  seg:close()
  expect(zero(perfr.start("after")), "idle again")
  perfr.shutdown()
  expect(not exists(segment), "unlinked")
end

if exists(configured) then
  with_library()
else
  quiet()
end

print("perfr lua ok (" .. (type(jit) == "table" and "luajit" or "lua") .. ")")
