// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Log lines split across fixed 32-byte slots and put back together.
//!
//! A log line is a run of [`kind::LOG`] records, not a side buffer and not a
//! string arena. Each record is one slot. The chunks of one line share a
//! timestamp and a thread; the thread id is what orders two lines that happen
//! to share a timestamp when the rings are merged. Inside one thread the
//! single-producer ring already wrote the chunks in order, and a stable sort
//! on `(timestamp, tid)` keeps that order.
//!
//! What each chunk carries, so a chunk that arrives alone is still placeable:
//!
//! * `scope_id` low 16 bits: chunk index, from 0
//! * `scope_id` bits 16..32: total chunk count
//! * `scope_id` bits 32..64: message id, unique on this thread while the
//!   line is in flight
//! * flags bit 0 ([`flags::MORE_TEXT`]): cleared on the last chunk
//! * flags bit 1 ([`flags::CUT`]): set on the last chunk when the line was
//!   longer than [`MAX_LOG_BYTES`]
//! * flags bits 4..7: log level
//! * chunk 0's first four text bytes: the original byte length, little-endian
//!
//! The message bytes themselves are split on bytes, not characters. A
//! multibyte character cut in half is decoded only after the chunks are
//! joined. A missing chunk is a hole: the text is marked truncated and the
//! hole is visible, never closed up and presented as the whole line.

use crate::event::{flags, kind, ScopeEvent, INLINE_TEXT};
use crate::text::Completeness;
use std::collections::HashMap;

/// Bytes of the length prefix on chunk 0.
pub const LOG_LEN_HEADER: usize = 4;

/// How many chunks one line may use. Past this the line is cut and the last
/// chunk is flagged [`flags::CUT`]. 1024 chunks is about 32 KiB, which covers
/// a stack trace without letting one line fill a ring by itself.
pub const MAX_LOG_CHUNKS: usize = 1024;

/// The most message bytes one line can carry.
pub const MAX_LOG_BYTES: usize =
    (INLINE_TEXT - LOG_LEN_HEADER) + (MAX_LOG_CHUNKS - 1) * INLINE_TEXT;

/// Log levels. The values are the bits stored in the record and the live event.
pub mod level {
    pub const TRACE: u8 = 0;
    pub const DEBUG: u8 = 1;
    pub const INFO: u8 = 2;
    pub const WARN: u8 = 3;
    pub const ERROR: u8 = 4;
    pub const FATAL: u8 = 5;
}

const LEVEL_SHIFT: u8 = 4;

/// A reassembled log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogMessage {
    pub timestamp_ns: u64,
    pub tid: u32,
    pub message_id: u32,
    pub level: u8,
    pub message: String,
    pub completeness: Completeness,
}

#[derive(Clone, Copy, Debug)]
struct ChunkRef {
    index: u16,
    total: u16,
    message_id: u32,
    level: u8,
    last: bool,
    cut: bool,
}

fn decode_chunk(event: &ScopeEvent) -> Option<ChunkRef> {
    if event.kind != kind::LOG {
        return None;
    }
    let index = event.scope_id as u16;
    let total = (event.scope_id >> 16) as u16;
    let message_id = (event.scope_id >> 32) as u32;
    if total == 0 || u32::from(index) >= u32::from(total) {
        return None;
    }
    Some(ChunkRef {
        index,
        total,
        message_id,
        level: (event.flags >> LEVEL_SHIFT) & 0x7,
        last: event.flags & flags::MORE_TEXT == 0,
        cut: event.flags & flags::CUT != 0,
    })
}

fn pack_scope_id(message_id: u32, index: u16, total: u16) -> u64 {
    (u64::from(message_id) << 32) | (u64::from(total) << 16) | u64::from(index)
}

fn chunk_count(message_len: usize) -> usize {
    let first = INLINE_TEXT - LOG_LEN_HEADER;
    if message_len <= first {
        1
    } else {
        1 + (message_len - first).div_ceil(INLINE_TEXT)
    }
}

