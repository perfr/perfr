// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The versioned story document and the checks a player can trust.

use serde::{Deserialize, Serialize};

/// `format` field of every story this crate reads and writes.
pub const FORMAT: &str = "perfr.story";

/// The only version this crate accepts. A higher version is rejected
/// rather than guessed at.
pub const VERSION: u32 = 1;

const TITLE_MAX: usize = 200;
const BODY_MAX: usize = 8_000;
const TEXT_MAX: usize = 2_000;
const SNIPPET_MAX: usize = 4_000;
const ID_MAX: usize = 80;
const FILE_MAX: usize = 500;
const DURATION_MAX_MS: u32 = 120_000;

/// One story. Steps play in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Story {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub title: String,
    /// One or two sentences on what the story is willing to claim.
    #[serde(default)]
    pub summary: String,
    /// Capture URL the story was written against. The viewer opens it
    /// only when the page URL did not already pass `capture=`.
    #[serde(default)]
    pub capture: String,
    /// Camera move between steps, in milliseconds.
    #[serde(default = "default_transition_ms")]
    pub transition_ms: u32,
    #[serde(default)]
    pub steps: Vec<Step>,
}

fn default_transition_ms() -> u32 {
    crate::TRANSITION_MS as u32
}

impl Default for Story {
    fn default() -> Self {
        Self {
            format: FORMAT.to_string(),
            version: VERSION,
            title: String::new(),
            summary: String::new(),
            capture: String::new(),
            transition_ms: default_transition_ms(),
            steps: Vec::new(),
        }
    }
}

impl Story {
    pub fn to_json_pretty(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn step_index(&self, id: &str) -> Option<usize> {
        self.steps.iter().position(|s| s.id == id)
    }

    pub fn add_step(&mut self, step: Step, at: Option<usize>) -> Result<(), String> {
        if step.id.is_empty() {
            return Err("a step needs an id".into());
        }
        if self.steps.iter().any(|s| s.id == step.id) {
            return Err(format!("step id {} is already used", step.id));
        }
        let at = at.unwrap_or(self.steps.len()).min(self.steps.len());
        self.steps.insert(at, step);
        Ok(())
    }

    pub fn remove_step(&mut self, id: &str) -> Result<Step, String> {
        let index = self.step_index(id).ok_or_else(|| format!("no step {id}"))?;
        Ok(self.steps.remove(index))
    }

    pub fn reorder(&mut self, id: &str, to: usize) -> Result<(), String> {
        let from = self.step_index(id).ok_or_else(|| format!("no step {id}"))?;
        let step = self.steps.remove(from);
        let to = to.min(self.steps.len());
        self.steps.insert(to, step);
        Ok(())
    }

    /// Replace fields present in `patch`. Absent fields stay.
    /// `view` and `overlays`, when present, replace the whole value.
    /// A patch that does not parse changes nothing.
    pub fn update_step(&mut self, id: &str, patch: &serde_json::Value) -> Result<(), String> {
        let kind: Option<StepKind> = patch
            .get("kind")
            .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("kind: {e}")))
            .transpose()?;
        let duration_ms = patch
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .map(|ms| u32::try_from(ms).map_err(|_| "duration_ms does not fit".to_string()))
            .transpose()?;
        let view: Option<View> = patch
            .get("view")
            .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("view: {e}")))
            .transpose()?;
        let overlays: Option<Vec<Overlay>> = patch
            .get("overlays")
            .map(|v| serde_json::from_value(v.clone()).map_err(|e| format!("overlays: {e}")))
            .transpose()?;
        let step = self
            .steps
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("no step {id}"))?;
        if let Some(title) = patch.get("title").and_then(|v| v.as_str()) {
            step.title = title.to_string();
        }
        if let Some(body) = patch.get("body").and_then(|v| v.as_str()) {
            step.body = body.to_string();
        }
        if let Some(kind) = kind {
            step.kind = kind;
        }
        if let Some(ms) = duration_ms {
            step.duration_ms = ms;
        }
        if let Some(view) = view {
            step.view = view;
        }
        if let Some(overlays) = overlays {
            step.overlays = overlays;
        }
        Ok(())
    }

    pub fn set_view(&mut self, id: &str, view: View) -> Result<(), String> {
        let step = self
            .steps
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("no step {id}"))?;
        step.view = view;
        Ok(())
    }

    pub fn add_overlay(&mut self, step_id: &str, overlay: Overlay) -> Result<(), String> {
        let id = overlay.id().to_string();
        if id.is_empty() {
            return Err("an overlay needs an id".into());
        }
        let step = self
            .steps
            .iter_mut()
            .find(|s| s.id == step_id)
            .ok_or_else(|| format!("no step {step_id}"))?;
        if step.overlays.iter().any(|o| o.id() == id) {
            return Err(format!("overlay id {id} is already used on step {step_id}"));
        }
        step.overlays.push(overlay);
        Ok(())
    }

    pub fn remove_overlay(&mut self, step_id: &str, overlay_id: &str) -> Result<(), String> {
        let step = self
            .steps
            .iter_mut()
            .find(|s| s.id == step_id)
            .ok_or_else(|| format!("no step {step_id}"))?;
        let index = step
            .overlays
            .iter()
            .position(|o| o.id() == overlay_id)
            .ok_or_else(|| format!("no overlay {overlay_id}"))?;
        step.overlays.remove(index);
        Ok(())
    }
}

