# perfr-tracing

A [`tracing-subscriber`](https://docs.rs/tracing-subscriber) `Layer` that
sends `tracing` spans to a capturing [perfr](https://github.com/perfr/perfr)
as scopes on the calling thread, `tracing` events as log lines, and the
`tracy.frame_mark` event Bevy emits once a frame as a frame mark
(`perfr_api::frame_mark`), which the viewer draws as a strip of frame times
and as bands behind the app's tracks. `PerfrLayer::frame_mark_on_exit("update")`
marks frames for an app built without that event.

```rust
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(perfr_tracing::PerfrLayer::new())
    .init();
```

In Bevy, one line in `LogPlugin`:

```rust
DefaultPlugins.set(bevy::log::LogPlugin {
    custom_layer: |_| Some(Box::new(perfr_tracing::PerfrLayer::new())),
    ..default()
})
```

with the `trace` cargo feature of `bevy` on (and `debug` for system names).
The guide is `integrations/bevy/README.md` in the perfr repository.

A span's name is formatted once, when the span is created; each enter and
exit is one `perfr_api` call, a few stores while a service is capturing and a
single branch otherwise.
