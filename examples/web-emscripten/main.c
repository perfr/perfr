/* Copyright (c) 2026 The perfr Authors. All rights reserved.
 * Use of this source code is governed by a BSD-style license that can be
 * found in the LICENSE file. */

/* Emscripten (or a native link of libperfr_web) calling the C ABI.
 *
 *   emcc main.c -I perfr_web_js.h's directory -o demo.html
 *   and set Module.perfr to the @perfr/web profiler before the module runs.
 *
 * Linking the Rust staticlib instead: define nothing extra and pass
 * -DPERFR_WEB_LINKED is not required for perfr_web.h. Build the staticlib
 * with `cargo build -p perfr-web --target wasm32-unknown-emscripten` when
 * that target is installed, then `emcc main.c libperfr_web.a`.
 */

#ifdef __EMSCRIPTEN__
#include "perfr_web_js.h"
#else
#include "perfr_web.h"
#endif

#include <stdio.h>

static void frame(void) {
    uint64_t outer = perfr_web_begin("frame", 5);
    uint64_t inner = perfr_web_begin("simulate", 8);
    perfr_web_value("n", 1, 1.0);
    perfr_web_end(inner);
    perfr_web_end(outer);
    perfr_web_frame();
}

int main(void) {
    perfr_web_name_process("emscripten", 10);
    perfr_web_name_thread(1, "main", 4);
    frame();
    size_t len = 0;
    const uint8_t* bytes = perfr_web_snapshot(&len);
    printf("perfr stream %zu bytes, page pid %u\n", len, perfr_web_page_pid());
    (void)bytes;
    return 0;
}
