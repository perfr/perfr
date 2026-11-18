// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr share`, `perfr login` and `perfr logout`: a capture goes to the
//! website from the terminal, as the Share button sends it from the viewer.
//!
//! What is uploaded is a `.perfr.stream`, the file the website's viewer
//! opens. One is taken as it is. A `.perfr.zip` is opened by a service of
//! its own, started for the conversion on a spare loopback port and stopped
//! after, and exported from it: the code a running service exports a
//! capture with, and no second writer of the format. With no file, the
//! capture the service on this machine holds is exported.
//!
//! The website knows the caller by an API token, made on its account page.
//! `perfr login` checks one and keeps it in a file only this user reads;
//! `PERFR_TOKEN` is used first when it is set. The token never goes on a
//! command line, where every process of the machine could read it: `curl`,
//! which does the HTTPS (as it does for `perfr update`), is handed its
//! headers on its standard input.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Where captures are shared. `--site`, then `PERFR_SHARE_BASE`, override it.
pub const DEFAULT_SITE: &str = "https://perfr.dev";
const DEFAULT_SERVICE: &str = "http://127.0.0.1:44766";
const TOKEN_ENV: &str = "PERFR_TOKEN";
const SITE_ENV: &str = "PERFR_SHARE_BASE";
const CREDENTIALS_FILE: &str = "credentials.json";
/// The hello frame a `.perfr.stream` opens with: a little-endian payload
/// length, the frame type, then the magic (perfr-live-protocol).
const STREAM_MAGIC: &[u8] = b"OLIV";
const FRAME_HELLO: u8 = 1;
/// The website's limits on one part of a capture's name (`handles.py` in
/// perfr-web): letters, digits, `.`, `_` and `-`, a letter or digit at
/// each end.
const SEGMENT_MAX: usize = 64;

#[derive(Clone, Debug, Default)]
pub struct ShareOpts {
    pub file: Option<String>,
    pub name: Option<String>,
    pub replace: bool,
    pub unlisted: bool,
    pub service: Option<String>,
    pub site: Option<String>,
    pub json: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LoginOpts {
    pub site: Option<String>,
    /// Say who is signed in, and read no token.
    pub status: bool,
}

pub struct Request<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// Sent to `curl` on its standard input, the token among them.
    pub headers: Vec<(String, String)>,
    /// A file sent as the body, as it is.
    pub body_file: Option<&'a Path>,
    /// A small body, sent as JSON.
    pub body_json: Option<String>,
    /// Where the response's body is written; it is returned otherwise.
    pub out: Option<&'a Path>,
}

impl<'a> Request<'a> {
    fn new(method: &'a str, url: &'a str) -> Self {
        Self { method, url, headers: Vec::new(), body_file: None, body_json: None, out: None }
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub trait Http {
    fn send(&self, request: &Request) -> Result<Response, String>;
}

/// `curl`, as `perfr update` uses it. There is no `wget` fallback: it
/// cannot be given a header without putting it in its arguments.
pub struct Curl;

impl Http for Curl {
    fn send(&self, request: &Request) -> Result<Response, String> {
        let curl = which("curl").ok_or("curl is needed to reach the website, and it is not on PATH")?;
        let mut command = Command::new(curl);
        command.args(curl_args(request));
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| format!("curl: {error}"))?;
        let config = curl_config(request);
        if let Some(mut stdin) = child.stdin.take() {
            // A curl that has already failed has closed its end.
            stdin.write_all(config.as_bytes()).ok();
        }
        let output = child.wait_with_output().map_err(|error| format!("curl: {error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim().trim_start_matches("curl: ").trim();
            return Err(if detail.is_empty() { format!("curl failed ({})", output.status) } else { detail.to_string() });
        }
        split_status(output.stdout)
    }
}

/// What `curl` is run with. Nothing secret: the headers are in the
/// configuration it reads from its standard input (`-K -`).
fn curl_args(request: &Request) -> Vec<String> {
    let mut args: Vec<String> = ["-sS", "-K", "-", "--connect-timeout", "20", "-X", request.method]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    args.push("-A".into());
    args.push(format!("perfr/{}", perfr_build_info::package_version()));
    // The status after the body, on a line of its own.
    args.push("-w".into());
    args.push("\n%{http_code}".into());
    if let Some(path) = request.body_file {
        args.push("--data-binary".into());
        args.push(format!("@{}", path.display()));
    } else if let Some(body) = &request.body_json {
        args.push("--data-binary".into());
        args.push(body.clone());
    }
    if let Some(path) = request.out {
        args.push("-o".into());
        args.push(path.display().to_string());
    }
    args.push(request.url.to_string());
    args
}

/// The headers as lines of a `curl` configuration file.
fn curl_config(request: &Request) -> String {
    let mut config = String::new();
    for (name, value) in &request.headers {
        let line = format!("{name}: {value}");
        let quoted: String = line
            .chars()
            .filter(|ch| *ch != '\r' && *ch != '\n')
            .flat_map(|ch| match ch {
                '"' | '\\' => vec!['\\', ch],
                other => vec![other],
            })
            .collect();
        config.push_str(&format!("header = \"{quoted}\"\n"));
    }
    config
}

/// `curl -w "\n%{http_code}"` ends its output with the status: the body is
/// what comes before that last line.
fn split_status(mut stdout: Vec<u8>) -> Result<Response, String> {
    let at = stdout.iter().rposition(|byte| *byte == b'\n').ok_or("curl printed no status")?;
    let status = String::from_utf8_lossy(&stdout[at + 1..]).trim().parse::<u16>().map_err(|_| "curl printed no status")?;
    stdout.truncate(at);
    Ok(Response { status, body: stdout })
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        let exe = dir.join(format!("{name}.exe"));
        if cfg!(windows) && exe.is_file() {
            return Some(exe);
        }
    }
    None
}

/// What the process was started in: where the token is, and which site.
pub struct Env {
    pub token: Option<String>,
    pub site: Option<String>,
    pub config_dir: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty());
        Self { token: var(TOKEN_ENV), site: var(SITE_ENV), config_dir: config_dir() }
    }
}

