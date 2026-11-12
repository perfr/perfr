# perfr-web

In-memory ring of live events, encoded as a `.perfr.stream` with
`perfr-live-protocol`. This is the encoder behind `@perfr/web`.

Rust and the C ABI (`include/perfr_web.h`) share one global session.
`cargo build -p perfr-web --target wasm32-unknown-unknown --features bindgen`
produces the package's WASM module (`web/perfr/build_wasm.sh`).

A browser page still cannot sample CPU stacks. The events here are the ones
the caller records. See [docs/manual/web.md](../../docs/manual/web.md).
