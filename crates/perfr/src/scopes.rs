// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Manual instrumentation, the consumer half: the target's scope segment
//! drained onto the timeline.
//!
//! The producer (`perfr-api`) writes fixed records into per-thread rings in
//! `/dev/shm/perfr-scopes-<pid>`. This opens that segment for the captured
//! process, drains each ring in claim order every loop, rebuilds names that
//! spilled across records, pairs starts with stops, and turns the result into
//! `LiveEvent`s:
//!
//! - a sync scope becomes an `API_SCOPE` on its thread at the depth the
//!   reader computed from the order of starts and stops on that thread;
//! - an async scope becomes an `API_TRACK` span, on the thread that started
//!   it, ended by whichever thread stopped it;
//! - an instant is an `API_SCOPE` of zero duration, which the renderer draws
//!   one pixel wide;
//! - a value is a `VALUE` sample on a lane named by the track;
//! - a link is counted and otherwise dropped, until the viewer can draw an
//!   arrow.
//!
//! Depth is computed here rather than stamped by the producer, so a scope
//! that is never stopped costs nothing but itself: it stays open until the
//! capture ends and is then closed at the end timestamp, and nothing after it
//! on that thread is skewed.

use crate::visible::VisibleProcesses;
use perfr_live_event::{event_flags, kind, LiveEvent};
use perfr_scope_ring::event::{flags, kind as rk};
use perfr_scope_ring::logmsg::{LogAssembler, LogMessage};
use perfr_scope_ring::merge::{drain_from, Cursors, Producer};
use perfr_scope_ring::text::{Completeness, TextAssembler};
use perfr_scope_ring::{DropTally, NameInterner, ScopeEvent, ScopeRingReader};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Name ids for manual scopes start here, clear of the sampler's frame names.
const SCOPE_NAME_ID_BASE: u32 = 2 << 20;

/// How often to look for a segment that has not appeared yet.
const REOPEN_EVERY_NS: u64 = 250_000_000;

/// The deepest nesting the timeline will draw for one thread. Real call
/// stacks are far shallower; a depth climbing past this means STOPs are being
/// lost -- a Frida `onLeave` that never fired, or a ring overflow -- and
/// drawing hundreds of nested lanes only turns that into a slowdown. Both the
/// per-thread counter and the depth stamped on each scope are clamped here, so
/// the counter also recovers once matching STOPs resume.
const MAX_TIMELINE_DEPTH: u8 = 64;

/// The most open (started, not yet stopped) sync scopes one segment may hold.
/// A steady leak of unmatched STARTs -- again, lost STOPs -- is bounded here
/// rather than left to grow the `open` map without limit; normal captures
/// never approach it.
const MAX_OPEN_SCOPES: usize = 1 << 16;

/// A thread and a ring-record kind: what a lost event was.
type DropKey = (u32, u8);

/// Losses the producer's drop table did not name: an older producer that
/// keeps no table, or a lap it counted against another reader's cursor.
const UNATTRIBUTED: DropKey = (0, 0xFF);

/// How much distinct log text one capture keeps. Every distinct line is
/// interned for the life of the capture, here and in every viewer, while the
/// events that point at it are evicted from a bounded ring. A target that
/// logs a counter in every line would otherwise grow that table without
/// limit. Past the budget a line keeps its time, thread and level, and its
/// text reads [`LOG_TEXT_DROPPED`]. Repeats of a line already kept cost
/// nothing and are never affected.
const LOG_TEXT_BUDGET_BYTES: usize = 64 << 20;

const LOG_TEXT_DROPPED: &str = "(log text not kept: this capture's log text budget is spent)";

/// A scope started and not yet stopped.
#[derive(Clone, Copy)]
struct Open {
    start_ns: u64,
    pid: u32,
    tid: u32,
    name_id: u32,
    depth: u8,
    is_async: bool,
    dynamic: bool,
}

/// One instrumented process's segment and the reader state over it.
struct Segment {
    pid: u32,
    reader: ScopeRingReader,
    cursors: Cursors,
    text: TextAssembler,
    logs: LogAssembler,
    /// What the producer's drop table has said so far, and what of it is
    /// still unmatched by a loss of ours.
    drops: DropLedger,
    /// Heads whose names are still arriving in continuation records.
    awaiting_name: HashMap<(u32, u64), ScopeEvent>,
    /// Open scopes by handle, which is what a stop carries.
    open: HashMap<u64, Open>,
    /// Per-thread count of open sync scopes, which is the depth of the next.
    sync_depth: HashMap<u32, u8>,
}

impl Segment {
    /// Takes over reading `reader`'s segment and tells its producer to write.
    ///
    /// Reading starts at the rings' write position: what an earlier session
    /// wrote is not replayed, and a lap it suffered is not booked as this
    /// capture's loss (a cursor at 0 said "11,610,727 records lost" before
    /// anything was recorded). The drop table is the same story: its counts
    /// are totals since the process started and are never reset, so where
    /// they stand now is remembered and only what is added later is ours.
    ///
    /// The cursor is taken before the flag goes on, so the first drain reads
    /// everything written for this capture and nothing older.
    fn adopt(pid: u32, reader: ScopeRingReader) -> Segment {
        let cursors = Cursors::at_write(reader.rings());
        reader.publish_read_cursors(&cursors.read);
        let drops = DropLedger::starting_at(&reader.rings().snapshot_drops());
        // Until this, an instrumented process pays a relaxed load per call
        // and writes nothing. This is also what turns on the service's own
        // scopes, since it reads its own segment like any other.
        reader.set_capturing(true);
        Segment {
            pid,
            reader,
            cursors,
            text: TextAssembler::new(),
            logs: LogAssembler::new(),
            drops,
            awaiting_name: HashMap::new(),
            open: HashMap::new(),
            sync_depth: HashMap::new(),
        }
    }
}

