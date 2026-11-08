/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* C ABI for the perfr-web ring. Link libperfr_web (crate perfr-web).
 *
 * Names are pointer plus length, as in perfr.h. A zero handle means the
 * call was dropped because recording is off. perfr_web_flush and
 * perfr_web_snapshot return a pointer owned by the library; it is invalid
 * after the next call to either function, so copy the bytes first.
 *
 * An Emscripten app that cannot link the staticlib can use the EM_JS shim
 * in examples/web-emscripten/perfr_web_js.h, which forwards these calls to
 * the @perfr/web profiler on Module.perfr.
 */

#ifndef PERFR_WEB_H
#define PERFR_WEB_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

uint32_t perfr_web_page_pid(void);
uint64_t perfr_web_now_ns(void);
void perfr_web_set_tid(uint32_t tid);

uint64_t perfr_web_begin(const char* name, size_t name_len);
void perfr_web_end(uint64_t handle);
uint64_t perfr_web_begin_async(const char* name, size_t name_len);
void perfr_web_end_async(uint64_t handle);

void perfr_web_span(const char* name, size_t name_len, uint64_t start_ns, uint64_t end_ns);
void perfr_web_span_async(const char* name, size_t name_len, uint64_t start_ns, uint64_t end_ns);
void perfr_web_value(const char* name, size_t name_len, double value);
void perfr_web_mark(const char* name, size_t name_len);
void perfr_web_frame(void);

void perfr_web_name_thread(uint32_t tid, const char* name, size_t name_len);
void perfr_web_name_process(const char* name, size_t name_len);

void perfr_web_stop(void);
void perfr_web_clear(void);

const uint8_t* perfr_web_flush(size_t* out_len);
const uint8_t* perfr_web_snapshot(size_t* out_len);

#ifdef __cplusplus
}
#endif

#endif /* PERFR_WEB_H */
