// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The shared-memory layout, and the write path.
//!
//! A fixed number of rings, sixteen by default, with a thread writing to the
//! ring its thread id hashes to, for its whole life ([`ring_for_thread`]).
//! Departures from a textbook SPSC ring, all forced rather than chosen:
//!
//! **Slots are claimed with an atomic bump, not a plain store.** There are
//! more threads than rings, so a ring has several producers. An earlier
//! design had one ring per core, written by whichever thread was on it,
//! which is only single-producer if a thread cannot be preempted between
//! reading its core id and writing. In the kernel that is true; in userspace
//! it is not, and `rseq`, which exists for exactly this, is Linux-only and
//! intrusive. One `fetch_add` on the write cursor makes each ring MPSC
//! instead: still lock-free, ~20 cycles uncontended, and spreading threads
//! over sixteen rings keeps contention near zero anyway.
//!
//! **A slot is taken before it is written.** The bump hands out a claim, and
//! a claim is a slot only until the ring has gone round: a thread descheduled
//! between its bump and its stores, for one whole lap, wakes holding a slot
//! that now belongs to a later claim. So a producer first swaps its claim
//! into the slot's stamp with one compare-exchange, and stores nothing if the
//! slot has moved on. That is a second locked instruction per record, on a
//! cache line the record is about to dirty anyway. It retries only against
//! another claim on the very same slot, which is a producer a whole lap away.
//! [`Rings::push`] has the cases.
//!
//! **The commit is a release store.** Payload stores stay plain, but the
//! store that publishes a slot has to be `Release` or the consumer can see a
//! published slot before its contents. On x86-64 that compiles to an ordinary
//! `mov`; on aarch64, which this codebase also targets, it is one `stlr`.
//! "No fences on the write path" holds on x86 and costs one instruction
//! elsewhere, which is the honest version of the claim.
//!
//! A consequence of MPSC worth stating: records within one ring are *not*
//! perfectly timestamp-ordered across threads, because two producers can read
//! the clock in one order and claim slots in the other. Each *thread's* own
//! events are ordered, which is the only order anything downstream needs --
//! see the module docs in [`crate::merge`].

use crate::event::{ScopeEvent, EVENT_SIZE};
use std::cell::UnsafeCell;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

// The bytes spell "ORBIT_SG", the project's earlier name, which is what a
// hex dump of a segment shows. Changing them would be a new layout.
pub const MAGIC: u64 = 0x4F52_4249_545F_5347;
pub const VERSION: u32 = 2;

/// Cache line, to keep one ring's cursor off another's.
pub const CACHE_LINE: usize = 64;

/// The most rings a segment may declare. A hard limit, for validating a
/// header written by another process -- not a recommendation.
pub const MAX_RINGS: usize = 128;

/// How many rings a process actually opens.
///
/// Sixteen, and the number is a knee rather than a preference. Rings trade
/// two things off. More of them means less contention when several threads
/// claim at once; fewer of them means each gets a larger share of the memory
/// budget, which is what decides how long a single busy thread can run before
/// it laps its ring.
///
/// Measured at 32 threads claiming flat out, the cost of a whole scope is
/// 22.9 ns on one ring, 14.0 on eight, 12.9 on sixteen and 12.5 on 128. Most
/// of the benefit is gone by eight and all of it by sixteen. Meanwhile at an
/// 8 MiB budget, 128 rings leaves each with 1024 slots -- a single hot thread
/// laps after a thousand events -- against 8192 slots at sixteen.
///
/// So sixteen buys 95% of the contention win and eight times the headroom
/// against the far more likely failure, which is one thread doing most of the
/// work while the other rings sit empty.
pub const DEFAULT_RING_COUNT: usize = 16;

/// The control block, alone on the first page of the segment.
///
/// It is on its own page for one reason: the service maps it read-write to
/// set `capturing`, and maps the rings read-only. Because nothing but this
/// header lives on the page, a service that mishandled its writable view
/// still could not touch a single ring slot. `version` and the layout fields
/// let a reader validate a segment written by another process, possibly a
/// different build; `api_version` is the semantic version of the calls, bumped
/// when their meaning changes rather than when the record layout does.
#[repr(C)]
pub struct Header {
    pub magic: AtomicU64,
    /// Layout version; a reader refuses a segment it does not match. Bumped to
    /// 2 when the header moved onto its own page and gained the capturing and
    /// api_version fields, which changed every ring offset.
    pub version: u32,
    pub ring_count: u32,
    pub slots_per_ring: u32,
    pub event_size: u32,
    pub pid: u32,
    /// How many readers are draining this segment right now; the producer
    /// writes while it is non-zero. A producer reads it before doing any
    /// work, so an instrumented process that nobody is capturing pays a
    /// single relaxed load per call instead of writing a record no one will
    /// ever read.
    ///
    /// A count, not a flag: every perfr on the machine opens every
    /// instrumented segment, the other services' own included, and a flag
    /// cleared by whichever capture stopped first silenced the rest -- a
    /// service's self-scopes vanished mid-capture when another service on
    /// the box stopped one. Readers move it through [`ReaderLeases`].
    pub capturing: AtomicU32,
    /// Semantic version of the instrumentation calls.
    pub api_version: u32,
    /// Non-zero once the process has made itself a new segment, after a
    /// shutdown and a second init: nothing more will be written here, and a
    /// reader should open the segment the name leads to now. In bytes that
    /// were reserved and always zero, so no offset moves. A reader from
    /// before the field never looks, and goes on reading a segment that
    /// has gone quiet.
    pub superseded: AtomicU32,
    /// In-process API descriptor address, published by perfr-api. Never
    /// dereferenced by the service. Zero in older producers; offsets and size
    /// are unchanged because this occupies previously reserved header bytes.
    pub api_descriptor: AtomicU64,
    pub _pad: [u32; 4],
}

/// Who holds a count in [`Header::capturing`]: one slot per reader, the
/// reader's pid while it captures, zero when free. It sits right after the
/// header on the control page, in bytes that were always zero and that no
/// producer reads, so the layout version and every ring offset are
/// unchanged. The table exists for one case: a reader that dies without
/// stopping. The next reader to take a lease finds its pid gone, frees the
/// slot and takes its count back, where a leaked count would keep the
/// producer writing for no one until it exits.
#[repr(C)]
pub struct ReaderLeases {
    pub pids: [AtomicU32; READER_SLOTS],
}

