// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The consumer side of a perf ring buffer, twin of
//! `PerfEventRingBuffer.cpp`: the kernel writes `data_head` and reads
//! `data_tail`, we do the reverse, with acquire/release ordering on both.

use crate::attr::PerfEventAttr;
use crate::protocol::split_for_read;
use crate::sys;
use perfr_perf_records::{record_type, Lost};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

/// The head of `perf_event_mmap_page`, padded out to the `data_*` block that
/// sits 1024 bytes in. Never instantiated or referenced: the kernel keeps
/// writing the first kilobyte (`lock`, `index`, the `time_*` fields), which
/// a `&MetadataPage` would promise it does not. It exists for its field
/// offsets. `RingBuffer::from_mapping` checks `data_offset` and `data_size`
/// against what mmap was asked for, so a layout mistake here fails loudly at
/// open instead of corrupting reads.
#[repr(C)]
struct MetadataPage {
    _head: [u8; 1024],
    data_head: AtomicU64,
    data_tail: AtomicU64,
    data_offset: u64,
    data_size: u64,
}

pub struct RingBuffer {
    mmap: sys::Mmap,
    ring_size: u64,
    fd: i32,
    /// The one record per lap that straddles the end of the data area, put
    /// back together; every other record is handed out where it lies.
    wrapped: Vec<u8>,
}

// SAFETY: the mapping is owned by this value and all shared-memory accesses
// go through atomics; moving it to another thread is fine.
unsafe impl Send for RingBuffer {}

/// What one [`RingBuffer::drain`] read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drained {
    /// Records handed to the callback, `PERF_RECORD_LOST` ones included.
    pub records: u64,
    /// Records the kernel could not write because the ring was full, summed
    /// from the `PERF_RECORD_LOST` records in this drain.
    pub lost: u64,
}

