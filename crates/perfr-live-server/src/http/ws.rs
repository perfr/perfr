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
    let ok = matches!(tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Binary(bytes))).await, Ok(Ok(())));
    drop(scope);
    ok
}

/// How long one frame may take to leave for a viewer before the viewer is
/// let go. A send waits for room in the socket, and a client that connects
/// and never reads leaves none: its task then sat in the send for as long
/// as the connection stayed open, holding its copy of the ring (a quarter
/// of a gigabyte by default, for each such connection). A viewer that was
/// merely slow reconnects and is started over.
pub(super) const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

/// About how large a message of live frames joined for a viewer that is
/// behind is let grow. The viewer applies a message whole, so this is also
/// the most one of its frames takes on at once.
const BEHIND_MESSAGE_BYTES: usize = 1 << 20;

/// `first` with every frame already waiting behind it, up to about
/// [`BEHIND_MESSAGE_BYTES`], and how many frames the receiver was found to
/// have missed, if it had.
///
/// A viewer that keeps up finds nothing waiting and gets `first` as it is.
/// One that is behind gets what piled up as one message. Sent one frame to
/// a message, the backlog did not shrink: a page whose frames run long is
/// handed a message or a few between two of them, however many are queued,
/// so it took the stream in at a fraction of the rate it was produced and
/// fell further behind for as long as the capture ran.
pub(super) fn with_backlog(first: Vec<u8>, rx: &mut broadcast::Receiver<Vec<u8>>) -> (Vec<u8>, Option<u64>) {
    let mut message = first;
    while message.len() < BEHIND_MESSAGE_BYTES {
        match rx.try_recv() {
            Ok(frame) => message.extend_from_slice(&frame),
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => return (message, Some(skipped)),
            Err(_) => break,
        }
    }
    (message, None)
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
                let lagged = match live {
                    Ok(bytes) => {
                        let (message, lagged) = with_backlog(bytes, &mut rx);
                        if !send_frame(&mut sink, message).await {
                            break;
                        }
                        lagged
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => Some(skipped),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if let Some(skipped) = lagged {
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
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_that_keeps_up_gets_each_frame_as_it_is() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(8);
        tx.send(vec![1, 2, 3]).expect("a receiver");
        let first = rx.try_recv().expect("the frame");
        assert_eq!(with_backlog(first, &mut rx), (vec![1, 2, 3], None));
    }

    #[test]
    fn a_viewer_that_is_behind_gets_what_piled_up_as_one_message() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(64);
        for i in 0..5u8 {
            tx.send(vec![i; 4]).expect("a receiver");
        }
        let first = rx.try_recv().expect("the first frame");
        let (message, lagged) = with_backlog(first, &mut rx);
        assert_eq!(lagged, None);
        assert_eq!(message.len(), 20, "all five, back to back");
        assert_eq!(&message[16..], [4; 4], "in order");
        assert!(rx.try_recv().is_err(), "nothing is left queued");
    }

    #[test]
    fn a_joined_message_stops_growing_at_about_its_limit() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(64);
        for _ in 0..6 {
            tx.send(vec![0u8; BEHIND_MESSAGE_BYTES / 2]).expect("a receiver");
        }
        let first = rx.try_recv().expect("the first frame");
        let (message, _) = with_backlog(first, &mut rx);
        assert_eq!(message.len(), BEHIND_MESSAGE_BYTES, "two halves reach the limit");
        assert_eq!(rx.len(), 4, "the rest waits for the next message");
    }

    #[test]
    fn frames_the_receiver_missed_are_reported_with_what_was_joined() {
        let (tx, mut rx) = broadcast::channel::<Vec<u8>>(4);
        tx.send(vec![9]).expect("a receiver");
        let first = rx.try_recv().expect("the first frame");
        // Six more into a channel of four: two are gone before they are read.
        for i in 0..6u8 {
            tx.send(vec![i]).expect("a receiver");
        }
        let (message, lagged) = with_backlog(first, &mut rx);
        assert_eq!(message, [9], "nothing after the gap is joined across it");
        assert_eq!(lagged, Some(2));
    }
}