impl ReaderLeases {
    /// Frees the slots of readers that exited while capturing, returning
    /// their counts to `capturing`. Each slot is freed by exactly one
    /// compare-exchange, so a count is returned once however many parties
    /// sweep at the same time.
    pub(crate) fn free_dead(&self, capturing: &AtomicU32) {
        for pid in &self.pids {
            let holder = pid.load(Ordering::Acquire);
            if holder != 0
                && !crate::platform::process_alive(holder)
                && pid.compare_exchange(holder, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok()
            {
                drop_count(capturing);
            }
        }
    }
}

/// One reader's count in a segment's [`Header::capturing`], and where in
/// the lease table it is written down.
///
/// Here, and not in the two files that open a segment, because none of it
/// is an operating-system call: both had their own copy, and a fix to one
/// was a fix to remember for the other.
pub(crate) struct ReaderLease {
    /// `NO_LEASE`, a 1-based slot in the table, or `UNSLOTTED_LEASE` when
    /// the table was full. Behind a lock, because a reader is shared and
    /// taking a lease is several steps: two threads starting one reader at
    /// once both found it without a lease, both took a count, and one of
    /// the two was never returned.
    held: std::sync::Mutex<u32>,
}

const NO_LEASE: u32 = 0;
const UNSLOTTED_LEASE: u32 = u32::MAX;

impl ReaderLease {
    pub(crate) fn new() -> ReaderLease {
        ReaderLease { held: std::sync::Mutex::new(NO_LEASE) }
    }

    /// Takes this reader's count, if it has none. `header` and `leases`
    /// are the reader's writable view of the control page.
    pub(crate) fn take(&self, header: &Header, leases: &ReaderLeases) {
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *held != NO_LEASE {
            return;
        }
        leases.free_dead(&header.capturing);
        // The count before the slot. A reader that dies between the two
        // then leaves a count nobody takes back, and a producer writing for
        // no one, which is what a reader with no slot risks anyway. The
        // other order left a slot with no count behind it: the next sweep
        // took a count away for it, which was some live reader's, and that
        // reader's producer could go quiet under it.
        header.capturing.fetch_add(1, Ordering::Release);
        let me = std::process::id();
        *held = UNSLOTTED_LEASE;
        for (slot, pid) in leases.pids.iter().enumerate() {
            if pid.compare_exchange(0, me, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                *held = slot as u32 + 1;
                break;
            }
        }
    }

    /// Returns this reader's count, if it holds one.
    pub(crate) fn give_back(&self, header: &Header, leases: &ReaderLeases) {
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match std::mem::replace(&mut *held, NO_LEASE) {
            NO_LEASE => {}
            UNSLOTTED_LEASE => drop_count(&header.capturing),
            slot => {
                let pid = &leases.pids[slot as usize - 1];
                // Only the slot's holder returns its count: if another reader
                // already freed it as dead (a recycled pid), it took the count.
                if pid.compare_exchange(std::process::id(), 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                    drop_count(&header.capturing);
                }
            }
        }
    }
}

/// Takes one reader's count out of `capturing`. Never below zero: a reader
/// built before the count (a plain store of 0 or 1) may have reset it under
/// the leases.
pub(crate) fn drop_count(capturing: &AtomicU32) {
    let _ = capturing.fetch_update(Ordering::Release, Ordering::Relaxed, |n| n.checked_sub(1));
}

/// How many claims a ring hands out between a producer's looks at whether
/// its readers are still alive: often enough that a process is not left
/// recording for a dead service for long, rarely enough that the liveness
/// check, a system call per reader, is nothing against the records between.
const REAP_EVERY: u64 = 1 << 16;

/// Readers that can hold a lease at once. More than that still capture;
/// they just count without a slot, so a crash of theirs is not undone.
pub const READER_SLOTS: usize = 64;

/// Byte offset of [`ReaderLeases`] in the segment, on the control page.
pub const READER_LEASES_OFFSET: usize = CACHE_LINE;

const _: () = assert!(READER_LEASES_OFFSET + std::mem::size_of::<ReaderLeases>() <= 4096);

/// Semantic version of the API. Bumped when the meaning of a call changes.
///
/// 2: [`crate::event::kind::LOG`] records carry log lines, and the producer
/// counts what it overwrites in the control page's drop table (see
/// [`crate::meta`]). The byte layout of the record and the segment is
/// unchanged from version 1, so a reader accepts either.
pub const API_VERSION: u32 = 2;

const _: () = assert!(std::mem::size_of::<Header>() == CACHE_LINE);

/// Per-ring control block, one cache line so neighbouring rings never share.
#[repr(C, align(64))]
pub struct RingHeader {
    /// Monotonically increasing claim counter. It never wraps back, so a slot
    /// index is `claim % slots_per_ring` and the claim number itself doubles
    /// as the sequence stamp.
    ///
    /// Bumped *before* the payload is written, because a claim has to be
    /// handed out before there is anywhere to write.
    pub write_cursor: AtomicU64,
    /// One past the highest claim a producer gave up without writing,
    /// because an older claim's writer still held the slot (see
    /// [`Rings::push`]). Such a claim never commits, and a reader that is
    /// stopped on a claim below this stops waiting for it. In bytes that
    /// were always zero and that no older build reads or writes, so the
    /// layout version is unchanged: an older producer never gives a claim
    /// up, and an older reader waits on one as it always has.
    pub abandoned: AtomicU64,
    pub _pad: [u64; 6],
}

const _: () = assert!(std::mem::size_of::<RingHeader>() == CACHE_LINE);

/// One slot: a sequence stamp and the event.
///
/// `seq` is `claim + 1` once committed, so zero means never written and a
/// stale value from a previous lap is distinguishable from the current one.
/// While the producer of `claim` is writing the record it is that number
/// with [`WRITING`] set, which no reader takes for a committed claim.
#[repr(C, align(64))]
pub struct Slot {
    pub seq: AtomicU64,
    /// `UnsafeCell` because two threads hold `&Slot` while one writes: the
    /// producer that claimed this index, and the consumer reading it. The
    /// `seq` handshake, and the look at the cursor in `Rings::read_claim`
    /// behind it, is what makes that safe, and `UnsafeCell` is how you
    /// say so to the compiler -- writing through a `&T` cast to `*mut T` is
    /// undefined behaviour regardless of any handshake.
    pub event: UnsafeCell<ScopeEvent>,
}

/// Set in a slot's stamp from the moment a producer takes the slot until it
/// commits. A build from before the bit existed never sets it, and reads a
/// stamp that carries it as one more value that is not the claim it wants.
/// Claims are counted from zero per ring, so the bit is free: at a hundred
/// million records a second a ring takes three thousand years to reach it.
const WRITING: u64 = 1 << 63;

const _: () = assert!(8 + EVENT_SIZE == CACHE_LINE);

const _: () = assert!(std::mem::size_of::<Slot>() == CACHE_LINE);

/// Bytes reserved for the control page, ahead of the rings.
///
/// A whole page rather than a cache line, so the service can map it
/// read-write to set `capturing` while mapping the rings read-only, and the
/// two mappings never overlap a byte of ring data. Page-sized so the rings
/// that follow begin at a page boundary, which the split mapping needs.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn control_bytes() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) is always safe and never fails here.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    page.max(CACHE_LINE)
}

