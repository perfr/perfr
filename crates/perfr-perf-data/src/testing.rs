// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Writes a `perf.data` byte by byte, the way `perf record` lays one out,
//! for the tests of this crate and of the ones that read through it
//! (behind the `testing` feature). Not a writer for anything else: it
//! knows the fields the tests set and leaves the rest zero.

use crate::reader::sample_type::*;
use crate::{record, MAGIC};

pub const SAMPLE_ID_ALL: u64 = 1 << 18;
pub const FREQ: u64 = 1 << 10;
pub const CONTEXT_KERNEL: u64 = -128i64 as u64;
pub const CONTEXT_USER: u64 = -512i64 as u64;
pub const MISC_KERNEL: u16 = 1;
pub const MISC_USER: u16 = 2;

/// What `perf record -g` asks of its samples.
pub const CALLCHAIN_SAMPLE: u64 = IDENTIFIER | IP | TID | TIME | CALLCHAIN | PERIOD;

#[derive(Default, Clone)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    pub fn u64(mut self, v: u64) -> Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    pub fn raw(mut self, v: &[u8]) -> Self {
        self.0.extend(v);
        self
    }
    /// A name, zero-terminated and padded to eight bytes.
    pub fn name(mut self, v: &str) -> Self {
        self.0.extend(v.as_bytes());
        self.0.push(0);
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
        self
    }
}

/// A `perf_event_attr` of the current size (136 bytes).
pub fn attr(sample_type: u64, flags: u64, period: u64) -> Vec<u8> {
    let mut a = Bytes::default()
        .u32(0)
        .u32(136)
        .u64(0)
        .u64(period)
        .u64(sample_type)
        .u64(0)
        .u64(flags)
        .0;
    a.resize(136, 0);
    a
}

/// The `sample_id` block for an event of `sample_type` (TID, TIME and
/// IDENTIFIER are the ones the tests use).
fn trailer(sample_type: u64, pid: u32, tid: u32, time: u64, id: u64) -> Bytes {
    let mut b = Bytes::default();
    if sample_type & TID != 0 {
        b = b.u32(pid).u32(tid);
    }
    if sample_type & TIME != 0 {
        b = b.u64(time);
    }
    if sample_type & CPU != 0 {
        b = b.u32(3).u32(0);
    }
    if sample_type & IDENTIFIER != 0 {
        b = b.u64(id);
    }
    b
}

pub fn record(kind: u32, misc: u16, body: &[u8]) -> Vec<u8> {
    let size = u16::try_from(8 + body.len()).expect("a record fits 64 KiB");
    Bytes::default().u32(kind).u16(misc).u16(size).raw(body).0
}

/// The records of one event, all stamped with its id.
pub struct Event1 {
    pub sample_type: u64,
    pub id: u64,
}

impl Event1 {
    pub fn mmap2(
        &self,
        pid: u32,
        start: u64,
        len: u64,
        pgoff: u64,
        path: &str,
        time: u64,
    ) -> Vec<u8> {
        let body = Bytes::default()
            .u32(pid)
            .u32(pid)
            .u64(start)
            .u64(len)
            .u64(pgoff)
            .raw(&[0; 24])
            .u32(5)
            .u32(2)
            .name(path);
        record(
            record::MMAP2,
            MISC_USER,
            &body
                .raw(&trailer(self.sample_type, pid, pid, time, self.id).0)
                .0,
        )
    }

    /// The kernel's text, whose offset is the address of the symbol it
    /// was placed by, or a module, whose offset is zero.
    pub fn kernel_mmap(&self, start: u64, len: u64, path: &str) -> Vec<u8> {
        let pgoff = if path.starts_with("[kernel.kallsyms]") {
            start
        } else {
            0
        };
        let body = Bytes::default()
            .u32(u32::MAX)
            .u32(0)
            .u64(start)
            .u64(len)
            .u64(pgoff)
            .name(path);
        record(
            record::MMAP,
            MISC_KERNEL,
            &body.raw(&trailer(self.sample_type, 0, 0, 0, 0).0).0,
        )
    }

    pub fn comm(&self, pid: u32, tid: u32, name: &str, exec: bool, time: u64) -> Vec<u8> {
        let body = Bytes::default().u32(pid).u32(tid).name(name);
        record(
            record::COMM,
            if exec { 1 << 13 } else { 0 },
            &body
                .raw(&trailer(self.sample_type, pid, tid, time, self.id).0)
                .0,
        )
    }

