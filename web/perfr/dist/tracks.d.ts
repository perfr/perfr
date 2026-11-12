/** Synthetic pid of the page. Matches `perfr_web::PAGE_PID`. Not an OS pid. */
export declare const PAGE_PID = 2970288129;
/**
 * Thread ids the viewer shows as separate tracks. Workers are allocated
 * from {@link WORKER_BASE} upward.
 */
export declare const TID: {
    readonly main: 1;
    readonly frames: 2;
    readonly gc: 3;
    readonly longTasks: 4;
    readonly loaf: 5;
    readonly input: 6;
    readonly layout: 7;
    readonly paint: 8;
    readonly resources: 9;
    readonly fetch: 10;
    readonly navigation: 11;
    readonly userTiming: 12;
    readonly visibility: 13;
    readonly workerMsg: 14;
    readonly gpu: 15;
};
export declare const WORKER_BASE = 32;
export type TrackId = "frames" | "gc" | "longTasks" | "loaf" | "input" | "layout" | "paint" | "resources" | "fetch" | "navigation" | "userTiming" | "visibility" | "workerMessages" | "webgl";
export type TrackToggles = Partial<Record<TrackId, boolean>>;
export declare const TRACK_IDS: TrackId[];
export declare function allTracks(on: boolean): Record<TrackId, boolean>;
export declare function threadName(tid: number): string | undefined;
