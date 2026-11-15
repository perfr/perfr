// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-process ring of packed live events with optional filesystem spill.
//!
//! The ring has a budget in **bytes** and drops its oldest events to stay
//! inside it. What a byte is depends on how the ring was made:
//!
//! - [`EventRing::with_encoded_bytes`]: the bytes of the capture as it would
//!   be written out. Events are counted in chunks of
//!   [`Encoding::chunk_events`], each at the size [`Encoding::size`] gives
//!   it, and the oldest whole chunk goes when the total passes the budget.
//!   This is the ring a capture service uses: its budget is the most a saved
//!   capture can weigh, however long it has been recording.
//! - [`EventRing::with_bytes`]: [`LIVE_EVENT_SIZE`] bytes an event, dropped
//!   one at a time. A ring of exactly so many events.
//!
//! If a spill directory is configured, the events that are dropped are
//! appended to a file first. The in-memory ring used by the live view is
//! never read back from the spill file, so a spill I/O error cannot corrupt
//! live data.
//!
//! The events are held in blocks that are allocated as the ring fills and
//! freed as it empties, so an idle ring costs nothing whatever its budget.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use perfr_live_event::{LiveEvent, LIVE_EVENT_SIZE};
use parking_lot::Mutex;

/// Magic for the spill file (`ORSP` = perfr Ring Spill).
pub const SPILL_MAGIC: &[u8; 4] = b"ORSP";
pub const SPILL_VERSION: u16 = 1;
pub const DEFAULT_SPILL_FILE_NAME: &str = "perfr-live-spill.bin";

#[derive(Debug)]
pub enum RingError {
    CapacityTooSmall,
    Io(io::Error),
}

impl std::fmt::Display for RingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RingError::CapacityTooSmall => {
                write!(f, "ring_buffer_bytes must be at least {LIVE_EVENT_SIZE}")
            }
            RingError::Io(e) => write!(f, "spill I/O: {e}"),
        }
    }
}

impl std::error::Error for RingError {}

impl From<io::Error> for RingError {
    fn from(value: io::Error) -> Self {
        RingError::Io(value)
    }
}

struct SpillWriter {
    path: PathBuf,
    file: File,
    events_written: u64,
}

impl SpillWriter {
    /// The directory comes from whoever configures the ring, which over
    /// HTTP is anyone who can call the service, and the service is often
    /// root. So the file is never opened through a link someone left under
    /// its name (`O_NOFOLLOW`: a link to `/etc/shadow` in a directory of
    /// the caller's choosing was truncated and written over), and what it
    /// holds is the capture, for its owner alone to read.
    fn create(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(DEFAULT_SPILL_FILE_NAME);
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path)?;
        #[cfg(unix)]
        {
            // A file that was already there keeps the mode it had.
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(SPILL_MAGIC)?;
        file.write_all(&SPILL_VERSION.to_le_bytes())?;
        file.write_all(&(LIVE_EVENT_SIZE as u16).to_le_bytes())?;
        Ok(Self {
            path,
            file,
            events_written: 0,
        })
    }

