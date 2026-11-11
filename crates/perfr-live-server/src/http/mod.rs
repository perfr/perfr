// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use perfr_live_event::argb_to_css;
use perfr_live_render::{
    choose_lod, collect_instances, stack_height, TimelineLod, INSTANCE_MIN_PX,
};

use crate::{LiveService, ServerConfig};

mod origin;
mod viewer_log;
mod viewer_pack;
mod ws;

#[cfg(test)]
pub(crate) use origin::request_allowed;
use origin::{origin_middleware, share_origin_allowed};
use viewer_log::viewer_log;
pub(crate) use viewer_log::ViewerLogBudget;
#[cfg(test)]
pub(crate) use viewer_log::{VIEWER_LOG_BURST_BYTES, VIEWER_LOG_BYTES_PER_MINUTE};
#[cfg(test)]
use viewer_pack::{apply_isolation, ISOLATION};
use viewer_pack::{index, isolation_middleware, static_asset};
use ws::ws_upgrade;

pub async fn serve(service: Arc<LiveService>) -> Result<(), String> {
    let bind = service.config.lock().bind;
    let app = router(service);
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|e| format!("bind {bind}: {e}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("http server: {e}"))
}

pub fn router(service: Arc<LiveService>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_upgrade))
        .route("/api/status", get(status))
        .route("/api/processes", get(processes))
        .route("/api/peers", get(peers_list))
        .route("/api/peers/connect", post(peers_connect))
        .route("/api/peer", get(peer_ws))
        .route("/api/join", get(join_ws))
        .route("/api/capture/start", post(capture_start))
        .route("/api/capture/stop", post(capture_stop))
        .route("/api/symbols/load", post(symbols_load))
        .route("/api/symbols/status", get(symbols_status))
        .route("/api/functions/search", get(functions_search))
        .route("/api/functions/resolve", post(functions_resolve).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)))
        .route("/api/code/disassembly", get(code_disassembly))
        .route("/api/code/source", get(code_source))
        .route("/api/code/example", get(code_example))
        .route("/api/demo/start", post(demo_start))
        .route("/api/demo/stop", post(demo_stop))
        .route("/api/bench", get(bench_status))
        .route("/api/bench/start", post(bench_start))
        .route("/api/bench/rate", post(bench_rate))
        .route("/api/bench/stop", post(bench_stop))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/auto_profile", post(auto_profile))
        .route("/api/frame", get(frame))
        .route("/api/timeline", get(timeline))
        .route("/api/sampling/report", get(sampling_report))
        .route("/api/sampling/tree", get(sampling_tree))
        .route("/api/sampling/callstacks", get(sampling_callstacks))
        .route("/api/functions/stats", get(function_stats))
        .route("/api/logs", get(capture_logs))
        .route("/api/symbols/modules", get(symbols_modules))
        .route("/api/capture/export", get(capture_export))
        .route("/api/capture/targets", get(capture_targets))
        .route("/api/capture/share", post(capture_share))
        .route("/api/capture/open", post(capture_open))
        .route("/api/capture/clear", post(capture_clear))
        .route("/api/scope", post(agent_scope))
        .route("/api/events", post(ingest_events).layer(axum::extract::DefaultBodyLimit::max(EVENTS_BODY_LIMIT)))
        .route("/api/log", post(viewer_log))
        .route(
            "/api/capture/import",
            post(capture_import).layer(axum::extract::DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
        )
        .route("/*path", get(static_asset))
        // In front of every route, the WebSocket upgrades included: there
        // is no authentication behind this, so who may call is decided
        // here, once. See `request_allowed`.
        .layer(axum::middleware::from_fn_with_state(service.clone(), origin_middleware))
        // Outermost: HTML, js, wasm, worker snippets, and API all get
        // COOP/COEP/CORP so SharedArrayBuffer is not blocked by a missing
        // header on one worker URL.
        .layer(axum::middleware::from_fn(isolation_middleware))
        .with_state(service)
}

async fn status(State(svc): State<Arc<LiveService>>) -> Json<StatusBody> {
    Json(StatusBody::from_service(&svc))
}

#[derive(Serialize)]
struct StatusBody {
    capturing: bool,
    demo: bool,
    events_live: u64,
    events_capacity: u64,
    dropped: u64,
    spilled: u64,
    produced: u64,
    oldest_start_ns: u64,
    newest_end_ns: u64,
    /// Demo/capture end only. Not ring newest_end (pid 2/3).
    live_end_ns: u64,
    ring_bytes: u64,
    spill_path: Option<String>,
    http_bind: String,
    machine: String,
    /// PerfrService control hooks are registered (real capture, not rust-only).
    hooks: bool,
    /// Dynamic-instrumentation outcome for the running capture; empty when
    /// no functions were selected.
    instrumentation: String,
    /// JSON summary of a target crash blamed on a hook, empty when none.
    hook_crash: String,
    /// Auto-profiling is on, and what it is doing (null before it runs).
    auto_profile: bool,
    auto_profile_status: serde_json::Value,
    /// The event batch format on the WebSocket: raw, packed or deflate.
    wire: &'static str,
    /// The service's log file, where the viewer's own lines also end up.
    log_path: Option<String>,
    /// When the capture began on the capture clock; 0 until the loop says.
    capture_start_ns: u64,
    /// Events refused for starting before that, this capture.
    dropped_before_start: u64,
    /// The pid the capture targets; 0 when none was started.
    target_pid: u32,
    /// The service's own pid, so the viewer can put its rows last.
    service_pid: u32,
    /// Machines linked to this one (`GET /api/peers` lists them).
    peers: usize,
}

impl StatusBody {
    fn from_service(svc: &LiveService) -> Self {
        let stats = svc.stats();
        let cfg = svc.config.lock();
        Self {
            capturing: svc.capturing.load(std::sync::atomic::Ordering::Relaxed),
            demo: svc.demo.load(std::sync::atomic::Ordering::Relaxed),
            events_live: stats.events_live,
            events_capacity: stats.events_capacity,
            dropped: stats.dropped,
            spilled: stats.spilled,
            produced: stats.produced,
            oldest_start_ns: stats.oldest_start_ns,
            newest_end_ns: stats.newest_end_ns,
            live_end_ns: svc.live_end_ns(),
            ring_bytes: stats.bytes_capacity,
            spill_path: cfg.spill_path.as_ref().map(|p| p.display().to_string()),
            http_bind: cfg.bind.to_string(),
            machine: "local".into(),
            hooks: svc.has_hooks(),
            instrumentation: svc.instrumentation_status(),
            hook_crash: svc.hook_crash(),
            auto_profile: svc.auto_profile(),
            auto_profile_status: svc.auto_profile_status(),
            // From the guard already held: `svc.wire()` would take the same
            // lock again and hang the status route.
            wire: cfg.wire.name(),
            log_path: svc.log_path.lock().clone(),
            capture_start_ns: svc.capture_start_ns(),
            dropped_before_start: svc.dropped_before_start(),
            target_pid: svc.capture_pid(),
            service_pid: std::process::id(),
            peers: svc.peers.len(),
        }
    }
}

