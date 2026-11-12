import type { Profiler } from "./profiler.js";
import { type TrackSupport } from "./support.js";
import { type TrackId, type TrackToggles } from "./tracks.js";
export interface BrowserHandles {
    set(partial: TrackToggles): void;
    get(): Record<TrackId, boolean>;
    support(): Record<TrackId, TrackSupport>;
    stop(): void;
}
export interface BrowserOptions {
    /**
     * Also record what the browser buffered before the tracks started:
     * earlier resources, paints, marks, and the navigation. Default true.
     * Pass false when starting again over a ring that already holds them.
     */
    replayBuffered?: boolean;
}
/**
 * Browser timings as their own tracks. Every API is feature-detected.
 * Turning a track off disconnects its observer; it does not delete events
 * already recorded.
 */
export declare function startBrowser(profiler: Profiler, initial?: boolean | TrackToggles, options?: BrowserOptions): BrowserHandles;