/// Writes `message` as one or more [`kind::LOG`] records, in order.
///
/// `emit` is called once per chunk and should push the record. Nothing is
/// buffered beyond the record being built: there is no per-thread arena.
/// A message longer than [`MAX_LOG_BYTES`] is cut, and the last record says so.
pub fn write_log(
    timestamp_ns: u64,
    tid: u32,
    message_id: u32,
    level: u8,
    message: &[u8],
    mut emit: impl FnMut(ScopeEvent),
) {
    let original = message.len() as u32;
    let cut = message.len() > MAX_LOG_BYTES;
    let message = &message[..message.len().min(MAX_LOG_BYTES)];
    let total = chunk_count(message.len()) as u16;
    let level = level & 0x7;

    let mut rest = message;
    for index in 0..total {
        let last = index + 1 == total;
        let mut flags_b = level << LEVEL_SHIFT;
        if !last {
            flags_b |= flags::MORE_TEXT;
        } else if cut {
            flags_b |= flags::CUT;
        }
        let mut text = [0u8; INLINE_TEXT];
        let take = if index == 0 {
            text[..4].copy_from_slice(&original.to_le_bytes());
            let n = rest.len().min(INLINE_TEXT - LOG_LEN_HEADER);
            text[LOG_LEN_HEADER..LOG_LEN_HEADER + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            LOG_LEN_HEADER + n
        } else {
            let n = rest.len().min(INLINE_TEXT);
            text[..n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            n
        };
        emit(ScopeEvent {
            timestamp_ns,
            scope_id: pack_scope_id(message_id, index, total),
            tid,
            kind: kind::LOG,
            depth: 0,
            flags: flags_b,
            text_len: take as u8,
            text,
        });
    }
}

#[derive(Default)]
struct Partial {
    total: u16,
    level: u8,
    timestamp_ns: u64,
    parts: Vec<Option<Vec<u8>>>,
    got: u16,
    cut: bool,
    saw_gap: bool,
    original_len: Option<u32>,
}

/// Reassembles log lines from a stream sorted by `(timestamp_ns, tid)`.
#[derive(Default)]
pub struct LogAssembler {
    open: HashMap<(u32, u32), Partial>,
}

impl LogAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_lines(&self) -> usize {
        self.open.len()
    }

    /// Offers one record. Returns the line when this record completes it.
    /// Records that are not log chunks are ignored.
    pub fn accept(&mut self, event: &ScopeEvent) -> Option<LogMessage> {
        let meta = decode_chunk(event)?;
        let key = (event.tid, meta.message_id);
        let partial = self.open.entry(key).or_default();
        if partial.total == 0 {
            partial.total = meta.total;
            partial.level = meta.level;
            partial.timestamp_ns = event.timestamp_ns;
            partial.parts.resize(meta.total as usize, None);
        }
        if meta.total != partial.total || meta.index as usize >= partial.parts.len() {
            partial.saw_gap = true;
            return None;
        }
        if partial.parts[meta.index as usize].is_none() {
            partial.got += 1;
        }
        let bytes = event.text_bytes().to_vec();
        if meta.index == 0 && bytes.len() >= LOG_LEN_HEADER {
            partial.original_len = Some(u32::from_le_bytes(bytes[..4].try_into().unwrap()));
            partial.parts[0] = Some(bytes[LOG_LEN_HEADER..].to_vec());
        } else {
            partial.parts[meta.index as usize] = Some(bytes);
        }
        if meta.cut {
            partial.cut = true;
        }
        let complete = meta.last || partial.got == partial.total;
        if !complete {
            return None;
        }
        let key = (event.tid, meta.message_id);
        self.finish_key(key)
    }

    /// Lines whose last chunk never arrived.
    pub fn finish(&mut self) -> Vec<LogMessage> {
        let keys: Vec<(u32, u32)> = self.open.keys().copied().collect();
        let mut out = Vec::new();
        for key in keys {
            if let Some(msg) = self.finish_key(key) {
                out.push(msg);
            }
        }
        out.sort_by(|a, b| {
            a.timestamp_ns
                .cmp(&b.timestamp_ns)
                .then(a.tid.cmp(&b.tid))
                .then(a.message_id.cmp(&b.message_id))
        });
        out
    }

    fn finish_key(&mut self, key: (u32, u32)) -> Option<LogMessage> {
        let partial = self.open.remove(&key)?;
        let mut raw = Vec::new();
        let mut hole = false;
        let mut kept = 0u32;
        for part in &partial.parts {
            match part {
                Some(bytes) => {
                    kept += bytes.len() as u32;
                    raw.extend_from_slice(bytes);
                }
                None => {
                    hole = true;
                    // U+FFFD, so a joined line cannot be mistaken for the
                    // bytes that were actually recorded.
                    raw.extend_from_slice("\u{FFFD}".as_bytes());
                }
            }
        }
        // `original_len` is the caller's byte count, which is larger than
        // `kept` when the line was cut to [`MAX_LOG_BYTES`]. A hole is a
        // missing chunk. Either one is truncated; a complete line matches.
        let cut_short = partial.original_len.is_some_and(|n| kept < n);
        let truncated = hole
            || partial.cut
            || partial.saw_gap
            || partial.got != partial.total
            || cut_short;
        let completeness = if truncated {
            Completeness::Truncated
        } else {
            Completeness::Complete
        };
        Some(LogMessage {
            timestamp_ns: partial.timestamp_ns,
            tid: key.0,
            message_id: key.1,
            level: partial.level,
            message: String::from_utf8_lossy(&raw).into_owned(),
            completeness,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(level: u8, message: &str) -> LogMessage {
        let mut chunks = Vec::new();
        write_log(1_000, 7, 3, level, message.as_bytes(), |e| chunks.push(e));
        let mut asm = LogAssembler::new();
        let mut done = None;
        for chunk in &chunks {
            if let Some(msg) = asm.accept(chunk) {
                done = Some(msg);
            }
        }
        done.expect("line completed")
    }

    #[test]
    fn an_empty_line_is_one_chunk_and_comes_back_empty() {
        let mut chunks = Vec::new();
        write_log(5, 1, 1, level::INFO, b"", |e| chunks.push(e));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text_len, LOG_LEN_HEADER as u8);
        let msg = round_trip(level::INFO, "");
        assert_eq!(msg.message, "");
        assert_eq!(msg.completeness, Completeness::Complete);
        assert_eq!(msg.level, level::INFO);
    }

    #[test]
    fn a_short_line_needs_no_continuation() {
        let msg = round_trip(level::WARN, "frame dropped");
        assert_eq!(msg.message, "frame dropped");
        assert_eq!(msg.completeness, Completeness::Complete);
        assert_eq!(msg.level, level::WARN);
    }

    #[test]
    fn a_message_that_fills_chunk_zero_exactly_is_one_record() {
        let message = "a".repeat(INLINE_TEXT - LOG_LEN_HEADER);
        let mut chunks = Vec::new();
        write_log(1, 1, 1, level::DEBUG, message.as_bytes(), |e| chunks.push(e));
        assert_eq!(chunks.len(), 1, "exactly full is still one slot");
        assert_eq!(chunks[0].flags & flags::MORE_TEXT, 0);
        assert_eq!(round_trip(level::DEBUG, &message).message, message);
    }

    #[test]
    fn one_byte_past_chunk_zero_starts_a_second_chunk() {
        let message = "b".repeat(INLINE_TEXT - LOG_LEN_HEADER + 1);
        let mut chunks = Vec::new();
        write_log(1, 1, 1, level::INFO, message.as_bytes(), |e| chunks.push(e));
        assert_eq!(chunks.len(), 2);
        assert_ne!(chunks[0].flags & flags::MORE_TEXT, 0);
        assert_eq!(chunks[1].flags & flags::MORE_TEXT, 0);
        assert_eq!((chunks[0].scope_id >> 16) as u16, 2, "total-length marker");
        assert_eq!(chunks[0].scope_id as u16, 0);
        assert_eq!(chunks[1].scope_id as u16, 1);
        assert_eq!(round_trip(level::INFO, &message).message, message);
    }

    #[test]
    fn a_message_that_fills_two_slots_exactly_does_not_grow_a_third() {
        let len = (INLINE_TEXT - LOG_LEN_HEADER) + INLINE_TEXT;
        let message = "c".repeat(len);
        let mut chunks = Vec::new();
        write_log(1, 1, 1, level::ERROR, message.as_bytes(), |e| chunks.push(e));
        assert_eq!(chunks.len(), 2);
        assert_eq!(round_trip(level::ERROR, &message).message, message);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        // 4-byte char placed so its first bytes end chunk 0.
        let head = "x".repeat(INLINE_TEXT - LOG_LEN_HEADER - 2);
        let message = format!("{head}\u{1F600}tail");
        let msg = round_trip(level::INFO, &message);
        assert_eq!(msg.message, message);
        assert_eq!(msg.completeness, Completeness::Complete);
    }

    #[test]
    fn a_lost_middle_chunk_is_truncated_and_not_silently_joined() {
        let message = "d".repeat((INLINE_TEXT - LOG_LEN_HEADER) + INLINE_TEXT * 3);
        let mut chunks = Vec::new();
        write_log(9, 4, 8, level::ERROR, message.as_bytes(), |e| chunks.push(e));
        assert!(chunks.len() >= 4);
        let mut asm = LogAssembler::new();
        asm.accept(&chunks[0]);
        asm.accept(&chunks[1]);
        // chunks[2] dropped.
        let mut last = None;
        for chunk in chunks.iter().skip(3) {
            if let Some(msg) = asm.accept(chunk) {
                last = Some(msg);
            }
        }
        let msg = last.expect("the line still ends");
        assert_eq!(msg.completeness, Completeness::Truncated);
        assert!(msg.message.contains('\u{FFFD}'), "the hole stays visible");
        assert_ne!(msg.message.replace('\u{FFFD}', ""), message);
    }

    #[test]
    fn a_line_whose_tail_never_arrives_is_reported_at_the_end() {
        let message = "e".repeat((INLINE_TEXT - LOG_LEN_HEADER) + INLINE_TEXT * 2);
        let mut chunks = Vec::new();
        write_log(9, 4, 8, level::TRACE, message.as_bytes(), |e| chunks.push(e));
        let mut asm = LogAssembler::new();
        asm.accept(&chunks[0]);
        assert_eq!(asm.open_lines(), 1);
        let left = asm.finish();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].completeness, Completeness::Truncated);
        assert!(left[0].message.starts_with("eee"));
    }

    #[test]
    fn two_threads_with_the_same_timestamp_do_not_mix() {
        let mut a = Vec::new();
        let mut b = Vec::new();
        let a_msg = "A".repeat((INLINE_TEXT - LOG_LEN_HEADER) + 8);
        let b_msg = "B".repeat((INLINE_TEXT - LOG_LEN_HEADER) + 8);
        write_log(50, 1, 1, level::INFO, a_msg.as_bytes(), |e| a.push(e));
        write_log(50, 2, 1, level::INFO, b_msg.as_bytes(), |e| b.push(e));
        // Merge the way the consumer does: timestamp, then thread id.
        let mut all = Vec::new();
        all.extend(a);
        all.extend(b);
        all.sort_by(|l, r| l.timestamp_ns.cmp(&r.timestamp_ns).then(l.tid.cmp(&r.tid)));
        let mut asm = LogAssembler::new();
        let mut done = Vec::new();
        for event in &all {
            if let Some(msg) = asm.accept(event) {
                done.push(msg);
            }
        }
        assert_eq!(done.len(), 2);
        assert_eq!(done[0].message, a_msg);
        assert_eq!(done[1].message, b_msg);
        assert!(done[0].tid < done[1].tid, "thread id breaks the timestamp tie");
    }

    #[test]
    fn two_lines_on_one_thread_at_one_timestamp_stay_in_write_order() {
        let mut all = Vec::new();
        write_log(50, 3, 1, level::INFO, b"first-line-is-short", |e| all.push(e));
        let second = "S".repeat((INLINE_TEXT - LOG_LEN_HEADER) + 4);
        write_log(50, 3, 2, level::WARN, second.as_bytes(), |e| all.push(e));
        // Stable sort: equal (timestamp, tid) keeps write order.
        all.sort_by(|l, r| l.timestamp_ns.cmp(&r.timestamp_ns).then(l.tid.cmp(&r.tid)));
        let mut asm = LogAssembler::new();
        let mut done = Vec::new();
        for event in &all {
            if let Some(msg) = asm.accept(event) {
                done.push(msg);
            }
        }
        assert_eq!(done.len(), 2);
        assert_eq!(done[0].message, "first-line-is-short");
        assert_eq!(done[1].message, second);
        assert_eq!(done[1].level, level::WARN);
    }

    #[test]
    fn a_line_past_the_cap_is_cut_and_says_so() {
        let message = "n".repeat(MAX_LOG_BYTES + 50);
        let mut chunks = Vec::new();
        write_log(1, 1, 1, level::FATAL, message.as_bytes(), |e| chunks.push(e));
        assert_eq!(chunks.len(), MAX_LOG_CHUNKS);
        assert_ne!(chunks.last().unwrap().flags & flags::CUT, 0);
        let msg = round_trip(level::FATAL, &message);
        assert_eq!(msg.completeness, Completeness::Truncated);
        assert_eq!(msg.message.len(), MAX_LOG_BYTES);
        assert_eq!(msg.level, level::FATAL);
    }

    #[test]
    fn a_line_exactly_at_the_cap_is_complete() {
        let message = "m".repeat(MAX_LOG_BYTES);
        let msg = round_trip(level::INFO, &message);
        assert_eq!(msg.message.len(), MAX_LOG_BYTES);
        assert_eq!(msg.completeness, Completeness::Complete);
    }
}
