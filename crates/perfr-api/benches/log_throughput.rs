// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Throughput of the logging path against the profiling path.
//!
//! `cargo bench -p perfr-api --bench log_throughput`
//!
//! A reader holds the segment open and capturing. Each sample is a burst of
//! events from N threads. The clock read is included: that is what a call costs.

use perfr_api::{level, log, scope, start, stop};
use perfr_scope_ring::ScopeRingReader;
use std::hint::black_box;
use std::time::Instant;

const BURST: u64 = 40_000;

fn machine() {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let model = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with("MemTotal")).map(|l| l.to_string()))
        .unwrap_or_else(|| "MemTotal: unknown".into());
    println!("machine: {cpus} hardware threads, {model}");
    println!("{mem}");
}

fn time(threads: usize, each: impl Fn() + Sync) -> (f64, f64) {
    // Warm the thread locals.
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| each());
        }
    });
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..BURST {
                    each();
                }
            });
        }
    });
    let secs = started.elapsed().as_secs_f64();
    let events = threads as f64 * BURST as f64;
    (events / secs, secs)
}

fn main() {
    machine();
    perfr_api::init().expect("segment");
    let reader = ScopeRingReader::open(std::process::id()).expect("reader");
    reader.set_capturing(true);
    // A cursor at the far end of the claim space: every claim looks caught
    // up, so the bench measures the write plus the cursor load and not a lap.
    // A real capture publishes the cursor it has actually read.
    let rings = reader.rings();
    let cursors = vec![u64::MAX - 1; rings.ring_count()];
    reader.publish_read_cursors(&cursors);

    let short = "hello";
    let slot = "s".repeat(28);
    let multi = "m".repeat(28 + 32 * 8);
    let multi_bytes = multi.len();

    println!();
    println!("| threads | path | events/s | MB/s | ns/event |");
    println!("|---:|---|---:|---:|---:|");

    let threads = [1usize, 2, 4, 8, 16, 32];
    for n in threads {
        let (rate, _) = time(n, || {
            let h = start(black_box("frame"));
            stop(h);
        });
        // A scope is two records.
        row(n, "scope start+stop (2 records)", rate, 8);
        let (rate, _) = time(n, || {
            let _s = scope(black_box("frame"));
            black_box(_s.handle());
        });
        row(n, "scope guard", rate, 8);

        let (rate, _) = time(n, || log(level::INFO, black_box(short.as_bytes())));
        row(n, "log 5 B", rate, short.len());
        let (rate, _) = time(n, || log(level::INFO, black_box(slot.as_bytes())));
        row(n, "log 28 B (one slot)", rate, slot.len());
        let (rate, _) = time(n, || log(level::INFO, black_box(multi.as_bytes())));
        row(n, &format!("log {multi_bytes} B (multi-chunk)"), rate, multi_bytes);
    }
    let _ = reader;
}

fn row(threads: usize, path: &str, events_per_s: f64, bytes: usize) {
    let mb = events_per_s * bytes as f64 / 1_000_000.0;
    let ns = 1e9 / events_per_s;
    println!(
        "| {threads} | {path} | {:.0} | {:.1} | {:.1} |",
        events_per_s, mb, ns
    );
}
