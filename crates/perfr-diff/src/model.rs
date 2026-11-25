// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What a comparison says: the document `perfr diff --json` prints, the
//! service answers with and the viewer draws.

use serde::{Deserialize, Serialize};

use crate::input::SpanKind;

/// What one figure did between the base and the new side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// More time, beyond chance and beyond the threshold.
    Regressed,
    /// Less time, beyond chance and beyond the threshold.
    Improved,
    /// Tested, and either within chance or under the threshold.
    Unchanged,
    /// Too little of it, or too few units, to test.
    #[default]
    Untested,
}

/// One figure on both sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    pub base: f64,
    pub new: f64,
    /// `new - base`.
    pub delta: f64,
    /// The interval of `delta` at the report's confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_ci: Option<(f64, f64)>,
    /// `new / base - 1`; absent when the base is 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_ci: Option<(f64, f64)>,
    /// The two-sided p-value of "no difference", on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
    /// The same adjusted for every comparison of the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<f64>,
    pub verdict: Verdict,
}

impl Metric {
    pub fn changed(&self) -> bool {
        matches!(self.verdict, Verdict::Regressed | Verdict::Improved)
    }
}

/// A quantile on both sides. It is described, not judged: the verdicts
/// are those of the means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quantile {
    pub base: f64,
    pub new: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_ci: Option<(f64, f64)>,
}

/// A sampled function.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionRow {
    pub name: String,
    pub module: String,
    /// Samples with the function innermost, base then new.
    pub self_samples: (u64, u64),
    /// Samples with the function anywhere on the stack.
    pub inclusive_samples: (u64, u64),
    /// In the report's function unit ([`Method::function_unit`]).
    #[serde(rename = "self")]
    pub self_time: Metric,
    pub inclusive: Metric,
}

/// A scope, a hooked function or a frame series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopeRow {
    pub name: String,
    pub kind: SpanKind,
    pub calls: (u64, u64),
    /// Nanoseconds per call.
    pub mean: Metric,
    pub median: Quantile,
    pub p95: Quantile,
    /// Milliseconds per unit ([`Method::per`]): what the scope costs in
    /// all, which is where a scope called more often shows.
    pub total: Metric,
}

/// A node of the call tree both sides are folded into, outermost first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TreeNode {
    pub name: String,
    pub module: String,
    /// Samples at or under the node, base then new.
    pub samples: (u64, u64),
    pub inclusive: Metric,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeNode>,
}

/// One side of the comparison.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Side {
    /// The captures of the side, by label.
    pub captures: Vec<String>,
    pub seconds: f64,
    pub samples: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_period_ns: Option<u64>,
    pub spans: u64,
}

/// How the figures were made, so that a reader can tell what a row claims.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Method {
    /// What sampled time and scope totals are counted against:
    /// `second`, `sample`, `capture` or the name of a scope.
    pub per: String,
    /// `share` (of samples), `ms` or `samples`.
    pub function_unit: String,
    /// What all sampled time is in: `cores`, `samples/s`, `ms` or `samples`.
    pub cpu_unit: String,
    pub confidence: f64,
    /// The relative change under which a difference is not a verdict.
    pub threshold: f64,
    /// The share of a side under which a row is not tested.
    pub min_share: f64,
    /// `batches` of each capture, or whole `captures`.
    pub units: String,
    pub notes: Vec<String>,
}

/// How many rows went which way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub regressed: u32,
    pub improved: u32,
    /// Rows with at least one tested figure.
    pub tested: u32,
}

/// A comparison of two sides.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Diff {
    pub base: Side,
    pub new: Side,
    pub method: Method,
    pub outcome: Outcome,
    /// All sampled time: cores busy when `per` is `sample` or `second`,
    /// else CPU time per unit. Absent without samples.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Metric>,
    pub functions: Vec<FunctionRow>,
    /// Functions that were left out of `functions`: untested, and smaller
    /// than the ones kept.
    #[serde(default)]
    pub functions_omitted: u64,
    pub scopes: Vec<ScopeRow>,
    #[serde(default)]
    pub scopes_omitted: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<TreeNode>,
}

impl Diff {
    /// Whether anything got slower: what a regression test fails on.
    pub fn regressed(&self) -> bool {
        self.outcome.regressed > 0 || self.cpu.is_some_and(|cpu| cpu.verdict == Verdict::Regressed)
    }
}
