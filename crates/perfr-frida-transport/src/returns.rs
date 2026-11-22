// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bounded journal of return interceptions, in a separate mapping of the
//! lease file. No live target memory is read by the consumer.
//!
//! It is several rings, and a thread writes to the one its id picks. All
//! that is ever asked of the journal is asked about one thread at a time,
//! so nothing needs the rings in one order; and one ring for every thread
//! is one word every hooked call on every core has to own, which with
//! eight threads in a hook cost each call over a microsecond. A thread
//! that fills its ring also fills nobody else's, short of the few threads
//! that share it.
//!
//! What a record that is not written costs decides everything here:
//!
//! - An **entry** that is not written costs nothing but the frame: the slot
//!   it would have described holds a registered trampoline, and the unwinder
//!   stops there. So entries may be dropped freely, and a hook that has been
//!   muted for being too hot gives up its entries before the journal fills.
//! - A **leave** that is not written leaves its entry open in the consumer's
//!   history, where a later call at the same stack depth could be mistaken
//!   for it. The writer says which thread lost one, and the consumer closes
//!   that thread's open entries at the last instant it knows was complete.
//! - A **taint** (a call through a trampoline the agent could not identify)
//!   that is not written would let a sample be unwound through code nobody
//!   can recognise. That one is not recoverable: `unknown_return` is set and
//!   the capture's samples are leaf-only from then on.
use std::{
    fs::File,
    io,
    os::fd::AsRawFd,
    sync::atomic::{AtomicU64, Ordering},
};

/// Rings, each multi-producer and single-consumer. A power of two.
pub const RINGS: usize = 8;
/// Records one ring holds.
pub const CAPACITY: usize = 4096;
/// Return trampolines one process can have registered. Each hooked function
/// has one for as long as the hook is installed, and an older one stays
/// registered because a call that began under it may not have returned.
pub const TRAMPOLINES: usize = 4096;
const OFFSET: usize = 4096;
/// A droppable entry is written only while this many slots are free, so the
/// hooks that are not muted keep half of each ring to themselves.
const DROPPABLE_ROOM: u64 = CAPACITY as u64 / 2;
/// The consumer publishes its position every so many records, not after
/// each: the writers read that word on every call.
const READ_STRIDE: u64 = 256;

