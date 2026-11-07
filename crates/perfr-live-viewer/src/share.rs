// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Share a capture with the website.
//!
//! The viewer posts `.perfr.stream` bytes to `{base}/api/share` as
//! `application/octet-stream`. The server answers JSON `{id, url}`.
//! `base` is `https://perfr.dev`, unless `PERFR_SHARE_BASE` was set when
//! the viewer was built, or the page was opened with
//! `?share_base=http://127.0.0.1:port` (loopback only, so a link cannot
//! point the upload at some other host).
//!
//! Share asks before posting. The dialog shows the size of the bytes
//! that will be uploaded. A selected time range can be posted instead:
//! the stream is cut to the events that overlap that window, the same
//! rule the service uses for `capture_stream`. The cut is the whole
//! window, on every thread.

use perfr_live_protocol::{
    decode_frame, encode_event_batch_with, LiveFrame, WireFormat, FRAME_EVENT_BATCH,
    FRAME_EVENT_BATCH_DEFLATE, FRAME_EVENT_BATCH_PACKED,
};
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
    /// The bytes are in hand. Nothing has been posted yet.
    Prepared(Vec<u8>),
    Progress {
        loaded: u64,
        total: u64,
    },
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

/// Decimal size of the bytes that will be posted: `12.4 MB` is 12.4 million
/// bytes. Under 1000 bytes the unit is `B` with no decimal.
pub fn format_share_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let n = bytes as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

/// What the confirm dialog says about sharing a time range.
///
/// `can_slice` is false when the bytes could not be cut, so the sentence
/// must not tell the reader to pick a range. `has_range` is a committed
/// selection whose cut is ready to post.
pub fn share_section_hint(can_slice: bool, has_range: bool) -> &'static str {
    if !can_slice {
        "This capture cannot be cut to a time range, so only the whole capture can be shared."
    } else if has_range {
        "Share selected range uploads the events in that time range, on every thread, instead of the whole capture."
    } else {
        "To share a section instead, right-drag a range on the timeline, or drag across a thread's sample bar, then use Share selected range."
    }
}

