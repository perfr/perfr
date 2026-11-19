// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The reader against a real interpreter: a `python3` child running a small
//! program of known shape. Skipped, with a line saying so, on a machine with
//! no `python3`; a child may always be read by its parent, so no privilege
//! is needed.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use perfr_pystack::{Reader, State};

const PROGRAM: &str = r#"
import sys, threading, time

def spin(seconds):
    end = time.monotonic() + seconds
    total = 0
    while time.monotonic() < end:
        total += 1
    return total

def inner_work():
    while True:
        spin(0.002)

def outer_work():
    inner_work()

def idle():
    while True:
        time.sleep(0.5)

threading.Thread(target=outer_work, name="perfr-test-worker", daemon=True).start()
threading.Thread(target=idle, name="perfr-test-idle", daemon=True).start()
print("ready", flush=True)
outer_work()
"#;

struct Python(Child);

impl Drop for Python {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the program and waits until its threads are running.
fn start_python() -> Option<Python> {
    let mut child = match Command::new("python3")
        .args(["-I", "-c", PROGRAM])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = writeln!(
                std::io::stderr(),
                "skipped: python3 could not be started ({error})"
            );
            return None;
        }
    };
    let mut line = String::new();
    let _ = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
    let python = Python(child);
    (line.trim() == "ready").then_some(python)
}

/// A clock for the readings. Any monotonic one will do here: nothing else
/// is stamped. Never zero, which the readings take to mean "no reading yet".
fn monotonic_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

#[test]
fn a_python_program_is_read_without_pausing_it() {
    let Some(python) = start_python() else { return };
    let pid = python.0.id() as i32;
    let mut reader = Reader::start(pid, 1_000_000, monotonic_ns).unwrap();
    let readings = reader.readings().clone();

    // The interpreter is found, and readings arrive.
    let deadline = Instant::now() + Duration::from_secs(20);
    while readings.stats().readings < 300 {
        if let State::Stopped { why } = readings.state() {
            panic!("the reader stopped: {why}");
        }
        assert!(
            Instant::now() < deadline,
            "no readings after 20 s; state {:?}",
            readings.state()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let State::Reading { version } = readings.state() else {
        panic!("not reading: {:?}", readings.state())
    };
    assert!(version.starts_with('3'), "{version}");

    // Python's names for its threads, by the kernel's thread ids.
    let names = readings.take_thread_names();
    let tid_of = |wanted: &str| {
        names
            .iter()
            .find(|(_, name)| name == wanted)
            .map(|(tid, _)| *tid)
    };
    let worker = tid_of("perfr-test-worker").unwrap_or_else(|| panic!("no worker among {names:?}"));
    let idle =
        tid_of("perfr-test-idle").unwrap_or_else(|| panic!("no idle thread among {names:?}"));
    assert_eq!(
        tid_of("MainThread"),
        Some(pid as u32),
        "the main thread's id is the process's: {names:?}"
    );
    let task = |tid: u32| std::path::Path::new(&format!("/proc/{pid}/task/{tid}")).exists();
    assert!(
        task(worker) && task(idle),
        "thread ids the kernel does not know: {names:?}"
    );

    // The worker's stack, innermost first: spin (or the clock it calls, which
    // is C and has no Python frame), inner_work, outer_work, then threading.
    let stats = readings.stats();
    let stack = readings
        .near(worker, stats.last_ns)
        .nearest
        .expect("a reading of the worker");
    let frames: Vec<String> = stack
        .iter()
        .map(|frame| readings.symbol(frame.symbol).unwrap().name)
        .collect();
    let at = |function: &str| {
        frames
            .iter()
            .position(|name| name.starts_with(&format!("{function} (")))
    };
    let (inner, outer) = (at("inner_work"), at("outer_work"));
    assert!(inner.is_some() && inner < outer, "{frames:?}");
    assert!(
        frames
            .iter()
            .any(|name| name.starts_with("Thread.run (") || name.starts_with("run (")),
        "{frames:?}"
    );
    // Named with the file and the line of the `def`: the program came from
    // `-c`, which Python calls `<string>`.
    assert!(
        frames[inner.unwrap()].ends_with("(<string>:11)"),
        "{frames:?}"
    );

    // The idle thread is where it sleeps.
    let stack = readings
        .near(idle, stats.last_ns)
        .nearest
        .expect("a reading of the idle thread");
    let frames: Vec<String> = stack
        .iter()
        .map(|frame| readings.symbol(frame.symbol).unwrap().name)
        .collect();
    assert!(frames[0].starts_with("idle ("), "{frames:?}");

    // Two threads spin, so the GIL changes hands, and each span is one of
    // theirs.
    let mut spans = Vec::new();
    readings.take_gil_spans(&mut spans);
    assert!(spans.len() >= 4, "{} GIL spans", spans.len());
    assert!(spans.iter().all(|span| span.end_ns > span.start_ns));
    assert!(
        spans.iter().any(|span| span.tid == worker)
            && spans.iter().any(|span| span.tid == pid as u32),
        "{spans:?}"
    );
    assert!(
        spans
            .windows(2)
            .all(|pair| pair[0].end_ns <= pair[1].start_ns),
        "spans overlap"
    );

    // Nearly every reading succeeds though nothing is paused, and the pace
    // is the one asked for.
    assert!(
        stats.failed * 20 <= stats.readings,
        "{} of {} readings failed",
        stats.failed,
        stats.readings
    );
    let per_second = (stats.readings - 1) as f64 / ((stats.last_ns - stats.first_ns) as f64 / 1e9);
    // (A loaded machine reads more slowly: the reader gives itself a fifth
    // of a core and no more.)
    assert!(
        (100.0..=1100.0).contains(&per_second),
        "{per_second:.0} readings a second"
    );

    // The target was never stopped: it is not being traced.
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    assert!(
        status
            .lines()
            .any(|line| line.starts_with("TracerPid:") && line.ends_with("\t0")),
        "{status}"
    );

    reader.stop();
    let after = readings.stats().readings;
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        readings.stats().readings,
        after,
        "a stopped reader reads no more"
    );
}

#[test]
fn a_process_that_is_not_python_is_given_up_on_with_a_reason() {
    // `sleep` has no interpreter to find. The reader looks for ten seconds
    // before saying so; stopping it sooner must not hang.
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let mut reader = Reader::start(child.id() as i32, 1_000_000, monotonic_ns).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(reader.readings().state(), State::Looking);
    let started = Instant::now();
    reader.stop();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stopping took {:?}",
        started.elapsed()
    );
    assert_eq!(reader.readings().stats().readings, 0);
    let _ = child.kill();
    let _ = child.wait();
}
