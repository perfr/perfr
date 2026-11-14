// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The capture service, as the page's backend: the event socket, the
//! requests under `/api`, and the upload of a shared capture.
//!
//! This is what `perfr-client` adds to the viewer. The viewer asks through
//! `perfr_viewer::net::Backend` and reads the answers out of its inbox; a
//! page built without this backend asks nobody.

// The socket and the upload request are browser handles, which are not
// `Send`. They sit in the same `Arc<Mutex<..>>` as the rest of what the
// page shares with the callbacks the browser runs, all on one thread.
#![allow(clippy::arc_with_non_send_sync)]

use perfr_viewer::net::web::{get_bytes, get_text, js_err, FileBackend};
use perfr_viewer::net::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{BinaryType, MessageEvent, RequestInit, Response, WebSocket};

#[derive(Clone)]
pub struct ServiceNet {
    /// Capture files and stories, as the viewer reads them. Its inbox is
    /// the one everything here delivers to.
    file: FileBackend,
    inbox: Arc<Mutex<Inbox>>,
    http_busy: Arc<AtomicBool>,
    view_busy: Arc<AtomicBool>,
    /// Held so the JS WebSocket is not GC'd.
    ws: Arc<Mutex<Option<WebSocket>>>,
    /// A capture file was opened instead of a service: no socket, and
    /// every request that would go to the service is dropped.
    offline: bool,
    /// The Share upload in flight, kept so it can be aborted.
    upload: Arc<Mutex<ShareUpload<web_sys::XmlHttpRequest>>>,
    /// The latest request for each inbox slot whose replies do not name
    /// their request: both report forms share one, both tree forms too.
    report_seq: Latest,
    tree_seq: Latest,
    disassembly_seq: Latest,
    source_seq: Latest,
}

impl ServiceNet {
    pub fn connect() -> Self {
        // A service is there: its log file takes this page's lines.
        perfr_viewer::logging::set_relay(true);
        let file = FileBackend::default();
        let inbox = file.inbox();
        let ws = Arc::new(Mutex::new(None));
        start_ws(inbox.clone(), ws.clone());
        install_login_listener(inbox.clone());
        Self {
            file,
            inbox,
            http_busy: Arc::new(AtomicBool::new(false)),
            view_busy: Arc::new(AtomicBool::new(false)),
            ws,
            offline: false,
            upload: Arc::default(),
            report_seq: Latest::default(),
            tree_seq: Latest::default(),
            disassembly_seq: Latest::default(),
            source_seq: Latest::default(),
        }
    }

    /// Fetches a capture stream file (`/api/capture/export?format=stream`
    /// saved to disk) and feeds it in as if a service had sent it. The
    /// static web page's mode: no service, no socket.
    pub fn from_capture_url(url: &str) -> Self {
        let file = FileBackend::from_capture_url(url);
        let inbox = file.inbox();
        install_login_listener(inbox.clone());
        Self {
            file,
            inbox,
            http_busy: Arc::new(AtomicBool::new(false)),
            view_busy: Arc::new(AtomicBool::new(false)),
            ws: Arc::new(Mutex::new(None)),
            offline: true,
            upload: Arc::default(),
            report_seq: Latest::default(),
            tree_seq: Latest::default(),
            disassembly_seq: Latest::default(),
            source_seq: Latest::default(),
        }
    }

    fn abort_share_unless(&self, gen: u64) {
        let handle = self.upload.lock().unwrap_or_else(|e| e.into_inner()).stop_unless(gen);
        abort_upload(handle);
    }

    fn fetch_disassembly(&self, url: String) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        let (latest, seq) = (self.disassembly_seq.clone(), self.disassembly_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&url)
                .await
                .and_then(|t| serde_json::from_str::<perfr_viewer::code::Disassembly>(&t).map_err(|e| format!("disassembly: {e}")));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.disassembly = Some(result);
        });
    }

    fn send(&self, method: &'static str, path: &'static str, body: String) {
        // Every command the page gives the service, in the log: these
        // are clicks (Record, Stop, Demo, config), not per-frame pulls.
        log::info!(target: "perfr_client::net", "{method} {path} {}", body.chars().take(200).collect::<String>());
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = send_text(method, path, &body).await {
                log::warn!(target: "perfr_client::net", "{method} {e}");
                inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(e);
            }
        });
    }
}

impl Backend for ServiceNet {
    fn fetch_story(&self, url: &str) {
        self.file.fetch_story(url);
    }

    fn fetch_capture(&self, url: &str) {
        self.file.fetch_capture(url);
    }

