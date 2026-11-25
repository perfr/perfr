// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr diff`: what changed between saved captures, and the same asked
//! of the service about the capture it holds.
//!
//! The comparison itself is `perfr-diff`, which knows nothing of files.
//! This is the part that reads `.perfr.zip` bundles into what it takes.

use std::collections::{HashMap, HashSet};

use perfr_capture::CaptureBundle;
use perfr_diff::text::{render, TextOptions};
use perfr_diff::{CaptureInput, Diff, Frame, Options, Per, Sample, Span, SpanKind};
use perfr_live_event::{extra, kind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

#[derive(Clone, Debug)]
pub struct DiffOpts {
    pub base: Vec<String>,
    pub new: Vec<String>,
    pub per: String,
    pub confidence: f64,
    /// Percent.
    pub threshold: f64,
    /// Percent.
    pub min_share: f64,
    pub top: usize,
    pub format: Format,
    pub fail_on_regression: bool,
}

/// What a comparison reads of a bundle. Scheduling, thread states, values
/// and log lines are not compared; the frames of sampled stacks are drawn
/// events and not measured calls, and the samples themselves carry them.
///
/// A capture of a process is compared for that process, for the ones
/// sampled with it (what it launched, and the others it was started on)
/// and for what an agent marked. Any program that uses the API has its
/// scopes in every capture taken while it runs, the service itself among
/// them, and those say nothing of the program the capture is of. A
/// capture with no target is of whoever instruments itself, and all of it
/// is read.
pub fn input_of(label: &str, bundle: CaptureBundle) -> CaptureInput {
    let mut input = CaptureInput { label: label.to_string(), ..CaptureInput::default() };
    let mut names: HashMap<String, u32> = HashMap::new();
    let mut periods: HashMap<u64, u32> = HashMap::new();
    let process_of: HashMap<u32, u32> = bundle.threads.iter().map(|thread| (thread.tid, thread.pid)).collect();
    let mut captured: HashSet<u32> = bundle.samples.iter().filter_map(|sample| process_of.get(&sample.tid).copied()).collect();
    captured.extend([bundle.target_pid, perfr_live_event::AGENT_PID]);
    for row in &bundle.events {
        let event = &row.event;
        if bundle.target_pid != 0 && !captured.contains(&event.pid) {
            continue;
        }
        let span_kind = match event.kind {
            kind::API_SCOPE => SpanKind::Scope,
            kind::FUNCTION_CALL if event.extra != extra::SAMPLED_FRAME => SpanKind::Function,
            kind::FRAME => SpanKind::Frame,
            kind::API_TRACK if event.extra == extra::GPU_WORK => SpanKind::Gpu,
            kind::API_TRACK => SpanKind::Track,
            kind::SAMPLE => {
                *periods.entry(event.duration_ns).or_insert(0) += 1;
                continue;
            }
            _ => continue,
        };
        let name = match names.get(&row.name) {
            Some(index) => *index,
            None => {
                let index = input.names.len() as u32;
                input.names.push(row.name.clone());
                names.insert(row.name.clone(), index);
                index
            }
        };
        input.spans.push(Span { kind: span_kind, name, start_ns: event.start_ns, duration_ns: event.duration_ns });
    }
    // A sample's mark lasts the sampling period; the commonest is the
    // capture's.
    input.sample_period_ns =
        periods.into_iter().filter(|(period, _)| *period > 0).max_by_key(|(period, count)| (*count, *period)).map(|(period, _)| period);
    input.frames =
        bundle.frames.into_iter().map(|frame| (frame.id, Frame { name: frame.name, module: frame.module })).collect();
    input.samples =
        bundle.samples.into_iter().map(|sample| Sample { timestamp_ns: sample.timestamp_ns, frames: sample.frames }).collect();
    input
}

/// A `.perfr.zip` as a comparison's input, called by its file name.
pub fn read(path: &str) -> Result<CaptureInput, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("{path}: {error}"))?;
    let bundle = CaptureBundle::from_zip(&bytes).map_err(|error| format!("{path}: not a capture ({error})"))?;
    let label = std::path::Path::new(path).file_name().map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned());
    Ok(input_of(&label, bundle))
}

