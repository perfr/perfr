// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Names and measurements an agent needs to aim a story at a capture.
//!
//! Threads and scopes are matched by a case-insensitive substring.
//! Core figures are the fraction of `[t0, t1)` covered by scheduling
//! slices on each core. A core counts as busy at 5% or more. That is a
//! reading of this window, not a statement about how the program was written.

use std::collections::HashMap;

use perfr_live_event::{kind, InternTable};
use perfr_live_render::TrackIndex;
use serde_json::{json, Value};

const BUSY: f64 = 0.05;
const LIMIT: usize = 40;

/// What a walk over the index finds in a window, before any name is looked
/// up. The walk is the long part (every event of every scope lane), and it
/// needs no names: the intern table is the lock the capture thread takes
/// for every new name, and holding it for the length of a walk stalls the
/// capture's drain, which lets the producers' rings lap.
pub struct Scan {
    t0: u64,
    t1: u64,
    /// Threads that have a lane, in lane order.
    lane_threads: Vec<(u32, u32)>,
    scopes: Vec<ScopeRow>,
    /// `(core, busy fraction)`, by core.
    cores: Vec<(u8, f64)>,
}

pub fn scan(index: &TrackIndex, t0: u64, t1: u64) -> Scan {
    let t1 = t1.max(t0.saturating_add(1));
    let mut seen = std::collections::BTreeSet::new();
    let mut lane_threads = Vec::new();
    for (key, _) in index.lanes() {
        if !key.is_scheduler() && seen.insert((key.pid, key.tid)) {
            lane_threads.push((key.pid, key.tid));
        }
    }

    // One row per (scope, thread). A capture has thousands of these, so
    // they are keyed, not searched.
    let mut scopes: HashMap<(u32, u32, u32), ScopeRow> = HashMap::new();
    for (key, lane) in index.lanes() {
        if key.kind != kind::API_SCOPE
            && key.kind != kind::FUNCTION_CALL
            && key.kind != kind::API_TRACK
        {
            continue;
        }
        for ev in lane.events() {
            if ev.end_ns() <= t0 || ev.start_ns >= t1 {
                continue;
            }
            let overlap = ev.end_ns().min(t1).saturating_sub(ev.start_ns.max(t0));
            if overlap == 0 {
                continue;
            }
            let row = scopes
                .entry((ev.name_id, key.pid, key.tid))
                .or_insert(ScopeRow {
                    scope_id: ev.name_id,
                    pid: key.pid,
                    tid: key.tid,
                    count: 0,
                    total_ns: 0,
                });
            row.count += 1;
            row.total_ns += overlap;
        }
    }

    let span = t1.saturating_sub(t0).max(1) as f64;
    let mut cores = Vec::new();
    for (key, lane) in index.lanes() {
        if !key.is_scheduler() {
            continue;
        }
        let busy = busy_ns(lane.events(), t0, t1) as f64 / span;
        cores.push((key.extra, busy.clamp(0.0, 1.0)));
    }
    cores.sort_by_key(|(id, _)| *id);
    Scan { t0, t1, lane_threads, scopes: scopes.into_values().collect(), cores }
}

/// The scan and its naming in one call, for a caller that owns the table.
#[cfg(test)]
pub fn summarize(
    index: &TrackIndex,
    intern: &InternTable,
    threads: &[((u32, u32), String)],
    processes: &[(u32, String)],
    t0: u64,
    t1: u64,
    query: &str,
) -> Value {
    describe(scan(index, t0, t1), intern, threads, processes, query)
}

