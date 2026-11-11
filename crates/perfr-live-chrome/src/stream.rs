// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Stream-parse Chrome Trace Event Format JSON (array or `{traceEvents:[…]}`)
//! or a Perfetto proto trace (length-delimited `TracePacket`s).
//!
//! Bytes are pushed as they arrive. Complete events / packets are decoded one
//! at a time — never a `Vec<Value>` of the whole file. gzip is inflated a
//! slice at a time, as the parser asks for more: what is pushed ahead of the
//! parser waits compressed. zip is accepted when it holds a single
//! deflated/stored file. The format is sniffed from the first decoded bytes:
//! `[` / `{` is JSON, a `0x0a` packet tag is Perfetto.

use std::io::Write;

use flate2::write::MultiGzDecoder;
use crate::ingest::{ChromeEvent, ChromeIngestor, StackFrame, TimeUnit};
use crate::json::{parse_json_string, skip_ws, ValueScan};
use crate::id::FlexId;
use crate::perfetto::PerfettoState;
use crate::proto::{is_message, varint};

/// The parsed prefix of the buffer is not dropped before it is this long.
const COMPACT_EVERY: usize = 1 << 20;
/// Compressed input is inflated this much at a time, and only while fewer
/// decoded bytes than [`BACKLOG_TARGET`] wait for the parser. Inflating
/// every chunk as it was pushed ran far ahead of the parser (a frame takes
/// 2 MiB of gzip, which is 20-30 MB of JSON, and parses a quarter of that),
/// so most of a large `.json.gz` ended up decoded in one buffer; and a
/// single call inflated whatever a chunk held, up to a thousand times its
/// size. A slice bounds that to about 16 MB.
const INFLATE_SLICE: usize = 16 << 10;
const BACKLOG_TARGET: usize = 4 << 20;
/// What [`is_memory_dump`] looks for in an event's bytes before parsing
/// it: a memory dump (`"ph":"v"`) is parsed without its `args`, so a heap
/// snapshot is never built as a value tree.
const DUMP_SCAN: &[u8] = br#""ph""#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    ArrayEvents,
    ObjectKey,
    ObjectColon,
    AfterValue,
    Done,
}

/// What the decoded bytes turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Unknown,
    Json,
    Perfetto,
}

/// `Trace.packet` is field 1, length-delimited: every Perfetto file starts
/// with this byte.
const PACKET_TAG: u8 = 0x0a;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKey {
    None,
    TraceEvents,
    StackFrames,
    Samples,
    SystemTraceEvents,
    Other,
}

enum Decode {
    Identity,
    /// Streaming gzip (concatenated members), inflated into `raw` as the
    /// parser asks for it; see [`INFLATE_SLICE`].
    Gzip(Box<MultiGzDecoder<Vec<u8>>>),
    ZipPending,
    ZipDeflate {
        dec: Box<flate2::write::DeflateDecoder<Vec<u8>>>,
        remain: u64,
    },
    ZipStored {
        remain: u64,
    },
    /// The compressed stream or the zip entry has ended. What follows (a
    /// zip's central directory) is not trace data and is discarded: read as
    /// such, it raised "expected a packet tag" at the end of a Perfetto
    /// trace that had parsed in full.
    Ended,
}

/// Incremental Chrome-trace reader. Feed compressed or raw bytes with
/// [`push`], then [`pump`] into an [`ChromeIngestor`].
pub struct ChromeStream {
    raw: Vec<u8>,
    pos: usize,
    phase: Phase,
    pending: PendingKey,
    /// Where the scan of the value at `pos` has got to, when the value is
    /// not all here yet.
    scan: ValueScan,
    decode: Decode,
    /// Compressed input not inflated yet, from `packed_pos` on.
    packed: Vec<u8>,
    packed_pos: usize,
    /// The first bytes pushed, until there are enough of them to tell a
    /// gzip or zip container from plain text (`container_known`).
    magic_buf: Vec<u8>,
    container_known: bool,
    format: Format,
    perfetto: Option<Box<PerfettoState>>,
    eof: bool,
    /// The last pump stopped for want of input, not of budget.
    drained: bool,
    pub bytes_in: u64,
    pub bytes_decoded: u64,
    /// JSON events or Perfetto packets decoded so far (the progress line).
    pub events_seen: u64,
    /// Bytes of a JSON trace that were given up on: a last value the file
    /// ends in the middle of, or anything after the closing bracket.
    pub dropped_tail_bytes: u64,
    error: Option<String>,
}

impl Default for ChromeStream {
    fn default() -> Self {
        Self {
            raw: Vec::new(),
            pos: 0,
            phase: Phase::Start,
            pending: PendingKey::None,
            scan: ValueScan::default(),
            decode: Decode::Identity,
            packed: Vec::new(),
            packed_pos: 0,
            magic_buf: Vec::new(),
            container_known: false,
            format: Format::Unknown,
            perfetto: None,
            eof: false,
            drained: false,
            bytes_in: 0,
            bytes_decoded: 0,
            events_seen: 0,
            dropped_tail_bytes: 0,
            error: None,
        }
    }
}