/// The options of a comparison from what a command line or a request
/// gives: the threshold and the share in percent, and anything left out
/// at its default.
pub fn options(per: &str, confidence: Option<f64>, threshold: Option<f64>, min_share: Option<f64>) -> Result<Options, String> {
    let defaults = Options::default();
    let fraction = |name: &str, percent: Option<f64>, default: f64| match percent {
        None => Ok(default),
        Some(percent) if (0.0..=100.0).contains(&percent) => Ok(percent / 100.0),
        Some(percent) => Err(format!("{name} is a percentage between 0 and 100, not {percent}")),
    };
    let confidence = confidence.unwrap_or(defaults.confidence);
    if !(0.5..1.0).contains(&confidence) {
        return Err(format!("confidence is a fraction from 0.5 up to 1, such as 0.95, not {confidence}"));
    }
    Ok(Options {
        per: Per::parse(per),
        confidence,
        threshold: fraction("threshold", threshold, defaults.threshold)?,
        min_share: fraction("min-share", min_share, defaults.min_share)?,
        ..defaults
    })
}

/// Compares files with files.
pub fn compare_files(base: &[String], new: &[String], options: &Options) -> Result<Diff, String> {
    let read_side = |paths: &[String]| paths.iter().map(|path| read(path)).collect::<Result<Vec<_>, _>>();
    perfr_diff::diff(&read_side(base)?, &read_side(new)?, options)
}

