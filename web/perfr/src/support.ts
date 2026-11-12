// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import type { TrackId } from "./tracks.js";

export interface TrackSupport {
  available: boolean;
  /** What the page can actually see. Not a promise that entries will fire. */
  note: string;
}

function entryTypes(): string[] {
  const PO = (globalThis as unknown as { PerformanceObserver?: { supportedEntryTypes?: readonly string[] } })
    .PerformanceObserver;
  const list = PO?.supportedEntryTypes;
  return Array.isArray(list) ? list.slice() : [];
}

function hasType(types: string[], name: string): boolean {
  return types.includes(name);
}

/**
 * Feature detection for the built-in tracks. `available` is true only when
 * this browser exposes the API. It does not mean every field is filled in:
 * `gc` and long animation frames are Chromium-only, `performance.memory` is
 * a non-standard Chromium extension, and `measureUserAgentSpecificMemory`
 * also needs cross-origin isolation (and still rejects in many embeds).
 */
export function detectSupport(): Record<TrackId, TrackSupport> {
  const types = entryTypes();
  const perf = globalThis.performance as Performance & {
    memory?: { usedJSHeapSize?: number };
    measureUserAgentSpecificMemory?: () => Promise<unknown>;
  };
  const isolated = (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated === true;
  const hasMemory = typeof perf?.memory?.usedJSHeapSize === "number";
  const hasUaMemory = typeof perf?.measureUserAgentSpecificMemory === "function";
  const hasRaf = typeof globalThis.requestAnimationFrame === "function";
  const hasObserver = typeof (globalThis as { PerformanceObserver?: unknown }).PerformanceObserver === "function";
  const gpuQueue = (globalThis as { GPUQueue?: unknown }).GPUQueue;
  const webgl = typeof (globalThis as { WebGLRenderingContext?: unknown }).WebGLRenderingContext === "function";

  return {
    frames: {
      available: hasRaf,
      note: "requestAnimationFrame deltas. The refresh rate is an estimate from the median frame spacing; browsers do not expose the display's refresh rate to the page.",
    },
    gc: {
      available: hasType(types, "gc") || hasMemory || hasUaMemory,
      note: hasType(types, "gc")
        ? "PerformanceObserver 'gc' is implemented in Chromium. Other browsers will not deliver entries."
        : hasMemory
          ? "performance.memory (usedJSHeapSize) is a non-standard Chromium property. GC slices themselves are not reported here."
          : hasUaMemory
            ? isolated
              ? "measureUserAgentSpecificMemory is available. It is slow, may reject, and needs cross-origin isolation."
              : "measureUserAgentSpecificMemory exists but this page is not cross-origin isolated, so the call is expected to reject."
            : "No GC observer, performance.memory, or measureUserAgentSpecificMemory in this browser.",
    },
    longTasks: {
      available: hasType(types, "longtask"),
      note: "Long Tasks API (PerformanceObserver 'longtask'). Shipped in Chromium. Not in Firefox or Safari.",
    },
    loaf: {
      available: hasType(types, "long-animation-frame"),
      note: "Long Animation Frames (PerformanceObserver 'long-animation-frame'). Chromium 123 and later. Not in Firefox or Safari.",
    },
    input: {
      available: hasType(types, "event"),
      note: "Event Timing API. Chromium reports interactionId, which this track turns into an approximate INP (98th percentile of interaction durations seen this session). That is not Chrome's field INP over a full page visit. Firefox exposes some event timing and often no interactionId. Safari does not implement this entry type.",
    },
    layout: {
      available: hasType(types, "layout-shift"),
      note: "Layout Instability API. Chromium. Entries with hadRecentInput are ignored, matching the CLS spec. Other browsers only show this track when they list 'layout-shift' in supportedEntryTypes.",
    },
    paint: {
      available: hasType(types, "paint") || hasType(types, "largest-contentful-paint"),
      note: "Paint Timing (first-paint, first-contentful-paint) is widely available. Largest Contentful Paint is Chromium and Safari; Firefox does not implement 'largest-contentful-paint'.",
    },
    resources: {
      available: hasType(types, "resource") || typeof perf?.getEntriesByType === "function",
      note: "Resource Timing. Scripts, stylesheets, images and other initiator types. Fetch and XHR are filed on the Fetch track instead.",
    },
    fetch: {
      available: typeof globalThis.fetch === "function" || hasType(types, "resource"),
      note: "fetch() calls made after the track starts, plus Resource Timing entries whose initiator is xmlhttprequest. A fetch() this package wrapped is not also copied from Resource Timing.",
    },
    navigation: {
      available: hasType(types, "navigation") || typeof perf?.getEntriesByType === "function",
      note: "Navigation Timing Level 2 (redirect, DNS, connection, request, response, DOMContentLoaded, load). One navigation entry for this document.",
    },
    userTiming: {
      available: hasObserver && (hasType(types, "measure") || hasType(types, "mark") || typeof perf?.mark === "function"),
      note: "User Timing marks and measures, including entries already in the buffer when the track starts.",
    },
    visibility: {
      available: typeof globalThis.document !== "undefined" && "visibilityState" in globalThis.document,
      note: "visibilitychange, pagehide and pageshow. The Page Lifecycle API, widely available.",
    },
    workerMessages: {
      available:
        typeof globalThis.Worker === "function" ||
        typeof (globalThis as { WorkerGlobalScope?: unknown }).WorkerGlobalScope === "function",
      note: "Only workers passed to perfr.attachWorker, or a worker that calls installWorker. postMessage on other workers is not observed.",
    },
    webgl: {
      available: webgl || typeof gpuQueue === "function",
      note: "JS time around WebGL draw calls and GPUQueue.submit. This is not GPU-side time: timestamp queries and the GPU profiler are not used. WebGPU submit is patched only when GPUQueue exists (Chromium, and Safari where WebGPU is enabled). Firefox has no WebGPU by default.",
    },
  };
}
