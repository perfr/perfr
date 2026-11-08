// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { nowNs, nowNsAligned } from "./clock.js";
import { WORKER_BASE } from "./tracks.js";
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
    pageOrigin = 0;
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
        this.pageOrigin = pageOrigin;
        this.tid = tid;
        if (name)
            this.label = name;
        this.configured = true;
        this.flush();
    }
    nowNs() {
        return this.configured ? nowNsAligned(this.pageOrigin) : nowNs();
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
        this.open.set(h, { name, t: this.nowNs(), async: false });
        return h;
    }
    end(handle) {
        this.finish(handle, false);
    }
    beginAsync(name) {
        const h = this.local++;
        this.open.set(h, { name, t: this.nowNs(), async: true });
        return h;
    }
    endAsync(handle) {
        this.finish(handle, true);
    }
    mark(name) {
        this.queue.push({ k: "mark", name, a: this.nowNs(), b: 0 });
        this.schedule();
    }
    value(name, value) {
        this.queue.push({ k: "value", name, a: this.nowNs(), b: 0, v: value });
        this.schedule();
    }
    finish(handle, async) {
        const open = this.open.get(handle);
        if (!open || open.async !== async)
            return;
        this.open.delete(handle);
        this.queue.push({
            k: async ? "async" : "span",
            name: open.name,
            a: open.t,
            b: this.nowNs(),
        });
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
        self.postMessage({ source: SOURCE, kind: "batch", tid: this.tid, name: this.label, ops });
    }
}
let nextTid = WORKER_BASE;
/**
 * Treat `worker` as another thread. Its `installWorker` scopes are merged
 * into this profiler, and postMessage in both directions is marked on the
 * Worker messages track.
 */
export function attachWorker(profiler, worker, options = {}) {
    const tid = options.tid ?? nextTid++;
    const name = options.name ?? `worker ${tid}`;
    profiler.nameThread(name, tid);
    const pageOrigin = performance.timeOrigin;
    const sendInit = () => {
        worker.postMessage({ source: SOURCE, kind: "init", pageOrigin, tid, name });
    };
    sendInit();
    worker.addEventListener("message", (ev) => {
        const data = ev.data;
        if (!data || data.source !== SOURCE || data.kind !== "batch" || !data.ops)
            return;
        const thread = data.tid ?? tid;
        if (data.name)
            profiler.nameThread(data.name, thread);
        profiler.mark(`recv ${data.name ?? name}`, 14);
        for (const op of data.ops) {
            if (op.k === "span")
                profiler.span(op.name, op.a, op.b, thread);
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
        if (!(message && typeof message === "object" && message.source === SOURCE)) {
            profiler.mark(`send ${name}`, 14);
        }
        return orig(message, transfer);
    });
    return tid;
}
