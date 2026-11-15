# Perfetto UI and the perfr viewer, on the same traces

Opens one file in [ui.perfetto.dev](https://ui.perfetto.dev/) and in the viewer
[perfr.dev](https://perfr.dev/viewer/) embeds, in headless Chromium, and records
how long each takes to draw a timeline and how the frame times look while a
fixed zoom and pan runs.

`results/run.json` is the run from 2026-11-15 08:10–08:53 UTC. Every
perfr run in it reported `perfr 0.1.4 (fd8ba5aaf61c)`. The same file is
copied to `docs/blog/metrics/perfetto-compare.json`. Re-run this directory
to get a new one. Nothing here is a claim about a desktop GPU.

## What is timed

`traces.json` is the set. It is the two commands in the stable Perfetto UI's
example list (`Open Android example`, `Open Chrome example`), the example
files the Perfetto docs still link, the older hosted Chrome JSON example, and
a few files from `test/data` that are a different format or a different size.
The full `test/data` bucket is `catalog.json` (438 objects, sizes from HTTP
`Content-Length`). Those are test fixtures, including crash inputs and heap
dumps. They are listed, not all opened.

Both viewers get the file from disk through the open control, after the page
shell is already up. The clock starts when the file is handed over
(perfr: the file chooser accept; Perfetto: `input.trace_file`) and stops when
the timeline is up:

- perfr: the status line is no longer `Loading …` (that string is
  `symbols-status:` in `window.__perfr_ui`; event count can sit still for a
  frame while a large file is still parsing), and `__perfr_sel` has a
  non-zero event count and a content span, stable across two polls.
  `prims` is recorded and is not required. Counter tracks draw without
  adding to it.
- Perfetto: the sidebar has switched to `CURRENT TRACE` and an overview canvas
  exists. `Duration:` is painted into a canvas, so it is not in the DOM text.

A second clock, `interactMs`, is the first of those plus the wait until one
held `W` actually moves the view. perfr exposes the window in
`__perfr_sel.view`. Perfetto does not, so the check is a hash of a PNG clip of
the timeline, which adds one screenshot to that viewer's number.

The navigation sequence, the same for both, after that:

- `W`, `S`, `A`, `D`, each pressed and held 220 ms, ten or twelve times.
  The hold is there because the viewer samples keys once a frame, and a frame
  on a heavy trace can be longer than a tap.
- Ctrl+wheel, eight steps in and eight out.
- A 240 px pan drag. perfr pans on a left drag. Perfetto's left drag is an
  area selection and its pan is Shift+left drag. Same distance, the gesture
  each UI documents as pan.

Frame times are `requestAnimationFrame` stamps during that sequence. fps is
`(frames-1) / span`. p50, p95 and p99 are the gaps. Headless Chromium has no
display vsync. The gaps include the time this script spends injecting the
next input. Read them as a comparison of the two pages in this harness, not
as a 60 Hz panel.

Memory is the sum of `ps` RSS for the Chrome process and its children,
sampled every 100 ms. `baselineRss` is that sum after the shell is up and
before the file is opened. `peakRss` is the high point from the open through
the gesture. `performance.memory` is also stored. It does not include the
shared Wasm memory, so it is not the trace's cost.

Each file is opened five times. The summary is the median of the runs that
succeeded. A fresh browser is used for every run, with one profile directory
so the viewer assets stay cached after the first navigation. Page-load and
the Wasm download are not in the clock.

## How the machine is set up

Chromium is the system `google-chrome`, started with:

```
--headless=new
--use-gl=angle
--use-angle=swiftshader
--enable-unsafe-swiftshader
--ignore-gpu-blocklist
--enable-precise-memory-info
--window-size=1600,1000
```

SwiftShader is software GL. perfr's build string in the results reports
`WebGL`, which is the viewer's default (WebGPU is `?webgpu`, and this run
does not set it). The Perfetto UI version is the `vNN-commit` string in the
sidebar, stable channel.

perfr's keys do not move the view when sent with Puppeteer's `keyboard.press`.
The page receives the keydown. The Wasm viewer does not act on it. A CDP
`rawKeyDown` held across a frame does. Both viewers are driven that way.
Ctrl+wheel uses Puppeteer's mouse wheel, which does move perfr.

Opening `http://127.0.0.1` from `https://ui.perfetto.dev` is refused by
Chrome's local-network check, so the traces are not fetched the way "Open
example" does. The bytes are the same files, downloaded once and opened
locally. Download time is not in the numbers.

## Run

```sh
cd tools/bench/perfetto-compare
npm install
node bench.mjs --runs 5 --out results/run.json
node bench.mjs --runs 1 --only chrome-scroll
```

`--traces` is the cache directory (default `/tmp/traces-bench`). A file is
downloaded again when its size does not match `traces.json`.

`catalog.json` is every `test/data` object whose `.sha256` sidecar is in
google/perfetto at `6fffac7ff1049fe42dd61c5b25141c237af22b38`. The URL is
`https://storage.googleapis.com/perfetto/test_data/<basename>-<sha256>`.
Rebuild it with the sha256 files checked out:

```sh
python3 catalog.py /path/to/perfetto
```
