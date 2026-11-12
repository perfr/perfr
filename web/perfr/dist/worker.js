// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { nowNs } from "./clock.js";
import { TID, WORKER_BASE } from "./tracks.js";
const SOURCE = "perfr-worker";
/**
 * Call once in a dedicated worker. Scopes recorded here are posted to the
 * page that called `perfr.attachWorker` and show up as their own thread.
 * The worker clock is shifted onto the page's `timeOrigin`.
 */
export function installWorker(options = {}) {
    const client = new WorkerProfiler(options.name);
    self.addEventListener("message", (ev) => {
        const data = ev.data;
        if (!data || data.source !== SOURCE)
            return;
        if (data.kind === "init") {
            client.configure(data.pageOrigin ?? 0, data.tid ?? WORKER_BASE, data.name);
        }
    });
    return client;
}
class WorkerProfiler {
    /** Added to this worker's clock to land on the page's `timeOrigin`. */
    shiftNs = 0;
    tid = WORKER_BASE;
    configured = false;
    queue = [];
    local = 1;
    open = new Map();
    label;
    constructor(name) {
        this.label = name ?? "worker";
    }
    configure(pageOrigin, tid, name) {
        const origin = globalThis.performance?.timeOrigin;
        this.shiftNs = typeof origin === "number" ? (origin - pageOrigin) * 1e6 : 0;
        this.tid = tid;
        if (name)
            this.label = name;
        this.configured = true;
        this.flush();
    }
    /**
     * Nanoseconds on the page's clock. Before the page has attached this
     * worker the shift is unknown, so scopes are stored on the worker's own
     * clock and shifted when they are sent.
     */
    nowNs() {
        return nowNs() + this.shiftNs;
    }
    localNs() {
        return nowNs();
    }
    scope(name, fn) {
        const h = this.begin(name);
        try {
            return fn();
        }
        finally {
            this.end(h);
        }
    }
    async scopeAsync(name, fn) {
        const h = this.beginAsync(name);
        try {
            return await fn();
        }
        finally {
            this.endAsync(h);
        }
    }
    begin(name) {
        const h = this.local++;
        const t = this.localNs();
        this.open.set(h, { name, t, async: false });
        // Sent with the next batch; a begin alone does not need a message.
        this.queue.push({ k: "begin", name, a: t, b: 0, h });
        return h;
    }
    end(handle) {
        this.finish(handle, false);
    }
    beginAsync(name) {
        const h = this.local++;
        this.open.set(h, { name, t: this.localNs(), async: true });
        return h;
    }
    endAsync(handle) {
        this.finish(handle, true);
    }
    mark(name) {
        this.queue.push({ k: "mark", name, a: this.localNs(), b: 0 });
        this.schedule();
    }
    value(name, value) {
        this.queue.push({ k: "value", name, a: this.localNs(), b: 0, v: value });
        this.schedule();
    }
    finish(handle, async) {
        const open = this.open.get(handle);
        if (!open || open.async !== async)
            return;
        this.open.delete(handle);
        if (async)
            this.queue.push({ k: "async", name: open.name, a: open.t, b: this.localNs() });
        else
            this.queue.push({ k: "end", name: open.name, a: this.localNs(), b: 0, h: handle });
        this.schedule();
    }
    scheduled = false;
    schedule() {
        if (this.scheduled)
            return;
        this.scheduled = true;
        const raf = self.requestAnimationFrame;
        if (typeof raf === "function")
            raf(() => this.flush());
        else
            setTimeout(() => this.flush(), 16);
    }
    flush() {
        this.scheduled = false;
        if (!this.configured || this.queue.length === 0)
            return;
        const ops = this.queue;
        this.queue = [];
        const shift = this.shiftNs;
        if (shift !== 0) {
            for (const op of ops) {
                op.a += shift;
                if (op.k === "async")
                    op.b += shift;
            }
        }
        self.postMessage({ source: SOURCE, kind: "batch", tid: this.tid, name: this.label, ops });
    }
}
let nextTid = WORKER_BASE;
/**
 * Treat `worker` as another thread. Its `installWorker` scopes are merged
 * into this profiler, and postMessage in both directions is marked on the
 * Worker messages track.
 */
export function attachWorker(profiler, worker, options = {}, 
/** Whether the Worker messages track is on. Asked per message. */
messagesOn = () => true) {
    const tid = options.tid ?? nextTid++;
    const name = options.name ?? `worker ${tid}`;
    profiler.nameThread(name, tid);
    const pageOrigin = performance.timeOrigin;
    const sendInit = () => {
        worker.postMessage({ source: SOURCE, kind: "init", pageOrigin, tid, name });
    };
    sendInit();
    /** The worker's handle for each of its open scopes, to this profiler's. */
    const openScopes = new Map();
    worker.addEventListener("message", (ev) => {
        const data = ev.data;
        // The track is of the app's messages. The profiler's own batches are
        // its transport, and a mark for each would bury the app's under one
        // every flush.
        if (!data || data.source !== SOURCE) {
            if (messagesOn())
                profiler.mark(`recv ${name}`, TID.workerMsg);
            return;
        }
        if (data.kind !== "batch" || !data.ops)
            return;
        const thread = data.tid ?? tid;
        if (data.name)
            profiler.nameThread(data.name, thread);
        for (const op of data.ops) {
            if (op.k === "begin" && op.h != null) {
                openScopes.set(op.h, profiler.begin(op.name, thread, op.a));
            }
            else if (op.k === "end" && op.h != null) {
                const handle = openScopes.get(op.h);
                openScopes.delete(op.h);
                if (handle)
                    profiler.end(handle, op.a);
            }
            else if (op.k === "async")
                profiler.spanAsync(op.name, op.a, op.b, thread);
            else if (op.k === "mark")
                profiler.mark(op.name, thread, op.a);
            else if (op.k === "value")
                profiler.value(op.name, op.v ?? 0, thread, op.a);
        }
    });
    const orig = worker.postMessage.bind(worker);
    worker.postMessage = ((message, transfer) => {
        if (!(message && typeof message === "object" && message.source === SOURCE) && messagesOn()) {
            profiler.mark(`send ${name}`, TID.workerMsg);
        }
        return orig(message, transfer);
    });
    return tid;
}