fn hooks_clone(svc: &LiveService) -> Option<crate::ControlHooks> {
    svc.hooks.lock().clone()
}

/// Answers a request with `work`, run on the blocking pool. For a handler
/// that calls one of the service's hooks, copies or scans the ring, or
/// touches a disk: all of that is synchronous and some of it takes
/// seconds (the process list of a busy machine, a symbol search over a
/// large binary, an index over millions of events). Run on the handler's
/// own thread it parks one of the runtime's few workers, and with each
/// worker so parked nothing else is served, the WebSocket sends included.
async fn blocking(work: impl FnOnce() -> Response + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(response) => response,
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn processes(State(svc): State<Arc<LiveService>>) -> Response {
    blocking(move || processes_response(&svc)).await
}

fn processes_response(svc: &LiveService) -> Response {
    let raw = match hooks_clone(svc) {
        Some(h) => match (h.list_processes_json)() {
            Ok(json) => json,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        None => {
            let mut list: Vec<serde_json::Value> =
                if svc.demo.load(std::sync::atomic::Ordering::Relaxed) || svc.live_end_ns() > 0 {
                    serde_json::from_str(&crate::demo::process_list_json()).unwrap_or_default()
                } else {
                    Vec::new()
                };
            if svc.bench.running.load(std::sync::atomic::Ordering::Relaxed) {
                list.push(serde_json::json!({"pid": crate::bench::BENCH_PID, "name": "perfr-bench"}));
            }
            serde_json::to_string(&list).unwrap_or_else(|_| "[]".into())
        }
    };
    let raw = with_peer_processes(svc, raw);
    ([(header::CONTENT_TYPE, "application/json")], raw).into_response()
}

/// The remotes' processes appended to this machine's list, under their
/// packed pids and `@host` names.
fn with_peer_processes(svc: &LiveService, raw: String) -> String {
    if svc.peers.is_empty() {
        return raw;
    }
    let mut list: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap_or_default();
    list.extend(svc.peers.processes());
    serde_json::to_string(&list).unwrap_or(raw)
}

/// This machine's process list, as `GET /api/processes` gives it without
/// the remotes: what a remote sends its hub.
pub fn process_list_json(svc: &LiveService) -> String {
    match hooks_clone(svc) {
        Some(h) => (h.list_processes_json)().unwrap_or_else(|_| "[]".into()),
        None => "[]".into(),
    }
}

/// `GET /api/peers`: the machines linked to this one.
async fn peers_list(State(svc): State<Arc<LiveService>>) -> Response {
    let body = serde_json::json!({
        "machine": svc.machine_name(),
        "peers": svc.peers.list(),
        "dialing": svc.peers.dialing(),
    });
    ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

#[derive(Deserialize)]
struct ConnectBody {
    target: String,
}

/// `POST /api/peers/connect {"target": "host[:port]"}`: this service, as a
/// hub, dials that machine's service and keeps dialing it.
async fn peers_connect(State(svc): State<Arc<LiveService>>, Json(body): Json<ConnectBody>) -> Response {
    if let Err(error) = crate::peers::ws_url(&body.target, "/api/peer") {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    match svc.peers.start_dialing(&body.target) {
        Ok(true) => {
            tokio::spawn(crate::peers::redial(svc, body.target));
            StatusCode::OK.into_response()
        }
        Ok(false) => (StatusCode::OK, "already dialing").into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

/// `GET /api/peer` (WebSocket): a hub links this service as one of its
/// machines; this side runs the remote role.
async fn peer_ws(ws: WebSocketUpgrade, State(svc): State<Arc<LiveService>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let link = crate::peers::link_axum(socket);
        let info = crate::peers::RemoteInfo {
            host: svc.machine_name(),
            cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
            os: std::env::consts::OS.into(),
        };
        if let Err(error) = crate::peers::remote_role(svc, link, info).await {
            log::warn!("a hub's link ended: {error}");
        }
    })
}

/// `GET /api/join` (WebSocket): a remote dials in to be one of this
/// service's machines; this side runs the hub role.
async fn join_ws(ws: WebSocketUpgrade, State(svc): State<Arc<LiveService>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let link = crate::peers::link_axum(socket);
        if let Err(error) = crate::peers::hub_role(svc, link, "joined").await {
            log::warn!("a joined machine's link ended: {error}");
        }
    })
}

/// `GET /api/sampling/report?start_ns=&end_ns=` -- aggregates the sampled
/// callstacks inside a selection into a report. 501 when the service behind
/// this server does not sample.
#[derive(Deserialize)]
pub struct ReportQuery {
    #[serde(default)]
    pub start_ns: u64,
    #[serde(default)]
    pub end_ns: u64,
    /// Narrows to one thread; absent means every thread.
    pub tid: Option<u32>,
    /// Multi-select: a comma-separated list of `start-end` or `start-end:tid`
    /// windows. When present it supersedes `start_ns`/`end_ns`/`tid`, and the
    /// report is the union over all of them.
    pub ranges: Option<String>,
    /// A scope's name id: the report over every sample inside any instance
    /// of that scope, instead of a time selection.
    pub scope: Option<u32>,
}

#[derive(serde::Deserialize)]
struct TreeQuery {
    t0: Option<u64>,
    t1: Option<u64>,
    mode: Option<String>,
    /// Narrows to one thread, the way dragging on a single thread's sample
    /// bar does in the native UI. Absent means every thread.
    tid: Option<u32>,
    /// Multi-select union of windows; see `ReportQuery::ranges`.
    ranges: Option<String>,
    /// A scope's name id: the tree over every sample inside any instance of
    /// that scope, instead of a time selection.
    scope: Option<u32>,
}

/// Parses `start-end` / `start-end:tid` windows separated by commas into range
/// specs. Malformed entries are skipped rather than failing the request, so a
/// stray comma never blanks a report.
fn parse_ranges(text: &str) -> Vec<crate::SampleRangeSpec> {
    text.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (window, tid) = match part.split_once(':') {
                Some((w, t)) => (w, t.parse::<u32>().ok()),
                None => (part, None),
            };
            let (a, b) = window.split_once('-')?;
            let start = a.trim().parse::<u64>().ok()?;
            let end = b.trim().parse::<u64>().ok()?;
            Some((start.min(end), start.max(end), tid))
        })
        .collect()
}

