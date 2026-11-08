// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! In-memory ring of [`LiveEvent`]s and the `.perfr.stream` bytes the live
//! viewer already decodes.
//!
//! Sync scopes nest on a per-thread stack, the same way the service's scope
//! reader stamps depth. Async spans (`API_TRACK`) take the lowest free depth
//! on that thread so overlapping spans — many resource loads at once — each
//! get a lane. The ring drops the oldest event when it is full.

use std::collections::{HashMap, VecDeque};

use perfr_live_event::{kind, LiveEvent, LIVE_EVENT_SIZE};
use perfr_live_protocol::{encode_frame, LiveFrame, MAGIC, VERSION};

use crate::PAGE_PID;

/// Deepest lane the timeline will draw. Matches the service reader.
const MAX_DEPTH: u8 = 64;

/// Default ring length: 32 bytes an event, about 6 MB of events.
pub const DEFAULT_CAPACITY: usize = 200_000;

/// How many events go in one batch frame. The rest follow in further batches.
const BATCH_EVENTS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpanKind {
    Sync,
    Async,
}

struct OpenScope {
    tid: u32,
    start_ns: u64,
    name_id: u32,
    depth: u8,
    kind: SpanKind,
}

/// Counts the ring reports in a status frame and to callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub live: u64,
    pub capacity: u64,
    pub dropped: u64,
    pub produced: u64,
    pub oldest_start_ns: u64,
    pub newest_end_ns: u64,
    pub capturing: bool,
}

/// One page's capture. Not thread-safe by itself; the global session wraps it.
pub struct Session {
    pid: u32,
    capacity: usize,
    events: VecDeque<LiveEvent>,
    /// Index into `events` of the first event not yet written by [`flush`].
    flushed: usize,
    dropped: u64,
    produced: u64,
    /// `names[id]` for id >= 1. Id 0 is unused.
    names: Vec<String>,
    name_to_id: HashMap<String, u32>,
    pending_names: Vec<u32>,
    threads: HashMap<u32, String>,
    pending_threads: Vec<u32>,
    process_name: String,
    process_dirty: bool,
    sync_open: HashMap<u64, OpenScope>,
    sync_stack: HashMap<u32, Vec<u64>>,
    async_open: HashMap<u64, OpenScope>,
    next_handle: u64,
    hello_pending: bool,
    finish_pending: bool,
    enabled: bool,
    capture_start_ns: u64,
    capture_start_set: bool,
}

