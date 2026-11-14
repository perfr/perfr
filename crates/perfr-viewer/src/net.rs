// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What is behind the page: the [`Backend`] it asks, the [`Inbox`] it reads
//! answers from, and the shapes of those answers. The backend that reads
//! capture files is here; the one that talks to the capture service is in
//! `perfr-client`.

// The parsers and JSON mirrors here serve the wasm page and the backend in
// `perfr-client`; the native build only has the empty backend, so nothing
// reads them there.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use perfr_live_event::chrome;
use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct StatusJson {
    pub capturing: bool,
    pub demo: bool,
    #[serde(default)]
    pub events_live: u64,
    #[serde(default)]
    pub events_capacity: u64,
    #[serde(default)]
    pub dropped: u64,
    #[serde(default)]
    pub spilled: u64,
    #[serde(default)]
    pub produced: u64,
    #[serde(default)]
    pub oldest_start_ns: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub newest_end_ns: u64,
    /// Demo/capture producer clock. Not ring newest_end (pid 2/3).
    #[serde(default)]
    pub live_end_ns: u64,
    /// When the running (or last) capture began on its clock; 0 until the
    /// service says. A viewer that connects mid-capture gets no
    /// `CaptureStarted`, so this is how it learns the origin.
    #[serde(default)]
    pub capture_start_ns: u64,
    /// The batch format on the WebSocket, as the server names it.
    #[serde(default)]
    pub wire: String,
    /// The service's log file, where this page's lines are relayed to.
    #[serde(default)]
    pub log_path: Option<String>,
    #[serde(default)]
    pub ring_bytes: u64,
    pub spill_path: Option<String>,
    #[serde(default = "default_machine")]
    pub machine: String,
    /// The pid the running or last capture targets; 0 when none.
    #[serde(default)]
    pub target_pid: u32,
    /// The service's own pid; 0 from an older service.
    #[serde(default)]
    pub service_pid: u32,
    /// PerfrService registered control hooks (real capture).
    #[serde(default)]
    pub hooks: bool,
    /// What dynamic instrumentation did for the running capture: how many
    /// functions were armed, or why none were. Empty when none were asked for.
    #[serde(default)]
    pub instrumentation: String,
    /// JSON summary of a target crash blamed on a hook (the culprit and where
    /// it faulted), empty when none. The viewer shows a banner and marks the
    /// function. See the service's `hook_journal`.
    #[serde(default)]
    pub hook_crash: String,
    /// Auto-profiling is on, and what it is doing: step, budget, the scopes a
    /// second it costs, the hooked set and its last actions (null before it
    /// runs). See the service's `auto_profile.rs`.
    #[serde(default)]
    pub auto_profile: bool,
    #[serde(default)]
    pub auto_profile_status: serde_json::Value,
}

fn default_machine() -> String {
    "local".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProcessJson {
    pub pid: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cpu: f32,
    #[serde(default)]
    pub path: String,
}

/// One row of `GET /api/sampling/report`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SamplingRow {
    pub name: String,
    /// The binary the function came from. Two static functions can share a
    /// name; the module is what tells them apart.
    pub module: String,
    #[serde(rename = "self")]
    pub self_count: u64,
    #[serde(rename = "inclusive")]
    pub inclusive_count: u64,
    pub self_percent: f32,
    pub inclusive_percent: f32,
    /// The function index's id, for hooking the row; 0 when unknown.
    pub function_id: u64,
}

/// The whole report for one selection.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SamplingReport {
    pub samples: u64,
    pub start_ns: u64,
    pub end_ns: u64,
    /// Ranges in the selection, or instances of the scope for a
    /// scope-scoped report.
    pub range_count: u64,
    /// The scope's name for a scope-scoped report; empty otherwise.
    pub scope: String,
    #[serde(rename = "functions")]
    pub rows: Vec<SamplingRow>,
}

#[derive(Clone, Debug, Default, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct SymbolsStatusJson {
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub function_count: u64,
    #[serde(default)]
    pub module_count: u64,
    #[serde(default)]
    pub error: String,
}

#[derive(Clone, Debug, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct FunctionHit {
    pub function_id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub size: u64,
    /// Hook-safety cue: "safe" | "risky" | "unsafe" | "unknown" (or empty
    /// from an older service). See the service's `hook_safety`.
    #[serde(default)]
    pub safety: String,
    /// Why, for the tooltip.
    #[serde(default)]
    pub safety_reason: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct FunctionSearchJson {
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub status: String,
    /// How many functions the service indexed, listed or not.
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub functions: Vec<FunctionHit>,
}

/// One node of a call tree. Recursive, because that is what it is: the JSON
/// nests children inside parents and the panel walks it the same way.
#[derive(Clone, Debug, Default, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct TreeNodeJson {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub address: u64,
    #[serde(default)]
    pub function_id: u64,
    #[serde(default)]
    pub inclusive: u64,
    #[serde(default)]
    pub exclusive: u64,
    #[serde(default)]
    pub inclusive_percent: f64,
    #[serde(default)]
    pub of_parent_percent: f64,
    #[serde(default)]
    pub children: Vec<TreeNodeJson>,
}

#[derive(Clone, Debug, Default, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct SamplingTree {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub samples: u64,
    #[serde(default)]
    pub start_ns: u64,
    #[serde(default)]
    pub end_ns: u64,
    #[serde(default)]
    pub roots: Vec<TreeNodeJson>,
}

pub fn parse_sampling_tree_json(text: &str) -> Result<SamplingTree, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/sampling/tree: {e}"))
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ModuleRow {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub function_count: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
// Mirrors the service's JSON; fields the viewer does not read yet stay so the
// schema is written down once, here.
#[allow(dead_code)]
pub struct ModulesJson {
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub modules: Vec<ModuleRow>,
}

pub fn parse_modules_json(text: &str) -> Result<ModulesJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/symbols/modules: {e}"))
}