impl RingBuffer {
    /// Opens the perf event described by `attr` and maps its ring buffer.
    /// `size_kb` must be a power of two of at least one page.
    pub fn open(attr: &PerfEventAttr, pid: i32, cpu: i32, size_kb: u64) -> io::Result<RingBuffer> {
        let page_size = sys::page_size();
        let ring_size = size_kb * 1024;
        if ring_size < page_size || !ring_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ring size must be a power of two of at least one page",
            ));
        }
        let fd = sys::perf_event_open(attr, pid, cpu, -1)?;
        let mmap = match sys::Mmap::ring(fd, (page_size + ring_size) as usize) {
            Ok(mmap) => mmap,
            Err(error) => {
                sys::close(fd);
                return Err(error);
            }
        };

        RingBuffer::from_mapping(mmap, ring_size, fd, page_size)
    }

    /// Wraps an open perf fd and its mapping, checking that the kernel laid
    /// the ring out as asked. Takes ownership of both, on failure too.
    fn from_mapping(mmap: sys::Mmap, ring_size: u64, fd: i32, page_size: u64) -> io::Result<RingBuffer> {
        let ring = RingBuffer { mmap, ring_size, fd, wrapped: Vec::new() };
        // SAFETY: the mapping is at least a page, which covers both fields;
        // they are read through raw pointers because the kernel owns the page.
        let (data_offset, data_size) = unsafe {
            let page = ring.mmap.address;
            (
                page.add(std::mem::offset_of!(MetadataPage, data_offset)).cast::<u64>().read_volatile(),
                page.add(std::mem::offset_of!(MetadataPage, data_size)).cast::<u64>().read_volatile(),
            )
        };
        if data_offset != page_size || data_size != ring_size {
            // No close here: `ring` owns the descriptor and its drop closes
            // it. A second close could land on a number another thread had
            // been handed in between.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "perf_event_mmap_page layout mismatch",
            ));
        }
        Ok(ring)
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn enable(&self) -> io::Result<()> {
        sys::perf_event_enable(self.fd)
    }

    /// `data_head`: the kernel writes it, we only load it.
    fn head(&self) -> &AtomicU64 {
        // SAFETY: the mapping is at least a page, which holds the field, and
        // lives as long as self; an atomic may be shared with another writer.
        unsafe { &*self.mmap.address.add(std::mem::offset_of!(MetadataPage, data_head)).cast::<AtomicU64>() }
    }

    /// `data_tail`: we write it, the kernel loads it.
    fn tail(&self) -> &AtomicU64 {
        // SAFETY: as for `head`.
        unsafe { &*self.mmap.address.add(std::mem::offset_of!(MetadataPage, data_tail)).cast::<AtomicU64>() }
    }

    fn data(&self) -> *const u8 {
        // SAFETY: the data area starts one page in, checked at open against
        // the kernel's own `data_offset`; the mapping is a page plus the ring.
        unsafe { self.mmap.address.add(self.mmap.length - self.ring_size as usize) }
    }

    /// How much of the ring holds unread data, 0.0 to 1.0.
    ///
    /// The consumer's early warning: a ring that stays near full is one the
    /// kernel is about to overwrite, and the events it overwrites are gone
    /// with no record but a `PERF_RECORD_LOST`. Cheap enough to sample every
    /// pass -- two atomic loads on a page we already touch.
    pub fn fill_fraction(&self) -> f32 {
        let head = self.head().load(Ordering::Acquire);
        let tail = self.tail().load(Ordering::Relaxed);
        (head.saturating_sub(tail) as f32 / self.ring_size as f32).min(1.0)
    }

    pub fn has_new_data(&self) -> bool {
        self.head().load(Ordering::Acquire) > self.tail().load(Ordering::Relaxed)
    }

    /// Copies `dest.len()` bytes starting at ring position `from` into
    /// `dest`. The caller has checked that they lie between tail and head.
    fn copy_out(&self, from: u64, dest: &mut [u8]) {
        let split = split_for_read(from, dest.len(), self.ring_size);
        let (first, second) = dest.split_at_mut(split.first_len);
        // SAFETY: split_for_read confines both segments to the ring, which
        // is mapped and lives as long as self.
        unsafe {
            sys::copy_from_ring(self.data().add(split.first_start), first);
            if split.second_len > 0 {
                sys::copy_from_ring(self.data(), second);
            }
        }
    }

    /// Copies `count` bytes at `offset` past the current tail into `dest`,
    /// without consuming. A read beyond the head is an error.
    fn read_at_offset_from_tail(&self, dest: &mut [u8], offset: u64) -> io::Result<()> {
        let head = self.head().load(Ordering::Acquire);
        let tail = self.tail().load(Ordering::Relaxed);
        let count = dest.len() as u64;
        if offset + count > head - tail || count > self.ring_size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past head"));
        }
        self.copy_out(tail + offset, dest);
        Ok(())
    }

    /// Hands every unread record to `each` -- its `perf_event_header::type`
    /// and the whole record, header included -- and consumes them.
    ///
    /// The record is lent where it lies in the ring: the kernel does not
    /// write between tail and head, and the tail moves past a record only
    /// after `each` has returned. Only a record that straddles the end of
    /// the data area is copied. The tail is published every eighth of the
    /// ring rather than per record: often enough that a slow callback (an
    /// unwind per record) keeps handing space back to the kernel as it goes,
    /// seldom enough that a ring of small records does not write the page
    /// the kernel is reading once per record.
    ///
    /// A record whose size cannot be right (below a header, or beyond the
    /// head) means the ring cannot be walked any further. Left there it
    /// would never be read past, and the ring would fill and stay full for
    /// the rest of the capture; so everything unread is skipped, and the
    /// error says so. The caller logs it; the next drain starts clean.
    pub fn drain(&mut self, mut each: impl FnMut(u32, &[u8])) -> io::Result<Drained> {
        let mut drained = Drained::default();
        let mut tail = self.tail().load(Ordering::Relaxed);
        let mut published = tail;
        let publish_every = self.ring_size / 8;
        let mut wrapped = std::mem::take(&mut self.wrapped);
        let result = loop {
            let head = self.head().load(Ordering::Acquire);
            if head <= tail {
                break Ok(());
            }
            let mut bad = None;
            while tail < head {
                let available = head - tail;
                let mut header = [0u8; 8];
                if available >= 8 {
                    self.copy_out(tail, &mut header);
                }
                let size = u64::from(u16::from_le_bytes([header[6], header[7]]));
                if size < 8 || size > available {
                    bad = Some((size, available));
                    break;
                }
                let start = (tail & (self.ring_size - 1)) as usize;
                let record: &[u8] = if start as u64 + size <= self.ring_size {
                    // SAFETY: start + size is inside the data area, and the
                    // bytes between tail and head are not written again
                    // until the tail has moved past them.
                    unsafe { std::slice::from_raw_parts(self.data().add(start), size as usize) }
                } else {
                    wrapped.resize(size as usize, 0);
                    self.copy_out(tail, &mut wrapped);
                    &wrapped
                };
                let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                if kind == record_type::LOST {
                    // A LOST record too short to carry its count still says
                    // that something was dropped.
                    drained.lost += Lost::parse(record).map_or(1, |lost| lost.lost);
                }
                each(kind, record);
                drained.records += 1;
                tail += size;
                if tail - published >= publish_every {
                    self.tail().store(tail, Ordering::Release);
                    published = tail;
                }
            }
            if let Some((size, available)) = bad {
                tail = head;
                break Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "a record of {size} bytes with {available} unread; skipped {available} bytes to resynchronise"
                    ),
                ));
            }
        };
        if tail != published {
            self.tail().store(tail, Ordering::Release);
        }
        self.wrapped = wrapped;
        result.map(|()| drained)
    }

    /// Reads and consumes the record at the tail into `buf`, reusing its
    /// allocation. `false` means the ring had nothing new. A 1 kHz stack
    /// sample is tens of kilobytes; allocating a fresh `Vec` per record
    /// fragments the heap for the life of a capture.
    pub fn read_record_into(&mut self, buf: &mut Vec<u8>) -> io::Result<bool> {
        if !self.has_new_data() {
            return Ok(false);
        }
        let mut header_bytes = [0u8; 8];
        self.read_at_offset_from_tail(&mut header_bytes, 0)?;
        let size = u16::from_le_bytes([header_bytes[6], header_bytes[7]]) as usize;
        if size < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "record smaller than header"));
        }
        buf.resize(size, 0);
        self.read_at_offset_from_tail(buf, 0)?;
        let tail = self.tail().load(Ordering::Relaxed);
        self.tail().store(tail + size as u64, Ordering::Release);
        Ok(true)
    }

    /// Reads and consumes the record at the tail: the whole record,
    /// `header.size` bytes, header included.
    pub fn read_record(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut record = Vec::new();
        if self.read_record_into(&mut record)? {
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        sys::close(self.fd);
    }
}