impl Session {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut session = Self {
            pid: PAGE_PID,
            capacity,
            events: VecDeque::new(),
            flushed: 0,
            dropped: 0,
            produced: 0,
            names: vec![String::new()],
            name_to_id: HashMap::new(),
            pending_names: Vec::new(),
            threads: HashMap::new(),
            pending_threads: Vec::new(),
            process_name: "page".into(),
            process_dirty: true,
            sync_open: HashMap::new(),
            sync_stack: HashMap::new(),
            async_open: HashMap::new(),
            next_handle: 1,
            hello_pending: true,
            finish_pending: false,
            enabled: true,
            capture_start_ns: 0,
            capture_start_set: false,
        };
        session.name_thread(crate::tid::MAIN, "main");
        session
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Record again after [`stop`]. The ring is kept. The next [`flush`]
    /// starts over with hello, names and every event still in the ring, so
    /// a viewer that saw `CaptureFinished` gets one consistent snapshot
    /// instead of events after an end marker.
    pub fn resume(&mut self) {
        if self.enabled {
            return;
        }
        self.enabled = true;
        self.finish_pending = false;
        self.resync();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn stats(&self) -> Stats {
        Stats {
            live: self.events.len() as u64,
            capacity: self.capacity as u64,
            dropped: self.dropped,
            produced: self.produced,
            oldest_start_ns: self.events.front().map(|e| e.start_ns).unwrap_or(0),
            newest_end_ns: self.events.iter().map(|e| e.end_ns()).max().unwrap_or(0),
            capturing: self.enabled,
        }
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        let name = if name.is_empty() { "scope" } else { name };
        if let Some(&id) = self.name_to_id.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        self.pending_names.push(id);
        id
    }

    pub fn name_process(&mut self, name: &str) {
        let name = if name.is_empty() { "page" } else { name };
        if self.process_name == name && !self.process_dirty {
            return;
        }
        self.process_name = name.to_string();
        self.process_dirty = true;
    }

    pub fn name_thread(&mut self, tid: u32, name: &str) {
        let name = if name.is_empty() {
            format!("thread {tid}")
        } else {
            name.to_string()
        };
        if self.threads.get(&tid).is_some_and(|n| n == &name)
            && !self.pending_threads.contains(&tid)
        {
            return;
        }
        self.threads.insert(tid, name);
        if !self.pending_threads.contains(&tid) {
            self.pending_threads.push(tid);
        }
    }

    /// Next sync scope on `tid`. Returns 0 when recording is off.
    pub fn begin(&mut self, name: &str, tid: u32, now_ns: u64) -> u64 {
        self.begin_kind(name, tid, now_ns, SpanKind::Sync)
    }

    pub fn begin_async(&mut self, name: &str, tid: u32, now_ns: u64) -> u64 {
        self.begin_kind(name, tid, now_ns, SpanKind::Async)
    }

    pub fn end(&mut self, handle: u64, now_ns: u64) {
        self.end_kind(handle, now_ns, SpanKind::Sync);
    }

    pub fn end_async(&mut self, handle: u64, now_ns: u64) {
        self.end_kind(handle, now_ns, SpanKind::Async);
    }

    /// A sync scope that is already finished, at the current nesting depth.
    pub fn span(&mut self, name: &str, tid: u32, start_ns: u64, end_ns: u64) {
        self.push_closed(name, tid, start_ns, end_ns, SpanKind::Sync, None);
    }

    pub fn span_async(&mut self, name: &str, tid: u32, start_ns: u64, end_ns: u64) {
        let depth = self.alloc_async_depth(tid);
        self.push_closed(name, tid, start_ns, end_ns, SpanKind::Async, Some(depth));
    }

    pub fn mark(&mut self, name: &str, tid: u32, now_ns: u64) {
        if !self.enabled {
            return;
        }
        self.note_time(now_ns);
        let name_id = self.intern(name);
        let depth = self.sync_depth(tid);
        self.push_event(LiveEvent {
            start_ns: now_ns,
            duration_ns: 0,
            tid,
            pid: self.pid,
            kind: kind::API_SCOPE,
            depth,
            extra: 0,
            _pad: 0,
            name_id,
        });
    }

    pub fn value(&mut self, name: &str, tid: u32, value: f64, now_ns: u64) {
        if !self.enabled {
            return;
        }
        self.note_time(now_ns);
        let name_id = self.intern(name);
        let ev = LiveEvent::from_value(now_ns, self.pid, tid, name_id, value as f32);
        self.push_event(ev);
    }

    /// Clip open scopes at `now_ns` and stop recording. The next [`flush`]
    /// ends with `CaptureFinished`.
    pub fn stop(&mut self, now_ns: u64) {
        if self.enabled {
            self.close_open(now_ns);
        }
        self.enabled = false;
        self.finish_pending = true;
    }

    /// Drop events and open scopes. Name ids stay, so a viewer that kept the
    /// intern table does not attach an old string to a reused id.
    pub fn clear(&mut self) {
        self.events.clear();
        self.flushed = 0;
        self.sync_open.clear();
        self.sync_stack.clear();
        self.async_open.clear();
        self.hello_pending = true;
        self.finish_pending = false;
        self.enabled = true;
        self.capture_start_set = false;
        self.process_dirty = true;
        self.pending_threads = self.threads.keys().copied().collect();
        self.pending_names.clear();
    }

    /// Ask the next flush to resend hello, names and every event still in
    /// the ring. Used when the embedded viewer reconnects its socket.
    pub fn resync(&mut self) {
        self.hello_pending = true;
        self.process_dirty = true;
        self.pending_threads = self.threads.keys().copied().collect();
        self.pending_names = (1..self.names.len() as u32).collect();
        self.flushed = 0;
    }

    /// Bytes to append to the live socket. Empty when nothing new happened.
    pub fn flush(&mut self) -> Vec<u8> {
        let pending_events = self.events.len().saturating_sub(self.flushed);
        if !self.hello_pending
            && !self.process_dirty
            && self.pending_threads.is_empty()
            && self.pending_names.is_empty()
            && pending_events == 0
            && !self.finish_pending
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.hello_pending {
            self.write_hello(&mut out);
            self.hello_pending = false;
        }
        self.write_names(&mut out);
        if pending_events > 0 {
            let events: Vec<LiveEvent> = self.events.iter().skip(self.flushed).copied().collect();
            self.write_events(&mut out, &events);
            self.flushed = self.events.len();
        }
        if self.finish_pending {
            out.extend(encode_frame(&LiveFrame::CaptureFinished));
            self.finish_pending = false;
        }
        self.write_status(&mut out);
        out
    }

    /// A complete `.perfr.stream` of whatever the ring still holds, plus any
    /// scope that is open right now clipped at `now_ns`. Suitable for Share
    /// and for `?capture=`.
    pub fn snapshot(&self, now_ns: u64) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_hello(&mut out);
        self.write_all_names(&mut out);
        let mut events: Vec<LiveEvent> = self.events.iter().copied().collect();
        for open in self.sync_open.values().chain(self.async_open.values()) {
            events.push(close_event(open, now_ns, self.pid));
        }
        events.sort_by_key(|e| (e.start_ns, e.tid, e.depth));
        self.write_events(&mut out, &events);
        self.write_status(&mut out);
        out.extend(encode_frame(&LiveFrame::CaptureFinished));
        out
    }

