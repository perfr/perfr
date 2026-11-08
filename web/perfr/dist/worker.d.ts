import type { Profiler } from "./profiler.js";
export interface WorkerOptions {
    name?: string;
    tid?: number;
}
/**
 * Call once in a dedicated worker. Scopes recorded here are posted to the
 * page that called `perfr.attachWorker` and show up as their own thread.
 * The worker clock is shifted onto the page's `timeOrigin`.
 */
export declare function installWorker(options?: WorkerOptions): WorkerProfiler;
declare class WorkerProfiler {
    /** Added to this worker's clock to land on the page's `timeOrigin`. */
    private shiftNs;
    private tid;
    private configured;
    private queue;
    private local;
    private open;
    private label;
    constructor(name?: string);
    configure(pageOrigin: number, tid: number, name?: string): void;
    /**
     * Nanoseconds on the page's clock. Before the page has attached this
     * worker the shift is unknown, so scopes are stored on the worker's own
     * clock and shifted when they are sent.
     */
    nowNs(): number;
    private localNs;
    scope<T>(name: string, fn: () => T): T;
    scopeAsync<T>(name: string, fn: () => Promise<T>): Promise<T>;
    begin(name: string): number;
    end(handle: number): void;
    beginAsync(name: string): number;
    endAsync(handle: number): void;
    mark(name: string): void;
    value(name: string, value: number): void;
    private finish;
    private scheduled;
    private schedule;
    private flush;
}
/**
 * Treat `worker` as another thread. Its `installWorker` scopes are merged
 * into this profiler, and postMessage in both directions is marked on the
 * Worker messages track.
 */
export declare function attachWorker(profiler: Profiler, worker: Worker, options?: WorkerOptions): number;
export {};
