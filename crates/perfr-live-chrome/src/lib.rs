// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Streaming Chrome Trace Event Format and Perfetto proto traces →
//! 32-byte [`LiveEvent`]s.
//!
//! JSON: an array of events (legacy `chrome://tracing`) or an object with
//! `traceEvents[]`, plus `stackFrames`, `samples` and `systemTraceEvents`.
//! Times are microseconds, as the format says; `displayTimeUnit` is a
//! display preference and does not change how they are read.
//! Proto: Perfetto `.pftrace` / `.perfetto-trace` / `.pb` (see
//! [`perfetto`](crate::perfetto) for what maps). gzip and a single-file zip
//! are decoded into the parser a slice at a time: the decoded text is never
//! in memory whole. What a caller pushes ahead of the parser is (compressed,
//! when the file is).
//!
//! [`LiveEvent`] stays 32 bytes. Args are interned as a compact hover string
//! and looked up with [`ArgKey`]. Memory-dump (`ph: v`) payloads are skipped.

mod id;
mod ingest;
mod json;
mod perfetto;
pub mod proto;
mod stream;

pub use ingest::{
    ArgKey, ChromeIngestor, FlowEdge, FlowEnd, IngestStats, TimeUnit, PID_GLOBAL, TID_ASYNC_BASE,
    TID_COUNTER_BASE, TID_GLOBAL, TID_OBJECT_BASE, TID_PROCESS_MARKERS,
};
pub use stream::{ingest_bytes, ingest_collect, ChromeStream, Format};

use perfr_live_event::LIVE_EVENT_SIZE;

/// Documented WASM / process heap budget for a loaded file session.
/// The live ring is not used; events go into the viewer's `TrackIndex`.
pub const TRACE_HEAP_HINT: &str = "\
Loaded traces use the viewer TrackIndex (32 bytes per event + interned \
names/args), not the 64 MB capture ring. gzip is inflated as chunks arrive \
(not buffered then decoded). wasm32 heap is capped at 2 GiB (`--max-memory`); \
a 1–2 GB uncompressed JSON fits when streamed (JSON is not retained). A \
single enormous memory-dump object still transits the scan window and is \
then dropped.";