    fn append(&mut self, events: &[LiveEvent]) -> io::Result<()> {
        let mut buf = vec![0u8; events.len() * LIVE_EVENT_SIZE];
        for (i, ev) in events.iter().enumerate() {
            ev.write_bytes(&mut buf[i * LIVE_EVENT_SIZE..]);
        }
        self.file.write_all(&buf)?;
        self.events_written += events.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Events in one storage block: 128 KiB.
const BLOCK_EVENTS: usize = 4096;

/// The events, oldest first, in blocks. Every block but the last is full;
/// `skip` events at the front of the first one have been dropped.
#[derive(Default)]
struct Store {
    blocks: VecDeque<Vec<LiveEvent>>,
    skip: usize,
    len: usize,
}

impl Store {
    fn push(&mut self, event: LiveEvent) {
        match self.blocks.back_mut() {
            Some(block) if block.len() < BLOCK_EVENTS => block.push(event),
            _ => {
                let mut block = Vec::with_capacity(BLOCK_EVENTS);
                block.push(event);
                self.blocks.push_back(block);
            }
        }
        self.len += 1;
    }

    /// The `i`th oldest event.
    fn get(&self, i: usize) -> LiveEvent {
        let at = self.skip + i;
        self.blocks[at / BLOCK_EVENTS][at % BLOCK_EVENTS]
    }

    /// Drops the `n` oldest events, and the blocks that held nothing else.
    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        self.len -= n;
        self.skip += n;
        if self.len == 0 {
            self.blocks.clear();
            self.skip = 0;
            return;
        }
        while self.skip >= BLOCK_EVENTS {
            self.blocks.pop_front();
            self.skip -= BLOCK_EVENTS;
        }
    }

    /// Appends the events at `[from, to)`, counted from the oldest, to `out`.
    fn copy_range(&self, from: usize, to: usize, out: &mut Vec<LiveEvent>) {
        let to = to.min(self.len);
        if from >= to {
            return;
        }
        out.reserve(to - from);
        let mut at = self.skip + from;
        let mut left = to - from;
        while left > 0 {
            let block = &self.blocks[at / BLOCK_EVENTS];
            let offset = at % BLOCK_EVENTS;
            let take = left.min(block.len() - offset);
            out.extend_from_slice(&block[offset..offset + take]);
            at += take;
            left -= take;
        }
    }
}

/// How a ring made by [`EventRing::with_encoded_bytes`] counts its bytes.
#[derive(Clone)]
pub struct Encoding {
    /// Events in one chunk: what one batch of the written capture holds.
    pub chunk_events: usize,
    /// The size of one chunk as written.
    #[allow(clippy::type_complexity)]
    pub size: Arc<dyn Fn(&[LiveEvent]) -> usize + Send + Sync>,
}

/// One full chunk of an encoded ring.
struct Sealed {
    bytes: u64,
    /// The earliest end among its events.
    min_end_ns: u64,
}

enum Measure {
    /// [`LIVE_EVENT_SIZE`] bytes an event, dropped one at a time.
    Events,
    Encoded {
        encoding: Encoding,
        /// The full chunks, oldest first. The chunk `k` of the ring's life
        /// is the events `[k * chunk_events, (k + 1) * chunk_events)`, so
        /// the tail is always the first event of a chunk.
        sealed: VecDeque<Sealed>,
        sealed_bytes: u64,
        /// The largest chunk so far: the room kept for the one being filled.
        largest_chunk: u64,
        /// Bytes of the budget spent on something else: see
        /// [`EventRing::set_reserved`].
        reserved: u64,
    },
}

struct Inner {
    store: Store,
    /// Absolute sequence number of the next write (monotone).
    head: u64,
    /// Absolute sequence number of the oldest live event.
    tail: u64,
    budget: u64,
    measure: Measure,
    dropped: u64,
    spilled: u64,
    spill_errors: u64,
    spill: Option<SpillWriter>,
    /// See [`RingStats::evicted_before_ns`].
    evicted_before_ns: u64,
    /// One chunk, copied out to be measured or spilled.
    scratch: Vec<LiveEvent>,
}

impl Inner {
    fn len(&self) -> usize {
        self.store.len
    }

    /// Drops the `n` oldest events, to the spill file when there is one.
    fn evict(&mut self, n: usize) {
        let n = n.min(self.len());
        if n == 0 {
            return;
        }
        if let Some(spill) = self.spill.as_mut() {
            self.scratch.clear();
            self.store.copy_range(0, n, &mut self.scratch);
            match spill.append(&self.scratch) {
                Ok(()) => self.spilled += n as u64,
                Err(_) => self.spill_errors += n as u64,
            }
        }
        self.store.drop_front(n);
        self.tail += n as u64;
        self.dropped += n as u64;
    }

    fn push_one(&mut self, event: LiveEvent) {
        if matches!(self.measure, Measure::Events) && self.len() as u64 >= self.budget / LIVE_EVENT_SIZE as u64 {
            self.evict(1);
        }
        self.store.push(event);
        self.head += 1;
        let chunk = match &self.measure {
            Measure::Encoded { encoding, .. } => encoding.chunk_events as u64,
            Measure::Events => return,
        };
        if self.head % chunk == 0 {
            self.seal(chunk as usize);
            self.enforce();
        }
    }

    /// Measures the chunk that just filled.
    fn seal(&mut self, chunk: usize) {
        self.scratch.clear();
        self.store.copy_range(self.store.len - chunk, self.store.len, &mut self.scratch);
        let min_end_ns = self.scratch.iter().map(|e| e.end_ns()).min().unwrap_or(0);
        if let Measure::Encoded { encoding, sealed, sealed_bytes, largest_chunk, .. } = &mut self.measure {
            let bytes = (encoding.size)(&self.scratch) as u64;
            sealed.push_back(Sealed { bytes, min_end_ns });
            *sealed_bytes += bytes;
            *largest_chunk = (*largest_chunk).max(bytes);
        }
    }

