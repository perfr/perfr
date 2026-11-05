// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! WebSocket fan-out and the small HTTP API.
//!
//! `clients` is an atomic incremented when a socket is accepted and
//! decremented when that task ends. Publishing a batch does not read it.
//! `GET /status` and the once-a-second Status tail both load the same atomic.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use perfr_live_protocol::RelayStatusExt;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tower_http::cors::CorsLayer;

use crate::control::{
    ControlUpdate, KnobSnapshot, Knobs, Limiter, WsControl, DEFAULT_CONTROL_PER_SEC,
};
use crate::ring::{BroadcastRing, Poll};
use crate::session::{unix_ms, Session, DEFAULT_IDLE, HARD_CAP};
use crate::wire::{self, StatsFields};

const MAX_PREAMBLE: usize = 4096;
const DRAIN_PER_TURN: usize = 8;
const SEND_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub ring_slots: usize,
    pub coalesce_lag: u64,
    pub idle: Duration,
    pub hard_cap: Duration,
    pub control_per_sec: u32,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            ring_slots: 4096,
            coalesce_lag: 4,
            idle: DEFAULT_IDLE,
            hard_cap: HARD_CAP,
            control_per_sec: DEFAULT_CONTROL_PER_SEC,
        }
    }
}

pub struct Relay {
    ring: BroadcastRing,
    notify: Notify,
    clients: AtomicUsize,
    started: Instant,
    events_per_sec: AtomicU64,
    rate_started: AtomicBool,
    demo: AtomicBool,
    idle: Duration,
    hard_cap: Duration,
    knobs: Knobs,
    limiter: Limiter,
    preamble: Mutex<std::collections::VecDeque<Bytes>>,
}

impl Relay {
    pub fn new(config: RelayConfig) -> Arc<Self> {
        Arc::new(Self {
            ring: BroadcastRing::new(config.ring_slots, config.coalesce_lag),
            notify: Notify::new(),
            clients: AtomicUsize::new(0),
            started: Instant::now(),
            events_per_sec: AtomicU64::new(0),
            rate_started: AtomicBool::new(false),
            demo: AtomicBool::new(false),
            idle: config.idle,
            hard_cap: config.hard_cap,
            knobs: Knobs::new(),
            limiter: Limiter::new(config.control_per_sec),
            preamble: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    pub fn set_demo(&self, demo: bool) {
        self.demo.store(demo, Ordering::Relaxed);
    }

    pub fn knobs(&self) -> &Knobs {
        &self.knobs
    }

    pub fn clients(&self) -> usize {
        self.clients.load(Ordering::Relaxed)
    }

    /// Publish one already-serialized batch. This is the capture path: the
    /// synthetic knobs are not consulted.
    pub fn publish_events(&self, bytes: Bytes, events: u32) {
        self.ring.publish(bytes, events);
        self.notify.notify_waiters();
    }

    /// A name, interned string, or capture mark. Stored for clients that
    /// connect later, and published so clients already connected see it.
    /// Still not a synthetic-knob input.
    pub fn publish_control(&self, bytes: Bytes) {
        {
            let mut preamble = self.preamble.lock().unwrap_or_else(|err| err.into_inner());
            if preamble.len() >= MAX_PREAMBLE {
                preamble.pop_front();
            }
            preamble.push_back(Bytes::clone(&bytes));
        }
        self.publish_events(bytes, 0);
    }

    fn preamble_frames(&self) -> Vec<Bytes> {
        self.preamble
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn status_body(&self) -> StatusBody {
        StatusBody {
            clients: self.clients.load(Ordering::Relaxed) as u64,
            events_per_sec: self.events_per_sec.load(Ordering::Relaxed),
            uptime: self.started.elapsed().as_secs_f64(),
        }
    }

    fn stats_frame(&self, session: &Session, dropped: u64) -> Bytes {
        let produced = self.ring.events_published();
        wire::stats(&StatsFields {
            capturing: produced > 0 || self.demo.load(Ordering::Relaxed),
            demo: self.demo.load(Ordering::Relaxed),
            events_live: self.ring.events_live(),
            events_capacity: self.ring.capacity(),
            dropped,
            produced,
            ring_bytes: self.ring.footprint_bytes(),
            ext: RelayStatusExt {
                clients: self.clients.load(Ordering::Relaxed) as u64,
                events_per_sec: self.events_per_sec.load(Ordering::Relaxed),
                uptime_ms: self.started.elapsed().as_millis() as u64,
                session_expires_unix_ms: session.expires_unix_ms,
            },
        })
    }

    fn ensure_rate_task(self: &Arc<Self>) {
        if self.rate_started.swap(true, Ordering::Relaxed) {
            return;
        }
        let relay = Arc::clone(self);
        tokio::spawn(async move {
            let mut last = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let total = relay.ring.events_published();
                relay
                    .events_per_sec
                    .store(total.saturating_sub(last), Ordering::Relaxed);
                last = total;
            }
        });
    }
}

#[derive(Serialize)]
struct StatusBody {
    clients: u64,
    events_per_sec: u64,
    uptime: f64,
}

pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/ws", get(ws))
        .route("/control", get(control_get).post(control_post))
        .layer(CorsLayer::permissive())
        .with_state(relay)
}

pub async fn serve(addr: SocketAddr, relay: Arc<Relay>) -> std::io::Result<()> {
    relay.ensure_rate_task();
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    println!("perfr-relay listening on {bound}");
    axum::serve(listener, router(relay))
        .await
        .map_err(std::io::Error::other)
}

pub async fn serve_ephemeral(relay: Arc<Relay>) -> std::io::Result<SocketAddr> {
    relay.ensure_rate_task();
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(relay)).await;
    });
    Ok(addr)
}