    fn begin_kind(&mut self, name: &str, tid: u32, now_ns: u64, kind_of: SpanKind) -> u64 {
        if !self.enabled {
            return 0;
        }
        self.note_time(now_ns);
        let name_id = self.intern(name);
        let depth = match kind_of {
            SpanKind::Sync => {
                let stack = self.sync_stack.entry(tid).or_default();
                (stack.len() as u8).min(MAX_DEPTH - 1)
            }
            SpanKind::Async => self.alloc_async_depth(tid),
        };
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        let open = OpenScope {
            tid,
            start_ns: now_ns,
            name_id,
            depth,
            kind: kind_of,
        };
        match kind_of {
            SpanKind::Sync => {
                self.sync_stack.entry(tid).or_default().push(handle);
                self.sync_open.insert(handle, open);
            }
            SpanKind::Async => {
                self.async_open.insert(handle, open);
            }
        }
        handle
    }

    fn end_kind(&mut self, handle: u64, now_ns: u64, kind_of: SpanKind) {
        if handle == 0 || !self.enabled {
            return;
        }
        self.note_time(now_ns);
        let open = match kind_of {
            SpanKind::Sync => self.sync_open.remove(&handle),
            SpanKind::Async => self.async_open.remove(&handle),
        };
        let Some(open) = open else {
            return;
        };
        if kind_of == SpanKind::Sync {
            if let Some(stack) = self.sync_stack.get_mut(&open.tid) {
                stack.retain(|&h| h != handle);
            }
        }
        self.push_event(close_event(&open, now_ns, self.pid));
    }

    fn push_closed(
        &mut self,
        name: &str,
        tid: u32,
        start_ns: u64,
        end_ns: u64,
        kind_of: SpanKind,
        depth: Option<u8>,
    ) {
        if !self.enabled {
            return;
        }
        let (start_ns, end_ns) = if end_ns < start_ns {
            (end_ns, start_ns)
        } else {
            (start_ns, end_ns)
        };
        self.note_time(start_ns);
        let name_id = self.intern(name);
        let depth = depth.unwrap_or_else(|| match kind_of {
            SpanKind::Sync => self.sync_depth(tid),
            SpanKind::Async => self.alloc_async_depth(tid),
        });
        self.push_event(LiveEvent {
            start_ns,
            duration_ns: end_ns.saturating_sub(start_ns),
            tid,
            pid: self.pid,
            kind: match kind_of {
                SpanKind::Sync => kind::API_SCOPE,
                SpanKind::Async => kind::API_TRACK,
            },
            depth,
            extra: 0,
            _pad: 0,
            name_id,
        });
    }

