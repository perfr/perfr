// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Browser fetch + WebSocket. Native tests get a no-op stub plus parsers.

// The parsers and JSON mirrors here serve the wasm client; the native build
// only compiles the stub, so nothing reads them there.
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

#[allow(dead_code)] // used from the wasm Net impl
pub fn parse_status_json(text: &str) -> Result<StatusJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/status: {e}"))
}

#[allow(dead_code)] // used from the wasm Net impl
pub fn parse_processes_json(text: &str) -> Result<Vec<ProcessJson>, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/processes: {e}"))
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub fn parse_function_search_json(text: &str) -> Result<FunctionSearchJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/functions/search: {e}"))
}

#[allow(dead_code)] // used from the wasm Net impl
pub fn parse_timeline_json(text: &str) -> Result<TimelineJson, String> {
    serde_json::from_str(text).map_err(|e| format!("/api/timeline: {e}"))
}

/// `/api/frame` body: 16-byte header + `width * lanes * 4` RGBA.
#[allow(dead_code)] // used from the wasm Net impl
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

#[cfg(target_arch = "wasm32")]
mod wasm_impl {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{BinaryType, MessageEvent, RequestInit, Response, WebSocket};

    #[derive(Clone)]
    pub struct Net {
        inbox: Arc<Mutex<Inbox>>,
        http_busy: Arc<AtomicBool>,
        view_busy: Arc<AtomicBool>,
        self_busy: Arc<AtomicBool>,
        /// Held so the JS WebSocket is not GC'd.
        #[allow(dead_code)]
        ws: Arc<Mutex<Option<WebSocket>>>,
        /// A capture file was opened instead of a service: no socket, and
        /// every request that would go to the service is dropped.
        offline: bool,
        /// The Share upload in flight, kept so it can be aborted.
        upload: Arc<Mutex<ShareUpload<web_sys::XmlHttpRequest>>>,
        /// The latest request for each inbox slot whose replies do not name
        /// their request: both report forms share one, both tree forms too.
        report_seq: Latest,
        tree_seq: Latest,
        disassembly_seq: Latest,
        source_seq: Latest,
    }

    impl Net {
        pub fn connect() -> Self {
            // A service is there: its log file takes this page's lines.
            crate::logging::set_relay(true);
            let inbox = Arc::new(Mutex::new(Inbox::default()));
            let ws = Arc::new(Mutex::new(None));
            start_ws(inbox.clone(), ws.clone());
            install_login_listener(inbox.clone());
            Self {
                inbox,
                http_busy: Arc::new(AtomicBool::new(false)),
                view_busy: Arc::new(AtomicBool::new(false)),
                self_busy: Arc::new(AtomicBool::new(false)),
                ws,
                offline: false,
                upload: Arc::default(),
                report_seq: Latest::default(),
                tree_seq: Latest::default(),
                disassembly_seq: Latest::default(),
                source_seq: Latest::default(),
            }
        }

