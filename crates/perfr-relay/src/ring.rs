// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One broadcast ring for every viewer.
//!
//! A published batch is stored once, as `Bytes`. Each client keeps a cursor
//! (a sequence, a cumulative event count, and a catch-up flag). Publishing
//! writes one slot and returns. It does not walk the clients, and it does
//! not wait for a slow one.
//!
//! A client that falls more than `coalesce_lag` batches behind, or whose
//! slot has been overwritten, jumps to the newest batch. The events it
//! skipped are counted for that client alone. The bytes it does send are
//! the same allocation every other client sends.

use bytes::Bytes;
use crossbeam_epoch::{self as epoch, Atomic, Owned};

use crate::seqlock::SeqLock;
use crate::sync::{AtomicU64, Ordering};

/// One batch, shared by every client that still has it in range.
struct Payload {
    bytes: Bytes,
    events: u32,
    cumulative: u64,
    seq: u64,
    checksum: u64,
}

/// Copyable metadata covered by the seqlock. The `Bytes` pointer lives in
/// the epoch atomic beside it; a reader accepts the pair only when the
/// sequence snapshots match and this metadata agrees with the payload.
#[repr(C)]
#[derive(Clone, Copy)]
struct Meta {
    seq: u64,
    events: u32,
    _pad: u32,
    cumulative: u64,
    checksum: u64,
    len: u64,
}

impl Meta {
    fn empty() -> Self {
        Self {
            seq: u64::MAX,
            events: 0,
            _pad: 0,
            cumulative: 0,
            checksum: 0,
            len: 0,
        }
    }
}

#[repr(C, align(64))]
struct Slot {
    meta: SeqLock<Meta>,
    payload: Atomic<Payload>,
}

impl Slot {
    fn new() -> Self {
        Self {
            meta: SeqLock::new(Meta::empty()),
            payload: Atomic::null(),
        }
    }
}

/// A batch a cursor just observed. `bytes` is a refcount bump of the
/// published allocation, not a new serialization.
#[derive(Clone)]
pub struct SharedFrame {
    pub bytes: Bytes,
    pub events: u32,
    pub cumulative: u64,
    pub seq: u64,
    pub checksum: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadErr {
    Torn,
    Overwritten,
}

/// What [`BroadcastRing::poll`] found for one cursor.
#[derive(Clone)]
pub enum Poll {
    /// Nothing new at this cursor.
    Empty,
    /// The next batch. The caller commits it with [`Cursor::commit`] after
    /// the bytes have been handed to that client's socket.
    Frame(SharedFrame),
    /// This client skipped `dropped_events`. The next `poll` yields the
    /// batch it should resume from, if it is still in the ring.
    Gap { dropped_events: u64 },
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pos: u64,
    delivered_cumulative: u64,
    catching_up: bool,
}

impl Cursor {
    /// Records that `frame` was actually handed to the client. Gap accounting
    /// uses this, so a send that fails must not commit.
    pub fn commit(&mut self, frame: &SharedFrame) {
        self.delivered_cumulative = frame.cumulative;
    }