// Fixed across native ARM64, Intel, and Rosetta processes on the same Mac.
// A 16 KiB control region keeps ring data off the writable control mapping
// on both 4 KiB and 16 KiB hosts.
#[cfg(target_os = "macos")]
pub fn control_bytes() -> usize { 16 * 1024 }

// Windows pages are 4 KiB on every architecture the OS ships for (x64 and
// arm64 both), and the control page is mapped at offset zero, so the 64 KiB
// allocation granularity that constrains view *offsets* never applies here.
// A constant keeps the write path free of a GetSystemInfo call.
#[cfg(windows)]
pub fn control_bytes() -> usize { 4096 }

/// Total bytes for a mapping with these dimensions.
pub fn layout_size(ring_count: usize, slots_per_ring: usize) -> usize {
    control_bytes() + ring_count * CACHE_LINE + ring_count * slots_per_ring * CACHE_LINE
}

/// As [`layout_size`], for dimensions read out of another process's header:
/// `None` when they describe more bytes than an address can hold. Plain
/// arithmetic wraps there, on a 32-bit reader, and a wrapped size is a small
/// one that a small file then passes for.
pub fn checked_layout_size(ring_count: usize, slots_per_ring: usize) -> Option<usize> {
    let headers = ring_count.checked_mul(CACHE_LINE)?;
    let slots = ring_count.checked_mul(slots_per_ring)?.checked_mul(CACHE_LINE)?;
    control_bytes().checked_add(headers)?.checked_add(slots)
}

/// Byte offset of a ring's control block.
pub fn ring_header_offset(ring: usize) -> usize {
    control_bytes() + ring * CACHE_LINE
}

/// Byte offset of a ring's slot array.
pub fn slots_offset(ring_count: usize, slots_per_ring: usize, ring: usize) -> usize {
    control_bytes() + ring_count * CACHE_LINE + ring * slots_per_ring * CACHE_LINE
}

