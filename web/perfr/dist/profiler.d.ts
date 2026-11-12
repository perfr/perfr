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
type FlushListener = (packet: FlushPacket) => void;
/**
 * Records into the WASM ring. Calls before the module has loaded are queued
 * with their original timestamps and replayed in order, so depth still matches
 * the nesting the page actually ran.
 */
export declare class Profiler {
    pagePid: number;
    private wasm;
    private loading;
    private buf;
    private droppedQueue;
    private localHandle;
    private wasmHandles;
    private tid;
    private capacity;
    private listeners;
    private scheduled;
    private flushTimer;
    private raf;
    /** Set by {@link stop}. Recording calls are dropped here, encoder or not. */
    private paused;
    private pausedAt;
    constructor(capacity?: number);
    /** Ring length. Takes effect when the encoder loads; later calls are ignored. */
    setCapacity(capacity: number): void;
    setTid(tid: number): void;
    currentTid(): number;
    nowNs(): number;
    /** Load the Rust encoder. Safe to call more than once. */
    start(wasmUrl?: string): Promise<void>;
    onFlush(listener: FlushListener): () => void;
    scope<T>(name: string, fn: () => T, tid?: number): T;
    scopeAsync<T>(name: string, fn: () => Promise<T>, tid?: number): Promise<T>;
    /** `time` is for a scope that began elsewhere, such as in a worker. */
    begin(name: string, tid?: number, time?: number): number;
    end(handle: number, time?: number): void;
    beginAsync(name: string, tid?: number): number;
    endAsync(handle: number): void;
    span(name: string, startNs: number, endNs: number, tid?: number): void;
    spanAsync(name: string, startNs: number, endNs: number, tid?: number): void;
    mark(name: string, tid?: number, time?: number): void;
    frame(name?: string, tid?: number): void;
    value(name: string, value: number, tid?: number, time?: number): void;
    plot(name: string, value: number, tid?: number): void;
    nameThread(name: string, tid?: number): void;
    nameProcess(name: string): void;
    /**
     * Stop recording. This holds before the encoder has loaded too: what was
     * queued until now is kept, what is recorded from here on is dropped.
     */
    stop(): void;
    /**
     * Record again after {@link stop}. The ring is kept. The stream starts
     * over (hello, names, the ring) so an attached viewer stays consistent.
     */
    resume(): void;
    /** Drop the ring and record again. Thread and process names are kept. */
    clear(): void;
    resync(): void;
    stats(): Stats;
    /** Replay the queue and return the new `.perfr.stream` frames. */
    flushNow(): Uint8Array;
    exportStream(): Promise<Uint8Array>;
    exportBlob(): Promise<Blob>;
    download(filename?: string): Promise<void>;
    /** Bytes from the last flush, for the Emscripten EM_JS shim. */
    takeFlush(): Uint8Array;
    takeSnapshot(): Uint8Array;
    private push;
    private schedule;
    private cancelSchedule;
    private replay;
    /** Close what is open at {@link pausedAt} and end the stream. */
    private stopRing;
    private emit;
    private nameBuiltins;
    private load;
}
export {};