#[derive(Clone, Debug)]
pub struct CaptureStart {
    pub pid: u32,
    pub enable_api: bool,
    pub context_switches: bool,
    pub thread_states: bool,
    pub sampling: bool,
    pub samples_per_second: f64,
    pub unwinding: String,
    pub dynamic_instrumentation_method: String,
    pub instrumented_function_ids: Vec<u64>,
    pub show_all_processes: bool,
    pub uprobe_duplicate_filter: bool,
    pub auto_profile: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TimelineJson {
    pub lod: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    #[allow(dead_code)]
    pub height: u32,
    #[serde(default)]
    pub instances: Vec<InstanceJson>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct InstanceJson {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: String,
    pub r: f32,
}

#[derive(Clone, Debug)]
pub struct ServiceFrame {
    pub width: u32,
    pub lanes: u32,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct Inbox {
    pub share_notes: Vec<crate::share::ShareNote>,
    /// The login popup came back (storage event or postMessage).
    pub share_login_signalled: bool,
    /// A bearer the popup stored, if the login redirect sent `#share_token=`.
    pub share_token: Option<String>,
    /// A `.perfr.stream` just loaded from a file or `?capture=` URL. The
    /// app ingests it and keeps the bytes for Share.
    pub loaded_stream: Option<Vec<u8>>,
    /// Whether that stream was named by the page's address (`?capture=`,
    /// a story's `capture`) rather than opened by the reader. See
    /// `PerfrLiveApp::accept_stream_by_address`.
    pub loaded_stream_by_address: bool,
    /// A story document fetched from `?story=`.
    pub loaded_story: Option<String>,
    pub status: Option<StatusJson>,
    /// The service's persisted user settings (`/api/settings`), as the raw
    /// object so keys this viewer does not know survive a round trip.
    pub settings: Option<serde_json::Value>,
    /// `/api/bench`: the benchmark producer's knobs and counters.
    pub bench: Option<serde_json::Value>,
    pub processes: Option<Vec<ProcessJson>>,
    pub error: Option<String>,
    pub frames: Vec<Vec<u8>>,
    /// The bytes waiting in `frames`, kept so each message does not have to
    /// add them up.
    frames_bytes: usize,
    /// The backlog overflowed: messages are refused until a stream starts.
    frames_dropped: bool,
    pub timeline: Option<TimelineJson>,
    pub frame: Option<ServiceFrame>,
    pub http_ok: bool,
    pub ws_ok: bool,
    /// Bytes received on the event stream since the page opened. Cumulative,
    /// so a reader differences it against its last reading.
    pub bytes_in: u64,
    pub symbols: Option<SymbolsStatusJson>,
    pub tree: Option<SamplingTree>,
    pub modules: Option<ModulesJson>,
    pub sampling: Option<SamplingReport>,
    pub function_hits: Option<FunctionSearchJson>,
    /// Every function of a process, for the Functions view.
    pub function_list: Option<FunctionSearchJson>,
    pub preset_functions: Vec<(u64, Result<FunctionSearchJson, String>)>,
    /// The code views: a disassembly and a source file, or why not.
    pub disassembly: Option<Result<crate::code::Disassembly, String>>,
    pub source: Option<Result<crate::code::SourceFile, String>>,
}

/// How many bytes of the event stream may wait in the inbox while the page
/// is hidden. A hidden page draws no frames, so nothing takes the messages
/// out, yet the socket keeps delivering: left alone the queue grew at the
/// full stream rate until the 2 GiB heap ran out and the tab aborted. Far
/// above what gathers between two frames of a page that is drawing.
pub const HIDDEN_BACKLOG_MAX: usize = 64 << 20;

/// What became of a socket message handed to [`Inbox::queue_frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queued {
    /// It waits for the app.
    Kept,
    /// The page is hidden and the backlog is full: everything waiting was
    /// dropped, this message with it. The socket is to be closed; the next
    /// one opens with a Hello and the whole ring, which is how the app
    /// catches up without having held the stream in between.
    Overflow,
    /// It continues a stream that was dropped, and went the same way.
    Discarded,
}

impl Inbox {
    /// Queues one message of the event socket for the app.
    pub fn queue_frame(&mut self, bytes: Vec<u8>, page_hidden: bool) -> Queued {
        if self.frames_dropped {
            // The rest of a dropped stream would be read as garbage after
            // the part that got through. Only a new stream is taken.
            if !crate::frames::starts_stream(&bytes) {
                return Queued::Discarded;
            }
            self.frames_dropped = false;
        }
        if page_hidden && self.frames_bytes.saturating_add(bytes.len()) > HIDDEN_BACKLOG_MAX {
            self.frames = Vec::new();
            self.frames_bytes = 0;
            self.frames_dropped = true;
            return Queued::Overflow;
        }
        self.frames_bytes += bytes.len();
        self.frames.push(bytes);
        Queued::Kept
    }

    /// The queued messages, for the app; the backlog starts again from zero.
    pub fn take_frames(&mut self) -> Vec<Vec<u8>> {
        self.frames_bytes = 0;
        std::mem::take(&mut self.frames)
    }
}

/// Tells the latest request of one kind from those it replaced.
///
/// The report, the call tree, the disassembly and the source file each have
/// one slot in the inbox and replies that say nothing of the request they
/// answer. Two requests in flight could finish in either order, and the
/// slower one won: a long selection's report landing over the short one
/// made after it, the panel then showing a selection that was no longer
/// there. A request takes a number, and its reply is dropped unless that
/// number is still the latest.
#[derive(Clone, Default)]
pub struct Latest(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Latest {
    /// A new request: every earlier one of this kind is now stale.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    /// Whether the request numbered `seq` is still the one being waited for.
    pub fn is(&self, seq: u64) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst) == seq
    }
}

/// The one Share upload that may be on its way, so it can be stopped.
///
/// `H` is the handle that aborts the request once it is being sent (the
/// browser's `XMLHttpRequest`); between the click and the first byte there
/// is a session check, during which there is no handle yet and stopping
/// means never sending.
pub struct ShareUpload<H> {
    /// The share generation the upload belongs to; `None` when there is
    /// none, or it was stopped.
    gen: Option<u64>,
    handle: Option<H>,
}

impl<H> Default for ShareUpload<H> {
    fn default() -> Self {
        Self { gen: None, handle: None }
    }
}

impl<H> ShareUpload<H> {
    /// An upload for `gen` starts. Returns the handle of one still being
    /// sent for another generation, for the caller to abort.
    pub fn begin(&mut self, gen: u64) -> Option<H> {
        let stale = self.stop_unless(gen);
        self.gen = Some(gen);
        stale
    }

    /// The request for `gen` is about to be sent with `handle`. False when
    /// the upload was stopped or replaced meanwhile: it must not be sent.
    pub fn sending(&mut self, gen: u64, handle: H) -> bool {
        if self.gen != Some(gen) {
            return false;
        }
        self.handle = Some(handle);
        true
    }

    /// Stops the upload, whatever its generation. Returns the handle to
    /// abort when the request was already being sent.
    pub fn stop(&mut self) -> Option<H> {
        self.gen = None;
        self.handle.take()
    }