impl ChromeStream {
    /// A stream for JSON text carried inside another trace. It is not
    /// sniffed: no gzip, no zip, and above all not a Perfetto trace, which
    /// could carry JSON text of its own and so on down.
    pub(crate) fn json_text() -> Self {
        Self {
            format: Format::Json,
            container_known: true,
            ..Self::default()
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The input has ended and all of it has been parsed or given up on:
    /// no later [`Self::pump`] returns anything. Also true after an error.
    pub fn is_done(&self) -> bool {
        self.error.is_some() || (self.input_ended() && self.phase == Phase::Done)
    }

    /// The last [`Self::pump`] parsed all it could of what has been pushed:
    /// the next one needs a push (or the end of input) first.
    ///
    /// This, not an empty batch, is when to stop pumping. A batch is the
    /// events one budget of steps emitted, and long runs of input emit
    /// none: metadata, begins waiting for their ends, packets nothing maps.
    pub fn needs_input(&self) -> bool {
        self.drained || self.error.is_some()
    }

    /// Sniffed from the first decoded bytes; `Unknown` until they arrive.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Bytes pushed and not parsed yet, decoded or still compressed.
    pub fn pending_bytes(&self) -> usize {
        self.decoded_backlog() + (self.packed.len() - self.packed_pos)
    }

    fn decoded_backlog(&self) -> usize {
        self.raw.len().saturating_sub(self.pos)
    }

    /// Every byte the caller will ever push has been decoded into `raw`.
    fn input_ended(&self) -> bool {
        self.eof && self.packed_pos == self.packed.len() && !matches!(self.decode, Decode::Gzip(_))
    }

    /// Append a file chunk. Detects gzip / zip from the first bytes.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.error.is_some() || chunk.is_empty() {
            return;
        }
        self.bytes_in += chunk.len() as u64;
        self.drained = false;
        match self.decode {
            Decode::Identity => {}
            Decode::Ended => return,
            _ => {
                self.packed.extend_from_slice(chunk);
                self.inflate(false);
                return;
            }
        }
        if self.container_known {
            self.raw.extend_from_slice(chunk);
            self.bytes_decoded += chunk.len() as u64;
            return;
        }
        // Held across pushes: a reader may hand over a byte at a time, and
        // a first push shorter than the magic used to be set aside here
        // and never looked at again.
        self.magic_buf.extend_from_slice(chunk);
        if self.magic_buf.len() >= 4 || looks_like_json(&self.magic_buf) {
            self.open_container();
        }
    }

    /// Decide what the first bytes are a container of, and release them.
    fn open_container(&mut self) {
        self.container_known = true;
        let held = std::mem::take(&mut self.magic_buf);
        if held.starts_with(&[0x1f, 0x8b]) {
            self.decode = Decode::Gzip(Box::new(MultiGzDecoder::new(Vec::new())));
        } else if held.starts_with(b"PK\x03\x04") {
            self.decode = Decode::ZipPending;
        } else {
            self.bytes_decoded += held.len() as u64;
            self.raw.extend_from_slice(&held);
            return;
        }
        self.packed = held;
        self.inflate(false);
    }

    /// Decode more of the compressed input into `raw`: while the decoded
    /// backlog is under its target, or, when the parser is `starved` (it
    /// cannot go on without bytes, however many it already holds: one value
    /// larger than the target), until it has been given some. Returns
    /// whether `raw` grew.
    fn inflate(&mut self, mut starved: bool) -> bool {
        self.compact();
        let before = self.bytes_decoded;
        while self.error.is_none() && (starved || self.decoded_backlog() < BACKLOG_TARGET) {
            let grown = self.bytes_decoded;
            if !self.inflate_slice() {
                break;
            }
            if self.bytes_decoded > grown {
                starved = false;
            }
        }
        if self.packed_pos == self.packed.len() {
            self.packed.clear();
            self.packed_pos = 0;
        } else if self.packed_pos >= self.packed.len() / 2 {
            self.packed.drain(..self.packed_pos);
            self.packed_pos = 0;
        }
        self.bytes_decoded > before
    }

    /// One step of the decoder: a slice of compressed input, a zip header,
    /// or the final flush once the input has ended. False when there is
    /// nothing it can do until more is pushed.
    fn inflate_slice(&mut self) -> bool {
        let input = &self.packed[self.packed_pos..];
        match &mut self.decode {
            Decode::Identity | Decode::Ended => {
                self.packed_pos = self.packed.len();
                false
            }
            Decode::Gzip(dec) => {
                let n = input.len().min(INFLATE_SLICE);
                let result = if n > 0 {
                    dec.write_all(&input[..n])
                } else if self.eof {
                    dec.try_finish()
                } else {
                    return false;
                };
                if let Err(e) = result {
                    self.error = Some(format!("gzip: {e}"));
                    return false;
                }
                self.packed_pos += n;
                let out = dec.get_mut();
                self.bytes_decoded += out.len() as u64;
                self.raw.extend_from_slice(out);
                out.clear();
                if n == 0 {
                    self.decode = Decode::Ended;
                }
                true
            }
            Decode::ZipPending => {
                if input.len() < 30 {
                    return self.drop_unusable_input();
                }
                if !input.starts_with(b"PK\x03\x04") {
                    self.error = Some("zip: expected local file header".into());
                    return false;
                }
                let u16_at = |i: usize| u16::from_le_bytes([input[i], input[i + 1]]);
                let method = u16_at(8);
                let comp =
                    u32::from_le_bytes([input[18], input[19], input[20], input[21]]) as u64;
                let header = 30 + u16_at(26) as usize + u16_at(28) as usize;
                if input.len() < header {
                    return self.drop_unusable_input();
                }
                let flags = u16_at(6);
                if flags & 0x8 != 0 && comp == 0 {
                    self.error = Some("zip: data-descriptor sizes not supported".into());
                    return false;
                }
                self.packed_pos += header;
                match method {
                    // An empty entry has nothing to wait for; left in a
                    // state that waits, the bytes after it are never read.
                    0 | 8 if comp == 0 => self.decode = Decode::Ended,
                    0 => self.decode = Decode::ZipStored { remain: comp },
                    8 => {
                        self.decode = Decode::ZipDeflate {
                            dec: Box::new(flate2::write::DeflateDecoder::new(Vec::new())),
                            remain: comp,
                        };
                    }
                    other => {
                        self.error = Some(format!("zip: unsupported method {other}"));
                        return false;
                    }
                }
                true
            }
            Decode::ZipStored { remain } => {
                let n = input.len().min(INFLATE_SLICE).min(usize::try_from(*remain).unwrap_or(usize::MAX));
                if n == 0 {
                    return false;
                }
                *remain -= n as u64;
                self.bytes_decoded += n as u64;
                self.raw.extend_from_slice(&input[..n]);
                self.packed_pos += n;
                if *remain == 0 {
                    self.decode = Decode::Ended;
                }
                true
            }
            Decode::ZipDeflate { dec, remain } => {
                let n = input.len().min(INFLATE_SLICE).min(usize::try_from(*remain).unwrap_or(usize::MAX));
                if n == 0 {
                    return false;
                }
                *remain -= n as u64;
                if let Err(e) = dec.write_all(&input[..n]) {
                    self.error = Some(format!("zip deflate: {e}"));
                    return false;
                }
                self.packed_pos += n;
                if *remain == 0 {
                    if let Err(e) = dec.try_finish() {
                        self.error = Some(format!("zip deflate finish: {e}"));
                        return false;
                    }
                }
                let out = dec.get_mut();
                self.bytes_decoded += out.len() as u64;
                self.raw.extend_from_slice(out);
                out.clear();
                if *remain == 0 {
                    self.decode = Decode::Ended;
                }
                true
            }
        }
    }

