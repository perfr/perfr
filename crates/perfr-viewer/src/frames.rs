// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Bytes of a live stream in, frames out.
//!
//! A stream is `[u32 payload_len LE][u8 type][payload]` repeated. It arrives
//! in pieces that need not end on a frame (a WebSocket message, a file), so
//! the tail of one piece waits in a buffer for the next. This module owns
//! that buffer's rules, apart from the app, so they can be tested: what is
//! worth waiting for, what can be stepped over, and what means the stream
//! cannot be read any further.

use perfr_live_protocol::{decode_frame, LiveFrame, ProtocolError, FRAME_HELLO, MAGIC};

/// Length prefix and type byte.
const HEADER: usize = 5;

/// The largest payload worth waiting for. A batch of 2^24 events, the most
/// the packed decoder accepts, is under this in every wire format, so a
/// length above it is not a frame that is still arriving. Checked before
/// the length is added to anything: on wasm32 `5 + 0xFFFF_FFFF` wraps.
pub const MAX_FRAME_PAYLOAD: usize = 1 << 30;

/// Why a stream stopped being readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamFault {
    /// The other end speaks another protocol, or another version of this
    /// one. A new connection to it would say the same thing.
    Incompatible(String),
    /// Bytes that are not a frame. The same source may well be readable
    /// from the start.
    Corrupt(String),
}

impl StreamFault {
    /// Whether asking the service for the stream again can help. Each
    /// attempt costs it a snapshot of the whole ring, so one that is bound
    /// to end the same way is not made.
    pub fn reconnect_helps(&self) -> bool {
        matches!(self, StreamFault::Corrupt(_))
    }

    /// What to tell the reader when the stream is a service's socket.
    pub fn socket_message(&self) -> String {
        match self {
            StreamFault::Incompatible(why) => format!(
                "The service and this page speak different versions of the live protocol ({why}). Reload the page."
            ),
            StreamFault::Corrupt(why) => {
                format!("The live stream could not be read ({why}). Reconnecting for a fresh copy.")
            }
        }
    }

    /// What to tell the reader when the stream is a capture file.
    pub fn file_message(&self) -> String {
        match self {
            StreamFault::Incompatible(why) => {
                format!("This capture was written by a different perfr version ({why}) and cannot be read here.")
            }
            StreamFault::Corrupt(why) => {
                format!("This capture is damaged ({why}). Only what could be read before that is shown.")
            }
        }
    }
}

/// Every stream opens with a Hello: a chunk that begins with one, magic
/// and all, is the first of a stream and not the middle of some frame.
pub fn starts_stream(chunk: &[u8]) -> bool {
    chunk.len() >= HEADER + MAGIC.len() && chunk[4] == FRAME_HELLO && &chunk[HEADER..HEADER + MAGIC.len()] == MAGIC
}

/// Appends `chunk` to `leftover` and hands every complete frame to `apply`,
/// in order. Returns how many frames of a type this build does not know
/// were stepped over. An incomplete tail stays in `leftover` for the next
/// chunk. On a fault the frames before it have been applied and `leftover`
/// is empty: nothing after an unreadable frame can be trusted to start on a
/// frame boundary.
pub fn drain_frames(
    leftover: &mut Vec<u8>,
    chunk: &[u8],
    mut apply: impl FnMut(LiveFrame),
) -> Result<usize, StreamFault> {
    // A new stream (a reconnected socket, a file) is not the rest of a frame
    // the previous one stopped in the middle of. Glued behind that head, its
    // Hello and first frames were decoded as the old frame's payload.
    if !leftover.is_empty() && starts_stream(chunk) {
        leftover.clear();
    }
    leftover.extend_from_slice(chunk);
    // Decode by offset and drain once. Draining after every frame moved
    // the whole remaining buffer each time: a burst of a thousand small
    // batches in one chunk was a thousand memmoves of the chunk.
    let mut off = 0usize;
    let mut skipped = 0usize;
    let fault = loop {
        let rest = &leftover[off..];
        if rest.len() < HEADER {
            break None;
        }
        // The header says whether the frame is all here. The decoder's own
        // answer cannot be used for that: it reports a whole frame whose
        // payload is too short for its type as truncated too, and waiting
        // on that one stalls the stream for good.
        let payload_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if payload_len > MAX_FRAME_PAYLOAD {
            break Some(StreamFault::Corrupt(format!("a frame claims {payload_len} bytes")));
        }
        let total = HEADER + payload_len;
        if rest.len() < total {
            break None;
        }
        match decode_frame(&rest[..total]) {
            Ok((frame, _)) => apply(frame),
            // Length-prefixed so that a newer sender's frame types cost an
            // older reader nothing but the frames themselves.
            Err(ProtocolError::UnknownFrame(_)) => skipped += 1,
            Err(e @ (ProtocolError::BadMagic | ProtocolError::UnsupportedVersion(_))) => {
                break Some(StreamFault::Incompatible(e.to_string()));
            }
            Err(e) => break Some(StreamFault::Corrupt(e.to_string())),
        }
        off += total;
    };
    match fault {
        None => {
            leftover.drain(..off);
            Ok(skipped)
        }
        Some(fault) => {
            leftover.clear();
            Err(fault)
        }
    }
}

