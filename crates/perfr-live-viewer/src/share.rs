// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Share a capture with the website.
//!
//! The viewer posts the current capture's `.perfr.stream` bytes to
//! `{base}/api/share` as `application/octet-stream`. The server answers
//! JSON `{id, url}`. `base` is `https://perfr.dev`, unless `PERFR_SHARE_BASE`
//! was set when the viewer was built, or the page was opened with
//! `?share_base=http://127.0.0.1:port` (loopback only, so a link cannot
//! point the upload at some other host).

use serde::Deserialize;

/// `POST` target for a live capture: the service's stream export.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub const STREAM_EXPORT_PATH: &str = "/api/capture/export?format=stream";

/// Body type of `POST /api/share`.
pub const SHARE_CONTENT_TYPE: &str = "application/octet-stream";

/// What `POST /api/share` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareLink {
    pub id: String,
    pub url: String,
}

/// One update from the upload, tagged with the attempt that started it so a
/// Clear or a newer click can ignore a reply that arrives late.
#[derive(Clone, Debug)]
pub struct ShareNote {
    pub gen: u64,
    pub kind: ShareNoteKind,
}

#[derive(Clone, Debug)]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub enum ShareNoteKind {
    Reading,
    Progress { loaded: u64, total: u64 },
    Done(ShareLink),
    Failed(String),
}

/// `https://perfr.dev`, or `PERFR_SHARE_BASE` from the build environment
/// when that value is an http(s) URL.
pub fn default_share_base() -> &'static str {
    match option_env!("PERFR_SHARE_BASE") {
        Some(base) if is_http_base(base) => base,
        _ => "https://perfr.dev",
    }
}

/// Query override, then the build default. The query is accepted only for a
/// loopback host.
pub fn resolve_share_base(search: &str) -> String {
    if let Some(base) = query_share_base(search) {
        return base;
    }
    default_share_base()
        .trim()
        .trim_end_matches('/')
        .to_string()
}

/// `?share_base=`, percent-decoded, when it is an http(s) URL on localhost,
/// `127.0.0.1`, or `::1`. Anything else is ignored.
pub fn query_share_base(search: &str) -> Option<String> {
    let value = search
        .trim_start_matches('?')
        .split('&')
        .find_map(|kv| kv.strip_prefix("share_base="))?;
    if value.is_empty() {
        return None;
    }
    loopback_http_base(crate::dev::percent_decode(value).trim())
}

/// `{base}/api/share`, with a single slash between them.
pub fn share_post_url(base: &str) -> String {
    format!("{}/api/share", base.trim().trim_end_matches('/'))
}

/// A dropped or opened file that is already a capture stream.
pub fn is_stream_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".perfr.stream") || name.ends_with(".orbit.stream")
}

/// The words for the status codes the share server uses. Other failures stay
/// a short line; an HTML body is not shown.
pub fn share_failure_message(status: u16, body: &str) -> String {
    match status {
        0 => "Could not reach the share server.".into(),
        413 => "This capture is too large to share.".into(),
        429 => "Too many shares. Try again later.".into(),
        503 => "Sharing is not configured.".into(),
        _ => {
            let body = body.trim();
            if plain_error_body(body) {
                format!("Could not share this capture ({status}): {body}")
            } else {
                format!("Could not share this capture ({status}).")
            }
        }
    }
}

fn plain_error_body(body: &str) -> bool {
    !body.is_empty()
        && body.len() <= 160
        && !body.contains('<')
        && body
            .chars()
            .all(|c| c == '\n' || (c.is_ascii() && !c.is_control()))
}

/// `{id, url}` from a 2xx body.
pub fn parse_share_reply(text: &str) -> Result<ShareLink, String> {
    #[derive(Deserialize)]
    struct Reply {
        id: String,
        url: String,
    }
    let reply: Reply = serde_json::from_str(text)
        .map_err(|_| "The share server sent a response that is not a link.".to_string())?;
    if reply.id.is_empty() || reply.url.is_empty() {
        return Err("The share server sent a response that is not a link.".into());
    }
    Ok(ShareLink {
        id: reply.id,
        url: reply.url,
    })
}

fn is_http_base(base: &str) -> bool {
    let rest = base.trim();
    (rest.starts_with("https://") || rest.starts_with("http://"))
        && rest.len() > "https://".len()
        && !rest.contains(char::is_whitespace)
        && !rest.contains('\\')
}

