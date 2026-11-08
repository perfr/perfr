// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { nowNs } from "./clock.js";
import { PAGE_PID, TID, threadName } from "./tracks.js";

export interface Stats {
  live: number;
  capacity: number;
  dropped: number;
  produced: number;
  capturing: boolean;
}

export interface FlushPacket {
  bytes: Uint8Array;
  stats: Stats;
}

type WasmProfiler = {
  begin(name: string, tid: number, nowNs: number): number;
  end(handle: number, nowNs: number): void;
  begin_async(name: string, tid: number, nowNs: number): number;
  end_async(handle: number, nowNs: number): void;
  span(name: string, tid: number, startNs: number, endNs: number): void;
  span_async(name: string, tid: number, startNs: number, endNs: number): void;
  mark(name: string, tid: number, nowNs: number): void;
  value(name: string, tid: number, value: number, nowNs: number): void;
  name_thread(tid: number, name: string): void;
  name_process(name: string): void;
  stop(nowNs: number): void;
  clear(): void;
  resume(): void;
  resync(): void;
  set_enabled(on: boolean): void;
  flush(): Uint8Array;
  snapshot(nowNs: number): Uint8Array;
  live(): number;
  dropped(): number;
  produced(): number;
  capacity(): number;
  capturing(): boolean;
  page_pid(): number;
};

type Op =
  | { k: "b"; h: number; name: string; tid: number; t: number; async: boolean }
  | { k: "e"; h: number; t: number; async: boolean }
  | { k: "v"; name: string; tid: number; v: number; t: number }
  | { k: "m"; name: string; tid: number; t: number }
  | { k: "s"; name: string; tid: number; a: number; b: number; async: boolean }
  | { k: "tn"; tid: number; name: string }
  | { k: "pn"; name: string };

const QUEUE_CAP = 50_000;

type FlushListener = (packet: FlushPacket) => void;

/**
 * Records into the WASM ring. Calls before the module has loaded are queued
 * with their original timestamps and replayed in order, so depth still matches
 * the nesting the page actually ran.
 */
export class Profiler {
  pagePid: number = PAGE_PID;
  private wasm: WasmProfiler | null = null;
  private loading: Promise<void> | null = null;
  private buf: Op[] = [];
  private droppedQueue = 0;
  private localHandle = 1;
  private wasmHandles = new Map<number, number>();
  private tid: number = TID.main;
  private capacity: number;
  private listeners = new Set<FlushListener>();
  private scheduled = false;
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private raf = 0;

  constructor(capacity = 200_000) {
    this.capacity = capacity;
  }

  /** Ring length. Takes effect when the encoder loads; later calls are ignored. */
  setCapacity(capacity: number): void {
    if (this.wasm || !Number.isFinite(capacity) || capacity < 1) return;
    this.capacity = Math.floor(capacity);
  }

  setTid(tid: number): void {
    this.tid = tid;
  }

  currentTid(): number {
    return this.tid;
  }

  nowNs(): number {
    return nowNs();
  }

  /** Load the Rust encoder. Safe to call more than once. */
  start(wasmUrl?: string): Promise<void> {
    if (this.wasm) return Promise.resolve();
    if (this.loading) return this.loading;
    this.loading = this.load(wasmUrl).then(() => {
      this.nameBuiltins();
      this.flushNow();
    });
    return this.loading;
  }