/// `$PERFR_CONFIG_DIR`, else `%APPDATA%\perfr` on Windows, else
/// `$XDG_CONFIG_HOME/perfr`, else `~/.config/perfr`.
fn config_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    if let Some(dir) = var("PERFR_CONFIG_DIR") {
        return Some(dir);
    }
    if cfg!(windows) {
        if let Some(dir) = var("APPDATA") {
            return Some(dir.join("perfr"));
        }
    }
    var("XDG_CONFIG_HOME")
        .or_else(|| var("HOME").or_else(|| var("USERPROFILE")).map(|home| home.join(".config")))
        .map(|dir| dir.join("perfr"))
}

/// The site a token is sent to: `https://`, or `http://` on this machine
/// (a website under test). Anything else could read the token on its way.
pub fn clean_site(site: &str) -> Result<String, String> {
    let site = site.trim().trim_end_matches('/');
    let rest = site.strip_prefix("https://").or_else(|| {
        let rest = site.strip_prefix("http://")?;
        let host = rest.rsplit_once(':').map_or(rest, |(host, _)| host);
        matches!(host, "127.0.0.1" | "localhost" | "[::1]").then_some(rest)
    });
    match rest {
        Some(rest) if !rest.is_empty() && !rest.contains(['/', '?', '#', '@', ' ']) => Ok(site.to_string()),
        _ => Err(format!("{site}: the site is https://<host>, or http:// on this machine")),
    }
}

fn site_of(explicit: Option<&str>, env: &Env) -> Result<String, String> {
    clean_site(explicit.or(env.site.as_deref()).unwrap_or(DEFAULT_SITE))
}

fn credentials_path(env: &Env) -> Result<PathBuf, String> {
    env.config_dir.as_ref().map(|dir| dir.join(CREDENTIALS_FILE)).ok_or_else(|| "no home directory to keep the token in".to_string())
}

fn read_credentials(path: &Path) -> serde_json::Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// The token for `site`: `PERFR_TOKEN`, else the one `perfr login` kept.
fn token_for(site: &str, env: &Env) -> Option<String> {
    if let Some(token) = &env.token {
        return Some(token.clone());
    }
    let path = credentials_path(env).ok()?;
    read_credentials(&path).get(site)?.as_str().map(str::to_string).filter(|token| !token.is_empty())
}