async fn sampling_tree(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<TreeQuery>,
) -> Response {
    let mode = q.mode.unwrap_or_else(|| "top_down".to_string());
    if let Some(name_id) = q.scope {
        let hook = svc.sampling_tree_scope.lock().clone();
        let Some(tree) = hook else {
            return (StatusCode::NOT_IMPLEMENTED, "this service does not scope trees").into_response();
        };
        return match tokio::task::spawn_blocking(move || tree(name_id, &mode)).await {
            Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }
    let tree = svc.sampling_tree.lock().clone();
    let Some(tree) = tree else {
        return (StatusCode::NOT_FOUND, "no sampling tree available").into_response();
    };
    // No range means the whole capture, which is what you want the moment a
    // capture stops and you have not selected anything yet.
    let ranges: Vec<crate::SampleRangeSpec> = match q.ranges.as_deref() {
        Some(text) => parse_ranges(text),
        None => vec![(q.t0.unwrap_or(0), q.t1.unwrap_or(u64::MAX), q.tid)],
    };
    let ranges = if ranges.is_empty() {
        vec![(0, u64::MAX, None)]
    } else {
        ranges
    };
    blocking(move || match tree(&ranges, &mode) {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    })
    .await
}

/// The largest capture bundle an import accepts. A slice is rarely more than
/// a few hundred megabytes; the ring itself is 256 MiB by default.
const IMPORT_BODY_LIMIT: usize = 4 << 30;

#[derive(Deserialize)]
struct TargetsQuery {
    #[serde(default)]
    q: String,
    t0: Option<u64>,
    t1: Option<u64>,
}

/// `GET /api/capture/targets?q=&t0=&t1=` -- threads, scopes and per-core
/// busy fraction in a window. A missing `t0` is the start of the capture;
/// a `t1` that is absent, 0, or not after `t0` is its end.
/// The busy fraction is scheduling-slice coverage, not a claim about the program.
async fn capture_targets(State(svc): State<Arc<LiveService>>, Query(q): Query<TargetsQuery>) -> Response {
    blocking(move || capture_targets_body(&svc, q).into_response()).await
}

fn capture_targets_body(svc: &LiveService, q: TargetsQuery) -> Json<serde_json::Value> {
    let index = svc.cached_index();
    let (start, end) = index.time_bounds().unwrap_or((0, 1));
    let (t0, t1) = match (q.t0, q.t1) {
        (Some(a), Some(b)) if b > a => (a, b),
        (Some(a), _) => (a, end.max(a.saturating_add(1))),
        (None, Some(b)) if b > start => (start, b),
        (None, _) => (start, end),
    };
    // The walk first, the names after: see `targets::Scan`.
    let scan = super::targets::scan(&index, t0, t1);
    let (threads, processes) = svc.capture_names();
    let intern = svc.intern.lock();
    Json(super::targets::describe(scan, &intern, &threads, &processes, &q.q))
}

/// `GET /api/capture/export?format=ipc|parquet|bundle|stream&t0=..&t1=..` --
/// the capture as one file, offered as a download. `ipc` (the default) is an
/// Arrow IPC file of the events, `parquet` the same as Parquet, `bundle` a
/// self-contained `.perfr.zip` (events, samples, frames, thread and process
/// names) that the viewer can open again. With `t0` and `t1` (capture-clock
/// nanoseconds) only that time slice is exported. 501 when the service does
/// not provide an encoder, 400 for a format it does not know. `stream` is
/// the wire frames a connecting viewer receives, as one file
/// (`.perfr.stream`): the viewer opens it with `?capture=<url>` and no
/// service, which is how a web page embeds a capture. Served by the server
/// itself, no encoder needed.
#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
    t0: Option<u64>,
    t1: Option<u64>,
}

/// The file name a download gets: the format's extension, and `-slice`
/// when a window was asked for.
fn export_filename(format: &str, sliced: bool) -> String {
    let stem = if sliced { "capture-slice" } else { "capture" };
    match format {
        "parquet" => format!("{stem}.parquet"),
        "bundle" => format!("{stem}.perfr.zip"),
        "stream" => format!("{stem}.perfr.stream"),
        _ => format!("{stem}.arrow"),
    }
}

async fn capture_export(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<ExportQuery>,
) -> Response {
    let format = q.format.unwrap_or_else(|| "ipc".to_string());
    let window = match (q.t0, q.t1) {
        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        (None, None) => None,
        _ => {
            return (StatusCode::BAD_REQUEST, "give both t0 and t1, or neither").into_response();
        }
    };
    if format == "stream" {
        let filename = export_filename(&format, window.is_some());
        return blocking(move || {
            let bytes = svc.capture_stream(window);
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream"),
                    (
                        header::CONTENT_DISPOSITION,
                        &*format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        })
        .await;
    }
    let hook = svc.capture_export.lock().clone();
    let Some(export) = hook else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "this service does not export captures",
        )
            .into_response();
    };
    let content_type = match format.as_str() {
        "ipc" => "application/vnd.apache.arrow.file",
        "parquet" => "application/vnd.apache.parquet",
        "bundle" => "application/zip",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("unknown export format {other:?}; use ipc, parquet, bundle or stream"),
            )
                .into_response();
        }
    };
    let filename = export_filename(&format, window.is_some());
    match tokio::task::spawn_blocking(move || export(&format, window)).await {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, content_type),
                (
                    header::CONTENT_DISPOSITION,
                    &*format!("attachment; filename=\"{filename}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// Publish a stopped capture (or selected slice) to the configured bucket.
async fn capture_share(State(svc): State<Arc<LiveService>>, Query(q): Query<ExportQuery>, headers: axum::http::HeaderMap) -> Response {
    // Stricter than the rule every route is behind (`request_allowed`): an
    // upload leaves the machine, so the page asking must have been opened
    // by address or as localhost, not by the machine's name.
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default();
        if !share_origin_allowed(origin, host, svc.config.lock().bind) {
            return (StatusCode::FORBIDDEN,
                "Sharing must be requested from the service viewer, opened by IP address or as localhost").into_response();
        }
    }
    let window = match (q.t0, q.t1) {
        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        (None, None) => None,
        _ => return (StatusCode::BAD_REQUEST, "give both t0 and t1, or neither").into_response(),
    };
    if svc.capturing.load(std::sync::atomic::Ordering::Relaxed) || svc.demo.load(std::sync::atomic::Ordering::Relaxed) {
        return (StatusCode::CONFLICT, "Stop recording before sharing").into_response();
    }
    let config = match crate::sharing::ShareConfig::from_env() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
    };
    let Some(export) = svc.capture_share_export.lock().clone() else {
        return (StatusCode::NOT_IMPLEMENTED, "This service cannot export a complete capture").into_response();
    };
    match tokio::task::spawn_blocking(move || {
        let (bundle, stream) = export(window)?;
        config.upload(bundle, stream)
    }).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, e).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Share task failed").into_response(),
    }
}

/// `POST /api/scope` with `{"track": "agent", "action": "start", "name":
/// "tool call"}` (or `stop`, `instant`, `value` with `"value"`), optional
/// `"timestamp_ns"`: manual instrumentation for anything that can make an
/// HTTP request -- an agent shelling out to `perfr-scope`, a CI step, a
/// script. Scopes nest per track. 501 without the service.
#[derive(Deserialize)]
struct ScopeBody {
    #[serde(default = "default_track")]
    track: String,
    action: String,
    #[serde(default)]
    name: String,
    value: Option<f64>,
    timestamp_ns: Option<u64>,
}

fn default_track() -> String {
    "agent".to_string()
}