/// The events that overlap `[start_ns, end_ns]`, as a `.perfr.stream`.
///
/// Reversed bounds are swapped. An event overlaps when
/// `start_ns <= end && end_ns() >= start`, which is the service's rule:
/// scopes that cross an edge stay whole, and a sample is the instant
/// `end_ns()` reports (its `duration_ns` is the sampling period, not a
/// span). Name, hello, and status frames are copied unchanged. Event
/// batches are rewritten in the format they arrived in, and a batch that
/// loses every event is dropped. The returned length is the number of
/// bytes a share of this range would post.
pub fn slice_stream(bytes: &[u8], start_ns: u64, end_ns: u64) -> Result<Vec<u8>, String> {
    let (t0, t1) = (start_ns.min(end_ns), start_ns.max(end_ns));
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        if rest.len() < 5 {
            return Err("Could not read this capture.".into());
        }
        let payload_len = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        let total = 5usize.saturating_add(payload_len);
        if total < 5 || rest.len() < total {
            return Err("Could not read this capture.".into());
        }
        let wire = match rest[4] {
            FRAME_EVENT_BATCH => Some(WireFormat::Raw),
            FRAME_EVENT_BATCH_PACKED => Some(WireFormat::Packed),
            FRAME_EVENT_BATCH_DEFLATE => Some(WireFormat::Deflate),
            _ => None,
        };
        if let Some(wire) = wire {
            let (frame, n) =
                decode_frame(rest).map_err(|e| format!("Could not read this capture. {e}"))?;
            if n != total {
                return Err("Could not read this capture.".into());
            }
            let LiveFrame::EventBatch { mut events } = frame else {
                return Err("Could not read this capture.".into());
            };
            events.retain(|event| event.start_ns <= t1 && event.end_ns() >= t0);
            if !events.is_empty() {
                out.extend_from_slice(&encode_event_batch_with(&events, wire));
            }
        } else {
            out.extend_from_slice(&rest[..total]);
        }
        rest = &rest[total..];
    }
    Ok(out)
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

    #[test]
    fn share_sizes_are_decimal_units() {
        assert_eq!(format_share_bytes(0), "0 B");
        assert_eq!(format_share_bytes(999), "999 B");
        assert_eq!(format_share_bytes(1_000), "1.0 KB");
        assert_eq!(format_share_bytes(12_400_000), "12.4 MB");
        assert_eq!(format_share_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn the_section_hint_only_offers_a_cut_the_viewer_can_make() {
        let none = share_section_hint(true, false);
        assert!(none.contains("right-drag"));
        assert!(none.contains("sample bar"));
        assert!(none.contains("Share selected range"));
        let ready = share_section_hint(true, true);
        assert!(ready.contains("every thread"));
        assert!(ready.contains("time range"));
        assert!(!ready.contains("right-drag"));
        let blocked = share_section_hint(false, true);
        assert!(blocked.contains("only the whole capture"));
        assert!(!blocked.contains("right-drag"));
        assert!(!blocked.contains("Share selected range"));
    }

    fn event(start: u64, duration: u64, kind_id: u8) -> perfr_live_event::LiveEvent {
        perfr_live_event::LiveEvent {
            start_ns: start,
            duration_ns: duration,
            tid: 7,
            pid: 8,
            kind: kind_id,
            depth: 0,
            extra: 0,
            _pad: 0,
            name_id: 1,
        }
    }

    fn framed(events: &[perfr_live_event::LiveEvent], wire: WireFormat) -> Vec<u8> {
        use perfr_live_event::LIVE_EVENT_SIZE;
        use perfr_live_protocol::{encode_frame, VERSION};
        let mut out = encode_frame(&LiveFrame::Hello {
            version: VERSION,
            event_size: LIVE_EVENT_SIZE as u16,
        });
        out.extend_from_slice(&encode_frame(&LiveFrame::ProcessName {
            pid: 8,
            name: "demo".into(),
        }));
        out.extend_from_slice(&encode_event_batch_with(events, wire));
        out.extend_from_slice(&encode_frame(&LiveFrame::CaptureFinished));
        out
    }

    fn events_of(bytes: &[u8]) -> Vec<perfr_live_event::LiveEvent> {
        perfr_live_protocol::decode_all(bytes)
            .unwrap()
            .into_iter()
            .filter_map(|frame| match frame {
                LiveFrame::EventBatch { events } => Some(events),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn a_time_range_keeps_overlapping_events_and_the_bytes_are_the_upload() {
        use perfr_live_event::kind;
        let crossing = event(0, 200, kind::API_SCOPE);
        let inside = event(160, 10, kind::API_SCOPE);
        let outside = event(10, 5, kind::API_SCOPE);
        let sample = event(160, 1_000_000, kind::SAMPLE);
        let late_sample = event(190, 1_000_000, kind::SAMPLE);
        let kept = [crossing, inside, sample];
        for wire in [WireFormat::Raw, WireFormat::Packed, WireFormat::Deflate] {
            let full = framed(&[crossing, inside, outside, sample, late_sample], wire);
            let same = slice_stream(&full, 0, u64::MAX).unwrap();
            assert_eq!(
                same, full,
                "{wire:?} covering every event must not change the bytes"
            );
            let sliced = slice_stream(&full, 180, 150).unwrap();
            assert_eq!(events_of(&sliced), kept, "{wire:?}");
            assert!(
                sliced.len() < full.len(),
                "{wire:?} slice is {} bytes, whole capture is {}",
                sliced.len(),
                full.len()
            );
            let process = perfr_live_protocol::encode_frame(&LiveFrame::ProcessName {
                pid: 8,
                name: "demo".into(),
            });
            assert!(sliced.windows(process.len()).any(|w| w == process));
        }
        let broken = slice_stream(&[1, 2, 3, 4], 0, 1);
        assert!(broken.is_err());
    }
}
