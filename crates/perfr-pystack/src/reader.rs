// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The thread that reads a process's Python stacks with py-spy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use py_spy::config::{Config, LineNo, LockingStrategy};
use py_spy::PythonSpy;

use crate::readings::{RawFrame, Readings, State, ThreadReading};

/// How long the reader keeps looking for the interpreter. A process that
/// was only just started has not built it yet; a second or two covers that,
/// and the rest is for a program that loads Python as a plugin.
const LOOK_FOR: Duration = Duration::from_secs(10);
const LOOK_EVERY: Duration = Duration::from_millis(250);

/// Failed readings in a row after which the interpreter is looked for again:
/// the process may have replaced itself (`exec`) with another Python.
const FAILURES_BEFORE_LOOKING_AGAIN: u32 = 200;

/// The reader may spend this share of its time reading: one part in five.
const COST_TO_PERIOD: u64 = 5;

/// Reads the Python stacks of one process until dropped.
pub struct Reader {
    readings: Arc<Readings>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reader {
    /// Starts reading `pid` every `period_ns`, stamping each reading with
    /// `clock`, which must be the clock the capture's samples are stamped
    /// with. Returns at once: finding the interpreter is the thread's work,
    /// and [`Readings::state`] says how that went.
    pub fn start(pid: i32, period_ns: u64, clock: fn() -> u64) -> std::io::Result<Reader> {
        let readings = Arc::new(Readings::new(period_ns));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("perfr-python".to_string())
            .spawn({
                let (readings, stop) = (readings.clone(), stop.clone());
                move || {
                    run(pid, period_ns, clock, &readings, &stop);
                    readings.finish();
                }
            })?;
        Ok(Reader {
            readings,
            stop,
            thread: Some(thread),
        })
    }

    pub fn readings(&self) -> &Arc<Readings> {
        &self.readings
    }

    /// Stops the reader and waits for it. What it read stays readable.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop();
    }
}

fn config() -> Config {
    Config {
        // Never pause the target: see the crate documentation.
        blocking: LockingStrategy::NonBlocking,
        native: false,
        collect_thread_names: true,
        // The line a function is defined at, not the line it is on: the
        // first is the function's identity, the second would make a row per
        // line.
        lineno: LineNo::First,
        full_filenames: false,
        ..Config::default()
    }
}

fn alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Whether an error from py-spy is the kernel refusing to let this process
/// read the other's memory.
fn is_permission(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("permission denied")
        || text.contains("operation not permitted")
        || text.contains("eperm")
}

/// Finds the interpreter, trying for [`LOOK_FOR`]. `None` when it gave up,
/// with the reason left in the readings' state.
fn look(pid: i32, readings: &Readings, stop: &AtomicBool) -> Option<PythonSpy> {
    readings.set_state(State::Looking);
    let config = config();
    let started = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let error = match PythonSpy::new(pid, &config) {
            Ok(spy) => {
                readings.set_state(State::Reading {
                    version: spy.version.to_string(),
                });
                return Some(spy);
            }
            Err(error) => format!("{error:#}"),
        };
        let why = if !alive(pid) {
            Some("the process has exited".to_string())
        } else if is_permission(&error) {
            Some(format!(
                "this process may not read the memory of pid {pid} ({error})"
            ))
        } else if started.elapsed() >= LOOK_FOR {
            Some(error)
        } else {
            None
        };
        if let Some(why) = why {
            readings.set_state(State::Stopped { why });
            return None;
        }
        std::thread::sleep(LOOK_EVERY);
    }
}

fn run(pid: i32, base_period_ns: u64, clock: fn() -> u64, readings: &Readings, stop: &AtomicBool) {
    let Some(mut spy) = look(pid, readings, stop) else {
        return;
    };
    let mut period_ns = base_period_ns;
    // The cost of a reading, smoothed: one slow reading does not halve the
    // pace, a process with a thousand threads does.
    let mut cost_ns: u64 = 0;
    let mut failures_in_a_row: u32 = 0;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let before_ns = clock();
        let took_ns;
        match spy.get_stack_traces() {
            Ok(traces) => {
                let after_ns = clock();
                took_ns = after_ns.saturating_sub(before_ns);
                failures_in_a_row = 0;
                let threads: Vec<ThreadReading<'_>> = traces
                    .iter()
                    // Without the kernel's id there is no native thread to
                    // join this stack to.
                    .filter_map(|trace| {
                        Some(ThreadReading {
                            tid: trace.os_thread_id? as u32,
                            name: trace.thread_name.as_deref(),
                            holds_gil: trace.owns_gil,
                            frames: trace
                                .frames
                                .iter()
                                .map(|frame| RawFrame {
                                    name: &frame.name,
                                    file: frame
                                        .short_filename
                                        .as_deref()
                                        .unwrap_or(&frame.filename),
                                    line: frame.line,
                                    entry: frame.is_entry || frame.is_shim_entry,
                                })
                                .collect(),
                        })
                    })
                    .collect();
                // The reading belongs to the middle of the time it took.
                readings.record(before_ns + took_ns / 2, &threads);
            }
            Err(error) => {
                took_ns = clock().saturating_sub(before_ns);
                readings.record_failure();
                failures_in_a_row += 1;
                if failures_in_a_row == 1 {
                    log::debug!("a reading of pid {pid}'s Python stacks failed: {error:#}");
                }
                if failures_in_a_row >= FAILURES_BEFORE_LOOKING_AGAIN {
                    failures_in_a_row = 0;
                    match look(pid, readings, stop) {
                        Some(found) => spy = found,
                        None => return,
                    }
                    next = Instant::now();
                    continue;
                }
            }
        }
        cost_ns = if cost_ns == 0 {
            took_ns
        } else {
            (cost_ns * 7 + took_ns) / 8
        };
        let wanted = base_period_ns.max(cost_ns * COST_TO_PERIOD);
        if wanted != period_ns {
            period_ns = wanted;
            readings.set_period(period_ns);
        }
        next += Duration::from_nanos(period_ns);
        let now = Instant::now();
        match next.checked_duration_since(now) {
            Some(wait) => std::thread::sleep(wait),
            // Behind: do not try to catch up with a burst of readings.
            None => next = now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_to_read_memory_is_told_from_other_failures() {
        assert!(is_permission(
            "Failed to open process: Permission denied (os error 13)"
        ));
        assert!(is_permission("Operation not permitted (os error 1)"));
        assert!(!is_permission(
            "Failed to find python version from target process"
        ));
    }
}
