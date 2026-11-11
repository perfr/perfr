// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The live stream: `GET /ws`, what a viewer is sent when it connects and
//! what it is sent when it falls behind.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::LiveService;

pub(super) async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(svc): State<Arc<LiveService>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_loop(socket, svc))
}

/// The scope name for one wire frame, by its type byte: what a capture of
/// the service shows for every send to a viewer.
pub(super) fn send_scope_name(bytes: &[u8]) -> &'static str {
    use perfr_live_protocol::*;
    match bytes.get(4).copied() {
        Some(FRAME_EVENT_BATCH) | Some(FRAME_EVENT_BATCH_PACKED) | Some(FRAME_EVENT_BATCH_DEFLATE) => "ws send events",
        Some(FRAME_INTERNED_STRING) => "ws send string",
        Some(FRAME_THREAD_NAME) | Some(FRAME_PROCESS_NAME) => "ws send name",
        Some(FRAME_STATUS) => "ws send status",
        Some(FRAME_HELLO) => "ws send hello",
        Some(FRAME_CAPTURE_STARTED) | Some(FRAME_CAPTURE_FINISHED) => "ws send capture mark",
        _ => "ws send frame",
    }
}

/// One frame to one viewer, as a scope named for what it carries, with the
/// frame's size on a value lane.
pub(super) async fn send_frame(sink: &mut futures_util::stream::SplitSink<WebSocket, Message>, bytes: Vec<u8>) -> bool {
    let name = send_scope_name(&bytes);
    let size = bytes.len() as f64;
    let scope = crate::scope(name);
    crate::value("ws frame bytes", size);
    let ok = sink.send(Message::Binary(bytes)).await.is_ok();
    drop(scope);
    ok
}

/// Starts a viewer from the service's state as it is now: sends it the
/// Hello, the names and the whole ring, and returns the receiver of every
/// frame broadcast from that state on. `None` when the socket is gone.
pub(super) async fn start_viewer(
    svc: &Arc<LiveService>,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> Option<broadcast::Receiver<Vec<u8>>> {
    // Copying the ring holds its lock for the length of a large memcpy:
    // not on a thread the other requests are served from.
    let service = svc.clone();
    let (rx, snapshot) = tokio::task::spawn_blocking(move || service.subscribe_with_snapshot()).await.ok()?;
    for frame in &snapshot.head {
        if !send_frame(sink, frame.clone()).await {
            return None;
        }
    }
    for frame in snapshot.event_frames() {
        if !send_frame(sink, frame).await {
            return None;
        }
    }
    Some(rx)
}

pub(super) async fn ws_loop(socket: WebSocket, svc: Arc<LiveService>) {
    let (mut sink, mut stream) = socket.split();
    let Some(mut rx) = start_viewer(&svc, &mut sink).await else {
        return;
    };
    let mut lag_reported = false;
    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            live = rx.recv() => {
                match live {
                    Ok(bytes) => {
                        if !send_frame(&mut sink, bytes).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        if svc.is_live_capture() {
                            // A viewer that cannot keep up with a live
                            // capture goes on with a gap. It gets the names
                            // and strings again, since a name it missed
                            // would leave a track unlabelled for good; the
                            // events it missed stay in the ring for the
                            // report and the export. Starting it over with
                            // the whole ring instead (below) took longer
                            // than the slack it had, so it lagged again
                            // before the snapshot was through and again
                            // after that: the view reset every few seconds
                            // to the start of the capture and never reached
                            // the present.
                            if !lag_reported {
                                lag_reported = true;
                                eprintln!(
                                    "perfr-live-server: a viewer fell {skipped} frame(s) behind the live capture; \
                                     it continues with a gap"
                                );
                            }
                            for message in crate::join_frames(svc.names_and_status_frames()) {
                                if !send_frame(&mut sink, message).await {
                                    return;
                                }
                            }
                            continue;
                        }
                        // Otherwise the burst is finite -- opening a
                        // capture posts its names and batches all at once
                        // -- and rather than leave the viewer with holes it
                        // cannot see, start it over: a fresh Hello plus the
                        // whole ring, which the viewer takes as a reset.
                        // That holds while the capture is still being read
                        // back, too: it counts as capturing, and a viewer
                        // sent on with a gap then kept the gap, since
                        // nothing lags once the burst is over.
                        //
                        // The subscription is replaced along with the
                        // snapshot. The frames still queued in the old one
                        // carry events the ring, and so the snapshot,
                        // already holds: sent after it, they showed every
                        // event of the capture's tail twice.
                        match start_viewer(&svc, &mut sink).await {
                            Some(fresh) => rx = fresh,
                            None => return,
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
