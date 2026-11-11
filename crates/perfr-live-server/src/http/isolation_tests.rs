// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use axum::http::StatusCode;

#[test]
fn coop_coep_ready_for_shared_array_buffer() {
    assert_eq!(
        super::ISOLATION[0],
        ("cross-origin-opener-policy", "same-origin")
    );
    assert_eq!(
        super::ISOLATION[1],
        ("cross-origin-embedder-policy", "require-corp")
    );
    assert_eq!(
        super::ISOLATION[2],
        ("cross-origin-resource-policy", "same-origin")
    );
}

#[test]
fn apply_isolation_sets_all_three_on_any_response() {
    let resp = apply_isolation(StatusCode::OK.into_response());
    let h = resp.headers();
    assert_eq!(h.get("cross-origin-opener-policy").unwrap(), "same-origin");
    assert_eq!(
        h.get("cross-origin-embedder-policy").unwrap(),
        "require-corp"
    );
    assert_eq!(
        h.get("cross-origin-resource-policy").unwrap(),
        "same-origin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sharing_rejects_partial_windows_running_captures_and_cross_origin_requests() {
    let svc = test_service();
    svc.capturing.store(true, std::sync::atomic::Ordering::Relaxed);
    let base = spawn_router(svc).await;
    let post = |path: &str, origin: Option<&str>| {
        let mut cmd = std::process::Command::new("curl");
        cmd.args(["-si", "--max-time", "5", "-X", "POST"]);
        if let Some(origin) = origin { cmd.args(["-H", &format!("Origin: {origin}")]); }
        cmd.arg(format!("{base}{path}"));
        String::from_utf8_lossy(&cmd.output().unwrap().stdout).into_owned()
    };
    assert!(post("/api/capture/share?t0=10", None).contains("400 Bad Request"));
    assert!(post("/api/capture/share", None).contains("409 Conflict"));
    assert!(post("/api/capture/share", Some("https://unrelated.example")).contains("403 Forbidden"));
    // A page whose DNS name was rebound to us has Origin == Host, but not
    // a name we answer sharing for.
    let (_, host) = base.rsplit_once('/').unwrap();
    let rebound = post("/api/capture/share", Some("http://evil.example:1"));
    assert!(rebound.contains("403 Forbidden"), "{rebound}");
    assert!(post("/api/capture/share", Some(&format!("http://{host}"))).contains("409 Conflict"));
}

#[test]
fn share_origins_are_this_service_reached_without_dns() {
    let bind: std::net::SocketAddr = "0.0.0.0:44766".parse().unwrap();
    let ok = |origin: &str, host: &str| super::share_origin_allowed(origin, host, bind);
    assert!(ok("http://127.0.0.1:44766", "127.0.0.1:44766"));
    assert!(ok("http://localhost:44766", "localhost:44766"));
    assert!(ok("http://[::1]:44766", "[::1]:44766"));
    assert!(ok("http://192.168.1.20:44766", "192.168.1.20:44766"));
    assert!(ok("https://0.0.0.0", "0.0.0.0"));
    // Origin and Host agree, but the name could have been rebound.
    assert!(!ok("http://evil.example:44766", "evil.example:44766"));
    assert!(!ok("http://perfr.lan", "perfr.lan"));
    // Origin and Host disagree, or the origin is not a web origin.
    assert!(!ok("http://127.0.0.1:1", "127.0.0.1:44766"));
    assert!(!ok("https://127.0.0.1:44766", "127.0.0.1:44766x"));
    assert!(!ok("null", "127.0.0.1:44766"));
    assert!(!ok("", ""));
}

async fn spawn_router(svc: std::sync::Arc<crate::LiveService>) -> String {
    let app = super::router(svc);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn curl_si(url: &str) -> std::process::Output {
    for _ in 0..40 {
        let attempt = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", url])
            .output()
            .expect("curl -si");
        if attempt.status.success() && !attempt.stdout.is_empty() {
            return attempt;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("curl never reached {url}");
}

fn test_service() -> std::sync::Arc<crate::LiveService> {
    crate::LiveService::new(crate::ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ring_buffer_bytes: 1024 * 32,
        spill_path: None,
        wire: crate::WireFormat::default(),
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_export_serves_arrow_when_a_hook_is_set() {
    let svc = test_service();
    svc.set_capture_export(std::sync::Arc::new(|_fmt, _window| Ok(b"ARROW1_stub_body".to_vec())));
    let base = spawn_router(svc).await;
    let out = curl_si(&format!("{base}/api/capture/export"));
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(text.contains("200 ok"), "status line: {text}");
    assert!(
        text.contains("application/vnd.apache.arrow.file"),
        "content-type missing: {text}"
    );
    assert!(
        text.contains("attachment; filename=\"capture.arrow\""),
        "content-disposition missing: {text}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("ARROW1_stub_body"),
        "body missing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_export_serves_parquet_when_asked() {
    let svc = test_service();
    // The hook sees the format the client asked for.
    svc.set_capture_export(std::sync::Arc::new(|fmt, _window| {
        Ok(format!("PAR1_stub_{fmt}").into_bytes())
    }));
    let base = spawn_router(svc).await;
    let out = curl_si(&format!("{base}/api/capture/export?format=parquet"));
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    let text = raw.to_ascii_lowercase();
    assert!(text.contains("200 ok"), "status line: {text}");
    assert!(text.contains("application/vnd.apache.parquet"), "content-type: {text}");
    assert!(
        text.contains("attachment; filename=\"capture.parquet\""),
        "content-disposition: {text}"
    );
    assert!(raw.contains("PAR1_stub_parquet"), "body missing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_export_rejects_an_unknown_format() {
    let svc = test_service();
    svc.set_capture_export(std::sync::Arc::new(|_, _| Ok(Vec::new())));
    let base = spawn_router(svc).await;
    let out = curl_si(&format!("{base}/api/capture/export?format=csv"));
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(text.contains("400"), "expected 400, got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_windowed_bundle_export_hands_the_window_to_the_hook_and_names_the_slice() {
    let svc = test_service();
    svc.set_capture_export(std::sync::Arc::new(|fmt, window| {
        Ok(format!("{fmt}:{window:?}").into_bytes())
    }));
    let base = spawn_router(svc).await;
    let out = curl_si(&format!("{base}/api/capture/export?format=bundle&t0=900&t1=100"));
    let text = String::from_utf8_lossy(&out.stdout);
    let lower = text.to_ascii_lowercase();
    assert!(lower.contains("200 ok"), "{text}");
    assert!(lower.contains("application/zip"), "{text}");
    assert!(lower.contains("filename=\"capture-slice.perfr.zip\""), "{text}");
    // The window is ordered before the hook sees it.
    assert!(text.contains("bundle:Some((100, 900))"), "{text}");
    // Half a window is a client error, not a whole-capture export.
    let out = curl_si(&format!("{base}/api/capture/export?t0=5"));
    assert!(String::from_utf8_lossy(&out.stdout).contains("400"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_import_posts_the_body_to_the_hook() {
    let svc = test_service();
    svc.set_capture_import(std::sync::Arc::new(|bytes| {
        if bytes == b"PK-good" {
            Ok(r#"{"events":3}"#.to_string())
        } else if bytes == b"PK-busy" {
            Err("busy: a capture is running".to_string())
        } else {
            Err("not a capture".to_string())
        }
    }));
    let base = spawn_router(svc).await;
    let post = |body: &str| {
        let out = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", "-X", "POST", "--data-binary", body])
            .arg(format!("{base}/api/capture/import"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let ok = post("PK-good");
    assert!(ok.to_ascii_lowercase().contains("200 ok"), "{ok}");
    assert!(ok.contains(r#"{"events":3}"#), "{ok}");
    assert!(post("PK-busy").contains("409"), "busy is a conflict");
    assert!(post("garbage").contains("400"), "garbage is a bad request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scope_query_goes_to_the_scope_hooks() {
    let svc = test_service();
    svc.set_sampling_report_scope(std::sync::Arc::new(|id| Ok(format!(r#"{{"scope_id":{id}}}"#))));
    svc.set_sampling_tree_scope(std::sync::Arc::new(|id, mode| Ok(format!(r#"{{"scope_id":{id},"mode":"{mode}"}}"#))));
    let base = spawn_router(svc).await;
    let out = curl_si(&format!("{base}/api/sampling/report?scope=42"));
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#"{"scope_id":42}"#));
    let out = curl_si(&format!("{base}/api/sampling/tree?scope=42&mode=bottom_up"));
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#"{"scope_id":42,"mode":"bottom_up"}"#));
    // Without the hooks, 501 -- not a whole-capture report by accident.
    let base = spawn_router(test_service()).await;
    let out = curl_si(&format!("{base}/api/sampling/report?scope=42"));
    assert!(String::from_utf8_lossy(&out.stdout).contains("501"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_names_the_wire_format_without_deadlocking() {
    let base = spawn_router(test_service()).await;
    let out = curl_si(&format!("{base}/api/status"));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(r#""wire":"packed""#), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_scope_requests_reach_the_hook_typed() {
    use crate::{AgentAction, AgentScope};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<AgentScope>::new()));
    let svc = test_service();
    let log = seen.clone();
    svc.set_agent_scope(std::sync::Arc::new(move |req| {
        log.lock().unwrap().push(req);
        Ok(r#"{"ok":true}"#.to_string())
    }));
    let base = spawn_router(svc).await;
    let post = |body: &str| {
        let out = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", "-X", "POST", "-H", "content-type: application/json", "-d", body])
            .arg(format!("{base}/api/scope"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    assert!(post(r#"{"track":"ci","action":"start","name":"build","timestamp_ns":5}"#).contains("200"));
    assert!(post(r#"{"action":"stop"}"#).contains("200"));
    assert!(post(r#"{"action":"value","name":"tests","value":42.5}"#).contains("200"));
    assert!(post(r#"{"action":"start"}"#).contains("400"), "a start needs a name");
    assert!(post(r#"{"action":"dance","name":"x"}"#).contains("400"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0], AgentScope { track: "ci".into(), action: AgentAction::Start { name: "build".into() }, timestamp_ns: Some(5) });
    assert_eq!(seen[1], AgentScope { track: "agent".into(), action: AgentAction::Stop, timestamp_ns: None });
    assert_eq!(seen[2].action, AgentAction::Value { name: "tests".into(), value: 42.5 });
    assert_eq!(seen.len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_events_land_in_the_ring_with_their_names() {
    use perfr_live_protocol::{decode_frame, LiveFrame};
    let svc = test_service();
    let mut viewer = svc.subscribe();
    let base = spawn_router(svc.clone()).await;
    let post = |body: &str| {
        let out = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", "-X", "POST", "-H", "content-type: application/json", "-d", body])
            .arg(format!("{base}/api/events"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let reply = post(
        r##"{"processes":[{"pid":7001,"name":"#1 Benchmark"}],
            "threads":[{"pid":7001,"tid":8001,"name":"agent-a"}],
            "spans":[{"pid":7001,"tid":8001,"name":"claim","start_ns":100,"duration_ns":50}],
            "instants":[{"pid":7001,"tid":8001,"name":"note","timestamp_ns":120}],
            "values":[{"pid":7001,"tid":7001,"name":"progress","timestamp_ns":130,"value":40}]}"##,
    );
    assert!(reply.contains("200"), "{reply}");
    assert!(reply.contains(r#""accepted":3"#), "{reply}");
    assert!(reply.contains(r#""named":2"#), "{reply}");
    assert!(post("not json").contains("400"));
    assert!(post("{}").contains(r#""accepted":0"#), "an empty batch is fine");

    let (_, events) = svc.ring().snapshot();
    assert_eq!(events.len(), 3);
    assert_eq!(svc.intern.lock().get(events[0].name_id), Some("claim"));

    // Viewers saw the names, the interned strings and the batch.
    let mut kinds = Vec::new();
    while let Ok(bytes) = viewer.try_recv() {
        let (frame, _) = decode_frame(&bytes).expect("frame");
        kinds.push(match frame {
            LiveFrame::ProcessName { pid, name } => format!("process {pid} {name}"),
            LiveFrame::ThreadName { pid, tid, name } => format!("thread {pid} {tid} {name}"),
            LiveFrame::InternedString { text, .. } => format!("intern {text}"),
            LiveFrame::EventBatch { events } => format!("batch {}", events.len()),
            other => format!("{other:?}"),
        });
    }
    assert_eq!(
        kinds,
        vec![
            "process 7001 #1 Benchmark",
            "thread 7001 8001 agent-a",
            "intern claim",
            "intern note",
            "intern progress",
            "batch 3",
        ],
        "{kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_log_lines_reach_the_sink_capped_and_whatever_the_content_type() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<crate::ViewerLogBatch>::new()));
    let svc = test_service();
    let sink = seen.clone();
    svc.set_viewer_log(std::sync::Arc::new(move |batch| sink.lock().unwrap().push(batch)));
    let base = spawn_router(svc).await;
    let post = |content_type: &str, body: &str| {
        let out = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", "-X", "POST", "-H", &format!("content-type: {content_type}"), "-d", body])
            .arg(format!("{base}/api/log"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    // sendBeacon sends text/plain; a JSON content type must work too.
    assert!(post("text/plain;charset=UTF-8", r#"{"page":"7c1e","lines":[{"t_ms":1700000000123,"level":"warn","target":"perfr_live_viewer::net","message":"WebSocket closed"}]}"#).contains("204"));
    let long = "x".repeat(10_000);
    assert!(post("application/json", &format!(r#"{{"page":"a/b<c>d","lines":[{{"level":"info","message":"{long}"}}]}}"#)).contains("204"));
    assert!(post("application/json", "not json").contains("400"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].page, "7c1e");
    assert_eq!(seen[0].lines[0].t_ms, 1_700_000_000_123);
    assert_eq!(seen[0].lines[0].level, "warn");
    assert_eq!(seen[0].lines[0].target, "perfr_live_viewer::net");
    assert_eq!(seen[0].lines[0].message, "WebSocket closed");
    assert_eq!(seen[1].page, "abcd", "the page id is sanitised");
    assert!(seen[1].lines[0].message.len() < 4200, "long messages are cut");
    assert!(seen[1].lines[0].message.ends_with("..."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_empties_the_ring_and_tells_viewers_to_start_over() {
    use perfr_live_protocol::{decode_frame, LiveFrame};
    let svc = test_service();
    svc.push_events(&[perfr_live_event::LiveEvent {
        start_ns: 10, duration_ns: 5, tid: 1, pid: 1, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 1,
    }]);
    svc.set_thread_name(1, 1, "main");
    let mut rx = svc.subscribe();
    let base = spawn_router(svc.clone()).await;
    let out = std::process::Command::new("curl")
        .args(["-si", "--max-time", "5", "-X", "POST"])
        .arg(format!("{base}/api/capture/clear"))
        .output()
        .expect("curl");
    assert!(String::from_utf8_lossy(&out.stdout).to_ascii_lowercase().contains("200 ok"));
    assert_eq!(svc.ring().snapshot().1.len(), 0);
    assert!(svc.capture_names().0.is_empty());
    let mut seen = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        if let Ok((f, _)) = decode_frame(&bytes) {
            seen.push(match f {
                LiveFrame::CaptureStarted { pid, start_ns } => format!("started {pid} {start_ns}"),
                LiveFrame::CaptureFinished => "finished".into(),
                LiveFrame::Status { .. } => "status".into(),
                _ => "other".into(),
            });
        }
    }
    assert!(seen.contains(&"started 0 0".to_string()) && seen.contains(&"finished".to_string()), "{seen:?}");
    // With a hook that says busy, nothing is cleared.
    svc.set_capture_clear(std::sync::Arc::new(|| Err("busy: capturing".into())));
    svc.push_events(&[perfr_live_event::LiveEvent {
        start_ns: 20, duration_ns: 5, tid: 1, pid: 1, kind: 1, depth: 0, extra: 0, _pad: 0, name_id: 1,
    }]);
    let out = std::process::Command::new("curl")
        .args(["-si", "--max-time", "5", "-X", "POST"])
        .arg(format!("{base}/api/capture/clear"))
        .output()
        .expect("curl");
    assert!(String::from_utf8_lossy(&out.stdout).contains("409"));
    assert_eq!(svc.ring().snapshot().1.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_open_posts_the_path_and_window_to_the_hook() {
    let svc = test_service();
    svc.set_capture_open(std::sync::Arc::new(|path, window| Ok(format!("{path}:{window:?}"))));
    let base = spawn_router(svc).await;
    let post = |body: &str| {
        let out = std::process::Command::new("curl")
            .args(["-si", "--max-time", "5", "-X", "POST", "-H", "content-type: application/json", "-d", body])
            .arg(format!("{base}/api/capture/open"))
            .output()
            .expect("curl");
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    // The route only passes on a regular file, so there has to be one.
    let file = std::env::temp_dir().join(format!("perfr-open-hook-test-{}.perfr.zip", std::process::id()));
    std::fs::write(&file, b"stands in for a bundle").unwrap();
    let path = file.to_str().unwrap();
    let ok = post(&serde_json::json!({"path": path, "t0": 900, "t1": 100}).to_string());
    assert!(ok.contains(&format!("{path}:Some((100, 900))")), "{ok}");
    let whole = post(&serde_json::json!({"path": path}).to_string());
    assert!(whole.contains(&format!("{path}:None")), "{whole}");
    assert!(post(&serde_json::json!({"path": path, "t0": 5}).to_string()).contains("400"));
    let _ = std::fs::remove_file(&file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_import_is_501_without_a_hook() {
    let base = spawn_router(test_service()).await;
    let out = std::process::Command::new("curl")
        .args(["-si", "--max-time", "5", "-X", "POST", "--data-binary", "x"])
        .arg(format!("{base}/api/capture/import"))
        .output()
        .expect("curl");
    assert!(String::from_utf8_lossy(&out.stdout).contains("501"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_export_is_501_without_a_hook() {
    let base = spawn_router(test_service()).await;
    let out = curl_si(&format!("{base}/api/capture/export"));
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    assert!(text.contains("501"), "expected 501, got: {text}");
}

#[test]
fn parse_ranges_round_trips_the_viewer_query() {
    // Mirrors perfr-live-viewer's ranges_query output.
    let got = super::parse_ranges("100-200:7,500-800");
    assert_eq!(got, vec![(100, 200, Some(7)), (500, 800, None)]);
}

#[test]
fn parse_ranges_orders_endpoints_and_skips_garbage() {
    // Reversed window is normalised; a stray empty entry is dropped.
    let got = super::parse_ranges("300-100,,notarange,50-60:9");
    assert_eq!(got, vec![(100, 300, None), (50, 60, Some(9))]);
}

#[test]
fn apply_isolation_covers_worker_snippet_404s() {
    // Missing snippet URL must still carry isolation so a Worker
    // fetch is not a COEP violation on the error response.
    let resp = apply_isolation(StatusCode::NOT_FOUND.into_response());
    assert_eq!(
        resp.headers().get("cross-origin-embedder-policy").unwrap(),
        "require-corp"
    );
    assert_eq!(
        resp.headers().get("cross-origin-resource-policy").unwrap(),
        "same-origin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn function_stats_counts_hooks_and_skips_sampled_frames() {
    use perfr_live_event::{event_flags, extra, kind, LiveEvent};
    let svc = test_service();
    let name = svc.intern_string("inner");
    let event = |dur, extra_b, pad| LiveEvent {
        start_ns: 1,
        duration_ns: dur,
        tid: 1,
        pid: 9,
        kind: kind::FUNCTION_CALL,
        depth: 0,
        extra: extra_b,
        _pad: pad,
        name_id: name,
    };
    svc.push_events(&[
        event(100, 0, event_flags::DYNAMIC),
        event(300, 0, event_flags::DYNAMIC),
        event(1_000_000, extra::SAMPLED_FRAME, 0),
    ]);
    let base = spawn_router(svc).await;
    let out = std::process::Command::new("curl")
        .args(["-s", "--max-time", "5", &format!("{base}/api/functions/stats?q=inner")])
        .output()
        .expect("curl");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(body["total"], 1);
    assert_eq!(body["rows"][0]["kind"], "hook");
    assert_eq!(body["rows"][0]["count"], 2);
    assert_eq!(body["rows"][0]["avg_ns"], 200);
    assert_eq!(body["rows"][0]["name"], "inner");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_are_listed_by_window_and_grouped_by_the_scope_they_were_logged_in() {
    use perfr_live_event::{event_flags, kind, LiveEvent};
    let svc = test_service();
    let frame = svc.intern_string("frame");
    let load = svc.intern_string("load");
    let tick = svc.intern_string("tick");
    let slow = svc.intern_string("slow frame");
    let outside = svc.intern_string("between frames");
    let event = |kind_id, start_ns, duration_ns, tid, extra, pad, name_id| LiveEvent {
        start_ns,
        duration_ns,
        tid,
        pid: 9,
        kind: kind_id,
        depth: 0,
        extra,
        _pad: pad,
        name_id,
    };
    let mut events = Vec::new();
    // Ten frames on thread 1, each logging "tick"; frame 3 also warns.
    for i in 0..10u64 {
        let start = 1_000 + i * 100;
        events.push(event(kind::API_SCOPE, start, 50, 1, 0, 0, frame));
        events.push(event(kind::LOG, start + 10, 0, 1, 2, 0, tick));
        events.push(event(kind::LOG, start + 70, 0, 1, 1, 0, outside));
    }
    events.push(event(kind::LOG, 1_320, 0, 1, 3, event_flags::TRUNCATED, slow));
    // Another scope, and the same text on another thread.
    events.push(event(kind::API_SCOPE, 5_000, 100, 1, 0, 0, load));
    events.push(event(kind::LOG, 5_050, 0, 1, 2, 0, tick));
    events.push(event(kind::LOG, 1_010, 0, 2, 2, 0, tick));
    svc.push_events(&events);
    let base = spawn_router(svc).await;
    let get = |query: &str| -> serde_json::Value {
        let out = std::process::Command::new("curl")
            .args(["-s", "--max-time", "5", &format!("{base}/api/logs?{query}")])
            .output()
            .expect("curl");
        serde_json::from_slice(&out.stdout).expect("json")
    };

    // No scope: the lines in the window, in time order, one row each.
    let window = get("t0=1000&t1=1099&limit=200");
    assert_eq!(window["grouped"], false);
    assert_eq!(window["total"], 3);
    let rows = window["logs"].as_array().unwrap();
    assert_eq!(rows[0]["message"], "tick");
    assert_eq!(rows[0]["level"], "info");
    assert_eq!(rows[0]["t_ns"], 1_010);
    assert_eq!(rows[2]["message"], "between frames");
    assert_eq!(rows[2]["level"], "debug");
    assert_eq!(get("t0=1000&t1=1099&tid=2")["total"], 1);

    // A scope name: only what was logged inside it, on its thread,
    // identical lines folded into one row with a count.
    let scoped = get("scope=frame");
    assert_eq!(scoped["grouped"], true);
    assert_eq!(scoped["total"], 2);
    let rows = scoped["logs"].as_array().unwrap();
    assert_eq!(rows[0]["message"], "tick");
    assert_eq!(rows[0]["count"], 10, "not the tick in load, nor the one on thread 2");
    assert_eq!(rows[0]["t_ns"], 1_010);
    assert_eq!(rows[0]["last_ns"], 1_910);
    assert_eq!(rows[1]["message"], "slow frame");
    assert_eq!(rows[1]["level"], "warn");
    assert_eq!(rows[1]["truncated"], true);

    // With a scope the time range still applies: frames 2 to 4 only.
    let ranged = get("scope=frame&t0=1200&t1=1450");
    assert_eq!(ranged["logs"][0]["count"], 3);
    assert_eq!(ranged["logs"][0]["t_ns"], 1_210);
    assert_eq!(ranged["logs"][0]["last_ns"], 1_410);
    assert_eq!(ranged["total"], 2);

    assert_eq!(get("scope=load")["logs"][0]["count"], 1);
    assert_eq!(get("scope=no-such-scope")["total"], 0);
}
