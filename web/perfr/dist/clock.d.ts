/** Nanoseconds since this context's `performance.timeOrigin`. */
export declare function nowNs(): number;
/**
 * Nanoseconds on the page's clock, from inside a worker.
 * A worker's `timeOrigin` is later than the page's, so `performance.now()`
 * alone is not comparable. `pageOrigin` is `performance.timeOrigin` copied
 * from the page when the worker is attached.
 */
export declare function nowNsAligned(pageOrigin: number): number;