/// What a step is doing in the narrative. The player does not treat these
/// differently; they are how an author and a reader name the arc.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Context,
    Problem,
    Evidence,
    RootCause,
    ProposedFix,
    AppliedFix,
    BeforeAfter,
}

impl StepKind {
    pub fn label(self) -> &'static str {
        match self {
            StepKind::Context => "context",
            StepKind::Problem => "problem",
            StepKind::Evidence => "evidence",
            StepKind::RootCause => "root cause",
            StepKind::ProposedFix => "proposed fix",
            StepKind::AppliedFix => "applied fix",
            StepKind::BeforeAfter => "before / after",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: StepKind,
    #[serde(default)]
    pub title: String,
    /// Narration. The viewer renders a small subset of markdown:
    /// paragraphs, `**bold**`, and `` `code` ``.
    #[serde(default)]
    pub body: String,
    /// How long Play holds this step before the next one, in milliseconds.
    #[serde(default = "default_duration_ms")]
    pub duration_ms: u32,
    #[serde(default)]
    pub view: View,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
}

fn default_duration_ms() -> u32 {
    4_000
}

/// Where the camera sits for one step.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t0_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t1_ns: Option<u64>,
    /// Fraction of the capture's content span, used when the matching
    /// nanosecond field is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t0_frac: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t1_frac: Option<f64>,
    /// `content` fits the capture. `span` uses the bounds above.
    /// Absent means `span` when any bound is set, and `content` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<Fit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<Scroll>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanded: Vec<TrackRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed: Vec<TrackRef>,
    /// Tracks to bring near the top of the rail, first one first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub focused: Vec<TrackRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<Selection>,
}

impl View {
    pub fn effective_fit(&self) -> Fit {
        if let Some(fit) = self.fit {
            return fit;
        }
        if self.t0_ns.is_some()
            || self.t1_ns.is_some()
            || self.t0_frac.is_some()
            || self.t1_frac.is_some()
        {
            Fit::Span
        } else {
            Fit::Content
        }
    }

    /// Visible window in capture nanoseconds.
    ///
    /// `content` is the capture's own span `(start, end)`. A missing
    /// content span (the capture has not loaded) leaves a one-second
    /// placeholder at 0 so a story can still open.
    pub fn resolve_window(&self, content: Option<(u64, u64)>) -> Result<(u64, u64), String> {
        let (c0, c1) = match content {
            Some((a, b)) if b > a => (a, b),
            _ => (0, 1_000_000_000),
        };
        if self.effective_fit() == Fit::Content {
            return Ok((c0, c1));
        }
        let t0 = bound(self.t0_ns, self.t0_frac, c0, c1);
        let t1 = bound(self.t1_ns, self.t1_frac, c0, c1);
        match (t0, t1) {
            (Some(a), Some(b)) if b > a => Ok((a, b)),
            _ => Err("a span view needs a start before its end".into()),
        }
    }
}

