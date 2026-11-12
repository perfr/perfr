// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Text too long for one record, split across a chain and put back together.
//!
//! A scope's name and a log line's text go through the same path. Most of
//! them fit the record; this is for the rest, and for a log line there is
//! nothing to intern against on the producer either way.
//!
//! Thirty-two bytes ride inline in the record itself, which covers most of them.
//! Anything longer continues into [`kind::TEXT`] records carrying the same
//! `(tid, scope_id)`, and the merged stream puts them back in order because
//! the thread wrote them one after another and timestamps only go forwards.
//! Nothing here depends on the chain landing in one ring: a thread that
//! migrates mid-name scatters its continuations across rings and they still
//! reassemble, because the key is the scope and not the ring.
//!
//! Splitting is by *bytes*, not by characters. A multi-byte character can be
//! cut in half by a chunk boundary; that is fine because the halves are only
//! decoded once rejoined. Decoding a lone chunk would be wrong, and nothing
//! here does it.

use crate::event::{flags, kind, ScopeEvent, INLINE_TEXT, MAX_NAME_BYTES};
use std::collections::HashMap;

/// Fills `head` with as much of `name` as fits and returns the continuation
/// records for the rest, in the order they must be written.
///
/// The head keeps its own kind; continuations are [`kind::TEXT`] and carry
/// their position in `depth`, starting at 1. A name past [`MAX_NAME_BYTES`]
/// -- the most a `u8` chain position can address -- is cut there, and the
/// last record is flagged [`flags::CUT`] so the reader knows. The bytes need
/// not be UTF-8; they are only ever sliced, and decoded once rejoined.
pub fn split_name(head: &mut ScopeEvent, name: impl AsRef<[u8]>) -> Vec<ScopeEvent> {
    continuations(head, name.as_ref()).collect()
}

/// As [`split_name`], with the continuation records made one at a time as
/// they are asked for. This is the form the producer uses: a `Vec` of them
/// was an allocation, usually a reallocation or two, and a free on the
/// write path of every scope whose name is longer than the record holds,
/// which is nearly every `__PRETTY_FUNCTION__` and nearly every log line.
pub fn continuations<'a>(head: &mut ScopeEvent, name: &'a [u8]) -> Continuations<'a> {
    let cut = name.len() > MAX_NAME_BYTES;
    let bytes = &name[..name.len().min(MAX_NAME_BYTES)];
    let first = bytes.len().min(INLINE_TEXT);
    head.text[..first].copy_from_slice(&bytes[..first]);
    head.text_len = first as u8;

    let rest = &bytes[first..];
    if rest.is_empty() {
        head.flags &= !flags::MORE_TEXT;
        if cut {
            head.flags |= flags::CUT;
        }
    } else {
        head.flags |= flags::MORE_TEXT;
    }
    Continuations {
        rest,
        index: 1,
        cut,
        timestamp_ns: head.timestamp_ns,
        scope_id: head.scope_id,
        tid: head.tid,
    }
}

/// The [`kind::TEXT`] records that carry what did not fit a head, in the
/// order they must be written. See [`continuations`].
pub struct Continuations<'a> {
    rest: &'a [u8],
    index: u32,
    cut: bool,
    timestamp_ns: u64,
    scope_id: u64,
    tid: u32,
}

impl Iterator for Continuations<'_> {
    type Item = ScopeEvent;

    fn next(&mut self) -> Option<ScopeEvent> {
        if self.rest.is_empty() {
            return None;
        }
        let take = self.rest.len().min(INLINE_TEXT);
        let mut chunk = ScopeEvent {
            timestamp_ns: self.timestamp_ns,
            scope_id: self.scope_id,
            tid: self.tid,
            kind: kind::TEXT,
            depth: self.index as u8,
            flags: 0,
            text_len: take as u8,
            text: [0; 32],
        };
        chunk.text[..take].copy_from_slice(&self.rest[..take]);
        self.rest = &self.rest[take..];
        if !self.rest.is_empty() {
            chunk.flags |= flags::MORE_TEXT;
        } else if self.cut {
            chunk.flags |= flags::CUT;
        }
        self.index += 1;
        Some(chunk)
    }
}

/// How a reassembled name ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    /// Every chunk arrived.
    Complete,
    /// The chain said more was coming and it never did, or a chunk in the
    /// middle was lost to an overwritten slot. The text is a prefix, or a
    /// prefix with a hole, and is reported as truncated rather than passed
    /// off as the whole name.
    Truncated,
}

/// A name being rebuilt from a chain.
#[derive(Clone, Debug, Default)]
struct Partial {
    bytes: Vec<u8>,
    /// The chain position last appended, so a gap is visible.
    last_index: u32,
    expecting_more: bool,
    saw_gap: bool,
}