    pub fn fork(&self, pid: u32, ppid: u32, tid: u32, ptid: u32, time: u64) -> Vec<u8> {
        let body = Bytes::default()
            .u32(pid)
            .u32(ppid)
            .u32(tid)
            .u32(ptid)
            .u64(time);
        record(
            record::FORK,
            0,
            &body
                .raw(&trailer(self.sample_type, ppid, ptid, time, self.id).0)
                .0,
        )
    }

    pub fn switch(&self, pid: u32, tid: u32, time: u64, out: bool, preempt: bool) -> Vec<u8> {
        let misc = if out { 1 << 13 } else { 0 } | if preempt { 1 << 14 } else { 0 };
        record(
            record::SWITCH,
            misc,
            &trailer(self.sample_type, pid, tid, time, self.id).0,
        )
    }

    /// A sample of this event's layout; `chain` is the raw call chain,
    /// markers included.
    pub fn sample(&self, pid: u32, tid: u32, time: u64, ip: u64, chain: &[u64]) -> Vec<u8> {
        let has = |bit: u64| self.sample_type & bit != 0;
        let mut b = Bytes::default();
        if has(IDENTIFIER) {
            b = b.u64(self.id);
        }
        if has(IP) {
            b = b.u64(ip);
        }
        if has(TID) {
            b = b.u32(pid).u32(tid);
        }
        if has(TIME) {
            b = b.u64(time);
        }
        if has(CPU) {
            b = b.u32(3).u32(0);
        }
        if has(PERIOD) {
            b = b.u64(1000);
        }
        if has(CALLCHAIN) {
            b = b.u64(chain.len() as u64);
            for address in chain {
                b = b.u64(*address);
            }
        }
        let kernel = chain.first() == Some(&CONTEXT_KERNEL);
        record(
            record::SAMPLE,
            if kernel { MISC_KERNEL } else { MISC_USER },
            &b.0,
        )
    }
}

pub fn finished_round() -> Vec<u8> {
    record(record::FINISHED_ROUND, 0, &[])
}

/// A `perf.data` file: header, ids, attributes, records, features.
pub fn file(
    attrs: &[(Vec<u8>, Vec<u64>)],
    records: &[Vec<u8>],
    features: &[(u32, Vec<u8>)],
) -> Vec<u8> {
    const HEADER: usize = 104;
    let attr_size = 136 + 16;
    let mut ids = Bytes::default();
    let mut table = Bytes::default();
    for (attr, attr_ids) in attrs {
        let at = HEADER + ids.0.len();
        for id in attr_ids {
            ids = ids.u64(*id);
        }
        table = table
            .raw(attr)
            .u64(at as u64)
            .u64(attr_ids.len() as u64 * 8);
    }
    let attrs_at = HEADER + ids.0.len();
    let data_at = attrs_at + table.0.len();
    let data: Vec<u8> = records.concat();
    let mut bitmap = [0u64; 4];
    for (bit, _) in features {
        bitmap[(*bit / 64) as usize] |= 1 << (bit % 64);
    }
    let mut out = Bytes::default()
        .raw(&MAGIC)
        .u64(HEADER as u64)
        .u64(attr_size as u64)
        .u64(attrs_at as u64)
        .u64(table.0.len() as u64)
        .u64(data_at as u64)
        .u64(data.len() as u64)
        .u64(0)
        .u64(0);
    for word in bitmap {
        out = out.u64(word);
    }
    out = out.raw(&ids.0).raw(&table.0).raw(&data);
    // The section table, then the sections, in bit order.
    let mut sorted: Vec<&(u32, Vec<u8>)> = features.iter().collect();
    sorted.sort_by_key(|(bit, _)| *bit);
    let mut at = out.0.len() + sorted.len() * 16;
    for (_, section) in &sorted {
        out = out.u64(at as u64).u64(section.len() as u64);
        at += section.len();
    }
    for (_, section) in &sorted {
        out = out.raw(section);
    }
    out.0
}

pub fn string_feature(text: &str) -> Vec<u8> {
    let padded = Bytes::default().name(text).0;
    Bytes::default().u32(padded.len() as u32).raw(&padded).0
}

pub fn build_id_entry(path: &str, id: &[u8; 20]) -> Vec<u8> {
    let body = Bytes::default()
        .u32(u32::MAX)
        .raw(id)
        .raw(&[0; 4])
        .name(path);
    record(0, MISC_USER, &body.0)
}

/// A zstd frame of raw blocks, which is the framing without the entropy
/// coding: a 1 KiB window, never ended, as `perf record -z` leaves it.
pub fn zstd_raw(data: &[u8], block: usize) -> Vec<u8> {
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00];
    for piece in data.chunks(block) {
        let header = (piece.len() as u32) << 3;
        out.extend(&header.to_le_bytes()[..3]);
        out.extend(piece);
    }
    out
}
