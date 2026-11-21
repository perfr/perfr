// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bounded MPSC journal of return interceptions, in a separate mapping of
//! the lease file. No live target memory is read by the consumer.
use std::{
    fs::File,
    io,
    os::fd::AsRawFd,
    sync::atomic::{AtomicU64, Ordering},
};

pub const CAPACITY: usize = 16384;
const OFFSET: usize = 4096;
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
#[repr(C)]
struct Shared {
    write: AtomicU64,
    read: AtomicU64,
    lost: AtomicU64,
    unknown_return: AtomicU64,
    closed: AtomicU64,
    trampolines: [AtomicU64; 1024],
    slots: [Slot; CAPACITY],
}
pub const FILE_SIZE: usize = OFFSET + std::mem::size_of::<Shared>();
#[derive(Clone, Copy, Debug)]
pub struct Record {
    pub time: u64,
    pub tid: u32,
    pub sp: u64,
    pub original: u64,
    pub trampoline: u64,
    pub invocation: u64,
    pub entry: bool,
}
pub struct Journal {
    base: *mut libc::c_void,
}
// The mapping contains only atomics. One consumer; multiple writers.
unsafe impl Send for Journal {}
unsafe impl Sync for Journal {}
impl Journal {
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
    fn shared(&self) -> &Shared {
        // OFFSET is page-aligned; the file was zero-initialized before use.
        unsafe { &*((self.base as *const u8).add(OFFSET).cast::<Shared>()) }
    }
    pub fn close(&self) {
        self.shared().closed.store(1, Ordering::Release);
    }
    pub fn closed(&self) -> bool {
        self.shared().closed.load(Ordering::Acquire) != 0
    }
    pub fn invalidate(&self) {
        self.shared().lost.fetch_add(1, Ordering::Release);
    }
    pub fn unknown_return(&self) -> bool {
        self.shared().unknown_return.load(Ordering::Acquire) != 0
    }
    pub fn lost(&self) -> u64 {
        self.shared().lost.load(Ordering::Acquire)
    }
    pub fn trampolines(&self) -> Vec<u64> {
        self.shared()
            .trampolines
            .iter()
            .map(|v| v.load(Ordering::Acquire))
            .filter(|v| *v != 0)
            .collect()
    }
    /// Keep old trampolines recognizable when a call outlives its capture.
    pub fn register(&self, trampoline: u64) -> bool {
        if trampoline == 0 {
            self.shared().unknown_return.store(1, Ordering::Release);
            self.invalidate();
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
        self.shared().unknown_return.store(1, Ordering::Release);
        self.invalidate();
        false
    }
    pub fn enter(&self, tid: u32, sp: u64, original: u64, trampoline: u64) -> u64 {
        if sp == 0 || original == 0 || trampoline == 0 {
            self.shared().unknown_return.store(1, Ordering::Release);
            self.invalidate();
            return 0;
        }
        if !self.register(trampoline) {
            return 0;
        }
        self.push(tid, sp, original, trampoline, 0)
    }
    pub fn leave(&self, invocation: u64) {
        if invocation != 0 {
            self.push(0, 0, 0, 0, invocation);
        }
    }
    fn push(&self, tid: u32, sp: u64, original: u64, trampoline: u64, invocation: u64) -> u64 {
        let shared = self.shared();
        if self.lost() != 0 {
            return 0;
        }
        let mut write = shared.write.load(Ordering::Relaxed);
        loop {
            if write >= u64::MAX - 1
                || write.saturating_sub(shared.read.load(Ordering::Acquire)) >= CAPACITY as u64
            {
                self.invalidate();
                return 0;
            }
            match shared.write.compare_exchange_weak(
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
        let slot = &shared.slots[write as usize % CAPACITY];
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
        slot.kind
            .store(u64::from(invocation == 0), Ordering::Relaxed);
        slot.sequence.store(write + 1, Ordering::Release);
        write + 1
    }
    /// Return a safe sample-time cutoff only if every reservation visible
    /// after that cutoff was committed. A preempted writer holds the old
    /// watermark, never causes a sample to use incomplete history.
    pub fn drain(&self, records: &mut Vec<Record>) -> Option<u64> {
        let cutoff = crate::profile::now_ns();
        let shared = self.shared();
        let head = shared.write.load(Ordering::Acquire);
        let mut read = shared.read.load(Ordering::Relaxed);
        while read < head {
            let slot = &shared.slots[read as usize % CAPACITY];
            if slot.sequence.load(Ordering::Acquire) != read + 1 {
                return None;
            }
            records.push(Record {
                time: slot.time.load(Ordering::Relaxed),
                tid: slot.tid.load(Ordering::Relaxed) as u32,
                sp: slot.sp.load(Ordering::Relaxed),
                original: slot.original.load(Ordering::Relaxed),
                trampoline: slot.trampoline.load(Ordering::Relaxed),
                invocation: slot.invocation.load(Ordering::Relaxed),
                entry: slot.kind.load(Ordering::Relaxed) != 0,
            });
            read += 1;
            shared.read.store(read, Ordering::Release);
        }
        Some(cutoff)
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
        f.set_len(FILE_SIZE as u64).unwrap();
        let j = Journal::open(&f).unwrap();
        (f, j)
    }
    #[test]
    fn concurrent_publication_and_slot_reuse_are_complete() {
        let (_f, j) = journal();
        for _ in 0..3 {
            let done = std::sync::atomic::AtomicUsize::new(0);
            let mut records = Vec::new();
            std::thread::scope(|s| {
                for tid in 1..=4 {
                    let j = &j;
                    let done = &done;
                    s.spawn(move || {
                        for _ in 0..1000 {
                            let id = j.enter(tid, 0x1000, 0x2000, 0x3000);
                            assert_ne!(id, 0);
                            j.leave(id);
                        }
                        done.fetch_add(1, Ordering::Release);
                    });
                }
                while done.load(Ordering::Acquire) != 4 {
                    j.drain(&mut records);
                    std::thread::yield_now();
                }
            });
            assert!(j.drain(&mut records).is_some());
            assert_eq!(records.len(), 8000);
            assert_eq!(j.lost(), 0);
            let entries: std::collections::HashSet<_> = records
                .iter()
                .filter(|r| r.entry)
                .map(|r| r.invocation)
                .collect();
            assert_eq!(entries.len(), 4000);
            assert!(records
                .iter()
                .filter(|r| !r.entry)
                .all(|r| entries.contains(&r.invocation)));
        }
    }
    #[test]
    fn a_reserved_but_unpublished_record_blocks_the_watermark() {
        let (_f, j) = journal();
        j.shared().write.store(1, Ordering::Release);
        assert_eq!(j.drain(&mut Vec::new()), None);
    }
    #[test]
    fn an_unregistered_trampoline_is_remembered_as_unknown() {
        let (_f, j) = journal();
        for address in 1..=1024 {
            assert!(j.register(address));
        }
        assert!(!j.register(1025));
        assert_ne!(j.lost(), 0);
        assert!(
            j.unknown_return(),
            "the next capture must not assume the registry is complete"
        );
    }

    #[test]
    fn overflow_invalidates_instead_of_overwriting_unread_history() {
        let (_f, j) = journal();
        for _ in 0..CAPACITY {
            assert_ne!(j.enter(1, 8, 16, 24), 0);
        }
        assert_eq!(j.enter(1, 8, 16, 24), 0);
        assert_ne!(j.lost(), 0);
        let mut records = Vec::new();
        j.drain(&mut records);
        assert_eq!(records.len(), CAPACITY);
    }
}