    /// Drops the oldest chunks until what is left fits the budget, with room
    /// for the chunk being filled. That one is never dropped: a budget too
    /// small for a single chunk still holds the newest events.
    fn enforce(&mut self) {
        loop {
            let Measure::Encoded { encoding, sealed, sealed_bytes, largest_chunk, reserved } = &mut self.measure else {
                // By the event: whatever is over the count goes.
                let capacity = (self.budget / LIVE_EVENT_SIZE as u64) as usize;
                let over = self.len().saturating_sub(capacity);
                self.evict(over);
                return;
            };
            if sealed.is_empty() || *sealed_bytes + *reserved + *largest_chunk <= self.budget {
                return;
            }
            let chunk = encoding.chunk_events;
            let gone = sealed.pop_front().expect("checked non-empty");
            *sealed_bytes -= gone.bytes;
            let next_min_end = sealed.front().map(|next| next.min_end_ns);
            self.evict(chunk);
            // What is left begins with the next chunk. With none left it is
            // the chunk being filled, which is walked: this is the rare case
            // of a budget that holds less than two chunks. With nothing left
            // at all, the chunk that went is all there is to go by.
            let floor = next_min_end
                .or_else(|| {
                    self.scratch.clear();
                    self.store.copy_range(0, self.store.len, &mut self.scratch);
                    self.scratch.iter().map(|e| e.end_ns()).min()
                })
                .unwrap_or(gone.min_end_ns);
            self.evicted_before_ns = self.evicted_before_ns.max(floor);
        }
    }
}

/// Concurrent produce / consume ring. Producers serialize on a mutex; consumers
/// copy a snapshot or a seq-range under the same lock. The lock is held only
/// for the memcpy of the requested events, and for measuring a chunk when
/// one fills.
pub struct EventRing {
    inner: Mutex<Inner>,
    produce_count: AtomicU64,
}

pub type SharedRing = Arc<EventRing>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RingStats {
    /// The budget.
    pub bytes_capacity: u64,
    /// How many events the budget holds. For an encoded ring this is an
    /// estimate, from the size of the events it has measured so far.
    pub events_capacity: u64,
    pub events_live: u64,
    pub head: u64,
    pub tail: u64,
    pub dropped: u64,
    pub spilled: u64,
    pub spill_errors: u64,
    pub produced: u64,
    pub oldest_start_ns: u64,
    pub newest_end_ns: u64,
    /// The bytes the live events count for against the budget: for an
    /// encoded ring their size as written, the chunk being filled included.
    pub encoded_bytes: u64,
    /// See [`EventRing::set_reserved`].
    pub reserved_bytes: u64,
    /// The ring has dropped its events in the order they arrived, which is
    /// about the order they ended in. This is a time the events it still
    /// holds end at or after, give or take the ones that arrived out of
    /// order: a reader that mirrors the ring (the viewer's own index) drops
    /// what ended before it and holds about what the ring does. 0 until an
    /// encoded ring first drops a chunk; a ring counted by the event never
    /// sets it.
    pub evicted_before_ns: u64,
}

impl EventRing {
    /// A ring of `bytes / LIVE_EVENT_SIZE` events, dropped one at a time.
    pub fn with_bytes(bytes: u64, spill_dir: Option<&Path>) -> Result<Self, RingError> {
        Self::new(bytes, Measure::Events, spill_dir)
    }

    /// A ring whose budget is the size of its events as `encoding` writes
    /// them. See the module documentation.
    pub fn with_encoded_bytes(bytes: u64, encoding: Encoding, spill_dir: Option<&Path>) -> Result<Self, RingError> {
        if encoding.chunk_events == 0 {
            return Err(RingError::CapacityTooSmall);
        }
        let measure =
            Measure::Encoded { encoding, sealed: VecDeque::new(), sealed_bytes: 0, largest_chunk: 0, reserved: 0 };
        Self::new(bytes, measure, spill_dir)
    }

