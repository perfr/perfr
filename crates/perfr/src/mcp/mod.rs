// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MCP server for the Rust profiler: `perfr mcp`.
//!
//! Agents (Claude Code, Cursor, anything that speaks the Model Context
//! Protocol) spawn this process. It speaks JSON-RPC on stdin, or streamable
//! HTTP on a loopback port, and drives the perfr HTTP API. The
//! capture itself stays in the long-running service, so closing the agent
//! does not stop a profile and the service does not have to speak MCP.
//!
//! A Python server on the `loop` branch wraps an optimize-and-patch harness.
//! That work edits source. This server does not: profiling only.

mod tools;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use tools::Mcp;

const HELP: &str = "\
perfr mcp [--url http://127.0.0.1:44766] [--allow-remote] [--story-dir <dir>]
perfr mcp --http [127.0.0.1:44767] [--url http://127.0.0.1:44766]

Speaks the Model Context Protocol. The default is JSON-RPC on stdin, which
is what Claude Code and Cursor spawn. --http serves the same tools as
streamable HTTP and refuses any bind that is not loopback.

--url is the perfr to drive. It must be loopback unless
--allow-remote is set. The service has no authentication; remote means
anyone who can reach it can attach to processes.

--story-dir is the only directory perfr_story_load and perfr_story_export
read and write files in. The default is the directory the server is started
in. With --http there is no default: anything on this machine can post to
the port, so those tools take a path only when --story-dir is given.

The service has to be running already:
    perfr --host 127.0.0.1
";

pub fn entry() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("perfr mcp: {error}");
            2
        }
    }
}

struct Options {
    url: String,
    allow_remote: bool,
    http: Option<Option<String>>,
    story_dir: Option<String>,
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let Some(Options { url, allow_remote, http, story_dir }) = parse_args(&args)? else {
        eprintln!("{HELP}");
        return Ok(());
    };
    let story_dir = story_dir_for(story_dir.as_deref(), http.is_some())?;
    let mcp = Arc::new(Mcp::connect(&url, allow_remote, tools::Platform::host())?.with_story_dir(story_dir));
    if let Some(bind) = http {
        let addr = parse_loopback(bind.as_deref().unwrap_or("127.0.0.1:44767"))?;
        serve_http(mcp, addr)?;
        Ok(())
    } else {
        serve_stdio(&mcp)
    }
}

/// Where the story tools may read and write files, if anywhere.
fn story_dir_for(flag: Option<&str>, http: bool) -> Result<Option<PathBuf>, String> {
    match flag {
        Some(dir) if Path::new(dir).is_dir() => Ok(Some(PathBuf::from(dir))),
        Some(dir) => Err(format!("--story-dir {dir} is not a directory")),
        // No default over HTTP: the port has no authentication, so a default
        // would let any local process write story files as this user,
        // wherever the server happened to be started.
        None if http => Ok(None),
        // Over stdin the caller is the agent that spawned this process, in
        // the directory it works in.
        None => Ok(Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))),
    }
}

/// `None` when the arguments ask for the help text.
fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut url = "http://127.0.0.1:44766".to_string();
    let mut allow_remote = false;
    let mut http: Option<Option<String>> = None;
    let mut story_dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => return Ok(None),
            "--no-preflight" => {}
            "--allow-remote" => allow_remote = true,
            "--url" => {
                i += 1;
                url = args.get(i).cloned().ok_or("--url needs http://127.0.0.1:44766")?;
            }
            "--http" => {
                let next = args.get(i + 1).filter(|s| !s.starts_with('-'));
                if next.is_some() {
                    i += 1;
                }
                http = Some(next.cloned());
            }
            "--story-dir" => {
                i += 1;
                story_dir = Some(args.get(i).cloned().ok_or("--story-dir needs a directory")?);
            }
            // clap, which sees the arguments first, takes this spelling too.
            other if other.starts_with("--story-dir=") => {
                story_dir = Some(other["--story-dir=".len()..].to_string());
            }
            other => return Err(format!("unknown argument {other}")),
        }
        i += 1;
    }
    Ok(Some(Options { url, allow_remote, http, story_dir }))
}

fn parse_loopback(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = if text.starts_with('[') {
        text.parse().map_err(|_| format!("bad --http address {text}"))?
    } else if text.starts_with(':') || text.parse::<u16>().is_ok() {
        return Err("use --http 127.0.0.1:44767".into());
    } else {
        text.parse().map_err(|_| format!("bad --http address {text}"))?
    };
    if !addr.ip().is_loopback() {
        return Err(format!(
            "{addr} is not loopback. The MCP HTTP transport stays on this machine; \
             the profiling API has no authentication."
        ));
    }
    Ok(addr)
}

