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
use crate::ingest::{ChromeEvent, ChromeIngestor, StackFrame};
use crate::json::{parse_json_string, skip_ws, value_end};
use crate::id::FlexId;
use crate::perfetto::PerfettoState;
use crate::proto::varint;

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
/// Skip `args` for memory-dump events so a heap snapshot cannot explode RAM.
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
    DisplayTimeUnit,
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
    decode: Decode,
    /// Compressed input not inflated yet, from `packed_pos` on.
    packed: Vec<u8>,
    packed_pos: usize,
    magic_buf: Vec<u8>,
    format: Format,
    perfetto: Option<Box<PerfettoState>>,
    eof: bool,
    /// The input is JSON text and nothing else; see [`Self::json_text`].
    plain_json: bool,
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
            decode: Decode::Identity,
            packed: Vec::new(),
            packed_pos: 0,
            magic_buf: Vec::new(),
            format: Format::Unknown,
            perfetto: None,
            eof: false,
            plain_json: false,
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
            plain_json: true,
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
        match self.decode {
            Decode::Identity => {}
            Decode::Ended => return,
            _ => {
                self.packed.extend_from_slice(chunk);
                self.inflate(false);
                return;
            }
        }
        if !self.plain_json && self.bytes_in == chunk.len() as u64 {
            self.magic_buf.extend_from_slice(chunk);
            if self.magic_buf.len() < 4 && !looks_like_json(&self.magic_buf) {
                return;
            }
            let held = std::mem::take(&mut self.magic_buf);
            if held.len() >= 2 && held[0] == 0x1f && held[1] == 0x8b {
                self.decode = Decode::Gzip(Box::new(MultiGzDecoder::new(Vec::new())));
                self.packed = held;
                self.inflate(false);
                return;
            }
            if held.starts_with(b"PK\x03\x04") {
                self.decode = Decode::ZipPending;
                self.packed = held;
                self.inflate(false);
                return;
            }
            self.bytes_decoded += held.len() as u64;
            self.raw.extend_from_slice(&held);
            return;
        }
        self.raw.extend_from_slice(chunk);
        self.bytes_decoded += chunk.len() as u64;
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
        self.inflate(false);
    }

    /// Deserialize up to `budget` events into `ing`.
    pub fn pump(&mut self, ing: &mut ChromeIngestor, budget: usize) -> Vec<perfr_live_event::LiveEvent> {
        let mut out = Vec::new();
        if self.error.is_some() || budget == 0 {
            return out;
        }
        for _ in 0..budget {
            match self.step(ing, &mut out) {
                Step::Event => {}
                Step::Done => break,
                Step::NeedBytes => {
                    if self.inflate(true) {
                        continue;
                    }
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
                    ing.set_display_time_unit("ns");
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
                let Some(end) = value_end(&self.raw, self.pos) else {
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
                let Some(end) = value_end(&self.raw, self.pos) else {
                    return Step::NeedBytes;
                };
                let key = parse_json_string(&self.raw[self.pos..end]).unwrap_or_default();
                self.pos = end;
                self.pending = match key.as_str() {
                    "traceEvents" => PendingKey::TraceEvents,
                    "systemTraceEvents" => PendingKey::SystemTraceEvents,
                    "displayTimeUnit" => PendingKey::DisplayTimeUnit,
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
                    let Some(end) = value_end(&self.raw, self.pos) else {
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
            PendingKey::DisplayTimeUnit => {
                let Some(end) = value_end(&self.raw, self.pos) else {
                    return Step::NeedBytes;
                };
                if let Some(s) = parse_json_string(&self.raw[self.pos..end]) {
                    ing.set_display_time_unit(&s);
                }
                self.pos = end;
                self.phase = Phase::AfterValue;
                Step::Event
            }
            PendingKey::StackFrames => {
                let Some(end) = value_end(&self.raw, self.pos) else {
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
        let Some(end) = value_end(&self.raw, self.pos) else {
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
    /// JSON starts with `[` / `{` after optional whitespace; a Perfetto file
    /// starts with the packet tag, which is also `\n`, so JSON is asked
    /// first and only a tag that is not followed by a JSON opener is proto.
    fn sniff(&self) -> Option<Format> {
        let b = &self.raw[self.pos..];
        if b.is_empty() {
            return if self.input_ended() { Some(Format::Json) } else { None };
        }
        let i = skip_ws(b, 0);
        if i < b.len() && (b[i] == b'[' || b[i] == b'{') {
            return Some(Format::Json);
        }
        if b[0] == PACKET_TAG {
            return Some(Format::Perfetto);
        }
        if i >= b.len() && !self.input_ended() && b.len() < 64 {
            return None;
        }
        Some(Format::Json)
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
        tts: None,
        lane: None,
    })
}

const HELPER_CHUNK: usize = 1 << 20;

fn feed(stream: &mut ChromeStream, bytes: &[u8]) {
    for chunk in bytes.chunks(HELPER_CHUNK) {
        stream.push(chunk);
    }
}

/// One-shot helper for tests and native tools.
pub fn ingest_bytes(bytes: &[u8]) -> Result<ChromeIngestor, String> {
    let mut stream = ChromeStream::default();
    feed(&mut stream, bytes);
    stream.finish_input();
    let mut ing = ChromeIngestor::default();
    loop {
        let batch = stream.pump(&mut ing, 64 * 1024);
        if batch.is_empty() {
            if let Some(e) = stream.error() {
                return Err(e.to_string());
            }
            break;
        }
        // Events already recorded on the ingestor via ingest(); pump returns
        // them for the caller to insert. Drain so we do not hold two copies
        // in the one-shot helper — the tests read `ing` after.
        let _ = batch;
    }
    if let Some(e) = stream.error() {
        return Err(e.to_string());
    }
    Ok(ing)
}

/// Ingest and collect emitted [`LiveEvent`]s (including finish()).
/// Input is fed in 1 MiB chunks so a gzip is inflated into the scanner
/// incrementally instead of as one decompressed blob.
pub fn ingest_collect(bytes: &[u8]) -> Result<(ChromeIngestor, Vec<perfr_live_event::LiveEvent>), String> {
    let mut stream = ChromeStream::default();
    let mut ing = ChromeIngestor::default();
    let mut events = Vec::new();
    for chunk in bytes.chunks(HELPER_CHUNK) {
        stream.push(chunk);
        loop {
            let batch = stream.pump(&mut ing, 64 * 1024);
            if batch.is_empty() {
                if let Some(e) = stream.error() {
                    return Err(e.to_string());
                }
                break;
            }
            events.extend(batch);
        }
    }
    stream.finish_input();
    loop {
        let batch = stream.pump(&mut ing, 64 * 1024);
        if batch.is_empty() {
            if let Some(e) = stream.error() {
                return Err(e.to_string());
            }
            break;
        }
        events.extend(batch);
    }
    // The slices a thread holds back until `finish` are not in `events`
    // yet, so the end of the trace is asked of the ingestor as well.
    let seen_end = ing.content_time_bounds().map_or(0, |(_, end)| end);
    let max_end = events.iter().map(|e| e.end_ns()).max().unwrap_or(0).max(seen_end).max(1);
    events.extend(ing.finish(max_end));
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
                assert_eq!(stream.pending_bytes(), 0, "deflate {deflate}, cut at {cut}");
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
