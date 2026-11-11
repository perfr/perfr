// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use perfr_live_event::{kind, LiveEvent, LIVE_EVENT_SIZE};
use perfr_live_protocol::{decode_all, LiveFrame, VERSION};
use perfr_live_ring::read_spill_file;

use crate::{LiveService, ServerConfig};

fn small_cfg() -> ServerConfig {
    ServerConfig {
        ring_buffer_bytes: (LIVE_EVENT_SIZE * 1024) as u64,
        ..ServerConfig::default()
    }
}

fn ev(i: u64) -> LiveEvent {
    LiveEvent {
        start_ns: i * 10,
        duration_ns: 4,
        tid: 1,
        pid: 1,
        kind: kind::API_SCOPE,
        depth: 0,
        extra: 0,
        _pad: 0,
        name_id: i as u32,
    }
}

#[test]
fn snapshot_frames_are_decodable_hello_status_and_events() {
    let svc = LiveService::new(small_cfg()).unwrap();
    svc.intern_string("main");
    svc.push_events(&[ev(1), ev(2), ev(3)]);
    let mut bytes = Vec::new();
    for frame in svc.hello_and_snapshot_frames() {
        bytes.extend_from_slice(&frame);
    }
    let frames = decode_all(&bytes).unwrap();
    assert!(matches!(
        frames[0],
        LiveFrame::Hello {
            version: VERSION,
            ..
        }
    ));
    assert!(frames
        .iter()
        .any(|f| matches!(f, LiveFrame::InternedString { text, .. } if text == "main")));
    let events: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            LiveFrame::EventBatch { events } => Some(events.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].name_id, 3);
}

#[test]
fn paired_api_scopes_become_duration_events_on_the_stream() {
    let svc = LiveService::new(small_cfg()).unwrap();
    let name = svc.intern_string("scope");
    svc.ingest_scope_start(1, 9, 100, 0, name);
    svc.ingest_scope_stop(1, 9, 180);
    let (_, snap) = svc.ring().snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].duration_ns, 80);
    assert_eq!(snap[0].kind, kind::API_SCOPE);
}

#[test]
fn spill_from_service_ring_does_not_corrupt_live_snapshot() {
    let dir = std::env::temp_dir().join(format!("perfr-live-svc-spill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = ServerConfig::default();
    cfg.ring_buffer_bytes = (perfr_live_event::LIVE_EVENT_SIZE * 4) as u64;
    cfg.spill_path = Some(dir.clone());
    let svc = LiveService::new(cfg).unwrap();
    for i in 0..10 {
        svc.push_event(ev(i));
    }
    svc.ring().flush_spill().unwrap();
    let (_, live) = svc.ring().snapshot();
    assert_eq!(live.len(), 4);
    assert_eq!(live[0].name_id, 6);
    let spilled = read_spill_file(&svc.ring().spill_path().unwrap()).unwrap();
    assert_eq!(spilled.len(), 6);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn timeline_uses_instanced_lod_for_wide_perfr_colored_scopes() {
    let svc = LiveService::new(small_cfg()).unwrap();
    svc.push_events(&[LiveEvent {
        start_ns: 0,
        duration_ns: 50_000_000,
        tid: 100,
        pid: 1,
        kind: kind::API_SCOPE,
        depth: 1,
        extra: 0,
        _pad: 0,
        name_id: 1,
    }]);
    let index = svc.build_index();
    assert_eq!(
        perfr_live_render::choose_lod(
            &index,
            0,
            50_000_000,
            200,
            perfr_live_render::INSTANCE_MIN_PX
        ),
        perfr_live_render::TimelineLod::Instanced
    );
    assert_eq!(
        index.lanes().next().unwrap().1.events()[0].color_rgba(),
        perfr_live_event::named_scope_color(&1u32.to_le_bytes(), 1)
    );
}

#[test]
fn frame_body_is_16_byte_header_plus_exact_rgba() {
    let svc = LiveService::new(small_cfg()).unwrap();
    svc.push_events(&[ev(1), ev(2)]);
    let raster = svc.rasterize_frame(Some(0), Some(40), 32);
    let body = crate::http::encode_raster_body(&raster);
    let width = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let lanes = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
    assert_eq!(width, raster.width);
    assert_eq!(lanes, raster.lanes.len());
    assert_eq!(body.len(), 16 + width * lanes * 4);
    assert_eq!(&body[16..], raster.to_rgba8());
}

#[test]
fn demo_thread_names_are_interned_for_the_rail() {
    let svc = LiveService::new(small_cfg()).unwrap();
    svc.intern_id(100, "Main");
    svc.intern_id(101, "Worker-1");
    let intern = svc.hello_and_snapshot_frames();
    let mut bytes = Vec::new();
    for frame in intern {
        bytes.extend_from_slice(&frame);
    }
    let frames = decode_all(&bytes).unwrap();
    let texts: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            LiveFrame::InternedString { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&"Main"));
    assert!(texts.contains(&"Worker-1"));
}

#[test]
fn demo_scope_names_are_interned_as_words() {
    let svc = LiveService::new(small_cfg()).unwrap();
    crate::demo::intern_demo_names(&svc);
    let intern = svc.intern.lock();
    assert_eq!(intern.get(100), Some("Main"));
    assert_eq!(intern.get(10), Some("Async"));
    assert_eq!(intern.get(crate::demo::DEMO_SCOPE_BASE), Some("Tick"));
    assert_eq!(intern.get(crate::demo::DEMO_SCOPE_BASE + 1), Some("Simulate"));
    assert_eq!(intern.get(crate::demo::DEMO_ASYNC_BASE), Some("GpuSubmit"));
    assert_eq!(intern.get(crate::demo::DEMO_ASYNC_BASE + 3), Some("Encode"));
    assert_eq!(intern.get(600), Some("values"));
    assert_eq!(intern.get(5_100), Some("sine"));
    assert_eq!(intern.get(5_101), Some("cosine"));
    assert_eq!(
        intern.get(crate::demo::scope_name_id(0, 0, 0)),
        Some("Tick")
    );
}

#[test]
fn demo_stop_clears_status_flag_and_allows_restart() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let svc = LiveService::new(small_cfg()).unwrap();
        crate::demo::start(&svc, 1_000).unwrap();
        assert!(svc.demo.load(std::sync::atomic::Ordering::Relaxed));
        crate::demo::stop(&svc);
        assert!(
            !svc.demo.load(std::sync::atomic::Ordering::Relaxed),
            "stop must clear status.demo immediately"
        );
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert!(!svc.demo.load(std::sync::atomic::Ordering::Relaxed));
        crate::demo::start(&svc, 1_000).expect("stop must release the producer");
        crate::demo::stop(&svc);
    });
}

