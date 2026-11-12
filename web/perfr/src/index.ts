// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { startBrowser, type BrowserHandles } from "./browser.js";
import { attachViewer, type AttachViewerOptions, type ViewerHandle } from "./embed.js";
import { Profiler } from "./profiler.js";
import { detectSupport, type TrackSupport } from "./support.js";
import type { TrackId, TrackToggles } from "./tracks.js";
import { attachWorker, type WorkerOptions } from "./worker.js";

export { PAGE_PID, TID, WORKER_BASE, type TrackId, type TrackToggles } from "./tracks.js";
export type { TrackSupport } from "./support.js";
export type { AttachViewerOptions, ViewerHandle } from "./embed.js";
export type { Stats } from "./profiler.js";
export { detectSupport } from "./support.js";

export interface StartOptions {
  /** Ring length in events. Default 200_000. */
  capacity?: number;
  /** URL of `perfr_web.js` if it is not beside this package. */
  wasmUrl?: string;
  /**
   * Built-in browser tracks. `true` (default) enables every track the
   * browser actually supports. `false` records only what you instrument.
   * A partial object overrides individual tracks.
   */
  browser?: boolean | TrackToggles;
}

export interface Perfr {
  /** Run `fn` inside a named scope on the current thread. */
  scope<T>(name: string, fn: () => T): T;
  scopeAsync<T>(name: string, fn: () => Promise<T>): Promise<T>;
  begin(name: string): number;
  end(handle: number): void;
  beginAsync(name: string): number;
  endAsync(handle: number): void;
  /** A scope that has already finished. Times are nanoseconds from {@link nowNs}. */
  span(name: string, startNs: number, endNs: number): void;
  spanAsync(name: string, startNs: number, endNs: number): void;
  /** Zero-duration marker. */
  mark(name: string): void;
  /** Zero-duration frame marker. The name defaults to "frame". */
  frame(name?: string): void;
  /** A scalar sample. Drawn as its own lane. */
  value(name: string, value: number): void;
  /** Alias of {@link value}. */
  plot(name: string, value: number): void;
  nowNs(): number;
  nameThread(name: string, tid?: number): void;
  nameProcess(name: string): void;
  setTid(tid: number): void;
  /**
   * Stage-3 method decorator, and the older `(target, key, descriptor)` form.
   * `@perfr.decorate("physics")` or `@perfr.decorate()`.
   */
  decorate(name?: string): PerfrDecorator;
  /**
   * Load the encoder and, unless `browser` is false, start the built-in
   * tracks. After {@link stop}, records again into the same ring.
   */
  start(options?: StartOptions): Promise<void>;
  /**
   * Stop recording. Open scopes are closed. The ring is kept for export.
   * Scopes recorded after this are dropped until {@link start}.
   */
  stop(): void;
  /** Drop the ring and start a fresh capture. */
  clear(): void;
  /** The ring as one `.perfr.stream`. */
  exportStream(): Promise<Uint8Array>;
  exportBlob(): Promise<Blob>;
  download(filename?: string): Promise<void>;
  /** Mount the live viewer and stream this page into it. */
  attachViewer(options: AttachViewerOptions): ViewerHandle;
  detachViewer(): void;
  /** A worker that calls `installWorker` from `@perfr/web/worker`. */
  attachWorker(worker: Worker, options?: WorkerOptions): number;
  tracks: {
    set(partial: TrackToggles): void;
    get(): Record<TrackId, boolean> | null;
    support(): Record<TrackId, TrackSupport>;
  };
  stats(): ReturnType<Profiler["stats"]>;
  /** For the Emscripten shim: bytes since the last pull. */
  takeFlush(): Uint8Array;
  takeSnapshot(): Uint8Array;
  readonly pagePid: number;
}

/**
 * Both decorator shapes in one signature. The result is `any` because the
 * compiler checks it against the decorated member's own type, which differs
 * per use and per decorator mode; `unknown` there is a compile error.
 */
export type PerfrDecorator = (
  value: unknown,
  context?: { kind?: string; name?: string | symbol } | string | symbol,
  descriptor?: PropertyDescriptor,
) => any;

type AnyMethod = (this: unknown, ...args: unknown[]) => unknown;

function decorator(profiler: Profiler, name?: string): PerfrDecorator {
  const scoped = (fn: AnyMethod, label: string): AnyMethod =>
    function (this: unknown, ...args: unknown[]) {
      return profiler.scope(label, () => fn.apply(this, args));
    };
  return function decorate(value, context, descriptor) {
    // The older form: `(target, key, descriptor)`. The descriptor is what
    // gets wrapped, and what the caller defines the member from.
    if (typeof context === "string" || typeof context === "symbol") {
      if (!descriptor) return undefined;
      const label = name ?? String(context);
      for (const slot of ["value", "get", "set"] as const) {
        const fn = descriptor[slot];
        if (typeof fn === "function") descriptor[slot] = scoped(fn as AnyMethod, label);
      }
      return descriptor;
    }
    if (context && (context.kind === "method" || context.kind === "getter" || context.kind === "setter")) {
      return scoped(value as AnyMethod, name ?? String(context.name ?? "scope"));
    }
    return value;
  };
}

export function createPerfr(): Perfr {
  const profiler = new Profiler();
  let browser: BrowserHandles | null = null;
  let viewer: ViewerHandle | null = null;
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
  let autoStart: Promise<void> | null = null;
  let autoStartFailed = false;
  let browserOpt: boolean | TrackToggles | undefined = true;

  const api: Perfr = {
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
      if (options && "browser" in options) browserOpt = options.browser;
      if (options?.capacity != null) profiler.setCapacity(options.capacity);
      const wasmUrl = options?.wasmUrl;
      stopped = false;
      autoStartFailed = false;
      // Before the encoder loads, so what is recorded meanwhile is kept.
      profiler.resume();
      return profiler.start(wasmUrl).then(() => {
        if (stopped || started) return;
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
      if (stopped) autoStarted(api.start({ browser: browserOpt }));
    },
    exportStream: () => profiler.exportStream(),
    exportBlob: () => profiler.exportBlob(),
    download: (filename) => profiler.download(filename),
    attachViewer(options) {
      viewer?.close();
      if (!stopped) kick();
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
  function autoStarted(start: Promise<void>): void {
    start.catch((err) => {
      autoStartFailed = true;
      console.warn("perfr: the encoder did not load, so nothing is recorded. Call perfr.start() to retry.", err);
    });
  }
  /** One start per load, not one per recorded scope while it is loading. */
  function kick(): void {
    if (started || stopped || autoStart || autoStartFailed) return;
    const pending = api.start({ browser: browserOpt }).finally(() => {
      if (autoStart === pending) autoStart = null;
    });
    autoStart = pending;
    autoStarted(pending);
  }
  const wrap =
    <A extends unknown[], R>(fn: (...args: A) => R) =>
    (...args: A): R => {
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
export const perfr: Perfr = createPerfr();
