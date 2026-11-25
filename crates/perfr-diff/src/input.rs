// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a comparison is given: a capture reduced to its spans and samples.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// What a span is, which is part of what it is called: a scope and a
/// hooked function of the same name are two rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanKind {
    /// One frame of a program that marks its frames.
    Frame,
    /// A scope the program opened through the API.
    Scope,
    /// A call of a hooked function.
    Function,
    /// A span on an async track.
    Track,
    /// Work the GPU ran.
    Gpu,
}

impl SpanKind {
    pub fn label(self) -> &'static str {
        match self {
            SpanKind::Frame => "frame",
            SpanKind::Scope => "scope",
            SpanKind::Function => "function",
            SpanKind::Track => "track",
            SpanKind::Gpu => "gpu",
        }
    }
}

/// Something with a start and a duration: a scope, a hooked call, a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub kind: SpanKind,
    /// Index into [`CaptureInput::names`].
    pub name: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
}

/// One sampled callstack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub timestamp_ns: u64,
    /// Frame ids, innermost first.
    pub frames: Vec<u32>,
}

/// What a frame id of a [`Sample`] stands for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    pub module: String,
}

/// One capture, as much of it as a comparison reads.
#[derive(Clone, Debug, Default)]
pub struct CaptureInput {
    /// What the capture is called in the report: its file name, usually.
    pub label: String,
    pub names: Vec<String>,
    pub spans: Vec<Span>,
    pub samples: Vec<Sample>,
    pub frames: HashMap<u32, Frame>,
    /// The time between two samples of a thread that is running; nothing
    /// when the capture does not say, and then sampled time is counted in
    /// samples.
    pub sample_period_ns: Option<u64>,
}

impl CaptureInput {
    /// The index of `name` in `names`, added if it is new. For building an
    /// input by hand; a converter that has a table of its own fills
    /// `names` directly.
    pub fn name(&mut self, name: &str) -> u32 {
        if let Some(index) = self.names.iter().position(|known| known == name) {
            return index as u32;
        }
        self.names.push(name.to_string());
        (self.names.len() - 1) as u32
    }
}