    fn sync_depth(&self, tid: u32) -> u8 {
        self.sync_stack
            .get(&tid)
            .map(|s| (s.len() as u8).min(MAX_DEPTH - 1))
            .unwrap_or(0)
    }

    fn alloc_async_depth(&self, tid: u32) -> u8 {
        let mut used = [false; MAX_DEPTH as usize];
        for open in self.async_open.values().filter(|o| o.tid == tid) {
            used[open.depth as usize] = true;
        }
        used.iter()
            .position(|&u| !u)
            .map(|i| i as u8)
            .unwrap_or(MAX_DEPTH - 1)
    }

    fn close_open(&mut self, now_ns: u64) {
        let sync: Vec<OpenScope> = self.sync_open.drain().map(|(_, o)| o).collect();
        let async_open: Vec<OpenScope> = self.async_open.drain().map(|(_, o)| o).collect();
        self.sync_stack.clear();
        for open in sync.into_iter().chain(async_open) {
            self.push_event(close_event(&open, now_ns, self.pid));
        }
    }

    fn note_time(&mut self, now_ns: u64) {
        if !self.capture_start_set {
            self.capture_start_ns = now_ns;
            self.capture_start_set = true;
        }
    }

    fn push_event(&mut self, ev: LiveEvent) {
        if self.events.len() >= self.capacity {
            self.events.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            if self.flushed > 0 {
                self.flushed -= 1;
            }
        }
        self.events.push_back(ev);
        self.produced = self.produced.saturating_add(1);
    }

    fn write_hello(&self, out: &mut Vec<u8>) {
        out.extend(encode_frame(&LiveFrame::Hello {
            version: VERSION,
            event_size: LIVE_EVENT_SIZE as u16,
        }));
        let start = if self.capture_start_set {
            self.capture_start_ns
        } else {
            0
        };
        out.extend(encode_frame(&LiveFrame::CaptureStarted {
            pid: self.pid,
            start_ns: start,
        }));
    }

    fn write_names(&mut self, out: &mut Vec<u8>) {
        if self.process_dirty {
            out.extend(encode_frame(&LiveFrame::ProcessName {
                pid: self.pid,
                name: self.process_name.clone(),
            }));
            self.process_dirty = false;
        }
        let threads = std::mem::take(&mut self.pending_threads);
        for tid in threads {
            if let Some(name) = self.threads.get(&tid) {
                out.extend(encode_frame(&LiveFrame::ThreadName {
                    pid: self.pid,
                    tid,
                    name: name.clone(),
                }));
            }
        }
        let names = std::mem::take(&mut self.pending_names);
        for id in names {
            if let Some(text) = self.names.get(id as usize) {
                if !text.is_empty() {
                    out.extend(encode_frame(&LiveFrame::InternedString {
                        id,
                        text: text.clone(),
                    }));
                }
            }
        }
    }

    fn write_all_names(&self, out: &mut Vec<u8>) {
        out.extend(encode_frame(&LiveFrame::ProcessName {
            pid: self.pid,
            name: self.process_name.clone(),
        }));
        let mut tids: Vec<u32> = self.threads.keys().copied().collect();
        tids.sort_unstable();
        for tid in tids {
            if let Some(name) = self.threads.get(&tid) {
                out.extend(encode_frame(&LiveFrame::ThreadName {
                    pid: self.pid,
                    tid,
                    name: name.clone(),
                }));
            }
        }
        for (id, text) in self.names.iter().enumerate().skip(1) {
            out.extend(encode_frame(&LiveFrame::InternedString {
                id: id as u32,
                text: text.clone(),
            }));
        }
    }

    fn write_events(&self, out: &mut Vec<u8>, events: &[LiveEvent]) {
        for chunk in events.chunks(BATCH_EVENTS) {
            out.extend(encode_frame(&LiveFrame::EventBatch {
                events: chunk.to_vec(),
            }));
        }
    }