    /// What an earlier page load left for Share: the bearer from a
    /// login redirect (the URL fragment, else what `index.html` stored),
    /// taken out of storage as it is read, and whether a same-tab login
    /// asked for the dialog to reopen.
    fn stored_share_auth(&self) -> (Option<String>, bool) {
        let Some(window) = web_sys::window() else {
            return (None, false);
        };
        let token = window
            .location()
            .hash()
            .ok()
            .as_deref()
            .and_then(perfr_viewer::share::fragment_share_token)
            .or_else(|| take_stored_share_token(&window));
        let resume = window
            .session_storage()
            .ok()
            .flatten()
            .and_then(|s| s.get_item(perfr_viewer::share::LOGIN_RESUME_KEY).ok().flatten())
            .is_some_and(|v| v == "1");
        (token, resume)
    }

    fn clear_share_resume_flag(&self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        if let Ok(Some(storage)) = window.session_storage() {
            let _ = storage.remove_item(perfr_viewer::share::LOGIN_RESUME_KEY);
        }
    }

    /// `GET /api/me` with the session cookie and, when we have one, the bearer.
    fn check_share_session(&self, gen: u64, url: String, token: Option<String>) {
        self.abort_share_unless(gen);
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let check = fetch_session(&url, token.as_deref()).await;
            push_share(&inbox, gen, perfr_viewer::share::ShareNoteKind::Session(check));
        });
    }

    /// Open the login URL in a popup. Returns false when the browser
    /// blocked it and this tab is navigating there instead.
    fn open_share_login(&self, url: &str) -> bool {
        let Some(window) = web_sys::window() else {
            return false;
        };
        // What lets the page that comes back take a token from its
        // address: this page asked for one, just now.
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(perfr_viewer::share::LOGIN_PENDING_KEY, &format!("{:.0}", js_sys::Date::now()));
        }
        match window.open_with_url_and_target_and_features(
            url,
            perfr_viewer::share::LOGIN_WINDOW_NAME,
            "popup=yes,width=520,height=720",
        ) {
            Ok(Some(_)) => true,
            _ => {
                if let Ok(Some(storage)) = window.session_storage() {
                    let _ = storage.set_item(perfr_viewer::share::LOGIN_RESUME_KEY, "1");
                }
                let _ = window.location().set_href(url);
                false
            }
        }
    }

    /// Reads a `.perfr.stream` from the service so the dialog can show
    /// its size. Does not post it.
    fn prepare_share(&self, gen: u64, path: String) {
        self.abort_share_unless(gen);
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            push_share(&inbox, gen, perfr_viewer::share::ShareNoteKind::Reading);
            match get_bytes(&path).await {
                Ok(bytes) if !bytes.is_empty() => {
                    push_share(&inbox, gen, perfr_viewer::share::ShareNoteKind::Prepared(bytes));
                }
                Ok(_) => push_share(
                    &inbox,
                    gen,
                    perfr_viewer::share::ShareNoteKind::Failed(perfr_viewer::share::ShareFailure::message(
                        "Nothing to share.",
                    )),
                ),
                Err(e) => push_share(
                    &inbox,
                    gen,
                    perfr_viewer::share::ShareNoteKind::Failed(perfr_viewer::share::ShareFailure::message(
                        format!("Could not read the capture. {e}"),
                    )),
                ),
            }
        });
    }

    /// Posts `.perfr.stream` bytes to `endpoint` after one more `GET /api/me`.
    /// The dialog has already shown the size and the account.
    fn share_capture(
        &self,
        gen: u64,
        endpoint: String,
        me_url: String,
        token: Option<String>,
        bytes: Vec<u8>,
    ) {
        let stale = self.upload.lock().unwrap_or_else(|e| e.into_inner()).begin(gen);
        abort_upload(stale);
        let inbox = self.inbox.clone();
        let upload = self.upload.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if bytes.is_empty() {
                push_share(
                    &inbox,
                    gen,
                    perfr_viewer::share::ShareNoteKind::Failed(perfr_viewer::share::ShareFailure::message(
                        "Nothing to share.",
                    )),
                );
                return;
            }
            let session = fetch_session(&me_url, token.as_deref()).await;
            push_share(
                &inbox,
                gen,
                perfr_viewer::share::ShareNoteKind::Session(session.clone()),
            );
            match session {
                perfr_viewer::share::SessionCheck::SignedIn(account) => {
                    if let Some(message) = perfr_viewer::share::fit_block(
                        bytes.len() as u64,
                        &account.quota,
                        "This capture",
                    ) {
                        push_share(
                            &inbox,
                            gen,
                            perfr_viewer::share::ShareNoteKind::Failed(
                                perfr_viewer::share::ShareFailure::message(message),
                            ),
                        );
                        return;
                    }
                }
                perfr_viewer::share::SessionCheck::LoginRequired { .. } => {
                    push_share(
                        &inbox,
                        gen,
                        perfr_viewer::share::ShareNoteKind::Failed(perfr_viewer::share::ShareFailure::login(
                            "Your session expired. Sign in again to share.",
                        )),
                    );
                    return;
                }
                perfr_viewer::share::SessionCheck::Unavailable { message } => {
                    push_share(
                        &inbox,
                        gen,
                        perfr_viewer::share::ShareNoteKind::Failed(
                            perfr_viewer::share::ShareFailure::message(message),
                        ),
                    );
                    return;
                }
            }
            push_share(
                &inbox,
                gen,
                perfr_viewer::share::ShareNoteKind::Progress {
                    loaded: 0,
                    total: bytes.len() as u64,
                },
            );
            let result = post_share_xhr(&endpoint, &bytes, token.as_deref(), &inbox, gen, &upload).await;
            upload.lock().unwrap_or_else(|e| e.into_inner()).finish(gen);
            let kind = match result {
                Posted::Reply(text) => match perfr_viewer::share::parse_share_reply(&text) {
                    Ok(link) => perfr_viewer::share::ShareNoteKind::Done(link),
                    Err(e) => perfr_viewer::share::ShareNoteKind::Failed(
                        perfr_viewer::share::ShareFailure::message(e),
                    ),
                },
                Posted::Failed(status, body) => perfr_viewer::share::ShareNoteKind::Failed(
                    perfr_viewer::share::share_failure(status, &body),
                ),
                // Whoever stopped it has usually moved to another
                // share generation and never sees this note. When they
                // have not, it is what takes the dialog out of
                // "Uploading".
                Posted::Stopped => perfr_viewer::share::ShareNoteKind::Failed(
                    perfr_viewer::share::ShareFailure::message("The upload was stopped."),
                ),
            };
            push_share(&inbox, gen, kind);
        });
    }

    /// Stops the Share upload in flight, if any: the request is aborted,
    /// or never sent when it had not started. For when the capture it
    /// was reading is replaced or the user thinks better of it; without
    /// this the bytes went to the share server to the end whatever
    /// happened in the page, and the capture stayed there under a link
    /// nobody was shown.
    fn abort_share(&self) {
        let handle = self.upload.lock().unwrap_or_else(|e| e.into_inner()).stop();
        abort_upload(handle);
    }

    fn import_stream_file(&self, file: web_sys::File) {
        self.file.import_stream_file(file);
    }

    fn take(&self) -> Inbox {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).drain()
    }

    /// Closes the event socket. Its `onclose` empties the slot, after
    /// which `reconnect_ws_if_closed` opens a new one; the slot is left
    /// alone here so a socket still closing is not replaced and then
    /// forgotten by its own late `onclose`. A closing socket delivers no
    /// further messages.
    fn close_ws(&self) {
        if let Ok(slot) = self.ws.lock() {
            if let Some(ws) = slot.as_ref() {
                let _ = ws.close();
            }
        }
    }

    /// Opens a new WebSocket if the last one closed -- a service that was
    /// restarted comes back without a page reload. Cheap when connected.
    fn reconnect_ws_if_closed(&self) {
        if self.offline {
            return;
        }
        let closed = self.ws.lock().map(|w| w.is_none()).unwrap_or(true);
        if closed {
            start_ws(self.inbox.clone(), self.ws.clone());
        }
    }

    /// The persisted user settings; the reply lands in `Inbox::settings`.
    fn get_settings(&self) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text("/api/settings")
                .await
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| format!("/api/settings: {e}")));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(v) => g.settings = Some(v),
                Err(e) => g.error = Some(e),
            }
        });
    }

    /// Replaces the persisted user settings with `settings` (the whole
    /// object); the service saves them and echoes what it kept.
    fn put_settings(&self, settings: &serde_json::Value) {
        if self.offline {
            return;
        }
        self.send("PUT", "/api/settings", settings.to_string());
    }

    /// Auto-profiling on or off for the running capture (and the next).
    fn set_auto_profile(&self, on: bool) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/auto_profile", format!(r#"{{"on":{on}}}"#));
    }

    fn get_status(&self) {
        if self.offline {
            return;
        }
        if self
            .http_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let inbox = self.inbox.clone();
        let busy = self.http_busy.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text("/api/status")
                .await
                .and_then(|t| parse_status_json(&t));
            {
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(s) => {
                        g.status = Some(s);
                        g.http_ok = true;
                        g.error = None;
                    }
                    // A failed poll is the one way a dead service shows
                    // over HTTP; without this the flag stayed true forever.
                    Err(e) => {
                        g.http_ok = false;
                        g.error = Some(e);
                    }
                }
            }
            busy.store(false, Ordering::SeqCst);
        });
    }

    fn get_processes(&self) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text("/api/processes")
                .await
                .and_then(|t| parse_processes_json(&t));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(p) => g.processes = Some(p),
                Err(e) => g.error = Some(e),
            }
        });
    }

    fn pull_view(&self, t0: u64, t1: u64, width: u32) {
        if self.offline {
            return;
        }
        if self
            .view_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let width = width.clamp(16, 4096);
        let t1 = t1.max(t0 + 1);
        let inbox = self.inbox.clone();
        let busy = self.view_busy.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let qs = format!("t0={t0}&t1={t1}&width={width}");
            let result = pull_timeline_or_frame(&qs).await;
            {
                let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(ViewPull::Timeline(tl)) => g.timeline = Some(tl),
                    Ok(ViewPull::Frame(fr)) => g.frame = Some(fr),
                    Err(e) => g.error = Some(e),
                }
            }
            busy.store(false, Ordering::SeqCst);
        });
    }

    fn start_capture(&self, req: &CaptureStart) {
        // A new capture replaces the one being shared; the app has
        // dropped that share and would throw the link away.
        self.abort_share();
        if self.offline {
            return;
        }
        self.send("POST", "/api/capture/start", capture_start_body(req));
    }

    fn load_symbols(&self, pid: u32) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/symbols/load", format!(r#"{{"pid":{pid}}}"#));
    }

    fn get_symbols_status(&self, pid: u32) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/symbols/status?pid={pid}"))
                .await
                .and_then(|t| parse_symbols_status_json(&t));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                // Older services return pid 0 for idle. Keep the request
                // identity so a late response cannot reset a new selection.
                Ok(mut s) => {
                    if s.pid == 0 { s.pid = pid; }
                    g.symbols = Some(s);
                }
                Err(e) => g.error = Some(e),
            }
        });
    }

    /// Fetches the sampling report for a selection: the union of the given
    /// `(start_ns, end_ns, tid)` windows. An empty slice means the whole
    /// capture, which is what the panel shows before anything is selected.
    fn get_sampling_report(&self, ranges: &[(u64, u64, Option<u32>)]) {
        if self.offline {
            return;
        }
        let query = ranges_query(ranges);
        let inbox = self.inbox.clone();
        let (latest, seq) = (self.report_seq.clone(), self.report_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/sampling/report{query}"))
                .await
                .and_then(|t| parse_sampling_report_json(&t));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(r) => g.sampling = Some(r),
                // A service without sampling answers 501; that is not an
                // error worth showing the user on every selection.
                Err(_) => g.sampling = None,
            }
        });
    }

    /// The report over every sample inside any instance of the scope.
    fn get_sampling_report_scope(&self, name_id: u32) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        let (latest, seq) = (self.report_seq.clone(), self.report_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/sampling/report?scope={name_id}"))
                .await
                .and_then(|t| parse_sampling_report_json(&t));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.sampling = result.ok();
        });
    }

    fn get_sampling_tree_scope(&self, name_id: u32, mode: &str) {
        if self.offline {
            return;
        }
        let query = format!("/api/sampling/tree?scope={name_id}&mode={mode}");
        let inbox = self.inbox.clone();
        let (latest, seq) = (self.tree_seq.clone(), self.tree_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&query).await.and_then(|t| parse_sampling_tree_json(&t));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.tree = result.ok();
        });
    }

    /// The same samples as a call tree over the union of `ranges`. An empty
    /// slice means the whole capture, which is what the panel asks for when
    /// a capture stops and nothing is selected.
    fn get_sampling_tree(&self, ranges: &[(u64, u64, Option<u32>)], mode: &str) {
        if self.offline {
            return;
        }
        let query = sampling_tree_query(ranges, mode);
        let inbox = self.inbox.clone();
        let (latest, seq) = (self.tree_seq.clone(), self.tree_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/sampling/tree{query}"))
                .await
                .and_then(|t| parse_sampling_tree_json(&t));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.tree = result.ok();
        });
    }

    /// A function of `pid` disassembled, with its source lines.
    fn get_disassembly(&self, pid: u32, function_id: u64) {
        self.fetch_disassembly(format!("/api/code/disassembly?pid={pid}&function_id={function_id}"));
    }

    /// The service's example: one of its own functions.
    fn get_example_disassembly(&self) {
        self.fetch_disassembly("/api/code/example".to_string());
    }

    /// A source file the service may serve (one a disassembly named).
    fn get_source(&self, path: &str) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        let url = format!("/api/code/source?path={}", perfr_viewer::net::percent_encode(path));
        let (latest, seq) = (self.source_seq.clone(), self.source_seq.next());
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&url)
                .await
                .and_then(|t| serde_json::from_str::<perfr_viewer::code::SourceFile>(&t).map_err(|e| format!("source: {e}")));
            if !latest.is(seq) {
                return;
            }
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.source = Some(result);
        });
    }

    fn get_modules(&self, pid: u32) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/symbols/modules?pid={pid}"))
                .await
                .and_then(|t| parse_modules_json(&t));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.modules = result.ok();
        });
    }

    fn search_functions(&self, pid: u32, q: &str, limit: u32) {
        if self.offline {
            return;
        }
        let q = perfr_viewer::util::percent_encode(q);
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!(
                "/api/functions/search?pid={pid}&q={q}&limit={limit}"
            ))
            .await
            .and_then(|t| parse_function_search_json(&t));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(s) => g.function_hits = Some(s),
                Err(e) => g.error = Some(e),
            }
        });
    }

    /// Resolve portable identities against the complete symbol index.
    fn resolve_preset_functions(&self, pid: u32, generation: u64, keys: String) {
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let body = format!(r#"{{"pid":{pid},"functions":{keys}}}"#);
            let result = send_text("POST", "/api/functions/resolve", &body).await
                .and_then(|text| parse_function_search_json(&text));
            inbox.lock().unwrap_or_else(|e| e.into_inner()).preset_functions.push((generation, result));
        });
    }

    /// Every function the service indexed for `pid`, for the Functions
    /// view. One request; the view filters and pages on its own.
    fn list_functions(&self, pid: u32) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text(&format!("/api/functions/search?pid={pid}&q=&limit=200000"))
                .await
                .and_then(|t| parse_function_search_json(&t));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(s) => g.function_list = Some(s),
                Err(e) => g.error = Some(e),
            }
        });
    }

    fn stop_capture(&self) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/capture/stop", "{}".into());
    }

    /// The benchmark producer's state, for the Benchmark window.
    fn get_bench(&self) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = get_text("/api/bench")
                .await
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|e| format!("/api/bench: {e}")));
            let mut g = inbox.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(v) => g.bench = Some(v),
                Err(e) => g.error = Some(e),
            }
        });
    }

    fn start_bench(&self, threads: u32, depth_min: u32, depth_max: u32, rate: u64) {
        if self.offline {
            return;
        }
        self.send(
            "POST",
            "/api/bench/start",
            format!(r#"{{"threads":{threads},"depth_min":{depth_min},"depth_max":{depth_max},"rate":{rate}}}"#),
        );
    }

    /// The live knob: a new rate for a running benchmark.
    fn set_bench_rate(&self, rate: u64) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/bench/rate", format!(r#"{{"rate":{rate}}}"#));
    }

    fn stop_bench(&self) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/bench/stop", "{}".into());
    }

    fn start_demo(&self) {
        self.abort_share();
        if self.offline {
            return;
        }
        self.send(
            "POST",
            "/api/demo/start",
            r#"{"scopes_per_sec":50000}"#.into(),
        );
    }

    fn stop_demo(&self) {
        if self.offline {
            return;
        }
        self.send("POST", "/api/demo/stop", "{}".into());
    }

    /// Empties the capture on the service; the ring's reset comes back
    /// over the WebSocket.
    fn clear_capture(&self) {
        self.abort_share();
        if self.offline {
            return;
        }
        self.send("POST", "/api/capture/clear", "{}".into());
    }

    /// Posts a `.perfr.zip` to the service, which opens it as the current
    /// capture and streams it back over the WebSocket.
    fn import_capture(&self, bytes: Vec<u8>) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = send_bytes("/api/capture/import", &bytes, "application/zip").await {
                inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("open capture: {e}"));
            }
        });
    }

    /// As [`import_capture`](Self::import_capture), reading the browser
    /// `File` first.
    fn import_capture_file(&self, file: web_sys::File) {
        if self.offline {
            return;
        }
        let inbox = self.inbox.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let buf = JsFuture::from(file.array_buffer()).await.map_err(js_err)?;
                let arr = js_sys::Uint8Array::new(&buf);
                let mut bytes = vec![0u8; arr.length() as usize];
                arr.copy_to(&mut bytes);
                send_bytes("/api/capture/import", &bytes, "application/zip").await
            }
            .await;
            if let Err(e) = result {
                inbox.lock().unwrap_or_else(|p| p.into_inner()).error = Some(format!("open capture: {e}"));
            }
        });
    }

    fn apply_config(&self, ring_bytes: u64, spill: &str) {
        if self.offline {
            return;
        }
        self.send("PUT", "/api/config", config_body(ring_bytes, spill));
    }

}