/// Drains the target's segment (and its descendants') into the timeline.
pub struct ScopeSource {
    service: Arc<perfr_live_server::LiveService>,
    names: NameInterner,
    segments: Vec<Segment>,
    /// Pids tried and not found, with when, so a process that never
    /// instruments is not probed every five milliseconds forever.
    last_probe_ns: HashMap<u32, u64>,
    /// Pids whose segment speaks another protocol version, warned about once
    /// so an upgrade-skewed process does not log every reprobe.
    warned_version_mismatch: HashSet<u32>,
    next_discovery_ns: u64,
    pub links_seen: u64,
    pub events_pushed: u64,
    pub events_lost: u64,
    /// Open scopes dropped because their ring lost records underneath them
    /// (see `drain`): the spans that would otherwise have been drawn wrong.
    pub scopes_discarded_on_loss: u64,
    /// [`Self::events_lost`] broken down by thread and record kind, for the
    /// status line. Sums to `events_lost`; what the producer's table could
    /// not name is under [`UNATTRIBUTED`].
    drop_totals: HashMap<DropKey, u64>,
    /// Bytes of distinct log text interned so far, against
    /// [`LOG_TEXT_BUDGET_BYTES`].
    log_text_bytes: usize,
    /// Log lines recorded without their text because that budget was spent.
    pub log_lines_without_text: u64,
    /// Name ids of hooks the Frida agent switched off for firing over its
    /// call-rate limit (a `\x02` marker record), in the order they arrived.
    pub auto_unhooked: Vec<u32>,
}

impl ScopeSource {
    pub fn new(service: Arc<perfr_live_server::LiveService>) -> ScopeSource {
        // Segments of processes that died without running shutdown would
        // otherwise sit in tmpfs until reboot. Cheap, and the only party in
        // a position to do it.
        let swept = perfr_scope_ring::sweep_dead_segments();
        if swept > 0 {
            log::info!("swept {swept} scope segment(s) left by dead processes");
        }
        ScopeSource {
            service,
            names: NameInterner::starting_at(SCOPE_NAME_ID_BASE),
            segments: Vec::new(),
            last_probe_ns: HashMap::new(),
            warned_version_mismatch: HashSet::new(),
            next_discovery_ns: 0,
            links_seen: 0,
            events_pushed: 0,
            events_lost: 0,
            scopes_discarded_on_loss: 0,
            drop_totals: HashMap::new(),
            log_text_bytes: 0,
            log_lines_without_text: 0,
            auto_unhooked: Vec::new(),
        }
    }

    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Scopes started but not yet stopped, across every segment. A count that
    /// only grows is STOPs being lost, the same signal behind [`Self::events_lost`].
    pub fn open_scopes(&self) -> usize {
        self.segments.iter().map(|s| s.open.len()).sum()
    }

    /// The fullest ring across every open segment, 0.0 to 1.0: unread claims
    /// over capacity. A ring at 1.0 is being lapped and losing events.
    pub fn fill_fraction(&self) -> f32 {
        let mut worst = 0.0f32;
        for segment in &self.segments {
            let rings = segment.reader.rings();
            let slots = rings.slots_per_ring() as f32;
            for ring in 0..rings.ring_count() {
                let unread = rings.write_cursor(ring).saturating_sub(segment.cursors.read[ring]);
                worst = worst.max((unread as f32 / slots).min(1.0));
            }
        }
        worst
    }

    /// Opens the service's own scope segment and turns its capturing flag on
    /// up front, before the capture loop's setup runs. The manual API is inert
    /// until a reader sets that flag, and the ordinary discovery below only
    /// does so on the first drain -- which is after the symbolizer is built,
    /// the rings opened and the uprobes armed. Without this those setup phases,
    /// and the whole-capture scope, are emitted while the segment is still
    /// inert and lost. Opened here at the rings' write position, before the
    /// flag goes on, the first drain reads them -- and nothing older. The
    /// segment outlives a capture: at cursor 0 the second capture replayed
    /// every scope the service wrote during the first (a hundred thousand
    /// and more, each refused as older than the capture and counted as
    /// dropped), booked a lap of its own ring as this capture's loss, and
    /// carried the first capture's unclosed scopes as extra depth.
    pub fn begin_self_capture(&mut self) {
        // Touch the manual API so the producer segment exists. The call writes
        // nothing (capturing is not set yet) but materialises the segment.
        let _ = perfr_api::start("");
        let self_pid = std::process::id();
        if self.segments.iter().any(|s| s.pid == self_pid) {
            return;
        }
        if let Ok(reader) = ScopeRingReader::open(self_pid) {
            self.segments.push(Segment::adopt(self_pid, reader));
        }
    }

