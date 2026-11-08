/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* Lua 5.4 module. perfr.h loads libperfr; this file does not link it.
 * Build: see the Makefile. luaopen_perfr is the entry, whatever the .so is named. */

#include <lauxlib.h>
#include <lua.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "perfr.h"

#if !defined(_WIN32)
#include <pthread.h>
#endif

#define SCOPE_MT "perfr.scope"

static void push_handle(lua_State *L, uint64_t handle) {
  if (handle <= (uint64_t)LUA_MAXINTEGER) {
    lua_pushinteger(L, (lua_Integer)handle);
    return;
  }
  char buf[32];
  snprintf(buf, sizeof buf, "%016llx", (unsigned long long)handle);
  lua_pushstring(L, buf);
}

static uint64_t check_handle(lua_State *L, int idx) {
  uint64_t *box;
  if (lua_isinteger(L, idx)) return (uint64_t)lua_tointeger(L, idx);
  if ((box = luaL_testudata(L, idx, SCOPE_MT)) != NULL) return *box;
  if (lua_type(L, idx) == LUA_TSTRING) return strtoull(lua_tostring(L, idx), NULL, 16);
  return 0;
}

static int l_init(lua_State *L) {
  lua_pushinteger(L, perfr_init());
  return 1;
}

static int l_shutdown(lua_State *L) {
  (void)L;
  perfr_shutdown();
  return 0;
}

static int l_available(lua_State *L) {
  lua_pushboolean(L, perfr_available());
  return 1;
}

/* On Linux the loaded path is the one in /proc/self/maps. perfr.h does not
 * remember it. Elsewhere this is nil. */
static int l_library_path(lua_State *L) {
  FILE *file = fopen("/proc/self/maps", "r");
  char line[1024];
  if (!file) {
    lua_pushnil(L);
    return 1;
  }
  while (fgets(line, sizeof line, file)) {
    char *path = strstr(line, "libperfr.so");
    char *start;
    char *end;
    if (!path) continue;
    start = strchr(line, '/');
    if (!start) continue;
    end = strchr(start, '\n');
    if (end) *end = 0;
    lua_pushstring(L, start);
    fclose(file);
    return 1;
  }
  fclose(file);
  lua_pushnil(L);
  return 1;
}

static int l_start(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  push_handle(L, perfr_start(name, len));
  return 1;
}

static int l_start_async(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  push_handle(L, perfr_start_async(name, len));
  return 1;
}

static int l_stop(lua_State *L) {
  perfr_stop(check_handle(L, 1));
  return 0;
}

static int l_instant(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  push_handle(L, perfr_instant(name, len));
  return 1;
}

static int l_link(lua_State *L) {
  perfr_link(check_handle(L, 1), check_handle(L, 2));
  return 0;
}

static int l_value(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  perfr_value(name, len, luaL_checknumber(L, 2));
  return 0;
}

static int l_now_ns(lua_State *L) {
  push_handle(L, perfr_now_ns());
  return 1;
}

static int l_span(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  perfr_span(name, len, (uint64_t)luaL_checkinteger(L, 2), (uint64_t)luaL_checkinteger(L, 3));
  return 0;
}

static int l_span_async(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  perfr_span_async(name, len, (uint64_t)luaL_checkinteger(L, 2), (uint64_t)luaL_checkinteger(L, 3));
  return 0;
}

static int scope_close(lua_State *L) {
  uint64_t *box = luaL_checkudata(L, 1, SCOPE_MT);
  perfr_stop(*box);
  *box = 0;
  return 0;
}

static int l_scope(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  int async = lua_toboolean(L, 2);
  uint64_t *box = lua_newuserdata(L, sizeof *box);
  *box = async ? perfr_start_async(name, len) : perfr_start(name, len);
  luaL_setmetatable(L, SCOPE_MT);
  return 1;
}

static int l_with_scope(lua_State *L) {
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  int async = lua_toboolean(L, 3);
  uint64_t handle;
  luaL_checktype(L, 2, LUA_TFUNCTION);
  handle = async ? perfr_start_async(name, len) : perfr_start(name, len);
  lua_pushvalue(L, 2);
  if (lua_pcall(L, 0, 0, 0) != LUA_OK) {
    perfr_stop(handle);
    return lua_error(L);
  }
  perfr_stop(handle);
  return 0;
}

static int l_name_thread(lua_State *L) {
#if defined(_WIN32)
  (void)L;
  lua_pushboolean(L, 0);
  return 1;
#else
  size_t len = 0;
  const char *name = luaL_checklstring(L, 1, &len);
  char buf[16];
  int rc;
  if (len > 15) len = 15;
  memcpy(buf, name, len);
  buf[len] = 0;
#if defined(__APPLE__)
  rc = pthread_setname_np(buf);
#else
  rc = pthread_setname_np(pthread_self(), buf);
#endif
  lua_pushboolean(L, rc == 0);
  return 1;
#endif
}

static const luaL_Reg kFuncs[] = {
    {"init", l_init},
    {"shutdown", l_shutdown},
    {"available", l_available},
    {"library_path", l_library_path},
    {"start", l_start},
    {"start_async", l_start_async},
    {"stop", l_stop},
    {"instant", l_instant},
    {"link", l_link},
    {"value", l_value},
    {"now_ns", l_now_ns},
    {"span", l_span},
    {"span_async", l_span_async},
    {"scope", l_scope},
    {"with_scope", l_with_scope},
    {"name_thread", l_name_thread},
    {NULL, NULL},
};

int luaopen_perfr(lua_State *L) {
  luaL_newmetatable(L, SCOPE_MT);
  lua_pushcfunction(L, scope_close);
  lua_setfield(L, -2, "__close");
  lua_pushcfunction(L, scope_close);
  lua_setfield(L, -2, "__gc");
  lua_pushcfunction(L, l_stop);
  lua_setfield(L, -2, "stop");
  lua_pop(L, 1);

  luaL_newlib(L, kFuncs);
  lua_pushinteger(L, PERFR_API_ABI_VERSION);
  lua_setfield(L, -2, "ABI_VERSION");
  lua_pushinteger(L, PERFR_E_NOLIB);
  lua_setfield(L, -2, "E_NOLIB");
  lua_pushinteger(L, PERFR_E_ABI);
  lua_setfield(L, -2, "E_ABI");
  return 1;
}
