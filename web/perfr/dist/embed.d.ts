import type { Profiler } from "./profiler.js";
export interface AttachViewerOptions {
    /** Element the viewer fills. Omit for an overlay panel or a popup. */
    container?: HTMLElement;
    /** Floating panel. Default when `container` and `popup` are omitted. */
    overlay?: boolean;
    /** Open the viewer in its own window. Popups must be opened from a click. */
    popup?: boolean;
    /**
     * Toggle the overlay. Default "F8". `null` disables the hotkey.
     * Ignored for a popup.
     */
    hotkey?: string | null;
    /**
     * URL of the perfr viewer's `index.html` (the files in
     * `crates/perfr-live-viewer/viewer-dist`). The page is opened with
     * `embed=1`. `?capture=` on that URL is left alone and embed mode stays off.
     */
    viewerUrl: string;
    /** Panel width in CSS pixels. Overlay only. */
    width?: number;
    /** Panel height in CSS pixels. Overlay only. */
    height?: number;
}
export interface ViewerHandle {
    close(): void;
    show(): void;
    hide(): void;
    toggle(): void;
    readonly element: HTMLElement | null;
    readonly usingSharedBuffer: boolean;
}
/** What the viewer's Stop, Record and Clear buttons call on the page. */
export interface ViewerControls {
    stop(): void;
    start(): void;
    clear(): void;
}
/**
 * Mount the existing live viewer and stream this profiler's frames into it.
 * The viewer page (embed.js) accepts the stream on a MessageChannel. When the
 * page is cross-origin isolated, encoded bytes also go through a
 * SharedArrayBuffer; the channel is the fallback when that ring is full or
 * SharedArrayBuffer is missing.
 *
 * Share in the viewer reads the ring back through the same channel, then
 * posts it with the viewer's existing Share dialog.
 */
export declare function attachViewer(profiler: Profiler, options: AttachViewerOptions, controls?: ViewerControls): ViewerHandle;
