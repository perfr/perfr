// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr diff` on captures written to disk, and the same comparison
//! asked of a running service: the real binary, real `.perfr.zip` files.

use std::process::{Command, Output};

use perfr_capture::{CaptureBundle, EventRow, FrameRow, SampleRow};
use perfr_live_event::{kind, LiveEvent};

const MS: u64 = 1_000_000;

/// Four seconds of a program sampled every millisecond: `solve` runs for
/// `solve_share` of the time and `draw` for 30%, under `main`; a `physics`
/// scope of `physics_ms` and a frame every 16 ms. `seed` moves the noise.
fn bundle(seed: u64, solve_share: f64, physics_ms: f64) -> CaptureBundle {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut unit = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let frame = |id: u32, name: &str| FrameRow { id, name: name.to_string(), module: "/opt/game/bin/game".to_string(), address: u64::from(id) * 64, language: 0 };
    // A name is stored once per id, as the service's are.
    let event = |kind: u8, name: &str, start_ns: u64, duration_ns: u64| EventRow {
        event: LiveEvent { start_ns, duration_ns, tid: 11, pid: 10, kind, name_id: name.len() as u32, ..LiveEvent::default() },
        name: name.to_string(),
    };
    let mut events = Vec::new();
    let mut samples = Vec::new();
    for at in 0..4000u64 {
        let now = 1_000_000_000 + at * MS;
        let drawn = unit();
        let leaf = if drawn < solve_share {
            Some(2)
        } else if drawn < solve_share + 0.30 {
            Some(3)
        } else {
            None
        };
        if let Some(leaf) = leaf {
            samples.push(SampleRow { timestamp_ns: now, tid: 11, frames: vec![leaf, 1] });
            events.push(event(kind::SAMPLE, "", now, MS));
        }
        if at % 16 == 0 {
            events.push(event(kind::FRAME, "frame", now, 16 * MS));
            let jitter = 1.0 + 0.1 * (unit() - 0.5);
            events.push(event(kind::API_SCOPE, "physics", now, (physics_ms * jitter * MS as f64) as u64));
        }
    }
    CaptureBundle {
        target_pid: 10,
        slice_ns: None,
        processes: Vec::new(),
        threads: Vec::new(),
        events,
        samples,
        frames: vec![frame(1, "main"), frame(2, "solve"), frame(3, "draw")],
        track_order: String::new(),
        cores: Vec::new(),
        topology: Vec::new(),
    }
}

struct Files {
    dir: tempfile::TempDir,
}

impl Files {
    fn new() -> Files {
        Files { dir: tempfile::tempdir().expect("a scratch directory") }
    }

    fn write(&self, name: &str, bundle: &CaptureBundle) -> String {
        let path = self.dir.path().join(name);
        std::fs::write(&path, bundle.to_zip().expect("the bundle zips")).unwrap();
        path.to_str().unwrap().to_string()
    }
}

