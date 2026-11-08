// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { startBrowser } from "./browser.js";
import { attachViewer } from "./embed.js";
import { Profiler } from "./profiler.js";
import { detectSupport } from "./support.js";
import { TID } from "./tracks.js";
import { attachWorker } from "./worker.js";
export { PAGE_PID, TID, WORKER_BASE } from "./tracks.js";
export { detectSupport } from "./support.js";
function decorator(profiler, name) {
    return function decorate(value, context) {
        if (context && (context.kind === "method" || context.kind === "getter" || context.kind === "setter")) {
            const label = name ?? String(context.name ?? "scope");
            const fn = value;
            return function (...args) {
                return profiler.scope(label, () => fn.apply(this, args));
            };
        }
        return value;
    };
}
export function createPerfr() {
    const profiler = new Profiler();
    let browser = null;
    let viewer = null;
    let started = false;
    let browserOpt = true;
    const api = {
        pagePid: profiler.pagePid,
        scope: (name, fn) => profiler.scope(name, fn),
        scopeAsync: (name, fn) => profiler.scopeAsync(name, fn),
        begin: (name) => profiler.begin(name),
        end: (handle) => profiler.end(handle),
        beginAsync: (name) => profiler.beginAsync(name),
        endAsync: (handle) => profiler.endAsync(handle),
        span: (name, a, b) => profiler.span(name, a, b),
        spanAsync: (name, a, b) => profiler.spanAsync(name, a, b),
        mark: (name) => profiler.mark(name),
        frame: (name) => profiler.frame(name),
        value: (name, value) => profiler.value(name, value),
        plot: (name, value) => profiler.value(name, value),
        nowNs: () => profiler.nowNs(),
        nameThread: (name, tid) => profiler.nameThread(name, tid ?? TID.main),
        nameProcess: (name) => profiler.nameProcess(name),
        setTid: (tid) => profiler.setTid(tid),
        decorate: (name) => decorator(profiler, name),
        start(options) {
            if (options && "browser" in options)
                browserOpt = options.browser;
            const wasmUrl = options?.wasmUrl;
            return profiler.start(wasmUrl).then(() => {
                if (started)
                    return;
                started = true;
                if (browserOpt !== false && typeof window !== "undefined") {
                    browser = startBrowser(profiler, browserOpt ?? true);
                }
            });
        },
        stop() {
            browser?.stop();
            browser = null;
            profiler.stop();
            started = false;
        },
        clear() {
            profiler.clear();
        },
        exportStream: () => profiler.exportStream(),
        exportBlob: () => profiler.exportBlob(),
        download: (filename) => profiler.download(filename),
        attachViewer(options) {
            viewer?.close();
            void api.start({ browser: browserOpt });
            viewer = attachViewer(profiler, options);
            return viewer;
        },
        detachViewer() {
            viewer?.close();
            viewer = null;
        },
        attachWorker: (worker, options) => attachWorker(profiler, worker, options),
        tracks: {
            set(partial) {
                browser?.set(partial);
            },
            get: () => browser?.get() ?? null,
            support: () => detectSupport(),
        },
        stats: () => profiler.stats(),
        takeFlush: () => profiler.takeFlush(),
        takeSnapshot: () => profiler.takeSnapshot(),
    };
    const kick = () => {
        if (!started)
            void api.start({ browser: browserOpt });
    };
    const wrap = (fn) => (...args) => {
        kick();
        return fn(...args);
    };
    api.scope = wrap(api.scope);
    api.scopeAsync = wrap(api.scopeAsync);
    api.begin = wrap(api.begin);
    api.end = wrap(api.end);
    api.beginAsync = wrap(api.beginAsync);
    api.endAsync = wrap(api.endAsync);
    api.span = wrap(api.span);
    api.spanAsync = wrap(api.spanAsync);
    api.mark = wrap(api.mark);
    api.frame = wrap(api.frame);
    api.value = wrap(api.value);
    api.plot = wrap(api.plot);
    return api;
}
/** The page-wide profiler. */
export const perfr = createPerfr();