    /// A zip header the file ends inside of. Nothing can be made of those
    /// bytes; held, they would keep the load from ever finishing.
    fn drop_unusable_input(&mut self) -> bool {
        if self.eof {
            self.packed_pos = self.packed.len();
        }
        false
    }

    /// Signal end-of-file so gzip/zip can flush, and so a proto stream
    /// knows its last packet has arrived.
    pub fn finish_input(&mut self) {
        self.eof = true;
        self.drained = false;
        if !self.container_known {
            // A file of fewer than four bytes: it is what it is.
            self.open_container();
        }
        self.inflate(false);
    }

    /// Deserialize up to `budget` events into `ing`.
    pub fn pump(&mut self, ing: &mut ChromeIngestor, budget: usize) -> Vec<perfr_live_event::LiveEvent> {
        let mut out = Vec::new();
        if self.error.is_some() || budget == 0 {
            return out;
        }
        self.drained = false;
        for _ in 0..budget {
            match self.step(ing, &mut out) {
                Step::Event => {}
                Step::Done => {
                    self.drained = true;
                    break;
                }
                Step::NeedBytes => {
                    if self.inflate(true) {
                        continue;
                    }
                    self.drained = true;
                    // No more bytes are coming. What is left is a value the
                    // file ends inside of (the writer crashed or was killed)
                    // or one the scanner cannot read. Left in the buffer it
                    // kept the load unfinished for good: the open scopes,
                    // which are what such a trace is for, were never closed
                    // and drawn. The Perfetto path drops its own tail.
                    if self.input_ended() {
                        self.drop_tail();
                    }
                    break;
                }
            }
        }
        self.compact();
        out
    }

    fn drop_tail(&mut self) {
        self.dropped_tail_bytes += self.decoded_backlog() as u64;
        self.pos = self.raw.len();
        self.phase = Phase::Done;
        self.scan.reset();
    }

    /// Drop the parsed prefix of `raw`, once it is at least as long as what
    /// is left: the drain moves the remainder, and moving a backlog of
    /// hundreds of megabytes after every megabyte parsed was quadratic.
    fn compact(&mut self) {
        if self.pos < COMPACT_EVERY || self.pos < self.raw.len() - self.pos {
            return;
        }
        self.raw.drain(..self.pos);
        self.pos = 0;
    }

    fn step(&mut self, ing: &mut ChromeIngestor, out: &mut Vec<perfr_live_event::LiveEvent>) -> Step {
        if self.format == Format::Unknown {
            match self.sniff() {
                Some(Format::Perfetto) => {
                    self.format = Format::Perfetto;
                    self.perfetto = Some(Box::default());
                    ing.unit = TimeUnit::Nanos;
                }
                Some(f) => self.format = f,
                None => return Step::NeedBytes,
            }
        }
        if self.format == Format::Perfetto {
            return self.step_perfetto(ing, out);
        }
        self.pos = skip_ws(&self.raw, self.pos);
        if self.pos >= self.raw.len() {
            return if self.phase == Phase::Done {
                Step::Done
            } else {
                Step::NeedBytes
            };
        }
        match self.phase {
            Phase::Start => {
                match self.raw[self.pos] {
                    b'[' => {
                        self.pos += 1;
                        self.phase = Phase::ArrayEvents;
                        Step::Event
                    }
                    b'{' => {
                        self.pos += 1;
                        self.phase = Phase::ObjectKey;
                        Step::Event
                    }
                    other => {
                        self.error = Some(format!(
                            "chrome trace: expected [ or {{, got {}",
                            other as char
                        ));
                        Step::Done
                    }
                }
            }
            Phase::ArrayEvents => self.step_array(ing, out),
            Phase::ObjectKey => self.step_object_key(),
            Phase::ObjectColon => self.step_object_colon(ing, out),
            Phase::AfterValue => self.step_after_value(),
            Phase::Done => {
                // The document is closed; whatever follows is not part of it.
                self.drop_tail();
                Step::Done
            }
        }
    }

    fn step_array(&mut self, ing: &mut ChromeIngestor, out: &mut Vec<perfr_live_event::LiveEvent>) -> Step {
        self.pos = skip_ws(&self.raw, self.pos);
        if self.pos >= self.raw.len() {
            return Step::NeedBytes;
        }
        match self.raw[self.pos] {
            b']' => {
                self.pos += 1;
                // An array under any key (`traceEvents`, `samples`,
                // `systemTraceEvents`) closes back into the object. Only
                // the legacy top-level array, which has no key, ends the
                // document.
                if self.pending != PendingKey::None {
                    self.pending = PendingKey::None;
                    self.phase = Phase::AfterValue;
                } else {
                    self.phase = Phase::Done;
                }
                Step::Event
            }
            b',' => {
                self.pos += 1;
                Step::Event
            }
            _ => {
                let Some(end) = self.scan.end(&self.raw, self.pos) else {
                    return Step::NeedBytes;
                };
                let slice = &self.raw[self.pos..end];
                self.pos = end;
                match deserialize_event(slice) {
                    Ok(mut ev) => {
                        if self.pending == PendingKey::Samples
                            && ev.ph.as_deref().unwrap_or("").is_empty()
                        {
                            ev.ph = Some("P".into());
                        }
                        out.extend(ing.ingest(ev));
                        self.events_seen += 1;
                    }
                    Err(e) => {
                        if self.events_seen == 0 {
                            self.error = Some(format!("chrome event: {e}"));
                            return Step::Done;
                        }
                        ing.stats.malformed += 1;
                    }
                }
                Step::Event
            }
        }
    }