    /// Opens segments for any visible pid that has one and is not open yet --
    /// and always for the service itself.
    ///
    /// The service instruments its own capture loop with the same API any
    /// program uses, so it has a segment like any other and is discovered the
    /// same way. It is simply always on the list: whether it shows up in the
    /// viewer should not depend on whether it happens to be a descendant of
    /// the target.
    fn discover(&mut self, visible: &mut VisibleProcesses, now_ns: u64) {
        if now_ns < self.next_discovery_ns {
            return;
        }
        self.next_discovery_ns = now_ns.saturating_add(REOPEN_EVERY_NS);
        // Every process with a segment, whoever it is: manual instrumentation
        // is of interest by definition, target or not. Plus the visible set,
        // whose segments may not exist yet (perfr_init after capture start),
        // and the service itself.
        let mut pids = perfr_scope_ring::shm::live_segment_pids();
        pids.extend(visible.pids());
        pids.push(std::process::id());
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            if self.segments.iter().any(|s| s.pid == pid) {
                continue;
            }
            let due = self
                .last_probe_ns
                .get(&pid)
                .is_none_or(|last| now_ns.saturating_sub(*last) >= REOPEN_EVERY_NS);
            if !due {
                continue;
            }
            self.last_probe_ns.insert(pid, now_ns);
            // "Not initialised yet" and "no such segment" both come back as
            // errors and both mean try again later.
            let opened = ScopeRingReader::open(pid);
            if opened.is_err() {
                // Distinguish a real version mismatch -- a process built
                // against another perfr's libperfr_api -- from a segment that
                // is simply absent or half-written, which open() reports the
                // same way. Only the mismatch is worth telling the operator
                // about, and only once: the fix is to match the library to
                // this service (they ship together, so `pip install -U
                // perfr-api` or reinstalling brings them level).
                if let Some(found) = perfr_scope_ring::segment_version(pid) {
                    if found != perfr_scope_ring::VERSION
                        && self.warned_version_mismatch.insert(pid)
                    {
                        let ours = perfr_scope_ring::VERSION;
                        log::warn!(
                            "manual instrumentation: pid {pid} has a perfr                              segment at protocol version {found}, but this service reads                              version {ours}. Its scopes are skipped -- update its libperfr_api                              (or the perfr-api package) to match this service."
                        );
                    }
                }
            }
            if let Ok(reader) = opened {
                self.warned_version_mismatch.remove(&pid);
                self.add_segment(pid, reader);
                // An instrumented process gets rows, and its threads join the
                // state focus on the next refresh.
                visible.add_instrumented(pid);
            }
        }
    }

    /// A scope name id for a hooked function, from the same interner that
    /// names every other scope and announced to the viewer now, so a START
    /// token can carry it. Not the service's general string interner: that
    /// one allocates just above the highest id it has seen, which is this
    /// interner's range, and the next scope name interned here took the
    /// same id and renamed every span of the hook after it.
    pub fn intern_name(&mut self, name: &str) -> u32 {
        let id = self.names.id_for(name.as_bytes());
        for (id, name) in self.names.take_new() {
            self.service.intern_id(id, &name);
        }
        id
    }

    /// Starts reading `pid`'s segment now rather than at the next discovery
    /// tick. A Frida target writes from the moment its hooks arm; until its
    /// segment is read and told to capture, everything it writes -- the first
    /// half second of spans, and the marker a hook sends when it switches
    /// itself off -- is silently not recorded. Called right after arming.
    pub fn adopt(&mut self, pid: u32) {
        if self.segments.iter().any(|s| s.pid == pid) {
            return;
        }
        if let Ok(reader) = ScopeRingReader::open(pid) {
            self.add_segment(pid, reader);
        }
    }

    fn add_segment(&mut self, pid: u32, reader: ScopeRingReader) {
        let ring_count = reader.rings().ring_count();
        log::info!("manual instrumentation: opened segment of pid {pid} ({ring_count} rings)");
        self.segments.push(Segment::adopt(pid, reader));
    }

    /// A drain with no discovery, for the segments already adopted: the
    /// capture loop calls it once before its setup (symbols, unwinder, rings
    /// -- half a second on a big target) so a hooked target's first burst is
    /// read out of the rings before it can lap them.
    pub fn drain_now(&mut self, now_ns: u64, batch: &mut Vec<LiveEvent>) {
        self.drain(now_ns, batch);
    }

    /// One pass: discover, drain, convert. Appends to `batch`.
    pub fn poll(&mut self, visible: &mut VisibleProcesses, now_ns: u64, batch: &mut Vec<LiveEvent>) {
        self.discover(visible, now_ns);
        self.drain(now_ns, batch);
    }

    fn drain(&mut self, now_ns: u64, batch: &mut Vec<LiveEvent>) {
        for index in 0..self.segments.len() {
            let alive = if perfr_scope_ring::platform::process_alive(self.segments[index].pid) {
                Producer::Alive
            } else {
                Producer::Gone
            };
            let pass = {
                let segment = &mut self.segments[index];
                drain_from(segment.reader.rings(), &mut segment.cursors, now_ns, alive)
            };
            // Say how far this reader has got before doing anything with
            // what it read. The producer counts an overwrite as a loss when
            // it lands at or past this cursor, so every moment it stays
            // behind is a moment events already copied out are counted.
            self.segments[index].reader.publish_read_cursors(&self.segments[index].cursors.read);
            self.events_lost = self.events_lost.saturating_add(pass.dropped);
            self.account_drops(index, pass.dropped, now_ns, batch);
            // A lapped ring lost records in the middle of this process's
            // stream, STOPs among them. Every scope still open is now
            // unreliable: left alone, a START whose STOP was lost runs to the
            // end of the capture (a 5-second FEngineLoop::Tick), or gets
            // closed by some later STOP on its thread (a 5-second
            // dequeueInternal). Drop them: a lossy capture shows a gap, not
            // a lie. Their STOPs, if they do arrive, are ignored as
            // stop-without-start.
            if pass.dropped > 0 {
                let pending = self.segments[index].logs.finish();
                for msg in pending {
                    self.emit_log(index, msg, batch);
                }
                let segment = &mut self.segments[index];
                let discarded = segment.open.len() + segment.awaiting_name.len();
                segment.open.clear();
                segment.awaiting_name.clear();
                segment.sync_depth.clear();
                self.scopes_discarded_on_loss = self.scopes_discarded_on_loss.saturating_add(discarded as u64);
            }
            // Sort this pass by timestamp before pairing. The rings arrive in
            // ring-index order, but a START and its STOP can be on different
            // rings -- an async scope is started on one thread and stopped on
            // another. Processed in ring order, a STOP whose ring comes first
            // is dropped as unmatched and its START is orphaned, then closed
            // at capture end with a nonsense duration. A START always has an
            // earlier timestamp than its STOP, so ordering the pass by time
            // guarantees the START is seen first. Text continuations share
            // their head's timestamp and the assembler keys on
            // (tid, scope_id), so interleaving them is harmless.
            let mut events: Vec<ScopeEvent> =
                pass.slices.into_iter().flat_map(|s| s.events).collect();

            // Thread id breaks ties. Chunks of one line share a timestamp, and
            // a stable sort keeps the order that thread wrote them. Two threads
            // that stamped the same nanosecond stay separated by tid.
            events.sort_by(|a, b| a.timestamp_ns.cmp(&b.timestamp_ns).then(a.tid.cmp(&b.tid)));
            for event in events {
                self.accept(index, event, batch);
            }
        }
        self.announce_new_names();
    }

    /// Tells the service, and through it every viewer, what the ids handed
    /// out since the last call mean. Anything that interns a name has to be
    /// followed by this before its events are pushed.
    fn announce_new_names(&mut self) {
        for (id, name) in self.names.take_new() {
            self.service.intern_id(id, &name);
        }
    }

    fn accept(&mut self, index: usize, event: ScopeEvent, batch: &mut Vec<LiveEvent>) {
        let pid = self.segments[index].pid;
        match event.kind {
            rk::SCOPE_STOP => {
                let segment = &mut self.segments[index];
                if let Some(open) = segment.open.remove(&event.scope_id) {
                    if !open.is_async {
                        let d = segment.sync_depth.entry(open.tid).or_insert(0);
                        *d = d.saturating_sub(1);
                    }
                    batch.push(span(open, event.timestamp_ns));
                    self.events_pushed += 1;
                }
                // A stop with no start is a capture that began mid-scope;
                // there is nothing to draw and nothing to complain about.
            }
            rk::LINK => self.links_seen += 1,
            rk::LOG => {
                if let Some(msg) = self.segments[index].logs.accept(&event) {
                    self.emit_log(index, msg, batch);
                }
            }
            rk::TEXT => {
                // Continuation: feed the assembler; on completion, the head
                // it belongs to can finally be named and processed.
                let done = self.segments[index].text.accept(&event);
                if let Some((name, completeness)) = done {
                    let key = (event.tid, event.scope_id);
                    if let Some(head) = self.segments[index].awaiting_name.remove(&key) {
                        let owned = mark_cut(name.as_bytes(), completeness == Completeness::Truncated);
                        self.named(index, head, &owned, batch);
                    }
                }
            }
            _ => {
                // START, INSTANT, VALUE: named by inline text, possibly
                // continued. Feed the assembler first; it answers now for a
                // name that fits, or later via a TEXT record.
                if event.has_more_text() {
                    let key = (event.tid, event.scope_id);
                    self.segments[index].text.accept(&event);
                    self.segments[index].awaiting_name.insert(key, event);
                } else {
                    let cut = event.flags & flags::CUT != 0;
                    let name = mark_cut(event.text_bytes(), cut);
                    self.named(index, event, &name, batch);
                }
            }
        }
        let _ = pid;
    }

    fn named(&mut self, index: usize, event: ScopeEvent, name: &[u8], batch: &mut Vec<LiveEvent>) {
        // A hooked function's START names itself by a service-interned id
        // rather than its text (`\x01` + 8 hex digits, see frida.rs), so a
        // 200-character Unreal name costs one record instead of eight; a
        // `\x02` token is the agent reporting it switched that hook off.
        let name_id = match name_token(name) {
            Some((1, id)) => id,
            Some((2, id)) => {
                // The hook switched itself off: an instant on the thread's
                // track, where and when it happened, named for it
                // ("auto-unhooked: <function>"), and a note for the status.
                if !self.auto_unhooked.contains(&id) {
                    self.auto_unhooked.push(id);
                }
                let pid = self.segments[index].pid;
                batch.push(LiveEvent {
                    start_ns: event.timestamp_ns,
                    duration_ns: 0,
                    tid: event.tid,
                    pid,
                    kind: kind::API_SCOPE,
                    depth: 0,
                    extra: 0,
                    _pad: 0,
                    name_id: id,
                });
                self.events_pushed += 1;
                return;
            }
            _ => self.names.id_for(name),
        };
        // Bound the open set before borrowing the segment: a START whose STOP
        // never arrives stays open forever, so an unbounded run of them would
        // leak. Refuse new starts (sync or async) past the cap, counting them
        // as lost.
        if event.kind == rk::SCOPE_START
            && self.segments[index].open.len() >= MAX_OPEN_SCOPES
        {
            self.events_lost = self.events_lost.saturating_add(1);
            return;
        }
        let segment = &mut self.segments[index];
        let pid = segment.pid;
        match event.kind {
            rk::SCOPE_START => {
                let is_async = event.flags & flags::ASYNC != 0;
                let depth = if is_async {
                    0
                } else {
                    let d = segment.sync_depth.entry(event.tid).or_insert(0);
                    let here = (*d).min(MAX_TIMELINE_DEPTH - 1);
                    *d = d.saturating_add(1).min(MAX_TIMELINE_DEPTH);
                    here
                };
                segment.open.insert(
                    event.scope_id,
                    Open {
                        start_ns: event.timestamp_ns,
                        pid,
                        tid: event.tid,
                        name_id,
                        depth,
                        is_async,
                        dynamic: event.flags & flags::DYNAMIC != 0,
                    },
                );
            }
            rk::INSTANT => {
                batch.push(LiveEvent {
                    start_ns: event.timestamp_ns,
                    duration_ns: 0,
                    tid: event.tid,
                    pid,
                    kind: kind::API_SCOPE,
                    depth: *segment.sync_depth.get(&event.tid).unwrap_or(&0),
                    extra: 0,
                    _pad: 0,
                    name_id,
                });
                self.events_pushed += 1;
            }
            rk::VALUE => {
                if let Some(v) = event.value() {
                    batch.push(LiveEvent::from_value(event.timestamp_ns, pid, event.tid, name_id, v as f32));
                    self.events_pushed += 1;
                }
            }
            _ => {}
        }
    }

    /// [`Self::events_lost`] by thread and record kind, for the status line
    /// and the log. Empty when nothing was lost.
    pub fn loss_summary(&self) -> String {
        let mut keys: Vec<DropKey> = self.drop_totals.keys().copied().collect();
        keys.sort_unstable();
        let parts: Vec<String> = keys
            .into_iter()
            .map(|key| {
                let n = self.drop_totals[&key];
                if key == UNATTRIBUTED {
                    format!("{n} not broken down by thread")
                } else {
                    format!("tid {} {}={n}", key.0, kind_label(key.1))
                }
            })
            .collect();
        parts.join(", ")
    }

    fn emit_log(&mut self, index: usize, msg: LogMessage, batch: &mut Vec<LiveEvent>) {
        let pid = self.segments[index].pid;
        let mut truncated = msg.completeness == Completeness::Truncated;
        let name_id = match self.log_text_id(msg.message.as_bytes()) {
            Some(id) => id,
            None => {
                // The line happened, and when, where and how loudly are
                // still true. Only its text is not kept, and it says so.
                truncated = true;
                if self.log_lines_without_text == 0 {
                    log::warn!(
                        "log text budget spent ({} MiB of distinct lines): later new lines keep their time, thread and level but not their text",
                        LOG_TEXT_BUDGET_BYTES >> 20
                    );
                }
                self.log_lines_without_text += 1;
                self.names.id_for(LOG_TEXT_DROPPED.as_bytes())
            }
        };
        batch.push(LiveEvent {
            start_ns: msg.timestamp_ns,
            duration_ns: 0,
            tid: msg.tid,
            pid,
            kind: kind::LOG,
            depth: 0,
            extra: msg.level,
            _pad: if truncated { event_flags::TRUNCATED } else { 0 },
            name_id,
        });
        self.events_pushed += 1;
    }

    /// The id of a log line's text, or `None` when the line is new and the
    /// capture's budget for distinct log text is spent.
    fn log_text_id(&mut self, text: &[u8]) -> Option<u32> {
        if let Some(id) = self.names.get(text) {
            return Some(id);
        }
        let after = self.log_text_bytes.saturating_add(text.len());
        if after > LOG_TEXT_BUDGET_BYTES {
            return None;
        }
        self.log_text_bytes = after;
        Some(self.names.id_for(text))
    }

    /// Turns the `lost` records this pass's drain could not read into DROP
    /// markers, named by thread and kind as far as the producer's table can.
    ///
    /// `lost` is the count: it is what this reader's own cursors say, and it
    /// is exact. The table only says what the lost records were, and it can
    /// run ahead of `lost` (see [`perfr_scope_ring::meta`]), so its counts are
    /// spent against `lost` and never reported on their own. A pass that lost
    /// nothing writes no marker, whatever the table says.
    fn account_drops(&mut self, index: usize, lost: u64, now_ns: u64, batch: &mut Vec<LiveEvent>) {
        let segment = &mut self.segments[index];
        let table = segment.reader.rings().snapshot_drops();
        let split = segment.drops.split(lost, &table);
        let pid = segment.pid;
        let unnamed = (split.unnamed > 0).then_some((UNATTRIBUTED, split.unnamed));
        for ((tid, kind_id), count) in split.named.into_iter().chain(unnamed) {
            *self.drop_totals.entry((tid, kind_id)).or_insert(0) += count;
            batch.push(LiveEvent {
                start_ns: now_ns,
                duration_ns: count,
                tid,
                pid,
                kind: kind::DROP,
                depth: 0,
                extra: kind_id,
                _pad: 0,
                name_id: 0,
            });
            self.events_pushed += 1;
        }
    }

    /// Closes every scope still open at `end_ns`, so the last frame of a
    /// capture is drawn rather than lost, and tells every producer to stop
    /// writing.
    pub fn finish(&mut self, end_ns: u64, batch: &mut Vec<LiveEvent>) {
        // Disable new writes, then consume the committed prefix. A producer
        // already inside a write may still be in flight; this is not a global
        // quiescence barrier, and unfinished scopes are clipped at end_ns.
        for segment in &self.segments {
            segment.reader.set_capturing(false);
        }
        self.drain(end_ns, batch);
        // Lines whose last chunk never arrived. Their text is new, so it has
        // to be announced like any other name: the drain above announced
        // only what it had interned itself.
        for index in 0..self.segments.len() {
            let pending = self.segments[index].logs.finish();
            for msg in pending {
                self.emit_log(index, msg, batch);
            }
        }
        self.announce_new_names();
        for segment in &mut self.segments {
            for (_, open) in segment.open.drain() {
                batch.push(span(open, end_ns));
                self.events_pushed += 1;
            }
        }
    }
}

