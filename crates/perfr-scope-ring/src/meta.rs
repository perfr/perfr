// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Control-page tables that do not move the rings.
//!
//! The record layout and every ring offset are unchanged from segment
//! version 2. These tables occupy bytes of the control page that used to be
//! zero, past the reader-lease table. An older producer leaves them zero,
//! which a new reader treats as "no attributed drops". A new producer writes
//! them; an older reader never looks.
//!
//! Two tables:
//!
//! * **Read cursors.** A consumer publishes each ring's read claim so the
//!   producer can see, at the moment it claims a slot, whether that claim
//!   will overwrite an event the consumer has not read.
//! * **Drop counts.** When a claim will overwrite an unread slot, the
//!   producer reads the event still sitting there and adds one to that
//!   event's `(tid, kind)` counter. The lost event is counted as what it
//!   was, not as whatever replaced it. A record the producer gives up
//!   because its slot is still held by a writer a lap behind (see
//!   [`crate::ring::Rings::push`]) is counted here too, as itself.
//!
//! The drop table says *what* was lost. It is not the count of what a reader
//! lost: that is the reader's own lap count, which is exact. The table can
//! run ahead of it, for two reasons. The published cursor is always a little
//! behind the reader (it is stored once per drain), so a slot the reader has
//! copied but not yet published is counted when it is overwritten. And there
//! is one cursor per ring however many readers drain the segment: the last
//! one to publish wins, and the table then counts against that reader. A
//! consumer therefore uses the table to break its own loss down by thread and
//! kind, and never reports more than it lost itself.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ring::READER_LEASES_OFFSET;

/// First byte of the published read cursors. 32 rings × 8 bytes.
pub const READ_CURSOR_OFFSET: usize = 512;
pub const READ_CURSOR_SLOTS: usize = 32;

/// First byte of the per-thread drop table.
pub const DROP_TABLE_OFFSET: usize = 768;
pub const DROP_CELLS: usize = 32;
pub const DROP_CELL_STRIDE: usize = 40;
/// Kinds 0..=6 are the real record kinds. Slot 7 counts a loss whose record
/// could not be read back.
pub const DROP_KIND_SLOTS: usize = 8;
pub const DROP_KIND_UNKNOWN: u8 = 0xFF;

const _: () = assert!(READ_CURSOR_OFFSET >= READER_LEASES_OFFSET + 256);
const _: () = assert!(READ_CURSOR_OFFSET + READ_CURSOR_SLOTS * 8 <= DROP_TABLE_OFFSET);
const _: () = assert!(
    DROP_TABLE_OFFSET + DROP_CELLS * DROP_CELL_STRIDE <= 4096,
    "the tables have to fit the smallest control page"
);

/// One `(thread, kind)` loss total, as published on the control page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropTally {
    pub tid: u32,
    /// A real [`crate::event::kind`], or [`DROP_KIND_UNKNOWN`].
    pub kind: u8,
    pub count: u32,
}

#[repr(C)]
struct DropCell {
    tid: AtomicU32,
    counts: [AtomicU32; DROP_KIND_SLOTS],
}

const _: () = assert!(std::mem::size_of::<DropCell>() <= DROP_CELL_STRIDE);

fn cursor_ptr(base: *mut u8, ring: usize) -> *const AtomicU64 {
    // SAFETY: ring is bounded by the caller; the offset stays inside the
    // control page, which the asserts above pin under 4096.
    unsafe { base.add(READ_CURSOR_OFFSET + ring * 8).cast() }
}

fn drop_cell(base: *mut u8, index: usize) -> *const DropCell {
    unsafe { base.add(DROP_TABLE_OFFSET + index * DROP_CELL_STRIDE).cast() }
}

/// Publishes a ring's read claim. Stored as `read + 1` so zero still means
/// "the consumer has not said".
///
/// # Safety
/// `base` must be the writable control page of a scope segment.
pub unsafe fn publish_read_cursor(base: *mut u8, ring: usize, read: u64) {
    if ring >= READ_CURSOR_SLOTS {
        return;
    }
    // SAFETY: see `cursor_ptr`.
    unsafe { &*cursor_ptr(base, ring) }.store(read.wrapping_add(1), Ordering::Release);
}