fn serve_stdio(mcp: &Mcp) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let text = if line.trim_start().starts_with("Content-Length:") {
            read_framed(&mut input, &line)?
        } else {
            line.trim().to_string()
        };
        let message: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                let err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {error}") }
                });
                writeln!(output, "{err}").map_err(|e| e.to_string())?;
                output.flush().map_err(|e| e.to_string())?;
                continue;
            }
        };
        if let Some(response) = mcp.handle(&message) {
            writeln!(output, "{response}").map_err(|e| e.to_string())?;
            output.flush().map_err(|e| e.to_string())?;
        }
    }
}

fn read_framed(input: &mut impl BufRead, first: &str) -> Result<String, String> {
    let mut len = content_length(first);
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("unexpected end of framing headers".into());
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(n) = content_length(&line) {
            len = Some(n);
        }
    }
    let len = len.ok_or("Content-Length missing")?;
    if len > 1024 * 1024 {
        return Err("MCP message larger than 1MB".into());
    }
    let mut body = vec![0u8; len];
    input.read_exact(&mut body).map_err(|e| e.to_string())?;
    String::from_utf8(body).map_err(|_| "MCP message is not utf-8".to_string())
}

fn content_length(line: &str) -> Option<usize> {
    let (name, value) = line.split_once(':')?;
    if name.trim().eq_ignore_ascii_case("content-length") {
        value.trim().parse().ok()
    } else {
        None
    }
}

/// Streamable HTTP on a loopback socket. Blocks until the process is killed.
pub fn serve_http(mcp: Arc<Mcp>, addr: SocketAddr) -> Result<(), String> {
    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    eprintln!("perfr mcp listening on http://{bound}/mcp");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let mcp = mcp.clone();
        thread::spawn(move || {
            let _ = handle_http(stream, &mcp);
        });
    }
    Ok(())
}

fn handle_http(mut stream: TcpStream, mcp: &Mcp) -> Result<(), String> {
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(30))).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return write_status(&mut stream, 413, "headers too large");
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header.lines();
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let mut content_length = 0usize;
    let mut origin = None;
    let mut host = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("host") {
            host = value.to_string();
        }
    }
    if !host_is_loopback(&host) {
        return write_status(&mut stream, 403, "host is not loopback");
    }
    if let Some(origin) = &origin {
        if !origin_is_loopback(origin) {
            return write_status(&mut stream, 403, "origin is not loopback");
        }
    }
    if content_length > 1024 * 1024 {
        return write_status(&mut stream, 413, "body larger than 1MB");
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);
    if method == "OPTIONS" {
        return write_bytes(
            &mut stream,
            204,
            "text/plain",
            &[],
            "Allow: POST, GET, DELETE, OPTIONS\r\nAccess-Control-Allow-Origin: http://127.0.0.1\r\nAccess-Control-Allow-Headers: Content-Type, Accept, Mcp-Session-Id\r\n",
        );
    }
    if path != "/mcp" && path != "/" {
        return write_status(&mut stream, 404, "MCP endpoint is /mcp");
    }
    if method == "GET" || method == "DELETE" {
        // No server-pushed stream. DELETE ends nothing we stored per session:
        // the hook set lives in this process for as long as it runs.
        let status = if method == "GET" { 405 } else { 200 };
        return write_status(&mut stream, status, if method == "GET" { "method not allowed" } else { "" });
    }
    if method != "POST" {
        return write_status(&mut stream, 405, "method not allowed");
    }
    if body.is_empty() {
        return write_status(&mut stream, 400, "expected a JSON-RPC body");
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            let err = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("parse error: {error}") }
            });
            return write_json(&mut stream, 200, &err);
        }
    };
    match mcp.handle(&message) {
        Some(response) => write_json(&mut stream, 200, &response),
        None => write_status(&mut stream, 202, ""),
    }
}