impl Drop for ScopeSource {
    fn drop(&mut self) {
        for segment in &self.segments {
            segment.reader.set_capturing(false);
        }
    }
}

fn span(open: Open, end_ns: u64) -> LiveEvent {
    LiveEvent {
        start_ns: open.start_ns,
        duration_ns: end_ns.saturating_sub(open.start_ns),
        tid: open.tid,
        pid: open.pid,
        kind: if open.is_async { kind::API_TRACK } else { kind::API_SCOPE },
        depth: open.depth,
        extra: 0,
        _pad: if open.dynamic { perfr_live_event::event_flags::DYNAMIC } else { 0 },
        name_id: open.name_id,
    }
}

/// One reader's reading of a producer's drop table across passes.
#[derive(Default)]
struct DropLedger {
    /// Where each of the table's counters stood the last time it was read.
    seen: HashMap<DropKey, u32>,
    /// Counts read last pass that no loss of ours has matched yet.
    carry: Vec<(DropKey, u64)>,
}

impl DropLedger {
    /// A ledger for a reader that starts now. The table's counters are
    /// totals since the producer started and are never reset, so everything
    /// in `table` happened before this reader and is none of its losses.
    fn starting_at(table: &[DropTally]) -> DropLedger {
        DropLedger {
            seen: table.iter().map(|tally| ((tally.tid, tally.kind), tally.count)).collect(),
            carry: Vec::new(),
        }
    }

