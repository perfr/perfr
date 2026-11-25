// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What changed between two captures, and how sure that is.
//!
//! [`diff`] takes a base side and a new side, each one capture or several,
//! and gives a [`Diff`]: every sampled function, every scope, the call tree
//! and all sampled time, each with its value on both sides, the interval
//! of the difference and a verdict. [`text`] writes it for a terminal or a
//! pull request. [`stats`] is how the intervals are made and why.

pub mod compare;
pub mod input;
pub mod model;
pub mod stats;
pub mod text;

pub use compare::{diff, Options, Per, TIME_BATCHES};
pub use input::{CaptureInput, Frame, Sample, Span, SpanKind};
pub use model::{Diff, FunctionRow, Method, Metric, Outcome, Quantile, ScopeRow, Side, TreeNode, Verdict};
