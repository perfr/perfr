-- Copyright (c) 2026 The perfr Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

-- LuaJIT FFI binding. Same calls as perfr.h, loaded with ffi.load.
-- Handles stay uint64 cdata so a tid above 2^21 does not lose bits in a double.

local ffi = require("ffi")

ffi.cdef[[
  void *perfr_api_table_v1(uint32_t abi_version);
  int perfr_init(void);
  void perfr_shutdown(void);
  uint64_t perfr_start(const char *name, size_t name_len);
  uint64_t perfr_start_async(const char *name, size_t name_len);
  void perfr_stop(uint64_t scope);
  uint64_t perfr_instant(const char *name, size_t name_len);
  void perfr_link(uint64_t from, uint64_t to);
  void perfr_value(const char *name, size_t name_len, double value);
  uint64_t perfr_now_ns(void);
  void perfr_span(const char *name, size_t name_len, uint64_t start_ns, uint64_t end_ns);
  void perfr_span_async(const char *name, size_t name_len, uint64_t start_ns, uint64_t end_ns);
  ssize_t readlink(const char *path, char *buf, size_t n);
  int access(const char *path, int mode);
  typedef unsigned long pthread_t;
  pthread_t pthread_self(void);
  int pthread_setname_np(pthread_t thread, const char *name);
]]

local M = {}
M.ABI_VERSION = 1
M.E_NOLIB = -1000
M.E_ABI = -1001

local lib = nil
local lib_path = nil
local lib_file = jit.os == "OSX" and "libperfr.dylib" or (jit.os == "Windows" and "libperfr.dll" or "libperfr.so")
local service = jit.os == "Windows" and "perfr.exe" or "perfr"

local function exists(path)
  return ffi.C.access(path, 0) == 0
end

local function readlink(path)
  local buf = ffi.new("char[4096]")
  local n = ffi.C.readlink(path, buf, 4095)
  if n <= 0 then return nil end
  return ffi.string(buf, n)
end

local function candidates()
  local env = os.getenv("PERFR_LIB")
  if env and env ~= "" then return { env }, true end
  local list = {}
  local function add(path) list[#list + 1] = path end
  local path = os.getenv("PATH") or ""
  local sep = jit.os == "Windows" and ";" or ":"
  local start = 1
  while start <= #path + 1 do
    local stop = string.find(path, sep, start, true)
    local dir = string.sub(path, start, (stop or #path + 1) - 1)
    if dir ~= "" and exists(dir .. "/" .. service) then add(dir .. "/" .. lib_file) end
    if not stop then break end
    start = stop + 1
  end
  local home = os.getenv(jit.os == "Windows" and "USERPROFILE" or "HOME") or ""
  if home ~= "" then
    if jit.os == "Windows" then
      add(home .. "\\.perfr\\bin\\" .. lib_file)
    else
      add(home .. "/.local/bin/" .. lib_file)
      add(home .. "/.perfr/bin/" .. lib_file)
    end
  end
  local exe = readlink("/proc/self/exe")
  if exe then
    local dir = exe:match("^(.*)/")
    if dir then add(dir .. "/" .. lib_file) end
  end
  add(lib_file)
  return list, false
end

local function bind(opened, path)
  local table = opened.perfr_api_table_v1(M.ABI_VERSION)
  if table == nil then return false end
  local head = ffi.cast("uint32_t *", table)
  if tonumber(head[0]) ~= M.ABI_VERSION or tonumber(head[1]) < 104 then return false end
  lib = opened
  lib_path = path
  return true
end

local function load()
  if lib ~= nil then return lib end
  local list, exclusive = candidates()
  for _, path in ipairs(list) do
    if exclusive and not exists(path) then return nil, M.E_NOLIB end
    local ok, opened = pcall(ffi.load, path)
    if ok then
      if bind(opened, path) then return lib end
      return nil, M.E_ABI
    elseif exclusive then
      return nil, M.E_NOLIB
    end
  end
  return nil, M.E_NOLIB
end

local function bytes(name)
  if type(name) == "string" then return name end
  return tostring(name)
end

function M.init()
  local loaded, err = load()
  if not loaded then return err end
  return lib.perfr_init()
end

function M.shutdown()
  if lib then lib.perfr_shutdown() end
end

function M.available()
  return lib ~= nil
end

function M.library_path()
  return lib_path
end

function M.start(name)
  if not lib then return 0 end
  local b = bytes(name)
  return lib.perfr_start(b, #b)
end

function M.start_async(name)
  if not lib then return 0 end
  local b = bytes(name)
  return lib.perfr_start_async(b, #b)
end

function M.stop(handle)
  if lib and handle ~= nil and handle ~= 0 then lib.perfr_stop(handle) end
end

function M.instant(name)
  if not lib then return 0 end
  local b = bytes(name)
  return lib.perfr_instant(b, #b)
end

function M.link(from, to)
  if lib and from ~= 0 and to ~= 0 then lib.perfr_link(from, to) end
end

function M.value(name, v)
  if not lib then return end
  local b = bytes(name)
  lib.perfr_value(b, #b, v)
end

function M.now_ns()
  if not lib then return 0 end
  return lib.perfr_now_ns()
end

function M.span(name, start_ns, end_ns)
  if not lib then return end
  local b = bytes(name)
  lib.perfr_span(b, #b, start_ns, end_ns)
end

function M.span_async(name, start_ns, end_ns)
  if not lib then return end
  local b = bytes(name)
  lib.perfr_span_async(b, #b, start_ns, end_ns)
end

function M.scope(name)
  local handle = M.start(name)
  local scope = { handle = handle }
  function scope:stop()
    M.stop(self.handle)
    self.handle = 0
  end
  return scope
end

function M.with_scope(name, fn, async)
  local handle = async and M.start_async(name) or M.start(name)
  local ok, err = pcall(fn)
  M.stop(handle)
  if not ok then error(err, 0) end
end

function M.name_thread(name)
  if type(name) ~= "string" or #name == 0 then return false end
  if #name > 15 then name = name:sub(1, 15) end
  if jit.os ~= "Linux" then return false end
  return ffi.C.pthread_setname_np(ffi.C.pthread_self(), name) == 0
end

return M