    fn new(bytes: u64, measure: Measure, spill_dir: Option<&Path>) -> Result<Self, RingError> {
        if bytes < LIVE_EVENT_SIZE as u64 {
            return Err(RingError::CapacityTooSmall);
        }
        let spill = match spill_dir {
            Some(dir) if !dir.as_os_str().is_empty() => Some(SpillWriter::create(dir)?),
            _ => None,
        };
        // A ring counted by the event holds a whole number of them.
        let budget = match measure {
            Measure::Events => bytes - bytes % LIVE_EVENT_SIZE as u64,
            Measure::Encoded { .. } => bytes,
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                store: Store::default(),
                head: 0,
                tail: 0,
                budget,
                measure,
                dropped: 0,
                spilled: 0,
                spill_errors: 0,
                spill,
                evicted_before_ns: 0,
                scratch: Vec::new(),
            }),
            produce_count: AtomicU64::new(0),
        })
    }

    /// Changes the budget, keeping what the ring holds that still fits.
    pub fn set_bytes(&self, bytes: u64) -> Result<(), RingError> {
        if bytes < LIVE_EVENT_SIZE as u64 {
            return Err(RingError::CapacityTooSmall);
        }
        let mut inner = self.inner.lock();
        inner.budget = match inner.measure {
            Measure::Events => bytes - bytes % LIVE_EVENT_SIZE as u64,
            Measure::Encoded { .. } => bytes,
        };
        inner.enforce();
        Ok(())
    }

    /// Bytes of the budget that are not the events': what a written capture
    /// holds besides them (its names), so that the budget bounds the whole
    /// file. A ring counted by the event ignores it.
    pub fn set_reserved(&self, bytes: u64) {
        let mut inner = self.inner.lock();
        if let Measure::Encoded { reserved, .. } = &mut inner.measure {
            if *reserved != bytes {
                *reserved = bytes;
                inner.enforce();
            }
        }
    }

    pub fn push(&self, event: LiveEvent) {
        {
            let mut inner = self.inner.lock();
            inner.push_one(event);
        }
        self.produce_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn push_many(&self, events: &[LiveEvent]) {
        if events.is_empty() {
            return;
        }
        {
            let mut inner = self.inner.lock();
            for &event in events {
                inner.push_one(event);
            }
        }
        self.produce_count
            .fetch_add(events.len() as u64, Ordering::Relaxed);
    }

    /// Copy events with `seq ∈ [from, head)` into `out`. Returns the new cursor
    /// (`head`). If `from` is behind `tail`, the gap is skipped (those events
    /// were dropped / spilled). A `from` past the head (a cursor from a
    /// ring that has since been replaced) reads nothing; unclamped, the
    /// count below underflowed.
    pub fn read_from(&self, from: u64, out: &mut Vec<LiveEvent>) -> u64 {
        let inner = self.inner.lock();
        let end = inner.head;
        let start = from.max(inner.tail).min(end);
        inner.store.copy_range((start - inner.tail) as usize, (end - inner.tail) as usize, out);
        end
    }

    /// Copies the events appended since `cursor` into `out` and returns the
    /// new cursor, for a reader that keeps what it has read and wants only
    /// what is new. `None`, with nothing copied, when what it has read is no
    /// longer what the ring holds before `cursor`: the oldest event is not
    /// the one at `tail` any more (events were evicted since), or `cursor`
    /// is not a position of this ring. The reader then starts over from a
    /// [`Self::snapshot`], whose tail and length give its `tail` and `cursor`.
    pub fn read_appended(&self, tail: u64, cursor: u64, out: &mut Vec<LiveEvent>) -> Option<u64> {
        let inner = self.inner.lock();
        if inner.tail != tail || cursor < tail || cursor > inner.head {
            return None;
        }
        inner.store.copy_range((cursor - inner.tail) as usize, (inner.head - inner.tail) as usize, out);
        Some(inner.head)
    }

    /// Snapshot of every live event in time/sequence order, plus the tail seq.
    pub fn snapshot(&self) -> (u64, Vec<LiveEvent>) {
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity(inner.len());
        inner.store.copy_range(0, inner.len(), &mut out);
        (inner.tail, out)
    }

    pub fn stats(&self) -> RingStats {
        let inner = self.inner.lock();
        let live = inner.len();
        let mut stats = RingStats {
            bytes_capacity: inner.budget,
            events_live: live as u64,
            head: inner.head,
            tail: inner.tail,
            dropped: inner.dropped,
            spilled: inner.spilled,
            spill_errors: inner.spill_errors,
            produced: self.produce_count.load(Ordering::Relaxed),
            evicted_before_ns: inner.evicted_before_ns,
            ..RingStats::default()
        };
        if live > 0 {
            stats.oldest_start_ns = inner.store.get(0).start_ns;
            stats.newest_end_ns = inner.store.get(live - 1).end_ns();
        }
        let Measure::Encoded { encoding, sealed, sealed_bytes, reserved, .. } = &inner.measure else {
            stats.events_capacity = inner.budget / LIVE_EVENT_SIZE as u64;
            stats.encoded_bytes = (live * LIVE_EVENT_SIZE) as u64;
            return stats;
        };
        stats.reserved_bytes = *reserved;
        // What the budget holds, at the size the measured events came to;
        // at the size of an event in memory before any has been measured.
        let sealed_events = (sealed.len() * encoding.chunk_events) as u64;
        let room = inner.budget.saturating_sub(*reserved);
        stats.events_capacity = if *sealed_bytes > 0 {
            (room as u128 * sealed_events as u128 / *sealed_bytes as u128) as u64
        } else {
            room / LIVE_EVENT_SIZE as u64
        };
        // The chunk being filled is measured here, on a copy and outside the
        // lock: it is one chunk at most, and nobody pushing waits for it.
        let mut open = Vec::new();
        inner.store.copy_range(sealed_events as usize, live, &mut open);
        let (size, sealed_bytes) = (encoding.size.clone(), *sealed_bytes);
        drop(inner);
        stats.encoded_bytes = sealed_bytes + if open.is_empty() { 0 } else { size(&open) as u64 };
        stats
    }

    pub fn spill_path(&self) -> Option<PathBuf> {
        self.inner.lock().spill.as_ref().map(|s| s.path.clone())
    }

    pub fn flush_spill(&self) -> io::Result<()> {
        if let Some(spill) = self.inner.lock().spill.as_mut() {
            spill.flush()?;
        }
        Ok(())
    }

    /// The budget.
    pub fn bytes_capacity(&self) -> u64 {
        self.inner.lock().budget
    }

    /// How many events the budget holds: see [`RingStats::events_capacity`].
    pub fn events_capacity(&self) -> usize {
        self.stats().events_capacity as usize
    }
}