#[test]
fn demo_emits_three_processes_not_self_pids() {
    let svc = LiveService::new(small_cfg()).unwrap();
    crate::demo::intern_demo_names(&svc);
    let json = crate::demo::process_list_json();
    let list: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let pids: Vec<u64> = list
        .iter()
        .map(|v| v["pid"].as_u64().expect("pid"))
        .collect();
    assert!(json.contains("perfr-demo"));
    assert!(json.contains("perfr-render"));
    assert!(json.contains("perfr-audio"));
    assert!(pids.contains(&1));
    assert!(pids.contains(&10));
    assert!(pids.contains(&11));
    assert!(pids.contains(&20));
    assert!(pids.contains(&21));
    assert!(!pids.contains(&2), "must not reuse reserved viewer pid");
    assert!(!pids.contains(&3), "must not reuse reserved service pid");
}

#[test]
fn capture_start_json_includes_sampling_and_hooks() {
    let body = crate::http::StartBody {
        pid: 42,
        enable_api: true,
        context_switches: true,
        thread_states: true,
        sampling: true,
        samples_per_second: 1000.0,
        unwinding: "dwarf".into(),
        dynamic_instrumentation_method: "user_space".into(),
        instrumented_functions: vec![crate::http::InstrumentedFnRef { function_id: 7 }],
        show_all_processes: false,
        uprobe_duplicate_filter: true,
        max_hook_calls_per_s: None,
        auto_profile: false,
    };
    let json = body.to_json();
    assert!(json.contains("\"pid\":42"));
    assert!(json.contains("\"show_all_processes\":false"));
    assert!(json.contains("\"samples_per_second\":1000"));
    assert!(json.contains("\"unwinding\":\"dwarf\""));
    assert!(json.contains("\"function_id\":7"));
    assert!(!json.contains("elf"));
}

#[test]
fn status_reports_hooks_false_without_control_hooks() {
    let svc = LiveService::new(small_cfg()).unwrap();
    assert!(!svc.has_hooks());
}

#[test]
fn capture_stop_without_hooks_stops_demo() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let svc = LiveService::new(small_cfg()).unwrap();
        crate::demo::start(&svc, 50_000).unwrap();
        assert!(svc.demo.load(std::sync::atomic::Ordering::Relaxed));
        crate::demo::stop(&svc);
        svc.mark_capture_finished();
        assert!(!svc.demo.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!svc.capturing.load(std::sync::atomic::Ordering::Relaxed));
    });
}

