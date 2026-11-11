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
//! Sharing requires an account. Before the upload, the viewer calls
//! `GET {base}/api/me` (`credentials: include`, and `Authorization:
//! Bearer` when a login return handed back a token). `200` is the
//! account and quota; `401` is `{error: "login_required", login_url}`.
//! The confirm dialog then either says who the capture will be shared
//! as, or "Sign in to share" with GitHub and Google. Paths, the JSON
//! shape, and the CORS headers the website must send live in this
//! module so they can move with the perfr-web accounts PR. The local
//! viewer (served by the native service, not perfr.dev) is cross-origin:
//! see `SHARE_AUTH.md`.
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

/// `GET {base}/api/me`. Change this if the website accounts PR moves it.
pub const ME_PATH: &str = "/api/me";

/// `POST {base}/api/share`.
pub const SHARE_PATH: &str = "/api/share";

/// Fallback login path when a 401 does not name one on this share host.
pub const LOGIN_PATH: &str = "/login";

/// `credentials` mode on `GET /api/me` and `POST /api/share`.
pub const SHARE_CREDENTIALS: &str = "include";

/// `window.name` of the login popup. `index.html` sees this when the
/// popup is sent back to the viewer and signals the opener before wasm boots.
pub const LOGIN_WINDOW_NAME: &str = "perfr-share-login";

/// `sessionStorage` key set before a same-tab login navigation, so Share
/// reopens when the browser comes back.
pub const LOGIN_RESUME_KEY: &str = "perfr.share.resume";

/// `localStorage` key the login popup writes on return. A storage event
/// reaches the opener even when COOP has severed `window.opener`.
pub const LOGIN_SIGNAL_KEY: &str = "perfr.share.logged_in";

/// `localStorage` / `sessionStorage` key for the optional bearer.
pub const SHARE_TOKEN_KEY: &str = "perfr.share.token";

/// URL fragment the login redirect may append: `{next}#share_token=<token>`.
/// A fragment is not sent to the local perfr service.
pub const SHARE_TOKEN_FRAGMENT: &str = "share_token";

/// `postMessage` data the popup (or the website) may send the opener.
pub const LOGIN_MESSAGE_SOURCE: &str = "perfr";
pub const LOGIN_MESSAGE_TYPE: &str = "login";

/// The words on the signed-out dialog.
pub const SIGN_IN_TITLE: &str = "Sign in to share";

/// Shown when a cross-origin login returned and `/api/me` is still 401.
pub const COOKIE_BLOCKED_NOTICE: &str = "Signed in in the other window, but this page still has no perfr.dev session. The browser may be blocking the perfr.dev cookie. Allow cookies for perfr.dev, or share from the perfr.dev viewer.";

/// Providers the signed-out dialog offers, in button order.
pub const LOGIN_PROVIDERS: &[LoginProvider] = &[
    LoginProvider {
        id: "github",
        label: "GitHub",
    },
    LoginProvider {
        id: "google",
        label: "Google",
    },
];

/// One OAuth provider button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoginProvider {
    pub id: &'static str,
    pub label: &'static str,
}

/// CORS response headers `GET /api/me`, `POST /api/share`, and their
/// preflight must send when the viewer is not on the share host.
/// `Access-Control-Allow-Origin` is the request's Origin, never `*`.
/// Read by the contract test and `SHARE_AUTH.md`; the browser applies
/// whatever the website actually sends.
#[cfg_attr(not(test), allow(dead_code))]
pub const SHARE_CORS: &[(&str, &str)] = &[
    (
        "Access-Control-Allow-Origin",
        "<the request Origin, never *>",
    ),
    ("Access-Control-Allow-Credentials", "true"),
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    (
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization",
    ),
    ("Access-Control-Max-Age", "600"),
    ("Vary", "Origin"),
];

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

/// Why an upload or a session check stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareFailure {
    pub message: String,
    /// The session is missing or expired. The dialog goes back to sign-in.
    pub needs_login: bool,
}

impl ShareFailure {
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            message: text.into(),
            needs_login: false,
        }
    }

    pub fn login(text: impl Into<String>) -> Self {
        Self {
            message: text.into(),
            needs_login: true,
        }
    }
}

/// The account `GET /api/me` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareUser {
    pub id: String,
    pub name: String,
    pub avatar: String,
}

/// Bytes already shared, the account cap, and the largest single capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareQuota {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub max_capture_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareAccount {
    pub user: ShareUser,
    pub quota: ShareQuota,
}

/// What `GET /api/me` meant. `status` 0 is a network failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCheck {
    SignedIn(ShareAccount),
    LoginRequired { login_url: String },
    Unavailable { message: String },
}

