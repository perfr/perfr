// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The story shipped with the viewer for `captures/box3d.perfr.stream`.
//!
//! The windows are fractions of whatever span that capture has, so the
//! file does not have to be rewritten when the recording's clock changes.
//! The words describe what the lanes are. They do not state a frame time,
//! a core count, or a source line.

use crate::{Anchor, Fit, Highlight, Overlay, Step, StepKind, Story, TrackRef, View, FORMAT};

/// Story for the box3d demo capture.
pub fn demo_box3d() -> Story {
    Story {
        format: FORMAT.to_string(),
        version: crate::VERSION,
        title: "box3d capture".into(),
        summary: "A short tour of captures/box3d.perfr.stream. The steps point at the scheduler and the sample bar. They do not state a frame time or a core count; those are whatever this capture shows.".into(),
        capture: "captures/box3d.perfr.stream".into(),
        transition_ms: 800,
        steps: vec![
            step(
                "context",
                StepKind::Context,
                "The whole capture",
                "This is the box3d demo capture, fitted to its own span. The rail below is the machine, the process, and its threads.",
                4500,
                View::default(),
                vec![],
            ),
            step(
                "problem",
                StepKind::Problem,
                "A stretch of the timeline",
                "The highlighted band is the middle of the capture. Read the sample bar and the scopes inside it for where the time went. This text does not name a function.",
                5000,
                View {
                    t0_frac: Some(0.35),
                    t1_frac: Some(0.65),
                    fit: Some(Fit::Span),
                    focused: vec![TrackRef::Name { name: "box3d".into() }],
                    ..View::default()
                },
                vec![Overlay::Highlight {
                    id: "mid".into(),
                    target: Highlight::Time { t0_ns: None, t1_ns: None, t0_frac: Some(0.42), t1_frac: Some(0.58) },
                }],
            ),
            step(
                "cores",
                StepKind::Evidence,
                "The scheduler lanes",
                "Each scheduler lane is one core. A lane with no slice in this window was idle here. Count the busy lanes on the capture; this story does not state how many there are.",
                5500,
                View {
                    t0_frac: Some(0.35),
                    t1_frac: Some(0.65),
                    fit: Some(Fit::Span),
                    expanded: vec![TrackRef::Scheduler],
                    focused: vec![TrackRef::Scheduler],
                    ..View::default()
                },
                vec![
                    Overlay::Rect {
                        id: "sched".into(),
                        t0_ns: None,
                        t1_ns: None,
                        t0_frac: Some(0.35),
                        t1_frac: Some(0.65),
                        tracks: vec![TrackRef::Scheduler],
                        label: "scheduler".into(),
                    },
                    Overlay::Callout {
                        id: "cores".into(),
                        anchor: Anchor {
                            time_ns: None,
                            time_frac: Some(0.5),
                            track: TrackRef::Scheduler,
                            y: 0.15,
                        },
                        text: "One lane per core. Idle lanes stay empty.".into(),
                    },
                ],
            ),
            step(
                "samples",
                StepKind::Evidence,
                "Where the samples landed",
                "The arrow points from the scheduler at the threads. The sampling report for this window is the list of functions the samples landed in. Open it on the capture before naming a hotspot.",
                5500,
                View {
                    t0_frac: Some(0.40),
                    t1_frac: Some(0.60),
                    fit: Some(Fit::Span),
                    collapsed: vec![TrackRef::Scheduler],
                    focused: vec![TrackRef::Name { name: "box3d".into() }],
                    ..View::default()
                },
                vec![Overlay::Arrow {
                    id: "to-threads".into(),
                    from: Anchor {
                        time_ns: None,
                        time_frac: Some(0.48),
                        track: TrackRef::Scheduler,
                        y: 0.8,
                    },
                    to: Anchor {
                        time_ns: None,
                        time_frac: Some(0.52),
                        track: TrackRef::Name { name: "box3d".into() },
                        y: 0.25,
                    },
                }],
            ),
            step(
                "code",
                StepKind::ProposedFix,
                "A code reference is a pointer",
                "A code overlay is a file and a line the author wants the reader to open. It is not a measurement. This one is only an example of the overlay, and it is not a claim about box3d.",
                5000,
                View {
                    fit: Some(Fit::Content),
                    ..View::default()
                },
                vec![Overlay::Code {
                    id: "example-code".into(),
                    anchor: Some(Anchor {
                        time_ns: None,
                        time_frac: Some(0.5),
                        track: TrackRef::Name { name: "box3d".into() },
                        y: 0.2,
                    }),
                    file: "example/not-from-this-capture.rs".into(),
                    line: 1,
                    snippet: "Replace this with a file and a line the capture's symbols support.".into(),
                }],
            ),
        ],
    }
}

fn step(
    id: &str,
    kind: StepKind,
    title: &str,
    body: &str,
    duration_ms: u32,
    view: View,
    overlays: Vec<Overlay>,
) -> Step {
    Step {
        id: id.into(),
        kind,
        title: title.into(),
        body: body.into(),
        duration_ms,
        view,
        overlays,
    }
}

/// Pretty JSON of [`demo_box3d`], the bytes served as `stories/box3d.perfr.story`.
pub fn demo_box3d_json() -> String {
    demo_box3d()
        .to_json_pretty()
        .expect("demo story serializes")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_story, validate};

    #[test]
    fn demo_story_validates_and_roundtrips() {
        let story = demo_box3d();
        assert!(validate(&story).is_empty(), "{:?}", validate(&story));
        let text = demo_box3d_json();
        let again = parse_story(&text).unwrap();
        assert_eq!(again, story);
        assert_eq!(again.capture, "captures/box3d.perfr.stream");
        assert!(again.steps.len() >= 5);
        assert!(again.steps.iter().any(|s| s.kind == StepKind::Evidence));
        assert!(again.summary.contains("do not state"));
    }

    /// Both packs serve the demo story as a file: the client's, committed
    /// with the rest of its pack, and the viewer's, copied into its pack
    /// from `web/` when it is built.
    #[test]
    fn served_files_match_the_demo() {
        for served in [
            "/../perfr-client/viewer-dist/stories/box3d.perfr.story",
            "/../perfr-viewer/web/stories/box3d.perfr.story",
        ] {
            let path = format!("{}{served}", env!("CARGO_MANIFEST_DIR"));
            let disk = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            assert_eq!(disk, demo_box3d_json(), "{path}");
        }
    }
}
