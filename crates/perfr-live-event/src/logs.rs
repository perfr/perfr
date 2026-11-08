// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Which log lines belong to which scopes.
//!
//! A log line carries a thread and a timestamp, nothing else that points at
//! a scope. It belongs to a scope when that timestamp falls inside the
//! scope's enter/exit interval on the same thread (and the same process).
//! The interval is closed: a line at the enter timestamp and a line at the
//! exit timestamp both belong. Nested scopes both contain a line that sits
//! inside the inner one. A line on another thread never does, even at the
//! same timestamp.
//!
//! That makes a scope the way to group log lines. There is no separate
//! category on a line: to read what one subsystem logged, wrap it in a scope
//! and ask for the lines inside that scope.

use std::collections::HashMap;

use crate::{kind, LiveEvent};

/// The name of a log level, lower case. Anything past fatal reads as fatal.
pub fn level_name(level: u8) -> &'static str {
    match level {
        0 => "trace",
        1 => "debug",
        2 => "info",
        3 => "warn",
        4 => "error",
        _ => "fatal",
    }
}

/// One scope interval the correlator can test a log against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeSpan {
    pub pid: u32,
    pub tid: u32,
    pub name_id: u32,
    pub start_ns: u64,
    pub end_ns: u64,
}

impl ScopeSpan {
    pub fn from_event(event: &LiveEvent) -> Option<Self> {
        if event.kind != kind::API_SCOPE && event.kind != kind::API_TRACK {
            return None;
        }
        Some(Self {
            pid: event.pid,
            tid: event.tid,
            name_id: event.name_id,
            start_ns: event.start_ns,
            end_ns: event.end_ns(),
        })
    }

    pub fn contains_log(self, log: &LiveEvent) -> bool {
        log.kind == kind::LOG
            && log.pid == self.pid
            && log.tid == self.tid
            && log.start_ns >= self.start_ns
            && log.start_ns <= self.end_ns
    }

    /// Whether any part of the scope is inside the closed range `[t0, t1]`.
    pub fn overlaps(self, t0: u64, t1: u64) -> bool {
        self.start_ns <= t1 && self.end_ns >= t0
    }
}

/// A set of scope intervals, indexed so that asking whether a log line is
/// inside any of them is a binary search.
///
/// The naive test, every line against every span, is the product of the two:
/// a per-frame scope with ten thousand instances against a hundred thousand
/// lines is a billion comparisons. Here each thread's spans are merged into
/// disjoint intervals once, sorted, and a line costs one search on its own
/// thread.
#[derive(Clone, Debug, Default)]
pub struct SpanSet {
    /// Per `(pid, tid)`: disjoint closed intervals, sorted by start.
    by_thread: HashMap<(u32, u32), Vec<(u64, u64)>>,
}

impl SpanSet {
    pub fn new(spans: &[ScopeSpan]) -> SpanSet {
        let mut by_thread: HashMap<(u32, u32), Vec<(u64, u64)>> = HashMap::new();
        for span in spans {
            by_thread.entry((span.pid, span.tid)).or_default().push((span.start_ns, span.end_ns));
        }
        for intervals in by_thread.values_mut() {
            intervals.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::with_capacity(intervals.len());
            for &(start, end) in intervals.iter() {
                match merged.last_mut() {
                    Some(last) if start <= last.1 => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            *intervals = merged;
        }
        SpanSet { by_thread }
    }

    pub fn is_empty(&self) -> bool {
        self.by_thread.is_empty()
    }

    /// Whether `log` is a log line inside at least one span of the set.
    pub fn contains_log(&self, log: &LiveEvent) -> bool {
        if log.kind != kind::LOG {
            return false;
        }
        let Some(intervals) = self.by_thread.get(&(log.pid, log.tid)) else {
            return false;
        };
        // The last interval that starts at or before the line is the only
        // one that can hold it: the intervals do not overlap.
        let after = intervals.partition_point(|&(start, _)| start <= log.start_ns);
        after > 0 && log.start_ns <= intervals[after - 1].1
    }
}

/// Identical lines grouped across many instances of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogGroup {
    pub name_id: u32,
    pub level: u8,
    pub pid: u32,
    pub tid: u32,
    pub count: u32,
    pub first_ns: u64,
    pub last_ns: u64,
    pub truncated: bool,
}

/// Log lines in the closed interval `[t0, t1]`, optionally one thread.
/// This is the view while no scope is selected: the lines currently on screen.
pub fn logs_in_window(events: &[LiveEvent], t0: u64, t1: u64, tid: Option<u32>) -> Vec<LiveEvent> {
    let (t0, t1) = (t0.min(t1), t0.max(t1));
    let mut out: Vec<LiveEvent> = events
        .iter()
        .copied()
        .filter(|e| e.kind == kind::LOG)
        .filter(|e| tid.is_none_or(|t| e.tid == t))
        .filter(|e| e.start_ns >= t0 && e.start_ns <= t1)
        .collect();
    // Stable, so one thread's lines at one timestamp stay in the order they
    // were written.
    out.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then(a.tid.cmp(&b.tid)));
    out
}

