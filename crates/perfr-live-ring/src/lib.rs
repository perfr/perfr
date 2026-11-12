// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-process ring of packed live events with optional filesystem spill.
//!
//! Capacity is measured in **bytes** (rounded down to a whole number of
//! [`LiveEvent`]s). When the ring wraps, the oldest events are dropped.
//! If a spill directory is configured, those overwritten events are appended
//! to a file first. The in-memory ring used by the live view is never read
//! back from the spill file, so a spill I/O error cannot corrupt live data.

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

struct Inner {
    buf: Vec<LiveEvent>,
    /// Absolute sequence number of the next write (monotone).
    head: u64,
    /// Absolute sequence number of the oldest live event.
    tail: u64,
    dropped: u64,
    spilled: u64,
    spill_errors: u64,
    spill: Option<SpillWriter>,
}

impl Inner {
    fn capacity(&self) -> usize {
        self.buf.len()
    }

    fn len(&self) -> usize {
        (self.head - self.tail) as usize
    }

    fn slot(&self, seq: u64) -> usize {
        (seq % self.buf.len() as u64) as usize
    }

    fn push_one(&mut self, event: LiveEvent) {
        if self.len() == self.capacity() {
            let old = self.buf[self.slot(self.tail)];
            if let Some(spill) = self.spill.as_mut() {
                match spill.append(&[old]) {
                    Ok(()) => self.spilled += 1,
                    Err(_) => self.spill_errors += 1,
                }
            }
            self.tail += 1;
            self.dropped += 1;
        }
        let idx = self.slot(self.head);
        self.buf[idx] = event;
        self.head += 1;
    }
}

/// Concurrent produce / consume ring. Producers serialize on a mutex; consumers
/// copy a snapshot or a seq-range under the same lock. The lock is held only
/// for the memcpy of the requested events.
pub struct EventRing {
    inner: Mutex<Inner>,
    bytes_capacity: u64,
    events_capacity: usize,
    produce_count: AtomicU64,
}

pub type SharedRing = Arc<EventRing>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RingStats {
    pub bytes_capacity: u64,
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
}

impl EventRing {
    pub fn with_bytes(bytes: u64, spill_dir: Option<&Path>) -> Result<Self, RingError> {
        let events_capacity = (bytes as usize) / LIVE_EVENT_SIZE;
        if events_capacity == 0 {
            return Err(RingError::CapacityTooSmall);
        }
        let spill = match spill_dir {
            Some(dir) if !dir.as_os_str().is_empty() => Some(SpillWriter::create(dir)?),
            _ => None,
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                buf: vec![LiveEvent::default(); events_capacity],
                head: 0,
                tail: 0,
                dropped: 0,
                spilled: 0,
                spill_errors: 0,
                spill,
            }),
            bytes_capacity: (events_capacity * LIVE_EVENT_SIZE) as u64,
            events_capacity,
            produce_count: AtomicU64::new(0),
        })
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
        let n = (end - start) as usize;
        out.reserve(n);
        for seq in start..end {
            out.push(inner.buf[inner.slot(seq)]);
        }
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
        out.reserve((inner.head - cursor) as usize);
        for seq in cursor..inner.head {
            out.push(inner.buf[inner.slot(seq)]);
        }
        Some(inner.head)
    }

    /// Snapshot of every live event in time/sequence order, plus the tail seq.
    pub fn snapshot(&self) -> (u64, Vec<LiveEvent>) {
        let inner = self.inner.lock();
        let mut out = Vec::with_capacity(inner.len());
        for seq in inner.tail..inner.head {
            out.push(inner.buf[inner.slot(seq)]);
        }
        (inner.tail, out)
    }

    pub fn stats(&self) -> RingStats {
        let inner = self.inner.lock();
        let mut oldest_start_ns = 0;
        let mut newest_end_ns = 0;
        if inner.len() > 0 {
            oldest_start_ns = inner.buf[inner.slot(inner.tail)].start_ns;
            newest_end_ns = inner.buf[inner.slot(inner.head - 1)].end_ns();
        }
        RingStats {
            bytes_capacity: self.bytes_capacity,
            events_capacity: self.events_capacity as u64,
            events_live: inner.len() as u64,
            head: inner.head,
            tail: inner.tail,
            dropped: inner.dropped,
            spilled: inner.spilled,
            spill_errors: inner.spill_errors,
            produced: self.produce_count.load(Ordering::Relaxed),
            oldest_start_ns,
            newest_end_ns,
        }
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

    pub fn bytes_capacity(&self) -> u64 {
        self.bytes_capacity
    }

    pub fn events_capacity(&self) -> usize {
        self.events_capacity
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
