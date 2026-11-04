// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Control-page tables that do not move the rings.
//!
//! The record layout and every ring offset are unchanged from segment
//! version 2. These tables occupy bytes of the control page that used to be
//! zero, past the reader-lease table. An older producer leaves them zero,
//! which a new reader treats as "no named channels" and "no attributed
//! drops". A new producer writes them; an older reader never looks.
//!
//! Two tables:
//!
//! * **Read cursors.** The consumer publishes each ring's read claim so the
//!   producer can see, at the moment it claims a slot, whether that claim
//!   will overwrite an event the consumer has not read.
//! * **Drop counts.** When a claim will overwrite an unread slot, the
//!   producer reads the event still sitting there and adds one to that
//!   event's `(tid, kind)` counter. The lost event is counted as what it
//!   was, not as whatever replaced it.
//! * **Channel names.** `register_channel` writes a name once. Scope enter
//!   stamps the id; the service resolves the name when it serializes the
//!   capture. Logs do not carry a channel id.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

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

/// First byte of the channel-name directory.
pub const CHANNEL_TABLE_OFFSET: usize = 2048;
pub const CHANNEL_SLOTS: usize = 48;
pub const CHANNEL_NAME_MAX: usize = 28;
pub const CHANNEL_SLOT_STRIDE: usize = 32;

const _: () = assert!(READ_CURSOR_OFFSET >= READER_LEASES_OFFSET + 256);
const _: () = assert!(READ_CURSOR_OFFSET + READ_CURSOR_SLOTS * 8 <= DROP_TABLE_OFFSET);
const _: () = assert!(DROP_TABLE_OFFSET + DROP_CELLS * DROP_CELL_STRIDE <= CHANNEL_TABLE_OFFSET);
const _: () = assert!(
    CHANNEL_TABLE_OFFSET + CHANNEL_SLOTS * CHANNEL_SLOT_STRIDE <= 4096,
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

#[repr(C)]
struct ChannelSlot {
    /// 0 while empty. Otherwise the channel id, which is `index + 1`.
    id: AtomicU16,
    len: AtomicU8,
    _pad: u8,
    /// Written before `id` is published. `UnsafeCell` because a reader may
    /// hold `&ChannelSlot` while registration fills the bytes.
    name: UnsafeCell<[u8; CHANNEL_NAME_MAX]>,
}

const _: () = assert!(std::mem::size_of::<ChannelSlot>() <= CHANNEL_SLOT_STRIDE);

fn cursor_ptr(base: *mut u8, ring: usize) -> *const AtomicU64 {
    // SAFETY: ring is bounded by the caller; the offset stays inside the
    // control page, which the asserts above pin under 4096.
    unsafe { base.add(READ_CURSOR_OFFSET + ring * 8).cast() }
}

fn drop_cell(base: *mut u8, index: usize) -> *const DropCell {
    unsafe { base.add(DROP_TABLE_OFFSET + index * DROP_CELL_STRIDE).cast() }
}

fn channel_slot(base: *mut u8, index: usize) -> *const ChannelSlot {
    unsafe { base.add(CHANNEL_TABLE_OFFSET + index * CHANNEL_SLOT_STRIDE).cast() }
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

/// Writes a channel name into the directory. `id` is 1-based and at most
/// [`CHANNEL_SLOTS`]. Returns false when the id or the name is empty.
///
/// # Safety
/// `base` must be the writable control page.
pub unsafe fn write_channel(base: *mut u8, id: u8, name: &[u8]) -> bool {
    if id == 0 || id as usize > CHANNEL_SLOTS || name.is_empty() {
        return false;
    }
    let take = name.len().min(CHANNEL_NAME_MAX);
    // SAFETY: id is inside the table. The bytes are published before the id
    // store, so a reader that observes the id observes the name.
    unsafe {
        let slot = &*channel_slot(base, id as usize - 1);
        let name_ptr = slot.name.get().cast::<u8>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), name_ptr, take);
        if take < CHANNEL_NAME_MAX {
            std::ptr::write_bytes(name_ptr.add(take), 0, CHANNEL_NAME_MAX - take);
        }
        slot.len.store(take as u8, Ordering::Relaxed);
        slot.id.store(u16::from(id), Ordering::Release);
    }
    true
}

/// Channel names the producer has published, in id order.
///
/// # Safety
/// `base` must be a mapped control page.
pub unsafe fn read_channels(base: *const u8) -> Vec<(u8, String)> {
    let mut out = Vec::new();
    for index in 0..CHANNEL_SLOTS {
        // SAFETY: index is inside the table.
        let slot = unsafe { &*channel_slot(base as *mut u8, index) };
        let id = slot.id.load(Ordering::Acquire);
        if id == 0 {
            continue;
        }
        let len = (slot.len.load(Ordering::Acquire) as usize).min(CHANNEL_NAME_MAX);
        // SAFETY: the acquire load of `id` pairs with the release store that
        // published the bytes; `len` is at most the array. `[u8; N]` is sized,
        // so the pointer is thin.
        let bytes = unsafe { std::slice::from_raw_parts(slot.name.get().cast::<u8>(), len) };
        let name = String::from_utf8_lossy(bytes).into_owned();
        out.push((id as u8, name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_drops_and_channels_round_trip_on_a_control_page() {
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

            assert!(write_channel(base, 1, b"simulation"));
            assert!(write_channel(base, 2, b"procedure"));
            assert!(!write_channel(base, 0, b"nope"));
            assert_eq!(
                read_channels(base),
                vec![(1, "simulation".into()), (2, "procedure".into())]
            );
        }
    }
}
