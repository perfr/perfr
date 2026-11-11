// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Log lines through the same rings as profiling events: many threads at one
//! timestamp, lines longer than a record, and a lap that has to name what it
//! destroyed.

use perfr_scope_ring::event::{kind, level, ScopeEvent};
use perfr_scope_ring::merge::{drain, Cursors};
use perfr_scope_ring::ring::{self, ring_for_thread};
use perfr_scope_ring::text::{split_name, Completeness, TextAssembler};
use std::collections::HashMap;

struct Region {
    bytes: Vec<u8>,
    offset: usize,
    ring_count: usize,
    slots: usize,
}

impl Region {
    fn new(ring_count: usize, slots: usize) -> Region {
        let mut bytes = vec![0u8; ring::layout_size(ring_count, slots) + ring::CACHE_LINE];
        let offset = bytes.as_ptr().align_offset(ring::CACHE_LINE);
        unsafe { ring::init_region(bytes.as_mut_ptr().add(offset), ring_count, slots, 1) };
        Region { bytes, offset, ring_count, slots }
    }

    fn rings(&self) -> ring::RingWriter {
        unsafe {
            ring::RingWriter::from_raw(
                self.bytes.as_ptr().add(self.offset) as *mut u8,
                self.ring_count,
                self.slots,
            )
        }
    }
}

/// The records of one log line, as the producer writes them.
fn log_line(timestamp_ns: u64, tid: u32, id: u64, lvl: u8, text: &str) -> Vec<ScopeEvent> {
    let mut head = ScopeEvent {
        timestamp_ns,
        scope_id: id,
        tid,
        kind: kind::LOG,
        flags: level::to_flags(lvl),
        ..ScopeEvent::default()
    };
    let rest = split_name(&mut head, text);
    std::iter::once(head).chain(rest).collect()
}

struct Line {
    tid: u32,
    level: u8,
    text: String,
    completeness: Completeness,
}

