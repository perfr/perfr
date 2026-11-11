// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A whole capture, or a slice of one, as a single self-contained file.
//!
//! The events table alone is not a capture: it does not say what the
//! threads and processes were called, and it carries no sampled callstacks.
//! A [`CaptureBundle`] is everything the viewer needs to show a capture
//! again -- events with their names, the sample rows and the frame table
//! they point into, thread and process names, and which process was the
//! target -- and `to_zip` writes it as one `.perfr.zip`: three Parquet
//! tables and a `manifest.json`, in a stored zip, so any zip tool extracts
//! it into a directory the Python example opens. Parquet rather than Arrow
//! IPC because its own encodings (delta on the timestamps, dictionaries on
//! the rest; see `parquet_properties`) make the events table a fifth of
//! its size with no compressor at all, so the zip needs none -- see the
//! phase 10 metrics. A bundle written before this change, with `.arrow`
//! tables in a deflated zip, still opens.
//!
//! `slice` cuts a bundle down to a time window. Events that overlap the
//! window are kept whole -- a scope that straddles the edge is still a real
//! scope with its real duration, and clipping it would misreport it -- so
//! the manifest records the window that was asked for next to the bounds
//! of what was kept. Samples are point events and are kept by timestamp;
//! the frame table keeps only the frames those samples reference.

use std::collections::{HashMap, HashSet};

use perfr_live_event::LiveEvent;

use crate::zipstore::read_store_zip;
#[cfg(test)]
use crate::zipstore::write_store_zip;
use crate::{
    read_events_ipc, read_events_parquet, read_frames_ipc, read_frames_parquet, read_samples_ipc,
    read_samples_parquet, write_events_parquet, write_frames_parquet, write_samples_parquet,
    CaptureError, EventRow, FrameRow, SampleRow, DATASET_FORMAT, EVENTS_FILE, EVENTS_PARQUET,
    FRAMES_FILE, FRAMES_PARQUET, MANIFEST_FILE, SAMPLES_FILE, SAMPLES_PARQUET,
};

/// A thread's name, as `/proc/<pid>/task/<tid>/comm` or the producer gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadName {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
}

/// A process's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessName {
    pub pid: u32,
    pub name: String,
}

/// Everything a capture is. See the module docs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureBundle {
    /// The process the capture targeted; 0 for a capture with no target.
    pub target_pid: u32,
    /// The window this bundle was cut to, if it is a slice.
    pub slice_ns: Option<(u64, u64)>,
    pub processes: Vec<ProcessName>,
    pub threads: Vec<ThreadName>,
    pub events: Vec<EventRow>,
    pub samples: Vec<SampleRow>,
    pub frames: Vec<FrameRow>,
}

/// The file name suffix of a bundle. The viewer tells a bundle from a zipped
/// Chrome trace by it.
pub const BUNDLE_SUFFIX: &str = ".perfr.zip";

impl CaptureBundle {
    /// Earliest event start and latest event end (samples count as instants),
    /// or `None` when there is nothing in it.
    pub fn time_bounds(&self) -> Option<(u64, u64)> {
        let mut start = u64::MAX;
        let mut end = 0u64;
        for r in &self.events {
            start = start.min(r.event.start_ns);
            end = end.max(r.event.end_ns());
        }
        for s in &self.samples {
            start = start.min(s.timestamp_ns);
            end = end.max(s.timestamp_ns);
        }
        (start != u64::MAX).then_some((start, end.max(start)))
    }

