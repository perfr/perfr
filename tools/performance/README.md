# Measuring the native live client

`live_client.py` drives the existing benchmark producer and launches a real
native window, using `perfr-client`'s `live_bench` example. This measures the
shared client/viewer receiving packed WebSocket events, with ordinary GPU
rendering and vsync. It does not open the Self pane.

Use a **dedicated service**: the runner stops its benchmark and clears its
capture at the beginning of each trial. Its default port is deliberately
different from the normal service/profiler port. A service that is still
capturing ten seconds after its benchmark was stopped has a capture of its
own, and the runner stops there, before clearing anything.

```sh
cargo build --release -p perfr-client --features native --example live_bench
target/release/perfr serve --host 127.0.0.1 --port 44863
# In another terminal, with a working desktop display:
python3 tools/performance/live_client.py target/release/examples/live_bench /tmp/live-run --seconds 35
python3 tools/performance/summarize_live.py /tmp/live-run
```

The service binary can be built with the repository's ordinary perfr build.
Keep that binary, the viewport, display scale, renderer, and producer settings
identical across variants. The workload is 16 threads, nesting depths 8–16,
and a requested million events per second (`--rate` overrides the rate).
The producer uses its fixed seed. Avoid compiling or running other benchmarks
during measurements. Keep the desktop window unobscured and do not interact
with it. On a hybrid CPU, repeat trials using the same performance cores,
for example `taskset -c 4,6 python3 ...` on the machine used in the article;
choose suitable CPU IDs on your own machine. Alternate the variant order.

For a baseline predating the harness, apply just the example and what it
uses of `perfr-viewer` to that revision: `native_options`, the `eframe`
re-export, and `PerfrLiveApp::profile_readout` and `gpu_times_us`. Build and
copy each executable before making the rendering changes. The intermediate
variant only removes row padding in `TimelineGpu::upload_columns`.

The output preserves:

- `frames.json`: for every frame its update duration, its start-to-start
  period and `gpu_prepare_us`, plus once-per-second app counters. Update
  duration excludes egui tessellation and GPU submission after `App::update`;
  `gpu_prepare_us` is CPU elapsed time in the timeline callback, not GPU
  execution time. The callback runs after `App::update`, so a frame's row
  carries the reading of the frame before it.
- `process.json`: Linux `/proc` process CPU time and RSS, benchmark achieved
  rate, and service status, polled once a second. CPU cores means CPU seconds
  divided by elapsed seconds across all client threads, not percent of the
  whole machine. RSS includes retained event storage and driver allocations.
  A poll the service did not answer within two seconds keeps its process half
  and is counted in `command.json` as `missed_polls`; the trial goes on.
- `command.json`, `client.log`, and `pid`: settings, inherited CPU affinity,
  exit status, diagnostics, and the PID to attach the profiler to.

The producer is stopped when the runner ends, however it ends. The client
writes `frames.json` when its window closes after `--seconds`, so a trial
that is interrupted has none.

The summary excludes the first 15 seconds from frame/counter readings and
16 seconds from process readings (the latter clock starts before the window).
Its quantiles select the nearest-ranked observed value. The `gpu_prepare`
figures are over every frame after warmup, and `gpu_prepare_readings` is how
many that was. A run recorded before the harness kept the reading per frame
has it once a second only, some twenty readings to a trial: a mean over
those moves with a single slow frame and is not comparable with a per-frame
one. A run with too little after warmup to summarize is named on standard
error and left out, and the exit status is then 1. Queue counters are
snapshots of the viewer's pending-message queue, not an end-to-end latency or
loss detector. These short trials accumulate events; they are not a memory
steady-state or maximum-throughput benchmark.

For attribution, start a second perfr service on 44766, attach to the PID,
and capture after warmup. The article used 1 kHz sampling and Frida hooks on
`TimelinePayload::from_index`, `RasterizedFrame::to_rgba8_placed` and
`pack_rgba_aligned` before, then `from_index` and `to_rgba8_placed` for the retained padding-only fix.
The rejected atlas experiment instead hooked `from_index` and `to_rgba8_atlas`.
The render/viewer/protocol/event packages were built with release debug info
(`--config 'profile.release.package.perfr-viewer.debug=true'`, and likewise
for the other three packages). Scheduler recording was disabled. Save the
bundle and stream before starting another capture, then author the story
from its actual thread IDs, scopes, and hotspot readings. Profiled runs must
remain separate from unprofiled timing comparisons.