/// What the confirm dialog is showing. `Checking` covers the first
/// `/api/me` and a quiet re-check does not use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareSession {
    Unknown,
    Checking,
    SignedOut { login_url: String },
    SignedIn(ShareAccount),
    Unavailable(String),
}

impl ShareSession {
    pub fn from_check(check: SessionCheck) -> Self {
        match check {
            SessionCheck::SignedIn(account) => Self::SignedIn(account),
            SessionCheck::LoginRequired { login_url } => Self::SignedOut { login_url },
            SessionCheck::Unavailable { message } => Self::Unavailable(message),
        }
    }
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
    Failed(ShareFailure),
    /// `GET /api/me` answered. An upload already in flight is not cancelled
    /// by this; a refusal of that check arrives as [`Failed`] too.
    Session(SessionCheck),
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
    join_base(base, SHARE_PATH)
}

/// `{base}/api/me`.
pub fn me_url(base: &str) -> String {
    join_base(base, ME_PATH)
}

fn join_base(base: &str, path: &str) -> String {
    format!("{}{}", base.trim().trim_end_matches('/'), path)
}

/// A dropped or opened file that is already a capture stream.
pub fn is_stream_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".perfr.stream") || name.ends_with(".orbit.stream")
}

/// The `.perfr.stream` the page holds for what is on screen, which is what
/// Share posts without asking a service, and the rule for offering Share at
/// all. Kept apart from the app so the sequences that replace a capture can
/// be tested: Share must never post bytes other than the capture shown.
#[derive(Default)]
pub struct HeldStream {
    /// Bytes of a `.perfr.stream` loaded from a file or URL. Share posts
    /// these. A live session leaves this empty and exports from the service.
    bytes: Option<Vec<u8>>,
    /// Set while those bytes are being ingested, so a frame inside the file
    /// does not throw them away.
    loading: bool,
    /// The open capture is that file. Later socket frames are not mixed in.
    local: bool,
}

impl HeldStream {
    /// A stream file is about to be ingested.
    pub fn begin_load(&mut self) {
        self.loading = true;
    }

    /// The file is ingested: it is the capture on screen now.
    pub fn loaded(&mut self, bytes: Vec<u8>) {
        self.loading = false;
        self.bytes = Some(bytes);
        self.local = true;
    }

    /// The capture on screen is being replaced or cleared.
    pub fn forget(&mut self) {
        if !self.loading {
            self.bytes = None;
            self.local = false;
        }
    }

    /// A Chrome or Perfetto trace is being imported over whatever was shown.
    /// The file that was on screen is not any more: left held, its bytes are
    /// what Upload would post under a timeline showing the trace.
    pub fn trace_opened(&mut self) {
        self.loading = false;
        self.bytes = None;
        self.local = false;
    }

    pub fn bytes(&self) -> Option<&Vec<u8>> {
        self.bytes.as_ref()
    }

    pub fn has_bytes(&self) -> bool {
        self.bytes.as_ref().is_some_and(|b| !b.is_empty())
    }

    pub fn is_local(&self) -> bool {
        self.local
    }

    /// Whether Share is offered. `static_page` is a page with no service
    /// (`?capture=`), `imported_trace` a Chrome or Perfetto trace on screen,
    /// `events` what the index holds.
    pub fn can_share(&self, static_page: bool, imported_trace: bool, events: usize) -> bool {
        // An imported trace exists only in the index, and nothing writes the
        // index back out as a stream. The one other source is the service's
        // export, which is the service's own capture, not the trace shown.
        if imported_trace {
            return false;
        }
        if self.has_bytes() {
            return true;
        }
        !static_page && !self.local && events > 0
    }
}

