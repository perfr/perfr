// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A producer lapping a reader while it reads.
//!
//! The one interleaving the single-threaded tests cannot build: the reader
//! copies a slot out while the producer, a whole lap ahead, is writing the
//! next record into it. One producer writes, so a record's claim is the
//! number of records pushed before it, and that number is stamped into every
//! field. A record that was read whole, from the claim the reader asked for,
//! is that number throughout and nothing else.

use perfr_scope_ring::merge::{drain, Cursors};
use perfr_scope_ring::ring::{self, Rings};
use perfr_scope_ring::ScopeEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One run at a time. Each is a producer spinning flat out beside a reader,
/// and what they are after is the two of them truly running at once: on a
/// machine with few cores, two runs side by side would mostly take turns.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Long enough to fail every time on the reader this guards against, which
/// went wrong within 30 ms in each of 40 unoptimised runs.
const RUN: Duration = Duration::from_millis(500);

struct Region {
    bytes: Vec<u8>,
    offset: usize,
    slots: usize,
}

impl Region {
    fn new(slots: usize) -> Region {
        let mut bytes = vec![0u8; ring::layout_size(1, slots) + ring::CACHE_LINE];
        let offset = bytes.as_ptr().align_offset(ring::CACHE_LINE);
        // SAFETY: the allocation has room for the layout past the alignment.
        unsafe { ring::init_region(bytes.as_mut_ptr().add(offset), 1, slots, 1) };
        Region { bytes, offset, slots }
    }

    fn rings(&self) -> Rings {
        // SAFETY: initialised above at exactly these dimensions.
        unsafe { Rings::from_raw(self.bytes.as_ptr().add(self.offset) as *mut u8, 1, self.slots) }
    }
}

/// The record of claim `n`: `n` in every field, so a copy that mixes two
/// records cannot agree with itself.
fn stamped(n: u64) -> ScopeEvent {
    let b = n as u8;
    ScopeEvent {
        timestamp_ns: n,
        scope_id: n,
        tid: n as u32,
        kind: b,
        depth: b,
        flags: b,
        text_len: b,
        text: [b; 32],
    }
}

fn is_whole(event: &ScopeEvent) -> bool {
    *event == stamped(event.timestamp_ns)
}

#[derive(Debug, Default)]
struct Run {
    pushed: u64,
    delivered: u64,
    dropped: u64,
    /// Passes that lost something, and how many of those still read a record.
    lapped_passes: u64,
    lapped_passes_that_delivered: u64,
}

/// Checks one pass that moved the read cursor from `before` to `after`.
fn check_pass(events: &[ScopeEvent], dropped: u64, before: u64, after: u64) -> Result<(), String> {
    let mut floor = before;
    for event in events {
        if !is_whole(event) {
            return Err(format!(
                "a record torn between two claims was delivered, reading {before}..{after}: {event:?}"
            ));
        }
        // Claims are read in order, so a whole record that is out of order
        // or outside the claims this pass covered came from the wrong lap.
        let claim = event.timestamp_ns;
        if claim < floor || claim >= after {
            return Err(format!(
                "record {claim} was delivered as a claim in {floor}..{after}"
            ));
        }
        floor = claim + 1;
    }
    if events.len() as u64 + dropped != after - before {
        return Err(format!(
            "{} delivered and {dropped} dropped do not account for claims {before}..{after}",
            events.len()
        ));
    }
    Ok(())
}

/// One producer pushing flat out into `slots` slots while a reader drains,
/// back to back or `pause` apart, for [`RUN`]; then a last drain of the ring
/// at rest.
fn lap(slots: usize, pause: Option<Duration>) -> Run {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let region = Region::new(slots);
    let rings = region.rings();
    let stop = AtomicBool::new(false);
    let mut run = Run::default();
    let mut cursors = Cursors::for_rings(1);
    let mut failure = None;

    std::thread::scope(|scope| {
        let producer = scope.spawn(|| {
            let mut n = 0u64;
            loop {
                rings.push(0, stamped(n));
                n += 1;
                if stop.load(Ordering::Relaxed) {
                    return n;
                }
            }
        });

        let started = Instant::now();
        'reading: while started.elapsed() < RUN {
            // Back to back, the clock is read once per batch of passes.
            for _ in 0..if pause.is_some() { 1 } else { 256 } {
                if let Some(pause) = pause {
                    std::thread::sleep(pause);
                }
                let before = cursors.read[0];
                let pass = drain(&rings, &mut cursors, 0);
                let events = &pass.slices[0].events;
                if let Err(what) = check_pass(events, pass.dropped, before, cursors.read[0]) {
                    failure = Some(what);
                    break 'reading;
                }
                run.delivered += events.len() as u64;
                run.dropped += pass.dropped;
                if pass.dropped > 0 {
                    run.lapped_passes += 1;
                    run.lapped_passes_that_delivered += u64::from(!events.is_empty());
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        run.pushed = producer.join().unwrap();
    });
    if let Some(what) = failure {
        panic!("{slots} slots, after {} records: {what}", run.pushed);
    }

    // At rest nothing is in doubt: whatever the ring still holds is read,
    // through to the last record pushed, and every claim is accounted for.
    // One more record first, so there is always something left to read: the
    // reader's last pass may have caught up with a producer that was not
    // running at that moment.
    rings.push(0, stamped(run.pushed));
    run.pushed += 1;
    let before = cursors.read[0];
    let pass = drain(&rings, &mut cursors, 0);
    let events = &pass.slices[0].events;
    check_pass(events, pass.dropped, before, cursors.read[0]).unwrap();
    run.delivered += events.len() as u64;
    run.dropped += pass.dropped;
    assert_eq!(cursors.read[0], run.pushed, "the last drain reaches the write cursor");
    assert_eq!(events.last().map(|e| e.timestamp_ns), Some(run.pushed - 1));
    assert_eq!(run.delivered + run.dropped, run.pushed, "every claim is delivered or counted lost");
    println!("{slots} slots: {run:?}");
    run
}

#[test]
fn a_reader_being_lapped_never_delivers_a_torn_or_misplaced_record() {
    // Eight slots and a reader that never rests: the producer is a lap
    // ahead every few tens of nanoseconds, so nearly every slot the reader
    // reaches is being overwritten or about to be. `lap` checks every record
    // that comes out.
    let run = lap(8, None);
    assert!(run.dropped > 0, "the ring was never lapped: {run:?}");
}

#[test]
fn a_reader_being_lapped_still_delivers_what_it_can_reach() {
    // A reader that comes back every half millisecond to a ring the
    // producer has been round many times, and is still going round. The
    // slot it resumes at is overwritten before it gets there; taking that
    // for "not committed yet" ended the pass on the spot, with nothing. A
    // reader that steps clear of the slots about to go has time to copy
    // some of the others out.
    let run = lap(1024, Some(Duration::from_micros(500)));
    assert!(run.lapped_passes > 0, "the ring was never lapped: {run:?}");
    assert!(
        run.lapped_passes_that_delivered * 2 > run.lapped_passes,
        "most passes that were lapped came back empty: {run:?}"
    );
}