    /// The bundle cut to `[t0, t1]`: overlapping events whole, samples by
    /// timestamp, frames those samples use, and only the threads and
    /// processes that still have something in them.
    pub fn slice(&self, t0: u64, t1: u64) -> CaptureBundle {
        let (t0, t1) = (t0.min(t1), t0.max(t1));
        let events: Vec<EventRow> = self
            .events
            .iter()
            .filter(|r| {
                let e = &r.event;
                if e.kind == perfr_live_event::kind::FUNCTION_CALL && e.extra == perfr_live_event::extra::SAMPLED_FRAME {
                    // A frame of a sampled stack is its sample drawn, and
                    // lasts a sampling period only so that it has a width:
                    // it goes where the sample row and the tick go, by the
                    // sample's time. Kept by overlap, the stack of a sample
                    // taken just before the window stood at its left edge
                    // with no sample under it.
                    e.start_ns >= t0 && e.start_ns <= t1
                } else {
                    e.start_ns <= t1 && e.end_ns() >= t0
                }
            })
            .cloned()
            .collect();
        let samples: Vec<SampleRow> = self
            .samples
            .iter()
            .filter(|s| s.timestamp_ns >= t0 && s.timestamp_ns <= t1)
            .cloned()
            .collect();
        let used: HashSet<u32> = samples.iter().flat_map(|s| s.frames.iter().copied()).collect();
        let frames: Vec<FrameRow> = self.frames.iter().filter(|f| used.contains(&f.id)).cloned().collect();

        let mut pids: HashSet<u32> = events.iter().map(|r| r.event.pid).collect();
        let mut tids: HashSet<(u32, u32)> = events.iter().map(|r| (r.event.pid, r.event.tid)).collect();
        for s in &samples {
            // A sample names only its thread; its process is whichever the
            // thread table says, and the target as a fallback.
            let pid = self
                .threads
                .iter()
                .find(|t| t.tid == s.tid)
                .map(|t| t.pid)
                .unwrap_or(self.target_pid);
            pids.insert(pid);
            tids.insert((pid, s.tid));
        }
        // The target is part of the capture even if it was idle in the window.
        pids.insert(self.target_pid);
        CaptureBundle {
            target_pid: self.target_pid,
            slice_ns: Some((t0, t1)),
            processes: self.processes.iter().filter(|p| pids.contains(&p.pid)).cloned().collect(),
            threads: self.threads.iter().filter(|t| tids.contains(&(t.pid, t.tid))).cloned().collect(),
            events,
            samples,
            frames,
        }
    }