/// The words for a failed upload, and whether the dialog should go back
/// to sign-in. 401 is a dead session. 403 is permission. 413 is the
/// capture itself. 429 is the account's rate. Other failures stay a short
/// line; an HTML body is not shown.
pub fn share_failure(status: u16, body: &str) -> ShareFailure {
    let message = match status {
        0 => "Could not reach the share server.".into(),
        401 => "Your session expired. Sign in again to share.".into(),
        403 => "You don't have permission to share this capture.".into(),
        413 => "This capture is too large to share. Share a selected range to upload less.".into(),
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
    };
    ShareFailure {
        message,
        needs_login: status == 401,
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

/// `200` is an account. `401` is signed out, even when the body is not the
/// expected JSON (a session cookie that the browser dropped looks the same).
/// Anything else is a failure the dialog can show.
pub fn parse_session_response(status: u16, body: &str) -> SessionCheck {
    match status {
        200 => parse_account(body)
            .map(SessionCheck::SignedIn)
            .unwrap_or_else(|message| SessionCheck::Unavailable { message }),
        401 => {
            let login_url = serde_json::from_str::<LoginRequiredBody>(body)
                .ok()
                .map(|b| b.login_url)
                .unwrap_or_default();
            SessionCheck::LoginRequired { login_url }
        }
        0 => SessionCheck::Unavailable {
            message: "Could not reach the share server.".into(),
        },
        _ => {
            let body = body.trim();
            let message = if plain_error_body(body) {
                format!("Could not check the account ({status}): {body}")
            } else {
                format!("Could not check the account ({status}).")
            };
            SessionCheck::Unavailable { message }
        }
    }
}

#[derive(Deserialize)]
struct LoginRequiredBody {
    #[serde(default)]
    #[allow(dead_code)]
    error: String,
    #[serde(default)]
    login_url: String,
}

fn parse_account(body: &str) -> Result<ShareAccount, String> {
    #[derive(Deserialize)]
    struct Body {
        user: UserBody,
        quota: QuotaBody,
    }
    #[derive(Deserialize)]
    struct UserBody {
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        avatar: Option<String>,
    }
    #[derive(Deserialize)]
    struct QuotaBody {
        used_bytes: u64,
        limit_bytes: u64,
        max_capture_bytes: u64,
    }
    let body: Body = serde_json::from_str(body)
        .map_err(|_| "The share server sent an account that could not be read.".to_string())?;
    if body.user.id.is_empty() {
        return Err("The share server sent an account that could not be read.".into());
    }
    Ok(ShareAccount {
        user: ShareUser {
            id: body.user.id,
            name: body.user.name,
            avatar: body.user.avatar.unwrap_or_default(),
        },
        quota: ShareQuota {
            used_bytes: body.quota.used_bytes,
            limit_bytes: body.quota.limit_bytes,
            max_capture_bytes: body.quota.max_capture_bytes,
        },
    })
}

/// `https://perfr.dev/login?provider=github&next=...`.
///
/// An absolute `server_login_url` is used only when its origin is the share
/// host, so a 401 cannot send the browser somewhere else. A path (`/login`)
/// is joined onto `share_base`. Anything else falls back to [`LOGIN_PATH`].
pub fn login_url(share_base: &str, server_login_url: &str, provider: &str, next: &str) -> String {
    let endpoint = login_endpoint(share_base, server_login_url);
    format!(
        "{endpoint}?provider={}&next={}",
        percent_encode(provider),
        percent_encode(next)
    )
}

fn login_endpoint(share_base: &str, server_login_url: &str) -> String {
    let base = share_base.trim().trim_end_matches('/');
    let without_query = server_login_url
        .trim()
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if let Some(absolute) = absolute_login_endpoint(without_query, base) {
        return absolute;
    }
    if let Some(path) = without_query.strip_prefix('/') {
        if !path.is_empty() && !path.contains("..") && !path.contains('\\') && !path.contains("://")
        {
            return format!("{base}/{path}");
        }
    }
    format!("{base}{LOGIN_PATH}")
}

fn absolute_login_endpoint(url: &str, share_base: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    if url.contains(char::is_whitespace) || url.contains('\\') {
        return None;
    }
    let url_origin = origin_of(url)?;
    let base_origin = origin_of(share_base)?;
    if url_origin != base_origin {
        return None;
    }
    Some(url.to_string())
}

/// The viewer URL to send as `next`, without a fragment so a previous
/// token is not nested into the next login.
pub fn login_next_url(href: &str) -> String {
    href.split('#').next().unwrap_or(href).to_string()
}

/// Scheme, host, and port. Host is lowercased. No path.
pub fn origin_of(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        return None;
    };
    if rest.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(char::is_whitespace)
        || authority.contains('\\')
    {
        return None;
    }
    Some(format!("{scheme}://{}", authority.to_ascii_lowercase()))
}

/// The page and the share host are different origins, so the session cookie
/// is third-party and the login popup cannot rely on `window.opener`
/// (the viewer sends `Cross-Origin-Opener-Policy: same-origin`).
pub fn share_is_cross_origin(page_href: &str, share_base: &str) -> bool {
    match (origin_of(page_href), origin_of(share_base)) {
        (Some(page), Some(share)) => page != share,
        _ => false,
    }
}

/// `#share_token=` from a login redirect, when it is safe to put in a header.
pub fn fragment_share_token(hash: &str) -> Option<String> {
    let hash = hash.trim().trim_start_matches('#');
    if hash.is_empty() {
        return None;
    }
    for kv in hash.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if crate::dev::percent_decode(k) == SHARE_TOKEN_FRAGMENT {
            return usable_share_token(&crate::dev::percent_decode(v));
        }
    }
    None
}

