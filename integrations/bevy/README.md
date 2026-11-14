# perfr in Bevy

Three levels, each on top of the previous one. Written against Bevy `main`
(0.20-dev, November 2026); what it relies on -- the `trace` cargo feature,
the `tracing` spans behind it, and `LogPlugin::custom_layer` -- has been in
the engine since 0.14.

## 0. Nothing to add

Sampling, the scheduler and the Functions tab's dynamic hooks work on any
Linux build of a Bevy app. Build release with line tables so samples and
disassembly resolve to source lines:

```toml
[profile.release]
debug = "line-tables-only"
```

or, without touching the manifest,
`CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo build --release`. Point
`perfr` at the process and press Record. Bevy names its threads
(`Compute Task Pool (n)`, `Async Compute Task Pool (n)`, `IO Task Pool (n)`),
so the thread rows read as they do in the engine's own docs.

Every system is its own function in the binary, so the Functions tab can
hook one with no code change. Search for the system's name; the row to hook
is the monomorphized

```
<bevy_ecs::system::function_system::FunctionSystem<…, my_game::simulate_cars> as bevy_ecs::system::system::System>::run_unsafe
```

One level up, `bevy_ecs::schedule::executor::__rust_begin_short_backtrace::run_unsafe`
is the single function every system call passes through: one hook times
them all, under one name. The next level tells them apart.

## 1. The layer: Bevy's own spans reach perfr

Behind its `trace` cargo feature Bevy puts a `tracing` span around every
system run, every schedule, the render graph and its passes, parallel query
iteration, plugin builds, and more. [`perfr-tracing`](../../crates/perfr-tracing)
is a `tracing-subscriber` layer that turns each of them into a perfr scope
on the thread that ran it. Bevy's `LogPlugin` has a slot for exactly one
such layer:

```toml
[dependencies]
bevy = { version = "0.20", features = ["trace", "debug"] }
perfr-tracing = { path = "../perfr/crates/perfr-tracing" }   # or the git URL
```

```rust
use bevy::{
    log::{BoxedLayer, LogPlugin},
    prelude::*,
};

fn perfr_layer(_app: &mut App) -> Option<BoxedLayer> {
    // `update` is the span Bevy puts around each App::update, once a frame
    // on the main thread; its exit is the frame mark.
    Some(Box::new(
        perfr_tracing::PerfrLayer::new().frame_mark_on_exit("update"),
    ))
}

App::new()
    .add_plugins(DefaultPlugins.set(LogPlugin {
        custom_layer: perfr_layer,
        ..default()
    }))
    .run();
```

`PerfrLayer::new()` calls `perfr_api::init()`, which creates the shared-memory
segment a `perfr` service drains once Record targets the process. Until then
every span costs one branch. From then on:

| Bevy | perfr |
|---|---|
| each run of a system (the `system` span) | a scope on the thread that ran it, named by the system's path: `bevy_render::view::window::prepare_windows` |
| a system's deferred commands; its run conditions | `system_commands: <path>`; `check_conditions: <path>` and the condition functions by their own path |
| a schedule | `schedule: Update`, `schedule: Render`, `schedule: ExtractSchedule`, … |
| `App::update`, the main app, each sub app | `update`, `main app`, `sub app: RenderApp`, `sub app: RenderExtractApp` |
| render work: the render schedule, passes, command buffers, present | `main_render_schedule`, `main_opaque_pass_3d`, `finish_command_buffer system=…`, `queue_submit count=…`, `present_frames`, … |
| parallel query iteration | `par_for_each query=… filter=…` on every worker thread it ran on |
| `transform propagation worker`, `par_traversal_mark_dirty`, … | scopes, as named |
| `plugin build plugin=…`, `plugin finish`, `plugin cleanup` | once, at startup |
| `info!`, `warn!`, `error!`, `debug!`, `trace!`, and `log::` records from dependencies | a log line at the same level on the thread that logged it: `target: message key=value`; what `LogPlugin::filter` or `RUST_LOG` drops never reaches perfr |
| the exit of the `update` span, once a frame | a frame mark, with `frame_mark_on_exit("update")` |
| the `tracy.frame_mark` event (only with the `trace_tracy` feature) | a frame mark, and from the first one on the `update` span marks none |
| thread names | come from the OS; Bevy names its task-pool threads |

A span's name is its `tracing` name, then its fields: a field called `name`
follows after a colon with no key, every other field as ` key=value`. A span
named `system` is reduced to its `name` field, since that is what everyone
looks for. Names are formatted once, when the span is created; a span that
is created every frame (`queue_submit count=…`) is formatted every frame.

Things to know:

