<!--
Copyright (c) 2026 The perfr Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# In-page profiling

A JS, TypeScript or WASM app running in the browser has no connection to the
native perfr service, and the browser will not let the page sample CPU stacks
or read context switches. What the page can do is record the scopes it cares
about, plus the timings the browser already exposes, into the same
`.perfr.stream` the viewer opens with `?capture=`.

The package is `@perfr/web` (`web/perfr` in this repo). The encoder is the
`perfr-web` crate, compiled to WASM, and it calls `perfr-live-protocol` — the
framing the live viewer already decodes. A demo page lives at `web/demo`.

## What this is not

- No CPU sampling. There is no call stack under a scope unless you recorded the scopes yourself.
- No context switches, thread states, or scheduler lanes. A worker is a separate track only because the page named it.
- No GPU-side time. WebGL and WebGPU tracks are the time spent in the JS draw or `submit` call.
- Not a replacement for a native capture. A process outside the page is still profiled with `perfr` and `perfr.h`.

## JavaScript and TypeScript

```ts
import { perfr } from "@perfr/web";

perfr.scope("frame", () => {
  perfr.scope("update", () => step());
  perfr.scope("render", () => draw());
  perfr.value("entities", world.count); // perfr.plot is the same call
});

const load = perfr.beginAsync("assets");
await fetchLevel();
perfr.endAsync(load);

perfr.mark("checkpoint");
perfr.frame(); // a zero-duration marker named "frame"
perfr.nameThread("main");
```

`begin` / `end` are the same scopes without a callback. A zero handle means
the call was dropped. `@perfr.decorate("physics")` wraps a method, as a
stage-3 decorator or as the older `experimentalDecorators` form.

The first call loads the WASM encoder. Until it is ready, events are queued
with the timestamps they actually had and then replayed in order, so nesting
stays intact. The ring defaults to 200,000 events. Past that, the oldest
event is dropped.

```ts
await perfr.start({ browser: true }); // also the default
perfr.stop();                          // keeps the ring; scopes are dropped until start
await perfr.start();                   // records again into the same ring
const bytes = await perfr.exportStream(); // a .perfr.stream
await perfr.download("capture.perfr.stream");
```

After `stop`, a `scope` call does not restart recording on its own; only
`start` does. A viewer attached to the page sees a resumed capture as a fresh
stream of the whole ring, not as events after an end marker.

`browser: false` records only your scopes. A partial object turns individual
built-in tracks off: `perfr.start({ browser: { gc: false, loaf: false } })`.
`perfr.tracks.support()` reports what this browser actually exposes.
`perfr.tracks.set({ frames: false })` disconnects a track later.

## Embedding the viewer

Host the existing viewer (`crates/perfr-live-viewer/viewer-dist`) and point
at its `index.html`:

```ts
perfr.attachViewer({
  viewerUrl: "/viewer/index.html",
  overlay: true,   // default. Or pass container: element, or popup: true
  hotkey: "F8",
});
```

The viewer is opened with `?embed=1`. `embed.js` on that page talks to your
app over a `MessageChannel` and feeds the bytes into the viewer's existing
WebSocket ingest, so the timeline, the track list and Share are the viewer's.
When the page is cross-origin isolated (`Cross-Origin-Opener-Policy:
same-origin` and `Cross-Origin-Embedder-Policy: require-corp`) and the viewer
is served from the page's own origin, encoded bytes travel in a
`SharedArrayBuffer`. Otherwise the channel copies them.
Either way the viewer is the same build that opens `?capture=`.

`?capture=` on the viewer URL is unchanged: embed mode does not install
itself when a capture parameter is present.

Share uses the viewer's Share dialog. The bytes it posts are a snapshot of
the ring (hello, names, the events still in the ring, `CaptureFinished`).
The viewer's Save item "Stream for a web page" downloads that same stream.
The `.perfr.zip` export is the native service's bundle; this path does not
produce one.

Stop in the viewer stops the page recorder and its browser tracks. Clear
drops the ring. Record starts both again.

## Workers

```ts
// page
const worker = new Worker(new URL("./sim.js", import.meta.url), { type: "module" });
perfr.attachWorker(worker, { name: "sim" });

// sim.js
import { installWorker } from "@perfr/web/worker";
const perfr = installWorker({ name: "sim" });
perfr.scope("step", () => work());
```

The worker's clock is shifted onto the page's `performance.timeOrigin`.
`postMessage` in both directions is marked on a "Worker messages" track.
Other workers are not observed.

## Built-in tracks

Each track is a named thread under the page (pid `0xB10B0001`, which is not
an operating-system pid). Values such as `fps` and `CLS` are lanes on that
thread. A track that the browser does not implement stays empty;
`perfr.tracks.support()` says why.

