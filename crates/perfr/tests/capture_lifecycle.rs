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
