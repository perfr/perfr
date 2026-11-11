// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Render/prepare cost vs number of scopes **and** vs viewport pixels.
//!
//! The pixel-column path must stay close to O(width log n). The naive path is
//! O(scopes) and is included so a regression is visible in the HTML report.
//! Extra groups measure Y-cull, the LOD decision, a fully zoomed-out view
//! and a lane with overlap (CPU prepare only).

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use perfr_live_event::{kind, LiveEvent};
use perfr_live_render::{
    choose_lod, choose_lod_first8, collect_instances_layout_opts, generate_nested_scopes,
    stacked_layout, CollectOpts, TrackIndex, YCull, INSTANCE_MIN_PX,
};

fn build_index(n: usize) -> TrackIndex {
    let mut idx = TrackIndex::default();
    idx.extend(generate_nested_scopes(n, 8, 6, 0, 1_000_000));
    idx
}

fn tall_stack(threads: u32, per: usize) -> TrackIndex {
    let mut idx = TrackIndex::default();
    for t in 0..threads {
        for i in 0..per {
            idx.insert(LiveEvent {
                start_ns: (i as u64) * 100,
                duration_ns: 80,
                tid: 100 + t,
                pid: 1,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                _pad: 0,
                name_id: t * 1000 + i as u32,
            });
        }
    }
    idx
}

fn vs_scopes(c: &mut Criterion) {
    let width = 1920usize;
    let t0 = 0u64;
    let t1 = 1_000_000u64;
    let mut group = c.benchmark_group("rasterize_vs_scopes");
    for &n in &[10_000usize, 100_000, 1_000_000] {
        let idx = build_index(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("pixel_columns", n), &n, |b, _| {
            b.iter(|| black_box(idx.rasterize_pixel(t0, t1, width, None)));
        });
        group.bench_with_input(BenchmarkId::new("naive_quads", n), &n, |b, _| {
            b.iter(|| black_box(idx.rasterize_naive(t0, t1, width, None)));
        });
    }
    group.finish();
}

fn vs_pixels(c: &mut Criterion) {
    let n = 1_000_000usize;
    let idx = build_index(n);
    let t0 = 0u64;
    let t1 = 1_000_000u64;
    let mut group = c.benchmark_group("rasterize_vs_pixels");
    for &width in &[64usize, 256, 1024, 1280, 1920, 4096] {
        group.throughput(Throughput::Elements(width as u64));
        group.bench_with_input(BenchmarkId::new("pixel_columns", width), &width, |b, &w| {
            b.iter(|| black_box(idx.rasterize_pixel(t0, t1, w, None)));
        });
        group.bench_with_input(BenchmarkId::new("naive_quads", width), &width, |b, &w| {
            b.iter(|| black_box(idx.rasterize_naive(t0, t1, w, None)));
        });
    }
    group.finish();
}

/// Tall stack, small viewport: Y-cull vs walking every lane.
fn collect_y_cull(c: &mut Criterion) {
    let idx = tall_stack(200, 64);
    let keys: Vec<_> = idx.lanes().map(|(k, _)| k).collect();
    let layout = stacked_layout(&keys, 0.0);
    let t0 = 0u64;
    let t1 = 2_000u64;
    let width = 1280.0;
    let mut group = c.benchmark_group("collect_y_cull");
    group.bench_function("no_cull", |b| {
        b.iter(|| {
            black_box(collect_instances_layout_opts(
                &idx,
                t0,
                t1,
                width,
                &layout,
                None,
                CollectOpts::default(),
            ))
        });
    });
    group.bench_function("y_cull_small_view", |b| {
        b.iter(|| {
            black_box(collect_instances_layout_opts(
                &idx,
                t0,
                t1,
                width,
                &layout,
                None,
                CollectOpts { y_cull: Some(YCull::new(0.0, 80.0)), ..CollectOpts::default() },
            ))
        });
    });
    group.finish();
}