/// The consumer's read claim, if it has published one.
///
/// # Safety
/// `base` must be a mapped control page.
pub unsafe fn published_read(base: *const u8, ring: usize) -> Option<u64> {
    if ring >= READ_CURSOR_SLOTS {
        return None;
    }
    // SAFETY: see `cursor_ptr`. The load is fine through a read-only map.
    let stored = unsafe { &*cursor_ptr(base as *mut u8, ring) }.load(Ordering::Acquire);
    stored.checked_sub(1)
}

fn kind_index(kind: u8) -> usize {
    let k = kind as usize;
    if k < DROP_KIND_SLOTS - 1 { k } else { DROP_KIND_SLOTS - 1 }
}

/// Adds one lost event of `(tid, kind)` to the drop table.
///
/// # Safety
/// `base` must be the writable control page.
pub unsafe fn note_drop(base: *mut u8, tid: u32, kind: u8) {
    let tid = if tid == 0 { u32::MAX } else { tid };
    let cell = unsafe { drop_cell_for(base, tid) };
    // SAFETY: `drop_cell_for` returns a cell inside the table.
    unsafe { &*cell }.counts[kind_index(kind)].fetch_add(1, Ordering::Relaxed);
}

unsafe fn drop_cell_for(base: *mut u8, tid: u32) -> *const DropCell {
    let start = (tid as usize) % (DROP_CELLS - 1);
    for step in 0..DROP_CELLS - 1 {
        let index = (start + step) % (DROP_CELLS - 1);
        let cell = drop_cell(base, index);
        // SAFETY: index is inside the table.
        let slot = unsafe { &*cell };
        let cur = slot.tid.load(Ordering::Acquire);
        if cur == tid {
            return cell;
        }
        if cur == 0
            && slot
                .tid
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return cell;
        }
        if slot.tid.load(Ordering::Acquire) == tid {
            return cell;
        }
    }
    // The directory is full. The last cell mixes every thread that did not
    // fit, still split by kind, so the loss is not silent.
    let overflow = drop_cell(base, DROP_CELLS - 1);
    unsafe { &*overflow }.tid.store(u32::MAX, Ordering::Relaxed);
    overflow
}

/// Every non-zero drop counter.
///
/// # Safety
/// `base` must be a mapped control page.
pub unsafe fn snapshot_drops(base: *const u8) -> Vec<DropTally> {
    let mut out = Vec::new();
    for index in 0..DROP_CELLS {
        // SAFETY: index is inside the table.
        let cell = unsafe { &*drop_cell(base as *mut u8, index) };
        let tid = cell.tid.load(Ordering::Acquire);
        if tid == 0 {
            continue;
        }
        for kind_i in 0..DROP_KIND_SLOTS {
            let count = cell.counts[kind_i].load(Ordering::Acquire);
            if count == 0 {
                continue;
            }
            let kind = if kind_i == DROP_KIND_SLOTS - 1 {
                DROP_KIND_UNKNOWN
            } else {
                kind_i as u8
            };
            let tid = if tid == u32::MAX { 0 } else { tid };
            out.push(DropTally { tid, kind, count });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_and_drops_round_trip_on_a_control_page() {
        let mut page = vec![0u8; 4096];
        let base = page.as_mut_ptr();
        unsafe {
            assert_eq!(published_read(base, 0), None, "zero means unpublished");
            publish_read_cursor(base, 0, 0);
            assert_eq!(published_read(base, 0), Some(0));
            publish_read_cursor(base, 3, 40);
            assert_eq!(published_read(base, 3), Some(40));

            note_drop(base, 7, 6);
            note_drop(base, 7, 6);
            note_drop(base, 9, 0);
            note_drop(base, 0, 0xFF);
            let mut got = snapshot_drops(base);
            got.sort_by_key(|d| (d.tid, d.kind));
            assert!(got.contains(&DropTally { tid: 7, kind: 6, count: 2 }));
            assert!(got.contains(&DropTally { tid: 9, kind: 0, count: 1 }));
            assert!(got.iter().any(|d| d.tid == 0 && d.kind == DROP_KIND_UNKNOWN && d.count == 1));
        }
    }
}