enum ViewPull {
    Timeline(TimelineJson),
    Frame(ServiceFrame),
}

async fn pull_timeline_or_frame(qs: &str) -> Result<ViewPull, String> {
    let tl_text = get_text(&format!("/api/timeline?{qs}")).await?;
    let tl = parse_timeline_json(&tl_text)?;
    if tl.lod == "instanced" && !tl.instances.is_empty() {
        return Ok(ViewPull::Timeline(tl));
    }
    let bytes = get_bytes(&format!("/api/frame?{qs}")).await?;
    Ok(ViewPull::Frame(parse_frame_body(&bytes)?))
}

/// POSTs raw bytes with the given content type; the response text on
/// success, the status or error otherwise.
async fn send_bytes(url: &str, bytes: &[u8], content_type: &str) -> Result<String, String> {
    let opts = RequestInit::new();
    opts.set_method("POST");
    let body = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    body.copy_from(bytes);
    opts.set_body(&body.into());
    let headers = js_sys::Object::new();
    js_sys::Reflect::set(
        &headers,
        &JsValue::from_str("content-type"),
        &JsValue::from_str(content_type),
    )
    .map_err(js_err)?;
    opts.set_headers(&headers);
    let window = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(window.fetch_with_str_and_init(url, &opts))
        .await
        .map_err(js_err)?;
    let resp: Response = resp
        .dyn_into()
        .map_err(|_| "fetch: not a Response".to_string())?;
    let status = resp.status();
    let text = JsFuture::from(resp.text().map_err(js_err)?)
        .await
        .map_err(js_err)?
        .as_string()
        .unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("{status}: {text}"));
    }
    Ok(text)
}