    /// Splits the `lost` records of one pass by what `table` says was
    /// overwritten since the last pass.
    fn split(&mut self, lost: u64, table: &[DropTally]) -> DropSplit {
        let mut fresh = Vec::new();
        for tally in table {
            let key = (tally.tid, tally.kind);
            let seen = self.seen.insert(key, tally.count).unwrap_or(0);
            // The counters are u32 and only go up, so a smaller total wrapped.
            let delta = tally.count.wrapping_sub(seen);
            if delta > 0 {
                fresh.push((key, u64::from(delta)));
            }
        }
        let mut split = attribute_drops(lost, &self.carry, &fresh);
        self.carry = std::mem::take(&mut split.carry);
        split
    }
}

/// One pass's loss, split by what was lost.
#[derive(Debug, Default, PartialEq, Eq)]
struct DropSplit {
    /// Losses the producer's table names. Never more than `lost` in total.
    named: Vec<(DropKey, u64)>,
    /// The rest of `lost`.
    unnamed: u64,
    /// Table counts no loss has matched yet, kept for the next pass only.
    carry: Vec<(DropKey, u64)>,
}

/// Splits `lost` across the producer's drop-table counts.
///
/// `fresh` is what the table gained since the last pass and `carried` what
/// the last pass left over. Together they are what the producer says it
/// overwrote; `lost` is what this reader actually failed to read. When the
/// table says less, the difference is unnamed. When it says more, each key
/// gets its proportional share of `lost` and the surplus of `fresh` is
/// carried: a producer counts an overwrite the moment it happens, which can
/// be just after this pass's drain, and the lap then shows up in the next
/// one. A count still unmatched after that pass was never this reader's loss
/// (the slot had been read, or the count was against another reader's
/// cursor), so `carried` is not carried twice.
fn attribute_drops(lost: u64, carried: &[(DropKey, u64)], fresh: &[(DropKey, u64)]) -> DropSplit {
    use std::collections::BTreeMap;
    let mut pool: BTreeMap<DropKey, (u64, u64)> = BTreeMap::new();
    for (key, count) in carried {
        pool.entry(*key).or_default().0 += count;
    }
    for (key, count) in fresh {
        pool.entry(*key).or_default().1 += count;
    }
    let claimed: u64 = pool.values().map(|(old, new)| old + new).sum();
    if claimed <= lost {
        return DropSplit {
            named: pool.into_iter().map(|(key, (old, new))| (key, old + new)).filter(|(_, n)| *n > 0).collect(),
            unnamed: lost - claimed,
            carry: Vec::new(),
        };
    }
    // More claimed than lost. Give every key the floor of its share, then
    // hand the remainder out one at a time to the largest claims, so the
    // shares add up to exactly `lost`.
    let mut shares: Vec<(DropKey, u64, u64, u64)> = pool
        .into_iter()
        .map(|(key, (old, new))| {
            let share = ((old + new) as u128 * lost as u128 / claimed as u128) as u64;
            (key, old, new, share)
        })
        .collect();
    let mut rest = lost - shares.iter().map(|s| s.3).sum::<u64>();
    let mut by_size: Vec<usize> = (0..shares.len()).collect();
    by_size.sort_by_key(|&i| std::cmp::Reverse(shares[i].1 + shares[i].2));
    for i in by_size {
        if rest == 0 {
            break;
        }
        if shares[i].3 < shares[i].1 + shares[i].2 {
            shares[i].3 += 1;
            rest -= 1;
        }
    }
    let mut out = DropSplit::default();
    for (key, old, new, share) in shares {
        if share > 0 {
            out.named.push((key, share));
        }
        // The oldest claims are spent first; only this pass's may wait.
        let left = (old + new - share).min(new);
        if left > 0 {
            out.carry.push((key, left));
        }
    }
    out
}

