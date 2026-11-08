-- Copyright (c) 2026 The perfr Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

-- One require. LuaJIT gets the FFI module. Lua 5.4 loads perfr_native.so,
-- which includes perfr.h and does not link libperfr.

local function script_dir()
  local src = debug.getinfo(1, "S").source
  if src:sub(1, 1) == "@" then src = src:sub(2) end
  return src:match("^(.*)/") or "."
end

if type(jit) == "table" then
  local chunk = assert(loadfile(script_dir() .. "/perfr_ffi.lua"))
  return chunk()
end

local dir = script_dir()
local openers = {
  dir .. "/perfr_native.so",
  dir .. "/libperfr_native.so",
}
local err
for _, path in ipairs(openers) do
  local open, why = package.loadlib(path, "luaopen_perfr")
  if open then return open() end
  err = why
end
error("perfr: build perfr_native.so (make -C " .. dir .. "). " .. tostring(err))