    fn step_object_key(&mut self) -> Step {
        self.pos = skip_ws(&self.raw, self.pos);
        if self.pos >= self.raw.len() {
            return Step::NeedBytes;
        }
        match self.raw[self.pos] {
            b'}' => {
                self.pos += 1;
                self.phase = Phase::Done;
                Step::Event
            }
            b',' => {
                self.pos += 1;
                Step::Event
            }
            b'"' => {
                let Some(end) = self.scan.end(&self.raw, self.pos) else {
                    return Step::NeedBytes;
                };
                let key = parse_json_string(&self.raw[self.pos..end]).unwrap_or_default();
                self.pos = end;
                self.pending = match key.as_str() {
                    "traceEvents" => PendingKey::TraceEvents,
                    "systemTraceEvents" => PendingKey::SystemTraceEvents,
                    "stackFrames" => PendingKey::StackFrames,
                    "samples" => PendingKey::Samples,
                    _ => PendingKey::Other,
                };
                self.phase = Phase::ObjectColon;
                Step::Event
            }
            other => {
                self.error = Some(format!("chrome object: expected key, got {}", other as char));
                Step::Done
            }
        }
    }

    fn step_object_colon(
        &mut self,
        ing: &mut ChromeIngestor,
        out: &mut Vec<perfr_live_event::LiveEvent>,
    ) -> Step {
        self.pos = skip_ws(&self.raw, self.pos);
        if self.pos >= self.raw.len() {
            return Step::NeedBytes;
        }
        if self.raw[self.pos] == b':' {
            self.pos += 1;
            self.pos = skip_ws(&self.raw, self.pos);
            if self.pos >= self.raw.len() {
                return Step::NeedBytes;
            }
        }
        match self.pending {
            PendingKey::TraceEvents | PendingKey::Samples | PendingKey::SystemTraceEvents => {
                if self.raw[self.pos] == b'[' {
                    self.pos += 1;
                    self.phase = Phase::ArrayEvents;
                    return Step::Event;
                }
                if self.pending == PendingKey::SystemTraceEvents && self.raw[self.pos] == b'"' {
                    let Some(end) = self.scan.end(&self.raw, self.pos) else {
                        return Step::NeedBytes;
                    };
                    if let Some(s) = parse_json_string(&self.raw[self.pos..end]) {
                        out.extend(ing.ingest_systrace_text(&s));
                    }
                    self.pos = end;
                    self.phase = Phase::AfterValue;
                    return Step::Event;
                }
                self.skip_value()
            }
            PendingKey::StackFrames => {
                let Some(end) = self.scan.end(&self.raw, self.pos) else {
                    return Step::NeedBytes;
                };
                if let Ok(map) =
                    serde_json::from_slice::<std::collections::HashMap<String, StackFrame>>(
                        &self.raw[self.pos..end],
                    )
                {
                    for (id, frame) in map {
                        ing.add_stack_frame(id, frame);
                    }
                }
                self.pos = end;
                self.phase = Phase::AfterValue;
                Step::Event
            }
            PendingKey::Other | PendingKey::None => self.skip_value(),
        }
    }

    fn skip_value(&mut self) -> Step {
        let Some(end) = self.scan.end(&self.raw, self.pos) else {
            // Nothing will be made of this value: what has been scanned
            // need not be kept until its end arrives.
            self.pos += self.scan.forget_scanned();
            return Step::NeedBytes;
        };
        self.pos = end;
        self.phase = Phase::AfterValue;
        Step::Event
    }

    fn step_after_value(&mut self) -> Step {
        self.pos = skip_ws(&self.raw, self.pos);
        if self.pos >= self.raw.len() {
            return Step::NeedBytes;
        }
        match self.raw[self.pos] {
            b',' => {
                self.pos += 1;
                self.phase = Phase::ObjectKey;
                Step::Event
            }
            b'}' => {
                self.pos += 1;
                self.phase = Phase::Done;
                Step::Event
            }
            _ => {
                self.phase = Phase::ObjectKey;
                Step::Event
            }
        }
    }
}

enum Step {
    Event,
    NeedBytes,
    Done,
}

impl ChromeStream {
    /// JSON starts with `[` / `{` after optional whitespace (and an optional
    /// byte-order mark); a Perfetto file starts with the packet tag. `None`
    /// while the bytes held do not say which.
    ///
    /// The tag is a newline, which JSON skips, and the length after it is
    /// `[` for a 91-byte first packet and `{` for a 123-byte one: "is there
    /// a JSON opener after the whitespace" took such a trace for JSON,
    /// buffered all of it and showed nothing. So an opener after a leading
    /// tag is put to the test: Perfetto if the bytes frame as a packet, a
    /// message from end to end with the next packet or the end of the file
    /// right behind it. JSON text does not do that.
    fn sniff(&mut self) -> Option<Format> {
        const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let ended = self.input_ended();
        let held = &self.raw[self.pos..];
        if held.starts_with(BOM) {
            self.pos += BOM.len();
        } else if !ended && BOM.starts_with(held) {
            // Empty, or the beginning of a mark.
            return None;
        }
        let b = &self.raw[self.pos..];
        if b.is_empty() {
            return ended.then_some(Format::Json);
        }
        let i = skip_ws(b, 0);
        let opener = i < b.len() && (b[i] == b'[' || b[i] == b'{');
        if b[0] != PACKET_TAG {
            // Whitespace alone says nothing yet; anything else is JSON's
            // to accept or to report.
            return (opener || ended || i < b.len() || b.len() >= 64).then_some(Format::Json);
        }
        if i >= b.len() && !ended {
            // Newlines so far: blank lines before JSON, or a packet tag and
            // the start of a length.
            return None;
        }
        if !opener {
            return Some(Format::Perfetto);
        }
        let Some((len, n)) = varint(b, 1) else {
            return Some(Format::Json);
        };
        let Some(end) = usize::try_from(len).ok().and_then(|len| (1 + n).checked_add(len)) else {
            return Some(Format::Json);
        };
        if b.len() <= end && !ended {
            // Not decidable before the claimed packet and the byte behind
            // it are here. A JSON file this short is rare and loses
            // nothing by waiting for its end.
            return None;
        }
        let framed = b.len() >= end && is_message(&b[1 + n..end]) && b.get(end).is_none_or(|&t| t == PACKET_TAG);
        Some(if framed { Format::Perfetto } else { Format::Json })
    }