    /// A share call for `gen` arrived. The app moves to a new generation
    /// only by forgetting the capture the old one was about, so an upload
    /// of any other generation is one nobody is waiting for.
    pub fn stop_unless(&mut self, gen: u64) -> Option<H> {
        if self.gen.is_some_and(|g| g != gen) {
            self.stop()
        } else {
            None
        }
    }

    /// The request for `gen` ended, one way or another.
    pub fn finish(&mut self, gen: u64) {
        if self.gen == Some(gen) {
            self.gen = None;
            self.handle = None;
        }
    }
}

pub fn parse_status_json(text: &str) -> Result<StatusJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/status: {e}"))
}

pub fn parse_processes_json(text: &str) -> Result<Vec<ProcessJson>, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/processes: {e}"))
}

/// Builds the `?ranges=` (or empty) query for a set of `(start, end, tid)`
/// windows. `end` of 0 is left as 0, which the server reads as "to the end".
/// An empty selection returns an empty string -- the whole capture.
pub fn ranges_query(ranges: &[(u64, u64, Option<u32>)]) -> String {
    if ranges.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = ranges
        .iter()
        .map(|&(a, b, tid)| match tid {
            Some(t) => format!("{a}-{b}:{t}"),
            None => format!("{a}-{b}"),
        })
        .collect();
    format!("?ranges={}", parts.join(","))
}

/// The query of `GET /api/sampling/tree`: the selection's ranges, if any,
/// then the tree's mode.
pub fn sampling_tree_query(ranges: &[(u64, u64, Option<u32>)], mode: &str) -> String {
    let rq = ranges_query(ranges);
    let sep = if rq.is_empty() { '?' } else { '&' };
    format!("{rq}{sep}mode={mode}")
}

/// The body of `POST /api/capture/start`.
pub fn capture_start_body(req: &CaptureStart) -> String {
    let fns: String = req
        .instrumented_function_ids
        .iter()
        .map(|id| format!(r#"{{"function_id":{id}}}"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"{{"pid":{},"enable_api":{},"context_switches":{},"thread_states":{},"sampling":{},"samples_per_second":{},"unwinding":"{}","dynamic_instrumentation_method":"{}","instrumented_functions":[{fns}],"show_all_processes":{},"uprobe_duplicate_filter":{},"auto_profile":{}}}"#,
        req.pid,
        req.enable_api,
        req.context_switches,
        req.thread_states,
        req.sampling,
        req.samples_per_second,
        json_escape(&req.unwinding),
        json_escape(&req.dynamic_instrumentation_method),
        req.show_all_processes,
        req.uprobe_duplicate_filter,
        req.auto_profile,
    )
}

/// The body of `PUT /api/config`. An empty spill path means no spill file.
pub fn config_body(ring_bytes: u64, spill: &str) -> String {
    let spill_json = if spill.is_empty() {
        "null".to_string()
    } else {
        format!("\"{}\"", json_escape(spill))
    };
    format!(r#"{{"ring_buffer_bytes":{ring_bytes},"spill_path":{spill_json}}}"#)
}

/// The inside of a JSON string, for the bodies built with `format!` above.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parses the sampling report. Text that is not a report (an older
/// service's plain "not provided" body) reads as an empty one: the panel
/// then shows no rows instead of the previous selection's.
pub fn parse_sampling_report_json(text: &str) -> Result<SamplingReport, String> {
    Ok(serde_json::from_str(text).unwrap_or_default())
}

pub fn parse_symbols_status_json(text: &str) -> Result<SymbolsStatusJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/symbols/status: {e}"))
}

pub fn parse_function_search_json(text: &str) -> Result<FunctionSearchJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/functions/search: {e}"))
}

pub fn parse_timeline_json(text: &str) -> Result<TimelineJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/timeline: {e}"))
}

/// `/api/frame` body: 16-byte header + `width * lanes * 4` RGBA.
pub fn parse_frame_body(bytes: &[u8]) -> Result<ServiceFrame, String> {
    if bytes.len() < 16 {
        return Err(format!("/api/frame: short header ({})", bytes.len()));
    }
    let width = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let lanes = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let expected = width as usize * lanes as usize * 4;
    if bytes.len() < 16 + expected {
        return Err(format!(
            "/api/frame: body {} < 16+{expected} (width={width} lanes={lanes})",
            bytes.len()
        ));
    }
    Ok(ServiceFrame {
        width,
        lanes,
        rgba: bytes[16..16 + expected].to_vec(),
    })
}

/// `#RRGGBB` or `#RRGGBBAA` → `0xAARRGGBB`.
pub fn css_to_argb(css: &str) -> u32 {
    let s = css.trim().trim_start_matches('#');
    let n = u32::from_str_radix(s, 16).unwrap_or(0);
    match s.len() {
        8 => {
            let r = (n >> 24) & 0xFF;
            let g = (n >> 16) & 0xFF;
            let b = (n >> 8) & 0xFF;
            let a = n & 0xFF;
            (a << 24) | (r << 16) | (g << 8) | b
        }
        6 => 0xFF00_0000 | n,
        _ => 0xFF32_3232,
    }
}

pub fn instances_from_timeline(tl: &TimelineJson) -> Vec<perfr_live_render::ScopeInstance> {
    tl.instances
        .iter()
        .map(|i| perfr_live_render::ScopeInstance {
            x: i.x,
            y: i.y,
            w: i.w,
            h: i.h,
            color: css_to_argb(&i.color),
            radius: i.r,
            name_id: 0,
            start_ns: 0,
            duration_ns: 0,
            pid: 0,
            tid: 0,
            kind: 0,
            depth: 0,
            extra: 0,
            event_flags: 0,
            flags: 0.0,
        })
        .collect()
}

/// Repeat each lane row so a dest-rect blit is not a one-pixel barcode.
pub fn scale_frame_rgba(frame: &ServiceFrame, row_h: u32) -> (Vec<u8>, u32) {
    let row_h = row_h.max(1);
    let w = frame.width as usize;
    let height = frame.lanes.saturating_mul(row_h);
    let mut out = vec![0u8; w.saturating_mul(height as usize).saturating_mul(4)];
    for px in out.chunks_exact_mut(4) {
        px[0] = ((chrome::TRACK >> 16) & 0xFF) as u8;
        px[1] = ((chrome::TRACK >> 8) & 0xFF) as u8;
        px[2] = (chrome::TRACK & 0xFF) as u8;
        px[3] = ((chrome::TRACK >> 24) & 0xFF) as u8;
    }
    for lane in 0..frame.lanes as usize {
        let src = lane * w * 4;
        if src + w * 4 > frame.rgba.len() {
            break;
        }
        for dy in 0..row_h as usize {
            let dest = (lane * row_h as usize + dy) * w * 4;
            out[dest..dest + w * 4].copy_from_slice(&frame.rgba[src..src + w * 4]);
        }
    }
    (out, height.max(1))
}