fn choose_lod_sample(c: &mut Criterion) {
    let mut idx = TrackIndex::default();
    for tid in 1..=8u32 {
        idx.insert(LiveEvent {
            start_ns: 0,
            duration_ns: 8,
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: tid,
        });
    }
    idx.insert(LiveEvent {
        start_ns: 0,
        duration_ns: 100_000,
        tid: 99,
        pid: 1,
        kind: kind::API_SCOPE,
        depth: 0,
        extra: 0,
        _pad: 0,
        name_id: 99,
    });
    let mut group = c.benchmark_group("choose_lod_sample");
    group.bench_function("first8", |b| {
        b.iter(|| black_box(choose_lod_first8(&idx, 0, 100_000, 100, INSTANCE_MIN_PX)));
    });
    group.bench_function("density", |b| {
        b.iter(|| black_box(choose_lod(&idx, 0, 100_000, 100, INSTANCE_MIN_PX)));
    });
    group.finish();
}

/// One lane of `n` back-to-back scopes, 10 ns apart.
fn dense_lane(n: usize) -> TrackIndex {
    let mut idx = TrackIndex::default();
    for i in 0..n {
        idx.insert(LiveEvent {
            start_ns: i as u64 * 10,
            duration_ns: 8,
            tid: 1,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: (i % 97) as u32,
        });
    }
    idx
}

/// A big capture fully zoomed out: every scope is far under a pixel, so the
/// verdict is `PixelColumns` and no lane can answer early, and each pixel
/// column of the raster covers hundreds or thousands of scopes.
fn zoomed_out(c: &mut Criterion) {
    let n = 1_000_000usize;
    let many = build_index(n);
    let one = dense_lane(n);
    let one_t1 = n as u64 * 10;
    assert_eq!(
        choose_lod(&many, 0, 1_000_000, 1920, INSTANCE_MIN_PX),
        perfr_live_render::TimelineLod::PixelColumns
    );
    assert_eq!(
        choose_lod(&one, 0, one_t1, 1920, INSTANCE_MIN_PX),
        perfr_live_render::TimelineLod::PixelColumns
    );
    let mut group = c.benchmark_group("zoomed_out");
    group.bench_function("choose_lod/48_lanes", |b| {
        b.iter(|| black_box(choose_lod(&many, 0, 1_000_000, 1920, INSTANCE_MIN_PX)));
    });
    group.bench_function("choose_lod/1_lane", |b| {
        b.iter(|| black_box(choose_lod(&one, 0, one_t1, 1920, INSTANCE_MIN_PX)));
    });
    for &width in &[256usize, 1920] {
        group.bench_with_input(BenchmarkId::new("rasterize/1_lane", width), &width, |b, &w| {
            b.iter(|| black_box(one.rasterize_pixel(0, one_t1, w, None)));
        });
    }
    group.finish();
}

/// A lane an imported trace produces: a million scopes, and near its
/// start one 1 ns instant inside a longer slice, which is enough to put the
/// lane's ends out of order. The view is a window of 500 scopes at the end.
fn overlapping_lane(c: &mut Criterion) {
    let n = 1_000_000usize;
    let mut idx = dense_lane(n);
    idx.insert(LiveEvent {
        start_ns: 13,
        duration_ns: 1,
        tid: 1,
        pid: 1,
        kind: kind::API_SCOPE,
        depth: 0,
        extra: 0,
        _pad: 0,
        name_id: 7,
    });
    let (key, lane) = idx.lanes().next().unwrap();
    assert!(!lane.ends_are_sorted());
    let layout = stacked_layout(&[key], 0.0);
    let t1 = n as u64 * 10;
    let t0 = t1 - 5_000;
    let mut group = c.benchmark_group("overlapping_lane");
    group.bench_function("collect_tail_window", |b| {
        b.iter(|| {
            black_box(collect_instances_layout_opts(
                &idx,
                t0,
                t1,
                1920.0,
                &layout,
                None,
                CollectOpts::default(),
            ))
        });
    });
    group.bench_function("rasterize_tail_window", |b| {
        b.iter(|| black_box(idx.rasterize_pixel(t0, t1, 1920, None)));
    });
    group.finish();
}

criterion_group!(
    benches,
    zoomed_out,
    overlapping_lane,
    vs_scopes,
    vs_pixels,
    collect_y_cull,
    choose_lod_sample
);
criterion_main!(benches);
