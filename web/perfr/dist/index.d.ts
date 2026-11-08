import { type AttachViewerOptions, type ViewerHandle } from "./embed.js";
import { Profiler } from "./profiler.js";
import { type TrackSupport } from "./support.js";
import { type TrackId, type TrackToggles } from "./tracks.js";
import { type WorkerOptions } from "./worker.js";
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
export type PerfrDecorator = (value: unknown, context?: {
    kind?: string;
    name?: string | symbol;
}) => unknown;
export declare function createPerfr(): Perfr;
/** The page-wide profiler. */
export declare const perfr: Perfr;