    /// The distinct `(name_id, name)` pairs of the events, which is the
    /// intern table a reader has to rebuild.
    pub fn names(&self) -> Vec<(u32, String)> {
        let mut seen: HashMap<u32, &str> = HashMap::new();
        for r in &self.events {
            seen.entry(r.event.name_id).or_insert(&r.name);
        }
        let mut out: Vec<(u32, String)> = seen.into_iter().map(|(id, n)| (id, n.to_string())).collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    fn manifest_json(&self) -> serde_json::Value {
        let bounds = self.time_bounds();
        serde_json::json!({
            "format": DATASET_FORMAT,
            "rows": {
                "events": self.events.len(),
                "samples": self.samples.len(),
                "frames": self.frames.len(),
            },
            "time_bounds_ns": bounds.map(|(a, b)| serde_json::json!({"start": a, "end": b})),
            "files": {
                "events": EVENTS_PARQUET,
                "samples": SAMPLES_PARQUET,
                "frames": FRAMES_PARQUET,
            },
            "bundle": {
                "target_pid": self.target_pid,
                "slice_ns": self.slice_ns.map(|(a, b)| serde_json::json!({"start": a, "end": b})),
                "processes": self.processes.iter().map(|p| serde_json::json!({"pid": p.pid, "name": p.name})).collect::<Vec<_>>(),
                "threads": self.threads.iter().map(|t| serde_json::json!({"pid": t.pid, "tid": t.tid, "name": t.name})).collect::<Vec<_>>(),
            },
        })
    }

    /// Browser wire stream from this exact bundle. The archive remains the
    /// authoritative source of frame addresses and module metadata. Sampled
    /// stacks are rebuilt from sample rows, not inferred from retained spans.
    ///
    /// `CaptureStarted` goes out right after `Hello`: the viewer treats it as
    /// the start of a fresh capture and drops the names it holds, so every
    /// name has to follow it, not precede it.
    pub fn to_stream(&self) -> Vec<u8> {
        use perfr_live_event::{kind, extra, LIVE_EVENT_SIZE};
        use perfr_live_protocol::{encode_frame, LiveFrame, VERSION};
        let mut out = encode_frame(&LiveFrame::Hello { version: VERSION, event_size: LIVE_EVENT_SIZE as u16 });
        let mut append = |frame| out.extend_from_slice(&encode_frame(&frame));
        let start = self.slice_ns.or(self.time_bounds()).map(|(a, _)| a).unwrap_or(0);
        append(LiveFrame::CaptureStarted { pid: self.target_pid, start_ns: start });
        let names = self.names();
        for (id, text) in &names { append(LiveFrame::InternedString { id: *id, text: text.clone() }); }
        // Scope names and frame names share the viewer's one string table.
        // The service interns frames into the scope table so their ids never
        // clash there, but a bundle written by another producer may reuse an
        // id for both: such frames move to fresh ids above everything in use.
        let frame_ids = self.stream_frame_ids(&names);
        for f in &self.frames { append(LiveFrame::InternedString { id: frame_ids[&f.id], text: f.name.clone() }); }
        for p in &self.processes { append(LiveFrame::ProcessName { pid: p.pid, name: p.name.clone() }); }
        for t in &self.threads { append(LiveFrame::ThreadName { pid: t.pid, tid: t.tid, name: t.name.clone() }); }
        let mut events: Vec<LiveEvent> = self.events.iter().map(|r| r.event)
            .filter(|e| e.kind != kind::SAMPLE && !(e.kind == kind::FUNCTION_CALL && e.extra == extra::SAMPLED_FRAME)).collect();
        let periods: HashMap<(u32, u64), u64> = self.events.iter()
            .filter(|r| r.event.kind == kind::SAMPLE)
            .map(|r| ((r.event.tid, r.event.start_ns), r.event.duration_ns)).collect();
        let thread_pids: HashMap<u32, u32> = self.threads.iter().map(|t| (t.tid, t.pid)).collect();
        let frame_id = |id: &u32| frame_ids.get(id).copied().unwrap_or(*id);
        for sample in &self.samples {
            let pid = thread_pids.get(&sample.tid).copied().unwrap_or(self.target_pid);
            let period = periods.get(&(sample.tid, sample.timestamp_ns)).copied().unwrap_or(1);
            let event = LiveEvent { start_ns: sample.timestamp_ns, duration_ns: period, tid: sample.tid, pid,
                kind: kind::SAMPLE, depth: 0, extra: 0, _pad: 0, name_id: sample.frames.first().map(frame_id).unwrap_or(0) };
            events.push(event);
            for (depth, id) in sample.frames.iter().rev().enumerate() {
                events.push(LiveEvent { kind: kind::FUNCTION_CALL, extra: extra::SAMPLED_FRAME, depth: depth.min(255) as u8, name_id: frame_id(id), ..event });
            }
        }
        events.sort_unstable_by_key(|e| e.start_ns);
        for chunk in events.chunks(2048) { append(LiveFrame::EventBatch { events: chunk.to_vec() }); }
        append(LiveFrame::CaptureFinished);
        out
    }

    /// The string id each frame takes in the wire stream: its own, unless a
    /// scope name already holds that id with different text.
    fn stream_frame_ids(&self, names: &[(u32, String)]) -> HashMap<u32, u32> {
        let taken: HashMap<u32, &str> = names.iter().map(|(id, text)| (*id, text.as_str())).collect();
        let mut next = names.iter().map(|(id, _)| *id).chain(self.frames.iter().map(|f| f.id)).max().unwrap_or(0);
        self.frames.iter().map(|f| {
            let clashes = taken.get(&f.id).is_some_and(|text| *text != f.name);
            let id = if clashes { next += 1; next } else { f.id };
            (f.id, id)
        }).collect()
    }

    /// The bundle as one `.perfr.zip`: Parquet tables, stored.
    pub fn to_zip(&self) -> Result<Vec<u8>, CaptureError> {
        self.to_zip_with_level(None)
    }

    /// As [`to_zip`](Self::to_zip) with the zip entries deflated at
    /// `level`, for measuring what a second compressor is worth (little:
    /// the tables are already encoded). `None` stores them, the default.
    pub fn to_zip_with_level(&self, level: Option<u8>) -> Result<Vec<u8>, CaptureError> {
        let names: HashMap<u32, &str> = self.events.iter().map(|r| (r.event.name_id, r.name.as_str())).collect();
        let events: Vec<LiveEvent> = self.events.iter().map(|r| r.event).collect();
        let resolve = |id: u32| names.get(&id).map(|s| s.to_string()).unwrap_or_default();

        let mut events_buf = Vec::new();
        write_events_parquet(&mut events_buf, &events, resolve)?;
        let mut samples_buf = Vec::new();
        write_samples_parquet(&mut samples_buf, &self.samples)?;
        let mut frames_buf = Vec::new();
        write_frames_parquet(&mut frames_buf, &self.frames)?;
        let manifest = serde_json::to_string_pretty(&self.manifest_json())?;

        Ok(crate::zipstore::write_zip(
            &[
                (MANIFEST_FILE, manifest.as_bytes()),
                (EVENTS_PARQUET, &events_buf),
                (SAMPLES_PARQUET, &samples_buf),
                (FRAMES_PARQUET, &frames_buf),
            ],
            level,
        )?)
    }

    /// A bundle back from its zip. The manifest's `bundle` section is
    /// optional, so a zipped dataset directory (no names, no target) opens
    /// too.
    pub fn from_zip(bytes: &[u8]) -> Result<CaptureBundle, CaptureError> {
        let entries = read_store_zip(bytes)?;
        let find = |name: &str| -> Result<&[u8], CaptureError> {
            entries
                .iter()
                .find(|(n, _)| n == name || n.rsplit('/').next() == Some(name))
                .map(|(_, d)| d.as_slice())
                .ok_or_else(|| CaptureError::Manifest(format!("bundle has no {name}")))
        };
        let manifest_bytes = find(MANIFEST_FILE)?;
        let (target_pid, processes, threads) = crate::read_manifest_names(manifest_bytes)?;
        let manifest: serde_json::Value = serde_json::from_slice(manifest_bytes)?;
        // Parquet tables, or the Arrow IPC ones an earlier bundle carried.
        let owned = |b: &[u8]| bytes::Bytes::copy_from_slice(b);
        let events = match find(EVENTS_PARQUET) {
            Ok(b) => read_events_parquet(owned(b))?,
            Err(_) => read_events_ipc(std::io::Cursor::new(find(EVENTS_FILE)?))?,
        };
        let samples = match (find(SAMPLES_PARQUET), find(SAMPLES_FILE)) {
            (Ok(b), _) => read_samples_parquet(owned(b))?,
            (_, Ok(b)) => read_samples_ipc(std::io::Cursor::new(b))?,
            _ => Vec::new(),
        };
        let frames = match (find(FRAMES_PARQUET), find(FRAMES_FILE)) {
            (Ok(b), _) => read_frames_parquet(owned(b))?,
            (_, Ok(b)) => read_frames_ipc(std::io::Cursor::new(b))?,
            _ => Vec::new(),
        };
        let b = &manifest["bundle"];
        let slice_ns = match (&b["slice_ns"]["start"], &b["slice_ns"]["end"]) {
            (serde_json::Value::Number(a), serde_json::Value::Number(z)) => {
                Some((a.as_u64().unwrap_or(0), z.as_u64().unwrap_or(0)))
            }
            _ => None,
        };
        Ok(CaptureBundle {
            target_pid,
            slice_ns,
            processes,
            threads,
            events,
            samples,
            frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::kind;

    #[test]
    fn browser_stream_uses_exact_sample_rows_and_keeps_boundary_scopes() {
        use perfr_live_protocol::{decode_all, LiveFrame};
        let source = sample_bundle();
        let slice = source.slice(110, 220);
        let decoded = decode_all(&slice.to_stream()).unwrap();
        let events: Vec<_> = decoded.iter().filter_map(|f| match f {
            LiveFrame::EventBatch { events } => Some(events.clone()), _ => None,
        }).flatten().collect();
        for row in &slice.events { assert!(events.contains(&row.event)); }
        let sample_times: Vec<_> = events.iter().filter(|e| e.kind == kind::SAMPLE).map(|e| e.start_ns).collect();
        assert_eq!(sample_times, slice.samples.iter().map(|s| s.timestamp_ns).collect::<Vec<_>>());
        // Frame 1 "main" and scope name 1 "Tick" share an id in this fixture:
        // both names reach the viewer, the frame under a fresh id that the
        // sampled stacks use too.
        let strings: HashMap<u32, String> = decoded.iter().filter_map(|f| match f {
            LiveFrame::InternedString { id, text } => Some((*id, text.clone())), _ => None,
        }).collect();
        for row in &slice.events { assert_eq!(strings[&row.event.name_id], row.name); }
        for frame in &slice.frames {
            assert!(strings.values().any(|text| *text == frame.name), "frame {} missing", frame.name);
        }
        let stack_names: Vec<&str> = events.iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL && e.extra == perfr_live_event::extra::SAMPLED_FRAME)
            .map(|e| strings[&e.name_id].as_str()).collect();
        assert_eq!(stack_names, ["tick", "main", "physics"]);
        // The viewer forgets names on CaptureStarted: it must come first.
        let started = decoded.iter().position(|f| matches!(f, LiveFrame::CaptureStarted { .. })).unwrap();
        let first_name = decoded.iter().position(|f| matches!(f,
            LiveFrame::InternedString { .. } | LiveFrame::ThreadName { .. } | LiveFrame::ProcessName { .. })).unwrap();
        assert_eq!(started, 1);
        assert!(first_name > started);
        assert!(matches!(decoded.last(), Some(LiveFrame::CaptureFinished)));
    }

    #[test]
    fn browser_samples_ignore_crossing_old_flame_spans_and_restore_sample_only_stacks() {
        use perfr_live_protocol::{decode_all, LiveFrame};
        let mut bundle = sample_bundle();
        let mut old = ev(100, 100, 7, 70, 1, "main");
        old.event.kind = kind::FUNCTION_CALL;
        old.event.extra = perfr_live_event::extra::SAMPLED_FRAME;
        bundle.events.push(old); // The old flame span crosses the left edge.
        let slice = bundle.slice(110, 220);
        let events: Vec<_> = decode_all(&slice.to_stream()).unwrap().into_iter().filter_map(|f| match f {
            LiveFrame::EventBatch { events } => Some(events), _ => None,
        }).flatten().collect();
        let stacks: Vec<_> = events.iter().filter(|e| e.kind == kind::FUNCTION_CALL && e.extra == perfr_live_event::extra::SAMPLED_FRAME).collect();
        assert_eq!(stacks.len(), 3); // Two frames at 120; one at 210, no stack at 100.
        assert!(stacks.iter().all(|e| e.start_ns == 120 || e.start_ns == 210));
        assert_eq!(events.iter().filter(|e| e.kind == kind::SAMPLE).count(), 2);
    }

    fn ev(start: u64, dur: u64, pid: u32, tid: u32, name_id: u32, name: &str) -> EventRow {
        EventRow {
            event: LiveEvent {
                start_ns: start,
                duration_ns: dur,
                tid,
                pid,
                kind: kind::API_SCOPE,
                depth: 0,
                extra: 0,
                _pad: 0,
                name_id,
            },
            name: name.to_string(),
        }
    }

    fn sample_bundle() -> CaptureBundle {
        CaptureBundle {
            target_pid: 7,
            slice_ns: None,
            processes: vec![
                ProcessName { pid: 7, name: "game".into() },
                ProcessName { pid: 9, name: "other".into() },
            ],
            threads: vec![
                ThreadName { pid: 7, tid: 70, name: "Main".into() },
                ThreadName { pid: 7, tid: 71, name: "Worker".into() },
                ThreadName { pid: 9, tid: 90, name: "Idle".into() },
            ],
            events: vec![
                ev(100, 50, 7, 70, 1, "Tick"),      // 100..150
                ev(140, 100, 7, 71, 2, "Physics"),  // 140..240, straddles 200
                ev(300, 10, 7, 70, 1, "Tick"),      // 300..310
                ev(500, 10, 9, 90, 3, "Sleep"),     // 500..510
            ],
            samples: vec![
                SampleRow { timestamp_ns: 120, tid: 70, frames: vec![1, 2] },
                SampleRow { timestamp_ns: 210, tid: 71, frames: vec![3] },
                SampleRow { timestamp_ns: 505, tid: 90, frames: vec![4] },
            ],
            frames: vec![
                FrameRow { id: 1, name: "main".into(), module: "game".into(), address: 0x10 },
                FrameRow { id: 2, name: "tick".into(), module: "game".into(), address: 0x20 },
                FrameRow { id: 3, name: "physics".into(), module: "game".into(), address: 0x30 },
                FrameRow { id: 4, name: "nanosleep".into(), module: "libc".into(), address: 0x40 },
            ],
        }
    }

    #[test]
    fn a_bundle_round_trips_through_its_zip() {
        let b = sample_bundle();
        let zip = b.to_zip().unwrap();
        let back = CaptureBundle::from_zip(&zip).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.time_bounds(), Some((100, 510)));
    }

    #[test]
    fn a_slice_keeps_straddling_events_whole_and_cuts_samples_by_time() {
        let s = sample_bundle().slice(110, 220);
        assert_eq!(s.slice_ns, Some((110, 220)));
        // Tick (100..150) and Physics (140..240) overlap; the later Tick and
        // Sleep do not.
        let names: Vec<&str> = s.events.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["Tick", "Physics"]);
        assert_eq!(s.events[1].event.duration_ns, 100, "not clipped");
        // Samples at 120 and 210 are in, 505 is out; frame 4 is unreferenced.
        assert_eq!(s.samples.len(), 2);
        let ids: Vec<u32> = s.frames.iter().map(|f| f.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        // Process 9 and its thread had nothing in the window.
        assert_eq!(s.processes.len(), 1);
        assert_eq!(s.processes[0].pid, 7);
        assert_eq!(s.threads.len(), 2);
        // And the slice round-trips like the whole.
        let back = CaptureBundle::from_zip(&s.to_zip().unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn a_slice_cuts_a_sampled_stack_where_it_cuts_the_sample() {
        use perfr_live_event::extra::SAMPLED_FRAME;
        // What a sample leaves in the events: a tick at its timestamp, and
        // one frame event per level that lasts a sampling period.
        let period = 1_000;
        let stack = |timestamp: u64| -> Vec<EventRow> {
            let frame = |depth: u8, name: &str| {
                let mut row = ev(timestamp, period, 7, 70, 10 + u32::from(depth), name);
                row.event.kind = kind::FUNCTION_CALL;
                row.event.extra = SAMPLED_FRAME;
                row.event.depth = depth;
                row
            };
            let mut tick = ev(timestamp, period, 7, 70, 11, "tick");
            tick.event.kind = kind::SAMPLE;
            vec![tick, frame(0, "main"), frame(1, "tick")]
        };
        let mut b = sample_bundle();
        b.events = [stack(1_900), stack(2_500), stack(2_990), vec![ev(1_900, period, 7, 70, 1, "Tick")]].concat();
        b.samples = [1_900, 2_500, 2_990].map(|timestamp_ns| SampleRow { timestamp_ns, tid: 70, frames: vec![1, 2] }).to_vec();
        let s = b.slice(2_000, 3_000);
        // The sample at 1900 is before the window, and so is its stack,
        // although its frames last until 2900: drawn without the sample,
        // it was a stack at the left edge that the report did not count.
        let sampled: Vec<u64> = s.events.iter().filter(|r| r.event.extra == SAMPLED_FRAME).map(|r| r.event.start_ns).collect();
        assert_eq!(sampled, vec![2_500, 2_500, 2_990, 2_990]);
        assert_eq!(s.samples.iter().map(|x| x.timestamp_ns).collect::<Vec<_>>(), vec![2_500, 2_990]);
        assert_eq!(s.events.iter().filter(|r| r.event.kind == kind::SAMPLE).count(), 2);
        // A scope that was running at the left edge is another matter: kept whole.
        assert!(s.events.iter().any(|r| r.name == "Tick" && r.event.start_ns == 1_900));
        // The file path cuts the same way.
        let (from_file, _) = crate::slice_bundle_bytes(bytes::Bytes::from(b.to_zip().unwrap()), 2_000, 3_000).unwrap();
        assert_eq!(from_file, s);
    }

    #[test]
    fn a_reversed_window_and_an_empty_one_are_fine() {
        let b = sample_bundle();
        assert_eq!(b.slice(220, 110), b.slice(110, 220));
        let empty = b.slice(1000, 2000);
        assert!(empty.events.is_empty() && empty.samples.is_empty() && empty.frames.is_empty());
        assert_eq!(empty.time_bounds(), None);
        // The target process is still named, so the capture reads as its.
        assert_eq!(empty.processes.len(), 1);
        let back = CaptureBundle::from_zip(&empty.to_zip().unwrap()).unwrap();
        assert_eq!(back, empty);
    }

    #[test]
    fn names_are_the_distinct_intern_pairs() {
        assert_eq!(
            sample_bundle().names(),
            vec![(1, "Tick".to_string()), (2, "Physics".to_string()), (3, "Sleep".to_string())]
        );
    }

    #[test]
    fn the_zip_extracts_to_a_directory_the_manifest_reader_accepts() {
        let dir = std::env::temp_dir().join(format!("perfr-bundle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, data) in read_store_zip(&sample_bundle().to_zip().unwrap()).unwrap() {
            std::fs::write(dir.join(name), data).unwrap();
        }
        let m = crate::read_manifest(&dir).unwrap();
        assert_eq!(m.events, 4);
        assert_eq!(m.samples, 3);
        assert_eq!(m.frames, 4);
        assert_eq!(m.time_bounds_ns, Some((100, 510)));
        assert_eq!(m.files, vec!["events.parquet", "frames.parquet", "samples.parquet"]);
        let rows = read_events_parquet(std::fs::File::open(dir.join(EVENTS_PARQUET)).unwrap()).unwrap();
        assert_eq!(rows.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_and_drop_rows_round_trip() {
        let mut bundle = sample_bundle();
        let row = |kind_id: u8, duration_ns: u64, extra: u8, pad: u8, name_id: u32, name: &str| EventRow {
            event: LiveEvent {
                start_ns: 120,
                duration_ns,
                tid: 70,
                pid: 7,
                kind: kind_id,
                depth: 0,
                extra,
                _pad: pad,
                name_id,
            },
            name: name.into(),
        };
        bundle.events.push(row(kind::LOG, 0, 3, perfr_live_event::event_flags::TRUNCATED, 9, "tick"));
        bundle.events.push(row(kind::DROP, 41, 6, 0, 0, ""));
        let again = CaptureBundle::from_zip(&bundle.to_zip().unwrap()).unwrap();
        let log = again.events.iter().find(|r| r.event.kind == kind::LOG).expect("the log row");
        assert_eq!(log.name, "tick");
        assert_eq!(log.event.log_level(), Some(3));
        assert!(log.event.log_truncated(), "the truncated flag survives the file");
        let drop = again.events.iter().find(|r| r.event.kind == kind::DROP).expect("the drop row");
        assert_eq!(drop.event.drop_count(), Some(41), "a drop's count is not a duration");
        assert_eq!(drop.event.extra, 6);
    }

    #[test]
    fn a_bundle_saved_with_arrow_tables_in_a_deflated_zip_still_opens() {
        // The layout of the first day's bundles: IPC tables, deflate entries.
        let b = sample_bundle();
        let names: HashMap<u32, &str> = b.events.iter().map(|r| (r.event.name_id, r.name.as_str())).collect();
        let events: Vec<LiveEvent> = b.events.iter().map(|r| r.event).collect();
        let mut ev = Vec::new();
        crate::write_events_ipc(std::io::Cursor::new(&mut ev), &events, |id| names[&id].to_string()).unwrap();
        let mut sa = Vec::new();
        crate::write_samples_ipc(std::io::Cursor::new(&mut sa), &b.samples).unwrap();
        let mut fr = Vec::new();
        crate::write_frames_ipc(std::io::Cursor::new(&mut fr), &b.frames).unwrap();
        let mut manifest = b.manifest_json();
        manifest["files"] = serde_json::json!({"events": EVENTS_FILE, "samples": SAMPLES_FILE, "frames": FRAMES_FILE});
        let manifest = serde_json::to_string(&manifest).unwrap();
        let zip = crate::zipstore::write_zip(
            &[(MANIFEST_FILE, manifest.as_bytes()), (EVENTS_FILE, &ev), (SAMPLES_FILE, &sa), (FRAMES_FILE, &fr)],
            Some(6),
        )
        .unwrap();
        assert_eq!(CaptureBundle::from_zip(&zip).unwrap(), b);
    }

    /// A Parquet table of unsigned columns (`true` for 64-bit), two rows
    /// each: enough to write the first columns of the events, samples and
    /// frames tables with the right types, and stop short of the rest.
    fn narrow_table(columns: &[(&str, bool)]) -> Vec<u8> {
        use arrow_array::{Array, RecordBatch, UInt32Array, UInt64Array};
        use arrow_schema::{DataType, Field, Schema};
        use std::sync::Arc;
        let fields: Vec<Field> = columns
            .iter()
            .map(|(name, wide)| Field::new(*name, if *wide { DataType::UInt64 } else { DataType::UInt32 }, false))
            .collect();
        let arrays: Vec<Arc<dyn Array>> = columns
            .iter()
            .map(|(_, wide)| -> Arc<dyn Array> {
                if *wide { Arc::new(UInt64Array::from(vec![1u64, 2])) } else { Arc::new(UInt32Array::from(vec![1u32, 2])) }
            })
            .collect();
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let mut out = Vec::new();
        let mut writer = parquet::arrow::ArrowWriter::try_new(&mut out, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        out
    }

    #[test]
    fn a_bundle_whose_tables_have_too_few_columns_is_an_error_not_a_panic() {
        let good = read_store_zip(&sample_bundle().to_zip().unwrap()).unwrap();
        let with = |name: &str, table: &[u8]| {
            let entries: Vec<(&str, &[u8])> = good
                .iter()
                .map(|(n, d)| (n.as_str(), if n == name { table } else { d.as_slice() }))
                .collect();
            write_store_zip(&entries).unwrap()
        };
        // Every column present has the type its position calls for, so the
        // reader gets as far as the first one that is not there.
        let events = narrow_table(&[("start_ns", true), ("duration_ns", true), ("pid", false)]);
        let samples = narrow_table(&[("timestamp_ns", true), ("tid", false)]);
        let frames = narrow_table(&[("id", false)]);
        for (name, table) in [(EVENTS_PARQUET, &events), (SAMPLES_PARQUET, &samples), (FRAMES_PARQUET, &frames)] {
            let err = CaptureBundle::from_zip(&with(name, table)).expect_err(name).to_string();
            assert!(err.contains("column"), "{name}: {err}");
        }
    }

    /// Every prefix of `bytes`, then `bytes` with each bit flipped in turn.
    fn every_truncation_and_bit_flip(bytes: &[u8], mut check: impl FnMut(&[u8])) {
        for cut in 0..bytes.len() {
            check(&bytes[..cut]);
        }
        let mut flipped = bytes.to_vec();
        for bit in 0..bytes.len() * 8 {
            flipped[bit / 8] ^= 1 << (bit % 8);
            check(&flipped);
            flipped[bit / 8] ^= 1 << (bit % 8);
        }
    }

    #[test]
    fn no_truncation_or_bit_flip_of_a_valid_bundle_panics_the_reader() {
        let mut bundle = sample_bundle();
        bundle.events.truncate(2);
        bundle.samples.truncate(1);
        bundle.frames.truncate(2);
        let zip = bundle.to_zip().unwrap();
        assert_eq!(CaptureBundle::from_zip(&zip).unwrap(), bundle);
        // The zip as a whole: its directory, its headers, its CRCs. A
        // damaged table is stopped here by its entry's CRC and never
        // reaches the Parquet reader, which is why the tables are not swept
        // one by one below: parquet 59 itself panics, or asks for terabytes,
        // on some single-bit changes to a table, and only the CRC stands
        // between an accidentally damaged file and that.
        every_truncation_and_bit_flip(&zip, |bytes| {
            let _ = CaptureBundle::from_zip(bytes);
            let _ = crate::zipstore::stored_entry_ranges(bytes);
        });
        // The manifest is the one entry this crate parses itself.
        let entries = read_store_zip(&zip).unwrap();
        let (_, manifest) = entries.iter().find(|(name, _)| name == MANIFEST_FILE).unwrap();
        every_truncation_and_bit_flip(manifest, |bytes| {
            let _ = crate::read_manifest_names(bytes);
        });
    }

    #[test]
    fn something_that_is_not_a_capture_is_refused() {
        assert!(CaptureBundle::from_zip(b"not a zip").is_err());
        let zip = write_store_zip(&[("manifest.json", b"{\"format\":\"something-else/1\"}")]).unwrap();
        let err = CaptureBundle::from_zip(&zip).unwrap_err().to_string();
        assert!(err.contains("not a perfr capture"), "{err}");
    }
}
