// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The capture state machine, driven the way the viewer drives it: a real
//! service on a loopback port, and Record, Stop, Clear, Open and Import over
//! HTTP against a real target process.
//!
//! Nothing here needs a privilege. Where the kernel refuses a perf ring the
//! capture degrades and still starts, runs and stops, which is all these
//! tests look at.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A service of this test's own, with its log and state files in a
/// directory that goes away with it.
struct Service {
    child: Child,
    port: u16,
    dir: tempfile::TempDir,
}

impl Service {
    fn start() -> Service {
        let dir = tempfile::tempdir().expect("a scratch directory");
        // Whatever port is free now; the service binds it a moment later.
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let stderr = std::fs::File::create(dir.path().join("stderr.log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_perfr"))
            .args(["serve", "--port", &port.to_string(), "--no-preflight"])
            .env("PERFR_LOG_DIR", dir.path())
            .env("PERFR_STATE_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("the service starts");
        let service = Service { child, port, dir };
        let deadline = Instant::now() + Duration::from_secs(30);
        while service.try_request("GET", "/api/status", "", b"").is_none() {
            assert!(Instant::now() < deadline, "the service never answered:\n{}", service.stderr());
            std::thread::sleep(Duration::from_millis(50));
        }
        service
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("stderr.log")).unwrap_or_default()
    }

    /// One HTTP exchange: the status code and the body.
    fn try_request(&self, method: &str, path: &str, content_type: &str, body: &[u8]) -> Option<(u16, Vec<u8>)> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).ok()?;
        // A Stop waits for the capture's tail, a start for its setup.
        stream.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
        if !content_type.is_empty() {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(head.as_bytes()).ok()?;
        stream.write_all(body).ok()?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).ok()?;
        let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&response[..split]).to_string();
        let status = head.split_whitespace().nth(1)?.parse().ok()?;
        let body = response[split + 4..].to_vec();
        let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
        Some((status, if chunked { dechunk(&body) } else { body }))
    }

    fn request(&self, method: &str, path: &str, content_type: &str, body: &[u8]) -> (u16, Vec<u8>) {
        self.try_request(method, path, content_type, body)
            .unwrap_or_else(|| panic!("{method} {path} got no answer:\n{}", self.stderr()))
    }

    /// POST with a JSON body (or none); the status code and the body as text.
    fn post(&self, path: &str, json: &str) -> (u16, String) {
        let content_type = if json.is_empty() { "" } else { "application/json" };
        let (status, body) = self.request("POST", path, content_type, json.as_bytes());
        (status, String::from_utf8_lossy(&body).to_string())
    }

    fn record(&self, pid: u32) -> (u16, String) {
        self.post("/api/capture/start", &format!(r#"{{"pid":{pid}}}"#))
    }

    fn status(&self) -> serde_json::Value {
        let (code, body) = self.request("GET", "/api/status", "", b"");
        assert_eq!(code, 200);
        serde_json::from_slice(&body).expect("the status is JSON")
    }

    fn capturing(&self) -> bool {
        self.status()["capturing"].as_bool().expect("a capturing flag")
    }

    /// The current capture as a bundle.
    fn export(&self) -> Vec<u8> {
        let (code, bundle) = self.request("GET", "/api/capture/export?format=bundle", "", b"");
        assert_eq!(code, 200, "{}", String::from_utf8_lossy(&bundle));
        bundle
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // SIGTERM first, so that a test that failed mid-capture does not
        // leave the capture's leases on other processes' scope rings.
        // SAFETY: a signal to this test's own child.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = body.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&body[..line_end]).trim(), 16).unwrap_or(0);
        let start = line_end + 2;
        if size == 0 || start + size > body.len() {
            break;
        }
        out.extend_from_slice(&body[start..start + size]);
        body = &body[(start + size + 2).min(body.len())..];
    }
    out
}

/// A process to capture: it sleeps, which is all a target has to do here.
struct Target(Child);

impl Target {
    fn spawn() -> Target {
        Target(Command::new("sleep").arg("120").stdin(Stdio::null()).spawn().expect("sleep runs"))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn exit(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.exit();
    }
}

#[test]
fn a_capture_is_recorded_stopped_and_recorded_again_and_refuses_what_would_mix_two() {
    let service = Service::start();
    let target = Target::spawn();
    assert!(!service.capturing());

    assert_eq!(service.record(target.pid()).0, 200, "{}", service.stderr());
    let status = service.status();
    assert_eq!(status["capturing"], true);
    assert_eq!(status["target_pid"], target.pid());

    // A second Record is refused and leaves the first capture running.
    let (code, why) = service.record(target.pid());
    assert_eq!((code, why.as_str()), (409, "a capture is already running"));
    assert!(service.capturing());
    // So is everything that would replace what the capture is filling.
    let (code, why) = service.post("/api/capture/clear", "");
    assert_eq!(code, 409, "{why}");
    assert!(why.starts_with("busy"), "{why}");
    let (code, why) = service.request("POST", "/api/capture/import", "application/zip", b"not looked at");
    assert_eq!(code, 409, "{}", String::from_utf8_lossy(&why));
    assert!(service.capturing());

    assert_eq!(service.post("/api/capture/stop", "").0, 200);
    assert!(!service.capturing());
    // The viewer's second Stop, after the first already joined the capture.
    assert_eq!(service.post("/api/capture/stop", "").0, 200);

    let bundle = service.export();
    assert_eq!(service.post("/api/capture/clear", "").0, 200);

    // Record again on the same service, then put the first capture back.
    assert_eq!(service.record(target.pid()).0, 200, "{}", service.stderr());
    assert!(service.capturing());
    assert_eq!(service.post("/api/capture/stop", "").0, 200);
    let (code, summary) = service.request("POST", "/api/capture/import", "application/zip", &bundle);
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&summary));
    let summary: serde_json::Value = serde_json::from_slice(&summary).unwrap();
    assert_eq!(summary["target_pid"], target.pid());
    let path = service.dir.path().join("first.perfr.zip");
    std::fs::write(&path, &bundle).unwrap();
    let (code, summary) = service.post("/api/capture/open", &serde_json::json!({ "path": path }).to_string());
    assert_eq!(code, 200, "{summary}");
    assert!(!service.capturing());
}