/// The most names an assembler waits on at once. A real process has one in
/// flight per thread that is writing, plus those whose tails a lap took,
/// which the consumer clears on the loss.
pub const MAX_OPEN_CHAINS: usize = 1 << 16;

/// Reassembles chained names out of a merged, ordered event stream.
///
/// Feed it every event in timestamp order. Records that are not part of a
/// chain pass straight through untouched.
#[derive(Default)]
pub struct TextAssembler {
    open: HashMap<(u32, u64), Partial>,
}

impl TextAssembler {
    pub fn new() -> TextAssembler {
        TextAssembler::default()
    }

    /// Offers one event. Returns the finished name when this event completes
    /// a chain, along with whether anything was lost.
    ///
    /// What is held is bounded whatever the producer sends: a name is cut
    /// at [`MAX_NAME_BYTES`], and no more than [`MAX_OPEN_CHAINS`] are
    /// waited on at once. A head that arrives past that is returned at
    /// once, truncated, though it says more is coming.
    pub fn accept(&mut self, event: &ScopeEvent) -> Option<(String, Completeness)> {
        // A link's text is the target handle, not a name. Reading it as one
        // produced eight bytes of binary per link in the first dump of a real
        // program, so this is not hypothetical.
        if event.kind == kind::LINK {
            return None;
        }
        let key = (event.tid, event.scope_id);
        if event.kind == kind::TEXT {
            // A continuation whose head was never seen still gets an entry,
            // so that its last record ends it; with no room, it gets none.
            if self.open.len() >= MAX_OPEN_CHAINS && !self.open.contains_key(&key) {
                return None;
            }
            let partial = self.open.entry(key).or_default();
            // Chain positions run 1, 2, 3. Anything else means a slot was
            // overwritten before it was read.
            if u32::from(event.depth) != partial.last_index + 1 {
                partial.saw_gap = true;
            }
            partial.last_index = u32::from(event.depth);
            // No writer of ours sends more than this: the chain position is
            // a byte. One that does is not believed past it.
            let room = MAX_NAME_BYTES.saturating_sub(partial.bytes.len());
            let bytes = event.text_bytes();
            if bytes.len() > room {
                partial.saw_gap = true;
            }
            partial.bytes.extend_from_slice(&bytes[..bytes.len().min(room)]);
            partial.expecting_more = event.has_more_text();
            if partial.expecting_more {
                return None;
            }
            let partial = self.open.remove(&key)?;
            let cut = event.flags & flags::CUT != 0;
            let completeness = if partial.saw_gap || cut {
                Completeness::Truncated
            } else {
                Completeness::Complete
            };
            return Some((String::from_utf8_lossy(&partial.bytes).into_owned(), completeness));
        }

        if event.text_len == 0 {
            return None;
        }
        if !event.has_more_text() {
            // Fits in one record, the common case for a dynamic name.
            let completeness = if event.flags & flags::CUT != 0 {
                Completeness::Truncated
            } else {
                Completeness::Complete
            };
            return Some((String::from_utf8_lossy(event.text_bytes()).into_owned(), completeness));
        }
        // A head with continuations to come. With no room to wait for
        // them, it is finished as the prefix it holds.
        if self.open.len() >= MAX_OPEN_CHAINS && !self.open.contains_key(&key) {
            return Some((String::from_utf8_lossy(event.text_bytes()).into_owned(), Completeness::Truncated));
        }
        self.open.insert(
            key,
            Partial {
                bytes: event.text_bytes().to_vec(),
                last_index: 0,
                expecting_more: true,
                saw_gap: false,
            },
        );
        None
    }

    /// Names whose continuations never arrived, by `(tid, scope_id)`, with
    /// the prefix that did. Every one of them is truncated.
    ///
    /// A dropped tail is silent otherwise: the head is held waiting for a
    /// chunk that was overwritten before the consumer reached it, and without
    /// this the name would simply never appear.
    pub fn finish(&mut self) -> Vec<((u32, u64), String)> {
        let mut out: Vec<((u32, u64), Partial)> = self.open.drain().collect();
        out.sort_by_key(|(key, _)| *key);
        out.into_iter()
            .map(|(key, partial)| (key, String::from_utf8_lossy(&partial.bytes).into_owned()))
            .collect()
    }