- **`trace` is required.** Without it `info_span!` compiles to nothing and the
  layer sees no spans. `debug` gives systems their real names; without it
  every system is `<Enable the debug feature to see the name>`. Spans are at
  `INFO`, so a filter that drops `INFO` (`RUST_LOG=warn`) drops the spans
  too; filter by target instead.
- **Frames.** A frame mark (`perfr_api::frame_mark`) is where one frame
  ends and the next begins. The viewer draws the frames as a strip of frame
  times under the ruler, which always shows the whole capture and where a
  click on a bar shows that frame, and as bands behind the app's own
  tracks, every other frame a little darker. Bevy says where its frames end only with
  `trace_tracy`, after the render sub app has presented. Without that
  feature `frame_mark_on_exit("update")` marks the end of each `App::update`
  on the main thread instead; with it, Bevy's own mark is used and `update`
  stops marking, so a frame is never cut in two.
- **Logs.** A line is filed under the scopes open on its thread when it was
  written, so clicking a system's scope in the viewer lists what that
  system logged, and the Logs window lists the lines in the visible range.
  Lines are formatted only while a service is capturing.
- **Schedules created before `LogPlugin` have no span.** `tracing` disables a
  span for good when it is created before a subscriber exists, and
  `Schedule::new` creates the span. `Main`, `PostUpdate` and `Last` are set up
  by `App::new()`, before any plugin, so they never show -- not in perfr,
  not in Tracy. Their systems do: a system recreates its span when its
  schedule first initializes it. The per-frame bracket on the main thread is
  the `update` span, which is why the frame mark keys on it.
- **Volume.** `bevy_city` (44,800 entities, 1080p, below) emits about 3,000
  scopes a frame, some 900 of them system runs, 300,000 ring records a
  second at 30 fps; the ring kept every one. It drops past about a million
  records a second (the viewer shows the gap), so Bevy's `detailed_trace`
  feature, which adds per-entity and per-archetype spans, will overrun it:
  turn it on to look at one system, not to run with.
- **Tracy and Chrome** (`trace_tracy`, `trace_chrome`) can stay on beside the
  layer; they are other layers on the same subscriber.
- **No GPU spans yet.** `RenderDiagnosticsPlugin` reads pass timings off the
  GPU's clock, not `CLOCK_MONOTONIC`, so there is nothing to hand
  `perfr_api::span_async` without a clock correlation.

## 2. Your code: the perfr API directly

With the layer in place `perfr_api` is already initialized, so a system can
add what Bevy does not instrument:

```rust
use bevy::prelude::*;

fn simulate_cars(mut cars: Query<(&mut Car, &mut Transform)>, time: Res<Time>) {
    let _integrate = perfr_api::scope("cars: integrate");   // RAII, nests under the system's scope
    let mut moving = 0;
    for (mut car, mut transform) in &mut cars { /* ... */ moving += 1; }
    perfr_api::value("cars moving", moving as f64);        // a lane to graph
}

// A job handed to another thread: the handle is the identity, link joins them.
let queued = perfr_api::instant("queue bake");
AsyncComputeTaskPool::get().spawn(async move {
    let _job = perfr_api::scope_async("bake");
    perfr_api::link(queued, _job.handle());
    // ...
}).detach();
```

Without the layer, call `perfr_api::init()` once at startup. `perfr_api::log_*`
exists, but `info!` and friends already arrive through the layer. The
crate's docs list every call; `perfr_api::span` and `span_async` take
timestamps you captured yourself, on `perfr_api::now_ns()`'s clock.

## The `bevy_city` example

The large-scene stress test in the Bevy tree,
`examples/large_scenes/bevy_city`, has a `perfr` cargo feature that does
all of the above: the layer with the frame mark and the logs, plus two
lanes, `frame ms` and `entities`, from a one-system `perfr_values`. It expects this repository
checked out beside Bevy's (`../perfr`):

```
cargo run -p bevy_city --release --features perfr
```

The default features of `bevy` need `libasound2-dev` and `libudev-dev` on
Debian-family Linux (Bevy's `docs/linux_dependencies.md`).

## Checking it works

The service log says `manual instrumentation: opened segment of pid <n>`
when Record starts and `manual instrumentation: <k> segment(s), <m> events`
when it stops. `GET /api/functions/stats?pid=<n>&q=bevy_` is one row per
system with count, total, average, min, max, and `q=frame` has the row of
kind `frame` with the frame times. `GET /api/logs?pid=<n>` lists Bevy's log
lines. In the viewer the strip under the ruler has a bar per frame, and the
main thread carries `update` with `main app` and the schedules nested
inside, each system a band under its schedule, over a ground that is a
little darker every other frame; the render
thread carries `sub app: RenderApp`, `schedule: Render` and the passes; the
task-pool threads carry systems the executor handed them and the
`par_for_each` slices; `frame ms` and `entities` draw as lanes.