#[test]
fn sample_stack_ingest_paints_named_function_calls() {
    let svc = LiveService::new(small_cfg()).unwrap();
    let root = svc.intern_string("main");
    let leaf = svc.intern_string("foo::Bar");
    let evs = svc
        .pairer
        .lock()
        .sample_stack(9, 11, 1_000, 1_000_000, &[root, leaf]);
    svc.push_events(&evs);
    let snap = svc.ring().snapshot().1;
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].kind, kind::FUNCTION_CALL);
    assert_eq!(snap[0].name_id, root);
    assert_eq!(snap[0].depth, 0);
    assert_eq!(snap[1].name_id, leaf);
    assert_eq!(snap[1].depth, 1);
    assert_eq!(snap[0].duration_ns, 1_000_000);
}

#[test]
fn function_call_ingest_uses_interned_pretty_name() {
    let svc = LiveService::new(small_cfg()).unwrap();
    let name_id = svc.intern_string("HookMe");
    let ev = svc
        .pairer
        .lock()
        .function_call(1, 2, name_id, 40, 140, 0);
    svc.push_event(ev);
    let snap = svc.ring().snapshot().1;
    assert_eq!(snap[0].name_id, name_id);
    assert_eq!(svc.intern.lock().get(snap[0].name_id), Some("HookMe"));
}

#[test]
fn thread_and_process_names_are_replayed_to_a_late_subscriber() {
    use perfr_live_protocol::{decode_frame, LiveFrame};
    let svc = crate::LiveService::new(crate::ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ring_buffer_bytes: 1 << 20,
        spill_path: None,
        wire: crate::WireFormat::default(),
    })
    .unwrap();
    svc.set_thread_name(7, 70, "Worker-1");
    svc.set_process_name(7, "game");
    let mut seen_thread = false;
    let mut seen_process = false;
    for bytes in svc.hello_and_snapshot_frames() {
        let (frame, _) = decode_frame(&bytes).unwrap();
        match frame {
            LiveFrame::ThreadName { pid, tid, name } => {
                assert_eq!((pid, tid, name.as_str()), (7, 70, "Worker-1"));
                seen_thread = true;
            }
            LiveFrame::ProcessName { pid, name } => {
                assert_eq!((pid, name.as_str()), (7, "game"));
                seen_process = true;
            }
            _ => {}
        }
    }
    assert!(seen_thread && seen_process);
    let (threads, processes) = svc.capture_names();
    assert_eq!(threads, vec![((7, 70), "Worker-1".to_string())]);
    assert_eq!(processes, vec![(7, "game".to_string())]);
    svc.clear_names();
    assert!(svc.capture_names().0.is_empty());
}

#[test]
fn nothing_from_before_the_capture_start_reaches_the_ring() {
    let svc = LiveService::new(small_cfg()).unwrap();
    // The handler's provisional start, then the loop's real one.
    svc.mark_capture_started(7, 0);
    assert_eq!(svc.capture_start_ns(), 0);
    svc.mark_capture_started(7, 1_000);
    assert_eq!(svc.capture_start_ns(), 1_000);
    // A later provisional 0 (a second Record) does not forget the clock.
    svc.mark_capture_started(7, 0);
    assert_eq!(svc.capture_start_ns(), 1_000);
    svc.push_event(ev(50)); // start 500: before
    svc.push_events(&[ev(90), ev(100), ev(200)]); // 900 before, 1000 and 2000 in
    assert_eq!(svc.stats().events_live, 2);
    assert_eq!(svc.dropped_before_start(), 2);
    assert_eq!(svc.stats().oldest_start_ns, 1_000);
    // A scope opened before the capture and closed inside it is not paired.
    svc.ingest_scope_start(7, 1, 900, 0, 3);
    svc.ingest_scope_stop(7, 1, 1_500);
    assert_eq!(svc.stats().events_live, 2, "the straddling scope was refused");
    assert_eq!(svc.dropped_before_start(), 3);
    svc.ingest_scope_start(7, 1, 1_100, 0, 3);
    svc.ingest_scope_stop(7, 1, 1_500);
    assert_eq!(svc.stats().events_live, 3, "a scope inside the capture lands");
    // Clear forgets the clock; a capture with no known start guards nothing.
    svc.clear_capture().unwrap();
    assert_eq!(svc.capture_start_ns(), 0);
    svc.push_event(ev(50));
    assert_eq!(svc.stats().events_live, 1);
    assert_eq!(svc.dropped_before_start(), 0);
}