/// Writes the file with the token for `site` in it, or without (`None`).
/// The file is the user's alone from the moment it exists.
fn store_token(site: &str, token: Option<&str>, env: &Env) -> Result<PathBuf, String> {
    let path = credentials_path(env)?;
    let mut all = read_credentials(&path);
    match token {
        Some(token) => all.insert(site.to_string(), Value::String(token.to_string())),
        None => all.remove(site),
    };
    let dir = path.parent().ok_or("no directory for the token")?;
    fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let text = serde_json::to_string_pretty(&Value::Object(all)).map_err(|error| error.to_string())? + "\n";
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        // A file from before this one may be readable by others.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    }
    file.write_all(text.as_bytes()).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

fn bearer(token: &str) -> (String, String) {
    ("Authorization".to_string(), format!("Bearer {token}"))
}

fn json_of(response: &Response) -> Value {
    serde_json::from_slice(&response.body).unwrap_or(Value::Null)
}

fn text_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Who a token is, as the website says it: `GET /api/me`.
struct Account {
    name: String,
    handle: String,
    remaining: Option<u64>,
    max_capture: Option<u64>,
}

fn account(http: &dyn Http, site: &str, token: &str) -> Result<Account, String> {
    let url = format!("{site}/api/me");
    let mut request = Request::new("GET", &url);
    request.headers.push(bearer(token));
    request.headers.push(("Accept".into(), "application/json".into()));
    let response = http.send(&request)?;
    let body = json_of(&response);
    match response.status {
        200 => {
            let user = body.get("user").cloned().unwrap_or(Value::Null);
            let quota = body.get("quota").cloned().unwrap_or(Value::Null);
            let name = [text_of(&user, "name"), text_of(&user, "handle"), text_of(&user, "id")]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or("an account with no name")
                .to_string();
            Ok(Account {
                name,
                handle: text_of(&user, "handle").to_string(),
                remaining: quota.get("remaining").and_then(Value::as_u64),
                max_capture: quota.get("max_capture_bytes").and_then(Value::as_u64),
            })
        }
        401 => Err(format!("{site} does not accept this token. Make one on {site}/account and run `perfr login`.")),
        status => Err(format!("{url}: HTTP {status} {}", said(&body, &response))),
    }
}

/// The website's words for an error: `message`, else `error`, else the
/// start of whatever came back.
fn said(body: &Value, response: &Response) -> String {
    for key in ["message", "error"] {
        let text = text_of(body, key);
        if !text.is_empty() {
            return text.to_string();
        }
    }
    String::from_utf8_lossy(&response.body).trim().chars().take(200).collect()
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// A file's name as a capture's on the website: `runs/frame spike.perfr.zip`
/// is `frame-spike`. `None` when nothing of it is left to use.
pub fn name_from_file(path: &str) -> Option<String> {
    let file = Path::new(path).file_name()?.to_str()?;
    let stem = [".perfr.stream", ".perfr.zip", ".stream", ".zip"]
        .iter()
        .find_map(|suffix| file.strip_suffix(suffix))
        .unwrap_or(file);
    let mut name = String::new();
    for ch in stem.chars() {
        let ch = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') { ch } else { '-' };
        if ch == '-' && name.ends_with('-') {
            continue;
        }
        name.push(ch);
    }
    let name: String = name.chars().take(SEGMENT_MAX).collect();
    let name = name.trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
    (!name.is_empty() && name != "." && name != "..").then(|| name.to_string())
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Stream,
    Bundle,
}

/// What a file is, by how it begins and not by what it is called.
fn kind_of(path: &Path) -> Result<Kind, String> {
    let mut head = [0u8; 9];
    let read = fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let head = &head[..read];
    if head.len() == 9 && head[4] == FRAME_HELLO && &head[5..9] == STREAM_MAGIC {
        return Ok(Kind::Stream);
    }
    if head.starts_with(b"PK") {
        return Ok(Kind::Bundle);
    }
    Err(format!(
        "{}: not a capture. perfr share takes a .perfr.stream or a .perfr.zip, or no file for the capture the service holds.",
        path.display()
    ))
}

/// A service of this program's own on a spare loopback port, stopped when
/// this is dropped.
struct PrivateService {
    child: Child,
    url: String,
}

impl PrivateService {
    fn start(http: &dyn Http) -> Result<Self, String> {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .map_err(|error| format!("no free port: {error}"))?
            .port();
        let exe = std::env::current_exe().map_err(|error| format!("cannot find perfr itself: {error}"))?;
        let child = Command::new(exe)
            .args(["serve", "--no-preflight", "--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start a service to read the capture: {error}"))?;
        let mut service = Self { child, url: format!("http://127.0.0.1:{port}") };
        let status = format!("{}/api/status", service.url);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(response) = http.send(&Request::new("GET", &status)) {
                if response.status == 200 {
                    return Ok(service);
                }
            }
            if let Ok(Some(exit)) = service.child.try_wait() {
                return Err(format!("the service that reads the capture stopped ({exit})"));
            }
            if Instant::now() > deadline {
                return Err("the service that reads the capture did not start".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for PrivateService {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The capture `service` holds, as a stream with its batches deflated (what
/// the Share button uploads), written to `out`.
fn export_stream(http: &dyn Http, service: &str, out: &Path) -> Result<(), String> {
    let url = format!("{service}/api/capture/export?format=stream&wire=deflate");
    let mut request = Request::new("GET", &url);
    request.out = Some(out);
    let response = http.send(&request).map_err(|error| {
        format!("no capture from {service}: {error}. Is perfr running? Give a file, or --service <url>.")
    })?;
    if response.status != 200 {
        let detail = fs::read_to_string(out).unwrap_or_default();
        return Err(format!("{service} did not export its capture: HTTP {} {}", response.status, detail.trim()));
    }
    Ok(())
}

/// A `.perfr.zip` as a stream in `out`, by way of a private service.
fn convert_bundle(http: &dyn Http, bundle: &Path, out: &Path) -> Result<(), String> {
    let absolute = fs::canonicalize(bundle).map_err(|error| format!("{}: {error}", bundle.display()))?;
    let service = PrivateService::start(http)?;
    let url = format!("{}/api/capture/open", service.url);
    let mut request = Request::new("POST", &url);
    request.headers.push(("Content-Type".into(), "application/json".into()));
    request.body_json = Some(serde_json::json!({ "path": absolute.display().to_string() }).to_string());
    let response = http.send(&request)?;
    if response.status != 200 {
        return Err(format!(
            "{}: could not be opened: {}",
            bundle.display(),
            String::from_utf8_lossy(&response.body).trim()
        ));
    }
    export_stream(http, &service.url, out)
}

/// A service's address for the export: `http://` on this machine, where
/// the service listens unless it was told otherwise.
fn clean_service(service: &str) -> Result<String, String> {
    let service = service.trim().trim_end_matches('/');
    match service.strip_prefix("http://") {
        Some(rest) if !rest.is_empty() && !rest.contains(['/', '?', '#', '@', ' ']) => Ok(service.to_string()),
        _ => Err(format!("{service}: the service is http://<host>:<port>")),
    }
}

/// The upload itself. `Ok` is what the website answered, as JSON.
fn upload(http: &dyn Http, site: &str, token: &str, stream: &Path, opts: &ShareOpts, name: Option<&str>) -> Result<Value, String> {
    let url = format!("{site}/api/share");
    let mut request = Request::new("POST", &url);
    request.headers.push(bearer(token));
    request.headers.push(("Content-Type".into(), "application/octet-stream".into()));
    request.headers.push(("Accept".into(), "application/json".into()));
    request.headers.push(("X-Perfr-Version".into(), crate::build_info::version_line()));
    if let Some(name) = name {
        request.headers.push(("X-Perfr-Name".into(), name.to_string()));
    }
    if opts.replace {
        request.headers.push(("X-Perfr-Replace".into(), "1".into()));
    }
    if opts.unlisted {
        request.headers.push(("X-Perfr-Listed".into(), "0".into()));
    }
    request.body_file = Some(stream);
    let response = http.send(&request)?;
    let body = json_of(&response);
    let error = text_of(&body, "error");
    match (response.status, error) {
        (200, _) if !text_of(&body, "url").is_empty() => Ok(body),
        (401, _) => Err(format!("{site} does not accept this token. Make one on {site}/account and run `perfr login`.")),
        (409, "name_taken") => Err(format!(
            "you already have a capture named {} ({}). Pass --replace to put this one in its place, or --name for another name.",
            name.unwrap_or(text_of(&body, "name")),
            text_of(&body, "existing_url"),
        )),
        (400, "bad_name") => Err(format!("{}: that name cannot be used. {}", name.unwrap_or(""), text_of(&body, "message"))),
        (413, _) => Err(format!(
            "the capture is too large to share ({}{}). Save a selected range in the viewer and share that.",
            megabytes(fs::metadata(stream).map(|meta| meta.len()).unwrap_or(0)),
            body.get("limit_bytes").and_then(Value::as_u64).map(|limit| format!(", the limit is {}", megabytes(limit))).unwrap_or_default(),
        )),
        (status, _) => Err(format!("{url}: HTTP {status} {}", said(&body, &response))),
    }
}

pub fn share(opts: &ShareOpts, env: &Env, http: &dyn Http, out: &mut dyn Write, err: &mut dyn Write) -> Result<(), String> {
    if opts.file.is_some() && opts.service.is_some() {
        return Err("give a file or --service, not both".into());
    }
    let site = site_of(opts.site.as_deref(), env)?;
    let token = token_for(&site, env).ok_or_else(|| {
        format!("not signed in. Make an API token on {site}/account, then run `perfr login` (or set {TOKEN_ENV}).")
    })?;
    // Who and how much, before anything is read or converted.
    let who = account(http, &site, &token)?;

    let scratch = tempfile::tempdir().map_err(|error| format!("no temporary directory: {error}"))?;
    let made = scratch.path().join("capture.perfr.stream");
    let (stream, name) = match &opts.file {
        Some(file) => {
            let path = PathBuf::from(file);
            let name = opts.name.clone().or_else(|| name_from_file(file));
            match kind_of(&path)? {
                Kind::Stream => (path, name),
                Kind::Bundle => {
                    writeln!(err, "perfr: reading {file}").ok();
                    convert_bundle(http, &path, &made)?;
                    (made, name)
                }
            }
        }
        None => {
            let service = clean_service(opts.service.as_deref().unwrap_or(DEFAULT_SERVICE))?;
            writeln!(err, "perfr: taking the capture of {service}").ok();
            export_stream(http, &service, &made)?;
            (made, opts.name.clone())
        }
    };
    if kind_of(&stream).ok() != Some(Kind::Stream) {
        return Err(format!("{}: what came out is not a .perfr.stream", stream.display()));
    }
    let size = fs::metadata(&stream).map_err(|error| format!("{}: {error}", stream.display()))?.len();
    for (limit, what) in [(who.max_capture, "the most one capture can be"), (who.remaining, "what is left of your storage")] {
        if let Some(limit) = limit.filter(|limit| size > *limit) {
            return Err(format!(
                "the capture is {}, and {what} is {}. Save a selected range in the viewer and share that.",
                megabytes(size),
                megabytes(limit)
            ));
        }
    }
    writeln!(err, "perfr: sharing {} as {}", megabytes(size), who.name).ok();
    let answer = upload(http, &site, &token, &stream, opts, name.as_deref())?;
    if opts.json {
        writeln!(out, "{answer}").ok();
    } else {
        writeln!(out, "{}", text_of(&answer, "url")).ok();
    }
    if who.handle.is_empty() {
        writeln!(err, "perfr: choose a handle on {site}/handle and the link becomes {site}/<handle>/<name>").ok();
    }
    Ok(())
}

/// Reads a token (`input` is the terminal, or a pipe), asks the website
/// who it is, and keeps it.
pub fn login(opts: &LoginOpts, env: &Env, http: &dyn Http, input: &mut dyn Read, prompt: bool, err: &mut dyn Write) -> Result<(), String> {
    let site = site_of(opts.site.as_deref(), env)?;
    if prompt {
        write!(err, "Make an API token on {site}/account and paste it here: ").ok();
        err.flush().ok();
    }
    let mut text = String::new();
    // One line, and not more than a token could ever be.
    input.take(4096).read_to_string(&mut text).map_err(|error| format!("could not read the token: {error}"))?;
    let token = text.lines().next().unwrap_or("").trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(format!("no token given. One is made on {site}/account."));
    }
    let who = account(http, &site, token)?;
    let path = store_token(&site, Some(token), env)?;
    writeln!(err, "perfr: signed in to {site} as {}. The token is in {}.", who.name, path.display()).ok();
    Ok(())
}

/// Who the kept token is, asked of the website: what a script checks
/// before it does the work a share would end.
pub fn login_status(opts: &LoginOpts, env: &Env, http: &dyn Http, out: &mut dyn Write) -> Result<(), String> {
    let site = site_of(opts.site.as_deref(), env)?;
    let token = token_for(&site, env).ok_or_else(|| {
        format!("not signed in. Make an API token on {site}/account, then run `perfr login` (or set {TOKEN_ENV}).")
    })?;
    let who = account(http, &site, &token)?;
    let handle = if who.handle.is_empty() { "no handle yet".to_string() } else { format!("{site}/{}/", who.handle) };
    writeln!(out, "signed in to {site} as {} ({handle})", who.name).ok();
    Ok(())
}

pub fn logout(opts: &LoginOpts, env: &Env, err: &mut dyn Write) -> Result<(), String> {
    let site = site_of(opts.site.as_deref(), env)?;
    let path = credentials_path(env)?;
    if read_credentials(&path).contains_key(&site) {
        store_token(&site, None, env)?;
        writeln!(err, "perfr: the token for {site} is gone from {}. Revoke it on {site}/account to end it.", path.display()).ok();
    } else {
        writeln!(err, "perfr: no token for {site} is kept here.").ok();
    }
    Ok(())
}

fn finish(what: &str, result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("perfr {what}: {error}");
            1
        }
    }
}

pub fn run_share(opts: ShareOpts) -> i32 {
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    finish("share", share(&opts, &Env::from_process(), &Curl, &mut out, &mut err))
}

pub fn run_login(opts: LoginOpts) -> i32 {
    use std::io::IsTerminal;
    if opts.status {
        return finish("login", login_status(&opts, &Env::from_process(), &Curl, &mut std::io::stdout()));
    }
    let stdin = std::io::stdin();
    let prompt = stdin.is_terminal();
    finish("login", login(&opts, &Env::from_process(), &Curl, &mut stdin.lock(), prompt, &mut std::io::stderr()))
}

pub fn run_logout(opts: LoginOpts) -> i32 {
    finish("logout", logout(&opts, &Env::from_process(), &mut std::io::stderr()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// The website and a service, as far as these calls go.
    #[derive(Default)]
    struct Fake {
        seen: RefCell<Vec<String>>,
        headers: RefCell<Vec<Vec<(String, String)>>>,
        bodies: RefCell<Vec<Vec<u8>>>,
        me: Option<(u16, String)>,
        share: Option<(u16, String)>,
        export: Option<Vec<u8>>,
    }

    impl Http for Fake {
        fn send(&self, request: &Request) -> Result<Response, String> {
            self.seen.borrow_mut().push(format!("{} {}", request.method, request.url));
            self.headers.borrow_mut().push(request.headers.clone());
            let answer = |pair: &Option<(u16, String)>| {
                let (status, body) = pair.clone().unwrap_or((500, String::new()));
                Ok(Response { status, body: body.into_bytes() })
            };
            if request.url.ends_with("/api/me") {
                return answer(&self.me);
            }
            if request.url.ends_with("/api/share") {
                self.bodies.borrow_mut().push(fs::read(request.body_file.unwrap()).unwrap());
                return answer(&self.share);
            }
            if request.url.contains("/api/capture/export") {
                let Some(bytes) = &self.export else {
                    return Err("connection refused".into());
                };
                fs::write(request.out.unwrap(), bytes).unwrap();
                return Ok(Response { status: 200, body: Vec::new() });
            }
            Err(format!("unexpected {}", request.url))
        }
    }

    const ME: &str = r#"{"user": {"id": "u1", "name": "Ada Lovelace", "handle": "ada", "avatar": ""},
        "quota": {"used_bytes": 0, "limit_bytes": 1000, "remaining": 1000, "max_capture_bytes": 600}}"#;
    const STORED: &str = r#"{"id": "0123456789abcdefghijkl", "url": "https://perfr.dev/ada/spike", "path": "spike", "handle": "ada", "listed": true}"#;

    fn stream_bytes(extra: usize) -> Vec<u8> {
        let mut bytes = vec![8, 0, 0, 0, FRAME_HELLO];
        bytes.extend_from_slice(STREAM_MAGIC);
        bytes.extend_from_slice(&[1, 0, 32, 0]);
        bytes.resize(bytes.len() + extra, 0);
        bytes
    }

    fn env_in(dir: &Path, token: Option<&str>) -> Env {
        Env { token: token.map(str::to_string), site: None, config_dir: Some(dir.to_path_buf()) }
    }

    fn run(opts: &ShareOpts, env: &Env, fake: &Fake) -> (Result<(), String>, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let result = share(opts, env, fake, &mut out, &mut err);
        (result, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_stream_file_is_uploaded_as_it_is_under_its_own_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("frame spike.perfr.stream");
        fs::write(&file, stream_bytes(100)).unwrap();
        let fake = Fake { me: Some((200, ME.into())), share: Some((200, STORED.into())), ..Default::default() };
        let opts = ShareOpts { file: Some(file.display().to_string()), ..Default::default() };
        let (result, out, err) = run(&opts, &env_in(dir.path(), Some("perfr_secret")), &fake);
        result.unwrap();
        assert_eq!(out, "https://perfr.dev/ada/spike\n");
        assert!(err.contains("as Ada Lovelace"), "{err}");
        assert_eq!(*fake.seen.borrow(), ["GET https://perfr.dev/api/me", "POST https://perfr.dev/api/share"]);
        assert_eq!(fake.bodies.borrow()[0], stream_bytes(100));
        let headers = &fake.headers.borrow()[1];
        assert_eq!(header(headers, "Authorization"), Some("Bearer perfr_secret"));
        assert_eq!(header(headers, "Content-Type"), Some("application/octet-stream"));
        assert_eq!(header(headers, "X-Perfr-Name"), Some("frame-spike"));
        assert_eq!(header(headers, "X-Perfr-Replace"), None);
        assert_eq!(header(headers, "X-Perfr-Listed"), None);
        assert!(header(headers, "X-Perfr-Version").unwrap().starts_with("perfr "));
    }

    #[test]
    fn the_flags_reach_the_website_and_json_prints_its_answer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.perfr.stream");
        fs::write(&file, stream_bytes(0)).unwrap();
        let fake = Fake { me: Some((200, ME.into())), share: Some((200, STORED.into())), ..Default::default() };
        let opts = ShareOpts {
            file: Some(file.display().to_string()),
            name: Some("runs/spike".into()),
            replace: true,
            unlisted: true,
            json: true,
            ..Default::default()
        };
        let (result, out, _) = run(&opts, &env_in(dir.path(), Some("t")), &fake);
        result.unwrap();
        let answer: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(answer["id"], "0123456789abcdefghijkl");
        let headers = &fake.headers.borrow()[1];
        assert_eq!(header(headers, "X-Perfr-Name"), Some("runs/spike"));
        assert_eq!(header(headers, "X-Perfr-Replace"), Some("1"));
        assert_eq!(header(headers, "X-Perfr-Listed"), Some("0"));
    }

    #[test]
    fn with_no_file_the_capture_of_the_service_is_exported_and_shared() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            me: Some((200, ME.into())),
            share: Some((200, STORED.into())),
            export: Some(stream_bytes(40)),
            ..Default::default()
        };
        let (result, _, err) = run(&ShareOpts::default(), &env_in(dir.path(), Some("t")), &fake);
        result.unwrap();
        assert!(err.contains("http://127.0.0.1:44766"), "{err}");
        assert_eq!(fake.seen.borrow()[1], "GET http://127.0.0.1:44766/api/capture/export?format=stream&wire=deflate");
        assert_eq!(fake.bodies.borrow()[0], stream_bytes(40));
        // The site names a capture that came with no name.
        assert_eq!(header(&fake.headers.borrow()[2], "X-Perfr-Name"), None);
        // The token goes to the website and never to the service.
        assert_eq!(header(&fake.headers.borrow()[1], "Authorization"), None);

        let other = ShareOpts { service: Some("http://127.0.0.1:44799/".into()), ..Default::default() };
        run(&other, &env_in(dir.path(), Some("t")), &fake).0.unwrap();
        assert!(fake.seen.borrow().iter().any(|line| line.starts_with("GET http://127.0.0.1:44799/api/capture/export")));

        let none = Fake { me: Some((200, ME.into())), ..Default::default() };
        let error = run(&ShareOpts::default(), &env_in(dir.path(), Some("t")), &none).0.unwrap_err();
        assert!(error.contains("Is perfr running?"), "{error}");
    }

    #[test]
    fn nothing_is_sent_without_a_token_or_past_the_quota() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.perfr.stream");
        fs::write(&file, stream_bytes(700)).unwrap();
        let opts = ShareOpts { file: Some(file.display().to_string()), ..Default::default() };
        let fake = Fake { me: Some((200, ME.into())), share: Some((200, STORED.into())), ..Default::default() };

        let error = run(&opts, &env_in(dir.path(), None), &fake).0.unwrap_err();
        assert!(error.contains("perfr login"), "{error}");
        assert!(error.contains("https://perfr.dev/account"), "{error}");
        assert!(fake.seen.borrow().is_empty());

        // 713 bytes against 600 for one capture.
        let error = run(&opts, &env_in(dir.path(), Some("t")), &fake).0.unwrap_err();
        assert!(error.contains("the most one capture can be"), "{error}");
        assert_eq!(*fake.seen.borrow(), ["GET https://perfr.dev/api/me"]);

        let refused = Fake { me: Some((401, r#"{"error": "login_required"}"#.into())), ..Default::default() };
        let error = run(&opts, &env_in(dir.path(), Some("old")), &refused).0.unwrap_err();
        assert!(error.contains("does not accept this token"), "{error}");
        assert_eq!(refused.seen.borrow().len(), 1);
    }

    #[test]
    fn what_the_website_refuses_is_said_in_words() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("spike.perfr.stream");
        fs::write(&file, stream_bytes(0)).unwrap();
        let opts = ShareOpts { file: Some(file.display().to_string()), ..Default::default() };
        let refusal = |status: u16, body: &str| {
            let fake = Fake { me: Some((200, ME.into())), share: Some((status, body.into())), ..Default::default() };
            run(&opts, &env_in(dir.path(), Some("t")), &fake).0.unwrap_err()
        };
        let taken = refusal(409, r#"{"error": "name_taken", "name": "spike", "existing_url": "https://perfr.dev/ada/spike"}"#);
        assert!(taken.contains("--replace") && taken.contains("https://perfr.dev/ada/spike"), "{taken}");
        let name = refusal(400, r#"{"error": "bad_name", "message": "Use letters, digits."}"#);
        assert!(name.contains("Use letters, digits."), "{name}");
        let large = refusal(413, r#"{"error": "capture is too large", "limit_bytes": 209715200}"#);
        assert!(large.contains("200.0 MB"), "{large}");
        let rate = refusal(429, r#"{"error": "too many uploads"}"#);
        assert!(rate.contains("429") && rate.contains("too many uploads"), "{rate}");
        let down = refusal(502, "<html>Bad gateway</html>");
        assert!(down.contains("502") && down.contains("Bad gateway"), "{down}");
        // A 200 that is not the website's answer is not a link.
        let odd = refusal(200, "<html>a captive portal</html>");
        assert!(odd.contains("captive portal"), "{odd}");
    }

    #[test]
    fn a_file_that_is_no_capture_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake { me: Some((200, ME.into())), share: Some((200, STORED.into())), ..Default::default() };
        for (name, bytes) in [("page.html", &b"<!doctype html><title>x</title>"[..]), ("empty.perfr.stream", b""), ("short", b"PK"[..1].as_ref())] {
            let file = dir.path().join(name);
            fs::write(&file, bytes).unwrap();
            let opts = ShareOpts { file: Some(file.display().to_string()), ..Default::default() };
            let error = run(&opts, &env_in(dir.path(), Some("t")), &fake).0.unwrap_err();
            assert!(error.contains("not a capture"), "{name}: {error}");
        }
        let missing = ShareOpts { file: Some(dir.path().join("nope").display().to_string()), ..Default::default() };
        assert!(run(&missing, &env_in(dir.path(), Some("t")), &fake).0.is_err());
        assert!(fake.bodies.borrow().is_empty());
        assert_eq!(kind_of(&{
            let zip = dir.path().join("b.perfr.zip");
            fs::write(&zip, b"PK\x03\x04rest").unwrap();
            zip
        }), Ok(Kind::Bundle));
    }

    #[test]
    fn login_keeps_a_token_the_website_knows_in_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(&dir.path().join("perfr"), None);
        let fake = Fake { me: Some((200, ME.into())), share: Some((200, STORED.into())), ..Default::default() };
        let mut err = Vec::new();
        login(&LoginOpts::default(), &env, &fake, &mut &b"  perfr_abc123\nsecond line\n"[..], true, &mut err).unwrap();
        let said = String::from_utf8(err).unwrap();
        assert!(said.contains("paste it here"), "{said}");
        assert!(said.contains("as Ada Lovelace"), "{said}");
        assert!(!said.contains("perfr_abc123"), "{said}");
        assert_eq!(header(&fake.headers.borrow()[0], "Authorization"), Some("Bearer perfr_abc123"));
        let path = dir.path().join("perfr").join(CREDENTIALS_FILE);
        let kept: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(kept["https://perfr.dev"], "perfr_abc123");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // The kept token is what a share sends, and the environment's goes first.
        let file = dir.path().join("a.perfr.stream");
        fs::write(&file, stream_bytes(0)).unwrap();
        let opts = ShareOpts { file: Some(file.display().to_string()), ..Default::default() };
        run(&opts, &env, &fake).0.unwrap();
        assert_eq!(header(fake.headers.borrow().last().unwrap(), "Authorization"), Some("Bearer perfr_abc123"));
        let mut from_env = env_in(&dir.path().join("perfr"), Some("perfr_env"));
        run(&opts, &from_env, &fake).0.unwrap();
        assert_eq!(header(fake.headers.borrow().last().unwrap(), "Authorization"), Some("Bearer perfr_env"));

        // A token for another site is kept beside it, and not sent to this one.
        from_env.token = None;
        let local = LoginOpts { site: Some("http://127.0.0.1:8081".into()), status: false };
        login(&local, &from_env, &fake, &mut &b"perfr_local\n"[..], false, &mut Vec::new()).unwrap();
        assert_eq!(token_for("https://perfr.dev", &from_env).as_deref(), Some("perfr_abc123"));
        assert_eq!(token_for("http://127.0.0.1:8081", &from_env).as_deref(), Some("perfr_local"));

        let mut out = Vec::new();
        login_status(&LoginOpts::default(), &env, &fake, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "signed in to https://perfr.dev as Ada Lovelace (https://perfr.dev/ada/)\n");

        let mut err = Vec::new();
        logout(&LoginOpts::default(), &env, &mut err).unwrap();
        assert!(String::from_utf8(err).unwrap().contains("Revoke it"));
        let error = login_status(&LoginOpts::default(), &env, &fake, &mut Vec::new()).unwrap_err();
        assert!(error.contains("not signed in"), "{error}");
        assert_eq!(token_for("https://perfr.dev", &env), None);
        assert_eq!(token_for("http://127.0.0.1:8081", &env).as_deref(), Some("perfr_local"));
        let mut err = Vec::new();
        logout(&LoginOpts::default(), &env, &mut err).unwrap();
        assert!(String::from_utf8(err).unwrap().contains("no token"));
    }

    #[test]
    fn login_keeps_nothing_the_website_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_in(dir.path(), None);
        let refused = Fake { me: Some((401, "{}".into())), ..Default::default() };
        let error = login(&LoginOpts::default(), &env, &refused, &mut &b"perfr_old\n"[..], false, &mut Vec::new()).unwrap_err();
        assert!(error.contains("does not accept this token"), "{error}");
        assert!(!dir.path().join(CREDENTIALS_FILE).exists());
        for empty in [&b""[..], b"\n", b"two words\n"] {
            let fake = Fake::default();
            let error = login(&LoginOpts::default(), &env, &fake, &mut &empty[..], false, &mut Vec::new()).unwrap_err();
            assert!(error.contains("no token given"), "{error}");
            assert!(fake.seen.borrow().is_empty());
        }
    }

    #[test]
    fn a_token_is_only_sent_over_https_or_to_this_machine() {
        for good in ["https://perfr.dev", "https://perfr.dev/", "http://127.0.0.1:8081", "http://localhost:8081", "http://[::1]:8081"] {
            assert_eq!(clean_site(good).unwrap(), good.trim_end_matches('/'), "{good}");
        }
        for bad in ["http://perfr.dev", "perfr.dev", "ftp://perfr.dev", "https://", "https://perfr.dev/path", "https://user@evil.example", "http://127.0.0.1.evil.example:80", "https://perfr.dev?x"] {
            assert!(clean_site(bad).is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let opts = ShareOpts { site: Some("http://example.com".into()), ..Default::default() };
        assert!(run(&opts, &env_in(dir.path(), Some("t")), &fake).0.is_err());
        assert!(fake.seen.borrow().is_empty());
    }

    #[test]
    fn a_files_name_becomes_one_the_website_takes() {
        for (file, name) in [
            ("spike.perfr.stream", Some("spike")),
            ("runs/frame spike.perfr.zip", Some("frame-spike")),
            ("/tmp/bevy_city-2026-11-18.perfr.zip", Some("bevy_city-2026-11-18")),
            ("capture.stream", Some("capture")),
            ("notes", Some("notes")),
            ("--weird  (1).perfr.zip", Some("weird-1")),
            ("....perfr.zip", None),
            ("é.perfr.zip", None),
        ] {
            assert_eq!(name_from_file(file).as_deref(), name, "{file}");
        }
        let long = name_from_file(&format!("{}.perfr.zip", "a".repeat(200))).unwrap();
        assert_eq!(long.len(), SEGMENT_MAX);
    }

    #[test]
    fn curl_is_given_the_token_on_its_input_and_never_in_its_arguments() {
        let url = "https://perfr.dev/api/share";
        let mut request = Request::new("POST", url);
        request.headers.push(bearer("perfr_secret"));
        request.headers.push(("X-Perfr-Name".into(), "a \"quoted\\ name\r\nX-Injected: 1".into()));
        let body = PathBuf::from("/tmp/a.perfr.stream");
        request.body_file = Some(&body);
        let args = curl_args(&request);
        assert!(!args.iter().any(|arg| arg.contains("perfr_secret") || arg.contains("Authorization")), "{args:?}");
        assert!(args.windows(2).any(|pair| pair == ["-K", "-"]), "{args:?}");
        assert!(args.windows(2).any(|pair| pair == ["--data-binary", "@/tmp/a.perfr.stream"]), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some(url));
        let config = curl_config(&request);
        assert_eq!(
            config,
            "header = \"Authorization: Bearer perfr_secret\"\nheader = \"X-Perfr-Name: a \\\"quoted\\\\ nameX-Injected: 1\"\n"
        );

        let response = split_status(b"{\"a\": 1}\nmore\n200".to_vec()).unwrap();
        assert_eq!((response.status, response.body.as_slice()), (200, &b"{\"a\": 1}\nmore"[..]));
        assert_eq!(split_status(b"\n404".to_vec()).unwrap().status, 404);
        assert!(split_status(b"no status".to_vec()).is_err());
    }
}