    pub fn open_chains(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(scope_id: u64) -> ScopeEvent {
        ScopeEvent {
            timestamp_ns: 100,
            scope_id,
            tid: 7,
            kind: kind::SCOPE_START,
            ..ScopeEvent::default()
        }
    }

    fn round_trip(name: &str) -> (String, Completeness, usize) {
        let mut event = head(1);
        let chunks = split_name(&mut event, name);
        let mut assembler = TextAssembler::new();
        let mut result = assembler.accept(&event);
        for chunk in &chunks {
            if let Some(done) = assembler.accept(chunk) {
                result = Some(done);
            }
        }
        let (text, completeness) = result.expect("the chain completed");
        (text, completeness, chunks.len())
    }

    fn continuation(scope_id: u64, position: u8, more: bool) -> ScopeEvent {
        ScopeEvent {
            scope_id,
            tid: 7,
            kind: kind::TEXT,
            depth: position,
            flags: if more { flags::MORE_TEXT } else { 0 },
            text_len: INLINE_TEXT as u8,
            text: [b'z'; INLINE_TEXT],
            ..ScopeEvent::default()
        }
    }

    #[test]
    fn a_chain_that_never_ends_is_held_to_the_longest_name_there_is() {
        // A producer is another process, and may be broken or hostile. One
        // that goes on sending continuations for one name grew that name's
        // buffer in the service for as long as it cared to.
        let mut asm = TextAssembler::new();
        let mut first = head(1);
        first.text_len = INLINE_TEXT as u8;
        first.flags |= flags::MORE_TEXT;
        assert!(asm.accept(&first).is_none());
        for n in 1..=100_000u32 {
            assert!(asm.accept(&continuation(1, n as u8, true)).is_none());
        }
        let (text, completeness) = asm.accept(&continuation(1, 0, false)).expect("the chain ends");
        assert!(text.len() <= MAX_NAME_BYTES, "{} bytes of one name were kept", text.len());
        assert_eq!(completeness, Completeness::Truncated, "and it does not pass for the whole name");
    }

    #[test]
    fn names_whose_tails_never_come_are_not_waited_for_without_limit() {
        // Heads that promise more and never send it: each one is an entry
        // that only a loss or the end of the capture clears.
        let mut asm = TextAssembler::new();
        let mut refused = 0;
        for id in 0..(MAX_OPEN_CHAINS as u64 + 500) {
            let mut promising = head(id + 1);
            promising.text[..5].copy_from_slice(b"alpha");
            promising.text_len = 5;
            promising.flags |= flags::MORE_TEXT;
            if let Some((text, completeness)) = asm.accept(&promising) {
                // With no room to wait, a head is finished as what it
                // holds, and says it is not all there.
                assert_eq!((text.as_str(), completeness), ("alpha", Completeness::Truncated));
                refused += 1;
            }
        }
        assert_eq!(asm.open_chains(), MAX_OPEN_CHAINS);
        assert_eq!(refused, 500);
        // Continuations that arrive for no head at all get no entry either.
        assert!(asm.accept(&continuation(u64::MAX, 1, true)).is_none());
        assert_eq!(asm.open_chains(), MAX_OPEN_CHAINS);
        // A chain already open is not affected: it still completes.
        let (text, _) = asm.accept(&continuation(1, 1, false)).expect("an open chain still completes");
        assert!(text.starts_with("alpha"));
        assert_eq!(asm.open_chains(), MAX_OPEN_CHAINS - 1);
    }

    #[test]
    fn a_short_name_needs_no_continuation() {
        let (text, completeness, chunks) = round_trip("update");
        assert_eq!(text, "update");
        assert_eq!(completeness, Completeness::Complete);
        assert_eq!(chunks, 0, "one record is enough");
    }

    #[test]
    fn a_name_that_exactly_fills_the_record_needs_no_continuation() {
        let name = "a".repeat(INLINE_TEXT);
        let (text, _, chunks) = round_trip(&name);
        assert_eq!(text, name);
        assert_eq!(chunks, 0, "exactly full is still full, not overflowing");
    }

    #[test]
    fn one_byte_past_the_record_chains() {
        let name = "b".repeat(INLINE_TEXT + 1);
        let (text, completeness, chunks) = round_trip(&name);
        assert_eq!(text, name);
        assert_eq!(completeness, Completeness::Complete);
        assert_eq!(chunks, 1);
    }

    #[test]
    fn a_long_name_reassembles_exactly() {
        let name = "PhysicsStep(world=3, bodies=4096, substeps=4, continuous=true)";
        let (text, completeness, chunks) = round_trip(name);
        assert_eq!(text, name);
        assert_eq!(completeness, Completeness::Complete);
        assert_eq!(chunks, (name.len() - INLINE_TEXT).div_ceil(INLINE_TEXT));
    }

    #[test]
    fn a_multi_byte_character_split_across_chunks_survives() {
        // The boundary lands mid-character on purpose: chunks are bytes, and
        // decoding happens only after rejoining.
        let name = format!("{}\u{1F600}tail", "x".repeat(INLINE_TEXT - 2));
        let (text, completeness, _) = round_trip(&name);
        assert_eq!(text, name);
        assert_eq!(completeness, Completeness::Complete);
    }

    #[test]
    fn a_lost_middle_chunk_is_reported_as_truncated() {
        // The failure that matters: silently concatenating across a hole
        // would produce a plausible-looking name that is wrong.
        let name = "c".repeat(INLINE_TEXT * 4);
        let mut event = head(1);
        let chunks = split_name(&mut event, &name);
        assert!(chunks.len() >= 3);
        let mut assembler = TextAssembler::new();
        assembler.accept(&event);
        assembler.accept(&chunks[0]);
        // chunks[1] is dropped, as an overwritten slot would be.
        let mut last = None;
        for chunk in &chunks[2..] {
            if let Some(done) = assembler.accept(chunk) {
                last = Some(done);
            }
        }
        let (_, completeness) = last.expect("the chain still ends");
        assert_eq!(completeness, Completeness::Truncated);
    }

    #[test]
    fn a_chain_whose_tail_never_arrives_is_reported_at_the_end() {
        let name = "d".repeat(INLINE_TEXT * 3);
        let mut event = head(9);
        let chunks = split_name(&mut event, &name);
        let mut assembler = TextAssembler::new();
        assembler.accept(&event);
        assembler.accept(&chunks[0]);
        assert_eq!(assembler.open_chains(), 1);
        let leftover = assembler.finish();
        assert_eq!(leftover.len(), 1);
        assert_eq!(leftover[0].0, (7, 9), "keyed so the head can be found");
        assert!(leftover[0].1.starts_with("dddd"), "the prefix that did arrive is kept");
    }

    #[test]
    fn two_threads_naming_at_once_do_not_mix() {
        // The key is (tid, scope_id), so interleaved chains stay separate --
        // which is what makes a migration mid-name harmless.
        let mut a = ScopeEvent { tid: 1, ..head(1) };
        let mut b = ScopeEvent { tid: 2, ..head(1) };
        let a_chunks = split_name(&mut a, "A".repeat(INLINE_TEXT * 2));
        let b_chunks = split_name(&mut b, "B".repeat(INLINE_TEXT * 2));
        let mut assembler = TextAssembler::new();
        assembler.accept(&a);
        assembler.accept(&b);
        let mut done = Vec::new();
        for (x, y) in a_chunks.iter().zip(b_chunks.iter()) {
            if let Some(r) = assembler.accept(x) {
                done.push(r);
            }
            if let Some(r) = assembler.accept(y) {
                done.push(r);
            }
        }
        assert_eq!(done.len(), 2);
        assert!(done.iter().any(|(t, _)| t.chars().all(|c| c == 'A')));
        assert!(done.iter().any(|(t, _)| t.chars().all(|c| c == 'B')));
    }

    #[test]
    fn an_event_with_no_name_passes_through() {
        let event = head(1);
        let mut assembler = TextAssembler::new();
        assert_eq!(assembler.accept(&event), None, "nothing to assemble");
    }

    #[test]
    fn a_name_past_what_a_u8_chain_can_address_is_cut_and_says_so() {
        // 8 KiB is the structural limit, not a preference: 255 continuations
        // is all a u8 position can count. Beyond it the name is cut, and the
        // one thing that must not happen is the reader being told the
        // prefix is the whole name.
        let name = "n".repeat(MAX_NAME_BYTES + 1);
        let mut event = head(1);
        let chunks = split_name(&mut event, &name);
        assert_eq!(chunks.len(), 255, "every position a u8 can address, and no more");
        assert_eq!(chunks.last().unwrap().depth, 255);
        let (text, completeness, _) = round_trip(&name);
        assert_eq!(text.len(), MAX_NAME_BYTES);
        assert_eq!(completeness, Completeness::Truncated, "the reader is told");
    }

    #[test]
    fn a_name_exactly_at_the_limit_is_complete() {
        let name = "e".repeat(MAX_NAME_BYTES);
        let (text, completeness, chunks) = round_trip(&name);
        assert_eq!(text, name);
        assert_eq!(completeness, Completeness::Complete, "at the limit is not past it");
        assert_eq!(chunks, 255);
    }

    #[test]
    fn a_links_target_handle_is_not_mistaken_for_a_name() {
        let mut link = head(1);
        link.kind = kind::LINK;
        link.text_len = 8;
        link.text[..8].copy_from_slice(&0x4E5C_0000_0000_0001u64.to_le_bytes());
        let mut assembler = TextAssembler::new();
        assert_eq!(assembler.accept(&link), None, "handle bytes are not text");
    }
}