    /// One `TracePacket`: `0x0a`, varint length, bytes. A tail that cannot
    /// complete at EOF (a truncated capture) is dropped, not an error.
    fn step_perfetto(&mut self, ing: &mut ChromeIngestor, out: &mut Vec<perfr_live_event::LiveEvent>) -> Step {
        if self.phase == Phase::Done {
            return Step::Done;
        }
        let rest = &self.raw[self.pos..];
        let header = if rest.is_empty() {
            None
        } else if rest[0] != PACKET_TAG {
            self.error = Some(format!(
                "perfetto trace: expected a packet tag, got {:#04x} at byte {}",
                rest[0],
                self.bytes_decoded - rest.len() as u64
            ));
            return Step::Done;
        } else {
            varint(rest, 1).and_then(|(len, n)| {
                let start = 1 + n;
                let end = start.checked_add(usize::try_from(len).ok()?)?;
                (end <= rest.len()).then_some((start, end))
            })
        };
        let Some((start, end)) = header else {
            if self.input_ended() {
                self.pos = self.raw.len();
                if let Some(p) = self.perfetto.as_mut() {
                    out.extend(p.finish(ing));
                }
                self.phase = Phase::Done;
                return Step::Done;
            }
            return Step::NeedBytes;
        };
        let packet = &self.raw[self.pos + start..self.pos + end];
        if let Some(p) = self.perfetto.as_mut() {
            out.extend(p.on_packet(packet, ing));
        }
        self.pos += end;
        self.events_seen += 1;
        Step::Event
    }
}

fn looks_like_json(b: &[u8]) -> bool {
    let i = skip_ws(b, 0);
    i < b.len() && (b[i] == b'[' || b[i] == b'{')
}

