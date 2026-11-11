// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A producer that stops between being handed a claim and committing it,
//! while the rest of its ring goes all the way round.
//!
//! A claim number is unique; the slot it maps to is not. A thread that is
//! descheduled holding claim `c` wakes to find its slot has been given to
//! claim `c + capacity`, and anything it stores there lands on a record it
//! does not own. Every place a producer can stop is stepped through here by
//! hand, on a ring of eight slots, since no amount of running threads side
//! by side makes a thread sleep for exactly one lap.

use perfr_scope_ring::event::kind;
use perfr_scope_ring::merge::{drain, Cursors};
use perfr_scope_ring::ring::{self, Rings};
use perfr_scope_ring::{DropTally, ScopeEvent};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SLOTS: u64 = 8;

struct Region {
    bytes: Vec<u8>,
    offset: usize,
}

impl Region {
    fn new() -> Region {
        let mut bytes = vec![0u8; ring::layout_size(1, SLOTS as usize) + ring::CACHE_LINE];
        let offset = bytes.as_ptr().align_offset(ring::CACHE_LINE);
        // SAFETY: the allocation has room for the layout past the alignment.
        unsafe { ring::init_region(bytes.as_mut_ptr().add(offset), 1, SLOTS as usize, 1) };
        Region { bytes, offset }
    }

    fn rings(&self) -> Rings {
        // SAFETY: initialised above at exactly these dimensions.
        unsafe {
            Rings::from_raw(self.bytes.as_ptr().add(self.offset) as *mut u8, 1, SLOTS as usize)
        }
    }
}

/// The record of claim `n`: `n` in every field, so a slot holding bytes of
/// two records cannot pass for either.
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

/// One drain pass: the claims delivered, each checked to be whole, and the
/// number counted lost.
fn pass(rings: &Rings, cursors: &mut Cursors) -> (Vec<u64>, u64) {
    let pass = drain(rings, cursors, 0);
    let claims = pass.slices[0]
        .events
        .iter()
        .map(|event| {
            assert_eq!(*event, stamped(event.timestamp_ns), "a record of two claims was delivered");
            event.timestamp_ns
        })
        .collect();
    (claims, pass.dropped)
}

#[test]
fn a_producer_that_slept_through_a_lap_leaves_the_record_in_its_slot_alone() {
    let region = Region::new();
    let rings = region.rings();
    let mut cursors = Cursors::for_rings(1);

    // Claim 0 is handed out and its thread stops before storing a byte. The
    // ring goes round: claim 8 is written, and committed, into the same slot.
    let stale = rings.reserve_for_test(0);
    assert_eq!(stale, 0);
    for n in 1..=SLOTS {
        rings.push(0, stamped(n));
    }
    assert_eq!(rings.committed(0, SLOTS), Some(stamped(SLOTS)));

    // The thread wakes and finishes what it started.
    assert!(!rings.publish_reserved_for_test(0, stale, stamped(0)), "claim 0 was lapped");
    assert_eq!(
        rings.committed(0, SLOTS),
        Some(stamped(SLOTS)),
        "the record of claim 8 was erased by a commit of claim 0"
    );

    // Claim 0 is the only loss: it was lapped before it was written.
    let (claims, dropped) = pass(&rings, &mut cursors);
    assert_eq!(claims, (1..=SLOTS).collect::<Vec<_>>());
    assert_eq!(dropped, 1);

    // And the reader is not left waiting on a claim that can never commit.
    for n in SLOTS + 1..=SLOTS + 4 {
        rings.push(0, stamped(n));
    }
    let (claims, dropped) = pass(&rings, &mut cursors);
    assert_eq!(claims, (SLOTS + 1..=SLOTS + 4).collect::<Vec<_>>());
    assert_eq!(dropped, 0);
    assert_eq!(cursors.read[0], rings.write_cursor(0), "the reader is level with the producers");
}

#[test]
fn a_producer_that_wakes_while_its_slot_is_being_rewritten_stores_nothing() {
    let region = Region::new();
    let rings = region.rings();
    let mut cursors = Cursors::for_rings(1);

    // Claim 0 stops before storing a byte, as above. This time the thread
    // with claim 8 has taken the slot and is part way through its record
    // when claim 0 wakes: two threads storing into one slot left it holding
    // bytes of both, committed as claim 8.
    let stale = rings.reserve_for_test(0);
    for n in 1..SLOTS {
        rings.push(0, stamped(n));
    }
    let lapping = rings.reserve_for_test(0);
    assert_eq!(lapping, SLOTS);
    assert!(rings.take_reserved_for_test(0, lapping, &stamped(lapping)));

    assert!(!rings.publish_reserved_for_test(0, stale, stamped(stale)), "the slot is not claim 0's");
    assert!(rings.commit_taken_for_test(0, lapping, stamped(lapping)));

    let (claims, dropped) = pass(&rings, &mut cursors);
    assert_eq!(claims, (1..=SLOTS).collect::<Vec<_>>());
    assert_eq!(dropped, 1);
}