/// A bearer that can sit in `Authorization` without breaking the header.
pub fn usable_share_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() || token.len() > 512 {
        return None;
    }
    if token
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
    {
        Some(token.to_string())
    } else {
        None
    }
}

pub fn bearer_value(token: &str) -> String {
    format!("Bearer {token}")
}

/// `Sharing as Ada Lovelace`. An empty name falls back to the id.
pub fn sharing_as_line(user: &ShareUser) -> String {
    let name = user.name.trim();
    let name = if name.is_empty() {
        user.id.as_str()
    } else {
        name
    };
    format!("Sharing as {name}")
}

/// `12.0 MB of 1.0 GB used · 988.0 MB left · up to 50.0 MB per capture`.
pub fn quota_usage_line(quota: &ShareQuota) -> String {
    let remaining = quota.limit_bytes.saturating_sub(quota.used_bytes);
    format!(
        "{} of {} used · {} left · up to {} per capture",
        format_share_bytes(quota.used_bytes),
        format_share_bytes(quota.limit_bytes),
        format_share_bytes(remaining),
        format_share_bytes(quota.max_capture_bytes),
    )
}

/// 0 when the account has no quota, else `used / limit` clamped to 0..=1.
pub fn quota_fraction(quota: &ShareQuota) -> f32 {
    if quota.limit_bytes == 0 {
        return 1.0;
    }
    (quota.used_bytes as f64 / quota.limit_bytes as f64).clamp(0.0, 1.0) as f32
}

/// Why `size` cannot be uploaded, or `None` when it fits.
/// A size equal to the limit is allowed. The per-capture cap is checked
/// first; remaining quota is `limit_bytes - used_bytes`.
pub fn fit_block(size: u64, quota: &ShareQuota, what: &str) -> Option<String> {
    if size > quota.max_capture_bytes {
        return Some(format!(
            "{what} is {}. The largest capture you can share is {}. Share a selected range to upload less.",
            format_share_bytes(size),
            format_share_bytes(quota.max_capture_bytes),
        ));
    }
    let remaining = quota.limit_bytes.saturating_sub(quota.used_bytes);
    if size > remaining {
        return Some(format!(
            "{what} is {}. You have {} left of your {} share quota. Share a selected range to upload less.",
            format_share_bytes(size),
            format_share_bytes(remaining),
            format_share_bytes(quota.limit_bytes),
        ));
    }
    None
}

/// One line of the confirm dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareLine {
    pub text: String,
    pub role: ShareLineRole,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareLineRole {
    /// "Sign in to share", or "Sharing as …".
    Title,
    Body,
    Quota,
    /// The formatted size of the whole capture, not the whole sentence.
    Size,
    Hint,
    Error,
    Progress,
}

/// A button the confirm dialog draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShareAction {
    Provider {
        id: &'static str,
        label: &'static str,
    },
    Upload,
    Range {
        bytes: u64,
    },
    Retry,
    Cancel,
}

/// The confirm step, decided entirely from the session and the bytes.
/// The window paints this and does not invent a second set of rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareConfirmView {
    pub kind: ShareConfirmKind,
    pub lines: Vec<ShareLine>,
    pub actions: Vec<ShareAction>,
    /// Set when the whole-capture size is known, for the harness tag.
    pub whole_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareConfirmKind {
    Checking,
    SignedOut,
    SignedIn,
    Unavailable,
}

/// Inputs to [`confirm_view`]. `busy` is the re-check that runs after
/// Upload is clicked and before the bytes are posted.
pub struct ConfirmInput<'a> {
    pub session: &'a ShareSession,
    pub whole_bytes: Option<u64>,
    pub range_bytes: Option<u64>,
    pub can_slice: bool,
    pub has_range: bool,
    pub error: Option<&'a str>,
    pub login_waiting: bool,
    pub login_returned: bool,
    pub cross_origin: bool,
    pub busy: bool,
}

pub fn confirm_view(input: &ConfirmInput<'_>) -> ShareConfirmView {
    let mut view = match input.session {
        ShareSession::Unknown | ShareSession::Checking => ShareConfirmView {
            kind: ShareConfirmKind::Checking,
            lines: vec![progress("Checking your perfr.dev account…")],
            actions: vec![ShareAction::Cancel],
            whole_bytes: input.whole_bytes,
        },
        ShareSession::Unavailable(message) => ShareConfirmView {
            kind: ShareConfirmKind::Unavailable,
            lines: vec![ShareLine {
                text: message.clone(),
                role: ShareLineRole::Error,
            }],
            actions: vec![ShareAction::Retry, ShareAction::Cancel],
            whole_bytes: input.whole_bytes,
        },
        ShareSession::SignedOut { .. } => signed_out_view(input),
        ShareSession::SignedIn(account) => signed_in_view(input, account),
    };
    if input.busy && !matches!(view.kind, ShareConfirmKind::Checking) {
        view.lines
            .insert(0, progress("Checking your account before upload…"));
        view.actions
            .retain(|action| matches!(action, ShareAction::Cancel));
        if view.actions.is_empty() {
            view.actions.push(ShareAction::Cancel);
        }
    }
    view
}