/// Read a spill file written by [`EventRing`]. Used by tests; the live view
/// does not load spill data.
pub fn read_spill_file(path: &Path) -> io::Result<Vec<LiveEvent>> {
    let data = std::fs::read(path)?;
    if data.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "spill file too short",
        ));
    }
    if &data[0..4] != SPILL_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad spill magic",
        ));
    }
    let version = u16::from_le_bytes(data[4..6].try_into().unwrap());
    if version != SPILL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported spill version {version}"),
        ));
    }
    let ev_size = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
    if ev_size != LIVE_EVENT_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("spill event size {ev_size} != {LIVE_EVENT_SIZE}"),
        ));
    }
    let payload = &data[8..];
    if payload.len() % LIVE_EVENT_SIZE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "spill payload is not a whole number of events",
        ));
    }
    let mut events = Vec::with_capacity(payload.len() / LIVE_EVENT_SIZE);
    for chunk in payload.chunks_exact(LIVE_EVENT_SIZE) {
        let mut arr = [0u8; LIVE_EVENT_SIZE];
        arr.copy_from_slice(chunk);
        events.push(LiveEvent::from_bytes(&arr));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::kind;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    fn ev(i: u64) -> LiveEvent {
        LiveEvent {
            start_ns: i * 10,
            duration_ns: 5,
            tid: 1,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: i as u32,
        }
    }

    #[test]
    fn reject_capacity_smaller_than_one_event() {
        assert!(matches!(
            EventRing::with_bytes(LIVE_EVENT_SIZE as u64 - 1, None),
            Err(RingError::CapacityTooSmall)
        ));
    }

    #[test]
    fn rounds_bytes_down_to_event_size() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 3 + 10) as u64, None).unwrap();
        assert_eq!(ring.events_capacity(), 3);
        assert_eq!(ring.bytes_capacity(), (LIVE_EVENT_SIZE * 3) as u64);
    }

    #[test]
    fn wrap_drops_oldest() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 3) as u64, None).unwrap();
        for i in 0..5 {
            ring.push(ev(i));
        }
        let (tail, snap) = ring.snapshot();
        assert_eq!(tail, 2);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].name_id, 2);
        assert_eq!(snap[2].name_id, 4);
        let stats = ring.stats();
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats.events_live, 3);
    }

    #[test]
    fn reading_from_past_the_head_reads_nothing() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 4) as u64, None).unwrap();
        ring.push_many(&[ev(0), ev(1)]);
        // A cursor kept from a ring that was since replaced by a new one.
        let mut out = Vec::new();
        assert_eq!(ring.read_from(7, &mut out), 2, "the cursor comes back to the head");
        assert!(out.is_empty());
        assert_eq!(ring.read_from(u64::MAX, &mut out), 2);
        assert!(out.is_empty());
    }

    #[test]
    fn appended_events_are_read_once_until_the_ring_evicts() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 4) as u64, None).unwrap();
        ring.push_many(&[ev(0), ev(1)]);
        let (tail, held) = ring.snapshot();
        let mut cursor = tail + held.len() as u64;
        let mut fresh = Vec::new();
        // Nothing new: the cursor stays.
        assert_eq!(ring.read_appended(tail, cursor, &mut fresh), Some(cursor));
        assert!(fresh.is_empty());
        ring.push_many(&[ev(2), ev(3)]);
        cursor = ring.read_appended(tail, cursor, &mut fresh).unwrap();
        assert_eq!(fresh.iter().map(|e| e.name_id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(cursor, 4);
        // One more evicts event 0: what the reader holds is no longer the
        // ring, and it is told so rather than handed the tail.
        ring.push(ev(4));
        fresh.clear();
        assert_eq!(ring.read_appended(tail, cursor, &mut fresh), None);
        assert!(fresh.is_empty());
        // A cursor that is not a position of this ring.
        let (tail, held) = ring.snapshot();
        assert_eq!((tail, held.len()), (1, 4));
        assert_eq!(ring.read_appended(tail, 0, &mut fresh), None, "before the tail");
        assert_eq!(ring.read_appended(tail, 6, &mut fresh), None, "past the head");
        assert_eq!(ring.read_appended(tail, 5, &mut fresh), Some(5));
    }

    #[test]
    fn read_from_skips_dropped_prefix() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 2) as u64, None).unwrap();
        ring.push(ev(0));
        ring.push(ev(1));
        let mut out = Vec::new();
        let cursor = ring.read_from(0, &mut out);
        assert_eq!(out.len(), 2);
        ring.push(ev(2));
        out.clear();
        let cursor2 = ring.read_from(cursor - 2, &mut out);
        // tail is now 1 (event 0 dropped), so we get 1 and 2
        assert_eq!(
            out.iter().map(|e| e.name_id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(cursor2, 3);
    }

    #[test]
    fn concurrent_produce_consume_does_not_lose_or_corrupt() {
        let ring = Arc::new(EventRing::with_bytes((LIVE_EVENT_SIZE * 4096) as u64, None).unwrap());
        let produced = Arc::new(AtomicUsize::new(0));
        let consumers_ok = Arc::new(AtomicUsize::new(0));

        let producers: Vec<_> = (0..4)
            .map(|p| {
                let ring = Arc::clone(&ring);
                let produced = Arc::clone(&produced);
                thread::spawn(move || {
                    for i in 0..2_000u64 {
                        ring.push(LiveEvent {
                            start_ns: (p as u64) * 10_000 + i,
                            duration_ns: 1,
                            tid: p,
                            pid: 1,
                            kind: kind::FUNCTION_CALL,
                            depth: 0,
                            extra: 0,
                            _pad: 0,
                            name_id: i as u32,
                        });
                        produced.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        let consumers: Vec<_> = (0..3)
            .map(|_| {
                let ring = Arc::clone(&ring);
                let consumers_ok = Arc::clone(&consumers_ok);
                thread::spawn(move || {
                    let mut cursor = 0u64;
                    let mut buf = Vec::new();
                    for _ in 0..200 {
                        buf.clear();
                        cursor = ring.read_from(cursor, &mut buf);
                        for e in &buf {
                            assert_eq!(e.kind, kind::FUNCTION_CALL);
                            assert_eq!(e.duration_ns, 1);
                        }
                        consumers_ok.fetch_add(buf.len(), Ordering::Relaxed);
                        thread::yield_now();
                    }
                })
            })
            .collect();

        for t in producers {
            t.join().unwrap();
        }
        for t in consumers {
            t.join().unwrap();
        }
        assert_eq!(produced.load(Ordering::Relaxed), 8_000);
        let stats = ring.stats();
        assert_eq!(stats.produced, 8_000);
        assert_eq!(stats.events_live + stats.dropped, 8_000);
        let (_, snap) = ring.snapshot();
        assert_eq!(snap.len() as u64, stats.events_live);
    }

    /// Ten bytes an event and four for the chunk: a size that is easy to
    /// count by hand and is not the size in memory.
    fn encoding(chunk_events: usize) -> Encoding {
        Encoding { chunk_events, size: Arc::new(|events| 4 + events.len() * 10) }
    }

    #[test]
    fn storage_spans_blocks_and_frees_them_from_the_front() {
        let n = (BLOCK_EVENTS * 3 + 17) as u64;
        let ring = EventRing::with_bytes(n * LIVE_EVENT_SIZE as u64, None).unwrap();
        for i in 0..n + 5 {
            ring.push(ev(i));
        }
        let (tail, snap) = ring.snapshot();
        assert_eq!((tail, snap.len() as u64), (5, n));
        assert!(snap.iter().zip(5..).all(|(e, i)| e.name_id == i as u32));
        // A read across a block boundary, and one from the last block.
        let mut out = Vec::new();
        ring.read_from(BLOCK_EVENTS as u64 - 2, &mut out);
        assert_eq!(out.len() as u64, n + 5 - (BLOCK_EVENTS as u64 - 2));
        assert_eq!(out[0].name_id, BLOCK_EVENTS as u32 - 2);
        assert_eq!(ring.inner.lock().store.blocks.len(), 4);
        let stats = ring.stats();
        assert_eq!((stats.oldest_start_ns, stats.newest_end_ns), (50, (n + 4) * 10 + 5));
    }

    #[test]
    fn an_encoded_ring_counts_chunks_as_written_and_drops_whole_ones() {
        // Chunks of 4 events are 44 bytes. The budget holds three, but room
        // for the one being filled is kept: two full chunks stay.
        let ring = EventRing::with_encoded_bytes(140, encoding(4), None).unwrap();
        for i in 0..6 {
            ring.push(ev(i));
        }
        let stats = ring.stats();
        // One full chunk, and two events of the next measured as they are.
        assert_eq!(stats.encoded_bytes, 44 + 24);
        assert_eq!((stats.events_live, stats.dropped, stats.evicted_before_ns), (6, 0, 0));
        ring.push_many(&[ev(6), ev(7)]);
        assert_eq!(ring.stats().dropped, 0, "two chunks and room for a third");
        for i in 8..12 {
            ring.push(ev(i));
        }
        let stats = ring.stats();
        assert_eq!((stats.tail, stats.dropped, stats.evicted_before_ns), (4, 4, 45), "the third filled: the first went");
        ring.push_many(&[ev(12), ev(13), ev(14), ev(15)]);
        let stats = ring.stats();
        assert_eq!((stats.tail, stats.dropped, stats.events_live), (8, 8, 8));
        assert_eq!(stats.encoded_bytes, 88);
        // Event 8 is the earliest to end among what is left.
        assert_eq!(stats.evicted_before_ns, 85);
        let (tail, snap) = ring.snapshot();
        assert_eq!(tail % 4, 0, "the tail is the first event of a chunk");
        assert_eq!(snap.iter().map(|e| e.name_id).collect::<Vec<_>>(), (8..16).collect::<Vec<_>>());
        // The estimate of what the budget holds: 140 bytes at 11 an event.
        assert_eq!(stats.events_capacity, 12);
        assert_eq!(ring.events_capacity(), 12);
    }

    #[test]
    fn reserved_bytes_and_a_smaller_budget_drop_chunks_at_once() {
        let ring = EventRing::with_encoded_bytes(1000, encoding(4), None).unwrap();
        for i in 0..16 {
            ring.push(ev(i));
        }
        assert_eq!(ring.stats().dropped, 0);
        // 4 * 44 + 44 of room + 800 reserved is over by one chunk.
        ring.set_reserved(800);
        let stats = ring.stats();
        assert_eq!((stats.dropped, stats.reserved_bytes, stats.encoded_bytes), (4, 800, 132));
        ring.set_reserved(0);
        assert_eq!(ring.stats().dropped, 4, "room that comes back brings nothing back");
        ring.set_bytes(100).unwrap();
        let stats = ring.stats();
        assert_eq!((stats.bytes_capacity, stats.dropped, stats.events_live), (100, 12, 4));
        assert!(ring.set_bytes(1).is_err());
        assert_eq!(ring.bytes_capacity(), 100);
    }

    #[test]
    fn a_budget_under_one_chunk_still_holds_the_newest_events() {
        let ring = EventRing::with_encoded_bytes(LIVE_EVENT_SIZE as u64, encoding(4), None).unwrap();
        for i in 0..10 {
            ring.push(ev(i));
        }
        let (tail, snap) = ring.snapshot();
        assert_eq!((tail, snap.len()), (8, 2), "the chunk being filled is never dropped");
        // Nothing was left when the second chunk went: its own earliest end.
        assert_eq!(ring.stats().evicted_before_ns, 45);
        assert!(EventRing::with_encoded_bytes(1024, encoding(0), None).is_err());
    }

    #[test]
    fn a_ring_counted_by_the_event_resizes_in_place() {
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 8) as u64, None).unwrap();
        for i in 0..8 {
            ring.push(ev(i));
        }
        ring.set_bytes((LIVE_EVENT_SIZE * 3 + 5) as u64).unwrap();
        let (tail, snap) = ring.snapshot();
        assert_eq!((tail, snap.len()), (5, 3));
        let stats = ring.stats();
        assert_eq!((stats.bytes_capacity, stats.events_capacity), ((LIVE_EVENT_SIZE * 3) as u64, 3));
        assert_eq!(stats.encoded_bytes, (LIVE_EVENT_SIZE * 3) as u64);
        ring.set_reserved(1 << 20);
        assert_eq!(ring.stats().events_live, 3, "reserved bytes are an encoded ring's");
    }

    #[test]
    fn an_encoded_ring_spills_the_chunks_it_drops() {
        let dir = std::env::temp_dir().join(format!("perfr-live-spill-chunk-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ring = EventRing::with_encoded_bytes(100, encoding(4), Some(&dir)).unwrap();
        for i in 0..14 {
            ring.push(ev(i));
        }
        ring.flush_spill().unwrap();
        let stats = ring.stats();
        assert_eq!((stats.spilled, stats.dropped, stats.events_live), (8, 8, 6));
        let spilled = read_spill_file(&ring.spill_path().unwrap()).unwrap();
        assert_eq!(spilled.iter().map(|e| e.name_id).collect::<Vec<_>>(), (0..8).collect::<Vec<_>>());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_spill_file_is_private_and_never_opened_through_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("perfr-live-spill-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Someone left a link under the spill file's name, to a file the
        // service can write and they cannot.
        let precious = dir.join("precious");
        std::fs::write(&precious, b"do not truncate").unwrap();
        let spill_dir = dir.join("spill");
        std::fs::create_dir_all(&spill_dir).unwrap();
        std::os::unix::fs::symlink(&precious, spill_dir.join(DEFAULT_SPILL_FILE_NAME)).unwrap();
        assert!(EventRing::with_bytes((LIVE_EVENT_SIZE * 4) as u64, Some(&spill_dir)).is_err(), "a link is refused");
        assert_eq!(std::fs::read(&precious).unwrap(), b"do not truncate");

        // A file of its own, and one that was there with a wider mode:
        // readable by its owner only.
        let own = dir.join("own");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join(DEFAULT_SPILL_FILE_NAME), b"old").unwrap();
        std::fs::set_permissions(own.join(DEFAULT_SPILL_FILE_NAME), std::fs::Permissions::from_mode(0o644)).unwrap();
        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 4) as u64, Some(&own)).unwrap();
        let mode = std::fs::metadata(ring.spill_path().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spill_writes_overwritten_events_and_live_snapshot_stays_consistent() {
        let dir =
            std::env::temp_dir().join(format!("perfr-live-spill-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ring = EventRing::with_bytes((LIVE_EVENT_SIZE * 4) as u64, Some(&dir)).unwrap();
        for i in 0..10 {
            ring.push(ev(i));
        }
        ring.flush_spill().unwrap();

        let stats = ring.stats();
        assert_eq!(stats.events_live, 4);
        assert_eq!(stats.dropped, 6);
        assert_eq!(stats.spilled, 6);

        let (_, live) = ring.snapshot();
        assert_eq!(live.len(), 4);
        assert_eq!(live[0].name_id, 6);
        assert_eq!(live[3].name_id, 9);
        // Live view still has a contiguous recent window; no holes / corruption.
        for w in live.windows(2) {
            assert!(w[1].start_ns > w[0].start_ns);
            assert_eq!(w[1].name_id, w[0].name_id + 1);
        }

        let spilled = read_spill_file(&ring.spill_path().unwrap()).unwrap();
        assert_eq!(spilled.len(), 6);
        assert_eq!(spilled[0].name_id, 0);
        assert_eq!(spilled[5].name_id, 5);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