impl Inbox {
    /// Everything that arrived since the last call. The link's standing
    /// (`http_ok`, `ws_ok`) and the running total (`bytes_in`) are copied,
    /// not taken: they describe the connection, not one delivery.
    pub fn drain(&mut self) -> Inbox {
        Inbox {
            share_notes: std::mem::take(&mut self.share_notes),
            share_login_signalled: std::mem::take(&mut self.share_login_signalled),
            share_token: self.share_token.take(),
            loaded_stream: self.loaded_stream.take(),
            loaded_stream_by_address: std::mem::take(&mut self.loaded_stream_by_address),
            loaded_story: self.loaded_story.take(),
            status: self.status.take(),
            settings: self.settings.take(),
            bench: self.bench.take(),
            processes: self.processes.take(),
            sampling: self.sampling.take(),
            error: self.error.take(),
            frames: self.take_frames(),
            frames_bytes: 0,
            frames_dropped: false,
            timeline: self.timeline.take(),
            frame: self.frame.take(),
            http_ok: self.http_ok,
            ws_ok: self.ws_ok,
            bytes_in: self.bytes_in,
            symbols: self.symbols.take(),
            tree: self.tree.take(),
            modules: self.modules.take(),
            function_hits: self.function_hits.take(),
            function_list: self.function_list.take(),
            preset_functions: std::mem::take(&mut self.preset_functions),
            disassembly: self.disassembly.take(),
            source: self.source.take(),
        }
    }
}

/// What is behind the page: where captures and stories come from, and where
/// a request goes.
///
/// The page asks through these methods and reads the answers out of the
/// [`Inbox`] that [`take`](Self::take) hands over once a frame. Only `take`
/// has to be written. Every other method does nothing unless a backend
/// overrides it, which is what a request amounts to when there is nobody to
/// ask: this crate's own backend (`web::FileBackend`) reads capture files
/// and stories and leaves the rest alone. The one that talks to the capture
/// service is in `perfr-client`.
pub trait Backend {
    /// What arrived since the last call.
    fn take(&self) -> Inbox;

    // Files: what a page with no service can still do.

    /// Fetch a story document. The text lands in `loaded_story`.
    fn fetch_story(&self, _url: &str) {}
    /// Fetch a `.perfr.stream`. The bytes land in `loaded_stream`.
    fn fetch_capture(&self, _url: &str) {}
    /// Reads a `.perfr.stream` the reader opened or dropped on the page.
    #[cfg(target_arch = "wasm32")]
    fn import_stream_file(&self, _file: web_sys::File) {}

    // The capture service.

    fn get_status(&self) {}
    fn get_settings(&self) {}
    fn put_settings(&self, _settings: &serde_json::Value) {}
    fn set_auto_profile(&self, _on: bool) {}
    fn close_ws(&self) {}
    fn reconnect_ws_if_closed(&self) {}
    fn get_sampling_report(&self, _ranges: &[(u64, u64, Option<u32>)]) {}
    fn get_sampling_report_scope(&self, _name_id: u32) {}
    fn get_sampling_tree_scope(&self, _name_id: u32, _mode: &str) {}
    fn get_sampling_tree(&self, _ranges: &[(u64, u64, Option<u32>)], _mode: &str) {}
    fn get_modules(&self, _pid: u32) {}
    fn get_disassembly(&self, _pid: u32, _function_id: u64) {}
    fn get_example_disassembly(&self) {}
    fn get_source(&self, _path: &str) {}
    fn get_processes(&self) {}
    fn pull_view(&self, _t0: u64, _t1: u64, _width: u32) {}
    fn start_capture(&self, _req: &CaptureStart) {}
    fn stop_capture(&self) {}
    fn load_symbols(&self, _pid: u32) {}
    fn get_symbols_status(&self, _pid: u32) {}
    fn search_functions(&self, _pid: u32, _q: &str, _limit: u32) {}
    fn list_functions(&self, _pid: u32) {}
    fn resolve_preset_functions(&self, _pid: u32, _generation: u64, _keys: String) {}
    fn get_bench(&self) {}
    fn start_bench(&self, _threads: u32, _depth_min: u32, _depth_max: u32, _rate: u64) {}
    fn set_bench_rate(&self, _rate: u64) {}
    fn stop_bench(&self) {}
    fn start_demo(&self) {}
    fn stop_demo(&self) {}
    fn apply_config(&self, _ring_bytes: u64, _spill: &str) {}
    /// Posts a `.perfr.zip` to the service, which opens it as the current
    /// capture and streams it back.
    fn import_capture(&self, _bytes: Vec<u8>) {}
    /// As [`import_capture`](Self::import_capture), reading the browser
    /// `File` first.
    #[cfg(target_arch = "wasm32")]
    fn import_capture_file(&self, _file: web_sys::File) {}
    fn clear_capture(&self) {}

    // Sharing a capture with the website.

    /// What an earlier page load left for Share: a bearer from a login
    /// redirect, and whether a same-tab login asked for the dialog to
    /// reopen.
    fn stored_share_auth(&self) -> (Option<String>, bool) {
        (None, false)
    }
    fn clear_share_resume_flag(&self) {}
    fn check_share_session(&self, _gen: u64, _url: String, _token: Option<String>) {}
    fn open_share_login(&self, _url: &str) -> bool {
        false
    }
    fn prepare_share(&self, _gen: u64, _path: String) {}
    fn share_capture(
        &self,
        _gen: u64,
        _endpoint: String,
        _me_url: String,
        _token: Option<String>,
        _bytes: Vec<u8>,
    ) {
    }
    fn abort_share(&self) {}
}

/// A backend with nothing behind it. Native builds use it: the tests drive
/// the page by handing it frames and answers directly.
#[derive(Default)]
pub struct NoBackend;

impl Backend for NoBackend {
    fn take(&self) -> Inbox {
        Inbox::default()
    }
}

/// The page's backend, whichever it is.
pub type Net = Box<dyn Backend>;

/// What a page is built on. The viewer's own page is [`Host::viewer`];
/// `perfr-client` passes the one with the capture service in it.
#[derive(Clone, Copy)]
pub struct Host {
    service: bool,
    share: bool,
    /// Opens what is behind the page. Given an address (the page's
    /// `?capture=`), that capture file is fetched and nothing else is
    /// contacted.
    pub connect: fn(Option<&str>) -> Net,
}