    pub fn position(&self) -> u64 {
        self.pos
    }
}

pub struct BroadcastRing {
    slots: Box<[Slot]>,
    mask: u64,
    cap: u64,
    coalesce_lag: u64,
    next: AtomicU64,
    events_total: AtomicU64,
    events_live: AtomicU64,
}

impl BroadcastRing {
    /// `slots` is rounded up to a power of two, and at least 2.
    /// `coalesce_lag` is how many unpublished-to-this-client batches are
    /// allowed to queue before that client jumps to the newest one.
    pub fn new(slots: usize, coalesce_lag: u64) -> Self {
        let cap = (slots.max(2)).next_power_of_two() as u64;
        let mut storage = Vec::with_capacity(cap as usize);
        for _ in 0..cap {
            storage.push(Slot::new());
        }
        Self {
            slots: storage.into_boxed_slice(),
            mask: cap - 1,
            cap,
            coalesce_lag: coalesce_lag.max(1),
            next: AtomicU64::new(0),
            events_total: AtomicU64::new(0),
            events_live: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.cap
    }

    pub fn coalesce_lag(&self) -> u64 {
        self.coalesce_lag
    }

    pub fn head(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// True when `cursor` has not yet observed the newest publication.
    pub fn has_pending(&self, cursor: &Cursor) -> bool {
        cursor.pos < self.head()
    }

    pub fn events_published(&self) -> u64 {
        self.events_total.load(Ordering::Relaxed)
    }

    pub fn events_live(&self) -> u64 {
        self.events_live.load(Ordering::Relaxed)
    }

    /// Bytes occupied by the slots themselves, not by the batch payloads.
    pub fn footprint_bytes(&self) -> u64 {
        (self.slots.len() * std::mem::size_of::<Slot>()) as u64
    }

    /// Cursor placed at the live edge. A new client does not replay history
    /// out of the ring; the server sends the name preamble separately.
    pub fn cursor_at_head(&self) -> Cursor {
        Cursor {
            pos: self.head(),
            delivered_cumulative: self.events_published(),
            catching_up: false,
        }
    }

    /// Cursor at the oldest retained slot, for tests that want every batch.
    pub fn cursor_at_tail(&self) -> Cursor {
        let head = self.head();
        let pos = head.saturating_sub(self.cap);
        Cursor {
            pos,
            delivered_cumulative: 0,
            catching_up: false,
        }
    }

    /// Writes one batch. Single producer: two overlapping calls are not
    /// supported, because a slot's seqlock has one writer.
    ///
    /// The producer's work is one slot write. Client count does not appear
    /// in it.
    pub fn publish(&self, bytes: Bytes, events: u32) -> u64 {
        let seq = self.next.fetch_add(1, Ordering::AcqRel);
        let cumulative = self
            .events_total
            .fetch_add(u64::from(events), Ordering::Relaxed)
            + u64::from(events);
        let checksum = checksum(bytes.as_ref());
        let len = bytes.len() as u64;
        let payload = Payload {
            bytes,
            events,
            cumulative,
            seq,
            checksum,
        };
        let meta = Meta {
            seq,
            events,
            _pad: 0,
            cumulative,
            checksum,
            len,
        };
        // Field borrows stay disjoint so the slot write does not need `self`.
        let events_live = &self.events_live;
        let slot = &self.slots[(seq & self.mask) as usize];
        let guard = &epoch::pin();
        slot.meta.write_during(meta, || {
            let old = slot
                .payload
                .swap(Owned::new(payload), Ordering::Release, guard);
            if let Some(prev) = unsafe { old.as_ref() } {
                events_live.fetch_sub(u64::from(prev.events), Ordering::Relaxed);
                unsafe { guard.defer_destroy(old) };
            }
        });
        events_live.fetch_add(u64::from(events), Ordering::Relaxed);
        seq
    }

    pub fn poll(&self, cursor: &mut Cursor) -> Poll {
        for _ in 0..8 {
            if cursor.catching_up {
                return match self.load(cursor.pos) {
                    Ok(frame) => {
                        cursor.pos = cursor.pos.saturating_add(1);
                        cursor.catching_up = false;
                        Poll::Frame(frame)
                    }
                    Err(LoadErr::Torn) => Poll::Empty,
                    Err(LoadErr::Overwritten) => {
                        cursor.catching_up = false;
                        continue;
                    }
                };
            }
            let head = self.head();
            if cursor.pos >= head {
                return Poll::Empty;
            }
            let lag = head - cursor.pos;
            if lag > self.cap || lag > self.coalesce_lag {
                let oldest = head.saturating_sub(self.cap);
                let target = (head - 1).max(oldest);
                if target < cursor.pos {
                    return Poll::Empty;
                }
                match self.load(target) {
                    Ok(frame) => {
                        let before = frame.cumulative.saturating_sub(u64::from(frame.events));
                        let dropped = before.saturating_sub(cursor.delivered_cumulative);
                        cursor.pos = target;
                        cursor.catching_up = true;
                        return Poll::Gap {
                            dropped_events: dropped,
                        };
                    }
                    Err(LoadErr::Torn) => return Poll::Empty,
                    Err(LoadErr::Overwritten) => continue,
                }
            }
            match self.load(cursor.pos) {
                Ok(frame) => {
                    cursor.pos = cursor.pos.saturating_add(1);
                    return Poll::Frame(frame);
                }
                Err(LoadErr::Torn) => return Poll::Empty,
                Err(LoadErr::Overwritten) => continue,
            }
        }
        Poll::Empty
    }

    /// Drops everything this client has not committed and resumes at the
    /// newest batch. Used when a socket send times out: only this cursor moves.
    pub fn skip_to_head(&self, cursor: &mut Cursor) -> u64 {
        let head = self.head();
        if head == 0 || cursor.pos >= head {
            cursor.catching_up = false;
            return 0;
        }
        let target = head - 1;
        match self.load(target) {
            Ok(frame) => {
                let before = frame.cumulative.saturating_sub(u64::from(frame.events));
                let dropped = before.saturating_sub(cursor.delivered_cumulative);
                cursor.pos = target;
                cursor.catching_up = true;
                dropped
            }
            Err(_) => {
                let dropped = self
                    .events_published()
                    .saturating_sub(cursor.delivered_cumulative);
                cursor.pos = head;
                cursor.catching_up = false;
                dropped
            }
        }
    }

    fn load(&self, seq: u64) -> Result<SharedFrame, LoadErr> {
        let slot = &self.slots[(seq & self.mask) as usize];
        let guard = &epoch::pin();
        for _ in 0..32 {
            let read = slot.meta.read_during(1, |meta| {
                if meta.seq != seq {
                    return Inner::Overwritten;
                }
                let shared = slot.payload.load(Ordering::Acquire, guard);
                if shared.is_null() {
                    return Inner::Torn;
                }
                let Some(payload) = (unsafe { shared.as_ref() }) else {
                    return Inner::Torn;
                };
                if payload.seq != meta.seq
                    || payload.checksum != meta.checksum
                    || payload.bytes.len() as u64 != meta.len
                {
                    return Inner::Torn;
                }
                Inner::Hit(SharedFrame {
                    bytes: payload.bytes.clone(),
                    events: payload.events,
                    cumulative: payload.cumulative,
                    seq: payload.seq,
                    checksum: payload.checksum,
                })
            });
            match read {
                Some(Inner::Hit(frame)) => return Ok(frame),
                Some(Inner::Overwritten) => return Err(LoadErr::Overwritten),
                Some(Inner::Torn) | None => {}
            }
        }
        Err(LoadErr::Torn)
    }

    /// Address of slot 0, for the alignment test.
    pub fn slot_addr(&self, index: usize) -> usize {
        &self.slots[index] as *const Slot as usize
    }
}

enum Inner {
    Hit(SharedFrame),
    Overwritten,
    Torn,
}

/// Cheap mix of the bytes so a test can tell two batches apart. Not a hash
/// meant to resist anyone; a torn mix of two batches fails it.
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut acc = 0x9E37_79B9_7F4A_7C15u64 ^ bytes.len() as u64;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in chunks.by_ref() {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        acc = acc.rotate_left(5) ^ word;
    }
    for (i, byte) in chunks.remainder().iter().enumerate() {
        acc ^= u64::from(*byte) << (8 * i);
    }
    acc
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::seqlock::CACHE_LINE;
    use std::sync::Arc;
    use std::time::Instant;

    fn pattern(seq: u64) -> Bytes {
        let mut body = vec![0u8; 64];
        body[..8].copy_from_slice(&seq.to_le_bytes());
        for (i, b) in body[8..].iter_mut().enumerate() {
            *b = seq.wrapping_add(i as u64) as u8;
        }
        Bytes::from(body)
    }

    #[test]
    fn slots_are_cache_line_aligned_and_do_not_overlap() {
        let ring = BroadcastRing::new(4, 4);
        assert_eq!(ring.capacity(), 4);
        let stride = ring.slot_addr(1) - ring.slot_addr(0);
        assert_eq!(ring.slot_addr(0) % CACHE_LINE, 0);
        assert_eq!(stride % CACHE_LINE, 0);
        assert!(stride >= CACHE_LINE);
    }

    #[test]
    fn two_cursors_share_the_published_bytes() {
        let ring = BroadcastRing::new(8, 8);
        let mut a = ring.cursor_at_tail();
        let mut b = ring.cursor_at_tail();
        let bytes = Bytes::from_static(b"shared-batch");
        let ptr = bytes.as_ptr();
        ring.publish(bytes, 3);
        let Poll::Frame(fa) = ring.poll(&mut a) else {
            panic!("a missed the batch")
        };
        let Poll::Frame(fb) = ring.poll(&mut b) else {
            panic!("b missed the batch")
        };
        assert_eq!(fa.bytes.as_ptr(), ptr);
        assert_eq!(fb.bytes.as_ptr(), ptr);
        assert_eq!(&fa.bytes[..], b"shared-batch");
        assert_eq!(fa.events, 3);
        a.commit(&fa);
        b.commit(&fb);
        assert!(matches!(ring.poll(&mut a), Poll::Empty));
    }

    #[test]
    fn a_slow_cursor_drops_without_stalling_the_publisher() {
        let ring = Arc::new(BroadcastRing::new(8, 2));
        let mut slow = ring.cursor_at_tail();
        let mut fast = ring.cursor_at_tail();
        let mut fast_seqs = Vec::new();
        for i in 0..5u64 {
            ring.publish(pattern(i), 1);
            // Keep the fast client inside the coalesce window.
            if i < 3 {
                match ring.poll(&mut fast) {
                    Poll::Frame(frame) => {
                        fast_seqs.push(frame.seq);
                        fast.commit(&frame);
                    }
                    Poll::Gap { .. } => panic!("fast client coalesced while keeping up"),
                    Poll::Empty => panic!("fast client missed a batch it just published"),
                }
            }
        }
        assert_eq!(fast_seqs, vec![0, 1, 2]);

        let Poll::Gap { dropped_events } = ring.poll(&mut slow) else {
            panic!("slow client should have been coalesced");
        };
        // Five events published, resume on the last one, so four were skipped.
        assert_eq!(dropped_events, 4);
        let Poll::Frame(latest) = ring.poll(&mut slow) else {
            panic!("resume")
        };
        assert_eq!(latest.seq, 4);
        slow.commit(&latest);

        // The fast client is two batches behind (lag == coalesce_lag), so it
        // still walks them instead of jumping.
        let mut rest = Vec::new();
        loop {
            match ring.poll(&mut fast) {
                Poll::Frame(frame) => {
                    rest.push(frame.seq);
                    fast.commit(&frame);
                }
                Poll::Gap { .. } => panic!("fast client was coalesced at the coalesce boundary"),
                Poll::Empty => break,
            }
        }
        assert_eq!(rest, vec![3, 4]);
    }

    #[test]
    fn overwritten_slots_report_a_gap_and_the_publisher_does_not_wait() {
        let ring = Arc::new(BroadcastRing::new(4, 100));
        let mut cursor = ring.cursor_at_tail();
        let started = Instant::now();
        for i in 0..10_000u64 {
            ring.publish(pattern(i), 2);
        }
        assert!(
            started.elapsed().as_secs() < 2,
            "publish blocked on readers"
        );
        match ring.poll(&mut cursor) {
            Poll::Gap { dropped_events } => {
                assert!(dropped_events > 0, "a lapped cursor should report the skip");
            }
            other => panic!(
                "expected a gap, got a frame? {}",
                matches!(other, Poll::Frame(_))
            ),
        }
    }

    #[test]
    fn stress_readers_never_observe_a_torn_batch() {
        let ring = Arc::new(BroadcastRing::new(64, 8));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(std::sync::Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let ring = Arc::clone(&ring);
            let stop = Arc::clone(&stop);
            let release = Arc::clone(&release);
            readers.push(std::thread::spawn(move || {
                // Sit at the tail before the publisher starts, so the cursor
                // is not already at the live edge.
                let mut cursor = ring.cursor_at_tail();
                release.wait();
                let mut got = 0u64;
                loop {
                    match ring.poll(&mut cursor) {
                        Poll::Frame(frame) => {
                            assert_eq!(checksum(frame.bytes.as_ref()), frame.checksum);
                            assert_eq!(frame.bytes.len(), 64);
                            let seq = u64::from_le_bytes(frame.bytes[..8].try_into().unwrap());
                            assert_eq!(seq, frame.seq);
                            for (i, byte) in frame.bytes[8..].iter().enumerate() {
                                assert_eq!(*byte, seq.wrapping_add(i as u64) as u8);
                            }
                            cursor.commit(&frame);
                            got += 1;
                        }
                        Poll::Gap { .. } => {}
                        Poll::Empty => {
                            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                                break;
                            }
                            std::thread::yield_now();
                        }
                    }
                }
                got
            }));
        }
        release.wait();
        for i in 0..20_000u64 {
            ring.publish(pattern(i), 1);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut total = 0u64;
        for reader in readers {
            total += reader.join().unwrap();
        }
        assert!(total > 0, "readers received nothing");
    }
}
