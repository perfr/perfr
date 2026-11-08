// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { startBrowser, type BrowserHandles } from "./browser.js";
import { attachViewer, type AttachViewerOptions, type ViewerHandle } from "./embed.js";
import { Profiler } from "./profiler.js";
import { detectSupport, type TrackSupport } from "./support.js";
import { TID, type TrackId, type TrackToggles } from "./tracks.js";
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
  /** Load the encoder and, unless `browser` is false, start the built-in tracks. */
  start(options?: StartOptions): Promise<void>;
  /** Stop recording. Open scopes are closed. The ring is kept for export. */
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

export type PerfrDecorator = (
  value: unknown,
  context?: { kind?: string; name?: string | symbol },
) => unknown;

function decorator(profiler: Profiler, name?: string): PerfrDecorator {
  return function decorate(value, context) {
    if (context && (context.kind === "method" || context.kind === "getter" || context.kind === "setter")) {
      const label = name ?? String(context.name ?? "scope");
      const fn = value as (this: unknown, ...args: unknown[]) => unknown;
      return function (this: unknown, ...args: unknown[]) {
        return profiler.scope(label, () => fn.apply(this, args));
      };
    }
    return value;
  };
}

export function createPerfr(): Perfr {
  const profiler = new Profiler();
  let browser: BrowserHandles | null = null;
  let viewer: ViewerHandle | null = null;
  let started = false;
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
    nameThread: (name, tid) => profiler.nameThread(name, tid ?? TID.main),
    nameProcess: (name) => profiler.nameProcess(name),
    setTid: (tid) => profiler.setTid(tid),
    decorate: (name) => decorator(profiler, name),
    start(options) {
      if (options && "browser" in options) browserOpt = options.browser;
      const wasmUrl = options?.wasmUrl;
      return profiler.start(wasmUrl).then(() => {
        if (started) return;
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
    if (!started) void api.start({ browser: browserOpt });
  };
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