/// Whether the parts of the interface that need a capture service are in
/// this build at all: recording, the process picker, hooks, the service's
/// settings, the code views, Share. They are the `service` feature, which
/// `perfr-client` turns on and the viewer's own pack does not have. Every
/// one of them is reached through [`Host::service`] or [`Host::share`], so
/// in a build without the feature those are constant, the compiler drops
/// the branches behind them, and nothing of capture control or sharing is
/// left in the pack. The tests build both kinds of page, so they have it.
pub(crate) const SERVICE_BUILT: bool = cfg!(any(test, feature = "service"));

impl Host {
    /// The viewer's own page: capture files, no service, no sharing.
    pub fn viewer() -> Self {
        Self {
            service: false,
            share: false,
            connect: open_files,
        }
    }

    /// A page served by a capture service, which can share. `connect`
    /// opens the connection to it.
    #[cfg(any(test, feature = "service"))]
    pub fn with_service(connect: fn(Option<&str>) -> Net) -> Self {
        Self {
            service: true,
            share: true,
            connect,
        }
    }

    /// A capture service serves this page. With one, a page opened without
    /// `?capture=` connects to it and shows what goes with it: recording,
    /// the process picker, hooks, the service's settings. Without one the
    /// page shows the captures it is given and none of that.
    #[inline(always)]
    pub fn service(&self) -> bool {
        SERVICE_BUILT && self.service
    }

    /// Captures can be sent to the website from this page (Share).
    #[inline(always)]
    pub fn share(&self) -> bool {
        SERVICE_BUILT && self.share
    }
}

#[cfg(test)]
impl Host {
    /// A page with a service and sharing, as `perfr-client` builds it, over
    /// the empty backend: the tests hand the page its frames and answers.
    pub(crate) fn test_service() -> Self {
        Self::with_service(|_| Box::new(NoBackend))
    }
}

/// The backend of [`Host::viewer`].
fn open_files(capture: Option<&str>) -> Net {
    #[cfg(target_arch = "wasm32")]
    {
        Box::new(web::FileBackend::open(capture))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = capture;
        Box::new(NoBackend)
    }
}

/// Browser plumbing: the file backend, and the fetches a backend in another
/// crate is built from as well.
#[cfg(target_arch = "wasm32")]
pub mod web {
    use super::{Backend, Inbox};
    use std::sync::{Arc, Mutex};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::Response;

    /// The inbox as the browser's callbacks share it, all on one thread.
    pub type SharedInbox = Arc<Mutex<Inbox>>;

    /// Capture files and stories, fetched by address or opened by the
    /// reader. No service and no socket: every other request is the
    /// trait's default, which does nothing.
    #[derive(Clone, Default)]
    pub struct FileBackend {
        inbox: SharedInbox,
    }

    impl FileBackend {
        /// A page with nothing loaded yet. There is no link to lose, so it
        /// reads as up from the start.
        pub fn new() -> Self {
            let backend = Self::default();
            {
                let mut g = backend.inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.ws_ok = true;
                g.http_ok = true;
            }
            backend
        }

        /// Fetches a capture stream file (`/api/capture/export?format=stream`
        /// saved to disk) and feeds it in as if a service had sent it. The
        /// static web page's mode: no service, no socket.
        pub fn from_capture_url(url: &str) -> Self {
            let backend = Self::default();
            let url = url.to_string();
            let fetch_into = backend.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_bytes(&url).await;
                let mut g = fetch_into.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(bytes) => {
                        g.bytes_in += bytes.len() as u64;
                        g.loaded_stream = Some(bytes);
                        g.loaded_stream_by_address = true;
                        g.ws_ok = true;
                        g.http_ok = true;
                    }
                    Err(e) => g.error = Some(format!("capture file: {e}")),
                }
            });
            backend
        }

        /// [`from_capture_url`](Self::from_capture_url) when the page names
        /// a capture, [`new`](Self::new) when it does not.
        pub fn open(capture: Option<&str>) -> Self {
            match capture {
                Some(url) => Self::from_capture_url(url),
                None => Self::new(),
            }
        }

        /// The inbox, for a backend that adds to what this one delivers.
        pub fn inbox(&self) -> SharedInbox {
            self.inbox.clone()
        }
    }

    impl Backend for FileBackend {
        fn take(&self) -> Inbox {
            self.inbox.lock().unwrap_or_else(|e| e.into_inner()).drain()
        }

        fn fetch_story(&self, url: &str) {
            let url = url.to_string();
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&url).await;
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(text) => g.loaded_story = Some(text),
                    Err(e) => g.error = Some(format!("story: {e}")),
                }
            });
        }

        fn fetch_capture(&self, url: &str) {
            let url = url.to_string();
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_bytes(&url).await;
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(bytes) => {
                        g.bytes_in += bytes.len() as u64;
                        g.loaded_stream = Some(bytes);
                        g.loaded_stream_by_address = true;
                        g.http_ok = true;
                    }
                    Err(e) => g.error = Some(format!("capture file: {e}")),
                }
            });
        }

        fn import_stream_file(&self, file: web_sys::File) {
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = file_bytes(&file).await;
                let mut g = inbox.lock().unwrap_or_else(|p| p.into_inner());
                match result {
                    Ok(bytes) => {
                        g.loaded_stream = Some(bytes);
                        g.loaded_stream_by_address = false;
                    }
                    Err(e) => g.error = Some(format!("open capture: {e}")),
                }
            });
        }
    }

    /// The bytes of a file the reader gave the page.
    pub async fn file_bytes(file: &web_sys::File) -> Result<Vec<u8>, String> {
        let buf = JsFuture::from(file.array_buffer()).await.map_err(js_err)?;
        let arr = js_sys::Uint8Array::new(&buf);
        let mut bytes = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut bytes);
        Ok(bytes)
    }

    /// GET with `fetch_with_str` — no `RequestInit` (that path hung/panicked).
    pub async fn get_text(url: &str) -> Result<String, String> {
        let window = web_sys::window().ok_or("no window")?;
        let resp = JsFuture::from(window.fetch_with_str(url))
            .await
            .map_err(js_err)?;
        let resp: Response = resp
            .dyn_into()
            .map_err(|_| "fetch: not a Response".to_string())?;
        let status = resp.status();
        let text = JsFuture::from(resp.text().map_err(js_err)?)
            .await
            .map_err(js_err)?;
        let text = text.as_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("{url}: {status} {text}"));
        }
        Ok(text)
    }

    pub async fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
        let window = web_sys::window().ok_or("no window")?;
        let resp = JsFuture::from(window.fetch_with_str(url))
            .await
            .map_err(js_err)?;
        let resp: Response = resp
            .dyn_into()
            .map_err(|_| "fetch: not a Response".to_string())?;
        let status = resp.status();
        if !(200..300).contains(&status) {
            return Err(format!("{url}: {status}"));
        }
        let buf = JsFuture::from(resp.array_buffer().map_err(js_err)?)
            .await
            .map_err(js_err)?;
        let arr = js_sys::Uint8Array::new(&buf);
        let mut out = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut out);
        Ok(out)
    }

    pub fn js_err(v: JsValue) -> String {
        if let Some(s) = v.as_string() {
            return s;
        }
        if let Ok(e) = v.clone().dyn_into::<js_sys::Error>() {
            return String::from(e.message());
        }
        format!("{v:?}")
    }
}