/// Convenience: open, enabled later by the caller. Mirrors the fd-per-kind
/// helpers of `PerfEventOpen.h` for the kinds the differential exercises.
pub fn open_mmap_task(pid: i32, cpu: i32, size_kb: u64) -> io::Result<RingBuffer> {
    RingBuffer::open(&crate::attr::mmap_task(), pid, cpu, size_kb)
}

pub fn open_stack_sample(
    period_ns: u64,
    stack_dump_size: u16,
    pid: i32,
    cpu: i32,
    size_kb: u64,
) -> io::Result<RingBuffer> {
    RingBuffer::open(&crate::attr::stack_sample(period_ns, stack_dump_size), pid, cpu, size_kb)
}

pub fn open_callchain_sample(
    period_ns: u64,
    stack_dump_size: u16,
    pid: i32,
    cpu: i32,
    size_kb: u64,
) -> io::Result<RingBuffer> {
    RingBuffer::open(&crate::attr::callchain_sample(period_ns, stack_dump_size), pid, cpu, size_kb)
}

pub fn open_context_switch(pid: i32, cpu: i32, size_kb: u64) -> io::Result<RingBuffer> {
    RingBuffer::open(&crate::attr::context_switch(), pid, cpu, size_kb)
}

/// Opens one kernel tracepoint. Per CPU (`pid = -1`) is the useful shape for
/// scheduling: a switch is a property of a core, not of the thread watching.
pub fn open_tracepoint(id: u64, pid: i32, cpu: i32, size_kb: u64) -> io::Result<RingBuffer> {
    RingBuffer::open(&crate::attr::tracepoint(id), pid, cpu, size_kb)
}

/// Opens one uprobe (or uretprobe) on a task.
///
/// `uprobe` owns the path the kernel reads during the syscall; it only has to
/// outlive this call, not the returned ring.
pub fn open_uprobe(
    uprobe: &crate::attr::UprobeAttr,
    pid: i32,
    cpu: i32,
    size_kb: u64,
) -> io::Result<RingBuffer> {
    RingBuffer::open(uprobe.attr(), pid, cpu, size_kb)
}

