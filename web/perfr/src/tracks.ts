// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/** Synthetic pid of the page. Matches `perfr_web::PAGE_PID`. Not an OS pid. */
export const PAGE_PID = 0xb10b0001;

/**
 * Thread ids the viewer shows as separate tracks. Workers are allocated
 * from {@link WORKER_BASE} upward.
 */
export const TID = {
  main: 1,
  frames: 2,
  gc: 3,
  longTasks: 4,
  loaf: 5,
  input: 6,
  layout: 7,
  paint: 8,
  resources: 9,
  fetch: 10,
  navigation: 11,
  userTiming: 12,
  visibility: 13,
  workerMsg: 14,
  gpu: 15,
} as const;

export const WORKER_BASE = 32;

export type TrackId =
  | "frames"
  | "gc"
  | "longTasks"
  | "loaf"
  | "input"
  | "layout"
  | "paint"
  | "resources"
  | "fetch"
  | "navigation"
  | "userTiming"
  | "visibility"
  | "workerMessages"
  | "webgl";

export type TrackToggles = Partial<Record<TrackId, boolean>>;

export const TRACK_IDS: TrackId[] = [
  "frames",
  "gc",
  "longTasks",
  "loaf",
  "input",
  "layout",
  "paint",
  "resources",
  "fetch",
  "navigation",
  "userTiming",
  "visibility",
  "workerMessages",
  "webgl",
];

export function allTracks(on: boolean): Record<TrackId, boolean> {
  const out = {} as Record<TrackId, boolean>;
  for (const id of TRACK_IDS) out[id] = on;
  return out;
}

const THREAD_NAME: Record<number, string> = {
  [TID.main]: "main",
  [TID.frames]: "Frames",
  [TID.gc]: "GC",
  [TID.longTasks]: "Long tasks",
  [TID.loaf]: "Long animation frames",
  [TID.input]: "Input",
  [TID.layout]: "Layout shifts",
  [TID.paint]: "Paint",
  [TID.resources]: "Resources",
  [TID.fetch]: "Fetch",
  [TID.navigation]: "Navigation",
  [TID.userTiming]: "User Timing",
  [TID.visibility]: "Visibility",
  [TID.workerMsg]: "Worker messages",
  [TID.gpu]: "WebGL / WebGPU",
};

export function threadName(tid: number): string | undefined {
  return THREAD_NAME[tid];
}