fn push_share(inbox: &Arc<Mutex<Inbox>>, gen: u64, kind: perfr_viewer::share::ShareNoteKind) {
    inbox.lock().unwrap_or_else(|e| e.into_inner()).share_notes.push(perfr_viewer::share::ShareNote { gen, kind });
}

/// POST the stream with an XMLHttpRequest so the upload reports progress.
/// `fetch` does not. The status is 0 when the browser could not reach the host.
fn share_request_credentials() -> web_sys::RequestCredentials {
    if perfr_viewer::share::SHARE_CREDENTIALS == "include" {
        web_sys::RequestCredentials::Include
    } else {
        web_sys::RequestCredentials::SameOrigin
    }
}

async fn fetch_session(url: &str, token: Option<&str>) -> perfr_viewer::share::SessionCheck {
    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_credentials(share_request_credentials());
    if let Some(token) = token.and_then(perfr_viewer::share::usable_share_token) {
        let headers = js_sys::Object::new();
        if js_sys::Reflect::set(
            &headers,
            &JsValue::from_str("authorization"),
            &JsValue::from_str(&perfr_viewer::share::bearer_value(&token)),
        )
        .is_err()
        {
            return perfr_viewer::share::SessionCheck::Unavailable {
                message: "Could not reach the share server.".into(),
            };
        }
        opts.set_headers(&headers);
    }
    let Some(window) = web_sys::window() else {
        return perfr_viewer::share::SessionCheck::Unavailable {
            message: "Could not reach the share server.".into(),
        };
    };
    let resp = match JsFuture::from(window.fetch_with_str_and_init(url, &opts)).await {
        Ok(resp) => resp,
        Err(_) => {
            return perfr_viewer::share::parse_session_response(0, "");
        }
    };
    let Ok(resp) = resp.dyn_into::<Response>() else {
        return perfr_viewer::share::parse_session_response(0, "");
    };
    let status = resp.status();
    let text = match resp.text() {
        Ok(promise) => JsFuture::from(promise)
            .await
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    perfr_viewer::share::parse_session_response(status, &text)
}

fn take_stored_share_token(window: &web_sys::Window) -> Option<String> {
    let mut found = None;
    for storage in [window.session_storage(), window.local_storage()] {
        let Ok(Some(storage)) = storage else { continue };
        if let Ok(Some(raw)) = storage.get_item(perfr_viewer::share::SHARE_TOKEN_KEY) {
            if found.is_none() {
                found = perfr_viewer::share::usable_share_token(&raw);
            }
            let _ = storage.remove_item(perfr_viewer::share::SHARE_TOKEN_KEY);
        }
    }
    found
}

fn install_login_listener(inbox: Arc<Mutex<Inbox>>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let inbox_storage = inbox.clone();
    let onstorage = Closure::wrap(Box::new(move |ev: web_sys::StorageEvent| {
        let key = ev.key().unwrap_or_default();
        if key != perfr_viewer::share::LOGIN_SIGNAL_KEY && key != perfr_viewer::share::SHARE_TOKEN_KEY {
            return;
        }
        // The bearer is kept in memory only: whatever the popup stored
        // is taken out of storage on every event. It used to be removed
        // only when no token was waiting in the inbox, and the popup's
        // two writes (token, then signal) arrive before a frame drains
        // it, so the bearer stayed in localStorage until the next load.
        let stored = web_sys::window().and_then(|w| take_stored_share_token(&w));
        let mut g = inbox_storage.lock().unwrap_or_else(|e| e.into_inner());
        let from_event = if key == perfr_viewer::share::SHARE_TOKEN_KEY {
            ev.new_value().as_deref().and_then(perfr_viewer::share::usable_share_token)
        } else {
            None
        };
        if from_event.is_some() {
            g.share_token = from_event;
        } else if g.share_token.is_none() {
            g.share_token = stored;
        }
        g.share_login_signalled = true;
    }) as Box<dyn FnMut(_)>);
    let _ =
        window.add_event_listener_with_callback("storage", onstorage.as_ref().unchecked_ref());
    onstorage.forget();

    let inbox_message = inbox;
    let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| {
        let data = ev.data();
        let source = js_sys::Reflect::get(&data, &JsValue::from_str("source"))
            .ok()
            .and_then(|v| v.as_string());
        let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type"))
            .ok()
            .and_then(|v| v.as_string());
        if source.as_deref() != Some(perfr_viewer::share::LOGIN_MESSAGE_SOURCE)
            || kind.as_deref() != Some(perfr_viewer::share::LOGIN_MESSAGE_TYPE)
        {
            return;
        }
        // The message is only a wake-up. A token from another origin is ignored;
        // the bearer arrives through same-origin storage.
        inbox_message
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .share_login_signalled = true;
    }) as Box<dyn FnMut(_)>);
    let _ =
        window.add_event_listener_with_callback("message", onmessage.as_ref().unchecked_ref());
    onmessage.forget();
}