fn is_memory_dump(bytes: &[u8]) -> bool {
    // Cheap scan: `"ph":"v"` / `"ph": "v"` / `"ph":'v'` — ph is a short field.
    let Some(i) = find_sub(bytes, DUMP_SCAN) else {
        return false;
    };
    let rest = &bytes[i + DUMP_SCAN.len()..];
    let j = skip_ws(rest, 0);
    if j >= rest.len() || rest[j] != b':' {
        return false;
    }
    let k = skip_ws(rest, j + 1);
    matches!(rest.get(k..k + 3), Some(b"\"v\"" | b"'v'")) || rest.get(k) == Some(&b'v')
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn deserialize_event(bytes: &[u8]) -> Result<ChromeEvent, serde_json::Error> {
    if is_memory_dump(bytes) {
        return deserialize_dump_marker(bytes);
    }
    serde_json::from_slice(bytes)
}

fn deserialize_dump_marker(bytes: &[u8]) -> Result<ChromeEvent, serde_json::Error> {
    #[derive(serde::Deserialize)]
    struct Slim {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        ph: Option<String>,
        #[serde(default)]
        ts: Option<f64>,
        #[serde(default)]
        pid: Option<FlexId>,
        #[serde(default)]
        tid: Option<FlexId>,
    }
    let slim: Slim = serde_json::from_slice(bytes)?;
    Ok(ChromeEvent {
        name: slim.name,
        cat: None,
        ph: slim.ph,
        ts: slim.ts,
        dur: None,
        pid: slim.pid,
        tid: slim.tid,
        id: None,
        id2: None,
        args: None,
        s: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        sf: None,
        stack: None,
        lane: None,
    })
}

const HELPER_CHUNK: usize = 1 << 20;

/// Push `bytes` in 1 MiB chunks (so a gzip is inflated into the scanner
/// incrementally, as a reader would feed it) and parse them to the end.
fn pump_all(
    stream: &mut ChromeStream,
    ing: &mut ChromeIngestor,
    bytes: &[u8],
    mut emit: impl FnMut(Vec<perfr_live_event::LiveEvent>),
) -> Result<(), String> {
    for chunk in bytes.chunks(HELPER_CHUNK) {
        stream.push(chunk);
        while !stream.needs_input() {
            emit(stream.pump(ing, 64 * 1024));
        }
    }
    stream.finish_input();
    while !stream.is_done() {
        emit(stream.pump(ing, 64 * 1024));
    }
    match stream.error() {
        Some(e) => Err(e.to_string()),
        None => Ok(()),
    }
}

/// One-shot helper for tests and native tools: the ingestor after the whole
/// input, with the emitted events dropped.
pub fn ingest_bytes(bytes: &[u8]) -> Result<ChromeIngestor, String> {
    let mut stream = ChromeStream::default();
    let mut ing = ChromeIngestor::default();
    pump_all(&mut stream, &mut ing, bytes, drop)?;
    Ok(ing)
}

/// Ingest and collect emitted [`LiveEvent`]s (including finish()).
pub fn ingest_collect(bytes: &[u8]) -> Result<(ChromeIngestor, Vec<perfr_live_event::LiveEvent>), String> {
    let mut stream = ChromeStream::default();
    let mut ing = ChromeIngestor::default();
    let mut events = Vec::new();
    pump_all(&mut stream, &mut ing, bytes, |batch| events.extend(batch))?;
    // `finish` closes at the latest time the ingestor has seen.
    events.extend(ing.finish(1));
    Ok((ing, events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Drive a stream the way the viewer does: push, signal the end, pump,
    /// and call the load finished when the input is at its end and nothing
    /// is left to parse. Returns `None` when that never happens.
    fn load_like_the_viewer(bytes: &[u8]) -> Option<(ChromeIngestor, Vec<perfr_live_event::LiveEvent>)> {
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut events = Vec::new();
        stream.push(bytes);
        stream.finish_input();
        for _ in 0..64 {
            assert_eq!(stream.error(), None);
            events.extend(stream.pump(&mut ing, 48_000));
            if stream.pending_bytes() == 0 {
                let more = stream.pump(&mut ing, 48_000);
                if more.is_empty() {
                    events.extend(ing.finish(1));
                    return Some((ing, events));
                }
                events.extend(more);
            }
        }
        None
    }

    const OPEN_STACK: &str = r#"{"traceEvents":[
        {"name":"outer","ph":"B","ts":10,"pid":1,"tid":1},
        {"name":"inner","ph":"B","ts":20,"pid":1,"tid":1},
        {"name":"mark","ph":"i","ts":30,"pid":1,"tid":1,"args":{"text":"a ] } \" , b"}},
        {"name":"inner","ph":"E","ts":40,"pid":1,"tid":1},
        {"name":"last","ph":"B","ts":50,"pid":1,"tid":1}
    ],"displayTimeUnit":"ms","other":[1,2,{"k":"v"}]}"#;

    #[test]
    fn a_json_trace_cut_anywhere_still_finishes_and_shows_its_open_scopes() {
        let bytes = OPEN_STACK.as_bytes();
        let whole = load_like_the_viewer(bytes).expect("the whole file loads").1.len();
        assert_eq!(whole, 4, "outer, inner, mark, last");
        let outer_begins = OPEN_STACK.find(r#"{"name":"inner""#).unwrap();
        for cut in 0..bytes.len() {
            let Some((ing, events)) = load_like_the_viewer(&bytes[..cut]) else {
                panic!("cut at byte {cut}: the load never finishes");
            };
            // A process that died mid-write leaves exactly this: the scopes
            // it had open are what the trace is for.
            if cut >= outer_begins {
                assert!(
                    events.iter().any(|e| ing.intern.get(e.name_id) == Some("outer")),
                    "cut at byte {cut}: the open scope is missing"
                );
            }
        }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap()
    }

    /// Push `input` the way the viewer does (all of it here: it is under
    /// the 2 MiB a frame takes), pump a frame's budget at a time, and report
    /// the most decoded bytes that ever sat in the buffer.
    fn peak_decoded_backlog(input: &[u8]) -> (usize, u64) {
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        stream.push(input);
        stream.finish_input();
        let mut peak = stream.raw.len();
        for _ in 0..100_000 {
            let _ = stream.pump(&mut ing, 48_000);
            peak = peak.max(stream.raw.len());
            assert_eq!(stream.error(), None);
            if stream.pending_bytes() == 0 && stream.pump(&mut ing, 48_000).is_empty() {
                return (peak, stream.events_seen);
            }
        }
        panic!("the load never finishes");
    }

    /// A zip archive of one entry, with the central directory after it.
    fn zip_of(payload: &[u8], deflate: bool) -> Vec<u8> {
        let name = b"trace.json";
        let data = if deflate {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(payload).unwrap();
            enc.finish().unwrap()
        } else {
            payload.to_vec()
        };
        let mut z = Vec::new();
        z.extend_from_slice(b"PK\x03\x04");
        z.extend_from_slice(&[20, 0, 0, 0]); // version, flags
        z.extend_from_slice(&(if deflate { 8u16 } else { 0 }).to_le_bytes());
        z.extend_from_slice(&[0; 8]); // time, date, crc (not checked)
        z.extend_from_slice(&(data.len() as u32).to_le_bytes());
        z.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0, 0]); // extra length
        z.extend_from_slice(name);
        z.extend_from_slice(&data);
        z.extend_from_slice(b"PK\x01\x02");
        z.extend_from_slice(&[0; 42]);
        z.extend_from_slice(name);
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0; 18]);
        z
    }

    #[test]
    fn a_single_entry_zip_loads_stored_or_deflated() {
        let json = br#"[{"name":"a","ph":"i","ts":1,"pid":1,"tid":1},{"name":"b","ph":"i","ts":2,"pid":1,"tid":1}]"#;
        for deflate in [false, true] {
            let zip = zip_of(json, deflate);
            let (_, events) = crate::ingest_collect(&zip).unwrap_or_else(|e| panic!("deflate {deflate}: {e}"));
            assert_eq!(events.len(), 2, "deflate {deflate}");
            // An archive cut inside its header or its entry still ends.
            for cut in [3, 20, 35, zip.len() / 2] {
                let mut stream = ChromeStream::default();
                let mut ing = ChromeIngestor::default();
                stream.push(&zip[..cut]);
                stream.finish_input();
                let _ = stream.pump(&mut ing, 1024);
                assert!(stream.is_done(), "deflate {deflate}, cut at {cut}");
            }
            // The directory arriving in a push of its own is still not
            // trace data.
            let entry_end = zip.len() - (4 + 42 + 10 + 4 + 18);
            let mut stream = ChromeStream::default();
            let mut ing = ChromeIngestor::default();
            stream.push(&zip[..entry_end]);
            let mut n = stream.pump(&mut ing, 1024).len();
            stream.push(&zip[entry_end..]);
            stream.finish_input();
            n += stream.pump(&mut ing, 1024).len();
            assert_eq!((n, stream.error(), stream.pending_bytes()), (2, None, 0), "deflate {deflate}");
            assert_eq!(stream.dropped_tail_bytes, 0, "deflate {deflate}: the directory was read as JSON");
        }
        let empty = zip_of(b"", false);
        let (_, events) = crate::ingest_collect(&empty).expect("an empty entry is an empty trace");
        assert!(events.is_empty());
    }

    #[test]
    fn a_compressed_trace_is_not_inflated_ahead_of_the_parser() {
        const N: usize = 300_000;
        let mut json = String::with_capacity(N * 70);
        json.push('[');
        for i in 0..N {
            use std::fmt::Write as _;
            let comma = if i > 0 { "," } else { "" };
            write!(json, r#"{comma}{{"name":"scope","ph":"i","ts":{i},"pid":1,"tid":{}}}"#, i % 8).unwrap();
        }
        json.push(']');
        let gz = gzip(json.as_bytes());
        assert!(gz.len() < 2 << 20, "fixture must fit one frame's input budget: {}", gz.len());
        let t = std::time::Instant::now();
        let (peak, events) = peak_decoded_backlog(&gz);
        eprintln!(
            "{} MiB of JSON from {} KiB of gzip: peak decoded buffer {} KiB, {:?}",
            json.len() >> 20,
            gz.len() >> 10,
            peak >> 10,
            t.elapsed()
        );
        assert_eq!(events, N as u64);
        assert!(peak < 8 << 20, "{} MiB decoded and waiting", peak >> 20);
    }

    #[test]
    fn a_decompression_bomb_is_inflated_a_slice_at_a_time() {
        // 128 MiB of JSON whitespace in about 128 KiB of gzip.
        let mut text = vec![b' '; 128 << 20];
        text[0] = b'[';
        *text.last_mut().unwrap() = b']';
        let gz = gzip(&text);
        drop(text);
        let (peak, _) = peak_decoded_backlog(&gz);
        assert!(peak < 40 << 20, "{} MiB materialised at once", peak >> 20);
    }

    #[test]
    fn the_top_level_keys_load_in_any_order() {
        let keys = [
            r#""traceEvents":[{"name":"t","ph":"i","ts":1,"pid":1,"tid":1}]"#,
            r#""systemTraceEvents":[{"name":"s","ph":"i","ts":2,"pid":2,"tid":2}]"#,
            r#""samples":[{"name":"p","ts":3,"pid":3,"tid":3,"sf":"1"}]"#,
            r#""stackFrames":{"1":{"name":"leaf"}}"#,
        ];
        let mut orders = Vec::new();
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    for d in 0..4 {
                        let o = [a, b, c, d];
                        if (0..4).all(|k| o.contains(&k)) {
                            orders.push(o);
                        }
                    }
                }
            }
        }
        assert_eq!(orders.len(), 24);
        for o in orders {
            let text = format!("{{{}}}", o.map(|k| keys[k]).join(","));
            let (ing, events) =
                load_like_the_viewer(text.as_bytes()).unwrap_or_else(|| panic!("{text}: the load never finishes"));
            let mut names: Vec<&str> = events.iter().filter_map(|e| ing.intern.get(e.name_id)).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names, ["leaf", "s", "t"], "{text}");
        }
        // What a writer that sorts its keys produces when it has no samples.
        let text = r#"{"samples":[],"traceEvents":[{"name":"t","ph":"i","ts":1,"pid":1,"tid":1}]}"#;
        let (_, events) = load_like_the_viewer(text.as_bytes()).expect("finishes");
        assert_eq!(events.len(), 1);
    }

    /// One push per byte, the smallest a reader can hand over.
    fn load_byte_by_byte(bytes: &[u8]) -> Result<Vec<perfr_live_event::LiveEvent>, String> {
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut events = Vec::new();
        for b in bytes {
            stream.push(std::slice::from_ref(b));
            events.extend(stream.pump(&mut ing, 64));
        }
        stream.finish_input();
        while !stream.is_done() {
            events.extend(stream.pump(&mut ing, 64));
        }
        match stream.error() {
            Some(e) => Err(e.to_string()),
            None => Ok(events),
        }
    }

    const ONE_INSTANT: &[u8] = br#"[{"name":"a","ph":"i","ts":1,"pid":1,"tid":1}]"#;

    #[test]
    fn a_first_push_of_a_byte_or_two_does_not_lose_them() {
        // The container is told from the first four bytes; a first push
        // shorter than that used to be set aside and never looked at again.
        for (label, bytes) in [
            ("json", ONE_INSTANT.to_vec()),
            ("gzip", gzip(ONE_INSTANT)),
            ("zip", zip_of(ONE_INSTANT, true)),
        ] {
            assert_eq!(load_byte_by_byte(&bytes).map(|e| e.len()), Ok(1), "{label}");
            for first in 1..4 {
                let mut stream = ChromeStream::default();
                let mut ing = ChromeIngestor::default();
                stream.push(&bytes[..first]);
                stream.push(&bytes[first..]);
                stream.finish_input();
                let n = stream.pump(&mut ing, 1024).len();
                assert_eq!((n, stream.error()), (1, None), "{label}, first push of {first}");
            }
        }
        // A file shorter than four bytes is still read at the end of input.
        assert_eq!(load_byte_by_byte(b"[]").map(|e| e.len()), Ok(0));
        assert!(load_byte_by_byte(&[0x1f, 0x8b]).is_err());
    }

    #[test]
    fn a_byte_order_mark_before_json_is_skipped() {
        let mut text = vec![0xEF, 0xBB, 0xBF];
        text.extend_from_slice(ONE_INSTANT);
        assert_eq!(crate::ingest_collect(&text).map(|(_, e)| e.len()), Ok(1));
        assert_eq!(load_byte_by_byte(&text).map(|e| e.len()), Ok(1));
    }

    /// A Perfetto trace whose first packet is `len` bytes long: a clock
    /// snapshot padded with an unknown field, then one instant.
    fn perfetto_with_first_packet_of(len: usize) -> Vec<u8> {
        use crate::proto::Encoder;
        let mut clock = Encoder::new();
        clock.varint(1, 6).varint(2, 1_000);
        let mut snap = Encoder::new();
        snap.msg(1, &clock);
        let mut first = Encoder::new();
        first.msg(6, &snap);
        let pad = len - first.buf.len() - 2;
        first.bytes(900 % 16, &vec![0u8; pad]);
        assert_eq!(first.buf.len(), len);
        let mut th = Encoder::new();
        th.i32(1, 7).i64(2, 70).string(5, "Main");
        let mut td = Encoder::new();
        td.varint(1, 11).msg(4, &th);
        let mut second = Encoder::new();
        second.msg(60, &td);
        let mut e = Encoder::new();
        e.varint(9, 3).varint(11, 11).string(23, "tick");
        let mut third = Encoder::new();
        third.varint(8, 2_000).varint(10, 1).msg(11, &e);
        let mut trace = Encoder::new();
        trace.msg(1, &first).msg(1, &second).msg(1, &third);
        trace.buf
    }

    #[test]
    fn a_perfetto_trace_is_not_taken_for_json_by_the_length_of_its_first_packet() {
        // The packet tag is a newline, which JSON skips; the length byte
        // after it is `[` for a 91-byte packet and `{` for a 123-byte one.
        for len in [91usize, 123, 90, 124] {
            let trace = perfetto_with_first_packet_of(len);
            assert_eq!(trace[0], b'\n');
            let mut stream = ChromeStream::default();
            let mut ing = ChromeIngestor::default();
            stream.push(&trace);
            stream.finish_input();
            let events = stream.pump(&mut ing, 1024);
            assert_eq!((stream.format(), stream.error()), (Format::Perfetto, None), "first packet of {len}");
            assert_eq!(events.len(), 1, "first packet of {len}");
            assert_eq!(load_byte_by_byte(&trace).map(|e| e.len()), Ok(1), "first packet of {len}, a byte at a time");
        }
    }

    #[test]
    fn json_after_a_blank_line_is_still_json() {
        let long = format!(
            r#"{{"traceEvents":[{{"name":"{}","ph":"i","ts":1,"pid":1,"tid":1}}]}}"#,
            "n".repeat(300)
        );
        for body in [std::str::from_utf8(ONE_INSTANT).unwrap(), r#"{"traceEvents":[]}"#, long.as_str()] {
            for lead in ["\n", "\n\n", "\n  ", "\r\n", " \n"] {
                let text = format!("{lead}{body}");
                let mut stream = ChromeStream::default();
                let mut ing = ChromeIngestor::default();
                stream.push(text.as_bytes());
                stream.finish_input();
                let _ = stream.pump(&mut ing, 1024);
                assert_eq!((stream.format(), stream.error()), (Format::Json, None), "{text:?}");
                assert!(load_byte_by_byte(text.as_bytes()).is_ok(), "{text:?}, a byte at a time");
            }
        }
    }

    /// `cargo test --release -p perfr-live-chrome one_large_value_bench -- --ignored --nocapture`
    ///
    /// One value far larger than a chunk, the shape of a `stackFrames`
    /// table or a metadata blob, arriving 64 KiB at a time with a pump
    /// after each push.
    #[test]
    #[ignore]
    fn one_large_value_bench() {
        let mut text = String::from(r#"{"traceEvents":[{"name":"a","ph":"i","ts":1,"pid":1,"tid":1}],"blob":""#);
        text.push_str(&"x".repeat(16 << 20));
        text.push_str(r#"","stackFrames":{"#);
        for i in 0..150_000 {
            use std::fmt::Write as _;
            let comma = if i > 0 { "," } else { "" };
            write!(text, r#"{comma}"{i}":{{"name":"frame number {i}","parent":"{}"}}"#, i / 2).unwrap();
        }
        text.push_str("}}");
        let t = std::time::Instant::now();
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut peak = 0;
        for chunk in text.as_bytes().chunks(64 << 10) {
            stream.push(chunk);
            let _ = stream.pump(&mut ing, 48_000);
            peak = peak.max(stream.raw.len());
        }
        stream.finish_input();
        while !stream.is_done() {
            let _ = stream.pump(&mut ing, 48_000);
        }
        assert_eq!((stream.error(), ing.stack_frames.len()), (None, 150_000));
        println!(
            "one_large_value_bench: {} MiB in 64 KiB pushes: {:?}, peak buffer {} KiB",
            text.len() >> 20,
            t.elapsed(),
            peak >> 10
        );
    }

    /// A pump's batch is empty whenever a budget of steps emitted nothing:
    /// metadata, `B`s waiting for their `E`, packets nothing maps. The
    /// helpers took an empty batch for the end of the trace.
    #[test]
    fn a_long_run_of_events_that_emit_nothing_is_not_the_end_of_the_trace() {
        let mut text = String::from("[");
        for _ in 0..300_000 {
            text.push_str(r#"{"ph":"M"},"#);
        }
        text.push_str(r#"{"name":"after","ph":"i","ts":1,"pid":1,"tid":1}]"#);
        let (ing, events) = crate::ingest_collect(text.as_bytes()).expect("ingest");
        assert_eq!((ing.stats.events_in, events.len()), (300_001, 1));
        let ing = crate::ingest_bytes(text.as_bytes()).expect("ingest");
        assert_eq!(ing.stats.events_in, 300_001);
        // Pumped until it wants input after each push, a stream holds no
        // backlog when the next push comes.
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        for chunk in text.as_bytes().chunks(1 << 20) {
            stream.push(chunk);
            assert!(!stream.needs_input());
            while !stream.needs_input() {
                let _ = stream.pump(&mut ing, 1024);
            }
            assert!(stream.pending_bytes() < 64, "{} bytes not parsed", stream.pending_bytes());
        }
    }

    #[test]
    fn an_element_that_is_not_an_event_is_passed_over_and_counted() {
        let text = r#"[{"name":"a","ph":"i","ts":1,"pid":1,"tid":1},{"ts":"soon"},17,{"name":"b","ph":"i","ts":2,"pid":1,"tid":1}]"#;
        let (ing, events) = load_like_the_viewer(text.as_bytes()).expect("finishes");
        assert_eq!(events.len(), 2);
        assert_eq!(ing.stats.malformed, 2);
    }

    #[test]
    fn bytes_after_the_closing_bracket_do_not_keep_a_load_open() {
        for tail in ["\n\n", " garbage", "]", "}{", "\0\0\0"] {
            let text = format!(r#"[{{"name":"a","ph":"i","ts":1,"pid":1,"tid":1}}]{tail}"#);
            let (_, events) = load_like_the_viewer(text.as_bytes())
                .unwrap_or_else(|| panic!("tail {tail:?}: the load never finishes"));
            assert_eq!(events.len(), 1, "tail {tail:?}");
        }
    }

    #[test]
    fn a_value_the_scanner_cannot_read_ends_the_trace_instead_of_hanging_it() {
        // NaN is not JSON; neither is a single-quoted string.
        for bad in ["NaN", "'x'"] {
            let text = format!(
                r#"{{"traceEvents":[{{"name":"a","ph":"i","ts":1,"pid":1,"tid":1}}],"meta":{bad},"more":1}}"#
            );
            let (_, events) = load_like_the_viewer(text.as_bytes())
                .unwrap_or_else(|| panic!("{bad}: the load never finishes"));
            assert_eq!(events.len(), 1, "{bad}");
        }
    }
}
