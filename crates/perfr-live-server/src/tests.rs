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
fn a_config_that_names_no_address_binds_this_machine_only() {
    let bind = ServerConfig::default().bind;
    assert!(bind.ip().is_loopback(), "{bind}");
    assert_eq!(bind.port(), crate::DEFAULT_HTTP_PORT);
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
    let cfg = ServerConfig {
        ring_buffer_bytes: (perfr_live_event::LIVE_EVENT_SIZE * 4) as u64,
        spill_path: Some(dir.clone()),
        ..Default::default()
    };
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
fn a_single_pushed_event_reaches_the_index_and_moves_the_live_edge() {
    let svc = LiveService::new(small_cfg()).unwrap();
    svc.push_events(&[ev(1)]);
    // Something asked for the timeline: the index is built and cached.
    assert_eq!(svc.cached_index().event_count(), 1);
    let generation = svc.data_gen();
    // An agent's scope closes, with no capture running: one event, pushed
    // on its own.
    svc.ingest_scope_start(1, 9, 100, 0, 7);
    svc.ingest_scope_stop(1, 9, 180);
    svc.push_event(ev(30));
    assert_eq!(svc.stats().events_live, 3);
    assert!(svc.data_gen() > generation, "the caches were not told the ring changed");
    assert_eq!(svc.cached_index().event_count(), 3, "the timeline is served from an index without them");
    assert_eq!(svc.live_end_ns(), 304, "the live edge stayed where the batch left it");
}

#[test]
fn thread_and_process_names_are_replayed_to_a_late_subscriber() {
    use perfr_live_protocol::{decode_frame, LiveFrame};
    let svc = crate::LiveService::new(crate::ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        ring_buffer_bytes: 1 << 20,
        spill_path: None,
        wire: crate::WireFormat::default(),
        any_local_user: false,
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
    // 900 before, 1000 and 2000 in: the caller is told how many were kept.
    assert_eq!(svc.push_events(&[ev(90), ev(100), ev(200)]), 2);
    assert_eq!(svc.push_events(&[]), 0);
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
    runtime.spawn(async move {
        let _ = crate::http::serve_on(listener, svc).await;
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
    // This machine's name is no way to a service that listens on loopback
    // only: nothing reaches it by that name, except a page served under
    // the name by someone on the network who then points the name here.
    let named = format!("render-01:{port}");
    assert_eq!(from_page(&served, "GET", "/api/status", &named, None, None), 403);
    assert_eq!(from_page(&served, "GET", "/ws", &named, Some(&format!("http://{named}")), None), 403);
    assert_eq!(from_page(&served, "GET", "/api/status", &format!("render-01.local:{port}"), None, None), 403);
    // On a service that listens on the network it is. (The rule reads the
    // configured address; the listener of this test stays where it is.)
    svc.config.lock().bind = format!("192.168.1.20:{port}").parse().unwrap();
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
    // Bound to loopback, the machine's name is not a way in, and addresses
    // and localhost still are.
    for loopback in ["127.0.0.1:44766", "[::1]:44766"] {
        let bind: std::net::SocketAddr = loopback.parse().unwrap();
        let ok = |origin: Option<&str>, host: Option<&str>| crate::http::request_allowed(origin, host, bind, "render-01");
        assert!(!ok(None, Some("render-01:44766")), "{loopback}");
        assert!(!ok(None, Some("render-01.local:44766")), "{loopback}");
        assert!(!ok(Some("http://render-01:44766"), Some("render-01:44766")), "{loopback}");
        assert!(ok(None, Some("127.0.0.1:44766")), "{loopback}");
        assert!(ok(Some("http://localhost:44766"), Some("localhost:44766")), "{loopback}");
    }
}

#[test]
fn what_the_browser_says_of_the_page_decides_too() {
    use axum::http::Method;
    let allows = |site: Option<&'static str>, mode: Option<&'static str>, dest: Option<&'static str>, method: Method, path: &str| {
        crate::http::FetchMetadata { site, mode, dest }.allows(&method, path)
    };
    // Not a browser, the viewer's own requests, and what the user typed.
    assert!(allows(None, None, None, Method::POST, "/api/capture/start"));
    assert!(allows(Some("same-origin"), Some("cors"), Some("empty"), Method::POST, "/api/capture/start"));
    assert!(allows(Some("same-origin"), Some("websocket"), Some("websocket"), Method::GET, "/ws"));
    assert!(allows(Some("none"), Some("navigate"), Some("document"), Method::GET, "/api/status"));
    // A link to the viewer on another site, followed.
    assert!(allows(Some("cross-site"), Some("navigate"), Some("document"), Method::GET, "/"));
    assert!(allows(Some("same-site"), Some("navigate"), Some("document"), Method::GET, "/index.html"));
    // The same site's image, script, frame and form; and a link straight
    // into the API, which would hand the capture to the browser to save.
    assert!(!allows(Some("cross-site"), Some("no-cors"), Some("image"), Method::GET, "/api/status"));
    assert!(!allows(Some("cross-site"), Some("no-cors"), Some("script"), Method::GET, "/perfr_client.js"));
    assert!(!allows(Some("cross-site"), Some("navigate"), Some("iframe"), Method::GET, "/"));
    assert!(!allows(Some("cross-site"), Some("navigate"), Some("document"), Method::POST, "/"));
    assert!(!allows(Some("cross-site"), Some("navigate"), Some("document"), Method::GET, "/api/capture/export"));
    assert!(!allows(Some("cross-site"), Some("navigate"), Some("document"), Method::GET, "/ws"));
    // Another port of this machine is another site's page all the same,
    // and a value this does not know is not a pass.
    assert!(!allows(Some("same-site"), Some("no-cors"), Some("image"), Method::GET, "/api/status"));
    assert!(!allows(Some("something-new"), Some("cors"), Some("empty"), Method::GET, "/api/status"));
}

#[test]
fn a_page_on_another_site_cannot_spend_the_service_or_frame_the_viewer() {
    let (_svc, served) = serve_small();
    let host = host_of(served.addr);
    let fetch = |path: &str, site: &str, mode: &str, dest: &str| {
        let headers = [("Host", host.as_str()), ("Sec-Fetch-Site", site), ("Sec-Fetch-Mode", mode), ("Sec-Fetch-Dest", dest)];
        exchange(served.addr, "GET", path, &headers, None).0
    };
    // `<img src>` and `<script src>` from another site: no Origin, this
    // machine's address as Host, and until now an answer.
    assert_eq!(fetch("/api/status", "cross-site", "no-cors", "image"), 403);
    assert_eq!(fetch("/api/capture/export?format=stream", "cross-site", "no-cors", "script"), 403);
    assert_eq!(fetch("/api/status", "same-site", "no-cors", "image"), 403, "a page on another port of this machine");
    // The viewer in a frame of that site's page.
    assert_eq!(fetch("/", "cross-site", "navigate", "iframe"), 403);
    // A link to the viewer followed from there, and the viewer's own requests.
    assert_eq!(fetch("/", "cross-site", "navigate", "document"), 200);
    assert_eq!(fetch("/api/status", "same-origin", "cors", "empty"), 200);
    assert_eq!(fetch("/api/status", "none", "navigate", "document"), 200);
}

// ------------------------------------------------------ starting a capture ---

/// Control hooks that do nothing but start and stop: `start` decides
/// whether a Record is accepted.
fn hooks_with_start(start: impl Fn(&str) -> Result<(), String> + Send + Sync + 'static) -> crate::ControlHooks {
    use std::sync::Arc;
    crate::ControlHooks {
        list_processes_json: Arc::new(|| Ok("[]".into())),
        start_capture: Arc::new(start),
        stop_capture: Arc::new(|| Ok(())),
        load_symbols: Arc::new(|_| Ok(())),
        symbols_status_json: Arc::new(|_| Ok("{}".into())),
        search_functions_json: Arc::new(|_, _, _| Ok("{}".into())),
        resolve_functions_json: Arc::new(|_, _| Ok("{}".into())),
        disassemble_json: Arc::new(|_, _| Ok("{}".into())),
        source_json: Arc::new(|_| Ok("{}".into())),
        example_disassembly_json: Arc::new(|| Ok("{}".into())),
    }
}

#[test]
fn a_capture_that_came_and_went_before_record_returned_is_not_marked_running() {
    let (svc, served) = serve_small();
    let mut viewer = svc.subscribe();
    // The capture thread is started by the hook and runs on its own. Here
    // it gets as far as its real start and then gives up (its target is
    // gone already), all before the request that started it is answered.
    let capture = svc.clone();
    svc.set_hooks(hooks_with_start(move |_| {
        capture.mark_capture_started(42, 1_000);
        capture.mark_capture_finished();
        Ok(())
    }));
    assert_eq!(send_json(&served, "POST", "/api/capture/start", r#"{"pid":42}"#).0, 200);
    assert!(!svc.is_capturing(), "marked as capturing with nothing running");
    assert_eq!(svc.capture_pid(), 42);
    let mut marks = Vec::new();
    while let Ok(bytes) = viewer.try_recv() {
        for frame in decode_all(&bytes).unwrap() {
            match frame {
                LiveFrame::CaptureStarted { start_ns, .. } => marks.push(format!("started {start_ns}")),
                LiveFrame::CaptureFinished => marks.push("finished".into()),
                _ => {}
            }
        }
    }
    // No second start, with no clock, after the end: it emptied every
    // viewer and left them waiting for a capture that was over.
    assert_eq!(marks, ["started 1000", "finished"]);

    // The usual order still marks the capture at once, clock or no clock.
    svc.set_hooks(hooks_with_start(|_| Ok(())));
    assert_eq!(send_json(&served, "POST", "/api/capture/start", r#"{"pid":43}"#).0, 200);
    assert!(svc.is_capturing());
    assert_eq!(svc.capture_pid(), 43);
}

#[test]
fn a_record_that_is_refused_leaves_the_running_captures_auto_profiling_alone() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (svc, served) = serve_small();
    // The service's start hook: one capture at a time, and it notes what
    // the flag said when the capture it accepted began, which is what that
    // capture's first pass reads.
    let running = std::sync::Arc::new(AtomicBool::new(false));
    let seen_at_start = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<bool>::new()));
    let (hook_running, hook_seen, hook_svc) = (running.clone(), seen_at_start.clone(), svc.clone());
    svc.set_hooks(hooks_with_start(move |body| {
        if body.contains(r#""pid":666"#) {
            return Err("no such process".into());
        }
        if hook_running.swap(true, Ordering::SeqCst) {
            return Err("a capture is already running".into());
        }
        hook_seen.lock().push(hook_svc.auto_profile());
        Ok(())
    }));
    let start = |body: &str| send_json(&served, "POST", "/api/capture/start", body);

    // A capture that does not auto-profile is running.
    assert_eq!(start(r#"{"pid":1}"#).0, 200);
    assert!(!svc.auto_profile());
    // A second Record that asks for auto-profiling is turned down, and the
    // capture that is running does not start hooking on its account.
    let (status, text) = start(r#"{"pid":1,"auto_profile":true}"#);
    assert_eq!((status, text.as_str()), (409, "a capture is already running"));
    assert!(!svc.auto_profile(), "the refused Record switched auto-profiling on");

    // The other way round: the running capture auto-profiles (switched on
    // from the viewer), and a refused Record without it does not stop that.
    assert_eq!(send_json(&served, "POST", "/api/auto_profile", r#"{"on":true}"#).0, 200);
    assert_eq!(start(r#"{"pid":1}"#).0, 409);
    assert!(svc.auto_profile(), "the refused Record switched auto-profiling off");

    // With nothing running, a Record that fails for a reason of its own
    // leaves the flag as it found it too.
    svc.mark_capture_finished();
    running.store(false, Ordering::SeqCst);
    assert_eq!(send_json(&served, "POST", "/api/auto_profile", r#"{"on":false}"#).0, 200);
    assert_eq!(start(r#"{"pid":666,"auto_profile":true}"#).0, 409);
    assert!(!svc.auto_profile(), "a Record that never started left auto-profiling on");

    // A Record that is accepted has its choice in place before the capture
    // begins, both ways.
    assert_eq!(start(r#"{"pid":1,"auto_profile":true}"#).0, 200);
    assert!(svc.auto_profile());
    svc.mark_capture_finished();
    running.store(false, Ordering::SeqCst);
    assert_eq!(start(r#"{"pid":1}"#).0, 200);
    assert!(!svc.auto_profile());
    assert_eq!(*seen_at_start.lock(), vec![false, true, false]);
}

// ------------------------------------------------------- the live stream ---

/// A viewer on the WebSocket, as far as the stream goes: it holds the
/// events and names it was sent, and starts over where the real one does.
/// Blocking, so that a test decides when it reads.
struct Viewer {
    ws: tokio_tungstenite::tungstenite::WebSocket<std::net::TcpStream>,
    events: Vec<LiveEvent>,
    names: std::collections::HashMap<u32, String>,
    hellos: usize,
    finished: usize,
    statuses: usize,
    /// WebSocket messages received, whatever they carried.
    messages: usize,
}

impl Viewer {
    fn connect(addr: std::net::SocketAddr) -> Viewer {
        Viewer::connect_with(addr, |_| {})
    }

    /// As `connect`, with `prepare` run on the socket before the handshake.
    fn connect_with(addr: std::net::SocketAddr, prepare: impl FnOnce(&std::net::TcpStream)) -> Viewer {
        let stream = std::net::TcpStream::connect(addr).unwrap();
        prepare(&stream);
        // The short timeout is for the poll below, not the handshake. With it
        // already set, a server busy sending another viewer's snapshot makes
        // this read time out and tungstenite reports the handshake interrupted.
        let (mut ws, _) = tokio_tungstenite::tungstenite::client(format!("ws://{addr}/ws"), stream).expect("the handshake");
        ws.get_mut().set_read_timeout(Some(std::time::Duration::from_millis(50))).unwrap();
        Viewer { ws, events: Vec::new(), names: Default::default(), hellos: 0, finished: 0, statuses: 0, messages: 0 }
    }

    fn apply(&mut self, bytes: &[u8]) {
        self.messages += 1;
        for frame in decode_all(bytes).expect("a decodable message") {
            match frame {
                LiveFrame::Status { .. } => self.statuses += 1,
                // A Hello starts a snapshot of the whole ring, a
                // CaptureStarted a new capture: both empty the view.
                LiveFrame::Hello { .. } => {
                    self.hellos += 1;
                    self.events.clear();
                }
                LiveFrame::CaptureStarted { .. } => self.events.clear(),
                LiveFrame::CaptureFinished => self.finished += 1,
                LiveFrame::EventBatch { events } => self.events.extend(events),
                LiveFrame::InternedString { id, text } => {
                    self.names.insert(id, text);
                }
                _ => {}
            }
        }
    }

    /// Reads until `done` says so; panics when that takes over ten seconds.
    fn read_until(&mut self, what: &str, done: impl Fn(&Viewer) -> bool) {
        use tokio_tungstenite::tungstenite::{Error, Message};
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done(self) {
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: not within 10 s ({} events, {} hello, {} finished)",
                self.events.len(),
                self.hellos,
                self.finished
            );
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => self.apply(&bytes),
                Ok(_) => {}
                // The read timeout: nothing to read just now.
                Err(Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(error) => panic!("{what}: the socket failed: {error}"),
            }
        }
    }

    /// Reads until nothing has arrived for `quiet`.
    #[cfg(unix)]
    fn read_until_quiet(&mut self, quiet: std::time::Duration) {
        use tokio_tungstenite::tungstenite::{Error, Message};
        let mut last = std::time::Instant::now();
        while last.elapsed() < quiet {
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => {
                    self.apply(&bytes);
                    last = std::time::Instant::now();
                }
                Ok(_) => {}
                Err(Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(error) => panic!("the socket failed: {error}"),
            }
        }
    }

    /// The name ids of the events held, sorted: every test event has its own.
    fn event_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.events.iter().map(|e| e.name_id).collect();
        ids.sort_unstable();
        ids
    }
}

fn ring_ids(svc: &LiveService) -> Vec<u32> {
    let mut ids: Vec<u32> = svc.ring().snapshot().1.iter().map(|e| e.name_id).collect();
    ids.sort_unstable();
    ids
}

fn serve_with(cfg: ServerConfig) -> (std::sync::Arc<LiveService>, Served) {
    let svc = LiveService::new(cfg).unwrap();
    let served = serve(svc.clone());
    svc.config.lock().bind = served.addr;
    (svc, served)
}

#[test]
fn a_viewer_that_connects_while_events_arrive_misses_nothing_and_gets_nothing_twice() {
    const HELD: u64 = 8_192;
    const LIVE: u64 = 6_000;
    const LAST: u32 = u32::MAX;
    let (svc, served) = serve_with(ServerConfig { ring_buffer_bytes: 1 << 20, ..ServerConfig::default() });
    // What the ring already holds: several messages' worth, so that sending
    // it takes long enough for the producer to get a word in.
    let held: Vec<LiveEvent> = (0..HELD).map(ev).collect();
    svc.push_events(&held);
    let producer = {
        let svc = svc.clone();
        std::thread::spawn(move || {
            for i in HELD..HELD + LIVE {
                // A name now and then, as a capture interns them: one that
                // arrives while a viewer's snapshot is on its way must
                // still reach that viewer.
                if i % 16 == 0 {
                    svc.intern_id(i as u32, &format!("name {i}"));
                }
                svc.push_events(&[ev(i)]);
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
        })
    };
    // Viewers arrive while that goes on (a page reloaded mid-capture).
    let viewers: Vec<_> = (0..4)
        .map(|n| {
            let addr = served.addr;
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(30 * n));
                let mut viewer = Viewer::connect(addr);
                viewer.read_until("the last event", |v| v.events.iter().any(|e| e.name_id == LAST));
                viewer
            })
        })
        .collect();
    producer.join().unwrap();
    svc.push_events(&[LiveEvent { name_id: LAST, ..ev(HELD + LIVE) }]);
    let in_ring = ring_ids(&svc);
    assert_eq!(in_ring.len() as u64, HELD + LIVE + 1, "the ring was sized to hold all of it");
    for (n, viewer) in viewers.into_iter().enumerate() {
        let viewer = viewer.join().unwrap();
        let got = viewer.event_ids();
        let missing = in_ring.iter().filter(|id| got.binary_search(id).is_err()).count();
        assert_eq!(
            (got.len(), missing),
            (in_ring.len(), 0),
            "viewer {n}: holds {} events of the ring's {}, and lacks {missing} of them",
            got.len(),
            in_ring.len()
        );
        assert_eq!(got, in_ring, "viewer {n}");
        for i in (HELD..HELD + LIVE).filter(|i| i % 16 == 0) {
            assert_eq!(viewer.names.get(&(i as u32)).map(String::as_str), Some(format!("name {i}").as_str()), "viewer {n}");
        }
    }
}

/// A viewer whose socket takes little before the server's sends wait for
/// it: one that has stopped reading falls behind the broadcast for certain,
/// instead of having megabytes of slack in the kernel's buffers.
#[cfg(unix)]
fn connect_a_viewer_that_is_slow_to_read(addr: std::net::SocketAddr) -> Viewer {
    Viewer::connect_with(addr, |stream| {
        use std::os::fd::AsRawFd;
        // Not under a loopback segment (64 KiB), or the transfer crawls
        // from one window probe to the next.
        let size: libc::c_int = 256 << 10;
        // SAFETY: the option value is a c_int that outlives the call.
        let done = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(done, 0);
    })
}

/// What opening a stored capture does to the service, as far as viewers can
/// tell: the ring emptied, a start, thousands of names at once, the events
/// restored in chunks. Far more frames than a viewer may fall behind by.
/// Returns without marking the end.
#[cfg(unix)]
fn open_a_capture_larger_than_the_broadcast_slack(svc: &LiveService) {
    svc.clear_ring().unwrap();
    svc.clear_names();
    svc.mark_capture_started(7, 100);
    // Sixteen megabytes on the raw wire: more than the socket of a viewer
    // that is not reading will take.
    let first: Vec<LiveEvent> = (10..500_010).map(ev).collect();
    for chunk in first.chunks(4096) {
        svc.restore_events(chunk);
    }
    for i in 0..3_000u32 {
        svc.intern_id(2_000_000 + i, &format!("frame {i}"));
    }
    let rest: Vec<LiveEvent> = (500_010..520_010).map(ev).collect();
    for chunk in rest.chunks(4096) {
        svc.restore_events(chunk);
    }
}

#[cfg(unix)]
fn assert_the_viewer_holds_the_ring_once(viewer: &Viewer, svc: &LiveService) {
    let (got, in_ring) = (viewer.event_ids(), ring_ids(svc));
    let missing = in_ring.iter().filter(|id| got.binary_search(id).is_err()).count();
    assert_eq!(
        (got.len(), missing),
        (in_ring.len(), 0),
        "the viewer holds {} events of the ring's {}, and lacks {missing} of them",
        got.len(),
        in_ring.len()
    );
    assert_eq!(in_ring.len(), 520_000);
    for i in 0..3_000u32 {
        assert!(viewer.names.contains_key(&(2_000_000 + i)), "frame {i} has no name");
    }
}

#[cfg(unix)]
fn raw_wire_ring_of_a_million() -> ServerConfig {
    ServerConfig { ring_buffer_bytes: 32 << 20, wire: crate::WireFormat::Raw, ..ServerConfig::default() }
}

#[cfg(unix)]
#[test]
fn a_viewer_started_over_after_falling_behind_holds_the_capture_once() {
    let (svc, served) = serve_with(raw_wire_ring_of_a_million());
    svc.push_events(&[ev(1), ev(2)]);
    let mut viewer = connect_a_viewer_that_is_slow_to_read(served.addr);
    viewer.read_until("the first snapshot", |v| v.events.len() == 2);
    // The capture is opened and finished while the viewer reads nothing.
    open_a_capture_larger_than_the_broadcast_slack(&svc);
    svc.mark_capture_finished();
    svc.broadcast_status();
    // It reads again, finds it fell behind, and is started over.
    viewer.read_until("a second snapshot", |v| v.hellos == 2);
    viewer.read_until_quiet(std::time::Duration::from_millis(400));
    assert_the_viewer_holds_the_ring_once(&viewer, &svc);
}

#[cfg(unix)]
#[test]
fn a_viewer_that_falls_behind_while_a_capture_is_being_opened_has_no_holes() {
    let (svc, served) = serve_with(raw_wire_ring_of_a_million());
    svc.push_events(&[ev(1), ev(2)]);
    let mut viewer = connect_a_viewer_that_is_slow_to_read(served.addr);
    viewer.read_until("the first snapshot", |v| v.events.len() == 2);
    open_a_capture_larger_than_the_broadcast_slack(&svc);
    // The viewer reads again before the open is through: the service still
    // says it is capturing when the lag is found.
    assert!(svc.is_capturing());
    viewer.read_until_quiet(std::time::Duration::from_millis(400));
    svc.mark_capture_finished();
    svc.broadcast_status();
    viewer.read_until_quiet(std::time::Duration::from_millis(400));
    assert_the_viewer_holds_the_ring_once(&viewer, &svc);
}

// ------------------------------------------------------- the viewer's log ---

/// A service whose viewer-log sink collects what it is handed.
fn serve_collecting_viewer_logs() -> (std::sync::Arc<LiveService>, Served, std::sync::Arc<parking_lot::Mutex<Vec<crate::ViewerLogBatch>>>) {
    let (svc, served) = serve_small();
    let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink = seen.clone();
    svc.set_viewer_log(std::sync::Arc::new(move |batch| sink.lock().push(batch)));
    (svc, served, seen)
}

fn post_log(served: &Served, body: &serde_json::Value) -> u16 {
    let host = host_of(served.addr);
    // As `navigator.sendBeacon` labels it.
    exchange(served.addr, "POST", "/api/log", &[("Host", &host), ("Content-Type", "text/plain;charset=UTF-8")], Some(body.to_string().as_bytes())).0
}

#[test]
fn a_viewer_log_line_cannot_forge_another_or_reach_the_terminal_raw() {
    let (_svc, served, seen) = serve_collecting_viewer_logs();
    let status = post_log(
        &served,
        &serde_json::json!({"page": "7c1e", "lines": [
            {"level": "warn", "target": "net",
             "message": "socket closed\n[2026-01-01T00:00:00.000000] [ERROR] [serve.rs:1] the service was compromised\r\u{1b}[2Jgone"},
            {"level": "error\n[fake]", "target": "a]\n[2026-01-01T00:00:00.000000] [ERROR] [x] \u{1b}]0;owned\u{7}", "message": "tab\tand \u{9b}31m C1"},
            {"level": "info", "target": "t".repeat(1 << 20), "message": "a long target"},
            // What a real panic message looks like: several lines.
            {"level": "error", "target": "panic", "message": "panicked at src/app.rs:10:5:\nindex out of bounds\n"},
        ]}),
    );
    assert_eq!(status, 204);
    let seen = seen.lock();
    let lines = &seen[0].lines;
    assert_eq!(lines.len(), 4);
    for line in lines {
        for text in [&line.level, &line.target, &line.message] {
            // No character that moves the cursor, clears the screen or sets
            // the title; and no line of the text starts where a log line's
            // `[time] [LEVEL]` does.
            assert!(!text.chars().any(|c| c.is_control() && c != '\n' && c != '\t'), "{text:?}");
            assert!(!text.contains("\n["), "{text:?}");
        }
        assert!(!line.target.contains('\n') && !line.level.contains('\n'), "{line:?}");
        assert!(line.target.len() <= 128, "a target of {} bytes", line.target.len());
    }
    // What was said is still there to read, and a message of several lines
    // is still several lines.
    assert_eq!(lines[0].level, "warn");
    assert!(lines[0].message.starts_with("socket closed\n"), "{:?}", lines[0].message);
    assert!(lines[0].message.contains("the service was compromised\\r\\x1b[2Jgone"), "{:?}", lines[0].message);
    assert_eq!(lines[1].level, "info", "a level that is not one is the default");
    assert_eq!(lines[1].message, "tab\tand \\x9b31m C1");
    assert!(lines[1].target.starts_with("a]\\n[2026"), "{:?}", lines[1].target);
    assert_eq!(lines[3].message, "panicked at src/app.rs:10:5:\n    index out of bounds\n    ");
}

#[test]
fn viewer_log_lines_past_the_budget_are_dropped_and_counted() {
    let (_svc, served, seen) = serve_collecting_viewer_logs();
    let kept = |seen: &parking_lot::Mutex<Vec<crate::ViewerLogBatch>>| -> usize {
        seen.lock().iter().flat_map(|b| &b.lines).map(|l| l.message.len() + l.target.len()).sum()
    };
    // Each request is within the per-request caps; a page that loops sends
    // many. Six megabytes arrive within the second.
    let line = serde_json::json!({"level": "info", "target": "loop", "message": "x".repeat(4000)});
    let body = serde_json::json!({"page": "loop", "lines": vec![line; 380]});
    for _ in 0..4 {
        assert_eq!(post_log(&served, &body), 204, "an over-eager page is not an error");
    }
    let total = kept(&seen);
    // (What refills while the test runs is a few kilobytes a second.)
    assert!(total <= crate::http::VIEWER_LOG_BURST_BYTES + (256 << 10), "{total} bytes of viewer lines reached the log");
    assert!(total >= crate::http::VIEWER_LOG_BURST_BYTES / 2, "{total}: the budget is not that small");
}

#[test]
fn the_viewer_log_budget_refills_and_says_what_it_dropped() {
    use crate::http::{ViewerLogBudget, VIEWER_LOG_BURST_BYTES, VIEWER_LOG_BYTES_PER_MINUTE};
    let start = std::time::Instant::now();
    let at = |secs: u64| start + std::time::Duration::from_secs(secs);
    let mut budget = ViewerLogBudget::new(start);
    assert!(budget.take(at(0), VIEWER_LOG_BURST_BYTES));
    assert!(!budget.take(at(0), 1), "the burst is spent");
    assert!(!budget.take(at(0), 100));
    assert_eq!(budget.take_dropped(), 2);
    assert_eq!(budget.take_dropped(), 0, "reported once");
    // Half a minute later half a minute's worth is back, and no more.
    assert!(budget.take(at(30), VIEWER_LOG_BYTES_PER_MINUTE / 2 - 1));
    assert!(!budget.take(at(30), 4096));
    // However long nothing is logged, the burst is the most that is saved up.
    assert!(!budget.take(at(86_400), VIEWER_LOG_BURST_BYTES + 1));
    assert!(budget.take(at(86_400), VIEWER_LOG_BURST_BYTES));
}

// ------------------------------------------------------------- the bench ---

#[test]
fn a_bench_rate_beyond_the_ceiling_is_brought_down_to_it() {
    use crate::bench::{BenchParams, MAX_RATE};
    let (svc, served) = serve_small();
    // Not started: the rate is the knob alone. At this rate one 20 ms tick
    // would be 2e10 events, hundreds of gigabytes, allocated in one go.
    let (status, text) = send_json(&served, "POST", "/api/bench/rate", r#"{"rate":1000000000000}"#);
    assert_eq!(status, 200, "{text}");
    let reported: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(reported["rate"], MAX_RATE, "{text}");
    assert_eq!(svc.bench.rate.load(std::sync::atomic::Ordering::Relaxed), MAX_RATE);
    // A rate within reach is taken as it is, down to none at all.
    for rate in [0u64, 1, 1_000_000, MAX_RATE] {
        let (_, text) = send_json(&served, "POST", "/api/bench/rate", &format!(r#"{{"rate":{rate}}}"#));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["rate"], rate);
    }
    // The start request carries the same field.
    assert_eq!(BenchParams { rate: u64::MAX, ..BenchParams::default() }.clamped().rate, MAX_RATE);
    assert_eq!(BenchParams::default().clamped().rate, crate::bench::DEFAULT_RATE);
}

// ------------------------------------------------- slow work in a handler ---

#[test]
fn a_request_that_takes_long_does_not_hold_up_the_others() {
    const SLOW: std::time::Duration = std::time::Duration::from_millis(1500);
    let (svc, served) = serve_small();
    // The process list of a busy machine, a symbol search over a large
    // binary: a hook that takes its time. The runtime here has two workers.
    let mut hooks = hooks_with_start(|_| Ok(()));
    hooks.list_processes_json = std::sync::Arc::new(|| {
        std::thread::sleep(SLOW);
        Ok("[]".into())
    });
    svc.set_hooks(hooks);
    let waited = std::thread::scope(|scope| {
        let slow: Vec<_> = (0..2).map(|_| scope.spawn(|| get(&served, "/api/processes"))).collect();
        // Both are in the hook by now.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let asked = std::time::Instant::now();
        let (status, _) = get(&served, "/api/status");
        let waited = asked.elapsed();
        assert_eq!(status, 200);
        for request in slow {
            assert_eq!(request.join().unwrap(), (200, "[]".to_string()));
        }
        waited
    });
    assert!(waited < SLOW / 3, "the status waited {waited:?} behind two slow requests");
}

/// How long the capture thread would wait to intern a name while
/// `/api/capture/targets` is answered over a large capture. Run with
/// `cargo test --release -p perfr-live-server -- --ignored --nocapture targets_bench`.
#[test]
#[ignore]
fn targets_bench() {
    let (svc, served) = serve_with(ServerConfig { ring_buffer_bytes: 128 << 20, ..ServerConfig::default() });
    let events: Vec<LiveEvent> = (0..4_000_000u64)
        .map(|i| LiveEvent { tid: 1 + (i % 64) as u32, depth: (i % 8) as u8, name_id: (i % 5_000) as u32, ..ev(i) })
        .collect();
    for chunk in events.chunks(1 << 16) {
        svc.push_events(chunk);
    }
    for id in 0..5_000u32 {
        svc.intern_id(id, &format!("scope::number_{id}"));
    }
    // Built once; the requests below are served from the cached index.
    svc.cached_index();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut longest = std::time::Duration::ZERO;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let asked = std::time::Instant::now();
                drop(svc.intern.lock());
                longest = longest.max(asked.elapsed());
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
            longest
        });
        let asked = std::time::Instant::now();
        for _ in 0..5 {
            assert_eq!(get(&served, "/api/capture/targets?q=number_4").0, 200);
        }
        let per_request = asked.elapsed() / 5;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "targets over 4,000,000 events: {per_request:?} a request; the intern table was held for up to {:?}",
            waiter.join().unwrap()
        );
    });
}

// -------------------------------------------------- the server-side index ---

/// The index a request is served from, flattened for comparison: per lane,
/// its events in order.
fn index_contents(index: &perfr_live_render::TrackIndex) -> (usize, Option<(u64, u64)>, Vec<(String, Vec<LiveEvent>)>) {
    let lanes = index.lanes().map(|(key, lane)| (format!("{key:?}"), lane.events().to_vec())).collect();
    (index.event_count(), index.time_bounds(), lanes)
}

fn index_from_scratch(svc: &LiveService) -> perfr_live_render::TrackIndex {
    let mut index = perfr_live_render::TrackIndex::default();
    index.set_extent_floor(svc.capture_start_ns());
    index.extend(svc.ring().snapshot().1);
    index
}

#[test]
fn the_index_kept_between_requests_is_the_index_built_from_the_ring() {
    // A ring of 4096 events, so that the sequence below also runs it full.
    let svc = LiveService::new(ServerConfig { ring_buffer_bytes: (LIVE_EVENT_SIZE * 4096) as u64, ..ServerConfig::default() }).unwrap();
    let check = |what: &str| {
        let kept = svc.cached_index();
        assert_eq!(index_contents(&kept), index_contents(&index_from_scratch(&svc)), "{what}");
        kept
    };
    let batch = |from: u64, n: u64| -> Vec<LiveEvent> {
        (from..from + n).map(|i| LiveEvent { tid: 1 + (i % 5) as u32, depth: (i % 3) as u8, ..ev(i) }).collect()
    };
    check("empty");
    svc.push_events(&batch(0, 100));
    check("the first build");
    // More arrives between requests, a little at a time, as in a capture.
    let mut next = 100;
    for step in 0..40 {
        svc.push_events(&batch(next, 37));
        next += 37;
        // An event that starts earlier than what is held: sorted in.
        svc.push_event(LiveEvent { tid: 2, ..ev(next / 2) });
        check(&format!("step {step}"));
    }
    // A request still reading the index it was given is not disturbed by
    // the next one growing it.
    let held = check("held");
    let held_before = index_contents(&held);
    svc.push_events(&batch(next, 50));
    next += 50;
    check("grown while another request holds the old one");
    assert_eq!(index_contents(&held), held_before);
    // The capture's start moves the extent floor.
    svc.mark_capture_started(7, next * 10);
    check("a capture started");
    svc.push_events(&batch(next, 10));
    next += 10;
    check("events of the capture");
    // The ring runs full and evicts: what left the ring leaves the index.
    for step in 0..6 {
        svc.push_events(&batch(next, 1500));
        next += 1500;
        let kept = check(&format!("full ring, step {step}"));
        assert!(kept.event_count() <= 4096);
    }
    assert_eq!(svc.stats().events_live, 4096);
    // A new ring, and a cleared capture.
    svc.clear_ring().unwrap();
    assert_eq!(check("a new ring").event_count(), 0);
    svc.push_events(&batch(next, 20));
    assert_eq!(check("the new ring's events").event_count(), 20);
    svc.clear_capture().unwrap();
    svc.push_events(&batch(0, 3));
    assert_eq!(check("after a clear").event_count(), 3);
}

/// What the server-side timeline, frame and targets routes pay for their
/// index during a capture. Run with
/// `cargo test --release -p perfr-live-server -- --ignored --nocapture index_bench`.
#[test]
#[ignore]
fn index_bench() {
    let svc = LiveService::new(ServerConfig { ring_buffer_bytes: 256 << 20, ..ServerConfig::default() }).unwrap();
    let event = |i: u64| LiveEvent { tid: 1 + (i % 64) as u32, depth: (i % 8) as u8, name_id: (i % 5_000) as u32, ..ev(i) };
    let held: Vec<LiveEvent> = (0..4_000_000u64).map(event).collect();
    for chunk in held.chunks(1 << 16) {
        svc.push_events(chunk);
    }
    let asked = std::time::Instant::now();
    svc.cached_index();
    let first = asked.elapsed();
    // A request a drain pass: 2000 more events each time.
    let mut next = 4_000_000u64;
    let asked = std::time::Instant::now();
    for _ in 0..20 {
        let more: Vec<LiveEvent> = (next..next + 2_000).map(event).collect();
        next += 2_000;
        svc.push_events(&more);
        assert_eq!(svc.cached_index().event_count() as u64, next);
    }
    eprintln!(
        "index over 4,000,000 events: {first:?} to build; {:?} a request with 2,000 new events each",
        asked.elapsed() / 20
    );
}

// ------------------------------------------------------ the viewer's pack ---

/// The value of a response header, from the head `send_raw` returns.
fn header_of(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

#[test]
fn a_viewer_asset_is_served_with_an_etag_and_revalidated_by_it() {
    let (_svc, served) = serve_small();
    let host = host_of(served.addr);
    let fetch = |path: &str, etag: Option<&str>| {
        let condition = etag.map(|tag| format!("If-None-Match: {tag}\r\n")).unwrap_or_default();
        send_raw(served.addr, format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{condition}\r\n").as_bytes())
    };
    for path in ["/", "/perfr_client.js", "/perfr_client_bg.wasm"] {
        let (status, head, _) = fetch(path, None);
        assert_eq!(status, 200, "{path}");
        assert_eq!(header_of(&head, "cache-control").as_deref(), Some("no-cache"), "{path}");
        let etag = header_of(&head, "etag").unwrap_or_else(|| panic!("{path}: no ETag in {head}"));
        assert!(etag.starts_with('"') && etag.len() == 18, "{path}: {etag}");
        // The same bytes, asked for again: the same tag, and with it a 304.
        assert_eq!(header_of(&fetch(path, None).1, "etag").as_deref(), Some(etag.as_str()), "{path}");
        let (status, head, body) = fetch(path, Some(&etag));
        assert_eq!((status, body.len()), (304, 0), "{path}");
        assert_eq!(header_of(&head, "etag").as_deref(), Some(etag.as_str()), "{path}");
        assert_eq!(fetch(path, Some("\"0000000000000000\"")).0, 200, "{path}: another tag is not this one");
    }
    // Two assets do not share a tag.
    let tag = |path: &str| header_of(&fetch(path, None).1, "etag").unwrap();
    assert_ne!(tag("/perfr_client.js"), tag("/index.html"));
    assert_eq!(tag("/"), tag("/index.html"));
}

/// What a page load pays to be told its copy of the viewer is current. Run
/// with `cargo test --release -p perfr-live-server -- --ignored --nocapture etag_bench`.
#[test]
#[ignore]
fn etag_bench() {
    let (_svc, served) = serve_small();
    let host = host_of(served.addr);
    let revalidate = |etag: &str| {
        send_raw(
            served.addr,
            format!("GET /perfr_client_bg.wasm HTTP/1.1\r\nHost: {host}\r\nIf-None-Match: {etag}\r\n\r\n").as_bytes(),
        )
    };
    let etag = header_of(&revalidate("\"\"").1, "etag").unwrap();
    let asked = std::time::Instant::now();
    for _ in 0..50 {
        assert_eq!(revalidate(&etag).0, 304);
    }
    eprintln!("revalidating the wasm: {:?} a request", asked.elapsed() / 50);
}

#[test]
fn a_connecting_viewer_gets_its_names_in_a_few_messages_and_in_order() {
    let (svc, served) = serve_small();
    // A sampled capture's frame names: tens of thousands of short strings.
    for i in 0..30_000u32 {
        svc.intern_id(1_000_000 + i, &format!("module::function_{i}"));
    }
    svc.set_thread_name(7, 70, "Main");
    svc.set_process_name(7, "game");
    svc.push_events(&[ev(1), ev(2), ev(3)]);
    let mut viewer = Viewer::connect(served.addr);
    viewer.read_until("the snapshot", |v| v.events.len() == 3);
    assert_eq!((viewer.hellos, viewer.statuses), (1, 1));
    assert_eq!(viewer.names.len(), 30_000);
    assert_eq!(viewer.names.get(&1_029_999).map(String::as_str), Some("module::function_29999"));
    // One message per name was thirty thousand sends, and as many scopes
    // in the service's own profile, for every viewer that connects.
    assert!(viewer.messages < 100, "{} messages", viewer.messages);
    eprintln!("a snapshot with 30,000 names: {} messages", viewer.messages);
}

/// What connecting costs when the capture has many names. Run with
/// `cargo test --release -p perfr-live-server -- --ignored --nocapture connect_bench`.
#[test]
#[ignore]
fn connect_bench() {
    let (svc, served) = serve_small();
    for i in 0..30_000u32 {
        svc.intern_id(1_000_000 + i, &format!("module::function_{i}"));
    }
    svc.push_events(&[ev(1)]);
    let asked = std::time::Instant::now();
    let mut messages = 0;
    for _ in 0..10 {
        let mut viewer = Viewer::connect(served.addr);
        viewer.read_until("the snapshot", |v| v.events.len() == 1);
        messages = viewer.messages;
    }
    eprintln!("connecting with 30,000 names held: {:?} and {messages} messages a viewer", asked.elapsed() / 10);
}