async fn agent_scope(State(svc): State<Arc<LiveService>>, Json(body): Json<ScopeBody>) -> Response {
    let hook = svc.agent_scope.lock().clone();
    let Some(hook) = hook else {
        return (StatusCode::NOT_IMPLEMENTED, "this service does not take agent scopes").into_response();
    };
    let action = match body.action.as_str() {
        "start" if !body.name.is_empty() => crate::AgentAction::Start { name: body.name },
        "stop" => crate::AgentAction::Stop,
        "instant" if !body.name.is_empty() => crate::AgentAction::Instant { name: body.name },
        "value" => match (body.name.is_empty(), body.value) {
            (false, Some(v)) => crate::AgentAction::Value { name: body.name, value: v },
            _ => return (StatusCode::BAD_REQUEST, "value needs a name and a value").into_response(),
        },
        "start" | "instant" => return (StatusCode::BAD_REQUEST, "give the scope a name").into_response(),
        other => return (StatusCode::BAD_REQUEST, format!("unknown action {other:?}: start, stop, instant or value")).into_response(),
    };
    let req = crate::AgentScope { track: body.track, action, timestamp_ns: body.timestamp_ns };
    match tokio::task::spawn_blocking(move || hook(req)).await {
        Ok(Ok(summary)) => ([(header::CONTENT_TYPE, "application/json")], summary).into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// `POST /api/events`: a batch of processes, threads, spans, instants and
/// values from an outside producer, filed under the pids and tids the
/// caller chose. See [`crate::ingest`]. Unlike `/api/scope` this needs no
/// service hook: names and events go straight to the ring and the viewers.
const EVENTS_BODY_LIMIT: usize = 32 * 1024 * 1024;

async fn ingest_events(
    State(svc): State<Arc<LiveService>>,
    Json(body): Json<crate::ingest::EventsBody>,
) -> Response {
    // Both clocks now: the capture clock from the service, so a Windows
    // service stamps on the same QueryPerformanceCounter axis its capture uses.
    let now = crate::ingest::ClockNow { monotonic_ns: svc.now_ns(), unix_ns: crate::ingest::now_unix_ns() };
    blocking(move || Json(crate::ingest::ingest(&svc, body, now)).into_response()).await
}

/// `POST /api/capture/clear`: empties the capture -- the ring, the names,
/// and (through the service's hook) its samples -- so the view starts from
/// nothing. 409 while a capture is running.
async fn capture_clear(State(svc): State<Arc<LiveService>>) -> Response {
    blocking(move || {
        if let Some(hook) = svc.capture_clear.lock().clone() {
            if let Err(error) = hook() {
                let status = if error.starts_with("busy") { StatusCode::CONFLICT } else { StatusCode::INTERNAL_SERVER_ERROR };
                return (status, error).into_response();
            }
        }
        match svc.clear_capture() {
            Ok(()) => StatusCode::OK.into_response(),
            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
        }
    })
    .await
}

/// `POST /api/capture/open` with `{"path": "...", "t0": .., "t1": ..}`:
/// opens the bundle at that path on the service's machine as the current
/// capture -- the whole file, or just the window, which the file's
/// row-group statistics let the service cut without reading the rest.
/// The path must be a regular file no larger than an import may be.
#[derive(Deserialize)]
struct OpenBody {
    path: String,
    t0: Option<u64>,
    t1: Option<u64>,
}

/// Whether `path` is something the open hook may read whole: a regular
/// file, not empty, and no larger than `max_len`. The hook reads to the end
/// of whatever it is given, so a FIFO parks a blocking thread until someone
/// writes to it, `/dev/zero` is read until memory runs out, and a file under
/// `/proc` (regular, but of length 0 whatever it produces) can do either;
/// an empty file is no capture anyway. The file can still change between
/// this look and the read: this stops a path that was never a capture, not
/// a race.
pub(crate) fn check_capture_file(path: &str, max_len: u64) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    if meta.len() == 0 {
        return Err(format!("{path}: the file is empty"));
    }
    if meta.len() > max_len {
        return Err(format!(
            "{path}: {} bytes, and a capture file can be {max_len} at most",
            meta.len()
        ));
    }
    Ok(())
}

async fn capture_open(State(svc): State<Arc<LiveService>>, Json(body): Json<OpenBody>) -> Response {
    let hook = svc.capture_open.lock().clone();
    let Some(open) = hook else {
        return (StatusCode::NOT_IMPLEMENTED, "this service does not open capture files").into_response();
    };
    let window = match (body.t0, body.t1) {
        (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
        (None, None) => None,
        _ => return (StatusCode::BAD_REQUEST, "give both t0 and t1, or neither").into_response(),
    };
    let opened = tokio::task::spawn_blocking(move || {
        check_capture_file(&body.path, IMPORT_BODY_LIMIT as u64)?;
        open(&body.path, window)
    });
    match opened.await {
        Ok(Ok(summary)) => ([(header::CONTENT_TYPE, "application/json")], summary).into_response(),
        Ok(Err(error)) => {
            let status = if error.starts_with("busy") { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST };
            (status, error).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

/// `POST /api/capture/import` with a `.perfr.zip` body: opens that capture
/// as the current one. 501 when the service cannot, 400 when the bytes are
/// not a capture, 409 when a capture is running.
async fn capture_import(State(svc): State<Arc<LiveService>>, body: Bytes) -> Response {
    let hook = svc.capture_import.lock().clone();
    let Some(import) = hook else {
        return (StatusCode::NOT_IMPLEMENTED, "this service does not open captures").into_response();
    };
    match tokio::task::spawn_blocking(move || import(body.to_vec())).await {
        Ok(Ok(summary)) => (
            [(header::CONTENT_TYPE, "application/json")],
            summary,
        )
            .into_response(),
        Ok(Err(error)) => {
            let status = if error.starts_with("busy") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, error).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn symbols_modules(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let modules = svc.modules_json.lock().clone();
    let Some(modules) = modules else {
        return ([(header::CONTENT_TYPE, "application/json")], r#"{"modules":[]}"#).into_response();
    };
    blocking(move || match modules(q.pid.unwrap_or(0)) {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    })
    .await
}

#[derive(Deserialize)]
struct CallstacksQuery {
    #[serde(default)]
    start_ns: u64,
    #[serde(default)]
    end_ns: u64,
    tid: Option<u32>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// `GET /api/sampling/callstacks` — the hottest distinct stacks, one page.
/// 501 when this service does not sample.
async fn sampling_callstacks(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<CallstacksQuery>,
) -> Response {
    let hook = svc.sampling_callstacks.lock().clone();
    let Some(stacks) = hook else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            "this service does not provide sampling call stacks",
        )
            .into_response();
    };
    let end = if q.end_ns == 0 { u64::MAX } else { q.end_ns };
    let ranges = vec![(q.start_ns, end, q.tid)];
    let offset = q.offset;
    let limit = q.limit.unwrap_or(8).clamp(1, 32);
    match tokio::task::spawn_blocking(move || stacks(&ranges, offset, limit)).await {
        Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct StatsQuery {
    #[serde(default)]
    q: String,
    pid: Option<u32>,
    #[serde(default)]
    order: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default)]
    t0: u64,
    #[serde(default)]
    t1: u64,
    tid: Option<u32>,
    /// Scope name. Only lines logged while a scope of that name was open on
    /// the same thread, on `tid` or on every thread when `tid` is omitted.
    /// Identical lines come back as one row with a count.
    scope: Option<String>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// `GET /api/logs` — log lines in a time range, optionally one thread.
///
/// A scope name limits them to the lines logged inside that scope, which is
/// how lines are grouped: there is no category on a line. With a scope, the
/// time range selects which of its instances count, and identical lines are
/// folded into one row with a count.
async fn capture_logs(State(svc): State<Arc<LiveService>>, Query(q): Query<LogsQuery>) -> Response {
    use perfr_live_event::logs::{aggregate, level_name, logs_in_window, ScopeSpan, SpanSet};
    use perfr_live_event::kind;

    let offset = q.offset as usize;
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    let t0 = q.t0;
    let t1 = if q.t1 == 0 { u64::MAX } else { q.t1.max(t0) };
    let tid = q.tid;
    let scope_name = q.scope.unwrap_or_default();
    match tokio::task::spawn_blocking(move || {
        let (_gen, events) = svc.ring().snapshot();
        // The intern table is locked only to turn ids into text and back,
        // never across the scan: the capture thread interns every new name
        // through the same lock, and a drain that waits on it lets the
        // producers' rings lap.
        let mut logs = logs_in_window(&events, t0, t1, tid);
        let grouped = !scope_name.is_empty();
        let (total, rows): (usize, Vec<serde_json::Value>) = if grouped {
            // Which ids spell the scope's name. Checked per distinct scope
            // id rather than per event, so the lock is held for a handful of
            // lookups however long the capture is.
            let mut candidates: Vec<u32> = events
                .iter()
                .filter(|e| e.kind == kind::API_SCOPE || e.kind == kind::API_TRACK)
                .map(|e| e.name_id)
                .collect();
            candidates.sort_unstable();
            candidates.dedup();
            let named: std::collections::HashSet<u32> = {
                let intern = svc.intern.lock();
                candidates.into_iter().filter(|id| intern.get(*id) == Some(scope_name.as_str())).collect()
            };
            let spans: Vec<ScopeSpan> = events
                .iter()
                .filter(|e| named.contains(&e.name_id))
                .filter_map(ScopeSpan::from_event)
                .filter(|s| tid.is_none_or(|t| s.tid == t) && s.overlaps(t0, t1))
                .collect();
            let inside = SpanSet::new(&spans);
            logs.retain(|e| inside.contains_log(e));
            let groups = aggregate(&logs);
            let total = groups.len();
            let page: Vec<_> = groups.into_iter().skip(offset).take(limit).collect();
            let intern = svc.intern.lock();
            let rows = page
                .into_iter()
                .map(|g| serde_json::json!({
                    "t_ns": g.first_ns,
                    "last_ns": g.last_ns,
                    "tid": g.tid,
                    "pid": g.pid,
                    "level": level_name(g.level),
                    "message": intern.get(g.name_id).unwrap_or(""),
                    "count": g.count,
                    "truncated": g.truncated,
                }))
                .collect();
            (total, rows)
        } else {
            let total = logs.len();
            let page: Vec<_> = logs.into_iter().skip(offset).take(limit).collect();
            let intern = svc.intern.lock();
            let rows = page
                .into_iter()
                .map(|e| serde_json::json!({
                    "t_ns": e.start_ns,
                    "tid": e.tid,
                    "pid": e.pid,
                    "level": level_name(e.log_level().unwrap_or(0)),
                    "message": intern.get(e.name_id).unwrap_or(""),
                    "count": 1,
                    "truncated": e.log_truncated(),
                }))
                .collect();
            (total, rows)
        };
        serde_json::json!({
            "t0": t0,
            "t1": if t1 == u64::MAX { 0 } else { t1 },
            "total": total,
            "offset": offset,
            "grouped": grouped,
            "logs": rows,
        })
        .to_string()
    })
    .await
    {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /api/functions/stats` — one row per measured hook or manual scope.
/// Sampled frames are excluded. The page is small on purpose: the ring is not.
async fn function_stats(State(svc): State<Arc<LiveService>>, Query(q): Query<StatsQuery>) -> Response {
    let offset = q.offset as usize;
    let limit = q.limit.unwrap_or(20).clamp(1, 100) as usize;
    let order = crate::stats::StatsOrder::parse(&q.order);
    let query = q.q;
    let pid = q.pid;
    match tokio::task::spawn_blocking(move || {
        let (_gen, events) = svc.ring().snapshot();
        let scanned = events.len();
        // Names are resolved once per distinct scope, after the fold, not
        // once per event.
        let rows = crate::stats::fold_timing(&events, |id| {
            svc.intern.lock().get(id).unwrap_or("?").to_string()
        });
        crate::stats::page_timing(rows, &query, pid, order, offset, limit, scanned)
    })
    .await
    {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn sampling_report(
    State(svc): State<Arc<LiveService>>,
    axum::extract::Query(query): axum::extract::Query<ReportQuery>,
) -> Response {
    if let Some(name_id) = query.scope {
        let hook = svc.sampling_report_scope.lock().clone();
        let Some(report) = hook else {
            return (StatusCode::NOT_IMPLEMENTED, "this service does not scope reports").into_response();
        };
        return match tokio::task::spawn_blocking(move || report(name_id)).await {
            Ok(Ok(json)) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }
    let hook = svc.sampling_report.lock().clone();
    match hook {
        None => (
            StatusCode::NOT_IMPLEMENTED,
            "this service does not provide sampling reports",
        )
            .into_response(),
        Some(report) => {
            let ranges: Vec<crate::SampleRangeSpec> = match query.ranges.as_deref() {
                Some(text) => parse_ranges(text),
                None => {
                    let end = if query.end_ns == 0 { u64::MAX } else { query.end_ns };
                    vec![(query.start_ns, end, query.tid)]
                }
            };
            let ranges = if ranges.is_empty() {
                vec![(0, u64::MAX, None)]
            } else {
                ranges
            };
            match tokio::task::spawn_blocking(move || report(&ranges)).await {
                Ok(Ok(json)) => ([(axum::http::header::CONTENT_TYPE, "application/json")], json)
                    .into_response(),
                Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
                Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StartBody {
    pub pid: u32,
    #[serde(default = "default_true")]
    pub enable_api: bool,
    #[serde(default = "default_true")]
    pub context_switches: bool,
    #[serde(default = "default_true")]
    pub thread_states: bool,
    #[serde(default = "default_true")]
    pub sampling: bool,
    #[serde(default = "default_sps")]
    pub samples_per_second: f64,
    #[serde(default = "default_unwinding")]
    pub unwinding: String,
    #[serde(default = "default_dyn_instr")]
    pub dynamic_instrumentation_method: String,
    #[serde(default)]
    pub instrumented_functions: Vec<InstrumentedFnRef>,
    /// Show every process on the machine, not just the target and anything
    /// instrumented. Off by default: system-wide scheduling projects a thread
    /// bar per process, which buries the target under hundreds of rows.
    #[serde(default)]
    pub show_all_processes: bool,
    /// Drop the duplicate uprobe entries the kernel reports on thread
    /// migration (the C++ `UprobesUnwindingVisitor` filter). On by default;
    /// off shows the ghost scopes it removes.
    #[serde(default = "default_true")]
    pub uprobe_duplicate_filter: bool,
    /// Per-capture override of the auto-unhook limit (calls per second past
    /// which a hooked function is switched off; 0 = never). Absent, the
    /// persisted setting applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hook_calls_per_s: Option<u64>,
    /// Start with auto-profiling on: the service samples, hooks what the
    /// samples point at, and prunes it to a scopes-a-second budget.
    #[serde(default)]
    pub auto_profile: bool,
}

fn default_true() -> bool {
    true
}
fn default_sps() -> f64 {
    1000.0
}
fn default_unwinding() -> String {
    "dwarf".into()
}
fn default_dyn_instr() -> String {
    "frida".into()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstrumentedFnRef {
    pub function_id: u64,
}

impl StartBody {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| format!(r#"{{"pid":{}}}"#, self.pid))
    }
}

async fn capture_start(
    State(svc): State<Arc<LiveService>>,
    Json(body): Json<StartBody>,
) -> Response {
    // pid 0 is a capture without a target: the scheduler, the service, and
    // every process instrumenting itself. A pid of another machine's is
    // that machine's target: it captures that process, this one captures
    // with no target, and every other linked machine likewise.
    let remote_target = crate::peers::machine_of(body.pid).map(|m| (m, crate::peers::unpack(body.pid)));
    let mut local = body.clone();
    if remote_target.is_some() {
        local.pid = 0;
        local.instrumented_functions.clear();
        local.auto_profile = false;
    }
    let json = local.to_json();
    let auto_profile = AutoProfileChoice::propose(&svc, local.auto_profile);
    let starts_before = svc.capture_starts();
    match hooks_clone(&svc) {
        Some(h) => {
            let result = tokio::task::spawn_blocking(move || (h.start_capture)(&json)).await;
            auto_profile.settle(&svc, matches!(result, Ok(Ok(()))));
            match result {
                Ok(Ok(())) => {
                    svc.mark_capture_requested(body.pid, starts_before);
                    svc.peers.send_start(remote_target);
                    StatusCode::OK.into_response()
                }
                Ok(Err(e)) => (StatusCode::CONFLICT, e).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        None => {
            // Rust-only / missing hooks: Record falls back to the demo producer.
            let started = crate::demo::start(&svc, 50_000);
            auto_profile.settle(&svc, started.is_ok());
            match started {
                Ok(()) => (
                    StatusCode::OK,
                    Json(serde_json::json!({"demo": true, "reason": "no_hooks"})),
                )
                    .into_response(),
                Err(e) => (StatusCode::CONFLICT, e).into_response(),
            }
        }
    }
}

/// Each Record says whether its capture auto-profiles, and the capture loop
/// reads that from the service on every pass. The choice has to be in place
/// before the loop's first pass, so it goes in before the start hook runs;
/// but the hook is also what turns a Record down, and a Record that was
/// turned down has no say over the capture that is running: with the choice
/// simply stored up front, a second Record (another tab, an agent that did
/// not look at the status) answered "a capture is already running" and
/// switched that capture's hooking on or off all the same.
struct AutoProfileChoice {
    wanted: bool,
    /// What the flag said before this Record put its own choice in; `None`
    /// when it did not, because a capture was running and the flag was that
    /// capture's.
    previous: Option<bool>,
}

impl AutoProfileChoice {
    fn propose(svc: &LiveService, wanted: bool) -> Self {
        if svc.is_capturing() {
            return Self { wanted, previous: None };
        }
        let previous = svc.auto_profile.swap(wanted, std::sync::atomic::Ordering::Relaxed);
        Self { wanted, previous: Some(previous) }
    }

    /// The start hook has answered: an accepted Record's choice stands, a
    /// refused one's is taken back.
    fn settle(self, svc: &LiveService, accepted: bool) {
        match (accepted, self.previous) {
            // Accepted although a capture looked to be running (it ended
            // between the look and the hook): late, but this capture's.
            (true, None) => svc.set_auto_profile(self.wanted),
            (false, Some(previous)) => svc.set_auto_profile(previous),
            (true, Some(_)) | (false, None) => {}
        }
    }
}

/// Starts a capture of `pid` (0 for none) the way `POST /api/capture/start`
/// does with every other setting at its default: what a remote does when
/// its hub presses Record. Blocking.
pub fn start_capture_pid(svc: &Arc<LiveService>, pid: u32) -> Result<(), String> {
    let body: StartBody = serde_json::from_value(serde_json::json!({"pid": pid})).map_err(|e| e.to_string())?;
    let auto_profile = AutoProfileChoice::propose(svc, false);
    let starts_before = svc.capture_starts();
    let started = match hooks_clone(svc) {
        Some(h) => (h.start_capture)(&body.to_json()).map(|()| svc.mark_capture_requested(pid, starts_before)),
        None => crate::demo::start(svc, 50_000),
    };
    auto_profile.settle(svc, started.is_ok());
    started
}

/// Stops the capture the way `POST /api/capture/stop` does. Blocking.
pub fn stop_capture(svc: &Arc<LiveService>) -> Result<(), String> {
    match hooks_clone(svc) {
        Some(h) => {
            (h.stop_capture)()?;
            svc.mark_capture_finished();
            Ok(())
        }
        None => {
            crate::demo::stop(svc);
            svc.mark_capture_finished();
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct PidBody {
    pid: u32,
}

async fn symbols_load(State(svc): State<Arc<LiveService>>, Json(body): Json<PidBody>) -> Response {
    blocking(move || match hooks_clone(&svc) {
        Some(h) => match (h.load_symbols)(body.pid) {
            Ok(()) => StatusCode::OK.into_response(),
            Err(e) => (StatusCode::CONFLICT, e).into_response(),
        },
        None => (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"pid":0,"status":"idle","function_count":0,"module_count":0,"error":""}"#,
        )
            .into_response(),
    })
    .await
}

#[derive(Deserialize)]
struct SymbolsQuery {
    pid: Option<u32>,
}

async fn symbols_status(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<SymbolsQuery>,
) -> Response {
    let pid = q.pid.unwrap_or(0);
    blocking(move || match hooks_clone(&svc) {
        Some(h) => match (h.symbols_status_json)(pid) {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        None => (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"pid":0,"status":"idle","function_count":0,"module_count":0,"error":""}"#,
        )
            .into_response(),
    })
    .await
}

#[derive(Deserialize)]
struct SearchQuery {
    pid: Option<u32>,
    q: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct DisassemblyQuery {
    pid: Option<u32>,
    function_id: Option<u64>,
}

#[derive(Deserialize)]
struct SourceQuery {
    path: Option<String>,
}

fn json_or_error(result: Result<String, String>) -> Response {
    match result {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

async fn code_disassembly(State(svc): State<Arc<LiveService>>, Query(q): Query<DisassemblyQuery>) -> Response {
    blocking(move || match hooks_clone(&svc) {
        Some(h) => json_or_error((h.disassemble_json)(q.pid.unwrap_or(0), q.function_id.unwrap_or(0))),
        None => (StatusCode::NOT_IMPLEMENTED, "no service behind this server").into_response(),
    })
    .await
}

async fn code_source(State(svc): State<Arc<LiveService>>, Query(q): Query<SourceQuery>) -> Response {
    blocking(move || match hooks_clone(&svc) {
        Some(h) => json_or_error((h.source_json)(q.path.as_deref().unwrap_or(""))),
        None => (StatusCode::NOT_IMPLEMENTED, "no service behind this server").into_response(),
    })
    .await
}

async fn code_example(State(svc): State<Arc<LiveService>>) -> Response {
    blocking(move || match hooks_clone(&svc) {
        Some(h) => json_or_error((h.example_disassembly_json)()),
        None => (StatusCode::NOT_IMPLEMENTED, "no service behind this server").into_response(),
    })
    .await
}

async fn functions_search(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let pid = q.pid.unwrap_or(0);
    let query = q.q.unwrap_or_default();
    // A search wants a handful; the Functions view asks for the whole
    // index (u32::MAX) and filters on its own. No cap here: the largest
    // index seen (an Unreal Shipping build, 400k functions) is ~100 MB of
    // JSON built in about a second.
    let limit = q.limit.unwrap_or(24);
    blocking(move || match hooks_clone(&svc) {
        Some(h) => match (h.search_functions_json)(pid, &query, limit) {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        None => (
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"pid":0,"status":"idle","functions":[]}"#,
        )
            .into_response(),
    })
    .await
}

#[derive(Deserialize)]
struct ResolveFunctionsBody {
    pid: u32,
    functions: Vec<serde_json::Value>,
}

async fn functions_resolve(
    State(svc): State<Arc<LiveService>>,
    Json(body): Json<ResolveFunctionsBody>,
) -> Response {
    if body.functions.len() > 100_000 {
        return (StatusCode::BAD_REQUEST, "Too many preset functions").into_response();
    }
    blocking(move || {
        let keys = serde_json::to_string(&body.functions).unwrap();
        match hooks_clone(&svc) {
            Some(h) => match (h.resolve_functions_json)(body.pid, &keys) {
                Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
                Err(error) => (StatusCode::CONFLICT, error).into_response(),
            },
            None => (StatusCode::NOT_IMPLEMENTED, "Function resolution requires a capture service").into_response(),
        }
    })
    .await
}

async fn capture_stop(State(svc): State<Arc<LiveService>>) -> Response {
    svc.peers.send_stop();
    match hooks_clone(&svc) {
        Some(h) => {
            let result = tokio::task::spawn_blocking(move || (h.stop_capture)()).await;
            match result {
                Ok(Ok(())) => {
                    svc.mark_capture_finished();
                    StatusCode::OK.into_response()
                }
                Ok(Err(e)) => (StatusCode::CONFLICT, e).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        }
        None => {
            crate::demo::stop(&svc);
            svc.mark_capture_finished();
            StatusCode::OK.into_response()
        }
    }
}

#[derive(Deserialize)]
struct DemoBody {
    scopes_per_sec: Option<u64>,
}

async fn demo_start(State(svc): State<Arc<LiveService>>, Json(body): Json<DemoBody>) -> Response {
    match crate::demo::start(&svc, body.scopes_per_sec.unwrap_or(50_000)) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}

async fn demo_stop(State(svc): State<Arc<LiveService>>) -> Response {
    crate::demo::stop(&svc);
    StatusCode::OK.into_response()
}

/// The benchmark producer (`bench.rs`): `threads` threads of nested fake
/// scopes, `depth_min..=depth_max` deep, at `rate` events per second. Every
/// field is optional; the defaults are 16 threads, 8 to 16 deep, a million
/// a second. `rate` alone can be changed while it runs.
#[derive(Deserialize, Default)]
struct BenchBody {
    threads: Option<u32>,
    depth_min: Option<u32>,
    depth_max: Option<u32>,
    rate: Option<u64>,
}

async fn bench_status(State(svc): State<Arc<LiveService>>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], crate::bench::status_json(&svc)).into_response()
}

async fn bench_start(State(svc): State<Arc<LiveService>>, body: Option<Json<BenchBody>>) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let defaults = crate::bench::BenchParams::default();
    let params = crate::bench::BenchParams {
        threads: body.threads.unwrap_or(defaults.threads),
        depth_min: body.depth_min.unwrap_or(defaults.depth_min),
        depth_max: body.depth_max.unwrap_or(defaults.depth_max),
        rate: body.rate.unwrap_or(defaults.rate),
    };
    match crate::bench::start(&svc, params) {
        Ok(()) => ([(header::CONTENT_TYPE, "application/json")], crate::bench::status_json(&svc)).into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}

async fn bench_rate(State(svc): State<Arc<LiveService>>, Json(body): Json<BenchBody>) -> Response {
    match body.rate {
        Some(rate) => {
            crate::bench::set_rate(&svc, rate);
            ([(header::CONTENT_TYPE, "application/json")], crate::bench::status_json(&svc)).into_response()
        }
        None => (StatusCode::BAD_REQUEST, "rate is required").into_response(),
    }
}

async fn bench_stop(State(svc): State<Arc<LiveService>>) -> Response {
    crate::bench::stop(&svc);
    StatusCode::OK.into_response()
}

async fn get_config(State(svc): State<Arc<LiveService>>) -> Json<ConfigBody> {
    let cfg = svc.config.lock().clone();
    Json(ConfigBody::from_config(&cfg))
}

#[derive(Serialize, Deserialize)]
struct ConfigBody {
    ring_buffer_bytes: u64,
    spill_path: Option<String>,
}

impl ConfigBody {
    fn from_config(cfg: &ServerConfig) -> Self {
        Self {
            ring_buffer_bytes: cfg.ring_buffer_bytes,
            spill_path: cfg.spill_path.as_ref().map(|p| p.display().to_string()),
        }
    }
}

/// The body of `POST /api/auto_profile`.
#[derive(Deserialize)]
struct AutoProfileBody {
    on: bool,
}

/// Turns auto-profiling on or off, for the capture running now and the next
/// ones. Off leaves whatever it hooked as it stands.
async fn auto_profile(State(svc): State<Arc<LiveService>>, Json(body): Json<AutoProfileBody>) -> Response {
    svc.set_auto_profile(body.on);
    Json(serde_json::json!({ "on": body.on, "status": svc.auto_profile_status() })).into_response()
}

/// The persisted user settings (`settings.rs`).
async fn get_settings(State(svc): State<Arc<LiveService>>) -> Json<crate::Settings> {
    Json(svc.settings())
}

/// Replaces and saves them. The body is the whole object as `GET` returns
/// it; a key left out takes its default.
async fn put_settings(State(svc): State<Arc<LiveService>>, Json(body): Json<crate::Settings>) -> Response {
    blocking(move || match svc.update_settings(body) {
        Ok(saved) => Json(saved).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    })
    .await
}

/// The smallest ring `PUT /api/config` takes: under this a capture is a
/// fraction of a second, which nobody means to ask for.
pub(crate) const MIN_RING_BYTES: u64 = 1 << 20;

/// The largest, on a machine with memory to spare.
pub(crate) const MAX_RING_BYTES: u64 = 8 << 30;

/// The largest ring this machine takes: half of its memory, and never over
/// [`MAX_RING_BYTES`]. The ring is allocated and written in full when it is
/// created, so a size the machine cannot back is not a slow request but the
/// end of the process: the allocation fails (or its size overflows) and a
/// release build aborts. With the memory size unknown the fixed cap alone
/// applies.
pub(crate) fn max_ring_bytes(physical_memory: Option<u64>) -> u64 {
    physical_memory
        .map_or(MAX_RING_BYTES, |total| (total / 2).min(MAX_RING_BYTES))
        .max(MIN_RING_BYTES)
}

pub(crate) fn check_ring_bytes(bytes: u64, max: u64) -> Result<(), String> {
    if (MIN_RING_BYTES..=max).contains(&bytes) {
        return Ok(());
    }
    Err(format!(
        "ring_buffer_bytes must be between {MIN_RING_BYTES} (1 MiB) and {max} ({} MiB: half of this \
         machine's memory, {} GiB at most); got {bytes}",
        max >> 20,
        MAX_RING_BYTES >> 30,
    ))
}

/// The machine's physical memory, when the platform says.
#[cfg(target_os = "linux")]
fn physical_memory_bytes() -> Option<u64> {
    // `MemTotal:       65843088 kB`
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: sysconf takes no pointers; -1 is its "not known".
    let (pages, page_size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    (pages as u64).checked_mul(page_size as u64)
}

#[cfg(not(unix))]
fn physical_memory_bytes() -> Option<u64> {
    None
}

async fn put_config(State(svc): State<Arc<LiveService>>, Json(body): Json<ConfigBody>) -> Response {
    if let Err(error) = check_ring_bytes(body.ring_buffer_bytes, max_ring_bytes(physical_memory_bytes())) {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let spill = body
        .spill_path
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    // A new ring is allocated and written in full: gigabytes, at the top.
    blocking(move || match svc.replace_ring(body.ring_buffer_bytes, spill) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    })
    .await
}

#[derive(Deserialize)]
struct FrameQuery {
    t0: Option<u64>,
    t1: Option<u64>,
    width: Option<u32>,
}

/// `/api/frame` body: 16-byte header (`u32` width, `u32` lanes, 8 reserved) +
/// exactly `width * lanes * 4` RGBA bytes. t0/t1 stay on the query string.
pub(crate) fn encode_raster_body(raster: &perfr_live_render::RasterizedFrame) -> Vec<u8> {
    let expected = raster
        .width
        .saturating_mul(raster.lanes.len())
        .saturating_mul(4);
    let mut rgba = raster.to_rgba8();
    rgba.resize(expected, 0);
    let mut body = Vec::with_capacity(16 + expected);
    body.extend_from_slice(&(raster.width as u32).to_le_bytes());
    body.extend_from_slice(&(raster.lanes.len() as u32).to_le_bytes());
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&rgba);
    body
}

async fn frame(State(svc): State<Arc<LiveService>>, Query(q): Query<FrameQuery>) -> Response {
    let width = q.width.unwrap_or(1280).clamp(16, 4096) as usize;
    blocking(move || {
        let raster = svc.rasterize_frame(q.t0, q.t1, width);
        (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            encode_raster_body(&raster),
        )
            .into_response()
    })
    .await
}

#[derive(Clone, Serialize)]
pub(crate) struct InstanceJson {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: String,
    r: f32,
}

#[derive(Clone, Serialize)]
struct TimelineBody {
    lod: &'static str,
    width: u32,
    height: u32,
    lane_count: u32,
    instance_count: u32,
    instances: Vec<InstanceJson>,
}

fn timeline_body(svc: &LiveService, q: FrameQuery) -> TimelineBody {
    let width = q.width.unwrap_or(1280).clamp(16, 4096);
    let data_gen = svc.data_gen();
    {
        let cache = svc.timeline_cache.lock();
        if let Some(c) = cache.as_ref() {
            if c.t0 == q.t0.unwrap_or(c.t0)
                && c.t1 == q.t1.unwrap_or(c.t1)
                && c.width == width
                && c.data_gen == data_gen
                && q.t0.is_some()
                && q.t1.is_some()
            {
                return TimelineBody {
                    lod: c.lod,
                    width,
                    height: c.height,
                    lane_count: c.lane_count,
                    instance_count: c.instance_count,
                    instances: c.instances.clone(),
                };
            }
        }
    }
    let index = svc.cached_index();
    let (auto0, auto1) = index.time_bounds().unwrap_or((0, 1));
    let t0 = q.t0.unwrap_or(auto0);
    let t1 = q.t1.unwrap_or(auto1.max(t0 + 1));
    {
        let cache = svc.timeline_cache.lock();
        if let Some(c) = cache.as_ref() {
            if c.t0 == t0 && c.t1 == t1 && c.width == width && c.data_gen == data_gen {
                return TimelineBody {
                    lod: c.lod,
                    width,
                    height: c.height,
                    lane_count: c.lane_count,
                    instance_count: c.instance_count,
                    instances: c.instances.clone(),
                };
            }
        }
    }
    let lod = choose_lod(&index, t0, t1, width as usize, INSTANCE_MIN_PX);
    let height = stack_height(&index).ceil() as u32;
    let intern = svc.intern.lock();
    let instances = if lod == TimelineLod::Instanced {
        let frame = collect_instances(&index, t0, t1, width as f32, 0.0, Some(&*intern));
        let instances = frame
            .instances
            .iter()
            .map(|i| InstanceJson {
                x: i.x,
                y: i.y,
                w: i.w,
                h: i.h,
                color: argb_to_css(i.color),
                r: i.radius,
            })
            .collect();
        instances
    } else {
        Vec::new()
    };
    drop(intern);
    let body = TimelineBody {
        lod: lod.as_str(),
        width,
        height,
        lane_count: index.lane_count() as u32,
        instance_count: instances.len() as u32,
        instances,
    };
    *svc.timeline_cache.lock() = Some(crate::CachedTimeline {
        t0,
        t1,
        width,
        data_gen,
        lod: body.lod,
        height: body.height,
        lane_count: body.lane_count,
        instance_count: body.instance_count,
        instances: body.instances.clone(),
    });
    body
}

async fn timeline(
    State(svc): State<Arc<LiveService>>,
    Query(q): Query<FrameQuery>,
) -> Response {
    blocking(move || Json(timeline_body(&svc, q)).into_response()).await
}

#[cfg(test)]
mod isolation_tests;