/// `perfr diff`. 0 when the comparison was made (and, with
/// `--fail-on-regression`, found nothing slower), 1 when that flag found
/// a regression, 2 when the comparison could not be made.
pub fn run(opts: DiffOpts) -> i32 {
    let found = options(&opts.per, Some(opts.confidence), Some(opts.threshold), Some(opts.min_share))
        .and_then(|options| compare_files(&opts.base, &opts.new, &options));
    let diff = match found {
        Ok(diff) => diff,
        Err(error) => {
            eprintln!("perfr diff: {error}");
            return 2;
        }
    };
    match opts.format {
        Format::Json => println!("{}", serde_json::to_string_pretty(&diff).unwrap_or_default()),
        format => print!("{}", render(&diff, &TextOptions { top: opts.top, markdown: format == Format::Markdown })),
    }
    i32::from(opts.fail_on_regression && diff.regressed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_capture::{EventRow, FrameRow, SampleRow};
    use perfr_live_event::LiveEvent;

    fn event(kind: u8, extra: u8, name: &str, start_ns: u64, duration_ns: u64) -> EventRow {
        EventRow {
            event: LiveEvent { start_ns, duration_ns, tid: 7, pid: 3, kind, extra, ..LiveEvent::default() },
            name: name.to_string(),
        }
    }

    fn scope_of(pid: u32, name: &str) -> EventRow {
        let mut row = event(kind::API_SCOPE, 0, name, 100, 10);
        row.event.pid = pid;
        row
    }

    #[test]
    fn a_capture_of_a_process_is_read_for_what_it_is_of() {
        let thread = |pid: u32, tid: u32| perfr_capture::ThreadName { pid, tid, name: String::new() };
        let bundle = |target_pid: u32| CaptureBundle {
            target_pid,
            slice_ns: None,
            processes: Vec::new(),
            threads: vec![thread(3, 7), thread(40, 41), thread(900, 901)],
            events: vec![
                scope_of(3, "the target's"),
                scope_of(40, "a child's"),
                // The service's own, and any bystander's.
                scope_of(900, "capture pass"),
                scope_of(perfr_live_event::AGENT_PID, "what an agent marked"),
            ],
            // The child was sampled; the service never samples itself.
            samples: vec![SampleRow { timestamp_ns: 100, tid: 41, frames: vec![1] }, SampleRow { timestamp_ns: 110, tid: 5000, frames: vec![1] }],
            frames: vec![FrameRow { id: 1, name: "work".into(), module: "app".into(), address: 0 }],
            track_order: String::new(),
            cores: Vec::new(),
            topology: Vec::new(),
        };
        let of_target = input_of("a", bundle(3));
        assert_eq!(of_target.names, ["the target's", "a child's", "what an agent marked"]);
        assert_eq!(of_target.samples.len(), 2, "every sample is of the capture");
        // Nothing was the target: everyone who instruments themselves is.
        assert_eq!(input_of("b", bundle(0)).names.len(), 4);
    }

    #[test]
    fn a_bundle_is_read_for_its_spans_its_samples_and_its_sampling_period() {
        let bundle = CaptureBundle {
            target_pid: 3,
            slice_ns: None,
            processes: Vec::new(),
            threads: Vec::new(),
            events: vec![
                event(kind::API_SCOPE, 0, "tick", 100, 50),
                event(kind::FUNCTION_CALL, 0, "solve", 110, 20),
                // A frame of a sampled stack is not a call anyone timed.
                event(kind::FUNCTION_CALL, extra::SAMPLED_FRAME, "solve", 120, 1_000_000),
                event(kind::FRAME, 0, "frame", 100, 16_000_000),
                event(kind::API_TRACK, extra::GPU_WORK, "conv2d", 130, 40),
                event(kind::API_TRACK, 0, "request", 130, 40),
                event(kind::API_SCOPE, 0, "tick", 200, 60),
                event(kind::SAMPLE, 0, "", 120, 1_000_000),
                event(kind::SAMPLE, 0, "", 220, 1_000_000),
                event(kind::SAMPLE, 0, "", 320, 250_000),
                event(kind::SCHEDULING_SLICE, 0, "", 100, 900),
                event(kind::VALUE, 0, "loss", 100, 5),
                event(kind::LOG, 0, "started", 100, 0),
            ],
            samples: vec![SampleRow { timestamp_ns: 120, tid: 7, frames: vec![2, 1] }],
            frames: vec![
                FrameRow { id: 1, name: "main".into(), module: "/bin/app".into(), address: 0x10 },
                FrameRow { id: 2, name: "solve".into(), module: "/bin/app".into(), address: 0x20 },
            ],
            track_order: String::new(),
            cores: Vec::new(),
            topology: Vec::new(),
        };
        let input = input_of("run.perfr.zip", bundle);
        assert_eq!(input.label, "run.perfr.zip");
        assert_eq!(input.names, ["tick", "solve", "frame", "conv2d", "request"]);
        let spans: Vec<(SpanKind, u32, u64)> = input.spans.iter().map(|span| (span.kind, span.name, span.duration_ns)).collect();
        assert_eq!(
            spans,
            [
                (SpanKind::Scope, 0, 50),
                (SpanKind::Function, 1, 20),
                (SpanKind::Frame, 2, 16_000_000),
                (SpanKind::Gpu, 3, 40),
                (SpanKind::Track, 4, 40),
                (SpanKind::Scope, 0, 60),
            ]
        );
        assert_eq!(input.sample_period_ns, Some(1_000_000));
        assert_eq!(input.samples, [Sample { timestamp_ns: 120, frames: vec![2, 1] }]);
        assert_eq!(input.frames[&2], Frame { name: "solve".into(), module: "/bin/app".into() });
    }

    #[test]
    fn options_are_given_in_percent_and_refused_when_they_are_not_one() {
        let found = options("frame", Some(0.99), Some(2.5), Some(1.0)).unwrap();
        assert_eq!(found.per, Per::Scope("frame".to_string()));
        assert_eq!((found.confidence, found.threshold, found.min_share), (0.99, 0.025, 0.01));
        let defaults = options("", None, None, None).unwrap();
        assert_eq!(defaults, Options::default());
        assert_eq!(options("sample", None, None, None).unwrap().per, Per::Sample);
        for bad in [options("", Some(95.0), None, None), options("", None, Some(-1.0), None), options("", None, None, Some(250.0))] {
            assert!(bad.is_err());
        }
    }

    #[test]
    fn a_file_that_is_not_a_capture_says_which_file() {
        let dir = std::env::temp_dir().join(format!("perfr-diff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.perfr.zip");
        std::fs::write(&path, b"not a zip").unwrap();
        let error = read(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("notes.perfr.zip") && error.contains("not a capture"), "{error}");
        let missing = read(dir.join("missing.perfr.zip").to_str().unwrap()).unwrap_err();
        assert!(missing.contains("missing.perfr.zip"), "{missing}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