fn mark_cut(name: &[u8], cut: bool) -> Vec<u8> {
    let mut out = name.to_vec();
    if cut {
        out.extend_from_slice("…".as_bytes());
    }
    out
}

fn kind_label(kind_id: u8) -> &'static str {
    match kind_id {
        rk::SCOPE_START => "scope_start",
        rk::SCOPE_STOP => "scope_stop",
        rk::INSTANT => "instant",
        rk::VALUE => "value",
        rk::TEXT => "text",
        rk::LINK => "link",
        rk::LOG => "log",
        _ => "unknown",
    }
}

/// `(kind, id)` of a name token: a control byte (1 = named by id, 2 = hook
/// switched off) followed by the id as 8 hex digits. Anything else is text.
fn name_token(name: &[u8]) -> Option<(u8, u32)> {
    if name.len() != 9 || !(name[0] == 1 || name[0] == 2) {
        return None;
    }
    let hex = std::str::from_utf8(&name[1..]).ok()?;
    Some((name[0], u32::from_str_radix(hex, 16).ok()?))
}

/// The text a Frida hook sends per call in place of its name.
pub fn name_token_text(kind: u8, id: u32) -> String {
    format!("{}{id:08x}", kind as char)
}

#[cfg(test)]
mod token_tests {
    use super::{name_token, name_token_text};