fn bound(ns: Option<u64>, frac: Option<f64>, c0: u64, c1: u64) -> Option<u64> {
    if let Some(ns) = ns {
        return Some(ns);
    }
    let frac = frac?;
    if !frac.is_finite() {
        return None;
    }
    let span = c1.saturating_sub(c0) as f64;
    Some(c0.saturating_add((span * frac.clamp(0.0, 1.0)) as u64))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Content,
    Span,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scroll {
    Y { y: f32 },
    Track { track: TrackRef },
}

/// A row in the viewer, named by capture ids or by a name substring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrackRef {
    Scheduler,
    Process { pid: u32 },
    Thread { pid: u32, tid: u32 },
    Async { pid: u32, tid: u32 },
    Name { name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    Scope { scope_id: u64 },
    Thread { pid: u32, tid: u32 },
    Time { t0_ns: u64, t1_ns: u64 },
}

/// A point on the capture, not on the screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_frac: Option<f64>,
    pub track: TrackRef,
    /// 0 at the top of the track's band, 1 at the bottom. Default 0.35.
    #[serde(default = "default_y")]
    pub y: f32,
}

fn default_y() -> f32 {
    0.35
}

impl Anchor {
    pub fn resolve_time(&self, content: Option<(u64, u64)>) -> u64 {
        let (c0, c1) = content.unwrap_or((0, 1_000_000_000));
        bound(self.time_ns, self.time_frac, c0, c1).unwrap_or(c0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Highlight {
    Time {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t0_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t1_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t0_frac: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t1_frac: Option<f64>,
    },
    Track {
        track: TrackRef,
    },
    Scope {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Overlay {
    Arrow {
        id: String,
        from: Anchor,
        to: Anchor,
    },
    Rect {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t0_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t1_ns: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t0_frac: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        t1_frac: Option<f64>,
        tracks: Vec<TrackRef>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
    Highlight {
        id: String,
        target: Highlight,
    },
    Callout {
        id: String,
        anchor: Anchor,
        text: String,
    },
    Code {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        anchor: Option<Anchor>,
        file: String,
        line: u32,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        snippet: String,
    },
}

impl Overlay {
    pub fn id(&self) -> &str {
        match self {
            Overlay::Arrow { id, .. }
            | Overlay::Rect { id, .. }
            | Overlay::Highlight { id, .. }
            | Overlay::Callout { id, .. }
            | Overlay::Code { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoryError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for StoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Parse JSON and reject a document the player should not run.
pub fn parse_story(text: &str) -> Result<Story, String> {
    let story: Story =
        serde_json::from_str(text).map_err(|e| format!("could not read this story: {e}"))?;
    let errors = validate(&story);
    if errors.is_empty() {
        Ok(story)
    } else {
        Err(errors
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// Structural checks. An empty step list is a draft and is allowed.
pub fn validate(story: &Story) -> Vec<StoryError> {
    let mut errors = Vec::new();
    let err = |path: &str, message: &str| StoryError {
        path: path.to_string(),
        message: message.to_string(),
    };
    if story.format != FORMAT {
        errors.push(err("format", &format!("expected {FORMAT}")));
    }
    if story.version != VERSION {
        errors.push(err(
            "version",
            &format!("this reader accepts version {VERSION}"),
        ));
    }
    if story.title.chars().count() > TITLE_MAX {
        errors.push(err("title", "longer than 200 characters"));
    }
    if story.summary.chars().count() > BODY_MAX {
        errors.push(err("summary", "longer than 8000 characters"));
    }
    if !(100..=5_000).contains(&story.transition_ms) {
        errors.push(err("transition_ms", "expected 100..=5000"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (i, step) in story.steps.iter().enumerate() {
        let path = format!("steps[{i}]");
        if step.id.is_empty() || step.id.chars().count() > ID_MAX {
            errors.push(err(&format!("{path}.id"), "need 1..=80 characters"));
        } else if !seen.insert(step.id.clone()) {
            errors.push(err(&format!("{path}.id"), "duplicate step id"));
        }
        if step.title.chars().count() > TITLE_MAX {
            errors.push(err(&format!("{path}.title"), "longer than 200 characters"));
        }
        if step.body.chars().count() > BODY_MAX {
            errors.push(err(&format!("{path}.body"), "longer than 8000 characters"));
        }
        if step.duration_ms == 0 || step.duration_ms > DURATION_MAX_MS {
            errors.push(err(&format!("{path}.duration_ms"), "expected 1..=120000"));
        }
        errors.extend(validate_view(&format!("{path}.view"), &step.view));
        let mut overlays = std::collections::BTreeSet::new();
        for (j, overlay) in step.overlays.iter().enumerate() {
            let op = format!("{path}.overlays[{j}]");
            if overlay.id().is_empty() || overlay.id().chars().count() > ID_MAX {
                errors.push(err(&format!("{op}.id"), "need 1..=80 characters"));
            } else if !overlays.insert(overlay.id().to_string()) {
                errors.push(err(
                    &format!("{op}.id"),
                    "duplicate overlay id on this step",
                ));
            }
            errors.extend(validate_overlay(&op, overlay));
        }
    }
    errors
}

fn validate_view(path: &str, view: &View) -> Vec<StoryError> {
    let mut errors = Vec::new();
    for (name, frac) in [("t0_frac", view.t0_frac), ("t1_frac", view.t1_frac)] {
        if let Some(frac) = frac {
            if !frac.is_finite() || !(0.0..=1.0).contains(&frac) {
                errors.push(StoryError {
                    path: format!("{path}.{name}"),
                    message: "expected a fraction from 0 to 1".into(),
                });
            }
        }
    }
    if view.effective_fit() == Fit::Span {
        if let (Some(a), Some(b)) = (view.t0_ns, view.t1_ns) {
            if b <= a {
                errors.push(StoryError {
                    path: format!("{path}.t1_ns"),
                    message: "end must be after start".into(),
                });
            }
        } else if let Some(missing) = missing_end(view.t0_ns, view.t1_ns, view.t0_frac, view.t1_frac) {
            errors.push(StoryError {
                path: path.to_string(),
                message: format!("a span view needs {missing}"),
            });
        }
        if let (Some(a), Some(b)) = (view.t0_frac, view.t1_frac) {
            if view.t0_ns.is_none() && view.t1_ns.is_none() && b <= a {
                errors.push(StoryError {
                    path: format!("{path}.t1_frac"),
                    message: "end fraction must be after the start".into(),
                });
            }
        }
    }
    if let Some(Selection::Time { t0_ns, t1_ns }) = &view.selected {
        if *t1_ns <= *t0_ns {
            errors.push(StoryError {
                path: format!("{path}.selected"),
                message: "selected time ends at or before it starts".into(),
            });
        }
    }
    errors
}

/// What [`missing_end`] says of a range with neither end.
const BOTH_ENDS: &str = "bounds";

/// Which end of a time range is not given, as the tail of a message. Each
/// end is nanoseconds or a fraction of the capture, and the player needs
/// both to place the range: [`View::resolve_window`] fails without one,
/// and a view then falls back to the capture's first second while a rect
/// or a highlight is not drawn, neither with a word to the author.
fn missing_end(
    t0_ns: Option<u64>,
    t1_ns: Option<u64>,
    t0_frac: Option<f64>,
    t1_frac: Option<f64>,
) -> Option<&'static str> {
    match (t0_ns.is_some() || t0_frac.is_some(), t1_ns.is_some() || t1_frac.is_some()) {
        (true, true) => None,
        (false, false) => Some(BOTH_ENDS),
        (false, true) => Some("a start: t0_ns or t0_frac"),
        (true, false) => Some("an end: t1_ns or t1_frac"),
    }
}

fn validate_overlay(path: &str, overlay: &Overlay) -> Vec<StoryError> {
    let mut errors = Vec::new();
    match overlay {
        Overlay::Arrow { from, to, .. } => {
            errors.extend(validate_anchor(&format!("{path}.from"), from));
            errors.extend(validate_anchor(&format!("{path}.to"), to));
        }
        Overlay::Rect {
            t0_ns,
            t1_ns,
            t0_frac,
            t1_frac,
            tracks,
            label,
            ..
        } => {
            if tracks.is_empty() {
                errors.push(StoryError {
                    path: format!("{path}.tracks"),
                    message: "a rect names at least one track".into(),
                });
            }
            if let (Some(a), Some(b)) = (t0_ns, t1_ns) {
                if *b <= *a {
                    errors.push(StoryError {
                        path: format!("{path}.t1_ns"),
                        message: "end must be after start".into(),
                    });
                }
            }
            match missing_end(*t0_ns, *t1_ns, *t0_frac, *t1_frac) {
                Some(BOTH_ENDS) => errors.push(StoryError {
                    path: path.into(),
                    message: "a rect needs a time range".into(),
                }),
                Some(missing) => errors.push(StoryError {
                    path: path.into(),
                    message: format!("a rect needs {missing}"),
                }),
                None => {}
            }
            if label.chars().count() > TITLE_MAX {
                errors.push(StoryError {
                    path: format!("{path}.label"),
                    message: "longer than 200 characters".into(),
                });
            }
        }
        Overlay::Highlight { target, .. } => match target {
            Highlight::Time {
                t0_ns,
                t1_ns,
                t0_frac,
                t1_frac,
            } => {
                match missing_end(*t0_ns, *t1_ns, *t0_frac, *t1_frac) {
                    Some(BOTH_ENDS) => errors.push(StoryError {
                        path: format!("{path}.target"),
                        message: "a time highlight needs a range".into(),
                    }),
                    Some(missing) => errors.push(StoryError {
                        path: format!("{path}.target"),
                        message: format!("a time highlight needs {missing}"),
                    }),
                    None => {}
                }
                if let (Some(a), Some(b)) = (t0_ns, t1_ns) {
                    if *b <= *a {
                        errors.push(StoryError {
                            path: format!("{path}.target"),
                            message: "end must be after start".into(),
                        });
                    }
                }
            }
            Highlight::Track { track } => {
                errors.extend(validate_track(&format!("{path}.target.track"), track))
            }
            Highlight::Scope { scope_id, name } => {
                let named = name.as_ref().is_some_and(|n| !n.is_empty());
                if scope_id.is_none() && !named {
                    errors.push(StoryError {
                        path: format!("{path}.target"),
                        message: "a scope highlight needs a scope_id or a name".into(),
                    });
                }
            }
        },
        Overlay::Callout { anchor, text, .. } => {
            errors.extend(validate_anchor(&format!("{path}.anchor"), anchor));
            if text.trim().is_empty() {
                errors.push(StoryError {
                    path: format!("{path}.text"),
                    message: "a callout needs text".into(),
                });
            }
            if text.chars().count() > TEXT_MAX {
                errors.push(StoryError {
                    path: format!("{path}.text"),
                    message: "longer than 2000 characters".into(),
                });
            }
        }
        Overlay::Code {
            anchor,
            file,
            line,
            snippet,
            ..
        } => {
            if let Some(anchor) = anchor {
                errors.extend(validate_anchor(&format!("{path}.anchor"), anchor));
            }
            if file.trim().is_empty() || file.chars().count() > FILE_MAX {
                errors.push(StoryError {
                    path: format!("{path}.file"),
                    message: "need a file path, at most 500 characters".into(),
                });
            }
            if *line == 0 {
                errors.push(StoryError {
                    path: format!("{path}.line"),
                    message: "line is 1-based".into(),
                });
            }
            if snippet.chars().count() > SNIPPET_MAX {
                errors.push(StoryError {
                    path: format!("{path}.snippet"),
                    message: "longer than 4000 characters".into(),
                });
            }
        }
    }
    errors
}

fn validate_anchor(path: &str, anchor: &Anchor) -> Vec<StoryError> {
    let mut errors = validate_track(&format!("{path}.track"), &anchor.track);
    if let Some(frac) = anchor.time_frac {
        if !frac.is_finite() || !(0.0..=1.0).contains(&frac) {
            errors.push(StoryError {
                path: format!("{path}.time_frac"),
                message: "expected a fraction from 0 to 1".into(),
            });
        }
    }
    if !(0.0..=1.0).contains(&anchor.y) || !anchor.y.is_finite() {
        errors.push(StoryError {
            path: format!("{path}.y"),
            message: "expected a fraction from 0 to 1".into(),
        });
    }
    if anchor.time_ns.is_none() && anchor.time_frac.is_none() {
        errors.push(StoryError {
            path: path.into(),
            message: "an anchor needs time_ns or time_frac".into(),
        });
    }
    errors
}

fn validate_track(path: &str, track: &TrackRef) -> Vec<StoryError> {
    match track {
        TrackRef::Name { name } if name.trim().is_empty() => {
            vec![StoryError {
                path: path.into(),
                message: "a name track needs a name".into(),
            }]
        }
        _ => Vec::new(),
    }
}

/// A dropped or opened file that is a story document.
pub fn is_story_filename(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".perfr.story") || name.ends_with(".story.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str) -> Step {
        Step {
            id: id.into(),
            kind: StepKind::Evidence,
            title: "Busy lanes".into(),
            body: "Count the lanes that have a slice.".into(),
            duration_ms: 3000,
            view: View {
                t0_frac: Some(0.2),
                t1_frac: Some(0.6),
                fit: Some(Fit::Span),
                ..View::default()
            },
            overlays: vec![Overlay::Callout {
                id: "note".into(),
                anchor: Anchor {
                    time_ns: None,
                    time_frac: Some(0.4),
                    track: TrackRef::Scheduler,
                    y: 0.2,
                },
                text: "Each lane is one core.".into(),
            }],
        }
    }

    #[test]
    fn roundtrip_keeps_a_story() {
        let mut story = Story {
            title: "demo".into(),
            summary: "A short tour.".into(),
            capture: "captures/box3d.perfr.stream".into(),
            ..Story::default()
        };
        story.add_step(step("a"), None).unwrap();
        story.add_step(step("b"), Some(0)).unwrap();
        let text = story.to_json_pretty().unwrap();
        let again = parse_story(&text).unwrap();
        assert_eq!(again, story);
        assert_eq!(again.steps[0].id, "b");
        assert_eq!(again.format, FORMAT);
        assert_eq!(again.version, VERSION);
    }

    #[test]
    fn validation_rejects_a_bad_document() {
        let mut story = Story { version: 9, ..Default::default() };
        story.steps.push(step("a"));
        story.steps.push(step("a"));
        story.steps[1].view.t0_ns = Some(50);
        story.steps[1].view.t1_ns = Some(10);
        story.steps[1].view.t0_frac = None;
        story.steps[1].view.t1_frac = None;
        story.steps[1].duration_ms = 0;
        let errors = validate(&story);
        let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"version"), "{errors:?}");
        assert!(paths.iter().any(|p| p.contains("id")), "{errors:?}");
        assert!(paths.iter().any(|p| p.contains("duration")), "{errors:?}");
        assert!(paths.iter().any(|p| p.contains("t1_ns")), "{errors:?}");
        assert!(parse_story(&story.to_json_pretty().unwrap()).is_err());
    }

    /// The player resolves a range from both of its ends and shows the
    /// first second of the capture, or nothing, when it cannot. A range
    /// with one end has to be caught here, where the author is told.
    #[test]
    fn a_range_with_only_one_end_is_rejected_wherever_a_range_is_given() {
        let text = r#"{"format":"perfr.story","version":1,"steps":[{"id":"a","kind":"evidence","view":{"t0_ns":5000000}}]}"#;
        let err = parse_story(text).unwrap_err();
        assert_eq!(err, "steps[0].view: a span view needs an end: t1_ns or t1_frac");

        let mut story = Story::default();
        story.steps.push(step("a"));
        story.steps[0].view = View { t1_frac: Some(0.5), ..View::default() };
        story.steps[0].overlays = vec![
            Overlay::Rect {
                id: "r".into(),
                t0_ns: Some(10),
                t1_ns: None,
                t0_frac: None,
                t1_frac: None,
                tracks: vec![TrackRef::Scheduler],
                label: String::new(),
            },
            Overlay::Highlight {
                id: "h".into(),
                target: Highlight::Time { t0_ns: None, t1_ns: None, t0_frac: None, t1_frac: Some(0.9) },
            },
        ];
        let errors: Vec<String> = validate(&story).iter().map(|e| e.to_string()).collect();
        assert_eq!(
            errors,
            vec![
                "steps[0].view: a span view needs a start: t0_ns or t0_frac",
                "steps[0].overlays[0]: a rect needs an end: t1_ns or t1_frac",
                "steps[0].overlays[1].target: a time highlight needs a start: t0_ns or t0_frac",
            ]
        );

        // One end in nanoseconds and the other as a fraction is a range.
        story.steps[0].view = View { t0_ns: Some(10), t1_frac: Some(0.5), ..View::default() };
        story.steps[0].overlays.clear();
        assert!(validate(&story).is_empty(), "{:?}", validate(&story));
        // No end at all keeps the message it had.
        story.steps[0].view = View { fit: Some(Fit::Span), ..View::default() };
        assert_eq!(validate(&story)[0].message, "a span view needs bounds");
    }

    #[test]
    fn unknown_kind_does_not_parse() {
        let text = r#"{"format":"perfr.story","version":1,"steps":[{"id":"a","kind":"guess","duration_ms":1000}]}"#;
        let err = parse_story(text).unwrap_err();
        assert!(err.contains("could not read"), "{err}");
    }

    #[test]
    fn resolve_window_uses_content_fractions_and_fit() {
        let content = Some((1_000, 11_000));
        let whole = View::default();
        assert_eq!(whole.effective_fit(), Fit::Content);
        assert_eq!(whole.resolve_window(content).unwrap(), (1_000, 11_000));
        let span = View {
            t0_frac: Some(0.0),
            t1_frac: Some(0.5),
            fit: Some(Fit::Span),
            ..View::default()
        };
        assert_eq!(span.resolve_window(content).unwrap(), (1_000, 6_000));
        let abs = View {
            t0_ns: Some(2_000),
            t1_ns: Some(4_000),
            ..View::default()
        };
        assert_eq!(abs.effective_fit(), Fit::Span);
        assert_eq!(abs.resolve_window(content).unwrap(), (2_000, 4_000));
        assert_eq!(whole.resolve_window(None).unwrap(), (0, 1_000_000_000));
    }

    #[test]
    fn edits_keep_ids_unique() {
        let mut story = Story::default();
        story.add_step(step("a"), None).unwrap();
        assert!(story.add_step(step("a"), None).is_err());
        story
            .add_overlay(
                "a",
                Overlay::Highlight {
                    id: "h".into(),
                    target: Highlight::Track {
                        track: TrackRef::Thread { pid: 1, tid: 2 },
                    },
                },
            )
            .unwrap();
        assert!(story
            .add_overlay(
                "a",
                Overlay::Highlight {
                    id: "h".into(),
                    target: Highlight::Scope {
                        scope_id: Some(3),
                        name: None
                    },
                }
            )
            .is_err());
        story.remove_overlay("a", "h").unwrap();
        story.reorder("a", 0).unwrap();
        story
            .update_step(
                "a",
                &serde_json::json!({"title": "renamed", "kind": "problem"}),
            )
            .unwrap();
        assert_eq!(story.steps[0].title, "renamed");
        assert_eq!(story.steps[0].kind, StepKind::Problem);
        assert!(validate(&story).is_empty(), "{:?}", validate(&story));
        // A patch that fails to parse leaves every field alone, including
        // the ones that would have applied.
        let err = story
            .update_step(
                "a",
                &serde_json::json!({"title": "half", "kind": "not-a-kind"}),
            )
            .unwrap_err();
        assert!(err.starts_with("kind:"), "{err}");
        assert_eq!(story.steps[0].title, "renamed");
    }

    #[test]
    fn story_filenames() {
        assert!(is_story_filename("box3d.perfr.story"));
        assert!(is_story_filename("Notes.STORY.JSON"));
        assert!(!is_story_filename("box3d.perfr.stream"));
    }
}
