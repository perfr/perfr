-- Copyright (c) 2026 The perfr Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

-- lua samples/sample.lua   (Lua 5.4, after `make`)
-- luajit samples/sample.lua

local src = debug.getinfo(1, "S").source
if src:sub(1, 1) == "@" then src = src:sub(2) end
local dir = src:match("^(.*)/") or "."
local root = dir:match("^(.*)/") or "."
package.path = root .. "/?.lua;" .. package.path

local perfr = require("perfr")
local seconds = tonumber(arg[1]) or 3
local rc = perfr.init()
perfr.name_thread("perfr-lua")
print(string.format("sample init=%s library=%s seconds=%s", tostring(rc), tostring(perfr.library_path()), tostring(seconds)))

local frames = 0
local t0 = os.clock()
while os.clock() - t0 < seconds do
  perfr.with_scope("frame", function()
    perfr.instant("vsync")
    perfr.with_scope("update", function() end)
    perfr.value("fps", 60)
  end)
  frames = frames + 1
  local spin = os.clock()
  while os.clock() - spin < 0.008 do end
end
print("sample frames=" .. frames)
perfr.shutdown()