#[test]
fn a_capture_ends_with_its_target_and_the_next_one_starts_cleanly() {
    let service = Service::start();
    let mut target = Target::spawn();
    assert_eq!(service.record(target.pid()).0, 200, "{}", service.stderr());
    target.exit();
    // The loop looks every quarter of a second.
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.capturing() {
        assert!(Instant::now() < deadline, "the capture outlived its target:\n{}", service.stderr());
        std::thread::sleep(Duration::from_millis(20));
    }
    let said = service.status()["instrumentation"].as_str().unwrap_or_default().to_string();
    assert!(said.contains("exited"), "{said}");

    // Right behind it, with nobody having pressed Stop: an import is let in
    // only once the capture's thread is gone, and a new Record starts.
    let bundle = service.export();
    let (code, summary) = service.request("POST", "/api/capture/import", "application/zip", &bundle);
    assert_eq!(code, 200, "{}", String::from_utf8_lossy(&summary));
    assert!(!service.capturing(), "an import is a capture that started and finished");

    let next = Target::spawn();
    assert_eq!(service.record(next.pid()).0, 200, "{}", service.stderr());
    let status = service.status();
    assert_eq!(status["capturing"], true);
    assert_eq!(status["target_pid"], next.pid());
    assert_eq!(service.post("/api/capture/stop", "").0, 200);
    assert!(!service.capturing());
}

#[test]
fn sigterm_in_the_middle_of_a_capture_stops_it_before_the_service_dies_of_the_signal() {
    let wait_for_exit = |service: &mut Service| {
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            if let Some(status) = service.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "the service did not exit:\n{}", service.stderr());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let terminate = |service: &Service| {
        // SAFETY: a signal to this test's own child.
        assert_eq!(unsafe { libc::kill(service.child.id() as i32, libc::SIGTERM) }, 0);
    };

    let mut service = Service::start();
    let target = Target::spawn();
    assert_eq!(service.record(target.pid()).0, 200, "{}", service.stderr());
    assert!(service.capturing());
    terminate(&service);
    let status = wait_for_exit(&mut service);
    assert_eq!(status.signal(), Some(libc::SIGTERM), "it still dies of the signal: {status:?}");
    let log = service.stderr();
    assert!(log.contains("stopping the capture before exiting"), "{log}");
    assert!(!log.contains("did not stop in time"), "{log}");

    // With nothing to stop there is nothing to say.
    let mut idle = Service::start();
    terminate(&idle);
    let status = wait_for_exit(&mut idle);
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    assert!(!idle.stderr().contains("stopping the capture"));
}