#[test]
fn a_claim_whose_slot_a_stopped_producer_still_holds_is_given_up_and_not_waited_for() {
    let region = Region::new();
    let rings = region.rings();
    let mut cursors = Cursors::for_rings(1);
    rings.publish_read_cursors(&cursors.read);

    // Claim 0 has taken its slot and stops part way through its record.
    let stopped = rings.reserve_for_test(0);
    assert!(rings.take_reserved_for_test(0, stopped, &stamped(stopped)));
    for n in 1..SLOTS {
        rings.push(0, stamped(n));
    }
    // It is alive, and its record may yet come: the reader waits.
    assert_eq!(pass(&rings, &mut cursors), (vec![], 0));

    // Claim 8 comes round to the slot and cannot have it. Its record is a
    // stop on thread 77, and is lost; the ring carries on with claim 9.
    rings.push(0, ScopeEvent { kind: kind::SCOPE_STOP, tid: 77, ..stamped(SLOTS) });
    rings.push(0, stamped(SLOTS + 1));

    // Claims 0 and 1 were lapped, and claim 8 will never commit. Nothing
    // else is lost, and nothing behind claim 8 is held back by it.
    let (claims, dropped) = pass(&rings, &mut cursors);
    assert_eq!(claims, vec![2, 3, 4, 5, 6, 7, 9]);
    assert_eq!(dropped, 3);
    assert_eq!(cursors.read[0], rings.write_cursor(0));
    assert!(
        rings.snapshot_drops().contains(&DropTally { tid: 77, kind: kind::SCOPE_STOP, count: 1 }),
        "the record given up is in the breakdown as what it was: {:?}",
        rings.snapshot_drops()
    );

    // The stopped producer wakes and finishes. Its record is a lap stale
    // and nobody reads it; the slot is free again for claim 16.
    assert!(rings.commit_taken_for_test(0, stopped, stamped(stopped)));
    for n in SLOTS + 2..=2 * SLOTS {
        rings.push(0, stamped(n));
    }
    assert_eq!(rings.committed(0, 2 * SLOTS), Some(stamped(2 * SLOTS)));
    assert_eq!(pass(&rings, &mut cursors), ((SLOTS + 2..=2 * SLOTS).collect(), 0));
}

#[test]
fn a_claim_given_up_does_not_stop_the_reader_waiting_for_a_later_one() {
    let region = Region::new();
    let rings = region.rings();
    let mut cursors = Cursors::for_rings(1);

    let stopped = rings.reserve_for_test(0);
    assert!(rings.take_reserved_for_test(0, stopped, &stamped(stopped)));
    for n in 1..=SLOTS {
        rings.push(0, stamped(n)); // claim 8 is given up
    }
    assert!(rings.commit_taken_for_test(0, stopped, stamped(stopped)));
    assert_eq!(pass(&rings, &mut cursors), ((1..SLOTS).collect(), 2));

    // An ordinary producer, slow between its claim and its record: the
    // reader waits for it as it always did, and gets the record.
    let slow = rings.reserve_for_test(0);
    rings.push(0, stamped(slow + 1));
    assert_eq!(pass(&rings, &mut cursors), (vec![], 0), "claim 9 is still coming");
    assert!(rings.publish_reserved_for_test(0, slow, stamped(slow)));
    assert_eq!(pass(&rings, &mut cursors), (vec![slow, slow + 1], 0));
}