fn host_is_loopback(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn origin_is_loopback(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("http://") else { return false };
    host_is_loopback(rest.split('/').next().unwrap_or(""))
}

fn write_status(stream: &mut TcpStream, status: u16, message: &str) -> Result<(), String> {
    write_bytes(stream, status, "text/plain; charset=utf-8", message.as_bytes(), "")
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) -> Result<(), String> {
    write_bytes(stream, status, "application/json", value.to_string().as_bytes(), "")
}

fn write_bytes(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8], extra: &str) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Hit {
        path: String,
        body: String,
    }

    fn spawn_service(routes: Arc<Mutex<Vec<Hit>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let header_end = loop {
                    let Ok(n) = stream.read(&mut tmp) else { break None };
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(pos);
                    }
                };
                let Some(header_end) = header_end else { continue };
                let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let request = header.lines().next().unwrap_or("");
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("/").to_string();
                let len = header
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let mut body = buf[header_end + 4..].to_vec();
                while body.len() < len {
                    let Ok(n) = stream.read(&mut tmp) else { break };
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                body.truncate(len);
                let body = String::from_utf8_lossy(&body).into_owned();
                routes.lock().unwrap().push(Hit { path: path.clone(), body: body.clone() });
                let (status, payload) = mock_reply(&method, &path, &body);
                let bytes = payload.into_bytes();
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&bytes);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn mock_reply(method: &str, path: &str, body: &str) -> (u16, String) {
        let path = path.split('?').next().unwrap_or(path);
        if method == "GET" && path == "/api/status" {
            return (200, r#"{"capturing":false,"target_pid":0,"events_live":0,"dropped":0,"instrumentation":"","hook_crash":"","capture_start_ns":0}"#.into());
        }
        if method == "GET" && path == "/api/processes" {
            return (200, r#"[{"pid":4,"name":"idle","cpu":0.1},{"pid":9,"name":"perfr-frida-target","cpu":12.4},{"pid":10,"name":"perfr","cpu":1.0}]"#.into());
        }
        if method == "POST" && path == "/api/symbols/load" {
            return (200, "{}".into());
        }
        if method == "GET" && path == "/api/symbols/status" {
            return (200, r#"{"status":"ready","function_count":3,"module_count":1}"#.into());
        }
        if method == "GET" && path == "/api/functions/search" {
            return (200, r#"{"status":"ready","total":3,"functions":[
                {"function_id":11,"name":"perfr_frida_test_inner","module":"/tmp/perfr-frida-target","size":32,"safety":"safe"},
                {"function_id":12,"name":"perfr_frida_test_outer","module":"/tmp/perfr-frida-target","size":40,"safety":"safe"},
                {"function_id":13,"name":"decode","module":"/tmp/liba.so","size":8,"safety":"unknown"},
                {"function_id":14,"name":"decode","module":"/tmp/libb.so","size":8,"safety":"unknown"}
            ]}"#.into());
        }
        if method == "POST" && path == "/api/capture/start" {
            let _ = body;
            return (200, String::new());
        }
        if method == "POST" && path == "/api/capture/stop" {
            return (200, String::new());
        }
        if method == "GET" && path == "/api/sampling/report" {
            let mut functions = Vec::new();
            for i in 0..30 {
                functions.push(json!({
                    "name": format!("fn{i}"),
                    "module": "/usr/lib/libx.so",
                    "function_id": 100 + i,
                    "self": 30 - i,
                    "inclusive": 40 - i,
                    "self_percent": 10.04,
                    "inclusive_percent": 20.0
                }));
            }
            return (200, json!({"samples": 80, "functions": functions}).to_string());
        }
        if method == "GET" && path == "/api/sampling/callstacks" {
            return (200, r#"{"order":"outermost_first","samples":4,"total_stacks":1,"offset":0,"limit":8,"stacks":[{"tid":7,"count":4,"percent":100.0,"frame_count":2,"truncated":false,"frames":["main","inner"]}]}"#.into());
        }
        if method == "GET" && path == "/api/capture/targets" {
            return (200, r#"{"t0_ns":0,"t1_ns":1000,"threads":[{"pid":9,"tid":3,"name":"Worker","process":"box3d"}],"thread_total":1,"scopes":[{"scope_id":9,"name":"SolveContacts","pid":9,"tid":3,"count":4,"total_ns":400}],"scope_total":1,"cores":[{"core":0,"busy":0.5},{"core":1,"busy":0.0}],"core_count":2,"cores_busy":1,"busy_at":0.05,"note":"readings of the capture"}"#.into());
        }
        if method == "GET" && path == "/api/functions/stats" {
            return (200, r#"{"events_scanned":10,"total":1,"offset":0,"limit":15,"order":"total","rows":[{"pid":9,"name":"perfr_frida_test_inner","kind":"hook","count":180,"total_ns":180000,"avg_ns":1000,"min_ns":800,"max_ns":1200,"stddev_ns":50}]}"#.into());
        }
        (404, format!("no route {method} {path}"))
    }

    fn mcp(url: &str) -> Mcp {
        Mcp::connect(url, false, tools::Platform::linux()).unwrap()
    }

    fn call(mcp: &Mcp, id: i64, method: &str, params: Value) -> Value {
        mcp.handle(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).unwrap()
    }

    fn tool(mcp: &Mcp, id: i64, name: &str, args: Value) -> Value {
        let response = call(mcp, id, "tools/call", json!({"name": name, "arguments": args}));
        assert!(response.get("error").is_none(), "{response}");
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{result}");
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// The message of a tool call that has to be refused.
    fn refused(mcp: &Mcp, name: &str, args: Value) -> String {
        let response = call(mcp, 1, "tools/call", json!({"name": name, "arguments": args}));
        let result = &response["result"];
        assert_eq!(result["isError"], true, "{name} was not refused: {result}");
        let payload: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        payload["error"].as_str().unwrap().to_string()
    }

    /// A server whose story directory is `<tmp>/stories`, with one story
    /// loaded. `<tmp>` itself holds a story and a source file the tools must
    /// not reach.
    fn story_server() -> (tempfile::TempDir, std::path::PathBuf, Mcp) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        std::fs::create_dir(base.join("stories")).unwrap();
        std::fs::write(base.join("main.rs"), "fn main() {}\n").unwrap();
        let url = spawn_service(Arc::new(Mutex::new(Vec::new())));
        let mcp = mcp(&url).with_story_dir(Some(base.join("stories")));
        let story = tool(&mcp, 1, "perfr_story_create", json!({"title": "tour"}));
        std::fs::write(base.join("outside.perfr.story"), story["story"].to_string()).unwrap();
        (dir, base, mcp)
    }

    #[test]
    fn story_files_are_read_and_written_inside_the_story_directory() {
        let (_dir, base, mcp) = story_server();
        let exported = tool(&mcp, 2, "perfr_story_export", json!({"path": "tour.perfr.story"}));
        assert_eq!(exported["path"], "tour.perfr.story");
        let written = std::fs::read_to_string(base.join("stories/tour.perfr.story")).unwrap();
        assert_eq!(written, exported["json"].as_str().unwrap());
        // Parents are still created, and `..` that stays inside is a path
        // like any other.
        tool(&mcp, 3, "perfr_story_export", json!({"path": "drafts/v2/tour.json"}));
        assert!(base.join("stories/drafts/v2/tour.json").is_file());
        tool(&mcp, 4, "perfr_story_export", json!({"path": "drafts/../again.json"}));
        assert!(base.join("stories/again.json").is_file());
        let loaded = tool(&mcp, 5, "perfr_story_load", json!({"path": "drafts/v2/tour.json"}));
        assert_eq!(loaded["title"], "tour");
    }

    #[test]
    fn a_story_path_outside_the_story_directory_is_refused() {
        let (_dir, base, mcp) = story_server();
        let source = base.join("main.rs");
        let outside = base.join("outside.perfr.story");
        let new_file = base.join("new.json");
        for path in [
            source.to_str().unwrap(),
            new_file.to_str().unwrap(),
            "../main.rs",
            "../new.json",
            "drafts/../../new.json",
        ] {
            let error = refused(&mcp, "perfr_story_export", json!({"path": path}));
            assert!(error.contains("--story-dir"), "{path}: {error}");
        }
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "fn main() {}\n");
        assert!(!new_file.exists());
        for path in [outside.to_str().unwrap(), "../outside.perfr.story"] {
            let error = refused(&mcp, "perfr_story_load", json!({"path": path}));
            assert!(error.contains("--story-dir"), "{path}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_story_directory_does_not_lead_out_of_it() {
        let (_dir, base, mcp) = story_server();
        std::os::unix::fs::symlink(&base, base.join("stories/up")).unwrap();
        // Dangling: writing through it would create the file it names.
        std::os::unix::fs::symlink(base.join("new.json"), base.join("stories/dangling.json")).unwrap();
        for path in ["up/main.rs", "up/new.json", "dangling.json"] {
            let error = refused(&mcp, "perfr_story_export", json!({"path": path}));
            assert!(error.contains("--story-dir"), "{path}: {error}");
        }
        assert_eq!(std::fs::read_to_string(base.join("main.rs")).unwrap(), "fn main() {}\n");
        assert!(!base.join("new.json").exists());
        let error = refused(&mcp, "perfr_story_load", json!({"path": "up/outside.perfr.story"}));
        assert!(error.contains("--story-dir"), "{error}");
    }

    #[test]
    fn export_replaces_a_story_and_nothing_else() {
        let (_dir, base, mcp) = story_server();
        let stories = base.join("stories");
        std::fs::write(stories.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(stories.join("package.json"), "{\"name\": \"x\"}\n").unwrap();
        std::fs::write(stories.join("notes.bin"), [0xffu8, 0xfe, 0x00]).unwrap();
        for name in ["main.rs", "package.json", "notes.bin"] {
            let before = std::fs::read(stories.join(name)).unwrap();
            let error = refused(&mcp, "perfr_story_export", json!({"path": name}));
            assert!(error.contains("not a perfr.story"), "{name}: {error}");
            assert_eq!(std::fs::read(stories.join(name)).unwrap(), before, "{name}");
        }
        // An earlier export is what the tool is for replacing, valid or
        // not: a story someone broke by hand is still a story.
        tool(&mcp, 2, "perfr_story_export", json!({"path": "tour.perfr.story"}));
        std::fs::write(stories.join("tour.perfr.story"), "{\"format\": \"perfr.story\", \"version\": 0}").unwrap();
        let again = tool(&mcp, 3, "perfr_story_export", json!({"path": "tour.perfr.story"}));
        assert_eq!(
            std::fs::read_to_string(stories.join("tour.perfr.story")).unwrap(),
            again["json"].as_str().unwrap()
        );
    }

    #[test]
    fn without_a_story_directory_the_story_tools_take_no_path() {
        let url = spawn_service(Arc::new(Mutex::new(Vec::new())));
        let mcp = mcp(&url);
        let story = tool(&mcp, 1, "perfr_story_create", json!({"title": "tour"}));
        let name = format!("perfr-mcp-test-{}.perfr.story", std::process::id());
        let error = refused(&mcp, "perfr_story_export", json!({"path": name}));
        assert!(error.contains("--story-dir"), "{error}");
        assert!(!std::path::Path::new(&name).exists());
        let error = refused(&mcp, "perfr_story_load", json!({"path": "Cargo.toml"}));
        assert!(error.contains("--story-dir"), "{error}");
        // The document itself still goes both ways.
        let exported = tool(&mcp, 2, "perfr_story_export", json!({}));
        assert_eq!(exported["path"], Value::Null);
        let loaded = tool(&mcp, 3, "perfr_story_load", json!({"json": story["story"].to_string()}));
        assert_eq!(loaded["title"], "tour");
    }

    #[test]
    fn story_dir_is_a_flag_of_the_stdin_parser_in_both_spellings() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for list in [&["--story-dir", "stories"][..], &["--story-dir=stories"][..]] {
            let options = parse_args(&args(list)).unwrap().unwrap();
            assert_eq!(options.story_dir.as_deref(), Some("stories"), "{list:?}");
        }
        let options = parse_args(&args(&["--http", "--story-dir", "s", "--url", "http://127.0.0.1:1"])).unwrap().unwrap();
        assert_eq!(options.http, Some(None));
        assert_eq!(options.story_dir.as_deref(), Some("s"));
        assert_eq!(options.url, "http://127.0.0.1:1");
        assert!(parse_args(&args(&["--story-dir"])).is_err());
        assert!(parse_args(&args(&[])).unwrap().unwrap().story_dir.is_none());
    }

    #[test]
    fn the_http_transport_has_no_story_directory_unless_one_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().to_str().unwrap();
        assert_eq!(story_dir_for(None, true).unwrap(), None);
        assert_eq!(story_dir_for(Some(named), true).unwrap().as_deref(), Some(dir.path()));
        assert_eq!(story_dir_for(Some(named), false).unwrap().as_deref(), Some(dir.path()));
        assert_eq!(story_dir_for(None, false).unwrap(), std::env::current_dir().ok());
        let missing = dir.path().join("missing");
        let error = story_dir_for(Some(missing.to_str().unwrap()), false).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn initialize_lists_the_tools_with_schemas() {
        let routes = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_service(routes);
        let mcp = mcp(&url);
        let init = call(&mcp, 1, "initialize", json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0"}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "perfr");
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("perfr_attach"));
        assert!(mcp.handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
        let listed = call(&mcp, 2, "tools/list", json!({}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 29);
        assert!(tools.iter().any(|spec| spec["name"] == "perfr_logs"));
        for spec in tools {
            assert!(spec["description"].as_str().unwrap().len() > 40, "{spec}");
            assert_eq!(spec["inputSchema"]["type"], "object");
            assert!(spec["inputSchema"].get("properties").is_some(), "{spec}");
        }
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"perfr_story_create"), "{names:?}");
        assert!(names.contains(&"perfr_story_cpu"), "{names:?}");
    }

    #[test]
    fn a_story_is_authored_and_checked_against_the_capture() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_service(hits.clone());
        let mcp = mcp(&url);
        let created = tool(
            &mcp,
            1,
            "perfr_story_create",
            json!({
                "title": "box3d",
                "summary": "A tour. It does not state a core count.",
                "capture": "captures/box3d.perfr.stream"
            }),
        );
        assert_eq!(created["format"], "perfr.story");
        assert_eq!(created["version"], 1);
        tool(
            &mcp,
            2,
            "perfr_story_add_step",
            json!({
                "step": {
                    "id": "cores",
                    "kind": "evidence",
                    "title": "Scheduler",
                    "body": "Count the busy lanes.",
                    "duration_ms": 4000,
                    "view": { "fit": "content", "focused": [{ "kind": "scheduler" }] }
                }
            }),
        );
        tool(
            &mcp,
            3,
            "perfr_story_set_view",
            json!({
                "id": "cores",
                "view": {
                    "t0_frac": 0.2,
                    "t1_frac": 0.8,
                    "fit": "span",
                    "focused": [{ "kind": "scheduler" }]
                }
            }),
        );
        tool(
            &mcp,
            4,
            "perfr_story_add_overlay",
            json!({
                "step_id": "cores",
                "overlay": {
                    "kind": "callout",
                    "id": "note",
                    "text": "One lane per core.",
                    "anchor": { "time_frac": 0.5, "track": { "kind": "scheduler" }, "y": 0.2 }
                }
            }),
        );
        let checked = tool(&mcp, 5, "perfr_story_validate", json!({}));
        assert_eq!(checked["ok"], true);
        let exported = tool(&mcp, 6, "perfr_story_export", json!({}));
        let again = tool(
            &mcp,
            7,
            "perfr_story_load",
            json!({"json": exported["json"]}),
        );
        assert_eq!(again["steps"][0]["id"], "cores");
        let threads = tool(
            &mcp,
            8,
            "perfr_story_find_threads",
            json!({"query": "work"}),
        );
        assert_eq!(threads["threads"][0]["tid"], 3);
        let scopes = tool(
            &mcp,
            9,
            "perfr_story_find_scopes",
            json!({"query": "solve", "t0_ns": 0, "t1_ns": 1000}),
        );
        assert_eq!(scopes["scopes"][0]["scope_id"], 9);
        let cpu = tool(
            &mcp,
            10,
            "perfr_story_cpu",
            json!({"t0_ns": 0, "t1_ns": 1000}),
        );
        assert_eq!(cpu["cores_busy"], 1);
        assert_eq!(cpu["core_count"], 2);
        let hot = tool(
            &mcp,
            11,
            "perfr_story_hotspots",
            json!({"t0_ns": 10, "t1_ns": 20, "limit": 1}),
        );
        assert_eq!(hot["supported"], true);
        assert_eq!(hot["hotspots"][0]["name"], "fn0");
        let paths: Vec<String> = hits
            .lock()
            .unwrap()
            .iter()
            .map(|h| h.path.clone())
            .collect();
        assert!(
            paths
                .iter()
                .any(|p| p.starts_with("/api/capture/targets?") && p.contains("q=work")),
            "{paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| p.contains("start_ns=10") && p.contains("end_ns=20")),
            "{paths:?}"
        );
    }

    #[test]
    fn processes_are_filtered_and_paged() {
        let url = spawn_service(Arc::new(Mutex::new(Vec::new())));
        let mcp = mcp(&url);
        let page = tool(&mcp, 1, "perfr_list_processes", json!({"query":"perfr","limit":1,"offset":0}));
        assert_eq!(page["total"], 2);
        assert_eq!(page["processes"][0]["name"], "perfr-frida-target");
        assert_eq!(page["processes"][0]["cpu_percent"], 12.4);
        let next = tool(&mcp, 2, "perfr_list_processes", json!({"query":"perfr","limit":1,"offset":1}));
        assert_eq!(next["processes"][0]["pid"], 10);
    }

    #[test]
    fn attach_arm_start_unhook_and_recapture() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_service(hits.clone());
        let mcp = mcp(&url);
        let attached = tool(&mcp, 1, "perfr_attach", json!({"pid": 9}));
        assert_eq!(attached["function_count"], 3);
        let found = tool(&mcp, 2, "perfr_search_functions", json!({"query":"inner","limit":5}));
        assert_eq!(found["functions"][0]["function_id"], 11);
        assert_eq!(found["functions"][0]["module"], "perfr-frida-target");
        let ambiguous = tool(&mcp, 3, "perfr_arm_hooks", json!({"names":["decode"]}));
        assert_eq!(ambiguous["armed"], 0);
        assert_eq!(ambiguous["ambiguous"][0]["modules"].as_array().unwrap().len(), 2);
        let by_name = tool(&mcp, 4, "perfr_arm_hooks", json!({"names":["perfr_frida_test_inner"]}));
        assert_eq!(by_name["hooks"][0]["name"], "perfr_frida_test_inner");
        tool(&mcp, 5, "perfr_unhook", json!({"all": true}));
        let armed = tool(&mcp, 6, "perfr_arm_hooks", json!({"function_ids":[11]}));
        assert_eq!(armed["armed"], 1);
        assert_eq!(armed["hooks"][0]["function_id"], 11);
        assert_eq!(armed["hooks"][0]["name"], "perfr_frida_test_inner");
        assert_eq!(armed["hooks"][0]["module"], "perfr-frida-target");
        let started = tool(&mcp, 5, "perfr_start_capture", json!({"sampling": true, "scheduler": true}));
        assert_eq!(started["started"], true);
        assert_eq!(started["hooks"], 1);
        let stats = tool(&mcp, 6, "perfr_function_stats", json!({"query":"inner"}));
        assert_eq!(stats["rows"][0]["count"], 180);
        assert_eq!(stats["rows"][0]["kind"], "hook");
        let report = tool(&mcp, 7, "perfr_sampling_report", json!({"limit":2,"offset":1}));
        assert_eq!(report["supported"], true);
        assert_eq!(report["samples"], 80);
        assert_eq!(report["total_functions"], 30);
        assert_eq!(report["hotspots"].as_array().unwrap().len(), 2);
        assert_eq!(report["hotspots"][0]["name"], "fn1");
        assert_eq!(report["hotspots"][0]["module"], "libx.so");
        assert_eq!(report["hotspots"][0]["self_percent"], 10.0);
        let stacks = tool(&mcp, 8, "perfr_callstacks", json!({}));
        assert_eq!(stacks["stacks"][0]["frames"][0], "main");
        tool(&mcp, 9, "perfr_stop_capture", json!({}));
        let cleared = tool(&mcp, 10, "perfr_unhook", json!({"all": true}));
        assert_eq!(cleared["armed"], 0);
        tool(&mcp, 11, "perfr_start_capture", json!({}));
        let starts: Vec<Value> = hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.path == "/api/capture/start")
            .map(|hit| serde_json::from_str(&hit.body).unwrap())
            .collect();
        assert_eq!(starts.len(), 2);
        assert_eq!(starts[0]["instrumented_functions"][0]["function_id"], 11);
        assert_eq!(starts[0]["sampling"], true);
        assert_eq!(starts[0]["context_switches"], true);
        assert_eq!(starts[1]["instrumented_functions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn macos_reports_sampling_unsupported_and_does_not_start() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_service(hits.clone());
        let mcp = Mcp::connect(&url, false, tools::Platform::macos()).unwrap();
        let caps = tool(&mcp, 1, "perfr_capabilities", json!({}));
        assert_eq!(caps["platform"], "macos");
        assert_eq!(caps["features"]["sampling"]["supported"], false);
        assert_eq!(caps["features"]["dynamic_hooks"]["supported"], true);
        assert_eq!(caps["features"]["dynamic_hooks"]["engines"], json!(["frida"]));
        let refused = tool(&mcp, 2, "perfr_start_capture", json!({"pid": 9, "sampling": true}));
        assert_eq!(refused["started"], false);
        assert_eq!(refused["supported"], false);
        assert!(refused["unsupported"].as_array().unwrap().iter().any(|v| v == "sampling"));
        let report = tool(&mcp, 3, "perfr_sampling_report", json!({}));
        assert_eq!(report["supported"], false);
        let stacks = tool(&mcp, 4, "perfr_callstacks", json!({}));
        assert_eq!(stacks["supported"], false);
        assert!(hits.lock().unwrap().iter().all(|hit| hit.path != "/api/capture/start"));
        assert!(hits.lock().unwrap().iter().all(|hit| !hit.path.starts_with("/api/sampling")));
    }

    #[test]
    fn what_the_agent_is_told_matches_the_build_and_the_platform_table() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_service(hits);
        let mcp = Mcp::connect(&url, false, tools::Platform::windows()).unwrap();
        let init = call(&mcp, 1, "initialize", json!({}));
        assert_eq!(init["result"]["serverInfo"]["version"], crate::build_info::package_version());
        // Windows samples (`Platform::windows`). Text that calls sampling
        // Linux-only keeps an agent there from ever asking for the report.
        let caps = tool(&mcp, 2, "perfr_capabilities", json!({}));
        assert_eq!(caps["features"]["sampling"]["supported"], true);
        let instructions = init["result"]["instructions"].as_str().unwrap().to_string();
        let listed = call(&mcp, 3, "tools/list", json!({})).to_string();
        for text in [&instructions, &listed] {
            assert!(!text.contains("scheduler are Linux-only"), "{text}");
            assert!(!text.contains("on Linux, perfr_sampling_report"), "{text}");
            assert!(!text.contains("Default on for Linux, unsupported on macOS"), "{text}");
            assert!(!text.contains("the scheduler, kernel uprobes and auto-profiling are Linux-only"), "{text}");
        }
        assert!(instructions.contains("Windows"), "{instructions}");
        // And a refusal does not explain itself with another platform's name.
        let mut none = tools::Platform::windows();
        none.sampling = false;
        let mcp = Mcp::connect(&url, false, none).unwrap();
        let report = tool(&mcp, 4, "perfr_sampling_report", json!({}));
        assert_eq!(report["supported"], false);
        assert!(!report["message"].as_str().unwrap().contains("macOS"), "{report}");
        assert!(report["message"].as_str().unwrap().contains("perfr_function_stats"), "{report}");
    }

    #[test]
    fn remote_url_and_remote_bind_are_refused() {
        match Mcp::connect("http://10.1.2.3:44766", false, tools::Platform::linux()) {
            Err(error) => assert!(error.contains("not loopback"), "{error}"),
            Ok(_) => panic!("a non-loopback service URL was accepted"),
        }
        assert!(parse_loopback("0.0.0.0:44767").is_err());
        assert!(parse_loopback("192.168.1.9:1").is_err());
        assert!(parse_loopback("127.0.0.1:44767").is_ok());
    }

    #[test]
    fn streamable_http_initialize_and_rejects_a_foreign_origin() {
        let url = spawn_service(Arc::new(Mutex::new(Vec::new())));
        let mcp = Arc::new(Mcp::connect(&url, false, tools::Platform::linux()).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { continue };
                let mcp = mcp.clone();
                thread::spawn(move || {
                    let _ = handle_http(stream, &mcp);
                });
            }
        });
        let body = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}).to_string();
        let ok = http_post(port, &body, None);
        assert!(ok.contains("200 OK"), "{ok}");
        assert!(ok.contains("\"protocolVersion\":\"2024-11-05\""), "{ok}");
        let denied = http_post(port, &body, Some("http://evil.example"));
        assert!(denied.contains("403"), "{denied}");
        let get = http_exchange(port, "GET", "/mcp", "", None);
        assert!(get.contains("405"), "{get}");
    }

    fn http_post(port: u16, body: &str, origin: Option<&str>) -> String {
        let extra = origin.map(|o| format!("Origin: {o}\r\n")).unwrap_or_default();
        http_exchange(port, "POST", "/mcp", &format!("Content-Type: application/json\r\n{extra}Content-Length: {}\r\n", body.len()), Some(body))
    }

    fn http_exchange(port: u16, method: &str, path: &str, headers: &str, body: Option<&str>) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}\r\n");
        if headers.is_empty() {
            request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
        }
        stream.write_all(request.as_bytes()).unwrap();
        if let Some(body) = body {
            stream.write_all(body.as_bytes()).unwrap();
        }
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }
}
