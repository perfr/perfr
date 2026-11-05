// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One-way handoff of already-encoded live frames to `perfr-relay`.
//!
//! The capture thread only `try_send`s onto a bounded queue. A side thread
//! owns the TCP connection, so a relay that is down or slow cannot stall
//! the workload. The bytes are the OLIV frame the local viewers already
//! received; the relay does not ask the capture to serialize again.

use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use bytes::Bytes;

use crate::LiveService;

pub(crate) struct RelayOut {
    pub bytes: Bytes,
    pub events: u32,
}

pub(crate) fn attach(svc: &LiveService, addr: SocketAddr) -> Result<(), String> {
    if !is_loopback(addr) {
        return Err("relay ingest must be a loopback address".into());
    }
    let (tx, rx) = mpsc::sync_channel(256);
    *svc.relay_tx.lock() = Some(tx);
    thread::Builder::new()
        .name("perfr-relay-fwd".into())
        .spawn(move || forward_loop(addr, rx))
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn attach_from_env(svc: &LiveService) {
    let Ok(text) = std::env::var("PERFR_RELAY_INGEST") else {
        return;
    };
    let Ok(addr) = text.parse() else {
        eprintln!("PERFR_RELAY_INGEST is not a socket address: {text}");
        return;
    };
    if let Err(err) = attach(svc, addr) {
        eprintln!("perfr relay forward: {err}");
    }
}

fn is_loopback(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

fn forward_loop(addr: SocketAddr, rx: Receiver<RelayOut>) {
    loop {
        let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
            Ok(stream) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(200));
                match rx.try_recv() {
                    Err(mpsc::TryRecvError::Disconnected) => return,
                    Err(mpsc::TryRecvError::Empty) => continue,
                    // One stale frame per retry. The rest stay queued until
                    // the relay accepts, or are dropped by `try_send` once
                    // the queue is full.
                    Ok(_) => continue,
                }
            }
        };
        let _ = stream.set_nodelay(true);
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
        loop {
            let item = match rx.recv() {
                Ok(item) => item,
                Err(_) => return,
            };
            if write_record(&mut stream, &item).is_err() {
                break;
            }
        }
    }
}

fn write_record(stream: &mut TcpStream, item: &RelayOut) -> io::Result<()> {
    stream.write_all(&(item.bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&item.events.to_le_bytes())?;
    stream.write_all(&item.bytes)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Instant;

    use perfr_live_event::{kind, LiveEvent};
    use perfr_live_protocol::{decode_frame, LiveFrame};

    use crate::{LiveService, ServerConfig};

    fn new_service() -> std::sync::Arc<LiveService> {
        LiveService::new(ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ring_buffer_bytes: 64 * 1024,
            spill_path: None,
            wire: crate::WireFormat::Raw,
        })
        .unwrap()
    }

    fn one_event() -> LiveEvent {
        LiveEvent {
            start_ns: 1_000,
            duration_ns: 40,
            tid: 7,
            pid: 3,
            kind: kind::API_SCOPE,
            depth: 1,
            extra: 0,
            _pad: 0,
            name_id: 9,
        }
    }

    #[test]
    fn a_public_address_is_refused() {
        let svc = new_service();
        let err = svc
            .attach_relay_ingest("1.2.3.4:44781".parse().unwrap())
            .unwrap_err();
        assert!(err.contains("loopback"), "{err}");
    }

    #[test]
    fn a_full_forward_queue_does_not_block_the_capture() {
        let svc = new_service();
        // Nothing is listening. The forwarder thread retries; the queue fills.
        svc.attach_relay_ingest("127.0.0.1:1".parse().unwrap())
            .unwrap();
        let started = Instant::now();
        let event = one_event();
        for _ in 0..1_000 {
            svc.push_events(std::slice::from_ref(&event));
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "push_events blocked on the relay for {:?}",
            started.elapsed()
        );
        assert!(
            svc.relay_dropped() > 0,
            "expected the forward queue to drop"
        );
    }

    #[test]
    fn the_listener_receives_the_same_frame_the_viewers_would() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = new_service();
        svc.attach_relay_ingest(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut accepted = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((sock, _)) => {
                    accepted = Some(sock);
                    break;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("accept: {err}"),
            }
        }
        let mut sock = accepted.expect("relay forwarder did not connect");
        sock.set_nonblocking(false).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let event = one_event();
        svc.push_events(std::slice::from_ref(&event));
        let mut hdr = [0u8; 8];
        sock.read_exact(&mut hdr).unwrap();
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        let events = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        assert_eq!(events, 1);
        let mut body = vec![0u8; len];
        sock.read_exact(&mut body).unwrap();
        let (frame, n) = decode_frame(&body).unwrap();
        assert_eq!(n, body.len());
        match frame {
            LiveFrame::EventBatch { events } => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0], event);
            }
            other => panic!("expected an event batch, got {other:?}"),
        }
    }
}
