# Live-viewer render optimizations

CPU-prepare numbers from `cargo bench -p perfr-live-render --bench rasterize`
on this agent (Linux cloud VM, **no discrete GPU**). GPU-upload and on-screen
FPS were not measured. Re-run on the machine you care about; do not treat
these as a substitute.

```
cargo bench -p perfr-live-render --bench rasterize
```

Criterion reports median of 100 samples after a 0.5 s warmup / 2 s
measurement window (this run).

## Measured medians (CPU prepare)

| Bench | Median | vs counterpart | Notes |
|---|---|---|---|
| `collect_y_cull/no_cull` | **99.8 µs** | — | 200 lanes × 64 events, full stack |
| `collect_y_cull/y_cull_small_view` | **2.61 µs** | **38× faster** | same index, visible Y = 80 px + pad |
| `choose_lod_sample/density` | 245 ns | — | scores all lanes by `len()`; the first-8 sample it replaced (removed since) took 118 ns and missed the dense row |
| `rasterize_vs_pixels/pixel_columns/1280` | **5.86 ms** | — | 1e6 scopes, no-regress check |
| `rasterize_vs_pixels/pixel_columns/1920` | **7.70 ms** | — | same |

Not measured on this VM: GPU `write_buffer` / `write_texture` vs
`create_buffer_init` (unit-tested as `upload_mode_skips_idle_and_flags_hover`);
egui idle-chrome widget skip; on-screen FPS / `fps_sweep`.

## What landed

1. **Vertical Y-cull.** `YCull::from_clip` (content top + clip + 48 px pad)
   is passed into `collect_instances_layout_opts`, `rasterize_pixel_layout`,
   value-graph paint, and visible counts. `GpuDirtyKey` also stores dest
   rect, explicit cull `[y0,y1]`, and scale so resize / inspector / compact
   / collapse / follow recollect. Instanced `shift` is disabled while
   Y-cull is on (same time window is not the same visible set). VALUE
   graphs use `value_lanes_in_view` (egui polylines, never SDF). Header
   rows use their own clip, not the body-leaf window.
2. **Instanced early-out (removed).** After `first_ending_after(t0)`, if
   that scope covered `[t0, t1)` the lane's walk stopped early, unless a
   later scope still started before `t1`. That is exactly when the walk
   stops by itself on its next step, so the bench measured 130.3 ns
   against 130.2 ns and the two listings were always the same. The check
   is gone; `CollectOpts::early_out` is still there, and does nothing,
   until the viewer stops setting it.
3. **Dirty-flag upload.** `GpuDirtyKey` covers `(t0,t1,width,scroll,view_h,
   dest,cull_y0/y1,scale,layout_gen,lod,events,selected,hover,search)`.
   `UploadMode::Skip` emits `TimelinePayload::Keep` (uniforms + `u_time`
   only). `Flags` re-applies highlights and `write_buffer`s. GPU instance
   buffers / column textures grow in place (`VERTEX|COPY_DST`,
   `write_texture` when size matches). Follow still moves `t0/t1` so it
   stays dirty.
