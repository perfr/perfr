// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Seqlock slot.
//!
//! A writer marks the sequence odd, copies the value in, then marks it even
//! again (the previous sequence plus two). A reader snapshots the sequence,
//! copies the value, and snapshots the sequence again. The copy is kept only
//! when both snapshots match and the sequence was even. Otherwise the read
//! was torn: retry, or give up and let the caller skip.
//!
//! The sequence lives on its own cache line. The value starts on the next
//! one. `repr(C, align(64))` rounds the slot up to a whole number of cache
//! lines, so a torn copy of the value cannot spill into the next slot.

use std::cell::UnsafeCell;

use crate::sync::{fence, AtomicU64, Ordering};

/// Bytes in a cache line. Slots are aligned and padded to this.
pub const CACHE_LINE: usize = 64;

#[repr(C, align(64))]
pub struct SeqLock<T: Copy> {
    seq: AtomicU64,
    /// Fills the rest of the sequence's cache line.
    _seq_pad: [u8; CACHE_LINE - 8],
    value: UnsafeCell<T>,
}

unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            seq: AtomicU64::new(0),
            _seq_pad: [0; CACHE_LINE - 8],
            value: UnsafeCell::new(value),
        }
    }

    /// Publishes `value`. `during` runs while the sequence is odd, still
    /// inside the write, so a reader either sees the previous publication
    /// or this one — not a mix of the value and whatever `during` stored
    /// beside it.
    ///
    /// One writer per slot. Two overlapping `write_during` calls on the same
    /// slot break the even/odd protocol.
    pub fn write_during(&self, value: T, during: impl FnOnce()) {
        let start = self.seq.load(Ordering::Relaxed);
        debug_assert_eq!(start & 1, 0, "overlapping seqlock writers");
        self.seq.store(start.wrapping_add(1), Ordering::Release);
        fence(Ordering::Release);
        during();
        unsafe {
            self.value.get().write(value);
        }
        fence(Ordering::Release);
        self.seq.store(start.wrapping_add(2), Ordering::Release);
    }

    /// Convenience for tests and the loom model. The ring publishes with
    /// [`Self::write_during`] so the payload pointer moves inside the same
    /// odd sequence.
    #[cfg(test)]
    pub fn write(&self, value: T) {
        self.write_during(value, || {});
    }

    /// Copies the value out. `during` runs on that copy, before the sequence
    /// is checked again, and its result is returned only when the snapshot
    /// was stable. `None` means every attempt raced a writer.
    pub fn read_during<R>(&self, spins: u32, during: impl FnMut(&T) -> R) -> Option<R> {
        let mut during = during;
        for _ in 0..spins.max(1) {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            fence(Ordering::Acquire);
            // Local copy. `during` must not assume it matches a stable write;
            // the second sequence load below is what accepts it.
            let value = unsafe { self.value.get().read() };
            let out = during(&value);
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Acquire);
            if s1 == s2 {
                return Some(out);
            }
        }
        None
    }

    #[cfg(test)]
    pub fn read(&self, spins: u32) -> Option<T> {
        self.read_during(spins, |value| *value)
    }

    #[cfg(all(test, not(feature = "loom")))]
    pub fn seq_addr(&self) -> usize {
        &self.seq as *const AtomicU64 as usize
    }

    #[cfg(all(test, not(feature = "loom")))]
    pub fn value_addr(&self) -> usize {
        self.value.get() as usize
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn value_starts_on_the_next_cache_line_and_slots_do_not_share_one() {
        let lock = SeqLock::new([0u8; 40]);
        assert_eq!(lock.seq_addr() % CACHE_LINE, 0);
        assert_eq!(lock.value_addr() % CACHE_LINE, 0);
        assert!(lock.value_addr() >= lock.seq_addr() + CACHE_LINE);

        let pair = [SeqLock::new([0u8; 40]), SeqLock::new([0u8; 40])];
        let next_seq = pair[1].seq_addr();
        let value_end = pair[0].value_addr() + 40;
        assert!(next_seq >= value_end);
        assert_eq!(next_seq % CACHE_LINE, 0);
        assert_eq!(std::mem::size_of::<SeqLock<[u8; 40]>>() % CACHE_LINE, 0);
    }

    #[test]
    fn round_trip() {
        let lock = SeqLock::new((0u64, 0u64));
        lock.write((7, 7));
        assert_eq!(lock.read(4), Some((7, 7)));
    }

    #[test]
    fn concurrent_readers_never_see_a_mixed_word() {
        let lock = std::sync::Arc::new(SeqLock::new([0u64; 4]));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let lock = std::sync::Arc::clone(&lock);
            let stop = std::sync::Arc::clone(&stop);
            readers.push(std::thread::spawn(move || {
                let mut saw = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) || saw == 0 {
                    if let Some(word) = lock.read(64) {
                        let a = word[0];
                        assert!(word.iter().all(|w| *w == a), "torn seqlock read {word:?}");
                        saw = saw.max(a);
                    }
                    if saw == 2_000 {
                        break;
                    }
                }
                saw
            }));
        }
        for i in 1..=2_000u64 {
            lock.write([i; 4]);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for reader in readers {
            let saw = reader.join().unwrap();
            assert!(saw > 0);
        }
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    struct Word {
        a: u64,
        b: u64,
        c: u64,
        d: u64,
    }

    #[test]
    fn loom_reader_never_observes_a_torn_word() {
        // Four preemptions is enough to put the reader between the odd
        // sequence store and the even one — the torn-read window — without
        // exploring every spin of a longer run.
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(4);
        builder.check(|| {
            let lock = std::sync::Arc::new(SeqLock::new(Word {
                a: 0,
                b: 0,
                c: 0,
                d: 0,
            }));
            let writer_lock = std::sync::Arc::clone(&lock);
            let reader_lock = std::sync::Arc::clone(&lock);
            let writer = loom::thread::spawn(move || {
                writer_lock.write(Word {
                    a: 1,
                    b: 1,
                    c: 1,
                    d: 1,
                });
            });
            let reader = loom::thread::spawn(move || {
                if let Some(word) = reader_lock.read(2) {
                    assert_eq!(word.a, word.b, "torn seqlock word {word:?}");
                    assert_eq!(word.b, word.c);
                    assert_eq!(word.c, word.d);
                }
            });
            writer.join().unwrap();
            reader.join().unwrap();
        });
    }
}