#[test]
fn restoring_stored_events_keeps_those_that_begin_before_the_capture_start() {
    let span = |start_ns: u64, end_ns: u64| LiveEvent { start_ns, duration_ns: end_ns - start_ns, ..ev(0) };
    let svc = LiveService::new(small_cfg()).unwrap();
    let mut rx = svc.subscribe();
    // A capture cut to [100, 200] and opened again: its start is the
    // window's, and the events overlapping the window were stored whole.
    svc.mark_capture_started(7, 100);
    let stored = [span(10, 900), span(50, 150), span(120, 130), span(190, 260)];
    let generation = svc.data_gen();
    svc.restore_events(&stored);
    assert_eq!(svc.ring().snapshot().1, stored);
    assert_eq!(svc.dropped_before_start(), 0);
    // The rest of what a push does still happens.
    assert_eq!(svc.data_gen(), generation + 1);
    // The capture ends where the events that begin in it do: the one that
    // spans the window is held, and does not stretch the capture to 900.
    assert_eq!(svc.live_end_ns(), 260);
    assert_eq!(svc.cached_index().event_count(), 4);
    assert_eq!(svc.cached_index().time_bounds(), Some((120, 260)));
    let mut broadcast = Vec::new();
    while let Ok(bytes) = rx.try_recv() {
        for frame in decode_all(&bytes).unwrap() {
            if let LiveFrame::EventBatch { events } = frame {
                broadcast.extend(events);
            }
        }
    }
    assert_eq!(broadcast, stored);
    // A live push is still held to the capture's start.
    svc.push_events(&[span(50, 150), span(150, 160)]);
    assert_eq!(svc.stats().events_live, 5);
    assert_eq!(svc.dropped_before_start(), 1);
    svc.push_event(span(50, 150));
    assert_eq!(svc.stats().events_live, 5);
    assert_eq!(svc.dropped_before_start(), 2);
}

#[test]
fn a_provisional_start_after_the_real_one_is_not_broadcast() {
    let svc = LiveService::new(small_cfg()).unwrap();
    let mut rx = svc.subscribe();
    let started = |rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>| {
        let mut starts = Vec::new();
        while let Ok(bytes) = rx.try_recv() {
            for frame in decode_all(&bytes).unwrap() {
                if let LiveFrame::CaptureStarted { start_ns, .. } = frame {
                    starts.push(start_ns);
                }
            }
        }
        starts
    };
    // The usual order: the handler's 0, then the loop's clock. Both reach
    // the viewers, which take the second as the capture's origin.
    svc.mark_capture_started(7, 0);
    svc.mark_capture_started(7, 1_000);
    assert_eq!(started(&mut rx), vec![0, 1_000]);
    // The race lost: the loop marked first. The handler's 0 would have
    // told every viewer to start over at origin 0; it is swallowed.
    svc.mark_capture_finished();
    svc.mark_capture_started(7, 2_000);
    svc.push_events(&[ev(300)]);
    svc.mark_capture_started(7, 0);
    assert_eq!(started(&mut rx), vec![2_000]);
    assert_eq!(svc.capture_start_ns(), 2_000);
    assert_eq!(svc.live_end_ns(), 3_004, "the live edge was not reset by the late 0");
    assert!(svc.is_capturing());
}

#[test]
fn stream_slice_keeps_events_crossing_both_edges_and_normalizes_bounds() {
    let svc = LiveService::new(small_cfg()).unwrap();
    let mut crossing = ev(1); crossing.duration_ns = 100;
    svc.push_events(&[ev(0), crossing, ev(5), ev(20)]);
    let frames = decode_all(&svc.capture_stream(Some((60, 50)))).unwrap();
    let events: Vec<_> = frames.iter().filter_map(|f| match f {
        LiveFrame::EventBatch { events } => Some(events.clone()), _ => None,
    }).flatten().collect();
    assert_eq!(events, vec![crossing, ev(5)]);
    assert!(matches!(frames.last(), Some(LiveFrame::CaptureFinished)));
}

// ------------------------------------------------- the router, over TCP ---

/// The real router on a loopback port, with the runtime that serves it.
struct Served {
    addr: std::net::SocketAddr,
    _runtime: tokio::runtime::Runtime,
}

fn serve(svc: std::sync::Arc<LiveService>) -> Served {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::http::router(svc);
    runtime.spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Served { addr, _runtime: runtime }
}

/// A service whose configured bind is the port it is served on, as in a
/// real run.
fn serve_small() -> (std::sync::Arc<LiveService>, Served) {
    let svc = LiveService::new(small_cfg()).unwrap();
    let served = serve(svc.clone());
    svc.config.lock().bind = served.addr;
    (svc, served)
}

/// One HTTP/1.1 exchange written out by hand, so that a test decides every
/// header the server sees: `Host` and `Origin` are what the origin rules
/// read, and an HTTP client library would choose them for us. `headers` is
/// sent as given (no `Host` is added). Returns the status and the body.
fn exchange(addr: std::net::SocketAddr, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> (u16, String) {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    let mut request = request.into_bytes();
    request.extend_from_slice(body.unwrap_or_default());
    let (status, _head, body) = send_raw(addr, &request);
    (status, body)
}

/// Sends `request` as it is and returns the reply's status, head and body.
fn send_raw(addr: std::net::SocketAddr, request: &[u8]) -> (u16, String, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    stream.write_all(request).unwrap();
    // The head, then as much body as it announces: an upgraded connection
    // stays open, so reading to the end would wait for the timeout.
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => raw.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line in the reply to {:?}: {head:?}", String::from_utf8_lossy(request)));
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).unwrap();
    (status, head, String::from_utf8_lossy(&body).into_owned())
}