fn signed_out_view(input: &ConfirmInput<'_>) -> ShareConfirmView {
    let mut lines = vec![
        ShareLine {
            text: SIGN_IN_TITLE.into(),
            role: ShareLineRole::Title,
        },
        ShareLine {
            text: "Sharing uses your perfr.dev account.".into(),
            role: ShareLineRole::Body,
        },
    ];
    if let Some(notice) = signed_out_notice(input) {
        let role = if input.login_waiting && input.error.unwrap_or("").is_empty() {
            ShareLineRole::Body
        } else {
            ShareLineRole::Error
        };
        lines.push(ShareLine { text: notice, role });
    }
    ShareConfirmView {
        kind: ShareConfirmKind::SignedOut,
        lines,
        actions: LOGIN_PROVIDERS
            .iter()
            .map(|p| ShareAction::Provider {
                id: p.id,
                label: p.label,
            })
            .chain(std::iter::once(ShareAction::Cancel))
            .collect(),
        whole_bytes: input.whole_bytes,
    }
}

fn signed_out_notice(input: &ConfirmInput<'_>) -> Option<String> {
    if let Some(error) = input.error.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(error.to_string());
    }
    if input.login_waiting {
        return Some(
            "Finish signing in in the window that opened. This dialog continues when the session is ready."
                .into(),
        );
    }
    if input.login_returned && input.cross_origin {
        return Some(COOKIE_BLOCKED_NOTICE.into());
    }
    if input.login_returned {
        return Some("Still signed out. Try again.".into());
    }
    None
}

fn signed_in_view(input: &ConfirmInput<'_>, account: &ShareAccount) -> ShareConfirmView {
    let mut lines = vec![
        ShareLine {
            text: sharing_as_line(&account.user),
            role: ShareLineRole::Title,
        },
        ShareLine {
            text: quota_usage_line(&account.quota),
            role: ShareLineRole::Quota,
        },
    ];
    let mut actions = Vec::new();
    match input.whole_bytes {
        Some(size) => {
            lines.push(ShareLine {
                text: format_share_bytes(size),
                role: ShareLineRole::Size,
            });
            match fit_block(size, &account.quota, "This capture") {
                Some(block) => lines.push(ShareLine {
                    text: block,
                    role: ShareLineRole::Error,
                }),
                None => actions.push(ShareAction::Upload),
            }
        }
        None => lines.push(progress("Reading the capture…")),
    }
    if let Some(error) = input.error.map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(ShareLine {
            text: error.to_string(),
            role: ShareLineRole::Error,
        });
    }
    let range_fits = input.has_range
        && input
            .range_bytes
            .is_some_and(|n| fit_block(n, &account.quota, "The selected range").is_none());
    if input.has_range {
        if let Some(n) = input.range_bytes {
            if fit_block(n, &account.quota, "The selected range").is_some() {
                lines.push(ShareLine {
                    text: format!(
                        "The selected range ({}) is too large to share.",
                        format_share_bytes(n)
                    ),
                    role: ShareLineRole::Error,
                });
            }
        }
    }
    lines.push(ShareLine {
        text: share_section_hint(input.can_slice, input.has_range && range_fits).into(),
        role: ShareLineRole::Hint,
    });
    if range_fits {
        actions.push(ShareAction::Range {
            bytes: input.range_bytes.unwrap_or(0),
        });
    }
    actions.push(ShareAction::Cancel);
    ShareConfirmView {
        kind: ShareConfirmKind::SignedIn,
        lines,
        actions,
        whole_bytes: input.whole_bytes,
    }
}