pub fn live_event_size() -> usize {
    LIVE_EVENT_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::{kind, LIVE_EVENT_SIZE};
    use std::collections::HashSet;
    use std::io::Write;

    const FIXTURE: &str = r#"{
  "displayTimeUnit": "ms",
  "traceEvents": [
    {"name":"process_name","ph":"M","pid":1,"args":{"name":"Renderer"}},
    {"name":"thread_name","ph":"M","pid":1,"tid":10,"args":{"name":"Main"}},
    {"name":"process_sort_index","ph":"M","pid":1,"args":{"sort_index":-5}},
    {"name":"thread_sort_index","ph":"M","pid":1,"tid":10,"args":{"sort_index":1}},
    {"name":"outer","cat":"foo","ph":"B","ts":100,"pid":1,"tid":10,"args":{"k":1}},
    {"name":"inner","cat":"foo","ph":"B","ts":110,"pid":1,"tid":10},
    {"name":"inner","ph":"E","ts":130,"pid":1,"tid":10},
    {"name":"outer","ph":"E","ts":200,"pid":1,"tid":10},
    {"name":"complete","ph":"X","ts":210,"dur":40,"pid":1,"tid":10,"args":{"n":2}},
    {"name":"tick","ph":"I","s":"t","ts":220,"pid":1,"tid":10},
    {"name":"procMark","ph":"i","s":"p","ts":221,"pid":1},
    {"name":"globMark","ph":"I","s":"g","ts":222},
    {"name":"ram","ph":"C","ts":230,"pid":1,"args":{"a":10,"b":20}},
    {"name":"gpu","cat":"async","ph":"S","ts":240,"pid":1,"tid":10,"id":"0xabc"},
    {"name":"gpu","cat":"async","ph":"T","ts":250,"pid":1,"tid":99,"id":"0xabc"},
    {"name":"gpu","cat":"async","ph":"F","ts":280,"pid":1,"tid":10,"id":"0xabc"},
    {"name":"flowA","ph":"s","ts":150,"pid":1,"tid":10,"id":7},
    {"name":"flowB","ph":"t","ts":215,"pid":1,"tid":10,"id":7},
    {"name":"flowC","ph":"f","ts":225,"pid":1,"tid":10,"id":7},
    {"name":"mark","ph":"R","ts":300,"pid":1,"tid":10},
    {"name":"clock","ph":"c","ts":301,"pid":1,"tid":10},
    {"name":"obj","ph":"N","ts":310,"pid":1,"id":"o1"},
    {"name":"obj","ph":"O","ts":320,"pid":1,"id":"o1","args":{"x":1}},
    {"name":"obj","ph":"D","ts":330,"pid":1,"id":"o1"},
    {"name":"periodic_interval","ph":"v","ts":340,"pid":1,"tid":10,"args":{"dumps":{"huge":true}}},
    {"name":"sample","ph":"P","ts":350,"pid":1,"tid":10,"sf":"1"}
  ],
  "stackFrames": {
    "1": {"name":"leaf","parent":"2"},
    "2": {"name":"root"}
  }
}"#;

    fn collect(json: &str) -> (ChromeIngestor, Vec<perfr_live_event::LiveEvent>) {
        ingest_collect(json.as_bytes()).expect("ingest")
    }

    #[test]
    fn live_event_stays_32() {
        assert_eq!(LIVE_EVENT_SIZE, 32);
        assert_eq!(live_event_size(), 32);
    }

    #[test]
    fn fixture_pairs_and_names() {
        let (ing, evs) = collect(FIXTURE);
        assert_eq!(
            ing.process_names.get(&1).map(String::as_str),
            Some("Renderer")
        );
        assert_eq!(
            ing.thread_names.get(&(1, 10)).map(String::as_str),
            Some("Main")
        );
        assert_eq!(ing.process_sort.get(&1).copied(), Some(-5));
        assert_eq!(ing.thread_sort.get(&(1, 10)).copied(), Some(1));

        let scopes: Vec<_> = evs
            .iter()
            .filter(|e| e.kind == kind::API_SCOPE && e.tid == 10)
            .copied()
            .collect();
        let names = |id| ing.intern.get(id).unwrap_or("").to_string();
        let by_name = |n: &str| {
            scopes
                .iter()
                .find(|e| names(e.name_id) == n)
                .copied()
                .unwrap_or_else(|| panic!("missing {n}"))
        };
        let inner = by_name("inner");
        let outer = by_name("outer");
        assert_eq!(inner.start_ns, 110_000);
        assert_eq!(inner.duration_ns, 20_000);
        assert_eq!(inner.depth, 1);
        assert_eq!(outer.start_ns, 100_000);
        assert_eq!(outer.duration_ns, 100_000);
        assert_eq!(outer.depth, 0);
        let complete = by_name("complete");
        assert_eq!(complete.duration_ns, 40_000);
        assert!(ing.args.contains_key(&ArgKey::from_event(outer)));
        assert!(ing.args.contains_key(&ArgKey::from_event(complete)));

        let instants = evs
            .iter()
            .filter(|e| {
                matches!(
                    names(e.name_id).as_str(),
                    "tick" | "procMark" | "globMark" | "mark" | "clock"
                )
            })
            .count();
        assert_eq!(instants, 5);
        assert!(evs.iter().any(|e| e.tid == TID_PROCESS_MARKERS));
        assert!(evs
            .iter()
            .any(|e| e.pid == PID_GLOBAL && e.tid == TID_GLOBAL));

        let counters: Vec<_> = evs.iter().filter(|e| e.kind == kind::VALUE).collect();
        assert_eq!(counters.len(), 2);
        let labels: HashSet<_> = counters.iter().map(|e| names(e.name_id)).collect();
        assert!(labels.contains("ram:a"));
        assert!(labels.contains("ram:b"));
        assert_ne!(counters[0].tid, counters[1].tid);
        assert!(counters[0].tid >= TID_COUNTER_BASE);

        let asyncs: Vec<_> = evs.iter().filter(|e| e.kind == kind::API_TRACK).collect();
        assert!(asyncs
            .iter()
            .any(|e| names(e.name_id) == "gpu" && e.duration_ns > 1));
        let gpu = asyncs
            .iter()
            .find(|e| names(e.name_id) == "gpu" && e.duration_ns > 1)
            .unwrap();
        assert_eq!(gpu.duration_ns, 40_000);
        assert!(gpu.tid >= TID_ASYNC_BASE);
        assert_ne!(gpu.tid, 10, "async must not sit on the emitting tid");

        assert!(ing.flows.len() >= 2, "s→t and t→f");
        let flows = evs
            .iter()
            .filter(|e| names(e.name_id).starts_with("flow"))
            .count();
        assert_eq!(flows, 3);

        assert!(evs.iter().any(|e| names(e.name_id) == "periodic_interval"));
        let dump_key = evs
            .iter()
            .find(|e| names(e.name_id) == "periodic_interval")
            .map(|e| ArgKey::from_event(*e))
            .unwrap();
        let args = ing.intern.get(*ing.args.get(&dump_key).unwrap()).unwrap();
        assert!(args.contains("skipped"));
        assert!(!args.contains("huge"));

        let samples: Vec<_> = evs
            .iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL)
            .collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(names(samples[0].name_id), "root");
        assert_eq!(samples[0].depth, 0);
        assert_eq!(names(samples[1].name_id), "leaf");
        assert_eq!(samples[1].depth, 1);

        assert!(ing.stats.metadata >= 4);
        assert!(ing.stats.memory_dump >= 1);
    }

    #[test]
    fn legacy_array_and_numeric_ids() {
        let json = r#"[
          {"name":"A","ph":"B","ts":0,"pid":"42","tid":"7"},
          {"name":"A","ph":"E","ts":5,"pid":"42","tid":"7"}
        ]"#;
        let (ing, evs) = collect(json);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].pid, 42);
        assert_eq!(evs[0].tid, 7);
        assert_eq!(evs[0].duration_ns, 5_000);
        assert_eq!(ing.intern.get(evs[0].name_id), Some("A"));
    }

    /// `ts` and `dur` are microseconds whatever `displayTimeUnit` says: the
    /// key only chooses how a viewer prints them. Read as the unit of the
    /// data, a conforming trace that asks for nanosecond display came in a
    /// thousand times too short, and only for the events after the key.
    #[test]
    fn display_time_unit_does_not_rescale_timestamps() {
        let event = r#"{"name":"X","ph":"X","ts":1000,"dur":50,"pid":1,"tid":1}"#;
        for json in [
            format!(r#"{{"displayTimeUnit":"ns","traceEvents":[{event}]}}"#),
            format!(r#"{{"traceEvents":[{event}],"displayTimeUnit":"ns"}}"#),
            format!(r#"{{"displayTimeUnit":"ms","traceEvents":[{event}]}}"#),
            format!(r#"{{"displayTimeUnit":"ns","samples":[],"traceEvents":[{event}]}}"#),
        ] {
            let (_ing, evs) = collect(&json);
            assert_eq!((evs[0].start_ns, evs[0].duration_ns), (1_000_000, 50_000), "{json}");
        }
    }

    #[test]
    fn nestable_async_b_e() {
        let json = r#"[
          {"name":"job","ph":"b","ts":0,"pid":1,"tid":1,"id":"x"},
          {"name":"job","ph":"e","ts":40,"pid":1,"tid":1,"id":"x"}
        ]"#;
        let (ing, evs) = collect(json);
        let tracks: Vec<_> = evs.iter().filter(|e| e.kind == kind::API_TRACK).collect();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].duration_ns, 40_000);
        assert!(tracks[0].tid >= TID_ASYNC_BASE);
        assert_ne!(tracks[0].tid, 1);
        assert_eq!(ing.stats.async_ev, 2);
    }

    #[test]
    fn system_trace_events_array_and_systrace_string() {
        let json = r#"{
          "traceEvents":[{"name":"X","ph":"X","ts":1,"dur":1,"pid":1,"tid":1}],
          "systemTraceEvents":[
            {"name":"sys","ph":"X","ts":2,"dur":3,"pid":9,"tid":9}
          ]
        }"#;
        let (_ing, evs) = collect(json);
        assert_eq!(evs.len(), 2);
        let names: HashSet<_> = evs.iter().map(|e| e.pid).collect();
        assert!(names.contains(&1) && names.contains(&9));

        let systrace = r##"{
          "traceEvents":[],
          "systemTraceEvents":"# tracer: nop\n          chrome-42  [000] ....  1.500000: tracing_mark_write: B|42|Pump\n          chrome-42  [000] ....  1.500010: tracing_mark_write: E|42\n          chrome-42  [000] ....  1.500020: tracing_mark_write: C|42|cpu|7\n"
        }"##;
        let (ing, evs) = collect(systrace);
        assert_eq!(ing.stats.system_trace, 3);
        assert!(evs
            .iter()
            .any(|e| e.kind == kind::API_SCOPE && e.duration_ns == 10_000));
        assert!(
            evs.iter().any(|e| e.kind == kind::VALUE && e.value_f32() == Some(7.0)),
            "C|42|cpu|7 carries its value"
        );
    }

    #[test]
    fn global_async_ids_pair_across_processes() {
        let json = r#"[
          {"name":"nav","ph":"b","ts":0,"pid":1,"tid":1,"id2":{"global":"0x9"}},
          {"name":"nav","ph":"e","ts":10,"pid":2,"tid":2,"id2":{"global":"0x9"}},
          {"name":"loc","ph":"b","ts":0,"pid":1,"tid":1,"id2":{"local":"0x9"}},
          {"name":"loc","ph":"e","ts":10,"pid":2,"tid":2,"id2":{"local":"0x9"}}
        ]"#;
        let (ing, evs) = collect(json);
        let nav = evs
            .iter()
            .find(|e| ing.intern.get(e.name_id) == Some("nav"))
            .unwrap();
        assert_eq!((nav.pid, nav.duration_ns), (1, 10_000), "closed on the begin's lane");
        assert_eq!(ing.stats.unmatched_end, 1, "local ids stay per process");
    }

    #[test]
    fn b_event_flow_ids_bind_on_the_slice() {
        // bind_id / flow_out sit on the B (JSON) or the begin (Perfetto);
        // the E carries nothing. The edge must still be drawn.
        let json = r#"[
          {"name":"post","ph":"B","ts":10,"pid":1,"tid":1,"bind_id":"0x5","flow_out":true},
          {"name":"post","ph":"E","ts":20,"pid":1,"tid":1},
          {"name":"run","ph":"i","ts":30,"pid":1,"tid":2,"bind_id":"0x5","flow_in":true},
          {"name":"step","ph":"X","ts":40,"dur":1,"pid":1,"tid":3,"bind_id":"0x6","flow_out":true},
          {"name":"step","ph":"X","ts":50,"dur":1,"pid":1,"tid":4,"bind_id":"0x6","flow_in":true,"flow_out":true},
          {"name":"step","ph":"X","ts":60,"dur":1,"pid":1,"tid":5,"bind_id":"0x6","flow_in":true}
        ]"#;
        let (ing, _evs) = collect(json);
        assert_eq!(ing.flows.len(), 3, "{:?}", ing.flows);
        assert_eq!((ing.flows[0].from.tid, ing.flows[0].to.tid), (1, 2));
        assert_eq!((ing.flows[1].from.tid, ing.flows[1].to.tid), (3, 4));
        assert_eq!((ing.flows[2].from.tid, ing.flows[2].to.tid), (4, 5));
    }

    #[test]
    fn theverge_public_fixture_if_present() {
        let path = "/tmp/chrome-traces/theverge_trace.json";
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let (ing, evs) = ingest_collect(&bytes).expect("theverge");
        assert_eq!(ing.stats.events_in, 58_103);
        assert_eq!(ing.stats.duration, 54_224);
        assert_eq!(ing.stats.counter, 344);
        assert_eq!(ing.stats.async_ev, 785);
        assert!(ing.stats.object >= 2_101);
        assert!(evs.len() >= 30_000);
        assert!(ing.process_names.len() >= 6);
        let n66343 = ing.thread_names.keys().filter(|(p, _)| *p == 66343).count();
        assert!(
            n66343 < 32,
            "pid 66343 must not explode into {n66343} threads (group O/D and async by name)"
        );
        let (a, b) = ing.content_time_bounds().expect("theverge content");
        let span_s = (b - a) as f64 / 1e9;
        assert!(
            a > 100_000_000_000_000,
            "metadata ts=0 must not set content min: {a}"
        );
        assert!(
            (8.2..8.3).contains(&span_s),
            "theverge B/E cluster must be ~8.24 s, got {span_s} s ({a}..{b})"
        );
        let one_ns_scopes = evs
            .iter()
            .filter(|e| e.kind == kind::API_SCOPE && e.duration_ns == 1)
            .count();
        let mut lane_mix = std::collections::HashMap::<(u32, u32, u8, u8), (u32, u32)>::new();
        for e in &evs {
            if e.kind != kind::API_SCOPE {
                continue;
            }
            let slot = lane_mix
                .entry((e.pid, e.tid, e.kind, e.depth))
                .or_insert((0, 0));
            if e.duration_ns == 1 {
                slot.0 += 1;
            } else {
                slot.1 += 1;
            }
        }
        let shared = lane_mix
            .values()
            .filter(|(ones, longer)| *ones > 0 && *longer > 0)
            .count();
        eprintln!(
            "theverge API_SCOPE duration_ns==1: {one_ns_scopes} of {}, \
             lanes sharing 1 ns + longer: {shared} / {}",
            evs.iter().filter(|e| e.kind == kind::API_SCOPE).count(),
            lane_mix.len()
        );
        assert!(
            one_ns_scopes > 0,
            "theverge fixture is expected to emit 1 ns API_SCOPE instants"
        );
        assert!(
            shared > 0,
            "theverge 1 ns scopes must share lanes with longer B/E/X work"
        );
    }

    #[test]
    fn content_bounds_ignore_metadata_and_zero_instants() {
        let json = r#"[
          {"name":"thread_name","ph":"M","ts":0,"pid":1,"tid":1,"args":{"name":"Main"}},
          {"name":"process_name","ph":"M","ts":0,"pid":1,"args":{"name":"Browser"}},
          {"name":"orphan","ph":"I","ts":0,"pid":1,"tid":1},
          {"name":"work","ph":"B","ts":122403254982,"pid":1,"tid":1},
          {"name":"work","ph":"E","ts":122411498936,"pid":1,"tid":1}
        ]"#;
        let (ing, evs) = collect(json);
        let (a, b) = ing.content_time_bounds().expect("content");
        assert_eq!(a, 122_403_254_982_000);
        assert_eq!(b, 122_411_498_936_000);
        assert!(
            evs.iter().any(|e| e.start_ns == 0),
            "instant at 0 is stored"
        );
        assert!(
            evs.iter().any(|e| e.start_ns == 122_403_254_982_000),
            "real B is stored"
        );
    }

    #[test]
    fn many_object_and_async_ids_share_name_lanes() {
        let mut json = String::from("[");
        json.push_str(r#"{"name":"process_name","ph":"M","pid":7,"args":{"name":"Renderer"}},"#);
        json.push_str(r#"{"name":"thread_name","ph":"M","pid":7,"tid":1,"args":{"name":"Main"}},"#);
        for i in 0..400 {
            if i > 0 {
                json.push(',');
            }
            use std::fmt::Write;
            write!(
                json,
                r#"{{"name":"cc::Tile","ph":"O","ts":{i},"pid":7,"id":"{i}"}},"#,
            )
            .unwrap();
            write!(
                json,
                r#"{{"name":"cc::Tile","ph":"D","ts":{},"pid":7,"id":"{i}"}},"#,
                i + 1
            )
            .unwrap();
            write!(
                json,
                r#"{{"name":"PendingTree","ph":"S","ts":{i},"pid":7,"tid":1,"id":{i}}},"#,
            )
            .unwrap();
            write!(
                json,
                r#"{{"name":"PendingTree","ph":"F","ts":{},"pid":7,"tid":1,"id":{i}}}"#,
                i + 5
            )
            .unwrap();
        }
        json.push(']');
        let (ing, evs) = collect(&json);
        let tids: HashSet<u32> = ing
            .thread_names
            .keys()
            .filter(|(p, _)| *p == 7)
            .map(|(_, t)| *t)
            .collect();
        assert!(
            tids.len() < 8,
            "400 object ids + 400 async ids must share name lanes, got {} tids: {tids:?}",
            tids.len()
        );
        let objects: Vec<_> = evs
            .iter()
            .filter(|e| ing.intern.get(e.name_id) == Some("cc::Tile"))
            .collect();
        assert_eq!(objects.len(), 800);
        let obj_tids: HashSet<_> = objects.iter().map(|e| e.tid).collect();
        assert_eq!(obj_tids.len(), 1);
        let asyncs: Vec<_> = evs.iter().filter(|e| e.kind == kind::API_TRACK).collect();
        assert!(!asyncs.is_empty());
        let async_tids: HashSet<_> = asyncs.iter().map(|e| e.tid).collect();
        assert_eq!(async_tids.len(), 1);
        assert_ne!(*obj_tids.iter().next().unwrap(), 1);
        assert_ne!(*async_tids.iter().next().unwrap(), 1);
    }

    #[test]
    fn nestable_async_b_n_e_and_legacy_o_d() {
        // Spec: b = begin, n = instant, e = end. Old catapult dumps also
        // wrote o (instant) and d (end); both shapes pair on one lane.
        let json = r#"[
          {"name":"job","ph":"b","ts":0,"pid":2,"id":1},
          {"name":"step","ph":"n","ts":1,"pid":2,"id":1},
          {"name":"job","ph":"e","ts":10,"pid":2,"id":1},
          {"name":"job","ph":"b","ts":20,"pid":2,"id":2},
          {"name":"step","ph":"o","ts":21,"pid":2,"id":2},
          {"name":"job","ph":"d","ts":30,"pid":2,"id":2}
        ]"#;
        let (ing, evs) = collect(json);
        let tracks: Vec<_> = evs.iter().filter(|e| e.kind == kind::API_TRACK).collect();
        assert_eq!(tracks.iter().filter(|e| e.duration_ns == 10_000).count(), 2);
        assert_eq!(
            tracks.iter().filter(|e| ing.intern.get(e.name_id) == Some("step")).count(),
            2,
            "n and o are instants, not opens: {tracks:?}"
        );
        assert!(tracks
            .iter()
            .any(|e| ing.intern.get(e.name_id) == Some("step")));
        assert_eq!(
            tracks.iter().map(|e| e.tid).collect::<HashSet<_>>().len(),
            1
        );
    }

    #[test]
    fn gzip_multimember() {
        let mut a = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        a.write_all(br#"[{"name":"A","ph":"X","ts":1,"dur":1,"pid":1,"tid":1},"#)
            .unwrap();
        let mut gz = a.finish().unwrap();
        let mut b = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        b.write_all(br#"{"name":"B","ph":"X","ts":2,"dur":1,"pid":1,"tid":1}]"#)
            .unwrap();
        gz.extend(b.finish().unwrap());
        let (_ing, evs) = ingest_collect(&gz).expect("multi-gz");
        assert_eq!(evs.len(), 2);
    }

    #[test]
    fn gzip_roundtrip() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(br#"[{"name":"Z","ph":"X","ts":1,"dur":2,"pid":3,"tid":4}]"#)
            .unwrap();
        let gz = enc.finish().unwrap();
        let (_ing, evs) = ingest_collect(&gz).expect("gzip");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].pid, 3);
        assert_eq!(evs[0].duration_ns, 2_000);
    }

    #[test]
    fn gzip_emits_events_before_eof() {
        // Inflate-as-we-go: events must appear after a prefix of the gzip,
        // before finish_input(), so a multi-GB .json.gz cannot require the
        // whole file in RAM first.
        const N: usize = 8_000;
        let mut json = String::from("[");
        for i in 0..N {
            if i > 0 {
                json.push(',');
            }
            use std::fmt::Write;
            write!(
                json,
                r#"{{"name":"X","ph":"X","ts":{i},"dur":10,"pid":1,"tid":1}}"#
            )
            .unwrap();
        }
        json.push(']');
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(json.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut evs = 0usize;
        let mid = gz.len() / 2;
        stream.push(&gz[..mid.max(64)]);
        evs += stream.pump(&mut ing, 64 * 1024).len();
        // Parsed, not emitted: an `X` waits for the end of the file to
        // learn its row.
        assert!(
            stream.events_seen > 0,
            "streaming gzip must parse events before the last compressed byte"
        );
        stream.push(&gz[mid.max(64)..]);
        stream.finish_input();
        evs += stream.pump(&mut ing, 64 * 1024).len();
        evs += ing.finish(1).len();
        assert_eq!(evs, N);
    }

    #[test]
    fn stream_chunks_match_whole() {
        let bytes = FIXTURE.as_bytes();
        let whole = ingest_collect(bytes).unwrap().1;
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        let mut evs = Vec::new();
        for chunk in bytes.chunks(17) {
            stream.push(chunk);
            evs.extend(stream.pump(&mut ing, 8));
        }
        stream.finish_input();
        evs.extend(stream.pump(&mut ing, 1024));
        evs.extend(ing.finish(evs.iter().map(|e| e.end_ns()).max().unwrap_or(1)));
        assert_eq!(evs.len(), whole.len());
    }

    #[test]
    fn stream_hundreds_of_thousands() {
        const N: usize = 250_000;
        let mut json = String::with_capacity(N * 64);
        json.push('[');
        for i in 0..N {
            if i > 0 {
                json.push(',');
            }
            use std::fmt::Write;
            write!(
                json,
                r#"{{"name":"X","ph":"X","ts":{i},"dur":10,"pid":1,"tid":1}}"#
            )
            .unwrap();
        }
        json.push(']');
        let t0 = std::time::Instant::now();
        let (ing, evs) = ingest_collect(json.as_bytes()).expect("stream");
        let dt = t0.elapsed();
        assert_eq!(evs.len(), N);
        assert_eq!(ing.stats.complete as usize, N);
        assert!(evs.iter().all(|e| e.kind == kind::API_SCOPE));
        assert!(
            dt.as_secs_f64() < 30.0,
            "250k events took {dt:?} (should be well under 30s)"
        );
    }

    #[test]
    fn long_args_are_cut_between_characters_wherever_the_limit_falls() {
        // serde_json writes non-ASCII text as is, so the 512-byte hover limit
        // can land inside a character. `{"k":"` is 6 bytes: `pad` ASCII
        // letters put the next character at byte `6 + pad`.
        for ch in ['é', '€', '😀'] {
            for offset in 509..=513usize {
                let value = format!("{}{}", "a".repeat(offset - 6), ch.to_string().repeat(40));
                let json = format!(
                    r#"[{{"name":"X","ph":"X","ts":1,"dur":10,"pid":1,"tid":1,"args":{{"k":"{value}"}}}}]"#
                );
                let (ing, evs) = collect(&json);
                let id = *ing.args.get(&ArgKey::from_event(evs[0])).expect("args kept");
                let text = ing.intern.get(id).unwrap();
                let full = format!(r#"{{"k":"{value}"}}"#);
                let kept = text.strip_suffix('…').unwrap_or_else(|| panic!("{ch:?} at {offset}: no ellipsis"));
                assert!(full.starts_with(kept), "{ch:?} at {offset}: not a prefix");
                // Never more than the limit, and never short by more than
                // the character the limit fell in.
                assert!(kept.len() <= 512 && kept.len() > 512 - ch.len_utf8(), "{ch:?} at {offset}: kept {}", kept.len());
            }
        }
    }

    /// Depth of each named scope on thread (1, 1), and a check that no row
    /// holds two slices that overlap.
    fn rows_of(json: &str) -> std::collections::HashMap<String, u8> {
        let (ing, evs) = collect(json);
        let scopes: Vec<_> = evs
            .iter()
            .filter(|e| e.kind == kind::API_SCOPE && e.pid == 1 && e.tid == 1)
            .collect();
        for a in &scopes {
            for b in &scopes {
                let same = a.start_ns == b.start_ns && a.name_id == b.name_id;
                assert!(
                    same || a.depth != b.depth || a.end_ns() <= b.start_ns || b.end_ns() <= a.start_ns,
                    "{:?} and {:?} overlap on row {}",
                    ing.intern.get(a.name_id),
                    ing.intern.get(b.name_id),
                    a.depth
                );
            }
        }
        scopes
            .iter()
            .map(|e| (ing.intern.get(e.name_id).unwrap_or("").to_string(), e.depth))
            .collect()
    }

    #[test]
    fn nested_complete_events_get_a_row_each_whichever_is_written_first() {
        // Start order, as a sorted export writes them.
        let rows = rows_of(
            r#"[
          {"name":"outer","ph":"X","ts":0,"dur":100,"pid":1,"tid":1},
          {"name":"inner","ph":"X","ts":10,"dur":20,"pid":1,"tid":1},
          {"name":"leaf","ph":"X","ts":12,"dur":5,"pid":1,"tid":1},
          {"name":"sibling","ph":"X","ts":40,"dur":20,"pid":1,"tid":1},
          {"name":"next","ph":"X","ts":100,"dur":50,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!(rows["outer"], 0);
        assert_eq!(rows["inner"], 1);
        assert_eq!(rows["leaf"], 2);
        assert_eq!(rows["sibling"], 1, "a row is reused once its slice has ended");
        assert_eq!(rows["next"], 0);
        // End order, as a tracer that writes a scope when it closes does:
        // every child comes before its parent.
        let rows = rows_of(
            r#"[
          {"name":"leaf","ph":"X","ts":12,"dur":5,"pid":1,"tid":1},
          {"name":"inner","ph":"X","ts":10,"dur":20,"pid":1,"tid":1},
          {"name":"sibling","ph":"X","ts":40,"dur":20,"pid":1,"tid":1},
          {"name":"outer","ph":"X","ts":0,"dur":100,"pid":1,"tid":1},
          {"name":"next","ph":"X","ts":100,"dur":50,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!(
            (rows["outer"], rows["inner"], rows["leaf"], rows["sibling"], rows["next"]),
            (0, 1, 2, 1, 0)
        );
    }

    #[test]
    fn complete_events_and_begin_end_pairs_nest_in_each_other() {
        // X inside an open B.
        let rows = rows_of(
            r#"[
          {"name":"b","ph":"B","ts":0,"pid":1,"tid":1},
          {"name":"x","ph":"X","ts":10,"dur":20,"pid":1,"tid":1},
          {"name":"x2","ph":"X","ts":12,"dur":5,"pid":1,"tid":1},
          {"name":"b","ph":"E","ts":100,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!((rows["b"], rows["x"], rows["x2"]), (0, 1, 2));
        // A B/E pair inside an X that is written after it, when it ends.
        let rows = rows_of(
            r#"[
          {"name":"b","ph":"B","ts":10,"pid":1,"tid":1},
          {"name":"deep","ph":"B","ts":12,"pid":1,"tid":1},
          {"name":"deep","ph":"E","ts":15,"pid":1,"tid":1},
          {"name":"b","ph":"E","ts":30,"pid":1,"tid":1},
          {"name":"x","ph":"X","ts":0,"dur":100,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!((rows["x"], rows["b"], rows["deep"]), (0, 1, 2));
        // A B still open at the end of the file contains the X written in it.
        let rows = rows_of(
            r#"[
          {"name":"open","ph":"B","ts":0,"pid":1,"tid":1},
          {"name":"x","ph":"X","ts":10,"dur":20,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!((rows["open"], rows["x"]), (0, 1));
    }

    #[test]
    fn overlapping_complete_events_that_do_not_nest_still_get_separate_rows() {
        let rows = rows_of(
            r#"[
          {"name":"a","ph":"X","ts":0,"dur":50,"pid":1,"tid":1},
          {"name":"b","ph":"X","ts":30,"dur":50,"pid":1,"tid":1},
          {"name":"c","ph":"X","ts":60,"dur":10,"pid":1,"tid":1}
        ]"#,
        );
        assert_eq!((rows["a"], rows["b"], rows["c"]), (0, 1, 0));
    }

    #[test]
    fn a_thread_of_begin_end_pairs_still_streams_and_keeps_its_stack_depths() {
        // Far more pairs than a thread may hold while it is undecided: the
        // slices must come out of `pump`, before `finish`.
        const N: usize = 10_000;
        let mut json = String::from("[");
        for i in 0..N {
            use std::fmt::Write;
            let t = i * 10;
            write!(
                json,
                r#"{{"name":"o","ph":"B","ts":{t},"pid":1,"tid":1}},{{"name":"i","ph":"B","ts":{},"pid":1,"tid":1}},{{"name":"i","ph":"E","ts":{},"pid":1,"tid":1}},{{"name":"o","ph":"E","ts":{},"pid":1,"tid":1}},"#,
                t + 1,
                t + 2,
                t + 3
            )
            .unwrap();
        }
        json.push_str(r#"{"name":"last","ph":"i","ts":1,"pid":2,"tid":2}]"#);
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        stream.push(json.as_bytes());
        stream.finish_input();
        let mut pumped = Vec::new();
        loop {
            let before = stream.events_seen;
            pumped.extend(stream.pump(&mut ing, 64 * 1024));
            if stream.events_seen == before {
                break;
            }
        }
        let slices = pumped.iter().filter(|e| e.pid == 1).count();
        assert!(slices > N, "only {slices} of {} slices before the end of the file", 2 * N);
        pumped.extend(ing.finish(1));
        let name = |e: &perfr_live_event::LiveEvent| ing.intern.get(e.name_id).unwrap_or("");
        assert_eq!(pumped.iter().filter(|e| e.pid == 1).count(), 2 * N);
        assert!(pumped.iter().filter(|e| e.pid == 1).all(|e| e.depth == u8::from(name(e) == "i")));
    }

    #[test]
    fn open_scopes_are_closed_at_the_end_of_the_trace_not_of_the_last_batch() {
        // The viewer passes `finish` the end of the last batch it pumped.
        // Files are not in time order across processes, so that batch can
        // lie well before the end of the trace -- here it is one early
        // instant of another process.
        let json = r#"[
          {"name":"open","ph":"B","ts":100,"pid":1,"tid":1},
          {"name":"job","ph":"b","ts":150,"pid":1,"tid":1,"id":"7"},
          {"name":"late","ph":"B","ts":900,"pid":1,"tid":2},
          {"name":"late","ph":"E","ts":1000,"pid":1,"tid":2},
          {"name":"early","ph":"i","ts":5,"pid":2,"tid":2}
        ]"#;
        let mut stream = ChromeStream::default();
        let mut ing = ChromeIngestor::default();
        stream.push(json.as_bytes());
        stream.finish_input();
        let mut evs = stream.pump(&mut ing, 1024);
        let last_batch_end = evs.iter().map(|e| e.end_ns()).max().unwrap();
        assert_eq!(last_batch_end, 5_001);
        evs.extend(ing.finish(last_batch_end));
        let by_name = |n: &str| {
            *evs.iter()
                .find(|e| ing.intern.get(e.name_id) == Some(n))
                .unwrap_or_else(|| panic!("missing {n}"))
        };
        assert_eq!(by_name("open").end_ns(), 1_000_000);
        assert_eq!(by_name("job").end_ns(), 1_000_000);
    }

    #[test]
    fn async_slices_that_do_not_close_in_order_never_share_a_row() {
        // a opens, b opens, a closes, c opens while b is still open.
        let json = r#"[
          {"name":"req","ph":"S","ts":0,"pid":1,"tid":1,"id":"a"},
          {"name":"req","ph":"S","ts":10,"pid":1,"tid":1,"id":"b"},
          {"name":"req","ph":"F","ts":20,"pid":1,"tid":1,"id":"a"},
          {"name":"req","ph":"S","ts":30,"pid":1,"tid":1,"id":"c"},
          {"name":"req","ph":"T","ts":35,"pid":1,"tid":1,"id":"c"},
          {"name":"req","ph":"F","ts":40,"pid":1,"tid":1,"id":"b"},
          {"name":"req","ph":"S","ts":45,"pid":1,"tid":1,"id":"d"},
          {"name":"req","ph":"F","ts":50,"pid":1,"tid":1,"id":"c"},
          {"name":"req","ph":"F","ts":60,"pid":1,"tid":1,"id":"d"}
        ]"#;
        let (_ing, evs) = collect(json);
        let slices: Vec<_> = evs.iter().filter(|e| e.kind == kind::API_TRACK).collect();
        for a in &slices {
            for b in &slices {
                assert!(
                    a.start_ns == b.start_ns || a.depth != b.depth || a.end_ns() <= b.start_ns || b.end_ns() <= a.start_ns,
                    "{a:?} and {b:?} overlap on one row"
                );
            }
        }
        let row = |start_us: u64, dur: u64| {
            slices
                .iter()
                .find(|e| e.start_ns == start_us * 1000 && e.duration_ns == dur)
                .unwrap_or_else(|| panic!("no slice at {start_us}"))
                .depth
        };
        assert_eq!(row(0, 20_000), 0, "a");
        assert_eq!(row(10, 30_000), 1, "b");
        assert_eq!(row(30, 20_000), 0, "c takes the row a left");
        assert_eq!(row(45, 15_000), 1, "d takes the row b left");
        assert_eq!(row(35, 1), 2, "an instant sits under every open slice");
    }

    #[test]
    fn be_pairing_by_id() {
        let json = r#"[
          {"name":"A","ph":"B","ts":0,"pid":1,"tid":1,"id":"x"},
          {"name":"B","ph":"B","ts":1,"pid":1,"tid":1,"id":"y"},
          {"name":"A","ph":"E","ts":10,"pid":1,"tid":1,"id":"x"},
          {"name":"B","ph":"E","ts":20,"pid":1,"tid":1,"id":"y"}
        ]"#;
        let (ing, evs) = collect(json);
        let a = evs
            .iter()
            .find(|e| ing.intern.get(e.name_id) == Some("A"))
            .unwrap();
        let b = evs
            .iter()
            .find(|e| ing.intern.get(e.name_id) == Some("B"))
            .unwrap();
        assert_eq!(a.duration_ns, 10_000);
        assert_eq!(b.duration_ns, 19_000);
    }
}