#[repr(C)]
struct Slot {
    sequence: AtomicU64,
    time: AtomicU64,
    tid: AtomicU64,
    sp: AtomicU64,
    original: AtomicU64,
    trampoline: AtomicU64,
    invocation: AtomicU64,
    kind: AtomicU64,
}
/// One cache line to itself: every writer's CAS lands on `write`, and none
/// of them should take the consumer's `read` or the notes with it.
#[repr(C, align(64))]
struct Line<T>(T);
#[repr(C)]
struct Notes {
    /// Records not written because the journal was full. A statistic.
    lost: AtomicU64,
    /// Threads that lost a leave since the consumer last looked: bit
    /// `tid % 64`.
    lost_threads: AtomicU64,
    unknown_return: AtomicU64,
    /// Some call has gone through a trampoline the agent could not name.
    /// Written beside the record, so it is there for a reader that can no
    /// longer read records.
    tainted: AtomicU64,
    closed: AtomicU64,
}
#[repr(C)]
struct Ring {
    write: Line<AtomicU64>,
    read: Line<AtomicU64>,
    slots: [Slot; CAPACITY],
}
#[repr(C)]
struct Shared {
    notes: Line<Notes>,
    trampolines: [AtomicU64; TRAMPOLINES],
    rings: [Ring; RINGS],
}
pub const FILE_SIZE: usize = OFFSET + std::mem::size_of::<Shared>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Leave,
    Enter,
    /// The thread entered a hook whose return trampoline is not known.
    Taint,
    Untaint,
}
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub time: u64,
    pub tid: u32,
    /// Of an entry, where its return slot is. Of a taint, nonzero when it
    /// was carried over from before this journal.
    pub sp: u64,
    pub original: u64,
    pub trampoline: u64,
    pub invocation: u64,
    pub kind: Kind,
}
/// What one pass over the journal found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drained {
    /// The clock when the pass began. With `blocked` none, every record
    /// stamped at or before it has been handed over.
    pub cutoff: u64,
    /// A record that is reserved and not yet written, where the pass over
    /// its ring stopped: the ring and the position in it, as one number.
    pub blocked: Option<u64>,
    /// Bit `tid % 64` for each thread that lost a leave since the last pass.
    pub lost_threads: u64,
}
pub struct Journal {
    base: *mut libc::c_void,
}
// The mapping contains only atomics. One consumer; multiple writers.
unsafe impl Send for Journal {}
unsafe impl Sync for Journal {}
impl Journal {
    /// Grows a new lease file to hold the journal and maps it. The blocks
    /// are allocated here, by the controller: a file that is only given a
    /// length is sparse, and on a full filesystem the first write into it
    /// would fault in whichever thread of the target made the call.
    pub fn create(file: &File) -> io::Result<Self> {
        file.set_len(FILE_SIZE as u64)?;
        let status = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, FILE_SIZE as libc::off_t) };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        Self::open(file)
    }
    /// The controller creates a fresh zero-filled lease before publishing it.
    pub fn open(file: &File) -> io::Result<Self> {
        if file.metadata()?.len() != FILE_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid return journal size",
            ));
        }
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                FILE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { base })
    }
    /// Gives the file's pages back and leaves untouched zero pages at the
    /// same addresses, in one step. For a writer whose journal is finished
    /// but which cannot rule out one last call still on its way in: that
    /// call writes into memory nobody reads, and faults nowhere.
    pub fn discard(&self) -> io::Result<()> {
        let base = unsafe {
            libc::mmap(
                self.base,
                FILE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn shared(&self) -> &Shared {
        // OFFSET is page-aligned; the file was zero-initialized before use.
        unsafe { &*((self.base as *const u8).add(OFFSET).cast::<Shared>()) }
    }
    fn notes(&self) -> &Notes {
        &self.shared().notes.0
    }
    fn ring(&self, tid: u32) -> &Ring {
        &self.shared().rings[tid as usize % RINGS]
    }
    pub fn close(&self) {
        self.notes().closed.store(1, Ordering::Release);
    }
    pub fn closed(&self) -> bool {
        self.notes().closed.load(Ordering::Acquire) != 0
    }
    /// A sample may cross a trampoline this journal cannot name.
    pub fn unknown_return(&self) -> bool {
        self.notes().unknown_return.load(Ordering::Acquire) != 0
    }
    /// Some thread has been inside a hook whose trampoline is not known.
    pub fn tainted(&self) -> bool {
        self.notes().tainted.load(Ordering::Acquire) != 0
    }
    pub fn lost(&self) -> u64 {
        self.notes().lost.load(Ordering::Acquire)
    }
    /// The registered trampolines. They fill the table from the front and
    /// are never removed, so the first empty place ends them.
    pub fn trampolines(&self) -> Vec<u64> {
        self.shared()
            .trampolines
            .iter()
            .map(|v| v.load(Ordering::Acquire))
            .take_while(|v| *v != 0)
            .collect()
    }
    /// Makes a trampoline one the unwinder stops at. Done once, when a hook
    /// first sees its own, and before the slot it describes is replaced.
    /// False when it is not an address or the table is full: the caller
    /// then has a hook whose trampoline nobody can recognise.
    pub fn register(&self, trampoline: u64) -> bool {
        if trampoline == 0 {
            return false;
        }
        for address in &self.shared().trampolines {
            let found = address.load(Ordering::Acquire);
            if found == trampoline
                || (found == 0
                    && match address.compare_exchange(
                        0,
                        trampoline,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => true,
                        Err(actual) => actual == trampoline,
                    })
            {
                return true;
            }
        }
        false
    }
    /// Journals a call into a hooked function, before its return slot is
    /// replaced. The token names the call to `leave`; zero means no entry
    /// was written, and there is then nothing to leave. `droppable` is for
    /// a hook that calls too often to be worth the journal's last half.
    pub fn enter(
        &self,
        tid: u32,
        sp: u64,
        original: u64,
        trampoline: u64,
        droppable: bool,
    ) -> u64 {
        if sp == 0 || original == 0 || trampoline == 0 {
            return 0;
        }
        let room = if droppable { DROPPABLE_ROOM } else { 0 };
        self.push(Kind::Enter, tid, sp, original, trampoline, 0, room)
    }
    pub fn leave(&self, tid: u32, invocation: u64) {
        if invocation != 0 && self.push(Kind::Leave, tid, 0, 0, 0, invocation, 0) == 0 {
            // Its entry stays open in the consumer's history.
            self.notes()
                .lost_threads
                .fetch_or(1 << (tid % 64), Ordering::AcqRel);
        }
    }
    /// The thread is entering a hook whose trampoline is not known: until
    /// `untaint`, a walk of its stack cannot be trusted past the sampled pc.
    pub fn taint(&self, tid: u32) -> u64 {
        self.write_taint(tid, 0)
    }
    /// The same, for a call that began before this journal existed: the
    /// record's own time is not when the thread became untrustworthy.
    pub fn taint_carried(&self, tid: u32) -> u64 {
        self.write_taint(tid, 1)
    }
    fn write_taint(&self, tid: u32, carried: u64) -> u64 {
        self.notes().tainted.store(1, Ordering::Release);
        let token = self.push(Kind::Taint, tid, carried, 0, 0, 0, 0);
        if token == 0 {
            self.notes().unknown_return.store(1, Ordering::Release);
        }
        token
    }
    /// Takes the next place in the journal and writes nothing to it, as a
    /// writer preempted mid-record does. For tests of a reader.
    #[doc(hidden)]
    pub fn reserve_unwritten(&self, tid: u32) -> u64 {
        self.ring(tid).write.0.fetch_add(1, Ordering::AcqRel)
    }
    /// A taint whose end is not written stays: the thread's samples remain
    /// leaf-only, which is the safe side.
    pub fn untaint(&self, tid: u32, token: u64) {
        if token != 0 {
            self.push(Kind::Untaint, tid, 0, 0, 0, token, 0);
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        kind: Kind,
        tid: u32,
        sp: u64,
        original: u64,
        trampoline: u64,
        invocation: u64,
        room: u64,
    ) -> u64 {
        let ring = self.ring(tid);
        let mut write = ring.write.0.load(Ordering::Relaxed);
        loop {
            let used = write.saturating_sub(ring.read.0.load(Ordering::Acquire));
            if room != 0 && used + room >= CAPACITY as u64 {
                // Given up, not lost: nothing in the history depends on it.
                return 0;
            }
            if write >= u64::MAX - 1 || used >= CAPACITY as u64 {
                self.notes().lost.fetch_add(1, Ordering::Relaxed);
                return 0;
            }
            match ring.write.0.compare_exchange_weak(
                write,
                write + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => write = next,
            }
        }
        // Timestamp AFTER reservation. A consumer that takes its clock cutoff
        // before a head snapshot cannot miss an older event reserved later.
        let time = crate::profile::now_ns();
        let slot = &ring.slots[write as usize % CAPACITY];
        slot.time.store(time, Ordering::Relaxed);
        slot.tid.store(tid as u64, Ordering::Relaxed);
        slot.sp.store(sp, Ordering::Relaxed);
        slot.original.store(original, Ordering::Relaxed);
        slot.trampoline.store(trampoline, Ordering::Relaxed);
        slot.invocation.store(
            if invocation == 0 {
                write + 1
            } else {
                invocation
            },
            Ordering::Relaxed,
        );
        slot.kind.store(kind as u64, Ordering::Relaxed);
        slot.sequence.store(write + 1, Ordering::Release);
        write + 1
    }
    /// Hands over every record of every ring, each ring up to the first
    /// record still being written. A thread's records are in the order it
    /// wrote them; two threads' are in no particular order.
    ///
    /// The cutoff is a safe sample-time horizon only when nothing blocked
    /// the pass: a preempted writer holds the old watermark, and never
    /// causes a sample to be read against incomplete history.
    ///
    /// A leave reported lost here was lost after the previous pass looked,
    /// so the history was complete up to the previous pass's cutoff.
    pub fn drain(&self, records: &mut Vec<Record>) -> Drained {
        let cutoff = crate::profile::now_ns();
        let mut blocked = None;
        for (index, ring) in self.shared().rings.iter().enumerate() {
            let head = ring.write.0.load(Ordering::Acquire);
            let mut read = ring.read.0.load(Ordering::Relaxed);
            let published = read;
            while read < head {
                let slot = &ring.slots[read as usize % CAPACITY];
                if slot.sequence.load(Ordering::Acquire) != read + 1 {
                    blocked.get_or_insert(((index as u64) << 56) | (read & ((1 << 56) - 1)));
                    break;
                }
                records.push(Record {
                    time: slot.time.load(Ordering::Relaxed),
                    tid: slot.tid.load(Ordering::Relaxed) as u32,
                    sp: slot.sp.load(Ordering::Relaxed),
                    original: slot.original.load(Ordering::Relaxed),
                    trampoline: slot.trampoline.load(Ordering::Relaxed),
                    invocation: slot.invocation.load(Ordering::Relaxed),
                    kind: match slot.kind.load(Ordering::Relaxed) {
                        0 => Kind::Leave,
                        1 => Kind::Enter,
                        2 => Kind::Taint,
                        _ => Kind::Untaint,
                    },
                });
                read += 1;
                if read % READ_STRIDE == 0 {
                    ring.read.0.store(read, Ordering::Release);
                }
            }
            // Written only when there is something to say: the writers
            // read this word, and a pass that found nothing should not
            // take its line from them.
            if read != published {
                ring.read.0.store(read, Ordering::Release);
            }
        }
        let lost_threads = match self.notes().lost_threads.load(Ordering::Acquire) {
            0 => 0,
            _ => self.notes().lost_threads.swap(0, Ordering::AcqRel),
        };
        Drained {
            cutoff,
            blocked,
            lost_threads,
        }
    }
}
impl Drop for Journal {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, FILE_SIZE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn journal() -> (File, Journal) {
        let f = tempfile::tempfile().unwrap();
        let j = Journal::create(&f).unwrap();
        (f, j)
    }
    #[test]
    fn the_words_every_call_touches_do_not_share_a_cache_line() {
        let (_f, j) = journal();
        let shared = j.shared();
        let at = |field: *const u8| field as usize - std::ptr::from_ref(shared) as usize;
        assert_eq!(std::mem::size_of::<Slot>(), 64);
        let mut lines = std::collections::HashSet::from([at(std::ptr::from_ref(&shared.notes).cast()) / 64]);
        for ring in &shared.rings {
            for word in [
                std::ptr::from_ref(&ring.write).cast::<u8>(),
                std::ptr::from_ref(&ring.read).cast(),
                std::ptr::from_ref(&ring.slots).cast(),
            ] {
                assert_eq!(at(word) % 64, 0);
                assert!(lines.insert(at(word) / 64), "two of them in one line");
            }
        }
        // Threads with neighbouring ids, which is what a pool has, do not
        // share a ring.
        assert!(!std::ptr::eq(j.ring(100), j.ring(101)));
        assert!(std::ptr::eq(j.ring(100), j.ring(100 + RINGS as u32)));
    }
    #[test]
    fn a_new_journal_has_its_blocks_and_not_only_its_length() {
        use std::os::unix::fs::MetadataExt;
        let (f, _j) = journal();
        let metadata = f.metadata().unwrap();
        assert_eq!(metadata.len(), FILE_SIZE as u64);
        assert!(
            metadata.blocks() * 512 >= FILE_SIZE as u64,
            "{} blocks for {FILE_SIZE} bytes: the file is sparse",
            metadata.blocks()
        );
    }
    #[test]
    fn concurrent_publication_and_slot_reuse_are_complete() {
        let (_f, j) = journal();
        for _ in 0..3 {
            let done = std::sync::atomic::AtomicUsize::new(0);
            let mut records = Vec::new();
            std::thread::scope(|s| {
                // Two rings, each with two threads writing to it.
                for tid in [1, 2, 1 + RINGS as u32, 2 + RINGS as u32] {
                    let j = &j;
                    let done = &done;
                    s.spawn(move || {
                        for _ in 0..1000 {
                            let id = j.enter(tid, 0x1000, 0x2000, 0x3000, false);
                            assert_ne!(id, 0);
                            j.leave(tid, id);
                        }
                        done.fetch_add(1, Ordering::Release);
                    });
                }
                while done.load(Ordering::Acquire) != 4 {
                    j.drain(&mut records);
                    std::thread::yield_now();
                }
            });
            let last = j.drain(&mut records);
            assert_eq!(last.blocked, None);
            assert_eq!(last.lost_threads, 0);
            assert_eq!(records.len(), 8000);
            assert_eq!(j.lost(), 0);
            // A token names a call among its own thread's.
            let entries: std::collections::HashSet<_> = records
                .iter()
                .filter(|r| r.kind == Kind::Enter)
                .map(|r| (r.tid, r.invocation))
                .collect();
            assert_eq!(entries.len(), 4000);
            assert!(records
                .iter()
                .filter(|r| r.kind == Kind::Leave)
                .all(|r| entries.contains(&(r.tid, r.invocation))));
        }
    }
    #[test]
    fn a_reserved_but_unpublished_record_blocks_the_watermark_where_it_is() {
        let (_f, j) = journal();
        assert_ne!(j.enter(1, 8, 16, 24, false), 0);
        // A writer that has taken the next place and not yet filled it.
        assert_eq!(j.reserve_unwritten(1), 1);
        // Another thread's ring is not held up by it.
        assert_ne!(j.enter(2, 8, 16, 24, false), 0);
        let mut records = Vec::new();
        let blocked = j.drain(&mut records).blocked;
        assert_eq!(blocked, Some((1 << 56) | 1), "ring 1, position 1");
        assert_eq!(records.len(), 2, "what was written before it, and beside it, is handed over");
        assert_eq!(j.drain(&mut records).blocked, blocked);
    }
    #[test]
    fn the_trampoline_table_fills_from_the_front_and_then_says_no() {
        let (_f, j) = journal();
        assert!(!j.register(0));
        for address in 1..=TRAMPOLINES as u64 {
            assert!(j.register(address));
        }
        assert!(j.register(7), "one already there is still there");
        assert!(!j.register(TRAMPOLINES as u64 + 1));
        assert_eq!(j.trampolines().len(), TRAMPOLINES);
        assert!(
            !j.unknown_return(),
            "a full table is the caller's to act on, not the capture's end"
        );
    }
    #[test]
    fn a_full_journal_drops_entries_quietly_and_names_the_thread_that_lost_a_leave() {
        let (_f, j) = journal();
        // Two threads of one ring, told apart by the bit a loss is named by.
        let (calling, other) = (70, 70 + RINGS as u32);
        let first = j.enter(calling, 8, 16, 24, false);
        for _ in 1..CAPACITY {
            assert_ne!(j.enter(other, 8, 16, 24, false), 0);
        }
        // Full. An entry that does not fit is only a frame nobody can name.
        assert_eq!(j.enter(other, 8, 16, 24, false), 0);
        assert_eq!(j.lost(), 1);
        assert_ne!(j.enter(calling + 1, 8, 16, 24, false), 0, "the next ring has all its room");
        let mut records = Vec::new();
        let drained = j.drain(&mut records);
        assert_eq!(records.len(), CAPACITY + 1);
        assert_eq!(drained.lost_threads, 0);
        // Full again, and this time what does not fit closes an entry.
        for _ in 0..CAPACITY {
            assert_ne!(j.enter(other, 8, 16, 24, false), 0);
        }
        j.leave(calling, first);
        assert_eq!(j.lost(), 2);
        records.clear();
        assert_eq!(j.drain(&mut records).lost_threads, 1 << (calling % 64));
        assert_eq!(j.drain(&mut records).lost_threads, 0, "told once");
        // And the journal goes on: nothing about a loss is permanent.
        let next = j.enter(calling, 8, 16, 24, false);
        assert_ne!(next, 0);
        j.leave(calling, next);
        records.clear();
        j.drain(&mut records);
        assert_eq!(records.len(), 2);
    }
    #[test]
    fn a_muted_hook_leaves_half_of_its_ring_to_the_others() {
        let (_f, j) = journal();
        let (muted, other) = (1, 1 + RINGS as u32);
        let mut written = 0;
        while j.enter(muted, 8, 16, 24, true) != 0 {
            written += 1;
        }
        assert_eq!(written, CAPACITY / 2);
        assert_eq!(j.lost(), 0, "given up is not lost");
        for _ in 0..CAPACITY / 2 {
            assert_ne!(j.enter(other, 8, 16, 24, false), 0);
        }
        assert_eq!(j.enter(other, 8, 16, 24, false), 0);
        assert_eq!(j.lost(), 1);
    }
    #[test]
    fn a_taint_that_cannot_be_written_ends_trust_in_the_capture() {
        let (_f, j) = journal();
        let token = j.taint(9);
        assert_ne!(token, 0);
        j.untaint(9, token);
        let mut records = Vec::new();
        j.drain(&mut records);
        assert_eq!(
            records.iter().map(|r| (r.kind, r.tid, r.invocation)).collect::<Vec<_>>(),
            [(Kind::Taint, 9, token), (Kind::Untaint, 9, token)]
        );
        assert!(!j.unknown_return());
        assert!(j.tainted());
        assert_ne!(j.taint_carried(9), 0);
        records.clear();
        j.drain(&mut records);
        assert_ne!(records[0].sp, 0, "marked as older than the journal");
        for _ in 0..CAPACITY {
            j.enter(9 + RINGS as u32, 8, 16, 24, false);
        }
        assert_eq!(j.taint(9), 0);
        assert!(j.unknown_return());
    }
    #[test]
    fn a_discarded_journal_is_still_memory_and_no_longer_the_file() {
        let (f, j) = journal();
        assert_ne!(j.enter(1, 8, 16, 24, false), 0);
        let reader = Journal::open(&f).unwrap();
        j.discard().unwrap();
        // A late call writes into pages of its own.
        assert_ne!(j.enter(1, 8, 16, 24, false), 0);
        assert_ne!(j.enter(1, 8, 16, 24, false), 0);
        let mut records = Vec::new();
        reader.drain(&mut records);
        assert_eq!(records.len(), 1, "the file has what was written before");
    }
}