    fn write_status(&self, out: &mut Vec<u8>) {
        let stats = self.stats();
        out.extend(encode_frame(&LiveFrame::Status {
            capturing: stats.capturing,
            demo: false,
            events_live: stats.live,
            events_capacity: stats.capacity,
            dropped: stats.dropped,
            spilled: 0,
            produced: stats.produced,
            oldest_start_ns: stats.oldest_start_ns,
            newest_end_ns: stats.newest_end_ns,
            ring_bytes: stats.live.saturating_mul(LIVE_EVENT_SIZE as u64),
        }));
    }
}

fn close_event(open: &OpenScope, now_ns: u64, pid: u32) -> LiveEvent {
    let end = now_ns.max(open.start_ns);
    LiveEvent {
        start_ns: open.start_ns,
        duration_ns: end.saturating_sub(open.start_ns),
        tid: open.tid,
        pid,
        kind: match open.kind {
            SpanKind::Sync => kind::API_SCOPE,
            SpanKind::Async => kind::API_TRACK,
        },
        depth: open.depth,
        extra: 0,
        _pad: 0,
        name_id: open.name_id,
    }
}

/// Decode every frame. A short trailing fragment is an error; a well-formed
/// stream consumes its buffer.
pub fn decode_all(mut bytes: &[u8]) -> Result<Vec<LiveFrame>, String> {
    let mut frames = Vec::new();
    while !bytes.is_empty() {
        let (frame, n) = perfr_live_protocol::decode_frame(bytes).map_err(|e| format!("{e}"))?;
        frames.push(frame);
        bytes = &bytes[n..];
    }
    Ok(frames)
}