/// The `Host` a client on this machine sends for `addr`.
fn host_of(addr: std::net::SocketAddr) -> String {
    addr.to_string()
}

fn get(served: &Served, path: &str) -> (u16, String) {
    exchange(served.addr, "GET", path, &[("Host", &host_of(served.addr))], None)
}

fn send_json(served: &Served, method: &str, path: &str, body: &str) -> (u16, String) {
    let host = host_of(served.addr);
    exchange(
        served.addr,
        method,
        path,
        &[("Host", &host), ("Content-Type", "application/json")],
        Some(body.as_bytes()),
    )
}

#[test]
fn a_ring_size_outside_the_allowed_range_is_refused_rather_than_allocated() {
    let (svc, served) = serve_small();
    let before = svc.config.lock().ring_buffer_bytes;
    // Sizes whose element count overflows an allocation outright: before
    // the range check these reached `vec![..; n]` and the handler panicked,
    // which in a release build is the end of the service.
    for bytes in [u64::MAX, 1u64 << 62] {
        let (status, text) = send_json(&served, "PUT", "/api/config", &format!(r#"{{"ring_buffer_bytes":{bytes}}}"#));
        assert_eq!(status, 400, "{bytes}: {text}");
        assert!(text.contains("ring_buffer_bytes must be between"), "{text}");
    }
    // Too small to hold a capture worth looking at.
    for bytes in [0u64, 4096, (1 << 20) - 1] {
        let (status, text) = send_json(&served, "PUT", "/api/config", &format!(r#"{{"ring_buffer_bytes":{bytes}}}"#));
        assert_eq!(status, 400, "{bytes}: {text}");
        assert!(text.contains("ring_buffer_bytes must be between"), "{text}");
    }
    assert_eq!(svc.config.lock().ring_buffer_bytes, before, "a refused size changes nothing");
    // The smallest size allowed is taken, and the service is still up.
    let (status, text) = send_json(&served, "PUT", "/api/config", r#"{"ring_buffer_bytes":1048576}"#);
    assert_eq!(status, 200, "{text}");
    let (status, text) = get(&served, "/api/config");
    assert_eq!(status, 200);
    assert!(text.contains(r#""ring_buffer_bytes":1048576"#), "{text}");
}

#[test]
fn the_largest_ring_is_half_of_memory_up_to_eight_gibibytes() {
    use crate::http::{check_ring_bytes, max_ring_bytes, MAX_RING_BYTES, MIN_RING_BYTES};
    const GIB: u64 = 1 << 30;
    assert_eq!(max_ring_bytes(Some(4 * GIB)), 2 * GIB);
    assert_eq!(max_ring_bytes(Some(64 * GIB)), MAX_RING_BYTES);
    assert_eq!(max_ring_bytes(Some(16 * GIB)), MAX_RING_BYTES);
    assert_eq!(max_ring_bytes(None), MAX_RING_BYTES, "unknown memory leaves the fixed cap");
    // A machine so small that half of it is under the floor still takes
    // the one size the floor allows.
    assert_eq!(max_ring_bytes(Some(1 << 20)), MIN_RING_BYTES);
    assert_eq!(max_ring_bytes(Some(0)), MIN_RING_BYTES);

    let max = max_ring_bytes(Some(4 * GIB));
    assert!(check_ring_bytes(MIN_RING_BYTES, max).is_ok());
    assert!(check_ring_bytes(256 << 20, max).is_ok());
    assert!(check_ring_bytes(max, max).is_ok());
    assert!(check_ring_bytes(MIN_RING_BYTES - 1, max).is_err());
    let error = check_ring_bytes(max + 1, max).unwrap_err();
    assert!(error.contains("between 1048576 (1 MiB) and 2147483648 (2048 MiB"), "{error}");
    assert!(error.ends_with(&format!("got {}", max + 1)), "{error}");
}

#[cfg(unix)]
#[test]
fn opening_a_path_that_is_not_a_regular_file_is_refused_before_anything_reads_it() {
    let dir = std::env::temp_dir().join(format!("perfr-open-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let capture = dir.join("capture.perfr.zip");
    std::fs::write(&capture, b"stands in for a bundle").unwrap();
    let empty = dir.join("empty.perfr.zip");
    std::fs::write(&empty, b"").unwrap();
    let fifo = dir.join("fifo.perfr.zip");
    let c_fifo = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a NUL-terminated path that outlives the call.
    assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);

    let (svc, served) = serve_small();
    // The hook stands in for the service's reader, which reads the whole
    // file: on a FIFO it would wait for a writer for good, on /dev/zero it
    // would read until the machine ran out of memory.
    let opened = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let seen = opened.clone();
    svc.set_capture_open(std::sync::Arc::new(move |path, _window| {
        seen.lock().push(path.to_string());
        Ok("{}".to_string())
    }));
    let open = |path: &str| send_json(&served, "POST", "/api/capture/open", &serde_json::json!({ "path": path }).to_string());

    for (path, why) in [
        (fifo.to_str().unwrap(), "not a regular file"),
        ("/dev/zero", "not a regular file"),
        (dir.to_str().unwrap(), "not a regular file"),
        (empty.to_str().unwrap(), "empty"),
        (dir.join("missing.perfr.zip").to_str().unwrap(), "missing.perfr.zip"),
    ] {
        let (status, text) = open(path);
        assert_eq!(status, 400, "{path}: {text}");
        assert!(text.contains(why), "{path}: {text}");
    }
    assert!(opened.lock().is_empty(), "the reader was reached: {:?}", opened.lock());

    let (status, text) = open(capture.to_str().unwrap());
    assert_eq!(status, 200, "{text}");
    assert_eq!(*opened.lock(), vec![capture.to_str().unwrap().to_string()]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_capture_file_over_the_import_limit_is_refused_by_its_size() {
    let dir = std::env::temp_dir().join(format!("perfr-open-size-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture.perfr.zip");
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    let path = path.to_str().unwrap();
    assert!(crate::http::check_capture_file(path, 4096).is_ok());
    let error = crate::http::check_capture_file(path, 4095).unwrap_err();
    assert!(error.contains("4096 bytes") && error.contains("4095"), "{error}");
    std::fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------ who may call the API ---

/// The headers of a WebSocket handshake, less `Host` and `Origin`.
const WS_UPGRADE: [(&str, &str); 4] = [
    ("Connection", "Upgrade"),
    ("Upgrade", "websocket"),
    ("Sec-WebSocket-Version", "13"),
    ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
];

/// A request as a browser page on `origin` sends it to this service at
/// `host`; `origin` is `None` for a client that is not a page (and for a
/// page's own same-origin GETs, which carry no `Origin`).
fn from_page(served: &Served, method: &str, path: &str, host: &str, origin: Option<&str>, body: Option<&str>) -> u16 {
    let mut headers = vec![("Host", host)];
    if let Some(origin) = origin {
        headers.push(("Origin", origin));
    }
    if body.is_some() {
        headers.push(("Content-Type", "application/json"));
    }
    if path == "/ws" || path == "/api/peer" || path == "/api/join" {
        headers.extend(WS_UPGRADE);
    } else if method != "GET" && body.is_none() {
        headers.push(("Content-Length", "0"));
    }
    exchange(served.addr, method, path, &headers, body.map(str::as_bytes)).0
}

#[test]
fn the_viewer_served_by_this_service_reaches_every_kind_of_route() {
    let (svc, served) = serve_small();
    let host = host_of(served.addr);
    let origin = format!("http://{host}");
    // A page's own GETs carry no Origin; its POSTs and its WebSocket do.
    assert_eq!(from_page(&served, "GET", "/api/status", &host, None, None), 200);
    assert_eq!(from_page(&served, "GET", "/api/status", &host, Some(&origin), None), 200);
    assert_eq!(from_page(&served, "POST", "/api/auto_profile", &host, Some(&origin), Some(r#"{"on":false}"#)), 200);
    svc.capturing.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(from_page(&served, "POST", "/api/capture/stop", &host, Some(&origin), None), 200);
    assert!(!svc.is_capturing(), "the stop went through");
    assert_eq!(from_page(&served, "GET", "/ws", &host, Some(&origin), None), 101);
    // The same page opened as localhost: another name for this machine
    // that needs no DNS answer.
    let port = served.addr.port();
    let localhost = format!("localhost:{port}");
    assert_eq!(from_page(&served, "GET", "/ws", &localhost, Some(&format!("http://{localhost}")), None), 101);
    assert_eq!(from_page(&served, "POST", "/api/auto_profile", &localhost, Some(&format!("http://{localhost}")), Some(r#"{"on":false}"#)), 200);
}

#[test]
fn a_page_on_another_origin_is_refused_on_every_kind_of_route() {
    let (svc, served) = serve_small();
    let host = host_of(served.addr);
    for origin in ["https://unrelated.example", "http://127.0.0.1:1", "null"] {
        // A read, a JSON write (which a browser preflights), a write with
        // no body (which it does not), and the three WebSocket routes, to
        // which the browser applies no cross-origin rule at all.
        assert_eq!(from_page(&served, "GET", "/api/status", &host, Some(origin), None), 403, "{origin}");
        assert_eq!(from_page(&served, "GET", "/api/capture/export?format=stream", &host, Some(origin), None), 403, "{origin}");
        assert_eq!(from_page(&served, "POST", "/api/capture/start", &host, Some(origin), Some(r#"{"pid":1}"#)), 403, "{origin}");
        assert_eq!(from_page(&served, "PUT", "/api/config", &host, Some(origin), Some(r#"{"ring_buffer_bytes":2097152}"#)), 403, "{origin}");
        assert_eq!(from_page(&served, "POST", "/api/demo/start", &host, Some(origin), Some("{}")), 403, "{origin}");
        svc.capturing.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(from_page(&served, "POST", "/api/capture/stop", &host, Some(origin), None), 403, "{origin}");
        assert!(svc.is_capturing(), "{origin}: a refused stop stops nothing");
        svc.capturing.store(false, std::sync::atomic::Ordering::Relaxed);
        for ws in ["/ws", "/api/peer", "/api/join"] {
            assert_eq!(from_page(&served, "GET", ws, &host, Some(origin), None), 403, "{origin} {ws}");
        }
        assert_eq!(from_page(&served, "GET", "/", &host, Some(origin), None), 403, "{origin}");
    }
    assert!(!svc.demo.load(std::sync::atomic::Ordering::Relaxed), "no refused request started the demo");
    assert_eq!(svc.config.lock().ring_buffer_bytes, small_cfg().ring_buffer_bytes);
    assert!(svc.peers.list().is_empty(), "no refused handshake became a peer link");
}

#[test]
fn a_preflight_from_another_origin_gets_no_permission() {
    let (_svc, served) = serve_small();
    let host = host_of(served.addr);
    let request = format!(
        "OPTIONS /api/capture/start HTTP/1.1\r\nHost: {host}\r\nOrigin: https://unrelated.example\r\n\
         Access-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\n\r\n"
    );
    let (status, head, _) = send_raw(served.addr, request.as_bytes());
    assert_eq!(status, 403, "{head}");
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"), "{head}");
    // Nor does a reply to anyone say that other origins may read it.
    let (status, head, _) = send_raw(served.addr, format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes());
    assert_eq!(status, 200);
    assert!(!head.to_ascii_lowercase().contains("access-control-allow"), "{head}");
}

#[test]
fn a_name_that_dns_could_have_rebound_is_refused_with_or_without_an_origin() {
    let (svc, served) = serve_small();
    let port = served.addr.port();
    // The page was loaded from evil.example, whose DNS answer then changed
    // to this machine: its requests are same-origin as far as the browser
    // can tell, and its GETs carry no Origin at all.
    let rebound = format!("evil.example:{port}");
    let origin = format!("http://{rebound}");
    assert_eq!(from_page(&served, "GET", "/api/status", &rebound, None, None), 403);
    assert_eq!(from_page(&served, "GET", "/api/capture/export?format=stream", &rebound, None, None), 403);
    assert_eq!(from_page(&served, "GET", "/", &rebound, None, None), 403);
    assert_eq!(from_page(&served, "GET", "/api/status", &rebound, Some(&origin), None), 403);
    assert_eq!(from_page(&served, "POST", "/api/capture/start", &rebound, Some(&origin), Some(r#"{"pid":1}"#)), 403);
    svc.capturing.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(from_page(&served, "POST", "/api/capture/stop", &rebound, Some(&origin), None), 403);
    assert_eq!(from_page(&served, "POST", "/api/capture/stop", &rebound, None, None), 403);
    assert!(svc.is_capturing());
    assert_eq!(from_page(&served, "GET", "/ws", &rebound, Some(&origin), None), 403);
    assert_eq!(from_page(&served, "GET", "/ws", &rebound, None, None), 403);
    assert_eq!(from_page(&served, "GET", "/api/status", "evil.example", None, None), 403);
    assert_eq!(from_page(&served, "GET", "/api/status", "", None, None), 403);
}

#[test]
fn clients_that_are_not_pages_keep_working_without_an_origin() {
    let (svc, served) = serve_small();
    svc.set_machine_name("render-01");
    let port = served.addr.port();
    // curl, the MCP client, the e2e suite: loopback by address or by name.
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}"), "127.0.0.1".to_string()] {
        assert_eq!(from_page(&served, "GET", "/api/status", &host, None, None), 200, "{host}");
        assert_eq!(from_page(&served, "POST", "/api/auto_profile", &host, None, Some(r#"{"on":false}"#)), 200, "{host}");
        assert_eq!(from_page(&served, "POST", "/api/capture/stop", &host, None, None), 200, "{host}");
    }
    // A peer that was told `--connect render-01` or `--join render-01`
    // dials this machine by its own name, and by address otherwise.
    for host in [format!("render-01:{port}"), format!("RENDER-01:{port}"), format!("192.168.1.20:{port}")] {
        assert_eq!(from_page(&served, "GET", "/api/peer", &host, None, None), 101, "{host}");
        assert_eq!(from_page(&served, "GET", "/api/join", &host, None, None), 101, "{host}");
        assert_eq!(from_page(&served, "GET", "/api/status", &host, None, None), 200, "{host}");
    }
    // The viewer opened from another machine as http://render-01:PORT/ (or
    // by the name mDNS gives it) is this service's own page as well.
    for named in [format!("render-01:{port}"), format!("render-01.local:{port}")] {
        let origin = format!("http://{named}");
        assert_eq!(from_page(&served, "GET", "/ws", &named, Some(&origin), None), 101, "{named}");
        assert_eq!(from_page(&served, "POST", "/api/auto_profile", &named, Some(&origin), Some(r#"{"on":false}"#)), 200, "{named}");
        // Not for an upload, though: that leaves the machine, and is asked
        // for by address or as localhost only.
        assert_eq!(from_page(&served, "POST", "/api/capture/share", &named, Some(&origin), None), 403, "{named}");
    }
    // Another page saying it is addressed to that name is still another page.
    let named = format!("render-01:{port}");
    assert_eq!(from_page(&served, "GET", "/ws", &named, Some("https://unrelated.example"), None), 403);
    // HTTP/1.0 has no Host header, and no browser speaks it.
    let (status, _, _) = send_raw(served.addr, b"GET /api/status HTTP/1.0\r\n\r\n");
    assert_eq!(status, 200);
    let (status, _, _) = send_raw(served.addr, b"GET /api/status HTTP/1.0\r\nOrigin: https://unrelated.example\r\n\r\n");
    assert_eq!(status, 403);
}

#[test]
fn the_host_and_origin_rules_by_case() {
    let bind: std::net::SocketAddr = "192.168.1.20:44766".parse().unwrap();
    let ok = |origin: Option<&str>, host: Option<&str>| crate::http::request_allowed(origin, host, bind, "render-01");
    // No Origin: any address, localhost, the bound address, this machine's name.
    assert!(ok(None, Some("127.0.0.1:44766")));
    assert!(ok(None, Some("localhost")));
    assert!(ok(None, Some("LOCALHOST:8080")));
    assert!(ok(None, Some("[::1]:44766")));
    assert!(ok(None, Some("10.1.2.3:9")));
    assert!(ok(None, Some("render-01:44766")));
    assert!(ok(None, Some("Render-01")));
    assert!(ok(None, Some("render-01.local:44766")));
    assert!(!ok(None, Some("render-01.locale")));
    // The MCP client writes an IPv6 host without its brackets.
    assert!(ok(None, Some("::1:44766")));
    assert!(ok(None, None));
    assert!(!ok(None, Some("evil.example:44766")));
    assert!(!ok(None, Some("render-01.evil.example:44766")));
    assert!(!ok(None, Some("render-01x")));
    assert!(!ok(None, Some("")));
    assert!(!ok(None, Some(":44766")));
    // With an Origin: that origin must be this very host, reached without DNS.
    assert!(ok(Some("http://127.0.0.1:44766"), Some("127.0.0.1:44766")));
    assert!(ok(Some("https://localhost:8443"), Some("localhost:8443")));
    assert!(ok(Some("http://192.168.1.20:44766"), Some("192.168.1.20:44766")));
    assert!(!ok(Some("http://127.0.0.1:3000"), Some("127.0.0.1:44766")));
    assert!(!ok(Some("https://unrelated.example"), Some("127.0.0.1:44766")));
    assert!(!ok(Some("http://evil.example:44766"), Some("evil.example:44766")));
    assert!(ok(Some("http://render-01:44766"), Some("render-01:44766")));
    assert!(ok(Some("http://Render-01.local:44766"), Some("Render-01.local:44766")));
    assert!(!ok(Some("http://render-01:3000"), Some("render-01:44766")));
    assert!(!ok(Some("http://render-01.evil.example:44766"), Some("render-01.evil.example:44766")));
    assert!(!ok(Some("http://render-01.local.evil.example:44766"), Some("render-01.local.evil.example:44766")));
    assert!(!ok(Some("http://.local:44766"), Some(".local:44766")));
    assert!(!ok(Some("null"), Some("127.0.0.1:44766")));
    assert!(!ok(Some(""), Some("127.0.0.1:44766")));
    assert!(!ok(Some("http://127.0.0.1:44766"), None));
    // An unnamed machine does not make the empty name acceptable.
    assert!(!crate::http::request_allowed(None, Some(":44766"), bind, ""));
    assert!(!crate::http::request_allowed(None, Some(""), bind, ""));
}
