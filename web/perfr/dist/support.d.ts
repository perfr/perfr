import type { TrackId } from "./tracks.js";
export interface TrackSupport {
    available: boolean;
    /** What the page can actually see. Not a promise that entries will fire. */
    note: string;
}
/**
 * Feature detection for the built-in tracks. `available` is true only when
 * this browser exposes the API. It does not mean every field is filled in:
 * `gc` and long animation frames are Chromium-only, `performance.memory` is
 * a non-standard Chromium extension, and `measureUserAgentSpecificMemory`
 * also needs cross-origin isolation (and still rejects in many embeds).
 */
export declare function detectSupport(): Record<TrackId, TrackSupport>;