  onFlush(listener: FlushListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  scope<T>(name: string, fn: () => T, tid = this.tid): T {
    const handle = this.begin(name, tid);
    try {
      return fn();
    } finally {
      this.end(handle);
    }
  }

  async scopeAsync<T>(name: string, fn: () => Promise<T>, tid = this.tid): Promise<T> {
    const handle = this.beginAsync(name, tid);
    try {
      return await fn();
    } finally {
      this.endAsync(handle);
    }
  }

  begin(name: string, tid = this.tid): number {
    const h = this.localHandle++;
    this.push({ k: "b", h, name, tid, t: nowNs(), async: false });
    return h;
  }

  end(handle: number): void {
    if (!handle) return;
    this.push({ k: "e", h: handle, t: nowNs(), async: false });
  }

  beginAsync(name: string, tid = this.tid): number {
    const h = this.localHandle++;
    this.push({ k: "b", h, name, tid, t: nowNs(), async: true });
    return h;
  }

  endAsync(handle: number): void {
    if (!handle) return;
    this.push({ k: "e", h: handle, t: nowNs(), async: true });
  }

  span(name: string, startNs: number, endNs: number, tid = this.tid): void {
    this.push({ k: "s", name, tid, a: startNs, b: endNs, async: false });
  }

  spanAsync(name: string, startNs: number, endNs: number, tid = this.tid): void {
    this.push({ k: "s", name, tid, a: startNs, b: endNs, async: true });
  }

  mark(name: string, tid = this.tid, time = nowNs()): void {
    this.push({ k: "m", name, tid, t: time });
  }

  frame(name = "frame", tid = this.tid): void {
    this.mark(name, tid);
  }

  value(name: string, value: number, tid = this.tid, time = nowNs()): void {
    this.push({ k: "v", name, tid, v: value, t: time });
  }

  plot(name: string, value: number, tid = this.tid): void {
    this.value(name, value, tid);
  }

  nameThread(name: string, tid = this.tid): void {
    this.push({ k: "tn", tid, name });
  }

  nameProcess(name: string): void {
    this.push({ k: "pn", name });
  }

  stop(): void {
    this.flushNow();
    this.wasm?.stop(nowNs());
    this.emit();
  }

  /**
   * Record again after {@link stop}. The ring is kept. The stream starts
   * over (hello, names, the ring) so an attached viewer stays consistent.
   */
  resume(): void {
    this.wasm?.resume();
    this.emit();
  }

  clear(): void {
    this.buf = [];
    this.wasmHandles.clear();
    this.wasm?.clear();
    this.nameBuiltins();
    this.emit();
  }

  resync(): void {
    this.wasm?.resync();
    this.emit();
  }

  stats(): Stats {
    if (!this.wasm) {
      return {
        live: 0,
        capacity: this.capacity,
        dropped: this.droppedQueue,
        produced: 0,
        capturing: true,
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
  flushNow(): Uint8Array {
    this.cancelSchedule();
    const ops = this.buf;
    this.buf = [];
    if (this.wasm) this.replay(ops);
    else this.buf = ops;
    return this.emit();
  }

  async exportStream(): Promise<Uint8Array> {
    await this.start();
    this.flushNow();
    return this.wasm ? this.wasm.snapshot(nowNs()) : new Uint8Array();
  }

  async exportBlob(): Promise<Blob> {
    const bytes = await this.exportStream();
    return new Blob([bytes], { type: "application/octet-stream" });
  }

  async download(filename = "capture.perfr.stream"): Promise<void> {
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
  takeFlush(): Uint8Array {
    return this.flushNow();
  }

  takeSnapshot(): Uint8Array {
    this.flushNow();
    return this.wasm ? this.wasm.snapshot(nowNs()) : new Uint8Array();
  }

  private push(op: Op): void {
    if (this.buf.length >= QUEUE_CAP) {
      this.buf.shift();
      this.droppedQueue++;
    }
    this.buf.push(op);
    this.schedule();
  }

  private schedule(): void {
    if (this.scheduled) return;
    this.scheduled = true;
    const raf = globalThis.requestAnimationFrame;
    if (typeof raf === "function") {
      this.raf = raf(() => {
        this.scheduled = false;
        this.raf = 0;
        this.flushNow();
      });
      return;
    }
    this.flushTimer = setTimeout(() => {
      this.scheduled = false;
      this.flushTimer = null;
      this.flushNow();
    }, 16);
  }

  private cancelSchedule(): void {
    if (this.raf && typeof globalThis.cancelAnimationFrame === "function") {
      globalThis.cancelAnimationFrame(this.raf);
    }
    if (this.flushTimer != null) clearTimeout(this.flushTimer);
    this.raf = 0;
    this.flushTimer = null;
    this.scheduled = false;
  }

  private replay(ops: Op[]): void {
    const wasm = this.wasm;
    if (!wasm || ops.length === 0) return;
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
          if (op.async) wasm.end_async(handle, op.t);
          else wasm.end(handle, op.t);
          break;
        }
        case "v":
          wasm.value(op.name, op.tid, op.v, op.t);
          break;
        case "m":
          wasm.mark(op.name, op.tid, op.t);
          break;
        case "s":
          if (op.async) wasm.span_async(op.name, op.tid, op.a, op.b);
          else wasm.span(op.name, op.tid, op.a, op.b);
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

  private emit(): Uint8Array {
    if (!this.wasm) return new Uint8Array();
    const bytes = this.wasm.flush();
    if (bytes.length && this.listeners.size) {
      const packet = { bytes, stats: this.stats() };
      for (const listener of this.listeners) listener(packet);
    }
    return bytes;
  }

  private nameBuiltins(): void {
    if (!this.wasm) return;
    this.wasm.name_process("page");
    for (const tid of Object.values(TID)) {
      const name = threadName(tid);
      if (name) this.wasm.name_thread(tid, name);
    }
  }

  private async load(wasmUrl?: string): Promise<void> {
    const href = wasmUrl ?? new URL("../pkg/perfr_web.js", import.meta.url);
    const mod = await import(/* @vite-ignore */ href.toString());
    await mod.default();
    const ProfilerCtor = mod.WebProfiler as new (capacity: number) => WasmProfiler;
    this.wasm = new ProfilerCtor(this.capacity);
    this.pagePid = this.wasm.page_pid();
  }
}