    #[test]
    fn tokens_round_trip_and_text_is_left_alone() {
        assert_eq!(name_token(name_token_text(1, 0x1234_abcd).as_bytes()), Some((1, 0x1234_abcd)));
        assert_eq!(name_token(name_token_text(2, 7).as_bytes()), Some((2, 7)));
        assert_eq!(name_token(b"FEngineLoop::Tick"), None);
        assert_eq!(name_token(b"\x0100000zzz"), None);
        assert_eq!(name_token(b"\x03deadbeef"), None);
    }
}

#[cfg(test)]
mod drop_tests {
    use super::{attribute_drops, DropKey, DropLedger, DropSplit, UNATTRIBUTED};
    use perfr_scope_ring::DropTally;

    const LOGS_T4: DropKey = (4, 6);
    const STOPS_T9: DropKey = (9, 1);

    fn tally(key: DropKey, count: u32) -> DropTally {
        DropTally { tid: key.0, kind: key.1, count }
    }

    fn total(split: &DropSplit) -> u64 {
        split.named.iter().map(|(_, n)| n).sum::<u64>() + split.unnamed
    }

    #[test]
    fn a_reader_does_not_inherit_the_drops_of_the_capture_before_it() {
        // The first capture lapped a ring: 100,000 logs lost, counted on the
        // control page, which outlives the capture. The second capture loses
        // nothing and must say nothing.
        let history = [tally(LOGS_T4, 100_000)];
        let mut ledger = DropLedger::starting_at(&history);
        let split = ledger.split(0, &history);
        assert_eq!(split, DropSplit::default());
        // And when it does lose something, only that is reported.
        let split = ledger.split(3, &[tally(LOGS_T4, 100_003)]);
        assert_eq!(split.named, vec![(LOGS_T4, 3)]);
        assert_eq!(split.unnamed, 0);
    }

    #[test]
    fn the_table_never_reports_more_than_the_reader_lost() {
        // The producer counted 10 overwrites, but the reader had already
        // copied 8 of those slots out before its cursor said so. Two were
        // lost; two are reported.
        let mut ledger = DropLedger::starting_at(&[]);
        let split = ledger.split(2, &[tally(LOGS_T4, 10)]);
        assert_eq!(split.named, vec![(LOGS_T4, 2)]);
        assert_eq!(total(&split), 2);
        // A pass that lost nothing writes no marker, whatever is left over.
        let split = ledger.split(0, &[tally(LOGS_T4, 10)]);
        assert_eq!(total(&split), 0);
    }

    #[test]
    fn a_loss_the_table_does_not_name_is_still_counted() {
        // An older producer keeps no table at all.
        let mut ledger = DropLedger::starting_at(&[]);
        let split = ledger.split(500, &[]);
        assert!(split.named.is_empty());
        assert_eq!(split.unnamed, 500);
        // A table that names part of a loss leaves the rest unnamed.
        let split = ledger.split(10, &[tally(STOPS_T9, 4)]);
        assert_eq!(split.named, vec![(STOPS_T9, 4)]);
        assert_eq!(split.unnamed, 6);
        assert_eq!(UNATTRIBUTED, (0, 0xFF));
    }

    #[test]
    fn a_count_that_arrives_a_pass_early_is_matched_once_and_not_twice() {
        // The producer overwrites 5 slots just after pass 1's drain: the
        // table has them at pass 1, the reader sees the lap at pass 2.
        let mut ledger = DropLedger::starting_at(&[]);
        let first = ledger.split(0, &[tally(LOGS_T4, 5)]);
        assert_eq!(total(&first), 0);
        let second = ledger.split(5, &[tally(LOGS_T4, 5)]);
        assert_eq!(second.named, vec![(LOGS_T4, 5)], "named, not lumped in as unattributed");
        assert_eq!(second.unnamed, 0);
        // Spent. A later loss with nothing new in the table is unnamed.
        let third = ledger.split(5, &[tally(LOGS_T4, 5)]);
        assert!(third.named.is_empty());
        assert_eq!(third.unnamed, 5);
    }

    #[test]
    fn a_count_nothing_matched_is_dropped_after_one_pass() {
        // Counted against a slot this reader had already read. It waits one
        // pass for a lap that never comes, then stops being evidence.
        let mut ledger = DropLedger::starting_at(&[]);
        ledger.split(0, &[tally(LOGS_T4, 7)]);
        ledger.split(0, &[tally(LOGS_T4, 7)]);
        let later = ledger.split(4, &[tally(LOGS_T4, 7), tally(STOPS_T9, 4)]);
        assert_eq!(later.named, vec![(STOPS_T9, 4)], "the stale log count does not claim a scope loss");
    }

    #[test]
    fn an_oversubscribed_loss_is_shared_in_proportion_and_adds_up() {
        let fresh = [(LOGS_T4, 300), (STOPS_T9, 100)];
        let split = attribute_drops(100, &[], &fresh);
        assert_eq!(split.named, vec![(LOGS_T4, 75), (STOPS_T9, 25)]);
        assert_eq!(split.unnamed, 0);
        assert_eq!(split.carry, vec![(LOGS_T4, 225), (STOPS_T9, 75)]);
        // Shares that do not divide evenly still add up to exactly `lost`.
        for lost in 0..40u64 {
            let split = attribute_drops(lost, &[((1, 0), 3)], &[((2, 0), 7), ((3, 6), 11), ((4, 1), 19)]);
            assert_eq!(total(&split), lost, "lost {lost}");
            assert!(split.named.iter().all(|(_, n)| *n > 0));
        }
        // What was carried in is spent first and never carried again.
        let split = attribute_drops(4, &[(LOGS_T4, 10)], &[(LOGS_T4, 2)]);
        assert_eq!(split.named, vec![(LOGS_T4, 4)]);
        assert_eq!(split.carry, vec![(LOGS_T4, 2)]);
    }

