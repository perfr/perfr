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
//! Channel filtering, when a capture has scope channel metadata, filters
//! scopes. The lines that remain are the ones inside the scopes that remain.

use crate::{kind, LiveEvent};

/// One scope interval the correlator can test a log against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeSpan {
    pub pid: u32,
    pub tid: u32,
    pub name_id: u32,
    pub channel: u8,
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
            channel: event.scope_channel().unwrap_or(0),
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
}

/// Identical lines grouped across many instances of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogGroup {
    pub name_id: u32,
    pub level: u8,
    pub tid: u32,
    pub count: u32,
    pub first_ns: u64,
    pub last_ns: u64,
    pub truncated: bool,
}

/// Scopes in `events`, optionally limited to one thread and one channel.
/// Channel `None` keeps every scope. Channel `Some(id)` keeps scopes recorded
/// with that id, which drops their logs with them.
pub fn scope_spans(events: &[LiveEvent], tid: Option<u32>, channel: Option<u8>) -> Vec<ScopeSpan> {
    events
        .iter()
        .filter_map(ScopeSpan::from_event)
        .filter(|s| tid.is_none_or(|t| s.tid == t))
        .filter(|s| channel.is_none_or(|c| s.channel == c))
        .collect()
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
    out.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then(a.tid.cmp(&b.tid)));
    out
}

/// Log lines that fall inside at least one of `spans`.
///
/// A line inside nested scopes is returned once. Order is timestamp, then
/// thread id.
pub fn logs_in_spans(events: &[LiveEvent], spans: &[ScopeSpan]) -> Vec<LiveEvent> {
    let mut out: Vec<LiveEvent> = events
        .iter()
        .copied()
        .filter(|e| e.kind == kind::LOG && spans.iter().any(|s| s.contains_log(e)))
        .collect();
    out.sort_by(|a, b| a.start_ns.cmp(&b.start_ns).then(a.tid.cmp(&b.tid)).then(a.name_id.cmp(&b.name_id)));
    out.dedup_by(|a, b| a.start_ns == b.start_ns && a.tid == b.tid && a.name_id == b.name_id && a.extra == b.extra);
    out
}

/// Every instance of one scope name on one thread.
pub fn instances_of(events: &[LiveEvent], pid: u32, tid: u32, name_id: u32) -> Vec<ScopeSpan> {
    scope_spans(events, Some(tid), None)
        .into_iter()
        .filter(|s| s.pid == pid && s.name_id == name_id)
        .collect()
}

/// Group identical lines. `name_id` is the interned text, so the same message
/// at the same level on the same thread is one row with a count.
pub fn aggregate(logs: &[LiveEvent]) -> Vec<LogGroup> {
    let mut groups: Vec<LogGroup> = Vec::new();
    for log in logs.iter().filter(|e| e.kind == kind::LOG) {
        let level = log.log_level().unwrap_or(0);
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.name_id == log.name_id && g.level == level && g.tid == log.tid)
        {
            group.count = group.count.saturating_add(1);
            group.first_ns = group.first_ns.min(log.start_ns);
            group.last_ns = group.last_ns.max(log.start_ns);
            group.truncated |= log.log_truncated();
        } else {
            groups.push(LogGroup {
                name_id: log.name_id,
                level,
                tid: log.tid,
                count: 1,
                first_ns: log.start_ns,
                last_ns: log.start_ns,
                truncated: log.log_truncated(),
            });
        }
    }
    groups.sort_by(|a, b| b.count.cmp(&a.count).then(a.first_ns.cmp(&b.first_ns)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(start: u64, end: u64, tid: u32, name: u32, channel: u8) -> LiveEvent {
        LiveEvent {
            start_ns: start,
            duration_ns: end.saturating_sub(start),
            tid,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: channel,
            _pad: 0,
            name_id: name,
        }
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
        let frame = scope(100, 200, 7, 1, 0);
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
            scope(100, 300, 7, 1, 0),
            scope(150, 180, 7, 2, 0),
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
            events.push(scope(start, start + 100, 4, 1, 3));
            events.push(log_at(start + 10, 4, 20));
            if i % 5 == 0 {
                events.push(log_at(start + 20, 4, 21));
            }
        }
        // A different scope name, and the same message on another thread,
        // must not join the group.
        // Between frame 0 [0, 100] and frame 1 [1000, 1100], so it is not
        // inside any instance of the frame scope.
        events.push(scope(200, 250, 4, 9, 3));
        events.push(log_at(220, 4, 20));
        events.push(scope(10, 50, 5, 1, 3));
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
    }

    #[test]
    fn a_channel_filter_drops_scopes_and_the_logs_inside_them() {
        let events = vec![
            scope(0, 50, 1, 1, 2),
            log_at(25, 1, 8),
            scope(60, 100, 1, 2, 4),
            log_at(80, 1, 9),
            log_at(150, 1, 7),
        ];
        let sim = scope_spans(&events, Some(1), Some(2));
        assert_eq!(sim.len(), 1);
        assert_eq!(sim[0].name_id, 1);
        let logs = logs_in_spans(&events, &sim);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name_id, 8);
        // No channel filter: the line outside every scope is still a line
        // in the window, and both scopes' lines are too.
        let window = logs_in_window(&events, 0, 200, Some(1));
        assert_eq!(window.len(), 3);
    }

    #[test]
    fn an_old_scope_with_no_channel_still_correlates() {
        // Captures written before channels store extra = 0. That is the
        // default channel, and the interval test does not need a name.
        let events = vec![scope(10, 20, 3, 1, 0), log_at(10, 3, 4), log_at(21, 3, 5)];
        let spans = scope_spans(&events, None, None);
        let logs = logs_in_spans(&events, &spans);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].name_id, 4);
        assert!(events.iter().all(|e| e.kind != kind::LOG || e.scope_channel().is_none()));
    }
}