/// `http://127.0.0.1:9` or `http://127.0.0.1:9/prefix`, trailing slash removed.
fn loopback_http_base(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() || raw.contains(char::is_whitespace) || raw.contains('\\') {
        return None;
    }
    let rest = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))?;
    if rest.is_empty() || rest.starts_with('/') {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        if !(after.is_empty() || after.starts_with(':')) {
            return None;
        }
        host
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1" {
        Some(raw.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn share_base_is_the_website_unless_a_loopback_query_overrides_it() {
        assert_eq!(resolve_share_base(""), "https://perfr.dev");
        assert_eq!(resolve_share_base("?theme=dark"), "https://perfr.dev");
        assert_eq!(
            resolve_share_base("?share_base=https://evil.example"),
            "https://perfr.dev"
        );
        assert_eq!(
            resolve_share_base("?share_base=http://10.0.0.5:9"),
            "https://perfr.dev"
        );
        assert_eq!(
            resolve_share_base("?share_base=http%3A%2F%2F127.0.0.1%3A9"),
            "http://127.0.0.1:9"
        );
        assert_eq!(
            resolve_share_base("?capture=/api/share/abc&share_base=http://localhost:44766/"),
            "http://localhost:44766"
        );
        assert_eq!(
            resolve_share_base("?share_base=http://[::1]:9/prefix"),
            "http://[::1]:9/prefix"
        );
        assert_eq!(
            share_post_url("http://127.0.0.1:9"),
            "http://127.0.0.1:9/api/share"
        );
        assert_eq!(
            share_post_url("http://127.0.0.1:9/"),
            "http://127.0.0.1:9/api/share"
        );
        assert_eq!(
            share_post_url("http://127.0.0.1:9/prefix"),
            "http://127.0.0.1:9/prefix/api/share"
        );
    }

    #[test]
    fn a_stream_file_is_named_by_its_suffix() {
        assert!(is_stream_file("capture.perfr.stream"));
        assert!(is_stream_file("Old.ORBIT.stream"));
        assert!(!is_stream_file("capture.perfr.zip"));
        assert!(!is_stream_file("notes.stream"));
    }

    #[test]
    fn the_reply_is_an_id_and_a_url() {
        let link = parse_share_reply(r#"{"id":"abc","url":"https://perfr.dev/c/abc"}"#).unwrap();
        assert_eq!(link.id, "abc");
        assert_eq!(link.url, "https://perfr.dev/c/abc");
        assert!(parse_share_reply(r#"{"id":"","url":"https://perfr.dev/c/x"}"#).is_err());
        assert!(parse_share_reply(r#"{"id":"abc"}"#).is_err());
        assert!(parse_share_reply("not json").is_err());
    }

    #[test]
    fn status_codes_use_plain_words() {
        assert_eq!(
            share_failure_message(413, "<html>big</html>"),
            "This capture is too large to share."
        );
        assert_eq!(
            share_failure_message(429, ""),
            "Too many shares. Try again later."
        );
        assert_eq!(
            share_failure_message(503, "missing bucket"),
            "Sharing is not configured."
        );
        assert_eq!(
            share_failure_message(0, ""),
            "Could not reach the share server."
        );
        assert_eq!(
            share_failure_message(500, "disk full"),
            "Could not share this capture (500): disk full"
        );
        assert_eq!(
            share_failure_message(502, "<html>bad gateway</html>"),
            "Could not share this capture (502)."
        );
    }

    struct Hit {
        start: String,
        content_type: String,
        body: Vec<u8>,
    }

    /// One request, then the bytes the mock stored for `GET /api/share/<id>`.
    fn serve(status: u16, response_body: &[u8], content_type: &str) -> (String, Arc<Mutex<Hit>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hit = Arc::new(Mutex::new(Hit {
            start: String::new(),
            content_type: String::new(),
            body: Vec::new(),
        }));
        let stored = hit.clone();
        let response_body = response_body.to_vec();
        let content_type = content_type.to_string();
        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let (head, body) = loop {
                let n = sock.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len = content_length(&head);
                if buf.len() < end + 4 + len {
                    continue;
                }
                let body = buf[end + 4..end + 4 + len].to_vec();
                break (head, body);
            };
            {
                let mut g = stored.lock().unwrap();
                g.start = head.lines().next().unwrap_or("").to_string();
                g.content_type = header_value(&head, "content-type").unwrap_or_default();
                g.body = body;
            }
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            sock.write_all(resp.as_bytes()).ok();
            sock.write_all(&response_body).ok();
        });
        (format!("http://127.0.0.1:{port}"), hit)
    }

    fn content_length(head: &str) -> usize {
        header_value(head, "content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        head.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }

    fn post(url: &str, bytes: &[u8]) -> (u16, String) {
        let rest = url.strip_prefix("http://").expect("http");
        let (hostport, path) = rest.split_once('/').expect("path");
        let mut stream = std::net::TcpStream::connect(hostport).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
        let head = format!(
            "POST /{path} HTTP/1.1\r\nHost: {hostport}\r\nContent-Type: {SHARE_CONTENT_TYPE}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(bytes).unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).unwrap();
        let end = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = std::str::from_utf8(&resp[..end]).unwrap();
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let text = String::from_utf8_lossy(&resp[end + 4..]).into_owned();
        (status, text)
    }

    #[test]
    fn a_post_of_stream_bytes_reads_the_link_back() {
        let body = br#"{"id":"abc","url":"http://127.0.0.1:9/c/abc"}"#;
        let (base, hit) = serve(200, body, "application/json");
        let stream = b"perfr-stream-bytes";
        let (status, text) = post(&share_post_url(&base), stream);
        assert_eq!(status, 200);
        let link = parse_share_reply(&text).unwrap();
        assert_eq!(link.id, "abc");
        assert_eq!(link.url, "http://127.0.0.1:9/c/abc");
        let seen = hit.lock().unwrap();
        assert_eq!(seen.start, "POST /api/share HTTP/1.1");
        assert_eq!(seen.content_type, SHARE_CONTENT_TYPE);
        assert_eq!(seen.body, stream);
        // The page the link opens loads the viewer with this capture path.
        assert_eq!(
            crate::dev::query_capture_url("?capture=/api/share/abc").as_deref(),
            Some("/api/share/abc")
        );
    }

    #[test]
    fn the_mock_answers_too_large_rate_limited_and_not_configured() {
        for (status, words) in [
            (413, "This capture is too large to share."),
            (429, "Too many shares. Try again later."),
            (503, "Sharing is not configured."),
        ] {
            let (base, _) = serve(status, b"no", "text/plain");
            let (got, text) = post(&share_post_url(&base), b"bytes");
            assert_eq!(got, status);
            assert_eq!(share_failure_message(got, &text), words);
        }
    }
}
