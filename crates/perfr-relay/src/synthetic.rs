// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Synthetic scopes for the public demo.
//!
//! This is the only reader of the control knobs. `rate` is events per
//! second, `noise` is the fraction of events whose duration is jittered,
//! and `herd` is a one-shot burst. Capture frames that arrive on the ingest
//! socket never look at these values.
//!
//! Batches are raw `OLIV` event frames: one `Vec` of events and one output
//! buffer per batch, then `publish_events`. There is no per-event heap
//! allocation and no per-client encode.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use perfr_live_event::{kind, LiveEvent, LIVE_EVENT_SIZE};
use perfr_live_protocol::{encode_event_batch, encode_frame, LiveFrame, VERSION};

use crate::control::ControlUpdate;
use crate::server::Relay;

pub const SYNTH_PID: u32 = 42;
const THREADS: u32 = 8;
const NAMES: &[&str] = &["Tick", "Simulate", "Cull", "Draw", "PhysicsStep", "Animate"];
const BATCH: usize = 64;

pub fn start(relay: Arc<Relay>, rate: u64) {
    relay.set_demo(true);
    relay.knobs().apply_http(&ControlUpdate {
        rate: Some(rate),
        noise: None,
        herd: None,
    });
    publish_names(&relay);
    tokio::spawn(async move {
        run(relay).await;
    });
}

fn publish_names(relay: &Relay) {
    relay.publish_control(Bytes::from(encode_frame(&LiveFrame::Hello {
        version: VERSION,
        event_size: LIVE_EVENT_SIZE as u16,
    })));
    relay.publish_control(Bytes::from(encode_frame(&LiveFrame::ProcessName {
        pid: SYNTH_PID,
        name: "perfr-synthetic".into(),
    })));
    relay.publish_control(Bytes::from(encode_frame(&LiveFrame::CaptureStarted {
        pid: SYNTH_PID,
        start_ns: unix_ns(),
    })));
    for (i, name) in NAMES.iter().enumerate() {
        relay.publish_control(Bytes::from(encode_frame(&LiveFrame::InternedString {
            id: i as u32,
            text: (*name).to_string(),
        })));
    }
    for tid in 0..THREADS {
        relay.publish_control(Bytes::from(encode_frame(&LiveFrame::ThreadName {
            pid: SYNTH_PID,
            tid,
            name: format!("synth-{tid}"),
        })));
    }
}

async fn run(relay: Arc<Relay>) {
    let mut budget = 0.0f64;
    let mut seq = 0u64;
    let tick = Duration::from_millis(20);
    loop {
        let started = tokio::time::Instant::now();
        let rate = relay.knobs().rate();
        let noise = relay.knobs().noise();
        let herd = relay.knobs().take_herd();
        if herd > 0 {
            let mut left = herd as usize;
            while left > 0 {
                let n = left.min(BATCH);
                publish_batch(&relay, n, noise, &mut seq);
                left -= n;
            }
        }
        budget += rate as f64 * 0.020;
        // Cap a backlog so a stalled tick does not become a huge burst.
        budget = budget.min((rate as f64 / 10.0).max(BATCH as f64));
        // Yield between batches. A tick's worth of batches published
        // back-to-back looks, to every client, like falling behind the
        // coalesce window, and each client would keep only the last batch.
        while budget >= BATCH as f64 {
            publish_batch(&relay, BATCH, noise, &mut seq);
            budget -= BATCH as f64;
            tokio::task::yield_now().await;
        }
        let rest = tick.saturating_sub(started.elapsed());
        tokio::time::sleep(rest).await;
    }
}

fn publish_batch(relay: &Relay, n: usize, noise: f64, seq: &mut u64) {
    let now = unix_ns();
    let mut events = Vec::with_capacity(n);
    for i in 0..n {
        let id = *seq;
        *seq = seq.wrapping_add(1);
        let jitter = if noise > 0.0 && unit(id) < noise {
            1 + (id % 50_000)
        } else {
            20_000
        };
        events.push(LiveEvent {
            start_ns: now,
            duration_ns: jitter,
            tid: (i as u32) % THREADS,
            pid: SYNTH_PID,
            kind: kind::API_SCOPE,
            depth: (i % 4) as u8,
            extra: 0,
            _pad: 0,
            name_id: (i % NAMES.len()) as u32,
        });
    }
    let bytes = Bytes::from(encode_event_batch(&events));
    relay.publish_events(bytes, n as u32);
}

fn unix_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn unit(n: u64) -> f64 {
    let x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
    (x % 10_000) as f64 / 10_000.0
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use perfr_live_protocol::{decode_frame, LiveFrame};

    use crate::ring::Poll;
    use crate::server::RelayConfig;

    #[test]
    fn a_batch_is_one_raw_frame_and_the_knobs_do_not_rewrite_it() {
        let relay = Relay::new(RelayConfig {
            ring_slots: 32,
            ..RelayConfig::default()
        });
        relay.knobs().apply_http(&ControlUpdate {
            rate: Some(0),
            noise: Some(1.0),
            herd: Some(0),
        });
        let mut seq = 0u64;
        publish_batch(&relay, 4, 0.0, &mut seq);
        let mut cursor = relay.ring_for_test().cursor_at_tail();
        let Poll::Frame(frame) = relay.ring_for_test().poll(&mut cursor) else {
            panic!("batch missing");
        };
        assert_eq!(frame.events, 4);
        let (decoded, _) = decode_frame(&frame.bytes).unwrap();
        let LiveFrame::EventBatch { events } = decoded else {
            panic!("not a raw batch");
        };
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|ev| ev.pid == SYNTH_PID));
        assert!(events[0].start_ns > 1_000_000_000_000_000_000);
    }
}