    #[test]
    fn a_counter_that_wrapped_is_read_as_what_it_gained() {
        let mut ledger = DropLedger::starting_at(&[tally(LOGS_T4, u32::MAX - 1)]);
        let split = ledger.split(10, &[tally(LOGS_T4, 3)]);
        assert_eq!(split.named, vec![(LOGS_T4, 5)]);
        assert_eq!(split.unnamed, 5);
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use perfr_scope_ring::logmsg::{level, write_log};

    /// The tests share this process's one segment; one at a time.
    static SEGMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn source() -> (std::sync::MutexGuard<'static, ()>, Arc<perfr_live_server::LiveService>, ScopeSource) {
        let serial = SEGMENT.lock().unwrap_or_else(|p| p.into_inner());
        perfr_api::init().expect("this process's segment");
        let service = perfr_live_server::LiveService::new(perfr_live_server::ServerConfig {
            ring_buffer_bytes: 1 << 20,
            ..Default::default()
        })
        .unwrap();
        let mut source = ScopeSource::new(service.clone());
        source.begin_self_capture();
        assert_eq!(source.segments.len(), 1, "the process's own segment");
        (serial, service, source)
    }

    fn text_of(service: &perfr_live_server::LiveService, event: &LiveEvent) -> String {
        service.intern.lock().get(event.name_id).unwrap_or("<not announced>").to_string()
    }

    #[test]
    fn a_line_cut_off_by_the_end_of_the_capture_still_has_its_text() {
        let (_serial, service, mut source) = source();
        // A three-chunk line whose last chunk never arrives: the producer
        // was mid-write when the capture stopped.
        let message = format!("stopped-mid-line-{}", "x".repeat(70));
        let mut chunks = Vec::new();
        write_log(1, 77, 9, level::WARN, message.as_bytes(), |e| chunks.push(e));
        assert!(chunks.len() >= 3);
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(source.segments[0].logs.accept(chunk).is_none());
        }
        let mut batch = Vec::new();
        source.finish(u64::MAX, &mut batch);
        let line = batch
            .iter()
            .find(|e| e.kind == kind::LOG && e.tid == 77)
            .expect("the unfinished line is reported, not lost");
        assert!(line.log_truncated());
        assert_eq!(line.log_level(), Some(level::WARN));
        // Its text was interned after the drain announced its names. It has
        // to be announced too, or the viewer shows an empty line.
        let text = text_of(&service, line);
        assert!(text.starts_with("stopped-mid-line-"), "got {text:?}");
    }

    #[test]
    fn past_the_text_budget_a_new_line_keeps_everything_but_its_text() {
        let (_serial, service, mut source) = source();
        let line = |text: &str| LogMessage {
            timestamp_ns: 5,
            tid: 78,
            message_id: 1,
            level: level::ERROR,
            message: text.to_string(),
            completeness: Completeness::Complete,
        };
        let mut batch = Vec::new();
        source.emit_log(0, line("kept before the budget ran out"), &mut batch);
        assert_eq!(source.log_text_bytes, "kept before the budget ran out".len());
        // Spend the budget.
        source.log_text_bytes = LOG_TEXT_BUDGET_BYTES;
        source.emit_log(0, line("a brand new line"), &mut batch);
        source.emit_log(0, line("another brand new line"), &mut batch);
        source.emit_log(0, line("kept before the budget ran out"), &mut batch);
        source.announce_new_names();

        assert_eq!(batch.len(), 4, "every line is still an event");
        assert!(batch.iter().all(|e| e.kind == kind::LOG && e.tid == 78 && e.log_level() == Some(level::ERROR)));
        assert_eq!(text_of(&service, &batch[1]), LOG_TEXT_DROPPED);
        assert!(batch[1].log_truncated(), "and says it is not all there");
        assert_eq!(batch[1].name_id, batch[2].name_id, "one shared notice, not a string per line");
        assert_eq!(source.log_lines_without_text, 2);
        // A line already kept costs nothing, so it is never affected.
        assert_eq!(text_of(&service, &batch[3]), "kept before the budget ran out");
        assert!(!batch[3].log_truncated());
        assert_eq!(source.log_text_bytes, LOG_TEXT_BUDGET_BYTES, "the budget did not grow");
    }

    #[test]
    fn a_log_written_through_the_api_reaches_the_timeline_with_its_text() {
        let (_serial, service, mut source) = source();
        let long = format!("api-line-{}", "z".repeat(120));
        perfr_api::log_error(&long);
        perfr_api::log_info("api-short-line");
        let mut batch = Vec::new();
        source.drain_now(perfr_scope_ring::shm::now_monotonic_ns(), &mut batch);
        let texts: Vec<(String, u8, bool)> = batch
            .iter()
            .filter(|e| e.kind == kind::LOG)
            .map(|e| (text_of(&service, e), e.log_level().unwrap(), e.log_truncated()))
            .collect();
        assert!(texts.contains(&(long, level::ERROR, false)), "{texts:?}");
        assert!(texts.contains(&("api-short-line".to_string(), level::INFO, false)), "{texts:?}");
        assert!(batch.iter().all(|e| e.kind != kind::DROP), "nothing was lost, so nothing says so");
    }
}
