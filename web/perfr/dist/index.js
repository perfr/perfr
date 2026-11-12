// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { startBrowser } from "./browser.js";
import { attachViewer } from "./embed.js";
import { Profiler } from "./profiler.js";
import { detectSupport } from "./support.js";
import { attachWorker } from "./worker.js";
export { PAGE_PID, TID, WORKER_BASE } from "./tracks.js";
export { detectSupport } from "./support.js";
function decorator(profiler, name) {
    const scoped = (fn, label) => function (...args) {
        return profiler.scope(label, () => fn.apply(this, args));
    };
    return function decorate(value, context, descriptor) {
        // The older form: `(target, key, descriptor)`. The descriptor is what
        // gets wrapped, and what the caller defines the member from.
        if (typeof context === "string" || typeof context === "symbol") {
            if (!descriptor)
                return undefined;
            const label = name ?? String(context);
            for (const slot of ["value", "get", "set"]) {
                const fn = descriptor[slot];
                if (typeof fn === "function")
                    descriptor[slot] = scoped(fn, label);
            }
            return descriptor;
        }
        if (context && (context.kind === "method" || context.kind === "getter" || context.kind === "setter")) {
            return scoped(value, name ?? String(context.name ?? "scope"));
        }
        return value;
    };
}
export function createPerfr() {
    const profiler = new Profiler();
    let browser = null;
    let viewer = null;
    let started = false;
    /** Set by `stop()`. A plain `scope()` call does not restart recording. */
    let stopped = false;
    /**
     * The built-in tracks have run before. The browser's buffered entries
     * (resources, paints, marks, the navigation) are already in the ring, and
     * reading them again would record each one twice.
     */
    let browserRan = false;
    /** The start a recording call kicked off, while it is in flight. */
    let autoStart = null;
    let autoStartFailed = false;
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
        nameThread: (name, tid) => profiler.nameThread(name, tid),
        nameProcess: (name) => profiler.nameProcess(name),
        setTid: (tid) => profiler.setTid(tid),
        decorate: (name) => decorator(profiler, name),
        start(options) {
            if (options && "browser" in options)
                browserOpt = options.browser;
            if (options?.capacity != null)
                profiler.setCapacity(options.capacity);
            const wasmUrl = options?.wasmUrl;
            stopped = false;
            autoStartFailed = false;
            // Before the encoder loads, so what is recorded meanwhile is kept.
            profiler.resume();
            return profiler.start(wasmUrl).then(() => {
                if (stopped || started)
                    return;
                started = true;
                if (browserOpt !== false && typeof window !== "undefined") {
                    browser = startBrowser(profiler, browserOpt ?? true, { replayBuffered: !browserRan });
                    browserRan = true;
                }
            });
        },
        stop() {
            stopped = true;
            browser?.stop();
            browser = null;
            profiler.stop();
            started = false;
        },
        clear() {
            profiler.clear();
            // The ring records again after a clear. Bring the built-in tracks back
            // with it, or a capture cleared while stopped has user scopes only.
            if (stopped)
                autoStarted(api.start({ browser: browserOpt }));
        },
        exportStream: () => profiler.exportStream(),
        exportBlob: () => profiler.exportBlob(),
        download: (filename) => profiler.download(filename),
        attachViewer(options) {
            viewer?.close();
            if (!stopped)
                kick();
            viewer = attachViewer(profiler, options, {
                stop: () => api.stop(),
                start: () => autoStarted(api.start({ browser: browserOpt })),
                clear: () => api.clear(),
            });
            return viewer;
        },
        detachViewer() {
            viewer?.close();
            viewer = null;
        },
        // With the built-in tracks off there is no Worker messages track either;
        // the worker's own scopes are recorded regardless.
        attachWorker: (worker, options) => attachWorker(profiler, worker, options, () => browser?.get()?.workerMessages ?? false),
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
    /**
     * A start nobody awaits. A failed encoder load is reported once here
     * instead of as an unhandled rejection from every call that asked.
     */
    function autoStarted(start) {
        start.catch((err) => {
            autoStartFailed = true;
            console.warn("perfr: the encoder did not load, so nothing is recorded. Call perfr.start() to retry.", err);
        });
    }
    /** One start per load, not one per recorded scope while it is loading. */
    function kick() {
        if (started || stopped || autoStart || autoStartFailed)
            return;
        const pending = api.start({ browser: browserOpt }).finally(() => {
            if (autoStart === pending)
                autoStart = null;
        });
        autoStart = pending;
        autoStarted(pending);
    }
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
