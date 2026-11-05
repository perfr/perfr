// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Local ingest from perfr.
//!
//! The capture process already serialized each frame once. It pushes that
//! byte string here over a loopback TCP socket. This task does not parse
//! events and does not re-encode them: a batch is published into the ring
//! as the same bytes every browser will be sent.
//!
//! Record layout, little-endian:
//!
//! ```text
//! u32 byte_len
//! u32 event_count   // 0 for names, hello, capture marks
//! [byte_len bytes of one OLIV frame]
//! ```
//!
//! The listener refuses any bind that is not loopback. An accepted peer
//! that is not loopback is dropped. Browsers never use this socket.

use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;
use perfr_live_protocol::{FRAME_EVENT_BATCH, FRAME_EVENT_BATCH_DEFLATE, FRAME_EVENT_BATCH_PACKED};
use tokio::io::AsyncReadExt;
#[cfg(test)]
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::server::Relay;

const MAX_FRAME: usize = 8 * 1024 * 1024;

pub fn is_loopback(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

pub async fn serve_ingest(addr: SocketAddr, relay: std::sync::Arc<Relay>) -> std::io::Result<()> {
    if !is_loopback(addr) {
        return Err(std::io::Error::new(
            ErrorKind::AddrNotAvailable,
            "ingest must bind a loopback address",
        ));
    }
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (sock, peer) = listener.accept().await?;
        if !is_loopback(peer) {
            continue;
        }
        let relay = std::sync::Arc::clone(&relay);
        tokio::spawn(async move {
            let _ = read_conn(sock, relay).await;
        });
    }
}

async fn read_conn(mut sock: TcpStream, relay: std::sync::Arc<Relay>) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    loop {
        let mut hdr = [0u8; 8];
        sock.read_exact(&mut hdr).await?;
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        let events = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if len > MAX_FRAME {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "ingest frame is too large",
            ));
        }
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).await?;
        publish(&relay, Bytes::from(body), events);
    }
}

fn publish(relay: &Relay, bytes: Bytes, events: u32) {
    let batch = matches!(
        bytes.get(4).copied(),
        Some(FRAME_EVENT_BATCH | FRAME_EVENT_BATCH_PACKED | FRAME_EVENT_BATCH_DEFLATE)
    );
    if batch {
        relay.publish_events(bytes, events);
    } else {
        relay.publish_control(bytes);
    }
}

/// Writes one ingest record. The capture forwarder has its own blocking
/// writer; this is the async form the ingest tests use.
#[cfg(test)]
pub async fn write_record<W: AsyncWriteExt + Unpin>(
    sock: &mut W,
    frame: &[u8],
    events: u32,
) -> std::io::Result<()> {
    sock.write_all(&(frame.len() as u32).to_le_bytes()).await?;
    sock.write_all(&events.to_le_bytes()).await?;
    sock.write_all(frame).await?;
    Ok(())
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use perfr_live_event::LiveEvent;
    use perfr_live_protocol::encode_event_batch;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    use crate::ring::Poll;
    use crate::server::RelayConfig;
    use crate::Relay;

    #[tokio::test]
    async fn refuses_a_non_loopback_bind() {
        let relay = Relay::new(RelayConfig::default());
        let addr: SocketAddr = "8.8.8.8:9".parse().unwrap();
        let err = serve_ingest(addr, relay).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrNotAvailable);
    }

    #[tokio::test]
    async fn a_loopback_batch_is_published_without_being_rewritten() {
        let relay = Relay::new(RelayConfig {
            ring_slots: 16,
            ..RelayConfig::default()
        });
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task_relay = std::sync::Arc::clone(&relay);
        tokio::spawn(async move {
            let (sock, peer) = listener.accept().await.unwrap();
            assert!(is_loopback(peer));
            let _ = read_conn(sock, task_relay).await;
        });
        let frame = encode_event_batch(&[LiveEvent {
            start_ns: 10,
            duration_ns: 5,
            tid: 1,
            pid: 2,
            kind: 1,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: 3,
        }]);
        let mut sock = TcpStream::connect(addr).await.unwrap();
        write_record(&mut sock, &frame, 1).await.unwrap();
        sock.shutdown().await.ok();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut cursor = relay.ring_for_test().cursor_at_tail();
        loop {
            match relay.ring_for_test().poll(&mut cursor) {
                Poll::Frame(got) => {
                    assert_eq!(&got.bytes[..], frame.as_slice());
                    assert_eq!(got.events, 1);
                    return;
                }
                Poll::Gap { .. } => panic!("the only batch was coalesced"),
                Poll::Empty => {
                    if std::time::Instant::now() > deadline {
                        panic!("ingest did not publish");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }
}