fn progress(text: &str) -> ShareLine {
    ShareLine {
        text: text.into(),
        role: ShareLineRole::Progress,
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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

    #[test]
    fn a_trace_opened_on_a_service_page_is_not_shareable_as_the_services_capture() {
        // Nothing held, a service behind the page, events on screen: a live
        // capture, which Share reads from the service.
        let mut held = HeldStream::default();
        assert!(held.can_share(false, false, 100));
        // The same page after a Chrome trace is dropped on it. The events on
        // screen are the trace's; the service still holds its own capture,
        // and that is what an export would post.
        held.trace_opened();
        assert!(!held.can_share(false, true, 100));
    }

    #[test]
    fn a_trace_opened_over_a_stream_file_does_not_leave_the_file_to_share() {
        let mut held = HeldStream::default();
        held.begin_load();
        held.forget();
        held.loaded(vec![1, 2, 3]);
        assert!(held.can_share(false, false, 100));
        assert!(held.is_local());
        held.trace_opened();
        assert!(held.bytes().is_none(), "the previous file's bytes are still what Share would post");
        assert!(!held.is_local());
        assert!(!held.can_share(false, true, 100));
        assert!(!held.can_share(true, true, 100));
    }

    #[test]
    fn share_is_offered_for_a_held_stream_and_for_a_live_capture_only() {
        let mut held = HeldStream::default();
        // A static page before its capture arrives, and an empty live page.
        assert!(!held.can_share(true, false, 0));
        assert!(!held.can_share(false, false, 0));
        // A frame inside the file being ingested (a `CaptureStarted`) clears
        // the file trace; it must not drop the bytes being loaded.
        held.begin_load();
        held.forget();
        held.loaded(vec![9; 16]);
        assert!(held.can_share(true, false, 0));
        // Clear, or a new capture: the file is gone, and with it the block
        // on the live path.
        held.forget();
        assert!(held.bytes().is_none() && !held.is_local());
        assert!(held.can_share(false, false, 1));
        // An empty file is nothing to post, and it is still not a live page.
        held.begin_load();
        held.loaded(Vec::new());
        assert!(!held.can_share(false, false, 1));
    }

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
        let too_large = share_failure(413, "<html>big</html>");
        assert_eq!(
            too_large.message,
            "This capture is too large to share. Share a selected range to upload less."
        );
        assert!(!too_large.needs_login);
        let expired = share_failure(401, r#"{"error":"login_required"}"#);
        assert!(expired.needs_login);
        assert!(expired.message.contains("Sign in again"));
        let forbidden = share_failure(403, "<html>no</html>");
        assert!(!forbidden.needs_login);
        assert!(forbidden.message.contains("permission"));
        assert!(!share_failure(429, "").needs_login);
        // Each status the share server uses, with whatever body came along.
        for (status, body, words) in [
            (401, "no", "Your session expired. Sign in again to share."),
            (403, "no", "You don't have permission to share this capture."),
            (413, "no", "This capture is too large to share. Share a selected range to upload less."),
            (429, "", "Too many shares. Try again later."),
            (503, "missing bucket", "Sharing is not configured."),
            (0, "", "Could not reach the share server."),
            (500, "disk full", "Could not share this capture (500): disk full"),
            (502, "<html>bad gateway</html>", "Could not share this capture (502)."),
        ] {
            assert_eq!(share_failure(status, body).message, words, "status {status}");
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

    fn account(used: u64, limit: u64, max_capture: u64) -> ShareAccount {
        ShareAccount {
            user: ShareUser {
                id: "u1".into(),
                name: "Ada Lovelace".into(),
                avatar: "https://perfr.dev/a.png".into(),
            },
            quota: ShareQuota {
                used_bytes: used,
                limit_bytes: limit,
                max_capture_bytes: max_capture,
            },
        }
    }

    fn input<'a>(
        session: &'a ShareSession,
        whole: Option<u64>,
        range: Option<u64>,
    ) -> ConfirmInput<'a> {
        ConfirmInput {
            session,
            whole_bytes: whole,
            range_bytes: range,
            can_slice: true,
            has_range: range.is_some(),
            error: None,
            login_waiting: false,
            login_returned: false,
            cross_origin: false,
            busy: false,
        }
    }

    fn action_ids(view: &ShareConfirmView) -> Vec<String> {
        view.actions
            .iter()
            .map(|action| match action {
                ShareAction::Provider { id, .. } => format!("provider:{id}"),
                ShareAction::Upload => "upload".into(),
                ShareAction::Range { bytes } => format!("range:{bytes}"),
                ShareAction::Retry => "retry".into(),
                ShareAction::Cancel => "cancel".into(),
            })
            .collect()
    }

    #[test]
    fn me_is_an_account_or_a_login_prompt() {
        let ok = parse_session_response(
            200,
            r#"{"user":{"id":"u1","name":"Ada Lovelace","avatar":"https://perfr.dev/a.png"},"quota":{"used_bytes":12000000,"limit_bytes":1000000000,"max_capture_bytes":50000000}}"#,
        );
        let SessionCheck::SignedIn(account) = ok else {
            panic!("signed in")
        };
        assert_eq!(account.user.name, "Ada Lovelace");
        assert_eq!(account.quota.used_bytes, 12_000_000);
        assert_eq!(sharing_as_line(&account.user), "Sharing as Ada Lovelace");
        let nameless = ShareUser {
            name: "  ".into(),
            ..account.user.clone()
        };
        assert_eq!(sharing_as_line(&nameless), "Sharing as u1");
        assert_eq!(
            quota_usage_line(&account.quota),
            "12.0 MB of 1.0 GB used · 988.0 MB left · up to 50.0 MB per capture"
        );

        let missing = parse_session_response(
            401,
            r#"{"error":"login_required","login_url":"/login?next=/viewer"}"#,
        );
        assert_eq!(
            missing,
            SessionCheck::LoginRequired {
                login_url: "/login?next=/viewer".into()
            }
        );
        assert!(matches!(
            parse_session_response(401, "<html>login</html>"),
            SessionCheck::LoginRequired { .. }
        ));
        assert!(matches!(
            parse_session_response(200, r#"{"user":{"id":""}}"#,),
            SessionCheck::Unavailable { .. }
        ));
        assert!(matches!(
            parse_session_response(500, "nope"),
            SessionCheck::Unavailable { .. }
        ));
        assert_eq!(me_url("https://perfr.dev/"), "https://perfr.dev/api/me");
    }

    #[test]
    fn login_urls_stay_on_the_share_host() {
        let next = "http://127.0.0.1:44766/?capture=/api/share/demo&x=1";
        let url = login_url("https://perfr.dev", "", "github", next);
        assert_eq!(
            url,
            "https://perfr.dev/login?provider=github&next=http%3A%2F%2F127.0.0.1%3A44766%2F%3Fcapture%3D%2Fapi%2Fshare%2Fdemo%26x%3D1"
        );
        let google = login_url(
            "https://perfr.dev",
            "/login?next=/",
            "google",
            "https://perfr.dev/c/a",
        );
        assert!(google.starts_with("https://perfr.dev/login?provider=google&next="));
        let moved = login_url(
            "https://perfr.dev",
            "/auth/start",
            "github",
            "https://perfr.dev/",
        );
        assert!(moved.starts_with("https://perfr.dev/auth/start?provider=github&next="));
        let evil = login_url(
            "https://perfr.dev",
            "https://evil.example/login",
            "github",
            "https://perfr.dev/",
        );
        assert!(evil.starts_with("https://perfr.dev/login?"), "{evil}");
        assert!(!share_is_cross_origin(
            "https://perfr.dev/c/abc",
            "https://perfr.dev"
        ));
        assert!(share_is_cross_origin(
            "http://127.0.0.1:44766/",
            "https://perfr.dev"
        ));
        assert!(share_is_cross_origin(
            "http://localhost:44766/viewer/",
            "https://Perfr.dev"
        ));
        assert!(!share_is_cross_origin("not a url", "https://perfr.dev"));
        assert_eq!(
            fragment_share_token("#share_token=abc.def~").as_deref(),
            Some("abc.def~")
        );
        assert!(fragment_share_token("#share_token=a%20b").is_none());
        assert!(fragment_share_token("").is_none());
        assert_eq!(
            login_next_url("http://127.0.0.1:9/?capture=1#share_token=abc"),
            "http://127.0.0.1:9/?capture=1"
        );
        assert_eq!(bearer_value("abc"), "Bearer abc");
        assert_eq!(SHARE_CREDENTIALS, "include");
    }

    #[test]
    fn a_capture_over_the_cap_or_the_remaining_quota_is_blocked() {
        let quota = account(12_000_000, 1_000_000_000, 50_000_000).quota;
        assert!(fit_block(50_000_000, &quota, "This capture").is_none());
        let over_capture = fit_block(50_000_001, &quota, "This capture").unwrap();
        assert!(over_capture.contains("50.0 MB"));
        assert!(over_capture.contains("selected range"));
        let tight = account(980_000_000, 1_000_000_000, 50_000_000).quota;
        let over_quota = fit_block(30_000_000, &tight, "This capture").unwrap();
        assert!(over_quota.contains("20.0 MB left"));
        assert!(over_quota.contains("selected range"));
        // The per-capture cap wins when both are exceeded.
        let both = fit_block(80_000_000, &tight, "This capture").unwrap();
        assert!(both.contains("largest capture"));
    }

    #[test]
    fn the_dialog_asks_for_a_sign_in_before_it_offers_upload() {
        let signed_out = ShareSession::SignedOut {
            login_url: "/login".into(),
        };
        let view = confirm_view(&input(&signed_out, Some(1000), None));
        assert_eq!(view.kind, ShareConfirmKind::SignedOut);
        assert_eq!(view.lines[0].text, SIGN_IN_TITLE);
        assert_eq!(
            action_ids(&view),
            ["provider:github", "provider:google", "cancel"]
        );
        assert!(!action_ids(&view).iter().any(|id| id == "upload"));

        let waiting = ConfirmInput {
            login_waiting: true,
            ..input(&signed_out, Some(1000), None)
        };
        let view = confirm_view(&waiting);
        assert!(view
            .lines
            .iter()
            .any(|line| line.text.contains("window that opened")));

        let returned = ConfirmInput {
            login_returned: true,
            cross_origin: true,
            ..input(&signed_out, Some(1000), None)
        };
        let view = confirm_view(&returned);
        assert!(view
            .lines
            .iter()
            .any(|line| line.text.contains("blocking the perfr.dev cookie")));

        let expired = ConfirmInput {
            error: Some("Your session expired. Sign in again to share."),
            login_returned: true,
            cross_origin: true,
            ..input(&signed_out, Some(1000), None)
        };
        let view = confirm_view(&expired);
        assert!(view
            .lines
            .iter()
            .any(|line| line.role == ShareLineRole::Error && line.text.contains("Sign in again")));
    }

    #[test]
    fn the_signed_in_dialog_names_the_account_and_blocks_an_oversized_capture() {
        let session = ShareSession::SignedIn(account(12_000_000, 1_000_000_000, 50_000_000));
        let view = confirm_view(&input(&session, Some(2_100_000), None));
        assert_eq!(view.kind, ShareConfirmKind::SignedIn);
        assert_eq!(view.lines[0].text, "Sharing as Ada Lovelace");
        assert!(view.lines.iter().any(
            |line| line.role == ShareLineRole::Quota && line.text.contains("12.0 MB of 1.0 GB")
        ));
        assert!(view
            .lines
            .iter()
            .any(|line| line.role == ShareLineRole::Size && line.text == "2.1 MB"));
        assert_eq!(action_ids(&view), ["upload", "cancel"]);
        assert!(view
            .lines
            .iter()
            .any(|line| line.role == ShareLineRole::Hint && line.text.contains("right-drag")));

        let blocked = confirm_view(&input(&session, Some(80_000_000), None));
        assert!(blocked
            .lines
            .iter()
            .any(|line| line.role == ShareLineRole::Error && line.text.contains("selected range")));
        assert_eq!(action_ids(&blocked), ["cancel"]);

        let range_ok = confirm_view(&input(&session, Some(80_000_000), Some(400_000)));
        assert_eq!(action_ids(&range_ok), ["range:400000", "cancel"]);
        assert!(range_ok
            .lines
            .iter()
            .any(|line| line.role == ShareLineRole::Hint && line.text.contains("every thread")));

        let range_too = confirm_view(&input(&session, Some(80_000_000), Some(60_000_000)));
        assert_eq!(action_ids(&range_too), ["cancel"]);
        assert!(range_too
            .lines
            .iter()
            .any(|line| line.text.contains("selected range (60.0 MB) is too large")));

        let checking = ShareSession::Checking;
        let view = confirm_view(&input(&checking, None, None));
        assert_eq!(view.kind, ShareConfirmKind::Checking);
        assert!(view.lines[0]
            .text
            .contains("Checking your perfr.dev account"));
        assert_eq!(action_ids(&view), ["cancel"]);
    }

    #[test]
    fn the_auth_doc_and_the_popup_page_use_this_contract() {
        let doc = include_str!("../SHARE_AUTH.md");
        for (name, _) in SHARE_CORS {
            assert!(doc.contains(name), "{name} missing from SHARE_AUTH.md");
        }
        assert!(doc.contains(SIGN_IN_TITLE));
        assert!(doc.contains(SHARE_TOKEN_FRAGMENT));
        assert!(doc.contains("SameSite=None"));
        assert!(doc.contains(ME_PATH));
        assert!(doc.contains(&format!("`Content-Type: {SHARE_CONTENT_TYPE}`")));
        assert_eq!(LOGIN_RESUME_KEY, "perfr.share.resume");
        assert_eq!(LOGIN_MESSAGE_TYPE, "login");
        assert!(ShareFailure::login("Your session expired. Sign in again to share.").needs_login);
        assert!(!ShareFailure::message("Nothing to share.").needs_login);
        let html = include_str!("../viewer-dist/index.html");
        assert!(html.contains(LOGIN_WINDOW_NAME));
        assert!(html.contains(LOGIN_SIGNAL_KEY));
        assert!(html.contains(SHARE_TOKEN_KEY));
        assert!(html.contains(SHARE_TOKEN_FRAGMENT));
        assert!(html.contains(LOGIN_MESSAGE_SOURCE));
    }
}