/// A query value, percent-encoded: paths carry `/`, spaces and worse.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_report_with_sorted_keys_parses_its_rows() {
        // Exactly what the service writes: serde_json's sorted keys, so a
        // row starts with "inclusive", not "name"; plus a scope report's
        // extra keys.
        let text = r#"{"end_ns":441288689745100,"first_sample_ns":441285747520712,"functions":[{"inclusive":1217,"inclusive_percent":64.35,"module":"","name":"0x789be0844a09","self":1217,"self_percent":64.35},{"inclusive":1754,"inclusive_percent":92.75,"module":"libc.so.6","name":"__clock_gettime","self":63,"self_percent":3.33}],"last_sample_ns":441288689000000,"range_count":1877,"samples":1891,"scope":"physics-0","start_ns":441285747000000,"tid":null}"#;
        let r = super::parse_sampling_report_json(text).unwrap();
        assert_eq!(r.samples, 1891);
        assert_eq!(r.range_count, 1877);
        assert_eq!(r.scope, "physics-0");
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0].name, "0x789be0844a09");
        assert_eq!(r.rows[0].self_count, 1217);
        assert_eq!(r.rows[1].name, "__clock_gettime");
        assert_eq!(r.rows[1].module, "libc.so.6");
        assert_eq!(r.rows[1].inclusive_count, 1754);
        assert!((r.rows[1].self_percent - 3.33).abs() < 0.01);
        // A brace inside a name does not end the row.
        let odd = r#"{"functions":[{"inclusive":1,"module":"m","name":"operator{}","self":1}],"samples":1}"#;
        let r = super::parse_sampling_report_json(odd).unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].name, "operator{}");
    }

    use super::{parse_sampling_report_json, ranges_query, SamplingReport};

    #[test]
    fn ranges_query_is_empty_for_the_whole_capture() {
        assert_eq!(ranges_query(&[]), "");
    }

    #[test]
    fn ranges_query_encodes_windows_and_tids() {
        let q = ranges_query(&[(100, 200, Some(7)), (500, 800, None)]);
        assert_eq!(q, "?ranges=100-200:7,500-800");
    }


    #[test]
    fn a_row_keeps_the_module_its_function_came_from() {
        let json = r#"{"samples":10,"start_ns":0,"end_ns":9,"functions":[
            {"name":"work","module":"libc.so.6","self":6,"inclusive":6,"self_percent":60.0,"inclusive_percent":60.0},
            {"name":"main","module":"app","self":4,"inclusive":10,"self_percent":40.0,"inclusive_percent":100.0}]}"#;
        let report = parse_sampling_report_json(json).unwrap();
        assert_eq!(report.rows[0].module, "libc.so.6");
        assert_eq!(report.rows[1].module, "app");
        // The row slice must not leak the next row's module into this one.
        assert_eq!(report.rows[0].name, "work");
    }

    #[test]
    fn a_report_without_modules_still_parses() {
        // An older service does not send the field; an empty column beats a
        // failed parse.
        let json = r#"{"samples":1,"functions":[{"name":"main","self":1,"inclusive":1,"self_percent":100.0,"inclusive_percent":100.0}]}"#;
        let report = parse_sampling_report_json(json).unwrap();
        assert_eq!(report.rows[0].module, "");
    }

    #[test]
    fn a_call_tree_parses_with_its_nesting_intact() {
        let json = r#"{"mode":"bottom_up","samples":3,"roots":[
            {"kind":"function","name":"inner","module":"app","address":4096,"inclusive":2,"exclusive":0,
             "inclusive_percent":66.6,"of_parent_percent":66.6,"children":[
               {"kind":"thread","name":"Thread 7","module":"","address":0,"inclusive":2,"exclusive":2,
                "inclusive_percent":66.6,"of_parent_percent":100.0,"children":[]}]}]}"#;
        let tree = parse_sampling_tree_json(json).unwrap();
        assert_eq!(tree.mode, "bottom_up");
        assert_eq!(tree.samples, 3);
        let root = &tree.roots[0];
        assert_eq!(root.name, "inner");
        assert_eq!(root.address, 4096);
        let leaf = &root.children[0];
        assert_eq!(leaf.kind, "thread");
        assert_eq!(leaf.exclusive, 2);
        assert!(leaf.children.is_empty());
    }

    #[test]
    fn parses_a_sampling_report() {
        let json = r#"{"samples":1200,"start_ns":10,"end_ns":99,"functions":[
            {"name":"main","self":0,"inclusive":1200,"self_percent":0.0,"inclusive_percent":100.0},
            {"name":"work","self":800,"inclusive":900,"self_percent":66.6,"inclusive_percent":75.0}]}"#;
        let r = parse_sampling_report_json(json).unwrap();
        assert_eq!(r.samples, 1200);
        assert_eq!(r.start_ns, 10);
        assert_eq!(r.end_ns, 99);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0].name, "main");
        assert_eq!(r.rows[0].self_count, 0);
        assert_eq!(r.rows[0].inclusive_count, 1200);
        assert_eq!(r.rows[1].name, "work");
        assert_eq!(r.rows[1].self_count, 800);
        assert!((r.rows[1].inclusive_percent - 75.0).abs() < 0.01);
    }

    #[test]
    fn a_name_with_json_escapes_comes_out_as_the_name() {
        // A C++ user-defined literal, a Windows path, a non-ASCII name the
        // service chose to escape, and an exponent where a count could be.
        let json = r#"{"samples":3,"scope":"say \"hi\"","functions":[
            {"name":"operator\"\" _km(long double)","module":"C:\\bin\\app.exe","self":2,"inclusive":3,"self_percent":66.6,"inclusive_percent":1e2},
            {"name":"caf\u00e9::brew","module":"","self":1,"inclusive":1,"self_percent":3.33e1,"inclusive_percent":33.3}]}"#;
        let r = parse_sampling_report_json(json).unwrap();
        assert_eq!(r.scope, "say \"hi\"");
        assert_eq!(r.rows.len(), 2, "got {:?}", r.rows);
        assert_eq!(r.rows[0].name, "operator\"\" _km(long double)");
        assert_eq!(r.rows[0].module, "C:\\bin\\app.exe");
        assert!((r.rows[0].inclusive_percent - 100.0).abs() < 0.01);
        assert_eq!(r.rows[1].name, "café::brew");
        assert!((r.rows[1].self_percent - 33.3).abs() < 0.01);
    }

    #[test]
    fn an_empty_report_is_not_an_error() {
        let r = parse_sampling_report_json(r#"{"samples":0,"functions":[]}"#).unwrap();
        assert_eq!(r.samples, 0);
        assert!(r.rows.is_empty());
    }

    #[test]
    fn a_name_containing_braces_does_not_eat_the_rest_of_the_list() {
        // C++ symbols carry all sorts of punctuation; a row must end at the
        // next row, not at the next brace.
        let json = r#"{"samples":2,"functions":[
            {"name":"std::map<int, {weird}>::find","self":1,"inclusive":1,"self_percent":50.0,"inclusive_percent":50.0},
            {"name":"other","self":1,"inclusive":1,"self_percent":50.0,"inclusive_percent":50.0}]}"#;
        let r = parse_sampling_report_json(json).unwrap();
        assert_eq!(r.rows.len(), 2, "got {:?}", r.rows);
        assert_eq!(r.rows[1].name, "other");
    }

    #[test]
    fn a_501_body_yields_an_empty_report_not_a_panic() {
        let r: SamplingReport =
            parse_sampling_report_json("this service does not provide sampling reports").unwrap();
        assert_eq!(r.samples, 0);
        assert!(r.rows.is_empty());
    }

    use super::*;

    /// The first message of every socket.
    fn hello_message() -> Vec<u8> {
        perfr_live_protocol::encode_frame(&perfr_live_protocol::LiveFrame::Hello {
            version: perfr_live_protocol::VERSION,
            event_size: perfr_live_event::LIVE_EVENT_SIZE as u16,
        })
    }

    #[test]
    fn a_page_that_is_drawing_queues_every_message_whatever_the_backlog() {
        // A visible page takes the queue every frame. A burst larger than
        // the hidden limit between two frames (the snapshot of a large ring
        // on a slow frame) must still all arrive: dropping it would close
        // the socket, and the next snapshot would meet the same end.
        let mut inbox = Inbox::default();
        let message = vec![0u8; HIDDEN_BACKLOG_MAX / 4];
        for _ in 0..6 {
            assert_eq!(inbox.queue_frame(message.clone(), false), Queued::Kept);
        }
        assert_eq!(inbox.take_frames().len(), 6);
    }

    #[test]
    fn a_hidden_page_stops_holding_the_stream_once_the_backlog_is_full() {
        let mut inbox = Inbox::default();
        let message = vec![0u8; HIDDEN_BACKLOG_MAX / 4];
        for _ in 0..4 {
            assert_eq!(inbox.queue_frame(message.clone(), true), Queued::Kept);
        }
        // One byte more than the limit: what waited is given up, not just
        // the newcomer, since the stream is of no use with a hole in it.
        assert_eq!(inbox.queue_frame(vec![0u8; 1], true), Queued::Overflow);
        assert!(inbox.frames.is_empty());
        // The rest of that stream is refused, shown or hidden.
        assert_eq!(inbox.queue_frame(vec![0u8; 16], true), Queued::Discarded);
        assert_eq!(inbox.queue_frame(vec![0u8; 16], false), Queued::Discarded);
        assert!(inbox.take_frames().is_empty());
        // A new socket's stream is taken from its first message on.
        assert_eq!(inbox.queue_frame(hello_message(), false), Queued::Kept);
        assert_eq!(inbox.queue_frame(vec![0u8; 16], false), Queued::Kept);
        assert_eq!(inbox.take_frames().len(), 2);
    }

    #[test]
    fn the_backlog_is_counted_from_the_last_time_the_app_took_it() {
        // Hidden pages are drawn now and then (a tab being switched to);
        // what was taken no longer counts against the limit.
        let mut inbox = Inbox::default();
        let message = vec![0u8; HIDDEN_BACKLOG_MAX / 2];
        for _ in 0..5 {
            assert_eq!(inbox.queue_frame(message.clone(), true), Queued::Kept);
            assert_eq!(inbox.queue_frame(message.clone(), true), Queued::Kept);
            assert_eq!(inbox.take_frames().len(), 2);
        }
    }

    #[test]
    fn a_reply_to_a_request_that_was_replaced_is_stale() {
        let slot = Latest::default();
        // Two reports asked for in a row, the second before the first
        // answered. Whatever order the answers come in, only the second's
        // is wanted.
        let first = slot.next();
        let held_by_first = slot.clone();
        let second = slot.next();
        assert!(!held_by_first.is(first), "the replaced request still counts as the latest");
        assert!(slot.is(second));
        // One slot does not make another's requests stale.
        let other = Latest::default();
        let theirs = other.next();
        slot.next();
        assert!(other.is(theirs));
        assert!(!slot.is(second));
    }

    #[test]
    fn a_share_stopped_before_its_request_exists_is_never_sent() {
        // Between the click and the first byte the session is checked
        // again; a Clear during that wait must mean no upload at all.
        let mut upload: ShareUpload<&str> = ShareUpload::default();
        assert_eq!(upload.begin(4), None);
        assert_eq!(upload.stop(), None, "nothing to abort yet");
        assert!(!upload.sending(4, "xhr"), "a stopped upload was sent");
    }

    #[test]
    fn a_share_being_sent_is_handed_back_to_be_aborted_once() {
        let mut upload: ShareUpload<&str> = ShareUpload::default();
        upload.begin(4);
        assert!(upload.sending(4, "xhr"));
        assert_eq!(upload.stop(), Some("xhr"));
        assert_eq!(upload.stop(), None);
    }

    #[test]
    fn a_share_call_of_another_generation_stops_the_upload_and_its_own_does_not() {
        let mut upload: ShareUpload<&str> = ShareUpload::default();
        upload.begin(4);
        assert!(upload.sending(4, "xhr"));
        // The session poll of the dialog that is uploading.
        assert_eq!(upload.stop_unless(4), None);
        // The capture was replaced and Share opened again.
        assert_eq!(upload.stop_unless(6), Some("xhr"));
        assert_eq!(upload.stop_unless(6), None);
        // An upload replaced while its session check was still out.
        upload.begin(6);
        assert_eq!(upload.begin(7), None);
        assert!(!upload.sending(6, "old"));
        assert!(upload.sending(7, "new"));
    }

    #[test]
    fn an_upload_that_ended_leaves_nothing_to_abort_and_keeps_a_newer_one() {
        let mut upload: ShareUpload<&str> = ShareUpload::default();
        upload.begin(4);
        assert!(upload.sending(4, "xhr"));
        upload.finish(4);
        assert_eq!(upload.stop(), None);
        // A late end of generation 4 does not clear generation 5.
        upload.begin(5);
        assert!(upload.sending(5, "next"));
        upload.finish(4);
        assert_eq!(upload.stop(), Some("next"));
    }

    fn capture_start() -> CaptureStart {
        CaptureStart {
            pid: 4242,
            enable_api: true,
            context_switches: true,
            thread_states: false,
            sampling: true,
            samples_per_second: 1000.0,
            unwinding: "dwarf".into(),
            dynamic_instrumentation_method: "uprobes".into(),
            instrumented_function_ids: vec![7, 281474976710655],
            show_all_processes: false,
            uprobe_duplicate_filter: true,
            auto_profile: false,
        }
    }

    #[test]
    fn the_capture_start_body_is_the_json_the_service_reads() {
        let body = capture_start_body(&capture_start());
        // Byte for byte: the service's parser is serde, which would take
        // other spellings, but the body is also what the log line shows.
        assert_eq!(
            body,
            r#"{"pid":4242,"enable_api":true,"context_switches":true,"thread_states":false,"sampling":true,"samples_per_second":1000,"unwinding":"dwarf","dynamic_instrumentation_method":"uprobes","instrumented_functions":[{"function_id":7},{"function_id":281474976710655}],"show_all_processes":false,"uprobe_duplicate_filter":true,"auto_profile":false}"#
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["instrumented_functions"][1]["function_id"], 281474976710655u64);
        // No hooks: an empty array, not a missing key.
        let none = CaptureStart { instrumented_function_ids: Vec::new(), ..capture_start() };
        let v: serde_json::Value = serde_json::from_str(&capture_start_body(&none)).unwrap();
        assert_eq!(v["instrumented_functions"], serde_json::json!([]));
    }

    #[test]
    fn a_quote_or_backslash_in_a_request_string_stays_inside_its_string() {
        let odd = CaptureStart { unwinding: r#"a"b\c"#.into(), ..capture_start() };
        let v: serde_json::Value = serde_json::from_str(&capture_start_body(&odd)).unwrap();
        assert_eq!(v["unwinding"], r#"a"b\c"#);
        assert_eq!(v["dynamic_instrumentation_method"], "uprobes");
        let v: serde_json::Value = serde_json::from_str(&config_body(1 << 26, r#"C:\spill\"x""#)).unwrap();
        assert_eq!(v["spill_path"], r#"C:\spill\"x""#);
        assert_eq!(v["ring_buffer_bytes"], 1u64 << 26);
    }

    #[test]
    fn an_empty_spill_path_is_sent_as_null() {
        assert_eq!(config_body(1024, ""), r#"{"ring_buffer_bytes":1024,"spill_path":null}"#);
        assert_eq!(config_body(1024, "/tmp/spill"), r#"{"ring_buffer_bytes":1024,"spill_path":"/tmp/spill"}"#);
    }

    #[test]
    fn the_tree_query_puts_the_mode_after_the_ranges_or_alone() {
        assert_eq!(sampling_tree_query(&[], "top_down"), "?mode=top_down");
        assert_eq!(sampling_tree_query(&[(1, 2, Some(3))], "bottom_up"), "?ranges=1-2:3&mode=bottom_up");
    }

    #[test]
    fn status_json_parses_demo_live_ring() {
        let s = parse_status_json(
            r#"{"capturing":false,"demo":true,"events_live":2000000,"events_capacity":2097152,"dropped":0,"spilled":0,"produced":2000000,"oldest_start_ns":1,"newest_end_ns":4000000000,"ring_bytes":67108864,"spill_path":"/tmp/perfr-spill"}"#,
        )
        .unwrap();
        assert!(s.demo);
        assert_eq!(s.events_live, 2_000_000);
        assert_eq!(s.ring_bytes, 67_108_864);
        assert_eq!(s.newest_end_ns, 4_000_000_000);
        assert_eq!(s.machine, "local");
    }

    #[test]
    fn frame_body_is_header_plus_rgba() {
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let f = parse_frame_body(&body).unwrap();
        assert_eq!(f.width, 2);
        assert_eq!(f.lanes, 1);
        assert_eq!(f.rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        let (scaled, h) = scale_frame_rgba(&f, 3);
        assert_eq!(h, 3);
        assert_eq!(scaled.len(), 2 * 3 * 4);
        assert_eq!(&scaled[0..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn timeline_instances_parse_css_color() {
        let tl = parse_timeline_json(
            r##"{"lod":"instanced","width":200,"height":16,"lane_count":1,"instance_count":1,"instances":[{"x":0,"y":0,"w":200,"h":16,"color":"#E74435","r":3}]}"##,
        )
        .unwrap();
        assert_eq!(tl.lod, "instanced");
        let inst = instances_from_timeline(&tl);
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].color, 0xFFE7_4435);
        assert!((inst[0].w - 200.0).abs() < f32::EPSILON);
    }

    #[test]
    fn css_to_argb_accepts_rrggbb() {
        assert_eq!(css_to_argb("#64B5F6"), 0xFF64_B5F6);
    }

    #[test]
    fn process_json_keeps_cpu_and_path() {
        let list = parse_processes_json(
            r#"[{"pid":9,"name":"app","cpu":1.5,"path":"/usr/bin/app"}]"#,
        )
        .unwrap();
        assert_eq!(list[0].pid, 9);
        assert_eq!(list[0].path, "/usr/bin/app");
        assert!((list[0].cpu - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn symbols_and_search_json_are_paged() {
        let st = parse_symbols_status_json(
            r#"{"pid":3,"status":"ready","function_count":12,"module_count":2,"error":""}"#,
        )
        .unwrap();
        assert_eq!(st.status, "ready");
        assert_eq!(st.function_count, 12);
        let hits = parse_function_search_json(
            r#"{"pid":3,"status":"ready","functions":[{"function_id":1,"name":"foo::Bar","module":"/bin/app","size":16}]}"#,
        )
        .unwrap();
        assert_eq!(hits.functions.len(), 1);
        assert_eq!(hits.functions[0].name, "foo::Bar");
    }
}