/// A shell and whatever it launches, in a process group of their own so
/// that they go together. Its standard input is a pipe: a script that
/// reads a line waits there until the test says [`go`](Family::go).
struct Family(Child);

impl Family {
    fn spawn(script: &str) -> Family {
        use std::os::unix::process::CommandExt;
        Family(
            Command::new("sh")
                .args(["-c", script])
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("sh runs"),
        )
    }

    /// A process that does nothing but use its core.
    fn busy() -> Family {
        Family::spawn("while :; do :; done")
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn go(&mut self) {
        self.0.stdin.as_mut().unwrap().write_all(b"\n").unwrap();
    }

    /// The processes the shell has launched and that are running, each
    /// with the name the kernel has for it.
    fn children(&self) -> Vec<(u32, String)> {
        let mut children = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else { continue };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            // `pid (comm) state ppid ...`, and a comm can hold anything.
            let Some((comm, rest)) = stat.split_once('(').and_then(|(_, rest)| rest.rsplit_once(')')) else { continue };
            if rest.split_whitespace().nth(1).and_then(|ppid| ppid.parse().ok()) == Some(self.pid()) {
                children.push((pid, comm.to_string()));
            }
        }
        children
    }

    /// Waits for the shell to have a child of that name, and returns it.
    fn child_named(&self, name: &str) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some((pid, _)) = self.children().into_iter().find(|(_, comm)| comm == name) {
                return pid;
            }
            assert!(Instant::now() < deadline, "the shell never launched {name}: {:?}", self.children());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn exit(&mut self) {
        // SAFETY: a signal to the group this test made for its own child.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

impl Drop for Family {
    fn drop(&mut self) {
        self.exit();
    }
}

impl Service {
    fn get(&self, path: &str) -> serde_json::Value {
        let (code, body) = self.request("GET", path, "", b"");
        assert_eq!(code, 200, "{path}: {}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap_or_else(|error| panic!("{path} is not JSON: {error}"))
    }

    /// How many samples the capture holds of each of `tids`, by tid. All
    /// zero where the kernel lets nobody sample.
    fn samples_of(&self, tids: &[u32]) -> Vec<u64> {
        let whole = self.get("/api/sampling/report");
        let (first, last) = (whole["first_sample_ns"].as_u64().unwrap_or(0), whole["last_sample_ns"].as_u64().unwrap_or(0));
        tids.iter()
            .map(|tid| {
                let of_thread = self.get(&format!("/api/sampling/report?ranges={first}-{}:{tid}", last + 1));
                of_thread["samples"].as_u64().expect("a sample count")
            })
            .collect()
    }

    /// Whether the capture has a row for a thread of `pid`.
    fn has_rows_of(&self, pid: u32) -> bool {
        let found = self.get(&format!("/api/capture/targets?q={pid}"));
        found["threads"].as_array().is_some_and(|threads| threads.iter().any(|thread| thread["pid"] == pid))
    }
}

/// Enough of a second of one busy thread at a thousand samples a second to
/// say it was sampled, on a machine that is doing other things as well.
const SAMPLED: u64 = 50;

/// Sampling needs the kernel's leave (`perf_event_paranoid`), and where it
/// is not given a capture still runs, with no samples. These tests then
/// have nothing to look at; `control` is a busy process in the same capture
/// that tells that from children that were not sampled.
///
/// A run that sets `PERFR_TEST_REQUIRE_SAMPLES` says the kernel was told to
/// allow it, and no samples is then a failure: CI does, so that these
/// tests do not pass there by looking at nothing.
fn sampling_was_refused(control: u64, service: &Service) -> bool {
    if control > 0 {
        return false;
    }
    assert!(
        std::env::var_os("PERFR_TEST_REQUIRE_SAMPLES").is_none(),
        "a busy process of the capture has no samples, and this run requires them:\n{}",
        service.stderr()
    );
    eprintln!("this machine lets nobody sample (kernel.perf_event_paranoid): the samples were not looked at");
    true
}

#[test]
fn what_a_target_launched_before_and_during_the_capture_is_sampled_with_it() {
    let service = Service::start();
    // One child before Record, a fork of the shell that never calls exec;
    // one after it, another program. The shell itself only waits.
    let mut launcher = Family::spawn("(while :; do :; done) & read go; yes & wait");
    let before = launcher.child_named("sh");
    let control = Family::busy();
    let body = serde_json::json!({ "pid": launcher.pid(), "pids": [launcher.pid(), control.pid()] });
    assert_eq!(service.post("/api/capture/start", &body.to_string()).0, 200, "{}", service.stderr());
    std::thread::sleep(Duration::from_millis(300));
    launcher.go();
    let during = launcher.child_named("yes");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(service.post("/api/capture/stop", "").0, 200);

    assert!(service.has_rows_of(before) && service.has_rows_of(during), "{}", service.stderr());
    let samples = service.samples_of(&[control.pid(), before, during]);
    if sampling_was_refused(samples[0], &service) {
        return;
    }
    assert!(samples[1] >= SAMPLED, "the child that was already running: {samples:?}\n{}", service.stderr());
    assert!(samples[2] >= SAMPLED, "the child launched during the capture: {samples:?}\n{}", service.stderr());
    let log = service.stderr();
    assert!(log.contains(&format!("pid {during} launched by pid {}", launcher.pid())), "{log}");
    // Its frames are named from the program it became, not the shell it
    // was forked from: the samples of `yes` are in `yes`.
    let whole = service.get("/api/sampling/report");
    let (first, last) = (whole["first_sample_ns"].as_u64().unwrap(), whole["last_sample_ns"].as_u64().unwrap());
    let of_yes = service.get(&format!("/api/sampling/report?ranges={first}-{}:{during}", last + 1));
    let modules: Vec<&str> =
        of_yes["functions"].as_array().unwrap().iter().filter_map(|f| f["module"].as_str()).collect();
    assert!(modules.iter().any(|module| module.ends_with("yes")), "{modules:?}");
}

#[test]
fn children_are_not_sampled_when_the_capture_says_so_and_keep_their_rows() {
    let service = Service::start();
    let launcher = Family::spawn("(while :; do :; done) & wait");
    let child = launcher.child_named("sh");
    let control = Family::busy();
    let body = serde_json::json!({
        "pid": launcher.pid(),
        "pids": [control.pid()],
        "sample_children": false,
    });
    assert_eq!(service.post("/api/capture/start", &body.to_string()).0, 200, "{}", service.stderr());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(service.post("/api/capture/stop", "").0, 200);

    assert!(service.has_rows_of(child), "{}", service.stderr());
    let samples = service.samples_of(&[control.pid(), child]);
    if sampling_was_refused(samples[0], &service) {
        return;
    }
    assert_eq!(samples[1], 0, "{samples:?}\n{}", service.stderr());
}

#[test]
fn a_capture_of_two_processes_samples_both_and_ends_when_the_last_of_them_exits() {
    let service = Service::start();
    let mut first = Family::busy();
    let mut second = Family::busy();
    // Neither launched the other, and the list may name the first again.
    let body = serde_json::json!({ "pid": first.pid(), "pids": [second.pid(), first.pid()] });
    assert_eq!(service.post("/api/capture/start", &body.to_string()).0, 200, "{}", service.stderr());
    let status = service.status();
    assert_eq!(status["target_pid"], first.pid());
    assert_eq!(status["target_pids"], serde_json::json!([first.pid(), second.pid()]));
    std::thread::sleep(Duration::from_millis(1000));

    // The one the capture is named after exits: it goes on with the other.
    first.exit();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(service.capturing(), "the capture ended with the first of its two targets:\n{}", service.stderr());
    second.exit();
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.capturing() {
        assert!(Instant::now() < deadline, "the capture outlived its targets:\n{}", service.stderr());
        std::thread::sleep(Duration::from_millis(20));
    }
    let said = service.status()["instrumentation"].as_str().unwrap_or_default().to_string();
    assert!(said.contains("exited"), "{said}");

    let samples = service.samples_of(&[first.pid(), second.pid()]);
    if sampling_was_refused(samples[1], &service) {
        return;
    }
    assert!(samples[0] >= SAMPLED && samples[1] >= SAMPLED, "{samples:?}\n{}", service.stderr());
    assert!(samples[1] > samples[0], "the second ran for a second longer: {samples:?}");
}