/// The source has no more bytes (a file was read to its end). A frame still
/// waiting for its tail will never get it, and must not be completed by the
/// first bytes of whatever is read next.
pub fn end_of_stream(leftover: &mut Vec<u8>) -> Result<(), StreamFault> {
    if leftover.is_empty() {
        return Ok(());
    }
    let cut = leftover.len();
    leftover.clear();
    Err(StreamFault::Corrupt(format!("it ends {cut} bytes into a frame")))
}

/// Whether to open the event socket again now. A closed one is retried
/// every couple of seconds so a restarted service picks the page back up;
/// one closed because the service speaks another protocol version is not,
/// since only a reload of the page changes that.
pub fn ws_retry_due(has_service: bool, ws_ok: bool, incompatible: bool, now_s: f64, last_retry_s: f64) -> bool {
    has_service && !ws_ok && !incompatible && now_s - last_retry_s > 2.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::{kind, LiveEvent, LIVE_EVENT_SIZE};
    use perfr_live_protocol::{encode_frame, VERSION};

    fn hello() -> Vec<u8> {
        encode_frame(&LiveFrame::Hello { version: VERSION, event_size: LIVE_EVENT_SIZE as u16 })
    }

    fn batch(start_ns: u64) -> Vec<u8> {
        let ev = LiveEvent {
            start_ns,
            duration_ns: 10,
            tid: 1,
            pid: 1,
            kind: kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: 1,
        };
        encode_frame(&LiveFrame::EventBatch { events: vec![ev] })
    }

    /// A well-formed frame of a type no build knows.
    fn unknown_frame(typ: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_le_bytes().to_vec();
        out.push(typ);
        out.extend_from_slice(payload);
        out
    }

    /// The start time of each batch, or `u64::MAX` for any other frame.
    fn starts(frames: &[LiveFrame]) -> Vec<u64> {
        frames
            .iter()
            .map(|f| match f {
                LiveFrame::EventBatch { events } => events[0].start_ns,
                _ => u64::MAX,
            })
            .collect()
    }

    #[test]
    fn a_frame_split_across_two_pushes_arrives_once_it_is_whole() {
        let frame = batch(7);
        for cut in 1..frame.len() {
            let mut leftover = Vec::new();
            let mut got = Vec::new();
            assert_eq!(drain_frames(&mut leftover, &frame[..cut], |f| got.push(f)), Ok(0));
            assert!(got.is_empty(), "cut at {cut}: a frame came out of half its bytes");
            assert_eq!(leftover.len(), cut);
            assert_eq!(drain_frames(&mut leftover, &frame[cut..], |f| got.push(f)), Ok(0));
            assert_eq!(starts(&got), vec![7], "cut at {cut}");
            assert!(leftover.is_empty());
        }
    }

    #[test]
    fn an_unknown_frame_type_between_two_frames_is_stepped_over() {
        let mut bytes = batch(1);
        bytes.extend_from_slice(&unknown_frame(200, b"from a newer service"));
        bytes.extend_from_slice(&unknown_frame(201, b""));
        bytes.extend_from_slice(&batch(2));
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        let skipped = drain_frames(&mut leftover, &bytes, |f| got.push(f));
        assert_eq!(starts(&got), vec![1, 2]);
        assert!(leftover.is_empty());
        assert_eq!(skipped, Ok(2));
    }

    #[test]
    fn an_unknown_frame_still_arriving_is_waited_for_then_stepped_over() {
        let unknown = unknown_frame(200, &[0xAB; 64]);
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        assert_eq!(drain_frames(&mut leftover, &unknown[..20], |f| got.push(f)), Ok(0));
        assert_eq!(leftover.len(), 20);
        let mut rest = unknown[20..].to_vec();
        rest.extend_from_slice(&batch(3));
        let skipped = drain_frames(&mut leftover, &rest, |f| got.push(f));
        assert_eq!(starts(&got), vec![3]);
        assert!(leftover.is_empty());
        assert_eq!(skipped, Ok(1));
    }

    #[test]
    fn garbage_is_a_fault_and_empties_the_buffer() {
        // A length no frame has: nothing to wait for.
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        let fault = drain_frames(&mut leftover, &[0xFF; 64], |f| got.push(f));
        assert!(matches!(fault, Err(StreamFault::Corrupt(_))), "{fault:?}");
        assert!(leftover.is_empty() && got.is_empty());
        // Text where a stream should be (an HTML error page, say).
        let fault = drain_frames(&mut leftover, b"<!doctype html><html><body>502 Bad Gateway", |f| got.push(f));
        assert!(matches!(fault, Err(StreamFault::Corrupt(_))), "{fault:?}");
        assert!(leftover.is_empty() && got.is_empty());
    }

    #[test]
    fn a_whole_frame_whose_payload_is_wrong_is_a_fault_not_a_wait() {
        // A batch that claims more events than its payload holds. The
        // decoder calls that truncated, but the frame is all here: no
        // further byte will complete it.
        let mut payload = 1000u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0u8; LIVE_EVENT_SIZE]);
        let mut bytes = batch(1);
        bytes.extend_from_slice(&unknown_frame(perfr_live_protocol::FRAME_EVENT_BATCH, &payload));
        bytes.extend_from_slice(&batch(2));
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        let fault = drain_frames(&mut leftover, &bytes, |f| got.push(f));
        assert!(matches!(fault, Err(StreamFault::Corrupt(_))), "{fault:?}");
        // What came before the bad frame was good and is kept; what follows
        // it cannot be trusted to start on a frame.
        assert_eq!(starts(&got), vec![1]);
        assert!(leftover.is_empty());
    }

    #[test]
    fn a_hello_of_another_version_or_magic_is_incompatible() {
        let mut other_version = hello();
        other_version[9..11].copy_from_slice(&(VERSION + 1).to_le_bytes());
        other_version.extend_from_slice(&batch(1));
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        let fault = drain_frames(&mut leftover, &other_version, |f| got.push(f));
        assert!(matches!(fault, Err(StreamFault::Incompatible(_))), "{fault:?}");
        assert!(leftover.is_empty() && got.is_empty());

        let mut other_magic = hello();
        other_magic[5..9].copy_from_slice(b"NOPE");
        let fault = drain_frames(&mut leftover, &other_magic, |f| got.push(f));
        assert!(matches!(fault, Err(StreamFault::Incompatible(_))), "{fault:?}");
        assert!(leftover.is_empty() && got.is_empty());
    }

    #[test]
    fn a_new_stream_is_not_read_as_the_rest_of_a_frame_the_old_one_cut_short() {
        // The old stream ended inside a frame.
        let cut_short = batch(1);
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        assert_eq!(drain_frames(&mut leftover, &cut_short[..10], |f| got.push(f)), Ok(0));
        // A new one starts, as every stream does, with a Hello.
        let mut fresh = hello();
        fresh.extend_from_slice(&batch(2));
        assert_eq!(drain_frames(&mut leftover, &fresh, |f| got.push(f)), Ok(0));
        assert!(matches!(got.first(), Some(LiveFrame::Hello { .. })), "{got:?}");
        assert_eq!(starts(&got)[1..], [2]);
        assert!(leftover.is_empty());
    }

    #[test]
    fn a_file_that_ends_inside_a_frame_is_reported_and_leaves_nothing_behind() {
        let mut bytes = batch(1);
        bytes.extend_from_slice(&batch(2)[..11]);
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        assert_eq!(drain_frames(&mut leftover, &bytes, |f| got.push(f)), Ok(0));
        assert_eq!(starts(&got), vec![1]);
        assert!(matches!(end_of_stream(&mut leftover), Err(StreamFault::Corrupt(_))));
        assert!(leftover.is_empty());
        assert_eq!(end_of_stream(&mut leftover), Ok(()));
        // Text whose first four bytes read as a length under the limit is
        // waited on like any frame; the end of the file is what settles it.
        assert_eq!(drain_frames(&mut leftover, b"404 Not Found", |f| got.push(f)), Ok(0));
        assert!(end_of_stream(&mut leftover).is_err());
    }

    #[test]
    fn the_socket_is_reopened_after_a_corrupt_frame_but_not_for_another_version() {
        let corrupt = StreamFault::Corrupt("x".into());
        let incompatible = StreamFault::Incompatible("unsupported protocol version 2".into());
        assert!(corrupt.reconnect_helps());
        assert!(!incompatible.reconnect_helps());
        assert!(incompatible.socket_message().contains("Reload the page"));
        assert!(incompatible.socket_message().contains("unsupported protocol version 2"));
        // Closed, with a service, two seconds since the last try.
        assert!(ws_retry_due(true, false, false, 10.0, 7.5));
        assert!(!ws_retry_due(true, false, false, 10.0, 9.0));
        assert!(!ws_retry_due(true, true, false, 10.0, 0.0));
        assert!(!ws_retry_due(false, false, false, 10.0, 0.0));
        assert!(!ws_retry_due(true, false, true, 10.0, 0.0));
    }

    #[test]
    fn frames_after_a_fault_start_clean() {
        let mut leftover = Vec::new();
        let mut got = Vec::new();
        assert!(drain_frames(&mut leftover, &[0xFF; 9], |f| got.push(f)).is_err());
        assert_eq!(drain_frames(&mut leftover, &batch(5), |f| got.push(f)), Ok(0));
        assert_eq!(starts(&got), vec![5]);
    }
}
