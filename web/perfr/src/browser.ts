// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import type { Profiler } from "./profiler.js";
import { detectSupport, type TrackSupport } from "./support.js";
import { TID, TRACK_IDS, allTracks, type TrackId, type TrackToggles } from "./tracks.js";

export interface BrowserHandles {
  set(partial: TrackToggles): void;
  get(): Record<TrackId, boolean>;
  support(): Record<TrackId, TrackSupport>;
  stop(): void;
}

type Entry = PerformanceEntry & {
  value?: number;
  hadRecentInput?: boolean;
  interactionId?: number;
  detail?: { type?: string };
  blockingDuration?: number;
  initiatorType?: string;
};

function ms(entryTime: number): number {
  return entryTime * 1e6;
}

function shortUrl(url: string): string {
  try {
    const u = new URL(url, globalThis.location?.href);
    const path = u.pathname.split("/").filter(Boolean).slice(-2).join("/");
    return path || u.host || url;
  } catch {
    return url.slice(0, 80);
  }
}

function supportedTypes(): string[] {
  const PO = (globalThis as unknown as { PerformanceObserver?: { supportedEntryTypes?: readonly string[] } })
    .PerformanceObserver;
  return PO?.supportedEntryTypes ? PO.supportedEntryTypes.slice() : [];
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
export function startBrowser(
  profiler: Profiler,
  initial?: boolean | TrackToggles,
  options: BrowserOptions = {},
): BrowserHandles {
  const replayBuffered = options.replayBuffered !== false;
  const enabled = allTracks(true);
  if (initial === false) {
    for (const id of TRACK_IDS) enabled[id] = false;
  } else if (initial && typeof initial === "object") {
    for (const id of TRACK_IDS) {
      if (id in initial) enabled[id] = Boolean(initial[id]);
    }
  }

  const support = detectSupport();
  const stops: Array<() => void> = [];
  let stopped = false;
  const types = supportedTypes();

  const observe = (
    type: string,
    active: () => boolean,
    cb: (entry: Entry) => void,
    options?: Record<string, unknown>,
  ) => {
    if (!types.includes(type) || typeof PerformanceObserver === "undefined") return;
    let obs: PerformanceObserver | null = null;
    // The buffer is read once. An observer armed again, after its track was
    // switched off and on, would deliver the same entries a second time.
    let buffered = replayBuffered;
    const arm = () => {
      if (obs || stopped) return;
      obs = new PerformanceObserver((list) => {
        if (!active()) return;
        for (const entry of list.getEntries()) cb(entry as Entry);
      });
      try {
        obs.observe({ type, buffered, ...options });
        buffered = false;
      } catch {
        obs = null;
      }
    };
    const disarm = () => {
      obs?.disconnect();
      obs = null;
    };
    if (active()) arm();
    stops.push(disarm);
    return { arm, disarm };
  };

  const controls: Array<{ track: TrackId; arm: () => void; disarm: () => void }> = [];

  // Frames, jank, refresh estimate.
  {
    let raf = 0;
    let last = 0;
    let frames = 0;
    let windowStart = 0;
    const samples: number[] = [];
    const loop = (t: number) => {
      if (stopped || !enabled.frames) return;
      if (last > 0) {
        const dt = t - last;
        const end = ms(t);
        profiler.span("frame", ms(last), end, TID.frames);
        profiler.value("frame time", dt, TID.frames, end);
        samples.push(dt);
        if (samples.length > 90) samples.shift();
        const sorted = samples.slice().sort((a, b) => a - b);
        const med = sorted[sorted.length >> 1] || dt;
        if (med > 0 && dt > med * 1.5 && samples.length > 8) {
          const dropped = Math.max(1, Math.round(dt / med) - 1);
          profiler.mark(dropped > 1 ? `jank +${dropped}` : "jank", TID.frames, end);
          profiler.value("dropped frames", dropped, TID.frames, end);
        }
        frames++;
        if (t - windowStart >= 500) {
          profiler.value("fps", (frames * 1000) / (t - windowStart), TID.frames, end);
          profiler.value("refresh estimate", med > 0 ? 1000 / med : 0, TID.frames, end);
          frames = 0;
          windowStart = t;
        }
      } else {
        windowStart = t;
      }
      last = t;
      raf = requestAnimationFrame(loop);
    };
    const arm = () => {
      if (raf || stopped || !enabled.frames || typeof requestAnimationFrame !== "function") return;
      last = 0;
      raf = requestAnimationFrame(loop);
    };
    const disarm = () => {
      if (raf && typeof cancelAnimationFrame === "function") cancelAnimationFrame(raf);
      raf = 0;
    };
    controls.push({ track: "frames", arm, disarm });
    arm();
    stops.push(disarm);
  }

  // GC slices where the browser has them, plus heap gauges.
  {
    const gcObs = observe("gc", () => enabled.gc, (entry) => {
      const kind = entry.detail?.type ? `gc ${entry.detail.type}` : "gc";
      profiler.span(kind, ms(entry.startTime), ms(entry.startTime + entry.duration), TID.gc);
    });
    let timer = 0;
    const poll = () => {
      if (stopped || !enabled.gc) return;
      const perf = performance as Performance & {
        memory?: { usedJSHeapSize: number; totalJSHeapSize: number };
      };
      const now = profiler.nowNs();
      if (perf.memory && typeof perf.memory.usedJSHeapSize === "number") {
        profiler.value("js heap", perf.memory.usedJSHeapSize, TID.gc, now);
        profiler.value("js heap total", perf.memory.totalJSHeapSize, TID.gc, now);
      }
    };
    // Bound: called bare, a Performance method rejects with "Illegal invocation".
    const ua = (performance as Performance & {
      measureUserAgentSpecificMemory?: () => Promise<{ bytes: number }>;
    }).measureUserAgentSpecificMemory?.bind(performance);
    let uaTimer = 0;
    const measureUa = () => {
      if (stopped || !enabled.gc || typeof ua !== "function") return;
      ua()
        .then((result) => {
          if (result && typeof result.bytes === "number") {
            profiler.value("heap (measureUserAgentSpecificMemory)", result.bytes, TID.gc);
          }
        })
        .catch(() => {
          /* Isolated contexts only, and the call is allowed to reject. */
        });
    };
    const arm = () => {
      gcObs?.arm();
      if (!timer) timer = window.setInterval(poll, 1000);
      if (!uaTimer && typeof ua === "function") {
        measureUa();
        uaTimer = window.setInterval(measureUa, 20_000);
      }
    };
    const disarm = () => {
      gcObs?.disarm();
      if (timer) clearInterval(timer);
      if (uaTimer) clearInterval(uaTimer);
      timer = 0;
      uaTimer = 0;
    };
    controls.push({ track: "gc", arm, disarm });
    if (enabled.gc) arm();
    stops.push(disarm);
  }

  observe("longtask", () => enabled.longTasks, (entry) => {
    const name = entry.name && entry.name !== "unknown" ? entry.name : "long task";
    profiler.span(name, ms(entry.startTime), ms(entry.startTime + entry.duration), TID.longTasks);
  });

  observe("long-animation-frame", () => enabled.loaf, (entry) => {
    profiler.span("long animation frame", ms(entry.startTime), ms(entry.startTime + entry.duration), TID.loaf);
    if (typeof entry.blockingDuration === "number") {
      profiler.value("blocking", entry.blockingDuration, TID.loaf, ms(entry.startTime + entry.duration));
    }
  });

  {
    const interactions: number[] = [];
    observe(
      "event",
      () => enabled.input,
      (entry) => {
        const end = ms(entry.startTime + entry.duration);
        profiler.span(entry.name || "event", ms(entry.startTime), end, TID.input);
        const id = entry.interactionId ?? 0;
        if (id) {
          interactions.push(entry.duration);
          if (interactions.length > 400) interactions.shift();
          const sorted = interactions.slice().sort((a, b) => a - b);
          const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(sorted.length * 0.98) - 1));
          profiler.value("INP (approx)", sorted[index], TID.input, end);
          profiler.value("interaction max", sorted[sorted.length - 1], TID.input, end);
        }
      },
      { durationThreshold: 16 },
    );
  }

  {
    let cls = 0;
    observe("layout-shift", () => enabled.layout, (entry) => {
      if (entry.hadRecentInput) return;
      const value = entry.value ?? 0;
      cls += value;
      const t = ms(entry.startTime);
      profiler.mark(`shift ${value.toFixed(4)}`, TID.layout, t);
      profiler.value("CLS", cls, TID.layout, t);
    });
  }

  observe("paint", () => enabled.paint, (entry) => {
    profiler.mark(entry.name || "paint", TID.paint, ms(entry.startTime));
  });
  observe("largest-contentful-paint", () => enabled.paint, (entry) => {
    const t = ms(entry.startTime);
    profiler.mark("LCP", TID.paint, t);
    profiler.value("LCP", entry.startTime, TID.paint, t);
  });

  const fetchWrappedAt = { t: -1 };
  const resourceObs = observe("resource", () => enabled.resources || enabled.fetch, (entry) => {
    const kind = entry.initiatorType || "resource";
    const label = `${kind} ${shortUrl(entry.name)}`;
    const start = ms(entry.startTime);
    const end = ms(entry.startTime + entry.duration);
    if (kind === "fetch" || kind === "xmlhttprequest") {
      if (kind === "fetch" && fetchWrappedAt.t >= 0 && entry.startTime >= fetchWrappedAt.t) return;
      if (!enabled.fetch) return;
      profiler.spanAsync(kind === "xmlhttprequest" ? label : `net ${shortUrl(entry.name)}`, start, end, TID.fetch);
      return;
    }
    if (!enabled.resources) return;
    profiler.spanAsync(label, start, end, TID.resources);
  });

  // fetch() wrapper. Resource Timing for those calls is skipped above.
  {
    const orig = globalThis.fetch?.bind(globalThis);
    let wrapped = false;
    const arm = () => {
      if (wrapped || !orig || stopped) return;
      fetchWrappedAt.t = performance.now();
      globalThis.fetch = ((input: RequestInfo | URL, init?: RequestInit) => {
        if (!enabled.fetch) return orig(input, init);
        const label = `fetch ${shortUrl(String(input instanceof Request ? input.url : input))}`;
        const handle = profiler.beginAsync(label, TID.fetch);
        return orig(input, init).then(
          (res) => {
            profiler.endAsync(handle);
            return res;
          },
          (err) => {
            profiler.endAsync(handle);
            throw err;
          },
        );
      }) as typeof fetch;
      wrapped = true;
    };
    const disarm = () => {
      if (wrapped && orig) globalThis.fetch = orig as typeof fetch;
      wrapped = false;
    };
    const armFetch = () => {
      arm();
      resourceObs?.arm();
    };
    controls.push({ track: "fetch", arm: armFetch, disarm });
    controls.push({
      track: "resources",
      arm: () => resourceObs?.arm(),
      disarm: () => {
        if (!enabled.fetch) resourceObs?.disarm();
      },
    });
    if (enabled.fetch) arm();
    stops.push(disarm);
  }

  if (typeof performance?.getEntriesByType === "function") {
    const nav = performance.getEntriesByType("navigation")[0] as PerformanceNavigationTiming | undefined;
    if (nav && enabled.navigation && replayBuffered) {
      const piece = (name: string, a: number, b: number) => {
        if (!Number.isFinite(a) || !Number.isFinite(b) || b <= a || a < 0) return;
        profiler.span(name, ms(a), ms(b), TID.navigation);
      };
      piece("redirect", nav.redirectStart, nav.redirectEnd);
      piece("DNS", nav.domainLookupStart, nav.domainLookupEnd);
      piece("connect", nav.connectStart, nav.connectEnd);
      if (nav.secureConnectionStart > 0) piece("TLS", nav.secureConnectionStart, nav.connectEnd);
      piece("request", nav.requestStart, nav.responseStart);
      piece("response", nav.responseStart, nav.responseEnd);
      piece("DOM", nav.responseEnd, nav.domContentLoadedEventEnd);
      piece("load", nav.loadEventStart, nav.loadEventEnd);
    }
  }

  observe("mark", () => enabled.userTiming, (entry) => {
    profiler.mark(entry.name || "mark", TID.userTiming, ms(entry.startTime));
  });
  observe("measure", () => enabled.userTiming, (entry) => {
    profiler.span(entry.name || "measure", ms(entry.startTime), ms(entry.startTime + entry.duration), TID.userTiming);
  });

  {
    const onVis = () => {
      if (!enabled.visibility) return;
      profiler.mark(`visibility ${document.visibilityState}`, TID.visibility);
    };
    const onHide = () => {
      if (enabled.visibility) profiler.mark("pagehide", TID.visibility);
    };
    const onShow = () => {
      if (enabled.visibility) profiler.mark("pageshow", TID.visibility);
    };
    if (typeof document !== "undefined") {
      document.addEventListener("visibilitychange", onVis);
      window.addEventListener("pagehide", onHide);
      window.addEventListener("pageshow", onShow);
      if (enabled.visibility) onVis();
      stops.push(() => {
        document.removeEventListener("visibilitychange", onVis);
        window.removeEventListener("pagehide", onHide);
        window.removeEventListener("pageshow", onShow);
      });
    }
  }

  // WebGL draw counts and WebGPU submit, timed on the JS side only.
  {
    let draws = 0;
    let drawMs = 0;
    let submits = 0;
    let raf = 0;
    const flushGpu = (t: number) => {
      if (stopped || !enabled.webgl) return;
      const end = ms(t);
      if (draws > 0) {
        profiler.span("webgl draws", end - drawMs * 1e6, end, TID.gpu);
        profiler.value("webgl draw calls", draws, TID.gpu, end);
        draws = 0;
        drawMs = 0;
      }
      if (submits > 0) {
        profiler.value("webgpu submits", submits, TID.gpu, end);
        submits = 0;
      }
      raf = requestAnimationFrame(flushGpu);
    };
    const wrapDraw = (ctx: WebGLRenderingContext | WebGL2RenderingContext) => {
      const names = ["drawArrays", "drawElements", "drawArraysInstanced", "drawElementsInstanced", "drawRangeElements"] as const;
      for (const name of names) {
        const fn = (ctx as unknown as Record<string, unknown>)[name];
        if (typeof fn !== "function" || (fn as { __perfr?: boolean }).__perfr) continue;
        const orig = fn as (...args: unknown[]) => unknown;
        const wrapped = function (this: unknown, ...args: unknown[]) {
          const a = performance.now();
          const out = orig.apply(this, args);
          drawMs += performance.now() - a;
          draws++;
          return out;
        };
        (wrapped as { __perfr?: boolean }).__perfr = true;
        (ctx as unknown as Record<string, unknown>)[name] = wrapped;
      }
    };
    const canvasProto = globalThis.HTMLCanvasElement?.prototype;
    const origGet = canvasProto?.getContext;
    let patched = false;
    const gpuProto = (globalThis as { GPUQueue?: { prototype: { submit: (b: unknown) => unknown } } }).GPUQueue?.prototype;
    const origSubmit = gpuProto?.submit;
    const arm = () => {
      if (patched || stopped) return;
      if (canvasProto && origGet) {
        canvasProto.getContext = function (this: HTMLCanvasElement, type: string, attrs?: unknown) {
          const ctx = origGet.call(this, type, attrs as CanvasRenderingContext2DSettings);
          if (ctx && (type === "webgl" || type === "webgl2" || type === "experimental-webgl")) {
            wrapDraw(ctx as WebGLRenderingContext);
          }
          return ctx;
        } as typeof canvasProto.getContext;
      }
      if (gpuProto && origSubmit) {
        gpuProto.submit = function (this: unknown, buffers: unknown) {
          const a = performance.now();
          const out = origSubmit.call(this, buffers);
          submits++;
          const end = profiler.nowNs();
          profiler.span("webgpu submit", end - (performance.now() - a) * 1e6, end, TID.gpu);
          return out;
        };
      }
      if (!raf && typeof requestAnimationFrame === "function") raf = requestAnimationFrame(flushGpu);
      patched = true;
    };
    const disarm = () => {
      if (canvasProto && origGet) canvasProto.getContext = origGet;
      if (gpuProto && origSubmit) gpuProto.submit = origSubmit;
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      patched = false;
    };
    controls.push({ track: "webgl", arm, disarm });
    if (enabled.webgl) arm();
    stops.push(disarm);
  }

  return {
    set(partial: TrackToggles) {
      for (const id of TRACK_IDS) {
        if (!(id in partial)) continue;
        const next = Boolean(partial[id]);
        const prev = enabled[id];
        enabled[id] = next;
        if (next === prev) continue;
        for (const c of controls) {
          if (c.track !== id) continue;
          if (next) c.arm();
          else c.disarm();
        }
      }
    },
    get: () => ({ ...enabled }),
    support: () => support,
    stop() {
      stopped = true;
      for (const stop of stops) stop();
    },
  };
}