#[test]
fn a_claim_in_flight_since_before_one_was_given_up_is_let_go_with_it() {
    let region = Region::new();
    let rings = region.rings();
    let mut cursors = Cursors::for_rings(1);

    // Claim 0 holds its slot; claim 1 has not got as far as taking its own.
    let stopped = rings.reserve_for_test(0);
    assert!(rings.take_reserved_for_test(0, stopped, &stamped(stopped)));
    let slow = rings.reserve_for_test(0);
    for n in 2..=SLOTS {
        rings.push(0, stamped(n)); // claim 8 is given up
    }

    // The ring says only how far claims have been given up, not which. The
    // reader stops waiting for every pending claim up to there, which is
    // the price of never waiting for one that cannot come: claim 0 lapped,
    // claim 1 let go, claim 8 given up.
    assert_eq!(pass(&rings, &mut cursors), ((2..SLOTS).collect(), 3));
    assert_eq!(cursors.read[0], rings.write_cursor(0));

    // Claim 1 does commit in the end, into a slot nobody will read again.
    assert!(rings.publish_reserved_for_test(0, slow, stamped(slow)));
    assert_eq!(pass(&rings, &mut cursors), (vec![], 0));
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The record a thread pushes as its `seq`th: whole by [`stamped`]'s rule,
/// and saying whose it is.
fn of_thread(thread: u64, seq: u64) -> ScopeEvent {
    stamped(thread << 40 | seq)
}

#[test]
fn producers_stopping_mid_push_on_a_lapping_ring_never_corrupt_it_or_stall_the_reader() {
    // The interleavings above, found by threads rather than laid out: two
    // producers push flat out round eight slots while two others stop with
    // a claim in hand, before taking the slot or after, for long enough
    // that the ring has usually gone round. A reader drains throughout and
    // checks every record it is given.
    const FAST: u64 = 2;
    const STOPPING: u64 = 2;
    let region = Region::new();
    let rings = region.rings();
    let stop = AtomicBool::new(false);
    let gave_up = AtomicU64::new(0);
    let mut cursors = Cursors::for_rings(1);
    let mut next_seq = vec![0u64; (FAST + STOPPING) as usize];
    let (mut delivered, mut dropped) = (0u64, 0u64);

    let mut check = |cursors: &mut Cursors| {
        let before = cursors.read[0];
        let pass = drain(&rings, cursors, 0);
        let events = &pass.slices[0].events;
        for event in events {
            assert_eq!(*event, stamped(event.timestamp_ns), "a record of two pushes was delivered");
            let (thread, seq) = ((event.timestamp_ns >> 40) as usize, event.timestamp_ns & ((1 << 40) - 1));
            assert!(
                seq >= next_seq[thread],
                "thread {thread}'s record {seq} was delivered after its record {}",
                next_seq[thread]
            );
            next_seq[thread] = seq + 1;
        }
        assert_eq!(
            events.len() as u64 + pass.dropped,
            cursors.read[0] - before,
            "claims {before}..{} are not all delivered or counted lost",
            cursors.read[0]
        );
        delivered += events.len() as u64;
        dropped += pass.dropped;
    };

    std::thread::scope(|scope| {
        for thread in 0..FAST {
            let (rings, stop) = (&rings, &stop);
            scope.spawn(move || {
                let mut seq = 0;
                while !stop.load(Ordering::Relaxed) {
                    rings.push(0, of_thread(thread, seq));
                    seq += 1;
                }
            });
        }
        for thread in FAST..FAST + STOPPING {
            let (rings, stop, gave_up) = (&rings, &stop, &gave_up);
            scope.spawn(move || {
                let mut seq = 0;
                while !stop.load(Ordering::Relaxed) {
                    let event = of_thread(thread, seq);
                    let claim = rings.reserve_for_test(0);
                    let written = if seq % 2 == 0 {
                        std::thread::yield_now();
                        rings.publish_reserved_for_test(0, claim, event)
                    } else {
                        let taken = rings.take_reserved_for_test(0, claim, &event);
                        std::thread::yield_now();
                        taken && rings.commit_taken_for_test(0, claim, event)
                    };
                    gave_up.fetch_add(u64::from(!written), Ordering::Relaxed);
                    seq += 1;
                }
            });
        }

        // The producers run until told to stop, and the scope waits for
        // them: a check that fails has to stop them on its way out, or the
        // test hangs where it should fail.
        let _stop = StopOnDrop(&stop);
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(400) {
            for _ in 0..64 {
                check(&mut cursors);
            }
        }
    });

    // Every producer has returned, so nothing is in flight: one pass reads
    // the ring through to the write cursor. A claim the reader were still
    // waiting for here is one that will never commit.
    rings.push(0, of_thread(0, u64::MAX >> 24));
    check(&mut cursors);
    assert_eq!(cursors.read[0], rings.write_cursor(0), "the reader is stuck behind the producers");
    assert_eq!(delivered + dropped, rings.write_cursor(0));
    let gave_up = gave_up.load(Ordering::Relaxed);
    println!("{delivered} delivered, {dropped} lost, {gave_up} pushes gave their slot up");
    assert!(gave_up > 0, "no producer ever woke to a slot that had moved on");
}
