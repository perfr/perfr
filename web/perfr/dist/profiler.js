// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { nowNs } from "./clock.js";
import { PAGE_PID, TID, threadName } from "./tracks.js";
const QUEUE_CAP = 50_000;
/**
 * Flush this long after an event when no animation frame has come first.
 * A hidden tab gets no animation frames, but its timers, fetches and workers
 * still run, and their events would otherwise pile up in the queue and fall
 * off its end.
 */
const FLUSH_FALLBACK_MS = 250;
/**
 * Records into the WASM ring. Calls before the module has loaded are queued
 * with their original timestamps and replayed in order, so depth still matches
 * the nesting the page actually ran.
 */
export class Profiler {
    pagePid = PAGE_PID;
    wasm = null;
    loading = null;
    buf = [];
    droppedQueue = 0;
    localHandle = 1;
    wasmHandles = new Map();
    tid = TID.main;
    capacity;
    listeners = new Set();
    scheduled = false;
    flushTimer = null;
    raf = 0;
    /** Set by {@link stop}. Recording calls are dropped here, encoder or not. */
    paused = false;
    pausedAt = 0;
    constructor(capacity = 200_000) {
        this.capacity = capacity;
    }
    /** Ring length. Takes effect when the encoder loads; later calls are ignored. */
    setCapacity(capacity) {
        if (this.wasm || !Number.isFinite(capacity) || capacity < 1)
            return;
        this.capacity = Math.floor(capacity);
    }
    setTid(tid) {
        this.tid = tid;
    }
    currentTid() {
        return this.tid;
    }
    nowNs() {
        return nowNs();
    }
    /** Load the Rust encoder. Safe to call more than once. */
    start(wasmUrl) {
        if (this.wasm)
            return Promise.resolve();
        if (this.loading)
            return this.loading;
        this.loading = this.load(wasmUrl).then(() => {
            this.nameBuiltins();
            this.flushNow();
            // stop() ran before the encoder existed: the ring starts out stopped.
            if (this.paused)
                this.stopRing();
        }, (err) => {
            // A later start() may try again; do not hand this failure back forever.
            this.loading = null;
            throw err;
        });
        return this.loading;
    }
    onFlush(listener) {
        this.listeners.add(listener);
        return () => this.listeners.delete(listener);
    }
    scope(name, fn, tid = this.tid) {
        const handle = this.begin(name, tid);
        try {
            return fn();
        }
        finally {
            this.end(handle);
        }
    }
    async scopeAsync(name, fn, tid = this.tid) {
        const handle = this.beginAsync(name, tid);
        try {
            return await fn();
        }
        finally {
            this.endAsync(handle);
        }
    }
    /** `time` is for a scope that began elsewhere, such as in a worker. */
    begin(name, tid = this.tid, time = nowNs()) {
        const h = this.localHandle++;
        this.push({ k: "b", h, name, tid, t: time, async: false });
        return h;
    }
    end(handle, time = nowNs()) {
        if (!handle)
            return;
        this.push({ k: "e", h: handle, t: time, async: false });
    }
    beginAsync(name, tid = this.tid) {
        const h = this.localHandle++;
        this.push({ k: "b", h, name, tid, t: nowNs(), async: true });
        return h;
    }
    endAsync(handle) {
        if (!handle)
            return;
        this.push({ k: "e", h: handle, t: nowNs(), async: true });
    }
    span(name, startNs, endNs, tid = this.tid) {
        this.push({ k: "s", name, tid, a: startNs, b: endNs, async: false });
    }
    spanAsync(name, startNs, endNs, tid = this.tid) {
        this.push({ k: "s", name, tid, a: startNs, b: endNs, async: true });
    }
    mark(name, tid = this.tid, time = nowNs()) {
        this.push({ k: "m", name, tid, t: time });
    }
    frame(name = "frame", tid = this.tid) {
        this.mark(name, tid);
    }
    value(name, value, tid = this.tid, time = nowNs()) {
        this.push({ k: "v", name, tid, v: value, t: time });
    }
    plot(name, value, tid = this.tid) {
        this.value(name, value, tid);
    }
    nameThread(name, tid = this.tid) {
        this.push({ k: "tn", tid, name });
    }
    nameProcess(name) {
        this.push({ k: "pn", name });
    }
    /**
     * Stop recording. This holds before the encoder has loaded too: what was
     * queued until now is kept, what is recorded from here on is dropped.
     */
    stop() {
        this.flushNow();
        this.paused = true;
        this.pausedAt = nowNs();
        this.stopRing();
    }
    /**
     * Record again after {@link stop}. The ring is kept. The stream starts
     * over (hello, names, the ring) so an attached viewer stays consistent.
     */
    resume() {
        this.paused = false;
        this.wasm?.resume();
        this.emit();
    }
    /** Drop the ring and record again. Thread and process names are kept. */
    clear() {
        this.buf = this.buf.filter((op) => op.k === "tn" || op.k === "pn");
        this.wasmHandles.clear();
        this.paused = false;
        this.wasm?.clear();
        this.emit();
    }
    resync() {
        this.wasm?.resync();
        this.emit();
    }
    stats() {
        if (!this.wasm) {
            return {
                live: 0,
                capacity: this.capacity,
                dropped: this.droppedQueue,
                produced: 0,
                capturing: !this.paused,
            };
        }
        return {
            live: this.wasm.live(),
            capacity: this.wasm.capacity(),
            dropped: this.wasm.dropped() + this.droppedQueue,
            produced: this.wasm.produced(),
            capturing: this.wasm.capturing(),
        };
    }
    /** Replay the queue and return the new `.perfr.stream` frames. */
    flushNow() {
        this.cancelSchedule();
        const ops = this.buf;
        this.buf = [];
        if (this.wasm)
            this.replay(ops);
        else
            this.buf = ops;
        return this.emit();
    }
    async exportStream() {
        await this.start();
        this.flushNow();
        return this.wasm ? this.wasm.snapshot(nowNs()) : new Uint8Array();
    }
    async exportBlob() {
        const bytes = await this.exportStream();
        return new Blob([bytes], { type: "application/octet-stream" });
    }
    async download(filename = "capture.perfr.stream") {
        const blob = await this.exportBlob();
        const url = URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = filename;
        a.click();
        // Revoking in the same task can cancel the download in some browsers.
        setTimeout(() => URL.revokeObjectURL(url), 1000);
    }
    /** Bytes from the last flush, for the Emscripten EM_JS shim. */
    takeFlush() {
        return this.flushNow();
    }
    takeSnapshot() {
        this.flushNow();
        return this.wasm ? this.wasm.snapshot(nowNs()) : new Uint8Array();
    }
    push(op) {
        // Stopped: drop the event, as the ring would. Names are not events.
        if (this.paused && op.k !== "tn" && op.k !== "pn")
            return;
        if (this.buf.length >= QUEUE_CAP) {
            this.buf.shift();
            this.droppedQueue++;
        }
        this.buf.push(op);
        this.schedule();
    }
    schedule() {
        if (this.scheduled)
            return;
        this.scheduled = true;
        // Whichever fires first flushes; flushNow cancels the other.
        const flush = () => this.flushNow();
        const raf = globalThis.requestAnimationFrame;
        const hasRaf = typeof raf === "function";
        if (hasRaf)
            this.raf = raf(flush);
        this.flushTimer = setTimeout(flush, hasRaf ? FLUSH_FALLBACK_MS : 16);
    }
    cancelSchedule() {
        if (this.raf && typeof globalThis.cancelAnimationFrame === "function") {
            globalThis.cancelAnimationFrame(this.raf);
        }
        if (this.flushTimer != null)
            clearTimeout(this.flushTimer);
        this.raf = 0;
        this.flushTimer = null;
        this.scheduled = false;
    }
    replay(ops) {
        const wasm = this.wasm;
        if (!wasm || ops.length === 0)
            return;
        for (const op of ops) {
            switch (op.k) {
                case "b": {
                    const handle = op.async
                        ? wasm.begin_async(op.name, op.tid, op.t)
                        : wasm.begin(op.name, op.tid, op.t);
                    this.wasmHandles.set(op.h, handle);
                    break;
                }
                case "e": {
                    const handle = this.wasmHandles.get(op.h) ?? 0;
                    this.wasmHandles.delete(op.h);
                    if (op.async)
                        wasm.end_async(handle, op.t);
                    else
                        wasm.end(handle, op.t);
                    break;
                }
                case "v":
                    wasm.value(op.name, op.tid, op.v, op.t);
                    break;
                case "m":
                    wasm.mark(op.name, op.tid, op.t);
                    break;
                case "s":
                    if (op.async)
                        wasm.span_async(op.name, op.tid, op.a, op.b);
                    else
                        wasm.span(op.name, op.tid, op.a, op.b);
                    break;
                case "tn":
                    wasm.name_thread(op.tid, op.name);
                    break;
                case "pn":
                    wasm.name_process(op.name);
                    break;
            }
        }
    }
    /** Close what is open at {@link pausedAt} and end the stream. */
    stopRing() {
        if (!this.wasm)
            return;
        this.wasm.stop(this.pausedAt);
        // The ring closed every open scope, so no handle from before is live.
        this.wasmHandles.clear();
        this.emit();
    }
    emit() {
        if (!this.wasm)
            return new Uint8Array();
        const bytes = this.wasm.flush();
        if (bytes.length && this.listeners.size) {
            const packet = { bytes, stats: this.stats() };
            for (const listener of this.listeners)
                listener(packet);
        }
        return bytes;
    }
    nameBuiltins() {
        if (!this.wasm)
            return;
        this.wasm.name_process("page");
        for (const tid of Object.values(TID)) {
            const name = threadName(tid);
            if (name)
                this.wasm.name_thread(tid, name);
        }
    }
    async load(wasmUrl) {
        const href = wasmUrl ?? new URL("../pkg/perfr_web.js", import.meta.url);
        const mod = await import(/* @vite-ignore */ href.toString());
        await mod.default();
        const ProfilerCtor = mod.WebProfiler;
        this.wasm = new ProfilerCtor(this.capacity);
        this.pagePid = this.wasm.page_pid();
    }
}
