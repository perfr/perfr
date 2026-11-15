// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Prints what a `perf.data` holds: its events, its counts, and the
//! hottest frames by `module+offset`. `cargo run -p perfr-perf-data
//! --example dump -- perf.data`.

use std::collections::HashMap;

use perfr_perf_data::{read_all, Event, Processes};

fn main() -> Result<(), String> {
    let path = std::env::args().nth(1).ok_or("usage: dump <perf.data>")?;
    let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    let (session, events) = read_all(&bytes, false)?;
    println!(
        "{path}: {} records, {} samples, {} lost",
        session.stats.records, session.stats.samples, session.stats.lost
    );
    println!(
        "host {:?}  os {:?}  arch {:?}  cmdline {:?}",
        session.info.hostname, session.info.os_release, session.info.arch, session.info.cmdline
    );
    for (i, attr) in session.attrs().iter().enumerate() {
        println!(
            "event {i}: type {} config {:#x} sample_type {:#x} period {:?} ns, {} ids",
            attr.kind,
            attr.config,
            attr.sample_type,
            attr.period_ns(),
            attr.ids.len()
        );
    }
    let mut processes = Processes::default();
    let mut kinds: HashMap<&str, u64> = HashMap::new();
    let mut leaves: HashMap<String, u64> = HashMap::new();
    let mut threads: HashMap<(u32, u32), u64> = HashMap::new();
    let (mut first, mut last, mut depth) = (u64::MAX, 0u64, 0usize);
    for (time, event) in &events {
        processes.apply(event);
        let kind = match event {
            Event::Mmap(_) => "mmap",
            Event::Comm { .. } => "comm",
            Event::Fork { .. } => "fork",
            Event::Exit { .. } => "exit",
            Event::Switch { .. } => "switch",
            Event::Sample(sample) => {
                first = first.min(*time);
                last = last.max(*time);
                depth = depth.max(sample.frames.len());
                *threads.entry((sample.pid, sample.tid)).or_default() += 1;
                if let Some(leaf) = sample.frames.first() {
                    *leaves
                        .entry(processes.label(sample.pid, *leaf))
                        .or_default() += 1;
                }
                "sample"
            }
        };
        *kinds.entry(kind).or_default() += 1;
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort();
    println!("events: {kinds:?}");
    println!(
        "samples span {:.3} ms, deepest stack {depth}",
        last.saturating_sub(first) as f64 / 1e6
    );
    let mut threads: Vec<_> = threads.into_iter().collect();
    threads.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for ((pid, tid), n) in threads.iter().take(8) {
        println!(
            "  {n:>6} samples  {}/{} {:?} of {:?}",
            pid,
            tid,
            processes.thread_name(*tid).unwrap_or(""),
            processes.process_name(*pid).unwrap_or("")
        );
    }
    let mut leaves: Vec<_> = leaves.into_iter().collect();
    leaves.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (label, n) in leaves.iter().take(8) {
        println!("  {n:>6}  {label}");
    }
    Ok(())
}