/// What the consumer does: order by (timestamp, tid), then put each line's
/// text back together with the same assembler scope names use.
fn reassemble(events: &[ScopeEvent]) -> Vec<Line> {
    let mut ordered = events.to_vec();
    ordered.sort_by(|a, b| a.timestamp_ns.cmp(&b.timestamp_ns).then(a.tid.cmp(&b.tid)));
    let mut text = TextAssembler::new();
    let mut awaiting: HashMap<(u32, u64), ScopeEvent> = HashMap::new();
    let mut out = Vec::new();
    let finish = |head: ScopeEvent, text: String, completeness: Completeness, out: &mut Vec<Line>| {
        out.push(Line { tid: head.tid, level: level::from_flags(head.flags), text, completeness });
    };
    for event in &ordered {
        match event.kind {
            kind::LOG if event.has_more_text() => {
                text.accept(event);
                awaiting.insert((event.tid, event.scope_id), *event);
            }
            kind::LOG => {
                // Fits the record: the text is right there, as is the cut.
                let completeness = if event.flags & perfr_scope_ring::event::flags::CUT != 0 {
                    Completeness::Truncated
                } else {
                    Completeness::Complete
                };
                finish(*event, event.name(), completeness, &mut out);
            }
            kind::TEXT => {
                if let Some((name, completeness)) = text.accept(event) {
                    if let Some(head) = awaiting.remove(&(event.tid, event.scope_id)) {
                        finish(head, name, completeness, &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[test]
fn threads_write_lines_at_one_timestamp_and_the_consumer_puts_each_back() {
    const THREADS: usize = 8;
    let region = Region::new(4, 1 << 12);
    let rings = region.rings();
    std::thread::scope(|scope| {
        for tid in 0..THREADS {
            let rings = &rings;
            scope.spawn(move || {
                let ring = ring_for_thread(tid as u64 + 1, 4);
                // A four-byte character straddles the record boundary.
                let line = format!("{}\u{1F600}-thread-{tid}-{}", "x".repeat(31), "y".repeat(80));
                for n in 0..30u64 {
                    // Same timestamp on purpose: thread id is the tiebreak.
                    for record in log_line(50, tid as u32 + 1, n + 1, level::INFO, &line) {
                        rings.push(ring, record);
                    }
                }
            });
        }
    });
    let mut cursors = Cursors::for_rings(4);
    let pass = drain(&rings, &mut cursors, u64::MAX);
    assert_eq!(pass.dropped, 0);
    let events: Vec<_> = pass.slices.into_iter().flat_map(|s| s.events).collect();
    let lines = reassemble(&events);
    assert_eq!(lines.len(), THREADS * 30);
    assert!(lines.iter().all(|l| l.completeness == Completeness::Complete && l.level == level::INFO));
    for tid in 1..=THREADS as u32 {
        let mine: Vec<&Line> = lines.iter().filter(|l| l.tid == tid).collect();
        assert_eq!(mine.len(), 30);
        assert!(mine.iter().all(|l| l.text.contains(&format!("-thread-{}-", tid - 1))), "no other thread's bytes");
    }
}

#[test]
fn a_lap_names_the_thread_and_the_kind_of_what_it_overwrote() {
    let region = Region::new(1, 8);
    let rings = region.rings();
    rings.publish_read_cursors(&[0]);
    // Seven records of one log line from thread 4 fill most of the ring.
    let message = "L".repeat(200);
    let records = log_line(10, 4, 1, level::ERROR, &message);
    assert_eq!(records.len(), 7);
    for record in records {
        rings.push(0, record);
    }
    assert!(rings.snapshot_drops().is_empty(), "the ring was not full yet");
    // Three instants from thread 9 take the ring past full: the two oldest
    // slots, the line's head and first continuation, are overwritten.
    for n in 0..3u64 {
        rings.push(0, ScopeEvent { timestamp_ns: 100 + n, tid: 9, kind: kind::INSTANT, ..ScopeEvent::default() });
    }
    // The head is a LOG record and the continuation a TEXT record; each is
    // counted as what it was, on the thread that wrote it.
    let mut drops: Vec<(u32, u8, u32)> = rings.snapshot_drops().iter().map(|d| (d.tid, d.kind, d.count)).collect();
    drops.sort_unstable();
    assert_eq!(drops, vec![(4, kind::TEXT, 1), (4, kind::LOG, 1)], "what was lost, not what replaced it");
    let mut cursors = Cursors::for_rings(1);
    let pass = drain(&rings, &mut cursors, u64::MAX);
    assert_eq!(pass.dropped, 2, "the consumer's own count of the same lap");
    // With its head gone the line cannot be placed and is not invented.
    let events: Vec<_> = pass.slices.into_iter().flat_map(|s| s.events).collect();
    assert!(reassemble(&events).is_empty());
}

#[test]
fn empty_boundary_and_cut_lines_round_trip_through_a_ring() {
    let region = Region::new(1, 1 << 10);
    let rings = region.rings();
    let cases: Vec<(u8, String)> = vec![
        (level::DEBUG, String::new()),
        (level::INFO, "a".repeat(32)),
        (level::INFO, "b".repeat(33)),
        (level::WARN, "c".repeat(64)),
        (level::WARN, "d".repeat(65)),
        (level::TRACE, format!("{}\u{1F9E0}end", "z".repeat(30))),
        (level::FATAL, "f".repeat(perfr_scope_ring::event::MAX_NAME_BYTES + 1)),
        (9, "a level past fatal is fatal".into()),
    ];
    for (i, (lvl, text)) in cases.iter().enumerate() {
        for record in log_line(i as u64, 3, i as u64 + 1, *lvl, text) {
            rings.push(0, record);
        }
    }
    let mut cursors = Cursors::for_rings(1);
    let pass = drain(&rings, &mut cursors, u64::MAX);
    assert_eq!(pass.dropped, 0);
    let events: Vec<_> = pass.slices.into_iter().flat_map(|s| s.events).collect();
    let lines = reassemble(&events);
    assert_eq!(lines.len(), cases.len());
    for ((lvl, text), line) in cases.iter().zip(lines.iter()) {
        assert_eq!(line.tid, 3);
        assert_eq!(line.level, (*lvl).min(level::FATAL));
        if text.len() > perfr_scope_ring::event::MAX_NAME_BYTES {
            assert_eq!(line.completeness, Completeness::Truncated, "cut at the cap, and says so");
            assert_eq!(line.text.len(), perfr_scope_ring::event::MAX_NAME_BYTES);
        } else {
            assert_eq!(&line.text, text);
            assert_eq!(line.completeness, Completeness::Complete);
        }
    }
}
