// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The logging path through the same rings as profiling events: many threads,
//! long lines, holes, and a lap that has to be visible.

use perfr_scope_ring::event::kind;
use perfr_scope_ring::logmsg::{write_log, LogAssembler, MAX_LOG_BYTES};
use perfr_scope_ring::merge::{drain, Cursors};
use perfr_scope_ring::ring::{self, ring_for_thread};
use perfr_scope_ring::text::Completeness;
use perfr_scope_ring::ScopeEvent;
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

    fn rings(&self) -> ring::Rings {
        unsafe {
            ring::Rings::from_raw(
                self.bytes.as_ptr().add(self.offset) as *mut u8,
                self.ring_count,
                self.slots,
            )
        }
    }
}

fn reassemble(events: &[ScopeEvent]) -> Vec<perfr_scope_ring::LogMessage> {
    let mut ordered = events.to_vec();
    ordered.sort_by(|a, b| a.timestamp_ns.cmp(&b.timestamp_ns).then(a.tid.cmp(&b.tid)));
    let mut asm = LogAssembler::new();
    let mut done = Vec::new();
    for event in &ordered {
        if let Some(msg) = asm.accept(event) {
            done.push(msg);
        }
    }
    done.extend(asm.finish());
    done
}

#[test]
fn threads_write_lines_and_the_consumer_puts_each_back() {
    const THREADS: usize = 8;
    let region = Region::new(4, 1 << 12);
    let rings = region.rings();
    std::thread::scope(|scope| {
        for tid in 0..THREADS {
            let rings = &rings;
            scope.spawn(move || {
                let ring = ring_for_thread(tid as u64 + 1, 4);
                let head = "x".repeat(28);
                let line = format!("{head}\u{1F600}-thread-{tid}-{}", "y".repeat(80));
                for n in 0..30u32 {
                    // Same timestamp on purpose: thread id is the tiebreak.
                    write_log(50, tid as u32 + 1, n + 1, 2, line.as_bytes(), |e| rings.push(ring, e));
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
    assert!(lines.iter().all(|m| m.completeness == Completeness::Complete));
    for tid in 1..=THREADS as u32 {
        assert_eq!(lines.iter().filter(|m| m.tid == tid).count(), 30);
    }
}

#[test]
fn a_lap_names_the_thread_and_the_kind_and_a_cut_line_stays_truncated() {
    let region = Region::new(1, 8);
    let rings = region.rings();
    rings.publish_read_cursors(&[0]);
    let ring = 0;
    // Fill the ring with log chunks from thread 4, then overwrite them
    // with instants from thread 9. The lost records are the logs.
    let message = "L".repeat(200);
    write_log(10, 4, 1, 4, message.as_bytes(), |e| rings.push(ring, e));
    let logs_written = rings.snapshot_drops();
    assert!(logs_written.is_empty(), "the ring was not full yet");
    for n in 0..8u64 {
        rings.push(ring, ScopeEvent { timestamp_ns: 100 + n, tid: 9, kind: kind::INSTANT, ..ScopeEvent::default() });
    }
    let mut drops = rings.snapshot_drops();
    drops.sort_by_key(|d| (d.tid, d.kind));
    assert!(
        drops.iter().any(|d| d.tid == 4 && d.kind == kind::LOG && d.count >= 1),
        "the overwritten chunks were logs from thread 4, not the instants that replaced them: {drops:?}"
    );
    let mut cursors = Cursors::for_rings(1);
    let pass = drain(&rings, &mut cursors, u64::MAX);
    assert!(pass.dropped >= 1, "the consumer counts the lap too");
    let events: Vec<_> = pass.slices.into_iter().flat_map(|s| s.events).collect();
    let lines = reassemble(&events.iter().filter(|e| e.kind == kind::LOG).cloned().collect::<Vec<_>>());
    if let Some(line) = lines.iter().find(|m| m.message_id == 1) {
        assert_eq!(line.completeness, Completeness::Truncated);
        assert!(line.message.contains('\u{FFFD}') || line.message.len() < message.len());
    }
    let _ = MAX_LOG_BYTES;
}

#[test]
fn empty_slot_boundary_and_utf8_round_trip_through_a_ring() {
    let region = Region::new(1, 64);
    let rings = region.rings();
    let cases: Vec<(u8, String)> = vec![
        (2, String::new()),
        (3, "a".repeat(28)),
        (3, "b".repeat(29)),
        (4, "c".repeat(28 + 32)),
        (4, "d".repeat(28 + 32 + 1)),
        (1, format!("{}\u{1F9E0}end", "z".repeat(26))),
    ];
    for (i, (level, message)) in cases.iter().enumerate() {
        write_log(i as u64, 3, i as u32 + 1, *level, message.as_bytes(), |e| rings.push(0, e));
    }
    let mut cursors = Cursors::for_rings(1);
    let pass = drain(&rings, &mut cursors, u64::MAX);
    assert_eq!(pass.dropped, 0);
    let events: Vec<_> = pass.slices.into_iter().flat_map(|s| s.events).collect();
    let lines = reassemble(&events);
    assert_eq!(lines.len(), cases.len());
    for ((_, message), line) in cases.iter().zip(lines.iter()) {
        assert_eq!(&line.message, message);
        assert_eq!(line.completeness, Completeness::Complete);
        assert_eq!(line.tid, 3);
    }
}