/// Which ring a thread writes to. The only policy there is.
///
/// One rule for every thread, with no fast path and no fallback. An earlier
/// version handed out exclusive rings while any were free and sent the rest
/// to a shared pool, which was two policies wearing one name: with a bounded
/// number of rings and an unbounded number of threads, sharing is guaranteed
/// by pigeonhole, so exclusivity was never a policy but a lucky case.
///
/// Hashing is also *stateless*, which matters more than the arithmetic. A
/// claim has to be released when a thread exits, and this code lives inside
/// somebody else's application, where a thread-local destructor may run late
/// or not at all. An application that spawns thousands of short-lived threads
/// would have leaked rings until the pool was gone. Nothing to leak here.
///
/// The thread id is mixed before the remainder is taken, never
/// `tid % ring_count`. Thread ids are not arbitrary numbers: they are handed
/// out in runs, and Windows hands them out in multiples of four. This code
/// compiles into the profiled application, so Windows is not hypothetical,
/// and on multiples of four a plain modulo over sixteen rings reaches four of
/// them and leaves twelve idle. Multiplying by the 64-bit golden-ratio constant and
/// taking the *high* bits fixes it: the multiply pushes low-bit structure
/// upward and the shift discards the low bits entirely.
pub const fn ring_for_thread(tid: u64, ring_count: usize) -> usize {
    let ring_count = if ring_count == 0 { 1 } else { ring_count };
    let mixed = (tid.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize;
    mixed % ring_count
}

/// How many rings to open for a process expected to record from `threads`
/// threads.
///
/// One per thread up to [`DEFAULT_RING_COUNT`], then no more: past sixteen the
/// contention saved is under half a nanosecond a scope, while every extra ring
/// takes memory away from the ones doing the work. Threads beyond that share,
/// which every ring is built to survive.
pub const fn ring_count_for_threads(threads: usize) -> usize {
    if threads < 2 {
        2
    } else if threads > DEFAULT_RING_COUNT {
        DEFAULT_RING_COUNT
    } else {
        threads
    }
}

/// Slots per ring for a total budget, rounded down to a power of two.
///
/// The budget is the honest knob. Per-ring size stopped being one the moment
/// rings became per-thread: the interesting number is what the whole segment
/// costs the profiled process, and dividing that by the ring count is how you
/// get there. A ring that is too small laps and says so; a segment that is
/// too big is a tax on every run.
pub const fn slots_for_budget(ring_count: usize, total_bytes: usize) -> usize {
    let ring_count = if ring_count == 0 { 1 } else { ring_count };
    let per_ring = total_bytes / ring_count;
    let slots = per_ring / CACHE_LINE;
    if slots < 2 {
        return 2;
    }
    // Rounding *up* to a power of two would blow the budget, so round down.
    1usize << (usize::BITS - 1 - slots.leading_zeros()) as usize
}

/// The write path over an already-mapped region.
///
/// # Safety
/// `base` must point at a mapping of at least `layout_size(ring_count,
/// slots_per_ring)` bytes that was initialised by [`init_region`], and must
/// outlive every `Rings` built from it.
pub struct Rings {
    base: *mut u8,
    ring_count: usize,
    slots_per_ring: usize,
    /// Where the ring headers and the slots begin, worked out once. Both sit
    /// behind [`control_bytes`], which on Linux is a `sysconf` call the
    /// compiler can neither inline nor merge: asking it on every access put
    /// two libc calls, about as dear as the claim itself, into each `push`.
    headers: *const RingHeader,
    slots: *const Slot,
    /// `slots_per_ring - 1` when that is a power of two, which every segment
    /// this crate creates or agrees to open is; a claim's slot is then one
    /// AND instead of a division by a number the compiler cannot see. Zero
    /// for any other size, which only a hand-built region has, and which
    /// keeps the remainder.
    slot_mask: u64,
}

// SAFETY: every access goes through atomics or through slots this thread has
// exclusively claimed, so the pointer may be shared across threads.
unsafe impl Send for Rings {}
unsafe impl Sync for Rings {}

/// Writes the header and zeroes the cursors.
///
/// # Safety
/// `base` must point at a writable mapping of at least the layout size.
pub unsafe fn init_region(
    base: *mut u8,
    ring_count: usize,
    slots_per_ring: usize,
    pid: u32,
) {
    std::ptr::write_bytes(base, 0, layout_size(ring_count, slots_per_ring));
    init_zeroed_region(base, ring_count, slots_per_ring, pid);
}

/// As [`init_region`], for a region the caller knows to be all zero already:
/// a file the operating system has just created and extended.
///
/// Storing zeroes into such a file is not free. It touches every page, and a
/// shared mapping backs every page that is touched: the default segment went
/// from one resident page to all 32 MiB of tmpfs, 13 to 22 ms inside the
/// host's `perfr_init()`, for a process nobody might ever capture. On a
/// `/dev/shm` without that much room the store that found it out was the
/// memset itself, as a SIGBUS in the application.
///
/// # Safety
/// As [`init_region`], and every byte of the layout must read as zero.
pub(crate) unsafe fn init_zeroed_region(
    base: *mut u8,
    ring_count: usize,
    slots_per_ring: usize,
    pid: u32,
) {
    let header = base.cast::<Header>();
    (*header).version = VERSION;
    (*header).ring_count = ring_count as u32;
    (*header).slots_per_ring = slots_per_ring as u32;
    (*header).event_size = EVENT_SIZE as u32;
    (*header).pid = pid;
    (*header).api_version = API_VERSION;
    // capturing is left at the zero the region came with: a fresh segment is
    // not being captured until a service says so.

    // Magic last and with release ordering: a consumer that sees the magic
    // must see a fully written header behind it.
    (*header).magic.store(MAGIC, Ordering::Release);
}

impl Rings {
    /// # Safety
    /// See the type-level contract.
    pub unsafe fn from_raw(base: *mut u8, ring_count: usize, slots_per_ring: usize) -> Rings {
        let slot_mask =
            if slots_per_ring.is_power_of_two() { slots_per_ring as u64 - 1 } else { 0 };
        Rings {
            base,
            ring_count,
            slots_per_ring,
            headers: base.add(ring_header_offset(0)).cast::<RingHeader>(),
            slots: base.add(slots_offset(ring_count, slots_per_ring, 0)).cast::<Slot>(),
            slot_mask,
        }
    }

    pub fn ring_count(&self) -> usize {
        self.ring_count
    }

    pub fn slots_per_ring(&self) -> usize {
        self.slots_per_ring
    }

    fn ring_header(&self, ring: usize) -> &RingHeader {
        // SAFETY: ring < ring_count by the caller's contract, and the offset
        // is inside the mapping.
        unsafe { &*self.headers.add(ring) }
    }

    fn slot(&self, ring: usize, claim: u64) -> &Slot {
        let index = if self.slot_mask != 0 {
            claim & self.slot_mask
        } else {
            claim % self.slots_per_ring as u64
        } as usize;
        // SAFETY: index is masked into the ring's own slot array.
        unsafe { &*self.slots.add(ring * self.slots_per_ring + index) }
    }

    /// Appends one event. Never blocks and never fails; when the ring is full
    /// the oldest unread slot is overwritten, which the consumer detects.
    ///
    /// The whole write path: one atomic bump for the claim, one
    /// compare-exchange to take the slot, plain stores, one release store.
    /// If a consumer has published its read cursor and this claim will
    /// overwrite a slot at or past it, the event still sitting in that slot
    /// is counted against its own thread and kind before the bytes are
    /// replaced. The count lives on the control page, so a lap cannot erase
    /// what was lost. It is a breakdown for the consumer's own lap count, not
    /// a second count of the loss: see [`crate::meta`].
    ///
    /// The slot is taken, not just written, because the claim alone does not
    /// make it this producer's. A thread can be descheduled holding claim
    /// `c` for as long as the rest of its ring needs to go all the way
    /// round, 33 ms at a million records a second, and wake to find the
    /// slot given to claim `c + capacity`. Its stores then landed on that
    /// record, whole or half written, and its commit put the stamp of `c`
    /// on the slot, which the reader took for "claim `c + capacity` is not
    /// committed yet" and waited on, holding back everything behind it in
    /// the ring. So the record is given up instead, in two cases:
    ///
    /// * The slot already belongs to a later claim. This one was lapped
    ///   before it wrote, and the reader counts it with the rest of the lap
    ///   it has lost, by the cursor.
    /// * The slot is still being written by an earlier claim, whose thread
    ///   is the one that stopped. Taking it would mix two records. This
    ///   claim is not yet a lap behind, so the reader would wait for it:
    ///   [`RingHeader::abandoned`] tells it not to.
    pub fn push(&self, ring: usize, event: ScopeEvent) {
        let ring = ring.min(self.ring_count.saturating_sub(1));
        let claim = self.claim(ring);
        if claim % REAP_EVERY == REAP_EVERY - 1 {
            self.free_dead_readers();
        }
        self.note_overwrite(ring, claim);
        let slot = self.slot(ring, claim);
        if self.take_slot(slot, ring, claim, event.tid, event.kind) {
            Rings::commit(slot, claim, event);
        }
    }

    #[inline]
    fn claim(&self, ring: usize) -> u64 {
        // The strongest ordering, and not for the count's sake: `fetch_add`
        // hands each claim out once whatever the ordering. It is for the
        // reader this claim laps, which goes by the cursor to tell that its
        // slot has been handed on, and has to see the bump no later than
        // anything this producer does to the slot. `read_claim` has the
        // argument. On x86-64 every ordering is the same `lock xadd`; on
        // aarch64 this is `ldaddal` where a relaxed bump would be `ldadd`.
        self.ring_header(ring).write_cursor.fetch_add(1, Ordering::SeqCst)
    }

    /// Takes back the count of every reader that has died. A service
    /// killed mid-capture cannot return its own, and until now only the
    /// next reader to start capturing this segment did it: with no such
    /// reader, every process the dead service had been reading went on
    /// recording at full cost for the rest of its life.
    #[cold]
    fn free_dead_readers(&self) {
        // SAFETY: only a producer pushes, its mapping is writable, and the
        // header and the lease table are on its control page.
        let (header, leases) = unsafe {
            (&*self.base.cast::<Header>(), &*self.base.add(READER_LEASES_OFFSET).cast::<ReaderLeases>())
        };
        if header.capturing.load(Ordering::Relaxed) != 0 {
            leases.free_dead(&header.capturing);
        }
    }

    /// Makes `slot`, which is the slot of `claim`, this producer's to
    /// write, or says it is not to be written. `tid` and `kind` are the
    /// record's, and are only what to count as lost. They come by value:
    /// handed the record by reference, the compiler built it on the stack
    /// for the sake of the one call in a million that reads it, and copied
    /// it into the slot from there, at 4 ns a record.
    ///
    /// A stamp only ever moves to a later claim, and a stamp with
    /// [`WRITING`] set is changed by nobody but the producer that set it.
    /// Between them those two rules mean one producer at a time stores into
    /// a slot, and none stores into one that a later claim has had. The
    /// loop goes round again only when another claim on this same slot, a
    /// whole lap away, moved the stamp between the load and the exchange.
    #[inline]
    fn take_slot(&self, slot: &Slot, ring: usize, claim: u64, tid: u32, kind: u8) -> bool {
        let mine = claim + 1;
        let mut seen = slot.seq.load(Ordering::Relaxed);
        loop {
            if (seen & !WRITING) >= mine {
                return false;
            }
            if seen & WRITING != 0 {
                self.abandon(ring, claim, tid, kind);
                return false;
            }
            // `SeqCst` for the same reason as the claim: on aarch64 it is
            // what orders the exchange ahead of the record's plain stores.
            match slot.seq.compare_exchange(seen, mine | WRITING, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => seen = now,
            }
        }
    }

    /// Gives up a claim whose slot an earlier claim is still writing.
    #[cold]
    fn abandon(&self, ring: usize, claim: u64, tid: u32, kind: u8) {
        // Counted first, so the breakdown is there by the time a reader can
        // act on the mark below and book the loss.
        if self.published_read(ring).is_some() {
            // SAFETY: only a producer gets here, and its mapping is
            // writable, with the drop table on its control page.
            unsafe { crate::meta::note_drop(self.base, tid, kind) }
        }
        self.ring_header(ring).abandoned.fetch_max(claim + 1, Ordering::SeqCst);
    }

    /// Writes the record into a slot [`Rings::take_slot`] said was ours.
    #[inline]
    fn commit(slot: &Slot, claim: u64, event: ScopeEvent) {
        // SAFETY: the slot is ours alone until the stamp below says
        // otherwise, so writing the payload needs no further
        // synchronisation. The release store is what makes it visible.
        unsafe {
            std::ptr::write_volatile(slot.event.get(), event);
        }
        slot.seq.store(claim + 1, Ordering::Release);
    }

    /// One past the highest claim on `ring` that was given up unwritten.
    pub(crate) fn abandoned(&self, ring: usize) -> u64 {
        self.ring_header(ring).abandoned.load(Ordering::Acquire)
    }

    /// Counts the event about to be overwritten, if this claim laps the
    /// consumer. The producer mapping is writable, which is the only place
    /// this runs. The old slot is read by its stamp alone: the cursor a
    /// reader goes by already stands past this claim, and this claim is the
    /// one that overwrites the slot next.
    fn note_overwrite(&self, ring: usize, claim: u64) {
        let Some(read) = self.published_read(ring) else {
            return;
        };
        let capacity = self.slots_per_ring as u64;
        if capacity == 0 || claim.saturating_sub(read) < capacity {
            return;
        }
        let old_claim = claim - capacity;
        // SAFETY: `base` is the producer's writable mapping, and the drop
        // table sits on its control page.
        unsafe {
            match self.copy_if_stamped(ring, old_claim) {
                Some(old) => crate::meta::note_drop(self.base, old.tid, old.kind),
                None => crate::meta::note_drop(self.base, 0, crate::meta::DROP_KIND_UNKNOWN),
            }
        }
    }

    /// The consumer's published read claim for `ring`, if it has said.
    pub fn published_read(&self, ring: usize) -> Option<u64> {
        // SAFETY: the control page is inside the mapping `base` points at.
        unsafe { crate::meta::published_read(self.base, ring) }
    }

    /// Publishes read claims. Tests and the producer use this. A service
    /// reader must publish through its writable control mapping instead:
    /// its ring mapping is read-only.
    pub fn publish_read_cursors(&self, reads: &[u64]) {
        for (ring, read) in reads.iter().copied().enumerate() {
            // SAFETY: same mapping as `published_read`.
            unsafe { crate::meta::publish_read_cursor(self.base, ring, read) }
        }
    }

    /// Drop totals the producer has recorded, per thread and record kind.
    pub fn snapshot_drops(&self) -> Vec<crate::meta::DropTally> {
        // SAFETY: the drop table is on the control page of this mapping.
        unsafe { crate::meta::snapshot_drops(self.base) }
    }

    /// Claims a slot, but never commits it: a producer descheduled
    /// mid-write. Test-only, and the only way to construct that
    /// interleaving deterministically.
    #[doc(hidden)]
    pub fn reserve_for_test(&self, ring: usize) -> u64 {
        self.claim(ring)
    }

    /// Finishes the push of a claim from `reserve_for_test`, as a producer
    /// would on waking from whatever stalled it. Says whether the record
    /// was written. Test-only.
    #[doc(hidden)]
    pub fn publish_reserved_for_test(&self, ring: usize, claim: u64, event: ScopeEvent) -> bool {
        self.take_reserved_for_test(ring, claim, &event)
            && self.commit_taken_for_test(ring, claim, event)
    }

    /// The first half of `publish_reserved_for_test`: a producer that got
    /// as far as taking its slot. Test-only.
    #[doc(hidden)]
    pub fn take_reserved_for_test(&self, ring: usize, claim: u64, event: &ScopeEvent) -> bool {
        self.take_slot(self.slot(ring, claim), ring, claim, event.tid, event.kind)
    }

    /// The second half, for a claim `take_reserved_for_test` was given the
    /// slot of. Test-only.
    #[doc(hidden)]
    pub fn commit_taken_for_test(&self, ring: usize, claim: u64, event: ScopeEvent) -> bool {
        Rings::commit(self.slot(ring, claim), claim, event);
        true
    }

    /// The claim counter, for the consumer.
    pub fn write_cursor(&self, ring: usize) -> u64 {
        self.ring_header(ring).write_cursor.load(Ordering::Acquire)
    }

    /// The event in a slot if it is committed for `claim` and no producer
    /// has since been handed the slot to overwrite, else `None`.
    ///
    /// `None` is either of two things, not yet committed or lapped. The drain
    /// has to tell them apart and reads through `read_claim`, which says.
    pub fn committed(&self, ring: usize, claim: u64) -> Option<ScopeEvent> {
        match self.read_claim(ring, claim) {
            Claimed::Committed(event) => Some(event),
            Claimed::Pending | Claimed::Overwritten => None,
        }
    }

    /// What a reader finds at `claim`: its record, nothing yet, or a slot
    /// that has gone to a later lap.
    ///
    /// The slot's stamp cannot vouch for a copy on its own. A producer
    /// writes the record first and the stamp last, so while the producer of
    /// `claim + capacity` is rewriting this slot the stamp still reads
    /// `claim + 1`. Checked before the copy and again after, it passes both
    /// times around a record that is half one event and half another, or
    /// wholly the later one. A reader that has just been lapped resumes at
    /// exactly that slot, and was handed such records as committed.
    ///
    /// What does move first is the write cursor: `push` is handed its claim
    /// by `fetch_add` before it stores a byte. So the cursor is loaded
    /// after the copy. Past `claim + capacity`, some producer has been
    /// handed this slot and the copy is thrown away, whole or not. At or
    /// below it none has, and the copy holds no byte of a later lap.
    ///
    /// The language says nothing about a read that races a write, so that
    /// rests on two orderings in the machine. In the producer, the bump has
    /// to be visible no later than the payload stores after it. In the
    /// reader, the cursor load must not be satisfied before the payload
    /// loads ahead of it.
    ///
    /// * x86-64 gives both for nothing. The bump is a locked instruction,
    ///   a core's stores become visible in program order, and its loads see
    ///   memory in program order.
    /// * aarch64 gives neither: stores to different addresses may become
    ///   visible in either order, and loads may be satisfied in either
    ///   order. For the producer, the `SeqCst` bump is `ldaddal`, and Arm
    ///   orders the write of an atomic instruction that has both acquire
    ///   and release semantics before every later access by its thread
    ///   (barrier-ordered-before); the write of a relaxed `ldadd` it does
    ///   not. For the reader, `fence(Acquire)` is `dmb ishld`, which orders
    ///   the loads before it ahead of the loads after it, and is no
    ///   instruction at all on x86-64.
    ///
    /// Two cases this does not cover, and both are the producer's to close:
    ///
    /// * An aarch64 core without the LSE atomics (Armv8.0) runs the bump as
    ///   an `ldaxr`/`stlxr` loop, and the architecture orders an exclusive
    ///   store before later acquire loads, not before later plain stores.
    ///   Only a barrier between the bump and the record would promise it
    ///   there.
    /// * A producer that slept through a whole lap holding `claim -
    ///   capacity` would write this slot when it woke, and the cursor shows
    ///   nothing of it. `take_slot` is what stops it: it finds the stamp
    ///   of this claim, or of this claim being written, and stores nothing.
    ///   A producer built before `take_slot` existed still does it.
    pub(crate) fn read_claim(&self, ring: usize, claim: u64) -> Claimed {
        let copy = self.copy_if_stamped(ring, claim);
        fence(Ordering::Acquire);
        let cursor = self.ring_header(ring).write_cursor.load(Ordering::Relaxed);
        if cursor > claim.saturating_add(self.slots_per_ring as u64) {
            return Claimed::Overwritten;
        }
        match copy {
            Some(event) => Claimed::Committed(event),
            None => Claimed::Pending,
        }
    }

    /// The event in a slot whose stamp says `claim`, before and after the
    /// copy. Not proof that the copy is that event: see
    /// [`Rings::read_claim`].
    fn copy_if_stamped(&self, ring: usize, claim: u64) -> Option<ScopeEvent> {
        let slot = self.slot(ring, claim);
        if slot.seq.load(Ordering::Acquire) != claim + 1 {
            return None;
        }
        // SAFETY: the acquire load paired with the producer's release store
        // makes the payload visible.
        let event = unsafe { std::ptr::read_volatile(slot.event.get()) };
        // Re-check: catches a slot rewritten and committed during the copy.
        if slot.seq.load(Ordering::Acquire) != claim + 1 {
            return None;
        }
        Some(event)
    }
}

/// What [`Rings::read_claim`] found at a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Claimed {
    /// The record of that claim, whole.
    Committed(ScopeEvent),
    /// The claim has not been committed, and its slot has not been handed
    /// to a later one: a producer is still writing it, or nobody has claimed
    /// it yet.
    Pending,
    /// A producer a lap or more ahead has been handed the slot. The record
    /// is gone, or going, and waiting will not bring it back.
    Overwritten,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Region {
        bytes: Vec<u8>,
        ring_count: usize,
        slots: usize,
    }

    impl Region {
        fn new(ring_count: usize, slots: usize) -> Region {
            let mut bytes = vec![0u8; layout_size(ring_count, slots) + CACHE_LINE];
            // Align the base to a cache line so the repr(align(64)) types are
            // legal to place there.
            let offset = bytes.as_ptr().align_offset(CACHE_LINE);
            let base = unsafe { bytes.as_mut_ptr().add(offset) };
            unsafe { init_region(base, ring_count, slots, 4242) };
            let _ = &mut bytes;
            Region { bytes, ring_count, slots }
        }

        fn rings(&self) -> Rings {
            let offset = self.bytes.as_ptr().align_offset(CACHE_LINE);
            unsafe {
                Rings::from_raw(
                    self.bytes.as_ptr().add(offset) as *mut u8,
                    self.ring_count,
                    self.slots,
                )
            }
        }
    }

    fn event(ts: u64, tid: u32) -> ScopeEvent {
        ScopeEvent { timestamp_ns: ts, tid, ..ScopeEvent::default() }
    }

    #[test]
    fn cursors_at_write_skip_an_earlier_session_without_counting_its_lap_as_loss() {
        let region = Region::new(2, 8);
        let rings = region.rings();
        // 12 records into a ring of 8: the first 4 are gone.
        for ts in 1..=12 {
            rings.push(0, event(ts, 7));
        }
        let mut from_zero = crate::merge::Cursors::for_rings(2);
        let counted = crate::merge::drain(&rings, &mut from_zero, 100);
        assert_eq!(counted.dropped, 4, "a cursor at 0 books the lap as loss");
        let mut fresh = crate::merge::Cursors::at_write(&rings);
        assert_eq!(fresh.read, vec![12, 0]);
        let nothing = crate::merge::drain(&rings, &mut fresh, 100);
        assert_eq!(nothing.dropped, 0, "an earlier session's lap is not this reader's loss");
        assert!(nothing.slices.iter().all(|s| s.events.is_empty()), "and its backlog is not replayed");
        rings.push(0, event(50, 7));
        let next = crate::merge::drain(&rings, &mut fresh, 100);
        assert_eq!(next.slices[0].events.len(), 1, "the next record is read");
        assert_eq!(next.slices[0].events[0].timestamp_ns, 50);
    }

    #[test]
    fn a_pushed_event_reads_back_committed() {
        let region = Region::new(2, 8);
        let rings = region.rings();
        rings.push(0, event(100, 7));
        assert_eq!(rings.write_cursor(0), 1);
        assert_eq!(rings.committed(0, 0).unwrap().timestamp_ns, 100);
        // Nothing was written to the neighbouring ring.
        assert_eq!(rings.write_cursor(1), 0);
    }

    #[test]
    fn a_claim_that_was_never_committed_reads_as_none() {
        let region = Region::new(1, 8);
        let rings = region.rings();
        rings.push(0, event(1, 1));
        assert!(rings.committed(0, 1).is_none(), "claim 1 was never made");
    }

    #[test]
    fn a_lapped_slot_is_detected_rather_than_returning_stale_data() {
        // Four slots, five pushes: the first claim's slot now belongs to the
        // fifth. Reading claim 0 must refuse rather than hand back claim 4.
        let region = Region::new(1, 4);
        let rings = region.rings();
        for i in 0..5u64 {
            rings.push(0, event(i, 1));
        }
        assert!(rings.committed(0, 0).is_none(), "lapped slots must not read back");
        assert_eq!(rings.committed(0, 4).unwrap().timestamp_ns, 4);
    }

    #[test]
    fn a_slot_whose_next_claim_is_already_handed_out_no_longer_reads_back() {
        // Four slots hold claims 0..4, and claim 4 has been handed to a
        // producer that has not stored a byte yet. From here on it may be
        // anywhere in its write of slot 0, and the slot's stamp will say
        // "claim 0" until the whole record is in. A reader must go by the
        // cursor, not the stamp.
        let region = Region::new(1, 4);
        let rings = region.rings();
        for i in 0..4u64 {
            rings.push(0, event(i, 1));
        }
        assert_eq!(rings.committed(0, 0).unwrap().timestamp_ns, 0);
        let lapping = rings.reserve_for_test(0);
        assert_eq!(lapping, 4);
        assert!(rings.committed(0, 0).is_none(), "claim 0 may be half overwritten");
        assert_eq!(rings.committed(0, 1).unwrap().timestamp_ns, 1, "claim 1 is still whole");
        rings.publish_reserved_for_test(0, lapping, event(4, 1));
        assert_eq!(rings.committed(0, 4).unwrap().timestamp_ns, 4);
    }

    #[test]
    fn a_producer_stops_writing_for_a_reader_that_died() {
        let region = Region::new(1, 8);
        let rings = region.rings();
        let header = unsafe { &*rings.base.cast::<Header>() };
        let leases = unsafe { &*rings.base.add(READER_LEASES_OFFSET).cast::<ReaderLeases>() };
        // Two services capturing: this process, and one that is then
        // killed, leaving its lease and its count behind.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        leases.pids[0].store(std::process::id(), Ordering::Release);
        leases.pids[5].store(dead, Ordering::Release);
        header.capturing.store(2, Ordering::Release);

        // Nothing on the machine starts another capture, which was the only
        // thing that ever took a dead reader's count back. The producer
        // looks for itself now and then as it writes.
        for n in 0..REAP_EVERY {
            rings.push(0, event(n, 1));
        }
        assert_eq!(leases.pids[5].load(Ordering::Acquire), 0, "the dead reader still holds a lease");
        assert_eq!(leases.pids[0].load(Ordering::Acquire), std::process::id(), "a live reader keeps its own");
        assert_eq!(header.capturing.load(Ordering::Acquire), 1);

        // With the live one gone too, the count reaches zero and the
        // process is back to a load and a branch per call.
        leases.pids[0].store(dead, Ordering::Release);
        for n in 0..REAP_EVERY {
            rings.push(0, event(n, 1));
        }
        assert_eq!(header.capturing.load(Ordering::Acquire), 0);
    }

    #[test]
    fn every_claim_lands_on_the_slot_the_layout_gives_it() {
        // The header and slot addresses are worked out once per `Rings`
        // rather than on every access, and the slot index is a mask. Both
        // have to agree with the layout another process computes from the
        // header alone, for the ordinary power-of-two ring and for the odd
        // size only a hand-built region has.
        for slots in [8usize, 6, 1] {
            let region = Region::new(3, slots);
            let rings = region.rings();
            let base = rings.base as usize;
            for ring in 0..3 {
                assert_eq!(
                    rings.ring_header(ring) as *const RingHeader as usize - base,
                    ring_header_offset(ring)
                );
                for claim in 0..3 * slots as u64 {
                    let index = (claim % slots as u64) as usize;
                    assert_eq!(
                        rings.slot(ring, claim) as *const Slot as usize - base,
                        slots_offset(3, slots, ring) + index * CACHE_LINE,
                        "ring {ring} claim {claim} of {slots} slots"
                    );
                }
            }
        }
    }

    #[test]
    fn dimensions_too_large_to_address_have_no_layout_size() {
        assert_eq!(checked_layout_size(16, 32768), Some(layout_size(16, 32768)));
        assert_eq!(checked_layout_size(1, 2), Some(layout_size(1, 2)));
        // What a 32-bit reader computes for the largest dimensions a header
        // can state, 128 rings of 2^31 slots: 2^44 bytes, which wraps to
        // little more than the control page.
        assert_eq!(checked_layout_size(MAX_RINGS, usize::MAX / CACHE_LINE), None);
        assert_eq!(checked_layout_size(usize::MAX / CACHE_LINE + 1, 2), None);
        assert_eq!(checked_layout_size(2, usize::MAX / (2 * CACHE_LINE)), None, "the sum overflows");
    }

    #[test]
    fn rings_are_sized_by_threads_and_a_memory_budget() {
        // One per thread up to the knee, and never zero.
        assert_eq!(ring_count_for_threads(0), 2, "a floor, so there is always somewhere to write");
        assert_eq!(ring_count_for_threads(8), 8);
        assert_eq!(
            ring_count_for_threads(4096),
            DEFAULT_RING_COUNT,
            "past the knee, more rings only fragment the buffer"
        );

        // The budget is respected, never exceeded by rounding up.
        let slots = slots_for_budget(DEFAULT_RING_COUNT, 8 * 1024 * 1024);
        assert_eq!(slots, 8192, "512 KiB a ring, not 64");
        assert!(DEFAULT_RING_COUNT * slots * CACHE_LINE <= 8 * 1024 * 1024);
        assert!(slots.is_power_of_two());
        // A budget too small for a real ring still yields a legal one.
        assert_eq!(slots_for_budget(128, 16), 2);
        // The old default fragmented the buffer sixteen ways further.
        assert_eq!(slots_for_budget(128, 8 * 1024 * 1024), 1024);
    }

    #[test]
    fn concurrent_producers_on_one_ring_lose_nothing() {
        // The test that would fail if slots were claimed with a plain store
        // instead of fetch_add: eight threads on one ring, every event must
        // survive with its own slot.
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 4_000;
        let region = Region::new(1, (THREADS * PER_THREAD) as usize);
        let rings = region.rings();
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let rings = &rings;
                scope.spawn(move || {
                    for i in 0..PER_THREAD {
                        rings.push(0, event(i, t as u32));
                    }
                });
            }
        });
        assert_eq!(rings.write_cursor(0), THREADS * PER_THREAD);
        let mut per_thread = [0u64; THREADS as usize];
        for claim in 0..rings.write_cursor(0) {
            let event = rings.committed(0, claim).expect("every claim commits");
            per_thread[event.tid as usize] += 1;
        }
        assert!(
            per_thread.iter().all(|n| *n == PER_THREAD),
            "every producer's events survived: {per_thread:?}"
        );
    }

    #[test]
    fn the_header_describes_the_layout_it_was_built_with() {
        let region = Region::new(3, 16);
        let offset = region.bytes.as_ptr().align_offset(CACHE_LINE);
        let header = unsafe { &*region.bytes.as_ptr().add(offset).cast::<Header>() };
        assert_eq!(header.magic.load(Ordering::Acquire), MAGIC);
        assert_eq!(header.version, VERSION);
        assert_eq!(header.ring_count, 3);
        assert_eq!(header.slots_per_ring, 16);
        assert_eq!(header.event_size, EVENT_SIZE as u32);
        assert_eq!(header.pid, 4242);
    }

    #[test]
    fn every_thread_gets_a_ring_by_the_same_rule() {
        // No fast path, no fallback: one rule, and it always answers.
        for tid in [0u64, 1, 7, u64::MAX] {
            assert!(ring_for_thread(tid, DEFAULT_RING_COUNT) < DEFAULT_RING_COUNT);
        }
        assert_eq!(ring_for_thread(42, 1), 0, "one ring is still a ring");
        assert_eq!(ring_for_thread(42, 0), 0, "no rings is not a divide by zero");
    }

    #[test]
    fn distribution_survives_strided_thread_ids() {
        // Windows hands out thread ids in multiples of four, and Linux in
        // runs of one. A plain `tid % ring_count` collapses the first case
        // onto a fraction of the rings; the mix has to survive both.
        const RINGS: usize = DEFAULT_RING_COUNT;
        for stride in [1u64, 4, 16, 4096] {
            let mut counts = vec![0usize; RINGS];
            const THREADS: u64 = 64_000;
            for i in 0..THREADS {
                counts[ring_for_thread(100_000 + i * stride, RINGS)] += 1;
            }
            let ideal = THREADS as usize / RINGS;
            for (ring, count) in counts.iter().enumerate() {
                assert!(
                    *count > ideal / 2 && *count < ideal * 2,
                    "stride {stride} put {count} of {THREADS} threads on ring {ring}, \
                     against an even share of {ideal}"
                );
            }
        }
    }

    #[test]
    fn a_plain_modulo_would_have_failed_that() {
        // Kept as the counter-example, so the mix is not mistaken for
        // cargo-culting: on multiples of four, `tid % 128` reaches a quarter
        // of the rings and leaves the rest idle.
        let reached: std::collections::HashSet<u64> =
            (0..64_000u64).map(|i| (100_000 + i * 4) % 16).collect();
        assert_eq!(reached.len(), 4, "4 of 16 rings, and 12 idle");
    }
}