4. **End-time index.** Verified: non-overlapping start-sorted lanes have
   sorted `end_ns`, so `partition_point` is a real binary search. Comment +
   `ends_are_sorted` / overlapping-ends tests. No dual index (would cost
   append-mostly insert). A lane that loses the order (an imported
   trace's instant inside a slice) keeps, per position, the latest end so
   far, and searches that: 8 bytes an event on those lanes only.
5. **LOD sampling.** `choose_lod` samples the densest non-VALUE lanes by
   `Lane::len()` (O(lanes), not O(scopes)) plus the cursor/hover hint.
   `INSTANCE_MIN_PX = 4.0`. VALUE still skipped
   (`value_bits_do_not_force_instanced_lod`). Density is a few hundred ns
   slower than the first-8 sample it replaced and actually sees a busy row
   that one missed.
6. **Idle chrome.** `skip_idle_chrome` still skips `apply_perfr_visuals`
   on the 100 ms timer wake when not live/follow/dragging. Header rows
   stay full widgets (title-band names, chevrons, hide chips, drag
   handles) so idle skip cannot park a thread name in the middle of the
   scope stack.
7. **Multi-threading.** Native lane `par` always splits: rayon when
   `--features parallel` (viewer native Cargo.toml enables this), else
   `std::thread::scope` chunks so a build without rayon still emits
   `render-wN` worker tids. WASM
   uses **wasm-bindgen-rayon** + `SharedArrayBuffer` when the eframe
   bootstrap can init the pool (`initThreadPool` then
   `markWasmPoolReady`). SAB missing (old browser / missing
   COOP/COEP/CORP) stays sequential — no crash, no invert-default.
   Self-profile: parent `PrimitiveListing` plus per-worker
   `CollectLane` / `RasterLane` on distinct `render-wN` tids when the
   pool is up; `n_prims` / `n_lanes` VALUE samples on stats.

   Headers required on every served response (HTML, js, wasm, worker
   `snippets/`): `Cross-Origin-Opener-Policy: same-origin`,
   `Cross-Origin-Embedder-Policy: require-corp`,
   `Cross-Origin-Resource-Policy: same-origin`. The axum serve path
   (`//:live` / perfr-live-server) applies these as a router layer.

   Rebuild the checked-in pack (needs rustup nightly + rust-src):

   ```
   ./crates/perfr-live-viewer/build_wasm.sh
   ```

   That runs `nightly-2025-11-15` with `-Z build-std=panic_abort,std`,
   `+atomics,+bulk-memory,+mutable-globals`, shared/import memory, and
   `--features wasm-threads`. Override the pin with
   `PERFR_WASM_NIGHTLY`. Plain `cargo build --target wasm32-unknown-unknown`
   (CI) stays sequential so it does not need a rebuilt std.

   Checked-in `viewer-dist/` was rebuilt here with that script (includes
   `initThreadPool` / `markWasmPoolReady` and
   `snippets/wasm-bindgen-rayon-*/src/workerHelpers.no-bundler.js`).
   eframe's TypeMap needs `Send+Sync`; `TimelineGpuSlot` is an unsafe
   wrapper so GPU objects stay on the UI thread while rayon workers only
   run CPU collect/raster.

   This agent ran native `cargo test` for the live-viewer crates, a
   sequential wasm32 compile (CI path), and `build_wasm.sh`. It could
   not confirm SharedArrayBuffer + `render-wN` in a browser.
8. **Shader animation.** `uni.time` + selected pulse (~1.2 s, small
   radius/brightness) on `FLAG_SELECTED` (`#0080FF`, lift, glow; the
   drop shadow was removed, see `shaders/instance.wgsl`). Idle + selected
   writes `u_time` every frame without rebuilding instances.

9. **Longest event in a range.** Each lane keeps the longest span of
   every 64 consecutive events. The column raster (which shows the
   longest event under a column) and `choose_lod` (which asks whether any
   visible event is wide) skip whole blocks instead of visiting every
   event in view. `zoomed_out/*` in the bench, 1e6 scopes, on the machine
   that made the change (32 cores, shared): `choose_lod` 4.10 ms → 28 µs
   over 48 lanes; one lane rastered at 256 px 3.65 ms → 88 µs, at 1920 px
   6.17 ms → 1.31 ms. `overlapping_lane/*` (item 4's latest-end search,
   a 500-scope window at the end of 1e6): listing 2.10 ms → 19 µs,
   raster 2.21 ms → 21 µs.

Self-profile names: `PrimitiveListing` (parent of Y-cull + early-out +
collect), `YCull`, `EarlyOut`, `CollectLane` / `RasterLane` on
`render-w0`…, `n_prims` / `n_lanes`, `Upload` (plus existing
`CollectInstances` / `Rasterize` / `PaintCallback`).