/// Opens a uprobe event that writes into `leader`'s ring
/// (`PERF_EVENT_IOC_SET_OUTPUT`) instead of getting a ring of its own: one
/// ring per CPU carries every probe. Returns the fd and the event id its
/// records carry as `stream_id`. Enabled by the caller.
pub fn open_uprobe_attached(
    uprobe: &crate::attr::UprobeAttr,
    pid: i32,
    cpu: i32,
    leader: &RingBuffer,
) -> io::Result<(i32, u64)> {
    let fd = sys::perf_event_open(uprobe.attr(), pid, cpu, -1)?;
    if let Err(error) = sys::perf_event_set_output(fd, leader.fd()) {
        sys::close(fd);
        return Err(error);
    }
    match sys::perf_event_id(fd) {
        Ok(id) => Ok((fd, id)),
        Err(error) => {
            sys::close(fd);
            Err(error)
        }
    }
}

/// The id a ring's own event stamps on its records.
pub fn event_id(ring: &RingBuffer) -> io::Result<u64> {
    sys::perf_event_id(ring.fd())
}

pub fn enable_fd(fd: i32) -> io::Result<()> {
    sys::perf_event_enable(fd)
}

pub fn close_fd(fd: i32) {
    sys::close(fd)
}

pub fn disable_fd(fd: i32) -> io::Result<()> {
    sys::perf_event_disable(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_perf_records::{record_type, PerfEventHeader};

    /// A ring this test writes the way the kernel does: a memfd laid out as
    /// a perf mapping (the metadata page, then the data area), so the reader
    /// runs over real shared memory without a perf event behind it.
    fn synthetic_ring(ring_size: u64) -> RingBuffer {
        let page_size = sys::page_size();
        let length = (page_size + ring_size) as usize;
        let fd = unsafe { libc::memfd_create(c"perfr-ring-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0);
        assert_eq!(unsafe { libc::ftruncate(fd, length as libc::off_t) }, 0);
        let mmap = sys::Mmap::ring(fd, length).unwrap();
        // `data_offset` and `data_size`, which the kernel fills at mmap.
        unsafe {
            mmap.address.add(1024 + 16).cast::<u64>().write(page_size);
            mmap.address.add(1024 + 24).cast::<u64>().write(ring_size);
        }
        RingBuffer::from_mapping(mmap, ring_size, fd, page_size).unwrap()
    }

    /// The kernel's side of the protocol: copy the bytes in at `data_head`,
    /// wrapping at the end of the data area, then publish the new head.
    fn publish(ring: &RingBuffer, bytes: &[u8]) {
        let head = ring.head().load(Ordering::Relaxed);
        let tail = ring.tail().load(Ordering::Acquire);
        assert!(head - tail + bytes.len() as u64 <= ring.ring_size, "the test overfilled its ring");
        for (i, byte) in bytes.iter().enumerate() {
            let at = ((head + i as u64) & (ring.ring_size - 1)) as usize;
            unsafe { ring.data().cast_mut().add(at).write(*byte) };
        }
        ring.head().store(head + bytes.len() as u64, Ordering::Release);
    }

    /// A record of `size` bytes whose payload is a function of `seed`, so a
    /// read that returned the wrong bytes cannot compare equal.
    fn record(kind: u32, size: usize, seed: u8) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(size as u16).to_le_bytes());
        bytes.extend((8..size).map(|i| seed.wrapping_mul(31).wrapping_add(i as u8)));
        bytes
    }

    #[test]
    fn an_empty_ring_reads_nothing() {
        let mut ring = synthetic_ring(4096);
        assert!(!ring.has_new_data());
        assert_eq!(ring.read_record().unwrap(), None);
        assert_eq!(ring.fill_fraction(), 0.0);
    }

    #[test]
    fn records_come_back_whole_wherever_they_wrap() {
        // Sizes that are not divisors of the ring, so over a few laps a
        // record straddles the end at every 8-byte offset -- header split
        // from payload, payload split in two, header itself split.
        let mut ring = synthetic_ring(4096);
        let mut wrapped = 0;
        for i in 0..2_000u32 {
            let size = 8 + 8 * (i as usize % 13);
            let written = record(record_type::SAMPLE, size, i as u8);
            let tail = ring.tail().load(Ordering::Relaxed);
            if (tail & 4095) + size as u64 > 4096 {
                wrapped += 1;
            }
            publish(&ring, &written);
            assert_eq!(ring.read_record().unwrap().as_deref(), Some(&written[..]), "record {i}");
            assert_eq!(ring.read_record().unwrap(), None);
        }
        assert!(wrapped > 10, "only {wrapped} records wrapped; the sizes no longer exercise it");
    }

    #[test]
    fn several_records_are_read_in_the_order_written() {
        let mut ring = synthetic_ring(4096);
        // Start near the end so the batch itself crosses it.
        publish(&ring, &record(record_type::SAMPLE, 4000, 0));
        ring.read_record().unwrap().unwrap();
        let batch: Vec<Vec<u8>> = (0..20).map(|i| record(record_type::SAMPLE, 24 + 8 * (i % 3), i as u8)).collect();
        for written in &batch {
            publish(&ring, written);
        }
        assert!(ring.fill_fraction() > 0.0);
        let mut buf = Vec::new();
        for written in &batch {
            assert!(ring.read_record_into(&mut buf).unwrap());
            assert_eq!(&buf, written);
        }
        assert!(!ring.read_record_into(&mut buf).unwrap());
        assert_eq!(ring.fill_fraction(), 0.0);
    }

    #[test]
    fn a_record_smaller_than_a_header_is_an_error_and_is_not_consumed() {
        let mut ring = synthetic_ring(4096);
        let mut bad = record(record_type::SAMPLE, 16, 1);
        bad[6..8].copy_from_slice(&4u16.to_le_bytes());
        publish(&ring, &bad);
        assert_eq!(ring.read_record().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(ring.has_new_data(), "an unreadable record must not be skipped silently");
    }

    #[test]
    fn a_record_longer_than_what_was_written_is_an_error_not_a_read_past_the_head() {
        let mut ring = synthetic_ring(4096);
        let mut bad = record(record_type::SAMPLE, 16, 1);
        bad[6..8].copy_from_slice(&64u16.to_le_bytes());
        publish(&ring, &bad);
        assert_eq!(ring.read_record().unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    fn lost_record(count: u64) -> Vec<u8> {
        let mut bytes = record(record_type::LOST, Lost::SIZE, 0);
        bytes[8..16].copy_from_slice(&77u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&count.to_le_bytes());
        bytes
    }

    #[test]
    fn a_drain_hands_out_every_record_in_order_wherever_they_wrap() {
        let mut ring = synthetic_ring(4096);
        let mut next = 0u32;
        let mut wrapped = 0;
        for batch in 0..400 {
            let mut written = Vec::new();
            for _ in 0..(1 + batch % 5) {
                let size = 8 + 8 * (next as usize % 13);
                written.push(record(record_type::SAMPLE + next % 3, size, next as u8));
                next += 1;
            }
            let mut at = ring.tail().load(Ordering::Relaxed);
            for bytes in &written {
                if (at & 4095) + bytes.len() as u64 > 4096 {
                    wrapped += 1;
                }
                at += bytes.len() as u64;
                publish(&ring, bytes);
            }
            let mut read: Vec<(u32, Vec<u8>)> = Vec::new();
            let drained = ring.drain(|kind, bytes| read.push((kind, bytes.to_vec()))).unwrap();
            assert_eq!(drained, Drained { records: written.len() as u64, lost: 0 });
            for ((kind, bytes), expected) in read.iter().zip(&written) {
                assert_eq!(bytes, expected);
                assert_eq!(*kind, u32::from_le_bytes(expected[0..4].try_into().unwrap()));
            }
            assert!(!ring.has_new_data());
        }
        assert!(wrapped > 10, "only {wrapped} records wrapped; the sizes no longer exercise it");
    }

    #[test]
    fn a_drain_adds_up_what_the_kernel_says_it_dropped() {
        let mut ring = synthetic_ring(4096);
        publish(&ring, &record(record_type::SAMPLE, 40, 1));
        publish(&ring, &lost_record(7));
        publish(&ring, &record(record_type::SAMPLE, 40, 2));
        publish(&ring, &lost_record(35));
        // Too short to carry a count: one at least was dropped.
        publish(&ring, &record(record_type::LOST, 16, 0));
        let mut kinds = Vec::new();
        let drained = ring.drain(|kind, _| kinds.push(kind)).unwrap();
        assert_eq!(drained, Drained { records: 5, lost: 43 });
        // The caller sees the loss where it happened in the stream too.
        assert_eq!(
            kinds,
            [record_type::SAMPLE, record_type::LOST, record_type::SAMPLE, record_type::LOST, record_type::LOST]
        );
    }

    #[test]
    fn a_drain_that_meets_an_impossible_record_skips_it_and_the_ring_reads_on() {
        for bad_size in [4u16, 64] {
            let mut ring = synthetic_ring(4096);
            publish(&ring, &record(record_type::SAMPLE, 24, 1));
            let mut bad = record(record_type::SAMPLE, 16, 2);
            bad[6..8].copy_from_slice(&bad_size.to_le_bytes());
            publish(&ring, &bad);
            let mut seen = 0;
            let error = ring.drain(|_, _| seen += 1).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(seen, 1, "the record before the bad one was still delivered");
            assert!(!ring.has_new_data(), "a ring left at the bad record would never be read again");

            let good = record(record_type::SAMPLE, 32, 3);
            publish(&ring, &good);
            let mut read = Vec::new();
            ring.drain(|_, bytes| read.push(bytes.to_vec())).unwrap();
            assert_eq!(read, [good]);
        }
    }

    #[test]
    fn a_long_drain_gives_space_back_before_it_ends() {
        let mut ring = synthetic_ring(4096);
        let records: Vec<Vec<u8>> = (0..60).map(|i| record(record_type::SAMPLE, 64, i)).collect();
        for bytes in &records {
            publish(&ring, bytes);
        }
        let tail: *const AtomicU64 = ring.tail();
        let mut freed_during = Vec::new();
        // SAFETY: the mapping outlives the drain.
        ring.drain(|_, _| freed_during.push(unsafe { &*tail }.load(Ordering::Relaxed))).unwrap();
        assert_eq!(freed_during[0], 0, "nothing is given back before a record has been handled");
        assert!(
            freed_during.iter().any(|freed| *freed >= 512),
            "the kernel saw a full ring for the whole drain"
        );
        assert!(
            freed_during.windows(2).filter(|pair| pair[0] != pair[1]).count() <= 8,
            "the tail was published more often than every eighth of the ring"
        );
        assert_eq!(ring.tail().load(Ordering::Relaxed), 60 * 64);
    }

    /// `cargo test --release -p perfr-perf-ring -- --ignored --nocapture reading_cost`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn reading_cost_per_record() {
        const RING: u64 = 1 << 20;
        for size in [48usize, 104, 4096, 64224] {
            let count = (RING as usize / 2) / size;
            let laps = (20_000_000 / count).clamp(4, 20_000);
            let bytes = record(record_type::SWITCH_CPU_WIDE, size, 3);
            let mut ring = synthetic_ring(RING);
            let mut sum = 0u64;
            let mut time = |label: &str, read_all: &mut dyn FnMut(&mut RingBuffer) -> u64| {
                let mut reading = std::time::Duration::ZERO;
                for _ in 0..laps {
                    for _ in 0..count {
                        publish(&ring, &bytes);
                    }
                    let start = std::time::Instant::now();
                    sum += read_all(&mut ring);
                    reading += start.elapsed();
                }
                eprintln!(
                    "{size:5}-byte records, {label:16} {:6.1} ns/record",
                    reading.as_nanos() as f64 / (laps * count) as f64
                );
            };
            time("read_record", &mut |ring| {
                let mut n = 0;
                while let Ok(Some(bytes)) = ring.read_record() {
                    n += u64::from(bytes[bytes.len() - 1]);
                }
                n
            });
            let mut buf = Vec::new();
            time("read_record_into", &mut |ring| {
                let mut n = 0;
                while ring.read_record_into(&mut buf).unwrap_or(false) {
                    n += u64::from(buf[buf.len() - 1]);
                }
                n
            });
            time("drain", &mut |ring| {
                let mut n = 0;
                ring.drain(|_, bytes| n += u64::from(bytes[bytes.len() - 1])).unwrap();
                n
            });
            assert!(sum > 0);
        }
    }

    // A self-observing smoke test: watch our own pid, do some mmaps, and
    // require that every record the kernel hands us parses and that mmap
    // records for a named mapping show up. Runs wherever
    // perf_event_paranoid permits self-observation; skips (with a message)
    // where it does not, like CI sandboxes.
    #[test]
    fn self_observation_produces_parseable_records() {
        // perf_event_open's pid argument is a thread id, and the test
        // harness runs this on a worker thread -- watching the process id
        // would watch the (idle) main thread and see nothing.
        let tid = unsafe { libc::gettid() };
        let mut ring = match open_mmap_task(tid, -1, 512) {
            Ok(ring) => ring,
            Err(error) => {
                eprintln!("skipping: perf_event_open not permitted here ({error})");
                return;
            }
        };
        ring.enable().unwrap();

        let path = std::env::temp_dir().join(format!("perfr-perf-ring-test-{tid}"));
        std::fs::write(&path, vec![0u8; 65536]).unwrap();
        {
            // Map a real file to get a PERF_RECORD_MMAP with a filename.
            let file = std::fs::File::open(&path).unwrap();
            let mapped = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    65536,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    0,
                )
            };
            assert_ne!(mapped, libc::MAP_FAILED);
            unsafe { libc::munmap(mapped, 65536) };
        }
        std::fs::remove_file(&path).unwrap();

        let mut records = 0;
        let mut named_mmaps = 0;
        for _ in 0..1000 {
            match ring.read_record().unwrap() {
                None => break,
                Some(bytes) => {
                    records += 1;
                    let header = PerfEventHeader::parse(&bytes).unwrap();
                    assert_eq!({ header.size } as usize, bytes.len());
                    if { header.kind } == record_type::MMAP {
                        let mmap = perfr_perf_records::reader::parse_mmap(&bytes).unwrap();
                        if !mmap.filename.is_empty() {
                            named_mmaps += 1;
                        }
                    }
                }
            }
        }
        assert!(records > 0, "the kernel produced no records");
        assert!(named_mmaps > 0, "no named mmap record seen");
    }

    /// A descriptor closed twice is only visible when another thread is
    /// handed the number between the two closes -- then the second close
    /// takes that thread's file. So one thread keeps opening descriptors and
    /// checking that they are still its own, while this one fails opens over
    /// a mapping whose metadata page does not match (a zero-filled memfd
    /// stands in for the pre-4.1 kernel that leaves `data_offset` unset).
    /// The window is narrow, hence the repetitions; a correct `open` cannot
    /// fail this however the threads interleave.
    #[test]
    fn a_layout_mismatch_closes_the_descriptor_once_and_nobody_elses() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;

        let page_size = sys::page_size();
        let ring_size = page_size;
        let length = (page_size + ring_size) as usize;

        let stop = Arc::new(AtomicBool::new(false));
        let stolen = Arc::new(AtomicU64::new(0));
        let opener = {
            let (stop, stolen) = (stop.clone(), stolen.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
                    assert!(fd >= 0);
                    // Long enough for a stray close that is already on its
                    // way to land.
                    for _ in 0..8 {
                        std::hint::spin_loop();
                    }
                    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
                        // Closed under us. The number may already be someone
                        // else's again, so it is not ours to close.
                        stolen.fetch_add(1, Ordering::Relaxed);
                    } else {
                        unsafe { libc::close(fd) };
                    }
                }
            })
        };

        for _ in 0..20_000 {
            let fd = unsafe { libc::memfd_create(c"perfr-ring-test".as_ptr(), libc::MFD_CLOEXEC) };
            assert!(fd >= 0);
            assert_eq!(unsafe { libc::ftruncate(fd, length as libc::off_t) }, 0);
            let mmap = sys::Mmap::ring(fd, length).unwrap();
            let error = RingBuffer::from_mapping(mmap, ring_size, fd, page_size).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            if stolen.load(Ordering::Relaxed) > 0 {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        opener.join().unwrap();
        assert_eq!(
            stolen.load(Ordering::Relaxed),
            0,
            "a failed open closed a descriptor that another thread had been given"
        );
    }
}
