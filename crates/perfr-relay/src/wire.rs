// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Frames the relay sends besides the captured batches.
//!
//! Event batches are whatever the publisher already serialized — for perfr
//! that is an `OLIV` frame — and are not rebuilt here. Hello and the
//! once-a-second stats message are per client, because they carry that
//! client's drop count and its session deadline. They are the existing
//! Hello and Status frames with a tail the original decoder ignores.

use bytes::Bytes;
use perfr_live_event::LIVE_EVENT_SIZE;
use perfr_live_protocol::{
    append_hello_expiry, append_relay_status, encode_frame, LiveFrame, RelayStatusExt, VERSION,
};

#[derive(Clone, Copy, Debug)]
pub struct StatsFields {
    pub capturing: bool,
    pub demo: bool,
    pub events_live: u64,
    pub events_capacity: u64,
    pub dropped: u64,
    pub produced: u64,
    pub ring_bytes: u64,
    pub ext: RelayStatusExt,
}

pub fn hello(expires_unix_ms: u64) -> Bytes {
    let frame = encode_frame(&LiveFrame::Hello {
        version: VERSION,
        event_size: LIVE_EVENT_SIZE as u16,
    });
    Bytes::from(append_hello_expiry(frame, expires_unix_ms))
}

pub fn stats(fields: &StatsFields) -> Bytes {
    let frame = encode_frame(&LiveFrame::Status {
        capturing: fields.capturing,
        demo: fields.demo,
        events_live: fields.events_live,
        events_capacity: fields.events_capacity,
        dropped: fields.dropped,
        spilled: 0,
        produced: fields.produced,
        oldest_start_ns: 0,
        newest_end_ns: 0,
        ring_bytes: fields.ring_bytes,
    });
    Bytes::from(append_relay_status(frame, &fields.ext))
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use perfr_live_protocol::{decode_frame, read_hello_expiry, read_relay_status, LiveFrame};

    #[test]
    fn hello_and_stats_decode_as_the_viewer_frames() {
        let hello = hello(123_456);
        let (frame, n) = decode_frame(&hello).unwrap();
        assert_eq!(n, hello.len());
        assert!(matches!(
            frame,
            LiveFrame::Hello {
                version: VERSION,
                ..
            }
        ));
        assert_eq!(read_hello_expiry(&hello), Some(123_456));

        let bytes = stats(&StatsFields {
            capturing: true,
            demo: false,
            events_live: 10,
            events_capacity: 32,
            dropped: 7,
            produced: 40,
            ring_bytes: 128,
            ext: RelayStatusExt {
                clients: 3,
                events_per_sec: 80,
                uptime_ms: 1500,
                session_expires_unix_ms: 99,
            },
        });
        let (frame, _) = decode_frame(&bytes).unwrap();
        match frame {
            LiveFrame::Status {
                dropped, produced, ..
            } => {
                assert_eq!(dropped, 7);
                assert_eq!(produced, 40);
            }
            other => panic!("unexpected {other:?}"),
        }
        let ext = read_relay_status(&bytes).unwrap();
        assert_eq!(ext.clients, 3);
        assert_eq!(ext.events_per_sec, 80);
        assert_eq!(ext.session_expires_unix_ms, 99);
    }
}