/// Names what a scan found and filters it by `query`: lookups only, one
/// per thread and per scope row.
pub fn describe(
    scan: Scan,
    intern: &InternTable,
    threads: &[((u32, u32), String)],
    processes: &[(u32, String)],
    query: &str,
) -> Value {
    let Scan { t0, t1, lane_threads, scopes, cores } = scan;
    let needle = query.trim().to_ascii_lowercase();
    let process_name = |pid: u32| {
        processes
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, name)| name.as_str())
            .unwrap_or("")
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut thread_rows = Vec::new();
    for ((pid, tid), name) in threads {
        seen.insert((*pid, *tid));
        if thread_match(&needle, *pid, *tid, name, process_name(*pid)) {
            thread_rows.push(json!({
                "pid": pid,
                "tid": tid,
                "name": name,
                "process": process_name(*pid),
            }));
        }
    }
    for (pid, tid) in lane_threads {
        if !seen.insert((pid, tid)) {
            continue;
        }
        // A thread the names table does not know has no name. Only the
        // demo and bench producers name theirs by interning a string
        // under the tid. Everywhere else a string id is a frame or a scope,
        // and a tid can be the same number: the thread was then listed,
        // and matched by `q`, under some function's name.
        let name = if synthetic_pid(pid) { intern.get(tid).unwrap_or("") } else { "" };
        if thread_match(&needle, pid, tid, name, process_name(pid)) {
            thread_rows.push(json!({
                "pid": pid,
                "tid": tid,
                "name": name,
                "process": process_name(pid),
            }));
        }
    }
    let thread_total = thread_rows.len();
    thread_rows.truncate(LIMIT);

    let mut scope_rows = Vec::new();
    for row in scopes {
        let name = intern.get(row.scope_id).unwrap_or("");
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        scope_rows.push((
            row.total_ns,
            json!({
                "scope_id": row.scope_id,
                "name": name,
                "pid": row.pid,
                "tid": row.tid,
                "count": row.count,
                "total_ns": row.total_ns,
            }),
        ));
    }
    scope_rows.sort_by(|a, b| b.0.cmp(&a.0));
    let scope_total = scope_rows.len();
    let scopes: Vec<Value> = scope_rows
        .into_iter()
        .take(LIMIT)
        .map(|(_, row)| row)
        .collect();

    let core_count = cores.len();
    let cores_busy = cores.iter().filter(|(_, busy)| *busy >= BUSY).count();
    let cores: Vec<Value> = cores
        .into_iter()
        .map(|(core, busy)| json!({ "core": core, "busy": (busy * 1000.0).round() / 1000.0 }))
        .collect();

    json!({
        "t0_ns": t0,
        "t1_ns": t1,
        "query": query,
        "threads": thread_rows,
        "thread_total": thread_total,
        "scopes": scopes,
        "scope_total": scope_total,
        "cores": cores,
        "core_count": core_count,
        "cores_busy": cores_busy,
        "busy_at": BUSY,
        "note": "busy is the fraction of this window covered by scheduling slices on that core. cores_busy counts cores at or above busy_at. These are readings of the capture, not a description of how the program is written.",
    })
}

/// A process of the demo or the bench producer.
fn synthetic_pid(pid: u32) -> bool {
    pid == crate::bench::BENCH_PID
        || crate::demo::DEMO_PROCESSES.iter().chain(crate::demo::REMOTE_PROCESSES).any(|(demo, _)| *demo == pid)
}

struct ScopeRow {
    scope_id: u32,
    pid: u32,
    tid: u32,
    count: u64,
    total_ns: u64,
}

fn thread_match(needle: &str, pid: u32, tid: u32, name: &str, process: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    name.to_ascii_lowercase().contains(needle)
        || process.to_ascii_lowercase().contains(needle)
        || tid.to_string().contains(needle)
        || pid.to_string() == needle
}