        /// Fetches a capture stream file (`/api/capture/export?format=stream`
        /// saved to disk) and feeds it in as if a service had sent it. The
        /// static web page's mode: no service, no socket.
        pub fn from_capture_url(url: &str) -> Self {
            let inbox = Arc::new(Mutex::new(Inbox::default()));
            let url = url.to_string();
            let fetch_into = inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_bytes(&url).await;
                let mut g = fetch_into.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(bytes) => {
                        g.bytes_in += bytes.len() as u64;
                        g.loaded_stream = Some(bytes);
                        g.ws_ok = true;
                        g.http_ok = true;
                    }
                    Err(e) => g.error = Some(format!("capture file: {e}")),
                }
            });
            install_login_listener(inbox.clone());
            Self {
                inbox,
                http_busy: Arc::new(AtomicBool::new(false)),
                view_busy: Arc::new(AtomicBool::new(false)),
                self_busy: Arc::new(AtomicBool::new(false)),
                ws: Arc::new(Mutex::new(None)),
                offline: true,
                upload: Arc::default(),
                report_seq: Latest::default(),
                tree_seq: Latest::default(),
                disassembly_seq: Latest::default(),
                source_seq: Latest::default(),
            }
        }

        /// Fetch a story document. The bytes land in `loaded_story`.
        pub fn fetch_story(&self, url: &str) {
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

        /// Fetch a `.perfr.stream` into the same inbox a `?capture=` load uses.
        pub fn fetch_capture(&self, url: &str) {
            let url = url.to_string();
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_bytes(&url).await;
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(bytes) => {
                        g.bytes_in += bytes.len() as u64;
                        g.loaded_stream = Some(bytes);
                        g.http_ok = true;
                    }
                    Err(e) => g.error = Some(format!("capture file: {e}")),
                }
            });
        }

        /// Reads a `.perfr.stream` from the service so the dialog can show
        /// its size. Does not post it.
        pub fn stored_share_auth() -> (Option<String>, bool) {
            let Some(window) = web_sys::window() else {
                return (None, false);
            };
            let token = window
                .location()
                .hash()
                .ok()
                .as_deref()
                .and_then(crate::share::fragment_share_token)
                .or_else(|| take_stored_share_token(&window));
            let resume = window
                .session_storage()
                .ok()
                .flatten()
                .and_then(|s| s.get_item(crate::share::LOGIN_RESUME_KEY).ok().flatten())
                .is_some_and(|v| v == "1");
            (token, resume)
        }

        pub fn clear_share_resume_flag(&self) {
            let Some(window) = web_sys::window() else {
                return;
            };
            if let Ok(Some(storage)) = window.session_storage() {
                let _ = storage.remove_item(crate::share::LOGIN_RESUME_KEY);
            }
        }

        /// `GET /api/me` with the session cookie and, when we have one, the bearer.
        pub fn check_share_session(&self, gen: u64, url: String, token: Option<String>) {
            self.abort_share_unless(gen);
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let check = fetch_session(&url, token.as_deref()).await;
                push_share(&inbox, gen, crate::share::ShareNoteKind::Session(check));
            });
        }

        /// Open the login URL in a popup. Returns false when the browser
        /// blocked it and this tab is navigating there instead.
        pub fn open_share_login(&self, url: &str) -> bool {
            let Some(window) = web_sys::window() else {
                return false;
            };
            match window.open_with_url_and_target_and_features(
                url,
                crate::share::LOGIN_WINDOW_NAME,
                "popup=yes,width=520,height=720",
            ) {
                Ok(Some(_)) => true,
                _ => {
                    if let Ok(Some(storage)) = window.session_storage() {
                        let _ = storage.set_item(crate::share::LOGIN_RESUME_KEY, "1");
                    }
                    let _ = window.location().set_href(url);
                    false
                }
            }
        }

        /// Reads a `.perfr.stream` from the service so the dialog can show
        /// its size. Does not post it.
        pub fn prepare_share(&self, gen: u64, path: String) {
            self.abort_share_unless(gen);
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                push_share(&inbox, gen, crate::share::ShareNoteKind::Reading);
                match get_bytes(&path).await {
                    Ok(bytes) if !bytes.is_empty() => {
                        push_share(&inbox, gen, crate::share::ShareNoteKind::Prepared(bytes));
                    }
                    Ok(_) => push_share(
                        &inbox,
                        gen,
                        crate::share::ShareNoteKind::Failed(crate::share::ShareFailure::message(
                            "Nothing to share.",
                        )),
                    ),
                    Err(e) => push_share(
                        &inbox,
                        gen,
                        crate::share::ShareNoteKind::Failed(crate::share::ShareFailure::message(
                            format!("Could not read the capture. {e}"),
                        )),
                    ),
                }
            });
        }

        /// Posts `.perfr.stream` bytes to `endpoint` after one more `GET /api/me`.
        /// The dialog has already shown the size and the account.
        pub fn share_capture(
            &self,
            gen: u64,
            endpoint: String,
            me_url: String,
            token: Option<String>,
            bytes: Vec<u8>,
        ) {
            let stale = self.upload.lock().unwrap_or_else(|e| e.into_inner()).begin(gen);
            abort_upload(stale);
            let inbox = self.inbox.clone();
            let upload = self.upload.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if bytes.is_empty() {
                    push_share(
                        &inbox,
                        gen,
                        crate::share::ShareNoteKind::Failed(crate::share::ShareFailure::message(
                            "Nothing to share.",
                        )),
                    );
                    return;
                }
                let session = fetch_session(&me_url, token.as_deref()).await;
                push_share(
                    &inbox,
                    gen,
                    crate::share::ShareNoteKind::Session(session.clone()),
                );
                match session {
                    crate::share::SessionCheck::SignedIn(account) => {
                        if let Some(message) = crate::share::fit_block(
                            bytes.len() as u64,
                            &account.quota,
                            "This capture",
                        ) {
                            push_share(
                                &inbox,
                                gen,
                                crate::share::ShareNoteKind::Failed(
                                    crate::share::ShareFailure::message(message),
                                ),
                            );
                            return;
                        }
                    }
                    crate::share::SessionCheck::LoginRequired { .. } => {
                        push_share(
                            &inbox,
                            gen,
                            crate::share::ShareNoteKind::Failed(crate::share::ShareFailure::login(
                                "Your session expired. Sign in again to share.",
                            )),
                        );
                        return;
                    }
                    crate::share::SessionCheck::Unavailable { message } => {
                        push_share(
                            &inbox,
                            gen,
                            crate::share::ShareNoteKind::Failed(
                                crate::share::ShareFailure::message(message),
                            ),
                        );
                        return;
                    }
                }
                push_share(
                    &inbox,
                    gen,
                    crate::share::ShareNoteKind::Progress {
                        loaded: 0,
                        total: bytes.len() as u64,
                    },
                );
                let result = post_share_xhr(&endpoint, &bytes, token.as_deref(), &inbox, gen, &upload).await;
                upload.lock().unwrap_or_else(|e| e.into_inner()).finish(gen);
                let kind = match result {
                    Posted::Reply(text) => match crate::share::parse_share_reply(&text) {
                        Ok(link) => crate::share::ShareNoteKind::Done(link),
                        Err(e) => crate::share::ShareNoteKind::Failed(
                            crate::share::ShareFailure::message(e),
                        ),
                    },
                    Posted::Failed(status, body) => crate::share::ShareNoteKind::Failed(
                        crate::share::share_failure(status, &body),
                    ),
                    // Whoever stopped it has usually moved to another
                    // share generation and never sees this note. When they
                    // have not, it is what takes the dialog out of
                    // "Uploading".
                    Posted::Stopped => crate::share::ShareNoteKind::Failed(
                        crate::share::ShareFailure::message("The upload was stopped."),
                    ),
                };
                push_share(&inbox, gen, kind);
            });
        }

        /// Stops the Share upload in flight, if any: the request is aborted,
        /// or never sent when it had not started. For when the capture it
        /// was reading is replaced or the user thinks better of it; without
        /// this the bytes went to the share server to the end whatever
        /// happened in the page, and the capture stayed there under a link
        /// nobody was shown.
        pub fn abort_share(&self) {
            let handle = self.upload.lock().unwrap_or_else(|e| e.into_inner()).stop();
            abort_upload(handle);
        }

        fn abort_share_unless(&self, gen: u64) {
            let handle = self.upload.lock().unwrap_or_else(|e| e.into_inner()).stop_unless(gen);
            abort_upload(handle);
        }

        /// Reads a `.perfr.stream` the user opened and hands the bytes to the app.
        pub fn import_stream_file(&self, file: web_sys::File) {
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = async {
                    let buf = JsFuture::from(file.array_buffer()).await.map_err(js_err)?;
                    let arr = js_sys::Uint8Array::new(&buf);
                    let mut bytes = vec![0u8; arr.length() as usize];
                    arr.copy_to(&mut bytes);
                    Ok::<_, String>(bytes)
                }
                .await;
                let mut g = inbox.lock().unwrap_or_else(|p| p.into_inner());
                match result {
                    Ok(bytes) => g.loaded_stream = Some(bytes),
                    Err(e) => g.error = Some(format!("open capture: {e}")),
                }
            });
        }

        pub fn take(&self) -> Inbox {
            let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            Inbox {
                share_notes: std::mem::take(&mut inbox.share_notes),
                share_login_signalled: std::mem::take(&mut inbox.share_login_signalled),
                share_token: inbox.share_token.take(),
                loaded_stream: inbox.loaded_stream.take(),
                loaded_story: inbox.loaded_story.take(),
                status: inbox.status.take(),
                settings: inbox.settings.take(),
                bench: inbox.bench.take(),
                processes: inbox.processes.take(),
                sampling: inbox.sampling.take(),
                error: inbox.error.take(),
                frames: inbox.take_frames(),
                frames_bytes: 0,
                frames_dropped: false,
                timeline: inbox.timeline.take(),
                frame: inbox.frame.take(),
                http_ok: inbox.http_ok,
                ws_ok: inbox.ws_ok,
                bytes_in: inbox.bytes_in,
                symbols: inbox.symbols.take(),
                tree: inbox.tree.take(),
                modules: inbox.modules.take(),
                function_hits: inbox.function_hits.take(),
                function_list: inbox.function_list.take(),
                preset_functions: std::mem::take(&mut inbox.preset_functions),
                disassembly: inbox.disassembly.take(),
                source: inbox.source.take(),
            }
        }

        /// Closes the event socket. Its `onclose` empties the slot, after
        /// which `reconnect_ws_if_closed` opens a new one; the slot is left
        /// alone here so a socket still closing is not replaced and then
        /// forgotten by its own late `onclose`. A closing socket delivers no
        /// further messages.
        pub fn close_ws(&self) {
            if let Ok(slot) = self.ws.lock() {
                if let Some(ws) = slot.as_ref() {
                    let _ = ws.close();
                }
            }
        }

        /// Opens a new WebSocket if the last one closed -- a service that was
        /// restarted comes back without a page reload. Cheap when connected.
        pub fn reconnect_ws_if_closed(&self) {
            if self.offline {
                return;
            }
            let closed = self.ws.lock().map(|w| w.is_none()).unwrap_or(true);
            if closed {
                start_ws(self.inbox.clone(), self.ws.clone());
            }
        }

        /// The persisted user settings; the reply lands in `Inbox::settings`.
        pub fn get_settings(&self) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text("/api/settings")
                    .await
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| format!("/api/settings: {e}")));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(v) => g.settings = Some(v),
                    Err(e) => g.error = Some(e),
                }
            });
        }

        /// Replaces the persisted user settings with `settings` (the whole
        /// object); the service saves them and echoes what it kept.
        pub fn put_settings(&self, settings: &serde_json::Value) {
            if self.offline {
                return;
            }
            self.send("PUT", "/api/settings", settings.to_string());
        }

        /// Auto-profiling on or off for the running capture (and the next).
        pub fn set_auto_profile(&self, on: bool) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/auto_profile", format!(r#"{{"on":{on}}}"#));
        }

        pub fn get_status(&self) {
            if self.offline {
                return;
            }
            if self
                .http_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return;
            }
            let inbox = self.inbox.clone();
            let busy = self.http_busy.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text("/api/status")
                    .await
                    .and_then(|t| parse_status_json(&t));
                {
                    let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                    match result {
                        Ok(s) => {
                            g.status = Some(s);
                            g.http_ok = true;
                            g.error = None;
                        }
                        // A failed poll is the one way a dead service shows
                        // over HTTP; without this the flag stayed true forever.
                        Err(e) => {
                            g.http_ok = false;
                            g.error = Some(e);
                        }
                    }
                }
                busy.store(false, Ordering::SeqCst);
            });
        }

        pub fn get_processes(&self) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text("/api/processes")
                    .await
                    .and_then(|t| parse_processes_json(&t));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(p) => g.processes = Some(p),
                    Err(e) => g.error = Some(e),
                }
            });
        }

        pub fn pull_view(&self, t0: u64, t1: u64, width: u32) {
            if self.offline {
                return;
            }
            if self
                .view_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return;
            }
            let width = width.clamp(16, 4096);
            let t1 = t1.max(t0 + 1);
            let inbox = self.inbox.clone();
            let busy = self.view_busy.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let qs = format!("t0={t0}&t1={t1}&width={width}");
                let result = pull_timeline_or_frame(&qs).await;
                {
                    let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                    match result {
                        Ok(ViewPull::Timeline(tl)) => g.timeline = Some(tl),
                        Ok(ViewPull::Frame(fr)) => g.frame = Some(fr),
                        Err(e) => g.error = Some(e),
                    }
                }
                busy.store(false, Ordering::SeqCst);
            });
        }

        pub fn start_capture(&self, req: &CaptureStart) {
            // A new capture replaces the one being shared; the app has
            // dropped that share and would throw the link away.
            self.abort_share();
            if self.offline {
                return;
            }
            self.send("POST", "/api/capture/start", capture_start_body(req));
        }

        pub fn load_symbols(&self, pid: u32) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/symbols/load", format!(r#"{{"pid":{pid}}}"#));
        }

        pub fn get_symbols_status(&self, pid: u32) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/symbols/status?pid={pid}"))
                    .await
                    .and_then(|t| parse_symbols_status_json(&t));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    // Older services return pid 0 for idle. Keep the request
                    // identity so a late response cannot reset a new selection.
                    Ok(mut s) => {
                        if s.pid == 0 { s.pid = pid; }
                        g.symbols = Some(s);
                    }
                    Err(e) => g.error = Some(e),
                }
            });
        }

        /// Fetches the sampling report for a selection: the union of the given
        /// `(start_ns, end_ns, tid)` windows. An empty slice means the whole
        /// capture, which is what the panel shows before anything is selected.
        pub fn get_sampling_report(&self, ranges: &[(u64, u64, Option<u32>)]) {
            if self.offline {
                return;
            }
            let query = ranges_query(ranges);
            let inbox = self.inbox.clone();
            let (latest, seq) = (self.report_seq.clone(), self.report_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/sampling/report{query}"))
                    .await
                    .and_then(|t| parse_sampling_report_json(&t));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(r) => g.sampling = Some(r),
                    // A service without sampling answers 501; that is not an
                    // error worth showing the user on every selection.
                    Err(_) => g.sampling = None,
                }
            });
        }

        /// The report over every sample inside any instance of the scope.
        pub fn get_sampling_report_scope(&self, name_id: u32) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            let (latest, seq) = (self.report_seq.clone(), self.report_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/sampling/report?scope={name_id}"))
                    .await
                    .and_then(|t| parse_sampling_report_json(&t));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.sampling = result.ok();
            });
        }

        pub fn get_sampling_tree_scope(&self, name_id: u32, mode: &str) {
            if self.offline {
                return;
            }
            let query = format!("/api/sampling/tree?scope={name_id}&mode={mode}");
            let inbox = self.inbox.clone();
            let (latest, seq) = (self.tree_seq.clone(), self.tree_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&query).await.and_then(|t| parse_sampling_tree_json(&t));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.tree = result.ok();
            });
        }

        /// The same samples as a call tree over the union of `ranges`. An empty
        /// slice means the whole capture, which is what the panel asks for when
        /// a capture stops and nothing is selected.
        pub fn get_sampling_tree(&self, ranges: &[(u64, u64, Option<u32>)], mode: &str) {
            if self.offline {
                return;
            }
            let query = sampling_tree_query(ranges, mode);
            let inbox = self.inbox.clone();
            let (latest, seq) = (self.tree_seq.clone(), self.tree_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/sampling/tree{query}"))
                    .await
                    .and_then(|t| parse_sampling_tree_json(&t));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.tree = result.ok();
            });
        }

        /// A function of `pid` disassembled, with its source lines.
        pub fn get_disassembly(&self, pid: u32, function_id: u64) {
            self.fetch_disassembly(format!("/api/code/disassembly?pid={pid}&function_id={function_id}"));
        }

        /// The service's example: one of its own functions.
        pub fn get_example_disassembly(&self) {
            self.fetch_disassembly("/api/code/example".to_string());
        }

        fn fetch_disassembly(&self, url: String) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            let (latest, seq) = (self.disassembly_seq.clone(), self.disassembly_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&url)
                    .await
                    .and_then(|t| serde_json::from_str::<crate::code::Disassembly>(&t).map_err(|e| format!("disassembly: {e}")));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.disassembly = Some(result);
            });
        }

        /// A source file the service may serve (one a disassembly named).
        pub fn get_source(&self, path: &str) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            let url = format!("/api/code/source?path={}", percent_encode(path));
            let (latest, seq) = (self.source_seq.clone(), self.source_seq.next());
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&url)
                    .await
                    .and_then(|t| serde_json::from_str::<crate::code::SourceFile>(&t).map_err(|e| format!("source: {e}")));
                if !latest.is(seq) {
                    return;
                }
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.source = Some(result);
            });
        }

        pub fn get_modules(&self, pid: u32) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/symbols/modules?pid={pid}"))
                    .await
                    .and_then(|t| parse_modules_json(&t));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                g.modules = result.ok();
            });
        }

        pub fn search_functions(&self, pid: u32, q: &str, limit: u32) {
            if self.offline {
                return;
            }
            let q = crate::util::percent_encode(q);
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!(
                    "/api/functions/search?pid={pid}&q={q}&limit={limit}"
                ))
                .await
                .and_then(|t| parse_function_search_json(&t));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(s) => g.function_hits = Some(s),
                    Err(e) => g.error = Some(e),
                }
            });
        }

        /// Resolve portable identities against the complete symbol index.
        pub fn resolve_preset_functions(&self, pid: u32, generation: u64, keys: String) {
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let body = format!(r#"{{"pid":{pid},"functions":{keys}}}"#);
                let result = send_text("POST", "/api/functions/resolve", &body).await
                    .and_then(|text| parse_function_search_json(&text));
                inbox.lock().unwrap_or_else(|e| e.into_inner()).preset_functions.push((generation, result));
            });
        }

        /// Every function the service indexed for `pid`, for the Functions
        /// view. One request; the view filters and pages on its own.
        pub fn list_functions(&self, pid: u32) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text(&format!("/api/functions/search?pid={pid}&q=&limit=200000"))
                    .await
                    .and_then(|t| parse_function_search_json(&t));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(s) => g.function_list = Some(s),
                    Err(e) => g.error = Some(e),
                }
            });
        }

        pub fn stop_capture(&self) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/capture/stop", "{}".into());
        }

        /// The benchmark producer's state, for the Benchmark window.
        pub fn get_bench(&self) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = get_text("/api/bench")
                    .await
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| format!("/api/bench: {e}")));
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(v) => g.bench = Some(v),
                    Err(e) => g.error = Some(e),
                }
            });
        }

        pub fn start_bench(&self, threads: u32, depth_min: u32, depth_max: u32, rate: u64) {
            if self.offline {
                return;
            }
            self.send(
                "POST",
                "/api/bench/start",
                format!(r#"{{"threads":{threads},"depth_min":{depth_min},"depth_max":{depth_max},"rate":{rate}}}"#),
            );
        }

        /// The live knob: a new rate for a running benchmark.
        pub fn set_bench_rate(&self, rate: u64) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/bench/rate", format!(r#"{{"rate":{rate}}}"#));
        }

        pub fn stop_bench(&self) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/bench/stop", "{}".into());
        }

        pub fn start_demo(&self) {
            self.abort_share();
            if self.offline {
                return;
            }
            self.send(
                "POST",
                "/api/demo/start",
                r#"{"scopes_per_sec":50000}"#.into(),
            );
        }

        pub fn stop_demo(&self) {
            if self.offline {
                return;
            }
            self.send("POST", "/api/demo/stop", "{}".into());
        }

        /// Empties the capture on the service; the ring's reset comes back
        /// over the WebSocket.
        pub fn clear_capture(&self) {
            self.abort_share();
            if self.offline {
                return;
            }
            self.send("POST", "/api/capture/clear", "{}".into());
        }

        /// Posts a `.perfr.zip` to the service, which opens it as the current
        /// capture and streams it back over the WebSocket.
        pub fn import_capture(&self, bytes: Vec<u8>) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = send_bytes("/api/capture/import", &bytes, "application/zip").await {
                    inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("open capture: {e}"));
                }
            });
        }

        /// As [`import_capture`](Self::import_capture), reading the browser
        /// `File` first.
        pub fn import_capture_file(&self, file: web_sys::File) {
            if self.offline {
                return;
            }
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = async {
                    let buf = JsFuture::from(file.array_buffer()).await.map_err(js_err)?;
                    let arr = js_sys::Uint8Array::new(&buf);
                    let mut bytes = vec![0u8; arr.length() as usize];
                    arr.copy_to(&mut bytes);
                    send_bytes("/api/capture/import", &bytes, "application/zip").await
                }
                .await;
                if let Err(e) = result {
                    inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("open capture: {e}"));
                }
            });
        }

        pub fn apply_config(&self, ring_bytes: u64, spill: &str) {
            if self.offline {
                return;
            }
            self.send("PUT", "/api/config", config_body(ring_bytes, spill));
        }

        fn send(&self, method: &'static str, path: &'static str, body: String) {
            // Every command the page gives the service, in the log: these
            // are clicks (Record, Stop, Demo, config), not per-frame pulls.
            log::info!(target: "perfr_live_viewer::net", "{method} {path} {}", body.chars().take(200).collect::<String>());
            let inbox = self.inbox.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = send_text(method, path, &body).await {
                    log::warn!(target: "perfr_live_viewer::net", "{method} {e}");
                    inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(e);
                }
            });
        }
    }

    enum ViewPull {
        Timeline(TimelineJson),
        Frame(ServiceFrame),
    }

    async fn pull_timeline_or_frame(qs: &str) -> Result<ViewPull, String> {
        let tl_text = get_text(&format!("/api/timeline?{qs}")).await?;
        let tl = parse_timeline_json(&tl_text)?;
        if tl.lod == "instanced" && !tl.instances.is_empty() {
            return Ok(ViewPull::Timeline(tl));
        }
        let bytes = get_bytes(&format!("/api/frame?{qs}")).await?;
        Ok(ViewPull::Frame(parse_frame_body(&bytes)?))
    }

    /// GET with `fetch_with_str` — no `RequestInit` (that path hung/panicked).
    async fn get_text(url: &str) -> Result<String, String> {
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

    async fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
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

    /// POSTs raw bytes with the given content type; the response text on
    /// success, the status or error otherwise.
    async fn send_bytes(url: &str, bytes: &[u8], content_type: &str) -> Result<String, String> {
        let opts = RequestInit::new();
        opts.set_method("POST");
        let body = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        body.copy_from(bytes);
        opts.set_body(&body.into());
        let headers = js_sys::Object::new();
        js_sys::Reflect::set(
            &headers,
            &JsValue::from_str("content-type"),
            &JsValue::from_str(content_type),
        )
        .map_err(js_err)?;
        opts.set_headers(&headers);
        let window = web_sys::window().ok_or("no window")?;
        let resp = JsFuture::from(window.fetch_with_str_and_init(url, &opts))
            .await
            .map_err(js_err)?;
        let resp: Response = resp
            .dyn_into()
            .map_err(|_| "fetch: not a Response".to_string())?;
        let status = resp.status();
        let text = JsFuture::from(resp.text().map_err(js_err)?)
            .await
            .map_err(js_err)?
            .as_string()
            .unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(format!("{status}: {text}"));
        }
        Ok(text)
    }

    fn push_share(inbox: &Arc<Mutex<Inbox>>, gen: u64, kind: crate::share::ShareNoteKind) {
        inbox.lock().unwrap_or_else(|e| e.into_inner()).share_notes.push(crate::share::ShareNote { gen, kind });
    }

    /// POST the stream with an XMLHttpRequest so the upload reports progress.
    /// `fetch` does not. The status is 0 when the browser could not reach the host.
    fn share_request_credentials() -> web_sys::RequestCredentials {
        if crate::share::SHARE_CREDENTIALS == "include" {
            web_sys::RequestCredentials::Include
        } else {
            web_sys::RequestCredentials::SameOrigin
        }
    }

    async fn fetch_session(url: &str, token: Option<&str>) -> crate::share::SessionCheck {
        let opts = RequestInit::new();
        opts.set_method("GET");
        opts.set_credentials(share_request_credentials());
        if let Some(token) = token.and_then(crate::share::usable_share_token) {
            let headers = js_sys::Object::new();
            if js_sys::Reflect::set(
                &headers,
                &JsValue::from_str("authorization"),
                &JsValue::from_str(&crate::share::bearer_value(&token)),
            )
            .is_err()
            {
                return crate::share::SessionCheck::Unavailable {
                    message: "Could not reach the share server.".into(),
                };
            }
            opts.set_headers(&headers);
        }
        let Some(window) = web_sys::window() else {
            return crate::share::SessionCheck::Unavailable {
                message: "Could not reach the share server.".into(),
            };
        };
        let resp = match JsFuture::from(window.fetch_with_str_and_init(url, &opts)).await {
            Ok(resp) => resp,
            Err(_) => {
                return crate::share::parse_session_response(0, "");
            }
        };
        let Ok(resp) = resp.dyn_into::<Response>() else {
            return crate::share::parse_session_response(0, "");
        };
        let status = resp.status();
        let text = match resp.text() {
            Ok(promise) => JsFuture::from(promise)
                .await
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        };
        crate::share::parse_session_response(status, &text)
    }

    fn take_stored_share_token(window: &web_sys::Window) -> Option<String> {
        let mut found = None;
        for storage in [window.session_storage(), window.local_storage()] {
            let Ok(Some(storage)) = storage else { continue };
            if let Ok(Some(raw)) = storage.get_item(crate::share::SHARE_TOKEN_KEY) {
                if found.is_none() {
                    found = crate::share::usable_share_token(&raw);
                }
                let _ = storage.remove_item(crate::share::SHARE_TOKEN_KEY);
            }
        }
        found
    }

    fn install_login_listener(inbox: Arc<Mutex<Inbox>>) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let inbox_storage = inbox.clone();
        let onstorage = Closure::wrap(Box::new(move |ev: web_sys::StorageEvent| {
            let key = ev.key().unwrap_or_default();
            if key != crate::share::LOGIN_SIGNAL_KEY && key != crate::share::SHARE_TOKEN_KEY {
                return;
            }
            // The bearer is kept in memory only: whatever the popup stored
            // is taken out of storage on every event. It used to be removed
            // only when no token was waiting in the inbox, and the popup's
            // two writes (token, then signal) arrive before a frame drains
            // it, so the bearer stayed in localStorage until the next load.
            let stored = web_sys::window().and_then(|w| take_stored_share_token(&w));
            let mut g = inbox_storage.lock().unwrap_or_else(|e| e.into_inner());
            let from_event = if key == crate::share::SHARE_TOKEN_KEY {
                ev.new_value().as_deref().and_then(crate::share::usable_share_token)
            } else {
                None
            };
            if from_event.is_some() {
                g.share_token = from_event;
            } else if g.share_token.is_none() {
                g.share_token = stored;
            }
            g.share_login_signalled = true;
        }) as Box<dyn FnMut(_)>);
        let _ =
            window.add_event_listener_with_callback("storage", onstorage.as_ref().unchecked_ref());
        onstorage.forget();

        let inbox_message = inbox;
        let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| {
            let data = ev.data();
            let source = js_sys::Reflect::get(&data, &JsValue::from_str("source"))
                .ok()
                .and_then(|v| v.as_string());
            let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
                .ok()
                .and_then(|v| v.as_string());
            if source.as_deref() != Some(crate::share::LOGIN_MESSAGE_SOURCE)
                || kind.as_deref() != Some(crate::share::LOGIN_MESSAGE_TYPE)
            {
                return;
            }
            // The message is only a wake-up. A token from another origin is ignored;
            // the bearer arrives through same-origin storage.
            inbox_message
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .share_login_signalled = true;
        }) as Box<dyn FnMut(_)>);
        let _ =
            window.add_event_listener_with_callback("message", onmessage.as_ref().unchecked_ref());
        onmessage.forget();
    }

    /// How a Share upload ended.
    enum Posted {
        /// A 2xx answer and its body.
        Reply(String),
        /// The status (0 when the host was not reached) and the body.
        Failed(u16, String),
        /// Aborted through `Net::abort_share`, or never sent because of it.
        Stopped,
    }

    /// Aborts a request handed back by `ShareUpload`. Called with the
    /// upload's lock released: `abort` runs the request's handlers before it
    /// returns.
    fn abort_upload(handle: Option<web_sys::XmlHttpRequest>) {
        if let Some(xhr) = handle {
            log::info!(target: "perfr_live_viewer::net", "share upload aborted");
            let _ = xhr.abort();
        }
    }

    async fn post_share_xhr(
        url: &str,
        bytes: &[u8],
        token: Option<&str>,
        inbox: &Arc<Mutex<Inbox>>,
        gen: u64,
        upload: &Arc<Mutex<ShareUpload<web_sys::XmlHttpRequest>>>,
    ) -> Posted {
        let failed = |e: JsValue| Posted::Failed(0, js_err(e));
        let xhr = match web_sys::XmlHttpRequest::new() {
            Ok(xhr) => xhr,
            Err(e) => return failed(e),
        };
        if let Err(e) = xhr.open_with_async("POST", url, true) {
            return failed(e);
        }
        // Credentialed cross-origin upload. Same-origin cookies are sent either way.
        xhr.set_with_credentials(crate::share::SHARE_CREDENTIALS == "include");
        if let Err(e) = xhr.set_request_header("content-type", crate::share::SHARE_CONTENT_TYPE) {
            return failed(e);
        }
        if let Some(token) = token.and_then(crate::share::usable_share_token) {
            if let Err(e) = xhr.set_request_header("authorization", &crate::share::bearer_value(&token)) {
                return failed(e);
            }
        }
        let progress_inbox = inbox.clone();
        let onprogress = Closure::wrap(Box::new(move |ev: web_sys::ProgressEvent| {
            let total = if ev.length_computable() {
                ev.total() as u64
            } else {
                0
            };
            push_share(
                &progress_inbox,
                gen,
                crate::share::ShareNoteKind::Progress {
                    loaded: ev.loaded() as u64,
                    total,
                },
            );
        }) as Box<dyn FnMut(_)>);
        if let Ok(upload) = xhr.upload() {
            upload.set_onprogress(Some(onprogress.as_ref().unchecked_ref()));
        }
        onprogress.forget();

        let xhr_done = xhr.clone();
        let stopped = std::rc::Rc::new(std::cell::Cell::new(false));
        let promise = js_sys::Promise::new(&mut |resolve, _reject| {
            let resolve_ok = resolve.clone();
            let onload = Closure::wrap(Box::new(move || {
                let _ = resolve_ok.call1(&JsValue::NULL, &JsValue::UNDEFINED);
            }) as Box<dyn FnMut()>);
            xhr_done.set_onload(Some(onload.as_ref().unchecked_ref()));
            onload.forget();
            let resolve_err = resolve.clone();
            let onerror = Closure::wrap(Box::new(move || {
                let _ = resolve_err.call1(&JsValue::NULL, &JsValue::UNDEFINED);
            }) as Box<dyn FnMut()>);
            xhr_done.set_onerror(Some(onerror.as_ref().unchecked_ref()));
            onerror.forget();
            // An aborted request fires neither of the two above: without
            // this the future would wait for ever.
            let resolve_abort = resolve;
            let stopped_abort = stopped.clone();
            let onabort = Closure::wrap(Box::new(move || {
                stopped_abort.set(true);
                let _ = resolve_abort.call1(&JsValue::NULL, &JsValue::UNDEFINED);
            }) as Box<dyn FnMut()>);
            xhr_done.set_onabort(Some(onabort.as_ref().unchecked_ref()));
            onabort.forget();
        });
        // Stopped while the session was being checked: nothing leaves.
        if !upload.lock().unwrap_or_else(|e| e.into_inner()).sending(gen, xhr.clone()) {
            return Posted::Stopped;
        }
        // A copy in its own ArrayBuffer. The threaded build's wasm memory is a
        // SharedArrayBuffer, and XMLHttpRequest refuses to send a view of one.
        let copied = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
        copied.copy_from(bytes);
        if let Err(e) = xhr.send_with_opt_js_u8_array(Some(&copied)) {
            return failed(e);
        }
        let _ = JsFuture::from(promise).await;
        if stopped.get() {
            return Posted::Stopped;
        }
        let status = xhr.status().unwrap_or(0);
        let text = xhr.response_text().ok().flatten().unwrap_or_default();
        if (200..300).contains(&status) {
            Posted::Reply(text)
        } else {
            Posted::Failed(status, text)
        }
    }

    async fn send_text(method: &str, url: &str, body: &str) -> Result<String, String> {
        let opts = RequestInit::new();
        opts.set_method(method);
        opts.set_body(&JsValue::from_str(body));
        let headers = js_sys::Object::new();
        js_sys::Reflect::set(
            &headers,
            &JsValue::from_str("content-type"),
            &JsValue::from_str("application/json"),
        )
        .map_err(js_err)?;
        opts.set_headers(&headers);
        let window = web_sys::window().ok_or("no window")?;
        let resp = JsFuture::from(window.fetch_with_str_and_init(url, &opts))
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

    fn start_ws(inbox: Arc<Mutex<Inbox>>, slot: Arc<Mutex<Option<WebSocket>>>) {
        let Some(window) = web_sys::window() else {
            push_err(&inbox, "no window (WebSocket)");
            return;
        };
        let host = match window.location().host() {
            Ok(h) if !h.is_empty() => h,
            _ => {
                push_err(&inbox, "location.host empty");
                return;
            }
        };
        let proto = if window.location().protocol().ok().as_deref() == Some("https:") {
            "wss"
        } else {
            "ws"
        };
        let url = format!("{proto}://{host}/ws");
        let ws = match WebSocket::new(&url) {
            Ok(ws) => ws,
            Err(e) => {
                push_err(&inbox, &format!("WebSocket open failed: {}", js_err(e)));
                return;
            }
        };
        ws.set_binary_type(BinaryType::Arraybuffer);

        let inbox_open = inbox.clone();
        let onopen = Closure::wrap(Box::new(move |_ev: JsValue| {
            log::info!(target: "perfr_live_viewer::net", "WebSocket open");
            if let Ok(mut g) = inbox_open.lock() {
                g.ws_ok = true;
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();

        let inbox_msg = inbox.clone();
        let ws_msg = ws.clone();
        let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| match ws_bytes(&ev) {
            Ok(bytes) => {
                let queued = match inbox_msg.lock() {
                    Ok(mut g) => {
                        g.ws_ok = true;
                        g.bytes_in += bytes.len() as u64;
                        g.queue_frame(bytes, page_hidden())
                    }
                    Err(_) => Queued::Kept,
                };
                if queued == Queued::Overflow {
                    log::warn!(
                        target: "perfr_live_viewer::net",
                        "the page is hidden and {} MiB of the event stream were waiting: closing the socket; the capture is read again from the service when the page is shown",
                        HIDDEN_BACKLOG_MAX >> 20
                    );
                    // The app's retry opens a new socket once frames are
                    // drawn again, and that one starts with the whole ring.
                    let _ = ws_msg.close();
                }
            }
            Err(e) => push_err(&inbox_msg, &e),
        }) as Box<dyn FnMut(MessageEvent)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        let inbox_err = inbox.clone();
        let onerror = Closure::wrap(Box::new(move |_ev: JsValue| {
            push_err(&inbox_err, "WebSocket error");
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();

        let inbox_close = inbox;
        let slot_close = slot.clone();
        let onclose = Closure::wrap(Box::new(move |_ev: JsValue| {
            log::warn!(target: "perfr_live_viewer::net", "WebSocket closed");
            if let Ok(mut g) = inbox_close.lock() {
                g.ws_ok = false;
                g.error = Some("WebSocket closed".into());
            }
            if let Ok(mut s) = slot_close.lock() {
                *s = None;
            }
        }) as Box<dyn FnMut(JsValue)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();

        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(ws);
    }

    /// Whether the page is hidden (a background tab, a minimised window):
    /// the browser then runs no animation frames, so nothing drains the inbox.
    fn page_hidden() -> bool {
        web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.hidden())
    }

    fn ws_bytes(ev: &MessageEvent) -> Result<Vec<u8>, String> {
        let data = ev.data();
        if let Ok(buf) = data.clone().dyn_into::<js_sys::ArrayBuffer>() {
            let arr = js_sys::Uint8Array::new(&buf);
            let mut bytes = vec![0u8; arr.length() as usize];
            arr.copy_to(&mut bytes);
            return Ok(bytes);
        }
        if let Ok(arr) = data.dyn_into::<js_sys::Uint8Array>() {
            let mut bytes = vec![0u8; arr.length() as usize];
            arr.copy_to(&mut bytes);
            return Ok(bytes);
        }
        Err("WebSocket message is not binary".into())
    }

    fn push_err(inbox: &Arc<Mutex<Inbox>>, msg: &str) {
        if let Ok(mut g) = inbox.lock() {
            g.error = Some(msg.to_string());
        }
        log::error!(target: "perfr_live_viewer::net", "{msg}");
    }

    fn js_err(v: JsValue) -> String {
        if let Some(s) = v.as_string() {
            return s;
        }
        if let Ok(e) = v.clone().dyn_into::<js_sys::Error>() {
            return String::from(e.message());
        }
        format!("{v:?}")
    }
}

#[cfg(target_arch = "wasm32")]
pub use wasm_impl::Net;

#[cfg(not(target_arch = "wasm32"))]
mod native_impl {
    use super::*;

    #[derive(Default)]
    pub struct Net;

    impl Net {
        pub fn connect() -> Self {
            Self
        }
        pub fn prepare_share(&self, _gen: u64, _path: String) {}
        pub fn share_capture(
            &self,
            _gen: u64,
            _endpoint: String,
            _me_url: String,
            _token: Option<String>,
            _bytes: Vec<u8>,
        ) {
        }
        pub fn abort_share(&self) {}
        pub fn stored_share_auth() -> (Option<String>, bool) {
            (None, false)
        }
        pub fn clear_share_resume_flag(&self) {}
        pub fn check_share_session(&self, _gen: u64, _url: String, _token: Option<String>) {}
        pub fn open_share_login(&self, _url: &str) -> bool {
            false
        }
        pub fn from_capture_url(_url: &str) -> Self {
            Self
        }
        pub fn fetch_story(&self, _url: &str) {}
        pub fn fetch_capture(&self, _url: &str) {}
        pub fn take(&self) -> Inbox {
            Inbox::default()
        }
        pub fn get_status(&self) {}
        pub fn get_settings(&self) {}
        pub fn put_settings(&self, _settings: &serde_json::Value) {}
        pub fn set_auto_profile(&self, _on: bool) {}
        pub fn close_ws(&self) {}
        pub fn reconnect_ws_if_closed(&self) {}
        pub fn get_sampling_report(&self, _ranges: &[(u64, u64, Option<u32>)]) {}
        pub fn get_sampling_report_scope(&self, _name_id: u32) {}
        pub fn get_sampling_tree_scope(&self, _name_id: u32, _mode: &str) {}
        pub fn get_sampling_tree(&self, _ranges: &[(u64, u64, Option<u32>)], _mode: &str) {}
        pub fn get_modules(&self, _pid: u32) {}
        pub fn get_disassembly(&self, _pid: u32, _function_id: u64) {}
        pub fn get_example_disassembly(&self) {}
        pub fn get_source(&self, _path: &str) {}
        pub fn get_processes(&self) {}
        pub fn pull_view(&self, _t0: u64, _t1: u64, _width: u32) {}
        pub fn start_capture(&self, _req: &CaptureStart) {}
        pub fn stop_capture(&self) {}
        pub fn load_symbols(&self, _pid: u32) {}
        pub fn get_symbols_status(&self, _pid: u32) {}
        pub fn search_functions(&self, _pid: u32, _q: &str, _limit: u32) {}
        pub fn list_functions(&self, _pid: u32) {}
        pub fn resolve_preset_functions(&self, _pid: u32, _generation: u64, _keys: String) {}
        pub fn get_bench(&self) {}
        pub fn start_bench(&self, _threads: u32, _depth_min: u32, _depth_max: u32, _rate: u64) {}
        pub fn set_bench_rate(&self, _rate: u64) {}
        pub fn stop_bench(&self) {}
        pub fn start_demo(&self) {}
        pub fn stop_demo(&self) {}
        pub fn apply_config(&self, _ring_bytes: u64, _spill: &str) {}
        pub fn start_self(&self) {}
        pub fn stop_self(&self) {}
        pub fn import_capture(&self, _bytes: Vec<u8>) {}
        pub fn clear_capture(&self) {}
        pub fn push_self_scopes(&self, _scopes: &[perfr_live_event::dev::RelScope]) {}
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native_impl::Net;

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

/// A query value, percent-encoded: paths carry `/`, spaces and worse.
#[allow(dead_code)]
fn percent_encode(text: &str) -> String {
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

