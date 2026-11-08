/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* EM_JS stand-in for perfr_web.h. Include this instead of the linked header
 * when the Rust staticlib is not linked. The page sets `Module.perfr` to the
 * `@perfr/web` profiler (`takeFlush` / `takeSnapshot` are on that object).
 *
 * Compile with `-include perfr_web_js.h` or include it before perfr_web.h
 * and do not also include perfr_web.h.
 */

#ifndef PERFR_WEB_JS_H
#define PERFR_WEB_JS_H

#include <stddef.h>
#include <stdint.h>

#ifdef __EMSCRIPTEN__
#include <emscripten.h>

EM_JS(uint32_t, perfr_web_page_pid, (void), {
  return (Module.perfr && Module.perfr.pagePid) || 0xB10B0001;
});

EM_JS(double, perfr_web_now_ns, (void), {
  return Module.perfr ? Module.perfr.nowNs() : 0;
});

EM_JS(void, perfr_web_set_tid, (uint32_t tid), {
  if (Module.perfr) Module.perfr.setTid(tid);
});

EM_JS(double, perfr_web_begin, (const char* name, size_t name_len), {
  if (!Module.perfr) return 0;
  return Module.perfr.begin(UTF8ToString(name, name_len));
});

EM_JS(void, perfr_web_end, (double handle), {
  if (Module.perfr) Module.perfr.end(handle);
});

EM_JS(double, perfr_web_begin_async, (const char* name, size_t name_len), {
  if (!Module.perfr) return 0;
  return Module.perfr.beginAsync(UTF8ToString(name, name_len));
});

EM_JS(void, perfr_web_end_async, (double handle), {
  if (Module.perfr) Module.perfr.endAsync(handle);
});

EM_JS(void, perfr_web_span, (const char* name, size_t name_len, double start_ns, double end_ns), {
  if (Module.perfr) Module.perfr.span(UTF8ToString(name, name_len), start_ns, end_ns);
});

EM_JS(void, perfr_web_span_async, (const char* name, size_t name_len, double start_ns, double end_ns), {
  if (Module.perfr) Module.perfr.spanAsync(UTF8ToString(name, name_len), start_ns, end_ns);
});

EM_JS(void, perfr_web_value, (const char* name, size_t name_len, double value), {
  if (Module.perfr) Module.perfr.value(UTF8ToString(name, name_len), value);
});

EM_JS(void, perfr_web_mark, (const char* name, size_t name_len), {
  if (Module.perfr) Module.perfr.mark(UTF8ToString(name, name_len));
});

EM_JS(void, perfr_web_frame, (void), {
  if (Module.perfr) Module.perfr.frame();
});

EM_JS(void, perfr_web_name_thread, (uint32_t tid, const char* name, size_t name_len), {
  if (Module.perfr) Module.perfr.nameThread(UTF8ToString(name, name_len), tid);
});

EM_JS(void, perfr_web_name_process, (const char* name, size_t name_len), {
  if (Module.perfr) Module.perfr.nameProcess(UTF8ToString(name, name_len));
});

EM_JS(void, perfr_web_stop, (void), {
  if (Module.perfr) Module.perfr.stop();
});

EM_JS(void, perfr_web_clear, (void), {
  if (Module.perfr) Module.perfr.clear();
});

EM_JS(void, perfr_web_resume, (void), {
  if (Module.perfr) Module.perfr.start();
});

EM_JS(double, perfr_web_copy, (int which), {
  var bytes = !Module.perfr ? new Uint8Array()
    : which === 0 ? Module.perfr.takeFlush() : Module.perfr.takeSnapshot();
  if (!bytes) bytes = new Uint8Array();
  var ptr = Module._malloc(bytes.length);
  Module.HEAPU8.set(bytes, ptr);
  Module.__perfr_web_len = bytes.length;
  return ptr;
});

static inline const uint8_t* perfr_web_flush(size_t* out_len) {
  uint8_t* ptr = (uint8_t*)(uintptr_t)perfr_web_copy(0);
  if (out_len) *out_len = (size_t)EM_ASM_INT({ return Module.__perfr_web_len | 0; });
  return ptr;
}

static inline const uint8_t* perfr_web_snapshot(size_t* out_len) {
  uint8_t* ptr = (uint8_t*)(uintptr_t)perfr_web_copy(1);
  if (out_len) *out_len = (size_t)EM_ASM_INT({ return Module.__perfr_web_len | 0; });
  return ptr;
}

#endif /* __EMSCRIPTEN__ */
#endif /* PERFR_WEB_JS_H */