/// How a Share upload ended.
enum Posted {
    /// A 2xx answer and its body.
    Reply(String),
    /// The status (0 when the host was not reached) and the body.
    Failed(u16, String),
    /// Aborted through `Net::abort_share`, or never sent because of it.
    Stopped,
}

/// Aborts a request handed back by `ShareUpload`. Called with the
/// upload's lock released: `abort` runs the request's handlers before it
/// returns.
fn abort_upload(handle: Option<web_sys::XmlHttpRequest>) {
    if let Some(xhr) = handle {
        log::info!(target: "perfr_client::net", "share upload aborted");
        let _ = xhr.abort();
    }
}

async fn post_share_xhr(
    url: &str,
    bytes: &[u8],
    token: Option<&str>,
    inbox: &Arc<Mutex<Inbox>>,
    gen: u64,
    upload: &Arc<Mutex<ShareUpload<web_sys::XmlHttpRequest>>>,
) -> Posted {
    let failed = |e: JsValue| Posted::Failed(0, js_err(e));
    let xhr = match web_sys::XmlHttpRequest::new() {
        Ok(xhr) => xhr,
        Err(e) => return failed(e),
    };
    if let Err(e) = xhr.open_with_async("POST", url, true) {
        return failed(e);
    }
    // Credentialed cross-origin upload. Same-origin cookies are sent either way.
    xhr.set_with_credentials(perfr_viewer::share::SHARE_CREDENTIALS == "include");
    if let Err(e) = xhr.set_request_header("content-type", perfr_viewer::share::SHARE_CONTENT_TYPE) {
        return failed(e);
    }
    if let Some(token) = token.and_then(perfr_viewer::share::usable_share_token) {
        if let Err(e) = xhr.set_request_header("authorization", &perfr_viewer::share::bearer_value(&token)) {
            return failed(e);
        }
    }
    let progress_inbox = inbox.clone();
    let onprogress = Closure::wrap(Box::new(move |ev: web_sys::ProgressEvent| {
        let total = if ev.length_computable() {
            ev.total() as u64
        } else {
            0
        };
        push_share(
            &progress_inbox,
            gen,
            perfr_viewer::share::ShareNoteKind::Progress {
                loaded: ev.loaded() as u64,
                total,
            },
        );
    }) as Box<dyn FnMut(_)>);
    if let Ok(upload) = xhr.upload() {
        upload.set_onprogress(Some(onprogress.as_ref().unchecked_ref()));
    }
    onprogress.forget();

    let xhr_done = xhr.clone();
    let stopped = std::rc::Rc::new(std::cell::Cell::new(false));
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let resolve_ok = resolve.clone();
        let onload = Closure::wrap(Box::new(move || {
            let _ = resolve_ok.call1(&JsValue::NULL, &JsValue::UNDEFINED);
        }) as Box<dyn FnMut()>);
        xhr_done.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        let resolve_err = resolve.clone();
        let onerror = Closure::wrap(Box::new(move || {
            let _ = resolve_err.call1(&JsValue::NULL, &JsValue::UNDEFINED);
        }) as Box<dyn FnMut()>);
        xhr_done.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();
        // An aborted request fires neither of the two above: without
        // this the future would wait for ever.
        let resolve_abort = resolve;
        let stopped_abort = stopped.clone();
        let onabort = Closure::wrap(Box::new(move || {
            stopped_abort.set(true);
            let _ = resolve_abort.call1(&JsValue::NULL, &JsValue::UNDEFINED);
        }) as Box<dyn FnMut()>);
        xhr_done.set_onabort(Some(onabort.as_ref().unchecked_ref()));
        onabort.forget();
    });
    // Stopped while the session was being checked: nothing leaves.
    if !upload.lock().unwrap_or_else(|e| e.into_inner()).sending(gen, xhr.clone()) {
        return Posted::Stopped;
    }
    // A copy in its own ArrayBuffer. The threaded build's wasm memory is a
    // SharedArrayBuffer, and XMLHttpRequest refuses to send a view of one.
    let copied = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
    copied.copy_from(bytes);
    if let Err(e) = xhr.send_with_opt_js_u8_array(Some(&copied)) {
        return failed(e);
    }
    let _ = JsFuture::from(promise).await;
    if stopped.get() {
        return Posted::Stopped;
    }
    let status = xhr.status().unwrap_or(0);
    let text = xhr.response_text().ok().flatten().unwrap_or_default();
    if (200..300).contains(&status) {
        Posted::Reply(text)
    } else {
        Posted::Failed(status, text)
    }
}

