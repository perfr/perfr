// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `POST /api/log`: the viewer's own log lines, on their way into the
//! service's log.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::LiveService;

/// `POST /api/log`: a batch of the viewer's own log lines, for the
/// service's log file. The body is JSON whatever its content type says,
/// because the viewer sends it with `navigator.sendBeacon`, which labels a
/// string body `text/plain` and is the one way to get a line out of a page
/// that is panicking or closing.
///
/// What a page sends ends up in the service's log file, and its warnings
/// and errors on the service's terminal, so none of it is taken as it
/// comes: sizes are capped and the total is rationed (a page in a logging
/// loop must not fill the service's disk), and the text is made safe to
/// print (`printable_log_text`).
pub(super) async fn viewer_log(State(svc): State<Arc<LiveService>>, body: String) -> Response {
    const MAX_LINES: usize = 512;
    const MAX_MESSAGE: usize = 4096;
    const MAX_TARGET: usize = 128;
    let Ok(mut batch) = serde_json::from_str::<crate::ViewerLogBatch>(&body) else {
        return (StatusCode::BAD_REQUEST, "expected {\"page\":..,\"lines\":[{t_ms,level,target,message}]}").into_response();
    };
    batch.lines.truncate(MAX_LINES);
    batch.page = batch.page.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    for line in &mut batch.lines {
        if line.message.len() > MAX_MESSAGE {
            truncate_at_char(&mut line.message, MAX_MESSAGE);
            line.message.push_str("...");
        }
        line.message = printable_log_text(&line.message, true);
        // The target sits inside the line's `[viewer <page> <target>]`:
        // one line, and short.
        line.target = printable_log_text(&line.target, false);
        truncate_at_char(&mut line.target, MAX_TARGET);
        if !matches!(line.level.as_str(), "error" | "warn" | "info" | "debug" | "trace") {
            line.level = "info".into();
        }
    }
    {
        let now = std::time::Instant::now();
        let mut budget = svc.viewer_log_budget.lock();
        batch.lines.retain(|line| budget.take(now, line.message.len() + line.target.len()));
        // Said with the next lines that do get through, so the log shows
        // that it has a hole and how large.
        if !batch.lines.is_empty() {
            let dropped = budget.take_dropped();
            if dropped > 0 {
                batch.lines.insert(
                    0,
                    crate::ViewerLogLine {
                        t_ms: batch.lines[0].t_ms,
                        level: "warn".into(),
                        target: "perfr_live_server".into(),
                        message: format!(
                            "{dropped} viewer log line(s) before this one were not kept: over {} KiB of them a minute",
                            VIEWER_LOG_BYTES_PER_MINUTE >> 10
                        ),
                    },
                );
            }
        }
    }
    let sink = svc.viewer_log.lock().clone();
    match sink {
        Some(sink) => sink(batch),
        None => {
            for line in batch.lines.iter().filter(|l| matches!(l.level.as_str(), "error" | "warn")) {
                eprintln!("viewer {}: {}: {}: {}", batch.page, line.level, line.target, line.message);
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Cuts `text` to at most `max` bytes, on a character boundary.
pub(super) fn truncate_at_char(text: &mut String, max: usize) {
    if text.len() > max {
        let cut = (0..=max).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
        text.truncate(cut);
    }
}

/// Text from a web page, made safe to append to a log file and to print on
/// a terminal. A log line is `[time] [LEVEL] [origin] message`, one per
/// line: with the text taken raw, a newline in it began a line that read
/// as the service's own, with any origin, and an escape sequence in a
/// warning went to the terminal of a process that is often root. Control
/// characters are written out (`\x1b`). With `multiline` a newline is
/// kept, since a panic message is several lines, but what follows it is
/// indented, so no line of the text starts where a log line starts; a tab
/// is kept too.
pub(super) fn printable_log_text(text: &str, multiline: bool) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' if multiline => out.push_str("\n    "),
            '\t' if multiline => out.push('\t'),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// How many bytes of viewer log lines the service takes a minute, all pages
/// together, and how many it takes at once after a quiet spell.
pub(crate) const VIEWER_LOG_BYTES_PER_MINUTE: usize = 1 << 20;
pub(crate) const VIEWER_LOG_BURST_BYTES: usize = 1 << 20;

/// The allowance for viewer log lines: a bucket that refills at
/// [`VIEWER_LOG_BYTES_PER_MINUTE`] and holds [`VIEWER_LOG_BURST_BYTES`]. The
/// caps on one request say nothing about the next request, and the log
/// file is not rotated, so without this a page in a logging loop writes to
/// the service's disk as fast as it can post.
pub(crate) struct ViewerLogBudget {
    bytes: f64,
    refilled: std::time::Instant,
    dropped: u64,
}

impl ViewerLogBudget {
    pub(crate) fn new(now: std::time::Instant) -> Self {
        Self { bytes: VIEWER_LOG_BURST_BYTES as f64, refilled: now, dropped: 0 }
    }

    /// Whether a line of `bytes` may be logged at `now`; one that may not
    /// is counted.
    pub(crate) fn take(&mut self, now: std::time::Instant, bytes: usize) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        self.bytes = (self.bytes + elapsed * VIEWER_LOG_BYTES_PER_MINUTE as f64 / 60.0).min(VIEWER_LOG_BURST_BYTES as f64);
        if bytes as f64 <= self.bytes {
            self.bytes -= bytes as f64;
            true
        } else {
            self.dropped += 1;
            false
        }
    }

    /// The lines refused since this was last asked.
    pub(crate) fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}