/// The hello magic, so a caller can recognise a stream without the protocol crate.
pub fn stream_magic() -> &'static [u8; 4] {
    MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::kind;
    use std::collections::HashMap;

    fn events_of(frames: &[LiveFrame]) -> Vec<LiveEvent> {
        frames
            .iter()
            .filter_map(|f| match f {
                LiveFrame::EventBatch { events } => Some(events.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn nested_scopes_roundtrip_with_depth() {
        let mut s = Session::new(16);
        let outer = s.begin("frame", 1, 1_000);
        let inner = s.begin("update", 1, 1_200);
        s.end(inner, 1_500);
        s.end(outer, 2_000);
        let frames = decode_all(&s.snapshot(2_000)).unwrap();
        assert!(matches!(
            frames[0],
            LiveFrame::Hello {
                version: VERSION,
                ..
            }
        ));
        let events = events_of(&frames);
        assert_eq!(events.len(), 2);
        let update = events
            .iter()
            .find(|e| e.name_id != events[0].name_id)
            .unwrap();
        let frame = events.iter().find(|e| e.duration_ns == 1_000).unwrap();
        assert_eq!(frame.kind, kind::API_SCOPE);
        assert_eq!(frame.depth, 0);
        assert_eq!(frame.duration_ns, 1_000);
        assert_eq!(update.depth, 1);
        assert_eq!(update.duration_ns, 300);
        assert_eq!(update.kind, kind::API_SCOPE);
        let names: Vec<&str> = frames
            .iter()
            .filter_map(|f| match f {
                LiveFrame::InternedString { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"frame"));
        assert!(names.contains(&"update"));
        assert!(frames.iter().any(|f| matches!(
            f,
            LiveFrame::ThreadName { tid: 1, name, .. } if name == "main"
        )));
    }

    #[test]
    fn async_spans_and_values_use_viewer_kinds() {
        let mut s = Session::new(32);
        let a = s.begin_async("load", 1, 10);
        let b = s.begin_async("decode", 1, 12);
        s.end_async(a, 40);
        s.end_async(b, 50);
        s.value("fps", 2, 59.5, 60);
        s.mark("hit", 1, 70);
        let events = events_of(&decode_all(&s.snapshot(80)).unwrap());
        let tracks: Vec<_> = events
            .iter()
            .filter(|e| e.kind == kind::API_TRACK)
            .collect();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].depth, 0);
        assert_eq!(
            tracks[1].depth, 1,
            "overlapping async spans take the next lane"
        );
        let value = events.iter().find(|e| e.kind == kind::VALUE).unwrap();
        assert!((value.value_f32().unwrap() - 59.5).abs() < 0.01);
        assert_eq!(value.tid, 2);
        let mark = events
            .iter()
            .find(|e| e.duration_ns == 0 && e.kind == kind::API_SCOPE)
            .unwrap();
        assert_eq!(mark.start_ns, 70);
    }

    #[test]
    fn ring_drops_oldest_and_flush_does_not_repeat() {
        let mut s = Session::new(2);
        s.span("a", 1, 1, 2);
        s.span("b", 1, 3, 4);
        let first = s.flush();
        assert!(!first.is_empty());
        assert!(
            s.flush().is_empty(),
            "a second flush is empty until something new is recorded"
        );
        s.span("c", 1, 5, 6);
        let stats = s.stats();
        assert_eq!(stats.live, 2);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.produced, 3);
        let frames = decode_all(&s.snapshot(6)).unwrap();
        let mut id_of: HashMap<&str, u32> = HashMap::new();
        for frame in &frames {
            if let LiveFrame::InternedString { id, text } = frame {
                id_of.insert(text.as_str(), *id);
            }
        }
        let events = events_of(&frames);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.name_id != id_of["a"]));
        assert!(events.iter().any(|e| e.name_id == id_of["b"]));
        assert!(events.iter().any(|e| e.name_id == id_of["c"]));
        let more = decode_all(&s.flush()).unwrap();
        let flushed_events = events_of(&more);
        assert_eq!(flushed_events.len(), 1);
        assert_eq!(flushed_events[0].start_ns, 5);
    }

    #[test]
    fn snapshot_magic_and_thread_name() {
        let mut s = Session::new(8);
        s.name_process("demo");
        s.name_thread(4, "Frames");
        s.span("frame", 4, 0, 16_000_000);
        let bytes = s.snapshot(16_000_000);
        assert!(bytes.windows(4).any(|w| w == MAGIC));
        let frames = decode_all(&bytes).unwrap();
        assert!(frames.iter().any(|f| matches!(
            f,
            LiveFrame::ProcessName { name, .. } if name == "demo"
        )));
        assert!(frames.iter().any(|f| matches!(
            f,
            LiveFrame::ThreadName { tid: 4, name, .. } if name == "Frames"
        )));
        assert!(frames
            .iter()
            .any(|f| matches!(f, LiveFrame::CaptureFinished)));
    }

    #[test]
    fn disabled_session_records_nothing() {
        let mut s = Session::new(8);
        s.stop(0);
        assert_eq!(s.begin("nope", 1, 1), 0);
        s.span("nope", 1, 1, 2);
        s.value("x", 1, 1.0, 3);
        assert_eq!(s.stats().produced, 0);
    }

    #[test]
    fn resume_after_stop_records_again_and_restarts_the_stream() {
        let mut s = Session::new(8);
        s.span("before", 1, 1, 2);
        assert!(!s.flush().is_empty());
        s.stop(3);
        let stopped = decode_all(&s.flush()).unwrap();
        assert!(stopped
            .iter()
            .any(|f| matches!(f, LiveFrame::CaptureFinished)));
        assert!(!s.stats().capturing);
        s.resume();
        assert!(s.stats().capturing);
        s.span("after", 1, 4, 5);
        let again = decode_all(&s.flush()).unwrap();
        assert!(
            matches!(again[0], LiveFrame::Hello { .. }),
            "a resumed stream starts over so the viewer drops the finished one"
        );
        assert!(!again
            .iter()
            .any(|f| matches!(f, LiveFrame::CaptureFinished)));
        let events = events_of(&again);
        assert_eq!(events.len(), 2, "the ring is kept across stop and resume");
        assert!(s.flush().is_empty());
    }

    #[test]
    fn resync_resends_events_still_in_the_ring() {
        let mut s = Session::new(8);
        s.span("a", 1, 1, 2);
        assert!(!s.flush().is_empty());
        s.resync();
        let again = events_of(&decode_all(&s.flush()).unwrap());
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].start_ns, 1);
    }
}