async fn status(State(relay): State<Arc<Relay>>) -> Json<StatusBody> {
    Json(relay.status_body())
}

async fn control_get(State(relay): State<Arc<Relay>>) -> Json<KnobSnapshot> {
    Json(relay.knobs.snapshot())
}

async fn control_post(
    State(relay): State<Arc<Relay>>,
    Json(update): Json<ControlUpdate>,
) -> Response {
    if !relay.limiter.allow() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "rate limited"})),
        )
            .into_response();
    }
    relay.knobs.apply_http(&update);
    Json(relay.knobs.snapshot()).into_response()
}

async fn ws(upgrade: WebSocketUpgrade, State(relay): State<Arc<Relay>>) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| client_loop(socket, relay))
}

struct ClientCount<'a> {
    clients: &'a AtomicUsize,
}

impl<'a> ClientCount<'a> {
    fn enter(clients: &'a AtomicUsize) -> Self {
        clients.fetch_add(1, Ordering::Relaxed);
        Self { clients }
    }
}

impl Drop for ClientCount<'_> {
    fn drop(&mut self) {
        self.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn client_loop(socket: WebSocket, relay: Arc<Relay>) {
    let _count = ClientCount::enter(&relay.clients);
    let mut session = Session::start(Instant::now(), relay.idle, relay.hard_cap, unix_ms());
    let (mut sink, mut stream) = socket.split();
    if !send_bin(&mut sink, wire::hello(session.expires_unix_ms)).await {
        return;
    }
    for frame in relay.preamble_frames() {
        if !send_bin(&mut sink, frame).await {
            return;
        }
    }
    let mut cursor = relay.ring.cursor_at_head();
    let mut dropped = 0u64;
    let mut last_stats = Instant::now() - Duration::from_secs(1);

    loop {
        let now = Instant::now();
        if let Some(reason) = session.expired(now) {
            let _ = send_bin(&mut sink, relay.stats_frame(&session, dropped)).await;
            let _ = sink
                .send(Message::Close(Some(CloseFrame {
                    code: 1000,
                    reason: reason.close_reason().into(),
                })))
                .await;
            return;
        }
        if now.duration_since(last_stats) >= Duration::from_secs(1) {
            if !send_bin(&mut sink, relay.stats_frame(&session, dropped)).await {
                return;
            }
            last_stats = now;
        }

        let mut sent = 0;
        while sent < DRAIN_PER_TURN {
            match relay.ring.poll(&mut cursor) {
                Poll::Empty => break,
                Poll::Gap { dropped_events } => {
                    dropped = dropped.saturating_add(dropped_events);
                    if !send_bin(&mut sink, relay.stats_frame(&session, dropped)).await {
                        return;
                    }
                    for frame in relay.preamble_frames() {
                        if !send_bin(&mut sink, frame).await {
                            return;
                        }
                    }
                }
                Poll::Frame(frame) => {
                    let events = frame.events;
                    let payload = Bytes::clone(&frame.bytes);
                    match tokio::time::timeout(SEND_TIMEOUT, send_bin(&mut sink, payload)).await {
                        Ok(true) => {
                            cursor.commit(&frame);
                            if events > 0 {
                                session.note_delivery(Instant::now());
                            }
                            sent += 1;
                        }
                        Ok(false) => return,
                        Err(_) => {
                            let extra = relay.ring.skip_to_head(&mut cursor);
                            let missed = if extra == 0 { u64::from(events) } else { extra };
                            dropped = dropped.saturating_add(missed);
                            if !send_bin(&mut sink, relay.stats_frame(&session, dropped)).await {
                                return;
                            }
                            break;
                        }
                    }
                }
            }
        }

        if relay.ring.has_pending(&cursor) {
            continue;
        }

        let notified = relay.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if relay.ring.has_pending(&cursor) {
            continue;
        }
        let wake = session
            .idle_deadline()
            .min(session.hard_deadline())
            .min(last_stats + Duration::from_secs(1));
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        // A ping must not spend a control token. Knob changes do.
                        let preview = relay.knobs.apply_ws(&text, false);
                        if matches!(preview, WsControl::Limited) && relay.limiter.allow() {
                            relay.knobs.apply_ws(&text, true);
                        }
                        if preview.is_activity() {
                            session.note_activity(Instant::now());
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
            _ = &mut notified => {}
            _ = tokio::time::sleep(wake.saturating_duration_since(Instant::now())) => {}
        }
    }
}

async fn send_bin(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    bytes: Bytes,
) -> bool {
    sink.send(Message::Binary(bytes.to_vec())).await.is_ok()
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as ClientMessage;

    async fn http(addr: SocketAddr, request: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn body_of(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    async fn next_matching(ws: &mut ClientSocket, want: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let frame = tokio::time::timeout(Duration::from_millis(400), ws.next())
                .await
                .ok()
                .flatten()
                .unwrap()
                .unwrap();
            if let ClientMessage::Binary(bytes) = frame {
                if bytes.as_slice() == want {
                    return;
                }
            }
        }
        panic!("client missed the batch");
    }

    type ClientSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect(addr: SocketAddr) -> ClientSocket {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        ws
    }

    #[tokio::test]
    async fn status_counts_sockets_and_a_stats_frame_carries_clients() {
        let relay = Relay::new(RelayConfig {
            ring_slots: 32,
            idle: Duration::from_secs(60),
            hard_cap: HARD_CAP,
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(Arc::clone(&relay)).await.unwrap();
        let mut ws = connect(addr).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_clients = false;
        let mut saw_expiry = false;
        while Instant::now() < deadline {
            let frame = tokio::time::timeout(Duration::from_millis(500), ws.next())
                .await
                .expect("timed out waiting for a frame")
                .unwrap()
                .unwrap();
            let ClientMessage::Binary(bytes) = frame else {
                continue;
            };
            if perfr_live_protocol::read_hello_expiry(&bytes).is_some() {
                saw_expiry = true;
            }
            if let Some(ext) = perfr_live_protocol::read_relay_status(&bytes) {
                assert!(ext.clients >= 1, "stats clients {}", ext.clients);
                assert!(ext.session_expires_unix_ms > unix_ms());
                saw_clients = true;
                break;
            }
        }
        assert!(saw_expiry, "hello did not carry a session expiry");
        assert!(saw_clients, "no stats frame with clients");

        let status = http(
            addr,
            "GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(body_of(&status)).unwrap();
        assert_eq!(parsed["clients"], 1);
        assert!(parsed["uptime"].as_f64().unwrap() >= 0.0);
        assert!(parsed.get("events_per_sec").is_some());
        drop(ws);
        let mut gone = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let status = http(
                addr,
                "GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await;
            let parsed: serde_json::Value = serde_json::from_str(body_of(&status)).unwrap();
            if parsed["clients"] == 0 {
                gone = true;
                break;
            }
        }
        assert!(
            gone,
            "client count stayed above zero after the socket closed"
        );
    }

    #[tokio::test]
    async fn two_clients_receive_the_same_published_batch() {
        let relay = Relay::new(RelayConfig {
            ring_slots: 32,
            idle: Duration::from_secs(30),
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(Arc::clone(&relay)).await.unwrap();
        let mut a = connect(addr).await;
        let mut b = connect(addr).await;
        // Both are past hello before the batch, so the batch is the shared one.
        let _ = tokio::time::timeout(Duration::from_secs(1), a.next()).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), b.next()).await;
        relay.publish_events(Bytes::from_static(b"batch-once"), 4);
        next_matching(&mut a, b"batch-once").await;
        next_matching(&mut b, b"batch-once").await;
    }

    #[tokio::test]
    async fn published_bytes_do_not_depend_on_the_synthetic_knobs() {
        let relay = Relay::new(RelayConfig::default());
        relay.knobs.apply_http(&ControlUpdate {
            rate: Some(0),
            noise: Some(1.0),
            herd: Some(99),
        });
        relay.publish_events(Bytes::from_static(b"from-capture"), 2);
        let mut cursor = relay.ring.cursor_at_tail();
        match relay.ring.poll(&mut cursor) {
            Poll::Frame(frame) => {
                assert_eq!(&frame.bytes[..], b"from-capture");
                assert_eq!(frame.events, 2);
            }
            Poll::Gap { .. } => panic!("single batch was coalesced"),
            Poll::Empty => panic!("capture batch was dropped"),
        }
        assert_eq!(relay.knobs.herd(), 99);
    }

    #[tokio::test]
    async fn control_is_rate_limited() {
        let relay = Relay::new(RelayConfig {
            control_per_sec: 1,
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(Arc::clone(&relay)).await.unwrap();
        async fn post(addr: SocketAddr, rate: u64) -> String {
            let body = format!(r#"{{"rate":{rate}}}"#);
            let req = format!(
                "POST /control HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            http(addr, &req).await
        }
        let first = post(addr, 5).await;
        assert!(first.contains("200"), "{first}");
        let second = post(addr, 99).await;
        assert!(second.contains("429"), "{second}");
        let get = http(
            addr,
            "GET /control HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_str(body_of(&get)).unwrap();
        assert_eq!(parsed["rate"], 5);
    }

    #[tokio::test]
    async fn idle_closes_when_nothing_is_delivered_and_the_client_is_quiet() {
        let relay = Relay::new(RelayConfig {
            idle: Duration::from_millis(300),
            hard_cap: Duration::from_secs(30),
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(relay).await.unwrap();
        let mut ws = connect(addr).await;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut reason = None;
        while Instant::now() < deadline {
            let next = tokio::time::timeout(Duration::from_secs(3), ws.next()).await;
            let Ok(Some(Ok(msg))) = next else { break };
            if let ClientMessage::Close(frame) = msg {
                reason = frame.map(|frame| frame.reason.to_string());
                break;
            }
        }
        assert_eq!(reason.as_deref(), Some("idle"));
    }

    #[tokio::test]
    async fn a_ping_holds_the_idle_timer_and_the_hard_cap_still_fires() {
        let relay = Relay::new(RelayConfig {
            idle: Duration::from_millis(400),
            hard_cap: Duration::from_millis(900),
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(relay).await.unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
            .await
            .unwrap();
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(ClientMessage::Close(frame)) => {
                        let _ = tx.send(frame.map(|frame| frame.reason.to_string()));
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(700) {
            sink.send(ClientMessage::Text("{\"op\":\"ping\"}".into()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(rx.try_recv().is_err(), "closed before the hard cap");
        }
        let reason = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("hard cap did not close")
            .flatten();
        assert_eq!(reason.as_deref(), Some("session cap"));
        assert!(started.elapsed() >= Duration::from_millis(800));
    }

    #[tokio::test]
    async fn delivered_events_hold_the_idle_timer() {
        let relay = Relay::new(RelayConfig {
            idle: Duration::from_millis(400),
            hard_cap: Duration::from_secs(30),
            ..RelayConfig::default()
        });
        let addr = serve_ephemeral(Arc::clone(&relay)).await.unwrap();
        let mut ws = connect(addr).await;
        let started = Instant::now();
        let mut batches = 0;
        while started.elapsed() < Duration::from_millis(900) {
            relay.publish_events(Bytes::from_static(b"tick"), 1);
            let frame = tokio::time::timeout(Duration::from_millis(500), ws.next())
                .await
                .expect("server closed while events were flowing")
                .unwrap()
                .unwrap();
            match frame {
                ClientMessage::Binary(bytes) if bytes.as_slice() == b"tick" => batches += 1,
                ClientMessage::Close(_) => panic!("idle close while events were delivered"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(batches > 1);
    }
}