/// Group identical lines. `name_id` is the interned text, so the same message
/// at the same level on the same thread is one row with a count. Most
/// frequent first; ties in the order the lines first occurred.
pub fn aggregate(logs: &[LiveEvent]) -> Vec<LogGroup> {
    let mut groups: Vec<LogGroup> = Vec::new();
    let mut slot: HashMap<(u32, u8, u32, u32), usize> = HashMap::new();
    for log in logs.iter().filter(|e| e.kind == kind::LOG) {
        let level = log.log_level().unwrap_or(0);
        let index = *slot.entry((log.name_id, level, log.pid, log.tid)).or_insert_with(|| {
            groups.push(LogGroup {
                name_id: log.name_id,
                level,
                pid: log.pid,
                tid: log.tid,
                count: 0,
                first_ns: log.start_ns,
                last_ns: log.start_ns,
                truncated: false,
            });
            groups.len() - 1
        });
        let group = &mut groups[index];
        group.count = group.count.saturating_add(1);
        group.first_ns = group.first_ns.min(log.start_ns);
        group.last_ns = group.last_ns.max(log.start_ns);
        group.truncated |= log.log_truncated();
    }
    groups.sort_by(|a, b| b.count.cmp(&a.count).then(a.first_ns.cmp(&b.first_ns)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(start: u64, end: u64, tid: u32, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: end.saturating_sub(start),
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: name,
        }
    }

    /// Every instance of one scope name on one thread.
    fn instances_of(events: &[LiveEvent], pid: u32, tid: u32, name_id: u32) -> Vec<ScopeSpan> {
        events
            .iter()
            .filter_map(ScopeSpan::from_event)
            .filter(|s| s.pid == pid && s.tid == tid && s.name_id == name_id)
            .collect()
    }

    /// The log lines inside at least one of `spans`, in time order.
    fn logs_in_spans(events: &[LiveEvent], spans: &[ScopeSpan]) -> Vec<LiveEvent> {
        let set = SpanSet::new(spans);
        let all = logs_in_window(events, 0, u64::MAX, None);
        all.into_iter().filter(|e| set.contains_log(e)).collect()
    }

    fn log_at(ts: u64, tid: u32, name: u32) -> LiveEvent {
        LiveEvent {
            start_ns: ts,
            duration_ns: 0,
            tid,
            pid: 1,
            kind: kind::LOG,
            depth: 0,
            extra: 2,
            _pad: 0,
            name_id: name,
        }
    }

    #[test]
    fn a_log_on_the_boundary_belongs_and_one_past_it_does_not() {
        let frame = scope(100, 200, 7, 1);
        let span = ScopeSpan::from_event(&frame).unwrap();
        assert!(span.contains_log(&log_at(100, 7, 9)), "enter is inside");
        assert!(span.contains_log(&log_at(200, 7, 9)), "exit is inside");
        assert!(!span.contains_log(&log_at(99, 7, 9)));
        assert!(!span.contains_log(&log_at(201, 7, 9)));
        assert!(!span.contains_log(&log_at(150, 8, 9)), "another thread");
    }

    #[test]
    fn nested_scopes_each_see_only_their_own_interval() {
        // outer [100, 300], inner [150, 180]
        let events = vec![
            scope(100, 300, 7, 1),
            scope(150, 180, 7, 2),
            log_at(100, 7, 10),
            log_at(160, 7, 11),
            log_at(180, 7, 12),
            log_at(250, 7, 13),
            log_at(300, 7, 14),
            log_at(160, 8, 15),
        ];
        let outer = instances_of(&events, 1, 7, 1);
        let inner = instances_of(&events, 1, 7, 2);
        let outer_logs = logs_in_spans(&events, &outer);
        let inner_logs = logs_in_spans(&events, &inner);
        let ids = |logs: &[LiveEvent]| logs.iter().map(|e| e.name_id).collect::<Vec<_>>();
        assert_eq!(ids(&outer_logs), vec![10, 11, 12, 13, 14]);
        assert_eq!(ids(&inner_logs), vec![11, 12], "the inner interval, both boundaries");
        assert!(!inner_logs.iter().any(|e| e.tid == 8));
    }

    #[test]
    fn recurrent_scopes_aggregate_identical_lines_across_instances() {
        let mut events = Vec::new();
        for i in 0..50u64 {
            let start = i * 1000;
            events.push(scope(start, start + 100, 4, 1));
            events.push(log_at(start + 10, 4, 20));
            if i % 5 == 0 {
                events.push(log_at(start + 20, 4, 21));
            }
        }
        // A different scope name, and the same message on another thread,
        // must not join the group.
        // Between frame 0 [0, 100] and frame 1 [1000, 1100], so it is not
        // inside any instance of the frame scope.
        events.push(scope(200, 250, 4, 9));
        events.push(log_at(220, 4, 20));
        events.push(scope(10, 50, 5, 1));
        events.push(log_at(15, 5, 20));

        let spans = instances_of(&events, 1, 4, 1);
        assert_eq!(spans.len(), 50);
        let logs = logs_in_spans(&events, &spans);
        let groups = aggregate(&logs);
        let main = groups.iter().find(|g| g.name_id == 20).unwrap();
        assert_eq!(main.count, 50);
        assert_eq!(main.first_ns, 10);
        assert_eq!(main.last_ns, 49 * 1000 + 10);
        let rare = groups.iter().find(|g| g.name_id == 21).unwrap();
        assert_eq!(rare.count, 10);
        assert_eq!(groups[0].name_id, 20, "most frequent first");
    }

    #[test]
    fn a_scope_groups_its_lines_and_leaves_the_rest_to_the_window() {
        // Two subsystems, each wrapped in its own scope: the scope is what
        // tells their lines apart.
        let events = vec![
            scope(0, 50, 1, 1),
            log_at(25, 1, 8),
            scope(60, 100, 1, 2),
            log_at(80, 1, 9),
            log_at(150, 1, 7),
        ];
        let sim = instances_of(&events, 1, 1, 1);
        assert_eq!(sim.len(), 1);
        let logs = logs_in_spans(&events, &sim);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name_id, 8);
        // No scope: the line outside every scope is still a line in the
        // window, and both scopes' lines are too.
        let window = logs_in_window(&events, 0, 200, Some(1));
        assert_eq!(window.len(), 3);
    }

    #[test]
    fn identical_lines_at_one_instant_are_each_counted() {
        // A tight loop can log the same text twice inside one clock tick.
        // Both happened, so both are returned and both are counted.
        let events = vec![scope(0, 100, 3, 1), log_at(40, 3, 5), log_at(40, 3, 5), log_at(40, 3, 6)];
        let logs = logs_in_spans(&events, &instances_of(&events, 1, 3, 1));
        assert_eq!(logs.len(), 3);
        let groups = aggregate(&logs);
        assert_eq!(groups.iter().find(|g| g.name_id == 5).unwrap().count, 2);
        // The same thread at the same timestamp keeps its write order.
        assert_eq!(logs.iter().map(|e| e.name_id).collect::<Vec<_>>(), vec![5, 5, 6]);
    }

    #[test]
    fn a_span_set_agrees_with_testing_every_span() {
        // Overlapping, nested, touching and disjoint spans over two threads,
        // checked against the definition for every timestamp around them.
        let spans: Vec<ScopeSpan> = [
            scope(10, 20, 1, 1),
            scope(15, 30, 1, 2),
            scope(30, 31, 1, 3),
            scope(40, 40, 1, 4),
            scope(50, 90, 1, 5),
            scope(60, 70, 1, 6),
            scope(12, 14, 2, 1),
        ]
        .iter()
        .filter_map(ScopeSpan::from_event)
        .collect();
        let set = SpanSet::new(&spans);
        for tid in [1, 2, 3] {
            for ts in 0..100 {
                let log = log_at(ts, tid, 9);
                let expected = spans.iter().any(|s| s.contains_log(&log));
                assert_eq!(set.contains_log(&log), expected, "tid {tid} at {ts}");
            }
        }
        assert!(!set.contains_log(&scope(10, 20, 1, 1)), "only log lines are inside a scope");
        assert!(SpanSet::new(&[]).is_empty());
    }

    #[test]
    fn many_instances_against_many_lines_is_not_a_product() {
        // 20,000 instances and 200,000 lines: four billion pair tests the
        // naive way. Indexed it is a few hundred thousand searches.
        let mut events = Vec::new();
        for i in 0..20_000u64 {
            events.push(scope(i * 100, i * 100 + 50, 1, 1));
        }
        for i in 0..200_000u64 {
            events.push(log_at(i * 10, 1, 7));
        }
        let spans = instances_of(&events, 1, 1, 1);
        let started = std::time::Instant::now();
        let logs = logs_in_spans(&events, &spans);
        // Each scope is [i*100, i*100+50]: six of every ten lines.
        assert_eq!(logs.len(), 120_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }
}