async fn send_text(method: &str, url: &str, body: &str) -> Result<String, String> {
    let opts = RequestInit::new();
    opts.set_method(method);
    opts.set_body(&JsValue::from_str(body));
    let headers = js_sys::Object::new();
    js_sys::Reflect::set(
        &headers,
        &JsValue::from_str("content-type"),
        &JsValue::from_str("application/json"),
    )
    .map_err(js_err)?;
    opts.set_headers(&headers);
    let window = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(window.fetch_with_str_and_init(url, &opts))
        .await
        .map_err(js_err)?;
    let resp: Response = resp
        .dyn_into()
        .map_err(|_| "fetch: not a Response".to_string())?;
    let status = resp.status();
    let text = JsFuture::from(resp.text().map_err(js_err)?)
        .await
        .map_err(js_err)?;
    let text = text.as_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(format!("{url}: {status} {text}"));
    }
    Ok(text)
}

fn start_ws(inbox: Arc<Mutex<Inbox>>, slot: Arc<Mutex<Option<WebSocket>>>) {
    let Some(window) = web_sys::window() else {
        push_err(&inbox, "no window (WebSocket)");
        return;
    };
    let host = match window.location().host() {
        Ok(h) if !h.is_empty() => h,
        _ => {
            push_err(&inbox, "location.host empty");
            return;
        }
    };
    let proto = if window.location().protocol().ok().as_deref() == Some("https:") {
        "wss"
    } else {
        "ws"
    };
    let url = format!("{proto}://{host}/ws");
    let ws = match WebSocket::new(&url) {
        Ok(ws) => ws,
        Err(e) => {
            push_err(&inbox, &format!("WebSocket open failed: {}", js_err(e)));
            return;
        }
    };
    ws.set_binary_type(BinaryType::Arraybuffer);

    let inbox_open = inbox.clone();
    let onopen = Closure::wrap(Box::new(move |_ev: JsValue| {
        log::info!(target: "perfr_client::net", "WebSocket open");
        if let Ok(mut g) = inbox_open.lock() {
            g.ws_ok = true;
        }
    }) as Box<dyn FnMut(JsValue)>);
    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();

    let inbox_msg = inbox.clone();
    let ws_msg = ws.clone();
    let onmessage = Closure::wrap(Box::new(move |ev: MessageEvent| match ws_bytes(&ev) {
        Ok(bytes) => {
            let queued = match inbox_msg.lock() {
                Ok(mut g) => {
                    g.ws_ok = true;
                    g.bytes_in += bytes.len() as u64;
                    g.queue_frame(bytes, page_hidden())
                }
                Err(_) => Queued::Kept,
            };
            if queued == Queued::Overflow {
                log::warn!(
                    target: "perfr_client::net",
                    "the page is hidden and {} MiB of the event stream were waiting: closing the socket; the capture is read again from the service when the page is shown",
                    HIDDEN_BACKLOG_MAX >> 20
                );
                // The app's retry opens a new socket once frames are
                // drawn again, and that one starts with the whole ring.
                let _ = ws_msg.close();
            }
        }
        Err(e) => push_err(&inbox_msg, &e),
    }) as Box<dyn FnMut(MessageEvent)>);
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let inbox_err = inbox.clone();
    let onerror = Closure::wrap(Box::new(move |_ev: JsValue| {
        push_err(&inbox_err, "WebSocket error");
    }) as Box<dyn FnMut(JsValue)>);
    ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    let inbox_close = inbox;
    let slot_close = slot.clone();
    let onclose = Closure::wrap(Box::new(move |_ev: JsValue| {
        log::warn!(target: "perfr_client::net", "WebSocket closed");
        if let Ok(mut g) = inbox_close.lock() {
            g.ws_ok = false;
            g.error = Some("WebSocket closed".into());
        }
        if let Ok(mut s) = slot_close.lock() {
            *s = None;
        }
    }) as Box<dyn FnMut(JsValue)>);
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(ws);
}

/// Whether the page is hidden (a background tab, a minimised window):
/// the browser then runs no animation frames, so nothing drains the inbox.
fn page_hidden() -> bool {
    web_sys::window().and_then(|w| w.document()).is_some_and(|d| d.hidden())
}

fn ws_bytes(ev: &MessageEvent) -> Result<Vec<u8>, String> {
    let data = ev.data();
    if let Ok(buf) = data.clone().dyn_into::<js_sys::ArrayBuffer>() {
        let arr = js_sys::Uint8Array::new(&buf);
        let mut bytes = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut bytes);
        return Ok(bytes);
    }
    if let Ok(arr) = data.dyn_into::<js_sys::Uint8Array>() {
        let mut bytes = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut bytes);
        return Ok(bytes);
    }
    Err("WebSocket message is not binary".into())
}

fn push_err(inbox: &Arc<Mutex<Inbox>>, msg: &str) {
    if let Ok(mut g) = inbox.lock() {
        g.error = Some(msg.to_string());
    }
    log::error!(target: "perfr_client::net", "{msg}");
}