| Track | What is recorded | Where it exists |
|---|---|---|
| Frames | `requestAnimationFrame` deltas, a `jank` marker when a frame is more than 1.5× the recent median, `fps`, and a refresh-rate **estimate** from that median | Anywhere `requestAnimationFrame` exists. Browsers do not tell the page the display's real refresh rate. |
| GC | `PerformanceObserver` type `gc` when present; `performance.memory` (`usedJSHeapSize`) when present; `measureUserAgentSpecificMemory` every 20s when the function exists | `gc` and `performance.memory` are Chromium. `measureUserAgentSpecificMemory` also needs cross-origin isolation and is allowed to reject. |
| Long tasks | Long Tasks (`longtask`), tasks over ~50ms | Chromium. Not Firefox or Safari. |
| Long animation frames | `long-animation-frame`, plus `blockingDuration` when the entry has it | Chromium 123+. Not Firefox or Safari. |
| Input | Event Timing entries. Interactions (those with `interactionId`) update `INP (approx)`, the 98th percentile of interaction durations **in this session** | Chromium implements `interactionId`. Firefox has partial event timing. Safari does not list the `event` entry type. This number is not Chrome's field INP for a whole visit. |
| Layout shifts | Layout Instability entries that did not follow recent input, and a running `CLS` | Chromium. Other browsers only if they list `layout-shift`. |
| Paint | `paint` marks (first paint, first contentful paint) and LCP candidates | Paint timing is widely available. LCP is Chromium and Safari, not Firefox. |
| Resources | Resource Timing for scripts, images, css and the other initiator types | Widely available. |
| Fetch | `fetch()` calls after the track starts, and XHR resource entries. A wrapped `fetch()` is not copied a second time from Resource Timing | `fetch` is widely available. |
| Navigation | Redirect, DNS, connect, TLS, request, response, DOMContentLoaded, load | Navigation Timing Level 2, one entry for this document. |
| User Timing | `performance.mark` / `measure`, including entries already buffered | Widely available. |
| Visibility | `visibilitychange`, `pagehide`, `pageshow` | Widely available. |
| Worker messages | Send and receive marks for workers attached as above | Only those workers. |
| WebGL / WebGPU | JS time and a count around WebGL draw calls, and around `GPUQueue.submit` | Draw wrappers need WebGL. `submit` is patched only when `GPUQueue` exists (Chromium, and Safari with WebGPU on). This is not time on the GPU. |

## Rust (wasm-bindgen)

Depend on the `perfr-web` crate and call it from the app's own WASM module.
`examples/web-wasm` is a complete crate.

```rust
perfr_web::scope("frame", || {
    perfr_web::value("dt", dt_ms);
});
let bytes = perfr_web::flush(); // new .perfr.stream frames
```

`flush` is empty when nothing changed. `snapshot` is the whole ring, which
is what Share posts. `stop` keeps the ring and `resume` records again.
Thread ids for the built-in tracks are `perfr_web::tid`.
The page pid is `perfr_web::PAGE_PID` (`0xB10B_0001`).

## C and Emscripten

`crates/perfr-web/include/perfr_web.h` is the C ABI of that crate (`begin`
takes a pointer and a length, a zero handle means "not recorded"). Link
`libperfr_web`. The pointer from `perfr_web_flush` / `perfr_web_snapshot` is
owned by the library and dies on the next call.

Emscripten builds that do not link the staticlib include
`examples/web-emscripten/perfr_web_js.h` instead. Those functions are EM_JS
and call `Module.perfr`, which the page sets to the `@perfr/web` profiler
before the module runs. `examples/web-emscripten/build.sh` runs `emcc` when
it is installed and otherwise does nothing: the ABI is tested from the crate.

`wasm32-unknown-emscripten` is a separate Rust target. This repo does not
require it.

## Stream format

Frames are the live viewer's: magic `OLIV`, version 1, 32-byte `LiveEvent`s.
Sync scopes are `API_SCOPE` with a nesting depth. Async spans are `API_TRACK`,
given the lowest free depth on that thread so overlapping loads do not share
a lane. Values are `VALUE` (the `f32` bits in `duration_ns`). Marks and
`frame()` are zero-duration scopes. Names, the process name and thread names
are the usual intern and name frames.

Timestamps are nanoseconds from the page's `performance.timeOrigin`
(`performance.now() * 1e6`), not wall-clock Unix time. That stays exact in a
JS number for far longer than a capture. A worker adds the gap between its
`timeOrigin` and the page's.

## Demo

From the repo root, serve the tree and open `web/demo/`:

```sh
python3 -m http.server 44767
```

The page is a canvas animation with a worker, a deliberate stall button, and
the viewer in an overlay. `web/demo/tour.html` is the API walked through one
button at a time: nested scopes, async spans, values and marks, a worker
burst, a long task, a layout shift, stop, start and clear, and an export that
is decoded on the page so the frame and event counts can be read next to the
viewer. For the SharedArrayBuffer path, serve with
`Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy:
require-corp`. The demo works without those headers; the channel is the
fallback.

To rebuild the encoder after changing `perfr-web`:

```sh
web/perfr/build_wasm.sh
cd web/perfr && npm run build
```