fn perfr(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_perfr")).args(args).output().expect("perfr runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn two_captures_are_compared_and_the_exit_status_can_stand_for_the_verdict() {
    let files = Files::new();
    let base = files.write("base.perfr.zip", &bundle(1, 0.25, 5.0));
    let same = files.write("same.perfr.zip", &bundle(2, 0.25, 5.0));
    let slow = files.write("slow.perfr.zip", &bundle(3, 0.35, 6.0));

    let found = perfr(&["diff", &base, &slow]);
    let text = stdout(&found);
    assert_eq!(found.status.code(), Some(0), "{text}\n{}", stderr(&found));
    assert!(text.starts_with("REGRESSED: "), "{text}");
    assert!(text.contains("base  base.perfr.zip: ") && text.contains("new   slow.perfr.zip: "), "{text}");
    assert!(text.contains("samples every 1.00 ms"), "{text}");
    let solve = text.lines().find(|line| line.contains(" solve ")).unwrap_or_else(|| panic!("{text}"));
    assert!(solve.contains('▲') && solve.trim_end().ends_with("game"), "{solve}");
    let physics = text.lines().find(|line| line.trim_start().starts_with("physics")).unwrap_or_else(|| panic!("{text}"));
    assert!(physics.contains('▲') && physics.contains("+1"), "{physics}");

    // The same comparison as a regression test.
    assert_eq!(perfr(&["diff", &base, &slow, "--fail-on-regression"]).status.code(), Some(1));
    let quiet = perfr(&["diff", &base, &same, "--fail-on-regression"]);
    assert_eq!(quiet.status.code(), Some(0), "{}", stdout(&quiet));
    assert!(stdout(&quiet).starts_with("NO CHANGE: "), "{}", stdout(&quiet));
    // Getting faster is not a failure.
    let faster = perfr(&["diff", &slow, &base, "--fail-on-regression"]);
    assert_eq!(faster.status.code(), Some(0));
    assert!(stdout(&faster).starts_with("IMPROVED: "), "{}", stdout(&faster));
}

#[test]
fn the_comparison_is_printed_as_json_or_markdown_and_counted_against_what_is_asked() {
    let files = Files::new();
    let base = files.write("base.perfr.zip", &bundle(4, 0.25, 5.0));
    let slow = files.write("slow.perfr.zip", &bundle(5, 0.35, 6.0));

    let json = perfr(&["diff", &base, &slow, "--json", "--per", "frame", "--confidence", "0.99", "--threshold", "10", "--min-share", "1"]);
    assert_eq!(json.status.code(), Some(0), "{}", stderr(&json));
    let diff: serde_json::Value = serde_json::from_str(&stdout(&json)).expect("JSON");
    assert_eq!(diff["method"]["per"], "frame");
    assert_eq!(diff["method"]["function_unit"], "ms");
    assert_eq!((diff["method"]["confidence"].as_f64(), diff["method"]["threshold"].as_f64(), diff["method"]["min_share"].as_f64()), (Some(0.99), Some(0.1), Some(0.01)));
    let solve = diff["functions"].as_array().unwrap().iter().find(|row| row["name"] == "solve").unwrap();
    assert_eq!(solve["self"]["verdict"], "regressed");
    assert_eq!(solve["module"], "game");
    // Milliseconds of CPU per frame: a quarter of 16, then 35% of it.
    let (was, is) = (solve["self"]["base"].as_f64().unwrap(), solve["self"]["new"].as_f64().unwrap());
    assert!((was - 4.0).abs() < 0.4 && (is - 5.6).abs() < 0.4, "{was} {is}");
    assert_eq!(diff["tree"]["children"][0]["name"], "main");
    let physics = diff["scopes"].as_array().unwrap().iter().find(|row| row["name"] == "physics").unwrap();
    assert_eq!((physics["kind"].as_str(), physics["mean"]["verdict"].as_str()), (Some("scope"), Some("regressed")));
    assert_eq!(physics["calls"], serde_json::json!([250, 250]));

    let markdown = stdout(&perfr(&["diff", &base, &slow, "--markdown", "--top", "3"]));
    assert!(markdown.starts_with("**REGRESSED: "), "{markdown}");
    assert!(markdown.contains("\n### Functions, their own time (CPU time per s)\n"), "{markdown}");
    assert!(markdown.contains("| ▲ |"), "{markdown}");
}

#[test]
fn several_captures_a_side_are_the_units_of_the_comparison() {
    let files = Files::new();
    let run = |name: &str, seed: u64, solve: f64| files.write(name, &bundle(seed, solve, 5.0));
    let base: Vec<String> = (0..3).map(|i| run(&format!("main-{i}.perfr.zip"), 10 + i, 0.25)).collect();
    let new: Vec<String> = (0..3).map(|i| run(&format!("pr-{i}.perfr.zip"), 20 + i, 0.35)).collect();
    let mut args = vec!["diff", "--base"];
    args.extend(base.iter().map(String::as_str));
    args.push("--new");
    args.extend(new.iter().map(String::as_str));
    args.extend(["--json", "--fail-on-regression"]);
    let found = perfr(&args);
    assert_eq!(found.status.code(), Some(1), "{}", stderr(&found));
    let diff: serde_json::Value = serde_json::from_str(&stdout(&found)).expect("JSON");
    assert_eq!(diff["method"]["units"], "captures");
    assert_eq!(diff["base"]["captures"], serde_json::json!(["main-0.perfr.zip", "main-1.perfr.zip", "main-2.perfr.zip"]));
    assert_eq!(diff["new"]["samples"].as_u64().map(|n| n > 7000), Some(true));
}

#[test]
fn what_cannot_be_compared_is_said_and_is_status_two() {
    let files = Files::new();
    let base = files.write("base.perfr.zip", &bundle(6, 0.25, 5.0));
    let missing = files.dir.path().join("missing.perfr.zip");
    let missing = missing.to_str().unwrap();
    let text = files.dir.path().join("notes.perfr.zip");
    std::fs::write(&text, "not a zip").unwrap();

    for (args, said) in [
        (vec!["diff", &base], "give the base capture and the new one"),
        (vec!["diff"], "give the base capture and the new one"),
        (vec!["diff", &base, missing], "missing.perfr.zip"),
        (vec!["diff", &base, text.to_str().unwrap()], "notes.perfr.zip: not a capture"),
        (vec!["diff", &base, &base, "--json", "--markdown"], "--json or --markdown, not both"),
        (vec!["diff", &base, &base, "--base", &base], "not both"),
        (vec!["diff", &base, &base, "--per", "tick"], "no scope or frame named \"tick\""),
        (vec!["diff", &base, &base, "--confidence", "95"], "confidence is a fraction"),
        (vec!["diff", &base, &base, "--threshold", "250"], "threshold is a percentage"),
    ] {
        let found = perfr(&args);
        assert_eq!(found.status.code(), Some(2), "{args:?}: {}", stdout(&found));
        assert!(stderr(&found).contains(said), "{args:?}: {}", stderr(&found));
        assert!(stdout(&found).is_empty(), "{args:?}: {}", stdout(&found));
    }
    let help = stdout(&perfr(&["diff", "--help"]));
    assert!(help.contains("--fail-on-regression") && help.contains("perfr diff before.perfr.zip after.perfr.zip"), "{help}");
    assert!(stdout(&perfr(&["--help"])).contains("diff"), "the command is listed");
}

#[cfg(target_os = "linux")]
mod service {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct Service {
        child: Child,
        port: u16,
        dir: tempfile::TempDir,
    }

    impl Service {
        fn start() -> Service {
            let dir = tempfile::tempdir().expect("a scratch directory");
            let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let log = std::fs::File::create(dir.path().join("stderr.log")).unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_perfr"))
                .args(["serve", "--port", &port.to_string(), "--no-preflight"])
                .env("PERFR_LOG_DIR", dir.path())
                .env("PERFR_STATE_DIR", dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .expect("the service starts");
            let service = Service { child, port, dir };
            let deadline = Instant::now() + Duration::from_secs(30);
            while service.try_request("GET", "/api/status", b"").is_none() {
                assert!(Instant::now() < deadline, "the service never answered:\n{}", service.log());
                std::thread::sleep(Duration::from_millis(50));
            }
            service
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap_or_default()
        }

        fn try_request(&self, method: &str, path: &str, body: &[u8]) -> Option<(u16, String)> {
            let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
            stream.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
            let head = format!(
                "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/zip\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).ok()?;
            stream.write_all(body).ok()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).ok()?;
            let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
            let status = String::from_utf8_lossy(&response[..split]).split_whitespace().nth(1)?.parse().ok()?;
            Some((status, String::from_utf8_lossy(&response[split + 4..]).to_string()))
        }

        fn post(&self, path: &str, body: &[u8]) -> (u16, String) {
            self.try_request("POST", path, body).unwrap_or_else(|| panic!("POST {path} got no answer:\n{}", self.log()))
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn the_service_compares_a_capture_it_is_sent_with_the_one_it_holds() {
        let service = Service::start();
        let base = bundle(7, 0.25, 5.0).to_zip().unwrap();
        let slow = bundle(8, 0.35, 6.0).to_zip().unwrap();

        // It holds nothing yet.
        let (status, text) = service.post("/api/diff", &slow);
        assert_eq!(status, 400, "{text}");
        assert!(text.contains("this capture: nothing to compare"), "{text}");

        let (status, text) = service.post("/api/capture/import", &slow);
        assert_eq!(status, 200, "{text}\n{}", service.log());
        // What is sent is the base unless said otherwise: the capture on
        // screen is the one being judged.
        let (status, text) = service.post("/api/diff?name=base.perfr.zip", &base);
        assert_eq!(status, 200, "{text}");
        let diff: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(diff["base"]["captures"], serde_json::json!(["base.perfr.zip"]));
        assert_eq!(diff["new"]["captures"], serde_json::json!(["this capture"]));
        let verdict = |diff: &serde_json::Value, name: &str| {
            let row = diff["functions"].as_array().unwrap().iter().find(|row| row["name"] == name).unwrap().clone();
            row["self"]["verdict"].as_str().unwrap().to_string()
        };
        assert_eq!(verdict(&diff, "solve"), "regressed");
        assert!(diff["outcome"]["regressed"].as_u64().unwrap() >= 2, "{}", diff["outcome"]);

        // The other way round, and counted per frame.
        let (status, text) = service.post("/api/diff?name=base.perfr.zip&current=base&per=frame&threshold=10", &base);
        assert_eq!(status, 200, "{text}");
        let diff: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(diff["base"]["captures"], serde_json::json!(["this capture"]));
        assert_eq!(verdict(&diff, "solve"), "improved");
        assert_eq!((diff["method"]["per"].as_str(), diff["method"]["threshold"].as_f64()), (Some("frame"), Some(0.1)));

        // The capture it holds is still the one it held.
        let (status, text) = service.post("/api/diff", &slow);
        assert_eq!(status, 200, "{text}");
        let same: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(same["outcome"]["regressed"], 0, "{}", same["outcome"]);

        for (query, body, said) in [
            ("", b"not a zip".as_slice(), "not a capture"),
            ("?per=tick", base.as_slice(), "no scope or frame named \"tick\""),
            ("?confidence=2", base.as_slice(), "confidence is a fraction"),
        ] {
            let (status, text) = service.post(&format!("/api/diff{query}"), body);
            assert_eq!(status, 400, "{query}: {text}");
            assert!(text.contains(said), "{query}: {text}");
        }
    }
}