fn busy_ns(events: &[perfr_live_event::LiveEvent], t0: u64, t1: u64) -> u64 {
    let start = events.partition_point(|e| e.end_ns() <= t0);
    let mut busy = 0u64;
    for ev in &events[start..] {
        if ev.start_ns >= t1 {
            break;
        }
        busy += ev.end_ns().min(t1).saturating_sub(ev.start_ns.max(t0));
    }
    busy
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::{kind, LiveEvent};

    fn ev(
        start: u64,
        dur: u64,
        pid: u32,
        tid: u32,
        kind_id: u8,
        extra: u8,
        name: u32,
    ) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: dur,
            tid,
            pid,
            kind: kind_id,
            depth: 0,
            extra,
            _pad: 0,
            name_id: name,
        }
    }

    #[test]
    fn threads_scopes_and_cores_in_a_window() {
        let mut index = TrackIndex::default();
        let mut intern = InternTable::default();
        intern.insert_id(7, "Worker");
        intern.insert_id(9, "SolveContacts");
        index.insert(ev(0, 500, 0, 0, kind::SCHEDULING_SLICE, 0, 0));
        index.insert(ev(0, 1_000, 0, 0, kind::SCHEDULING_SLICE, 1, 0));
        index.insert(ev(0, 10, 0, 0, kind::SCHEDULING_SLICE, 2, 0));
        index.insert(ev(100, 400, 4, 7, kind::API_SCOPE, 0, 9));
        let threads = vec![((4, 7), "Worker".into())];
        let processes = vec![(4, "box3d".into())];
        let summary = summarize(&index, &intern, &threads, &processes, 0, 1_000, "solve");
        assert_eq!(summary["scopes"][0]["name"], "SolveContacts");
        assert_eq!(summary["scopes"][0]["scope_id"], 9);
        assert_eq!(summary["scopes"][0]["total_ns"], 400);
        assert_eq!(summary["thread_total"], 0);
        let all = summarize(&index, &intern, &threads, &processes, 0, 1_000, "");
        assert_eq!(all["thread_total"], 1);
        assert_eq!(all["threads"][0]["name"], "Worker");
        assert_eq!(all["core_count"], 3);
        assert_eq!(all["cores_busy"], 2);
        assert_eq!(all["cores"][0]["busy"], 0.5);
        assert_eq!(all["cores"][1]["busy"], 1.0);
        let named = summarize(&index, &intern, &threads, &processes, 0, 1_000, "box");
        assert_eq!(named["threads"][0]["process"], "box3d");
    }

    #[test]
    fn a_thread_nobody_named_is_not_given_the_string_whose_id_is_its_tid() {
        let mut index = TrackIndex::default();
        let mut intern = InternTable::default();
        // String ids are frame and scope names; a real capture's start at
        // 1 << 20, and Linux tids go four times as high.
        let tid = (1 << 20) + 1_424;
        intern.insert_id(tid, "physics::solve_contacts");
        intern.insert_id(9, "Tick");
        index.insert(ev(100, 400, 4242, tid, kind::API_SCOPE, 0, 9));
        let all = summarize(&index, &intern, &[], &[], 0, 1_000, "");
        assert_eq!(all["thread_total"], 1);
        assert_eq!(all["threads"][0]["tid"], tid);
        assert_eq!(all["threads"][0]["name"], "", "named after a function that happens to have its number");
        // Nor is it found by that function's name.
        let found = summarize(&index, &intern, &[], &[], 0, 1_000, "solve_contacts");
        assert_eq!(found["thread_total"], 0, "{}", found["threads"]);

        // The demo and the bench do name their threads that way, and only
        // that way: theirs keep their names.
        let mut index = TrackIndex::default();
        intern.insert_id(100, "Main");
        intern.insert_id(crate::bench::BENCH_TID_BASE, "bench-0");
        index.insert(ev(100, 400, crate::demo::DEMO_PID, 100, kind::API_SCOPE, 0, 9));
        index.insert(ev(100, 400, crate::bench::BENCH_PID, crate::bench::BENCH_TID_BASE, kind::API_SCOPE, 0, 9));
        let demo = summarize(&index, &intern, &[], &[], 0, 1_000, "");
        let names: Vec<&str> = demo["threads"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Main", "bench-0"]);
    }
}
