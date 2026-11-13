// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Open a `.perfr.stream` into the structures the timeline draws.
//!
//! The bytes are the live protocol (`perfr-live-protocol`): a Hello, interned
//! strings, thread and process names, and batches of 32-byte events. Arrow
//! and Parquet, which is how the service stores a `.perfr.zip`, are not read
//! here.

use std::collections::HashMap;

use perfr_live_event::InternTable;
use perfr_live_protocol::LiveFrame;
use perfr_live_render::TrackIndex;

use crate::frames::{drain_frames, end_of_stream};

/// What a capture file yielded. `fault` is set when the bytes stopped being
/// a stream; events before that point are kept.
#[derive(Default)]
pub struct OpenedCapture {
    pub index: TrackIndex,
    pub intern: InternTable,
    pub processes: HashMap<u32, String>,
    pub threads: HashMap<(u32, u32), String>,
    pub fault: Option<String>,
    pub skipped_frames: usize,
}

impl OpenedCapture {
    fn apply(&mut self, frame: LiveFrame) {
        match frame {
            LiveFrame::Hello { .. } | LiveFrame::CaptureFinished | LiveFrame::Status { .. } => {}
            LiveFrame::EventBatch { events } => {
                for ev in events {
                    self.index.insert(ev);
                }
            }
            LiveFrame::InternedString { id, text } => {
                self.intern.insert_id(id, &text);
            }
            LiveFrame::CaptureStarted { start_ns, .. } => {
                self.index.set_extent_floor(start_ns);
            }
            LiveFrame::ThreadName { pid, tid, name } => {
                self.threads.insert((pid, tid), name);
            }
            LiveFrame::ProcessName { pid, name } => {
                self.processes.insert(pid, name);
            }
        }
    }
}

/// Parse `bytes` as one `.perfr.stream`. A file that ends mid-frame, or that
/// is not this protocol, comes back with `fault` set and whatever was read
/// before the break.
pub fn open_capture(bytes: &[u8]) -> OpenedCapture {
    let mut opened = OpenedCapture::default();
    let mut leftover = Vec::new();
    match drain_frames(&mut leftover, bytes, |frame| opened.apply(frame)) {
        Ok(skipped) => opened.skipped_frames = skipped,
        Err(fault) => opened.fault = Some(fault.file_message()),
    }
    if opened.fault.is_none() {
        if let Err(fault) = end_of_stream(&mut leftover) {
            opened.fault = Some(fault.file_message());
        }
    }
    opened
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::{LiveEvent, LIVE_EVENT_SIZE};
    use perfr_live_protocol::{encode_frame, LiveFrame, VERSION};

    fn scope(pid: u32, tid: u32, start_ns: u64, duration_ns: u64, name_id: u32) -> LiveEvent {
        LiveEvent {
            start_ns,
            duration_ns,
            tid,
            pid,
            kind: perfr_live_event::kind::API_SCOPE,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id,
        }
    }

    fn hello() -> Vec<u8> {
        encode_frame(&LiveFrame::Hello {
            version: VERSION,
            event_size: LIVE_EVENT_SIZE as u16,
        })
    }

    #[test]
    fn a_stream_opens_into_events_names_and_strings() {
        let mut bytes = hello();
        bytes.extend(encode_frame(&LiveFrame::ProcessName {
            pid: 7,
            name: "demo".into(),
        }));
        bytes.extend(encode_frame(&LiveFrame::ThreadName {
            pid: 7,
            tid: 1,
            name: "main".into(),
        }));
        bytes.extend(encode_frame(&LiveFrame::InternedString {
            id: 3,
            text: "work".into(),
        }));
        bytes.extend(encode_frame(&LiveFrame::CaptureStarted {
            pid: 7,
            start_ns: 1_000,
        }));
        bytes.extend(encode_frame(&LiveFrame::EventBatch {
            events: vec![scope(7, 1, 1_000, 500, 3)],
        }));
        bytes.extend(encode_frame(&LiveFrame::CaptureFinished));

        let opened = open_capture(&bytes);
        assert!(opened.fault.is_none(), "{:?}", opened.fault);
        assert_eq!(opened.index.event_count(), 1);
        assert_eq!(opened.processes.get(&7).map(String::as_str), Some("demo"));
        assert_eq!(
            opened.threads.get(&(7, 1)).map(String::as_str),
            Some("main")
        );
        assert_eq!(opened.intern.get(3), Some("work"));
        assert_eq!(opened.index.time_bounds(), Some((1_000, 1_500)));
    }

    #[test]
    fn bytes_that_end_mid_frame_keep_what_was_whole() {
        let mut bytes = hello();
        bytes.extend(encode_frame(&LiveFrame::EventBatch {
            events: vec![scope(1, 1, 10, 10, 1)],
        }));
        bytes.extend_from_slice(&[4, 0, 0, 0, 2]);

        let opened = open_capture(&bytes);
        assert_eq!(opened.index.event_count(), 1);
        assert!(opened.fault.as_deref().unwrap_or("").contains("ends"));
    }
}
