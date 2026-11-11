// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The tools an agent calls, and the small HTTP client they share.
//!
//! State that the service does not keep — which process is attached, which
//! functions the next capture should hook — lives here, in the MCP process.
//! Hooks are installed only when a capture starts (`instrumented_functions`
//! on `POST /api/capture/start`). Arming and unhooking edit that set.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

/// Matches `hooks::MAX_HOOKS`. The service silently keeps the first 16;
/// the tool says so before that happens.
const MAX_HOOKS: usize = 16;
const SEARCH_CAP: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub name: &'static str,
    pub sampling: bool,
    pub scheduler: bool,
    pub kernel_uprobes: bool,
    pub auto_profile: bool,
    pub disassembly: bool,
    pub dynamic_hooks: bool,
}

impl Platform {
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::macos()
        } else if cfg!(target_os = "linux") {
            Self::linux()
        } else if cfg!(target_os = "windows") {
            Self::windows()
        } else {
            Self {
                name: "other",
                sampling: false,
                scheduler: false,
                kernel_uprobes: false,
                auto_profile: false,
                disassembly: false,
                dynamic_hooks: false,
            }
        }
    }

    pub fn linux() -> Self {
        Self {
            name: "linux",
            sampling: true,
            scheduler: true,
            kernel_uprobes: true,
            auto_profile: true,
            disassembly: true,
            dynamic_hooks: true,
        }
    }

    /// Windows: the kernel logger gives sampling, the scheduler and thread
    /// states (with an elevated prompt); Frida hooks and manual scopes
    /// need no privilege. Kernel uprobes are Linux, and auto-profiling
    /// rides on them.
    pub fn windows() -> Self {
        Self {
            name: "windows",
            sampling: true,
            scheduler: true,
            kernel_uprobes: false,
            auto_profile: false,
            disassembly: true,
            dynamic_hooks: true,
        }
    }

    pub fn macos() -> Self {
        Self {
            name: "macos",
            sampling: false,
            scheduler: false,
            kernel_uprobes: false,
            auto_profile: false,
            disassembly: false,
            dynamic_hooks: true,
        }
    }
}

#[derive(Clone, Debug)]
struct Armed {
    function_id: u64,
    name: String,
    module: String,
}

struct Session {
    pid: Option<u32>,
    hooks: Vec<Armed>,
    /// Names learned from `perfr_search_functions`, so an id the agent
    /// passes back still has a name. The service has no id lookup.
    known: HashMap<u64, Armed>,
}

pub struct Mcp {
    http: HttpClient,
    session: Mutex<Session>,
    /// The story being written. It lives in this process, not in the service.
    story: Mutex<Option<perfr_story::Story>>,
    /// The only directory `perfr_story_load` and `perfr_story_export` read
    /// and write files in. `None` refuses their `path` argument: the caller
    /// is an agent, or over HTTP anything on this machine, and the server
    /// tells both that it does not touch other files.
    story_dir: Option<PathBuf>,
    platform: Platform,
}

impl Mcp {
    pub fn connect(url: &str, allow_remote: bool, platform: Platform) -> Result<Self, String> {
        Ok(Self {
            http: HttpClient::parse(url, allow_remote)?,
            session: Mutex::new(Session { pid: None, hooks: Vec::new(), known: HashMap::new() }),
            story: Mutex::new(None),
            story_dir: None,
            platform,
        })
    }

    pub fn with_story_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.story_dir = dir;
        self
    }

    /// The file a story tool may read or write for its `path` argument.
    fn story_file(&self, path: &str) -> Result<PathBuf, String> {
        let Some(dir) = &self.story_dir else {
            return Err(format!(
                "{path}: this server was started without a story directory, so the story tools take no path. \
                 Pass the document as json (perfr_story_export returns it), or start `perfr mcp` with --story-dir <dir>."
            ));
        };
        let dir = std::fs::canonicalize(dir).map_err(|e| format!("story directory {}: {e}", dir.display()))?;
        file_inside(&dir, path).map_err(|why| {
            format!(
                "{path}: {why}. Story files are read and written only under {}; pass a path relative to it, \
                 or start `perfr mcp` with --story-dir <dir>.",
                dir.display()
            )
        })
    }

    /// One JSON-RPC message, or a batch of them. `None` when nothing in it
    /// asks for a reply.
    pub fn handle(&self, message: &Value) -> Option<Value> {
        match message {
            // A batch is answered with the replies of its requests, in one
            // array. It has no id of its own: read as a single message it
            // looks like a notification, and the client waits for ever.
            Value::Array(batch) if !batch.is_empty() => {
                let replies: Vec<Value> = batch.iter().filter_map(|one| self.handle_one(one)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            _ => self.handle_one(message),
        }
    }

    fn handle_one(&self, message: &Value) -> Option<Value> {
        if !message.is_object() {
            let error = rpc_error(-32600, "a JSON-RPC message is an object, or a non-empty array of them");
            return Some(json!({ "jsonrpc": "2.0", "id": null, "error": error }));
        }
        let id = message.get("id").cloned();
        if id.is_none() {
            return None;
        }
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_specs() })),
            "tools/call" => self.tools_call(&params),
            _ => Err(rpc_error(-32601, &format!("unknown method {method}"))),
        };
        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-03-26");
        let version = if matches!(requested, "2024-11-05" | "2025-03-26" | "2025-06-18") {
            requested
        } else {
            "2025-03-26"
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "perfr", "version": crate::build_info::package_version() },
            "instructions": INSTRUCTIONS,
        })
    }

    fn tools_call(&self, params: &Value) -> Result<Value, Value> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        if name.is_empty() {
            return Err(rpc_error(-32602, "tools/call needs a name"));
        }
        match self.dispatch(name, &args) {
            Ok(payload) => Ok(tool_ok(&payload)),
            Err(message) => Ok(tool_err(&message)),
        }
    }

    fn dispatch(&self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "perfr_capabilities" => self.capabilities(),
            "perfr_list_processes" => self.list_processes(args),
            "perfr_attach" => self.attach(args),
            "perfr_search_functions" => self.search_functions(args),
            "perfr_arm_hooks" => self.arm_hooks(args),
            "perfr_unhook" => self.unhook(args),
            "perfr_start_capture" => self.start_capture(args),
            "perfr_stop_capture" => self.stop_capture(),
            "perfr_capture_status" => self.capture_status(),
            "perfr_sampling_report" => self.sampling_report(args),
            "perfr_callstacks" => self.callstacks(args),
            "perfr_function_stats" => self.function_stats(args),
            "perfr_story_create" => self.story_create(args),
            "perfr_story_load" => self.story_load(args),
            "perfr_story_get" => self.story_get(),
            "perfr_story_add_step" => self.story_add_step(args),
            "perfr_story_update_step" => self.story_update_step(args),
            "perfr_story_remove_step" => self.story_remove_step(args),
            "perfr_story_set_view" => self.story_set_view(args),
            "perfr_story_add_overlay" => self.story_add_overlay(args),
            "perfr_story_remove_overlay" => self.story_remove_overlay(args),
            "perfr_story_reorder" => self.story_reorder(args),
            "perfr_story_validate" => self.story_validate(),
            "perfr_story_export" => self.story_export(args),
            "perfr_story_find_threads" => self.story_find(args, false),
            "perfr_story_find_scopes" => self.story_find(args, true),
            "perfr_story_hotspots" => self.story_hotspots(args),
            "perfr_story_cpu" => self.story_cpu(args),
            "perfr_logs" => self.logs(args),
            _ => Err(format!("unknown tool {name}")),
        }
    }

    fn capabilities(&self) -> Result<Value, String> {
        let (reachable, capture, error) = match self.http.get_json("/api/status", Duration::from_secs(2)) {
            Ok(status) => (true, Some(summarize_status(&status)), None),
            Err(error) => (false, None, Some(error)),
        };
        let session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
        let p = self.platform;
        let engines: Vec<&str> = if p.kernel_uprobes {
            vec!["frida", "kernel_uprobes"]
        } else if p.dynamic_hooks {
            vec!["frida"]
        } else {
            vec![]
        };
        Ok(json!({
            "platform": p.name,
            "service_url": self.http.display,
            "service_reachable": reachable,
            "service_error": error,
            "features": {
                "sampling": feature(p.sampling, "Call-stack sampling. Linux and Windows (elevated prompt); macOS has no sampler."),
                "scheduler": feature(p.scheduler, "Context switches and thread states. Linux and Windows (elevated prompt)."),
                "dynamic_hooks": {
                    "supported": p.dynamic_hooks,
                    "engines": engines,
                    "detail": "Frida hooks a running process with no rebuild. kernel_uprobes is Linux-only and needs root or CAP_SYS_ADMIN; it is never the default.",
                },
                "auto_profile": feature(p.auto_profile, "The service chooses hooks from samples and prunes them to a budget. Linux only, off unless requested."),
                "disassembly": feature(p.disassembly, "Disassembly next to source. Linux and Windows (no source lines from a PDB yet)."),
                "function_stats": feature(true, "Count, total, average, min, max and standard deviation for hooks and manual scopes."),
            },
            "session": {
                "attached_pid": session.pid,
                "armed_hooks": session.hooks.len(),
                "armed": session.hooks.iter().take(16).map(armed_json).collect::<Vec<_>>(),
            },
            "capture": capture,
            "limits": { "max_hooks": MAX_HOOKS },
            "auth": "perfr has no authentication, and this server does not add any. It talks to loopback unless started with --allow-remote, and its HTTP transport binds to loopback only. Keep the service on 127.0.0.1.",
        }))
    }

    fn list_processes(&self, args: &Value) -> Result<Value, String> {
        let limit = page_limit(args, 20, 50);
        let offset = page_offset(args);
        let query = arg_str(args, "query").unwrap_or("").to_ascii_lowercase();
        let raw = self.http.get_json("/api/processes", Duration::from_secs(10))?;
        let list = raw.as_array().cloned().unwrap_or_default();
        let matched: Vec<&Value> = list
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase().contains(&query)
            })
            .collect();
        let total = matched.len();
        let page: Vec<Value> = matched
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|row| {
                json!({
                    "pid": row.get("pid").and_then(|v| v.as_u64()).unwrap_or(0),
                    "name": row.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "cpu_percent": round1(row.get("cpu").and_then(|v| v.as_f64()).unwrap_or(0.0)),
                })
            })
            .collect();
        Ok(json!({
            "total": total,
            "offset": offset,
            "limit": limit,
            "processes": page,
        }))
    }

    fn attach(&self, args: &Value) -> Result<Value, String> {
        let pid = arg_u32(args, "pid").ok_or("pid is required")?;
        let timeout = Duration::from_millis(arg_u64(args, "timeout_ms").unwrap_or(30_000).min(120_000));
        self.http.post_ok("/api/symbols/load", &json!({ "pid": pid }), Duration::from_secs(30))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let status = self.http.get_json(&format!("/api/symbols/status?pid={pid}"), Duration::from_secs(10))?;
            let state = status.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if state == "ready" {
                let mut session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
                let switched = session.pid != Some(pid);
                let had_hooks = !session.hooks.is_empty();
                if switched {
                    session.hooks.clear();
                    session.known.clear();
                }
                session.pid = Some(pid);
                return Ok(json!({
                    "attached": true,
                    "pid": pid,
                    "function_count": status.get("function_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "module_count": status.get("module_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "hooks_cleared": switched && had_hooks,
                }));
            }
            if state == "error" {
                let error = status.get("error").and_then(|v| v.as_str()).unwrap_or("symbol load failed");
                return Err(error.to_string());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("symbols for pid {pid} were not ready within {} ms (status {state})", timeout.as_millis()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn search_functions(&self, args: &Value) -> Result<Value, String> {
        let pid = self.pid_of(args)?;
        let query = arg_str(args, "query").unwrap_or("");
        let limit = page_limit(args, 15, 40);
        let offset = page_offset(args);
        if offset + limit > SEARCH_CAP {
            return Err(format!(
                "search pages at most {SEARCH_CAP} hits. Narrow query instead of offset {offset}."
            ));
        }
        let ask = offset + limit;
        let path = format!(
            "/api/functions/search?pid={pid}&q={}&limit={ask}",
            urlencode(query)
        );
        let body = self.http.get_json(&path, Duration::from_secs(30))?;
        let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status != "ready" && status != "" {
            return Err(format!("symbols are {status}; call perfr_attach and wait until it returns"));
        }
        let functions = body.get("functions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        {
            let mut session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
            for row in &functions {
                let hit = armed_from(row);
                if hit.function_id != 0 {
                    session.known.insert(hit.function_id, hit);
                }
            }
        }
        let more = functions.len() >= ask;
        let page: Vec<Value> = functions
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|row| {
                json!({
                    "function_id": row.get("function_id").and_then(json_u64).unwrap_or(0),
                    "name": clip(row.get("name").and_then(|v| v.as_str()).unwrap_or("")),
                    "module": basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")),
                    "size": row.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    "safety": row.get("safety").and_then(|v| v.as_str()).unwrap_or("unknown"),
                })
            })
            .collect();
        Ok(json!({
            "pid": pid,
            "offset": offset,
            "limit": limit,
            "more": more,
            "index_size": body.get("total").and_then(|v| v.as_u64()).unwrap_or(0),
            "functions": page,
        }))
    }

    fn arm_hooks(&self, args: &Value) -> Result<Value, String> {
        let pid = self.pid_of(args)?;
        let mode = arg_str(args, "mode").unwrap_or("add");
        if mode != "add" && mode != "replace" {
            return Err("mode is add or replace".into());
        }
        let mut resolved = Vec::new();
        let mut ambiguous = Vec::new();
        let mut missing = Vec::new();
        for id in arg_u64s(args, "function_ids") {
            match self.lookup_id(pid, id)? {
                Some(hit) => resolved.push(hit),
                None => missing.push(id.to_string()),
            }
        }
        let module = arg_str(args, "module").unwrap_or("");
        for name in arg_strs(args, "names") {
            match self.resolve_name(pid, &name, module)? {
                NameHit::One(hit) => resolved.push(hit),
                NameHit::None => missing.push(name),
                NameHit::Many(modules) => ambiguous.push(json!({ "name": name, "modules": modules })),
            }
        }
        if resolved.is_empty() && ambiguous.is_empty() && missing.is_empty() {
            return Err("pass function_ids or names".into());
        }
        let mut session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
        if mode == "replace" {
            session.hooks.clear();
        }
        for hit in resolved {
            if session.hooks.iter().any(|h| h.function_id == hit.function_id) {
                continue;
            }
            session.hooks.push(hit);
        }
        let dropped = session.hooks.len().saturating_sub(MAX_HOOKS);
        session.hooks.truncate(MAX_HOOKS);
        let capturing = self.capturing().unwrap_or(false);
        Ok(json!({
            "pid": pid,
            "armed": session.hooks.len(),
            "hooks": session.hooks.iter().map(armed_json).collect::<Vec<_>>(),
            "dropped_over_cap": dropped,
            "missing": missing,
            "ambiguous": ambiguous,
            "applies": if capturing { "next_capture" } else { "next_start" },
            "capturing": capturing,
            "note": "Hooks are installed when perfr_start_capture runs. This call does not modify the target.",
        }))
    }

    fn unhook(&self, args: &Value) -> Result<Value, String> {
        let all = arg_bool(args, "all").unwrap_or(false);
        let ids = arg_u64s(args, "function_ids");
        let names = arg_strs(args, "names");
        if !all && ids.is_empty() && names.is_empty() {
            return Err("pass function_ids, names, or all:true".into());
        }
        let mut session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
        let before = session.hooks.len();
        if all {
            session.hooks.clear();
        } else {
            session.hooks.retain(|hook| {
                !ids.contains(&hook.function_id) && !names.iter().any(|name| name == &hook.name)
            });
        }
        let capturing = self.capturing().unwrap_or(false);
        Ok(json!({
            "removed": before - session.hooks.len(),
            "armed": session.hooks.len(),
            "hooks": session.hooks.iter().map(armed_json).collect::<Vec<_>>(),
            "capturing": capturing,
            "note": if capturing {
                "The running capture keeps the hooks it started with. Stop it and start again to apply this set."
            } else {
                "The next capture will not instrument the removed functions."
            },
        }))
    }

    fn start_capture(&self, args: &Value) -> Result<Value, String> {
        let pid = self.pid_of(args)?;
        let mut unsupported = Vec::new();
        if arg_bool(args, "sampling") == Some(true) && !self.platform.sampling {
            unsupported.push("sampling");
        }
        if arg_bool(args, "scheduler") == Some(true) && !self.platform.scheduler {
            unsupported.push("scheduler");
        }
        if arg_bool(args, "auto_profile") == Some(true) && !self.platform.auto_profile {
            unsupported.push("auto_profile");
        }
        let instrumentation = arg_str(args, "instrumentation").unwrap_or("frida");
        if instrumentation == "kernel_uprobes" && !self.platform.kernel_uprobes {
            unsupported.push("kernel_uprobes");
        }
        if instrumentation != "frida" && instrumentation != "kernel_uprobes" {
            return Err("instrumentation is frida or kernel_uprobes".into());
        }
        if !unsupported.is_empty() {
            return Ok(json!({
                "started": false,
                "supported": false,
                "platform": self.platform.name,
                "unsupported": unsupported,
                "message": format!(
                    "{} perfr does not support {}. Start again without those fields. Function timing from Frida hooks still works via perfr_function_stats.",
                    self.platform.name,
                    unsupported.join(", ")
                ),
            }));
        }
        let sampling = arg_bool(args, "sampling").unwrap_or(self.platform.sampling);
        let scheduler = arg_bool(args, "scheduler").unwrap_or(self.platform.scheduler);
        let auto_profile = arg_bool(args, "auto_profile").unwrap_or(false);
        let rate = arg_f64(args, "samples_per_second").unwrap_or(1000.0).clamp(1.0, 10_000.0);
        let hooks = self.session.lock().map_err(|_| "session poisoned".to_string())?.hooks.clone();
        let body = json!({
            "pid": pid,
            "enable_api": true,
            "sampling": sampling,
            "context_switches": scheduler,
            "thread_states": scheduler,
            "samples_per_second": rate,
            "dynamic_instrumentation_method": instrumentation,
            "instrumented_functions": hooks.iter().map(|h| json!({"function_id": h.function_id})).collect::<Vec<_>>(),
            "auto_profile": auto_profile,
        });
        self.http.post_ok("/api/capture/start", &body, Duration::from_secs(120))?;
        Ok(json!({
            "started": true,
            "supported": true,
            "pid": pid,
            "sampling": sampling,
            "scheduler": scheduler,
            "samples_per_second": rate,
            "instrumentation": instrumentation,
            "auto_profile": auto_profile,
            "hooks": hooks.len(),
            "hook_names": hooks.iter().map(|h| clip(&h.name)).collect::<Vec<_>>(),
        }))
    }

    fn stop_capture(&self) -> Result<Value, String> {
        self.http.post_ok("/api/capture/stop", &json!({}), Duration::from_secs(60))?;
        Ok(json!({ "stopped": true }))
    }

    fn capture_status(&self) -> Result<Value, String> {
        let status = self.http.get_json("/api/status", Duration::from_secs(5))?;
        Ok(summarize_status(&status))
    }

    fn sampling_report(&self, args: &Value) -> Result<Value, String> {
        if !self.platform.sampling {
            return Ok(unsupported_feature("sampling"));
        }
        let limit = page_limit(args, 15, 40);
        let offset = page_offset(args);
        match self.http.get_raw("/api/sampling/report", Duration::from_secs(60)) {
            Ok(raw) if raw.status == 501 => Ok(unsupported_feature("sampling")),
            Ok(raw) => {
                let body = parse_body(&raw)?;
                let functions = body.get("functions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let total = functions.len();
                let hotspots: Vec<Value> = functions
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|row| {
                        let id = row.get("function_id").and_then(json_u64).unwrap_or(0);
                        json!({
                            "name": clip(row.get("name").and_then(|v| v.as_str()).unwrap_or("")),
                            "module": basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")),
                            "function_id": if id == 0 { Value::Null } else { json!(id) },
                            "self": row.get("self").and_then(|v| v.as_u64()).unwrap_or(0),
                            "inclusive": row.get("inclusive").and_then(|v| v.as_u64()).unwrap_or(0),
                            "self_percent": round1(row.get("self_percent").and_then(|v| v.as_f64()).unwrap_or(0.0)),
                            "inclusive_percent": round1(row.get("inclusive_percent").and_then(|v| v.as_f64()).unwrap_or(0.0)),
                        })
                    })
                    .collect();
                Ok(json!({
                    "supported": true,
                    "samples": body.get("samples").and_then(|v| v.as_u64()).unwrap_or(0),
                    "total_functions": total,
                    "offset": offset,
                    "limit": limit,
                    "hotspots": hotspots,
                }))
            }
            Err(error) => Err(error),
        }
    }

    fn callstacks(&self, args: &Value) -> Result<Value, String> {
        if !self.platform.sampling {
            return Ok(unsupported_feature("callstacks"));
        }
        let limit = page_limit(args, 8, 20);
        let offset = page_offset(args);
        let path = format!("/api/sampling/callstacks?offset={offset}&limit={limit}");
        match self.http.get_raw(&path, Duration::from_secs(60)) {
            Ok(raw) if raw.status == 501 => Ok(unsupported_feature("callstacks")),
            Ok(raw) => {
                let mut body = parse_body(&raw)?;
                if let Some(stacks) = body.get_mut("stacks").and_then(|v| v.as_array_mut()) {
                    for stack in stacks {
                        if let Some(frames) = stack.get_mut("frames").and_then(|v| v.as_array_mut()) {
                            for frame in frames {
                                if let Some(text) = frame.as_str() {
                                    *frame = json!(clip(text));
                                }
                            }
                        }
                    }
                }
                if let Some(map) = body.as_object_mut() {
                    map.insert("supported".into(), json!(true));
                }
                Ok(body)
            }
            Err(error) => Err(error),
        }
    }

    fn logs(&self, args: &Value) -> Result<Value, String> {
        let limit = page_limit(args, 30, 100);
        let offset = page_offset(args);
        let mut path = format!("/api/logs?offset={offset}&limit={limit}");
        if let Some(t0) = arg_u64(args, "t0_ns") {
            path.push_str(&format!("&t0={t0}"));
        }
        if let Some(t1) = arg_u64(args, "t1_ns") {
            path.push_str(&format!("&t1={t1}"));
        }
        if let Some(tid) = arg_u32(args, "tid") {
            path.push_str(&format!("&tid={tid}"));
        }
        if let Some(scope) = arg_str(args, "scope") {
            if !scope.is_empty() {
                path.push_str("&scope=");
                path.push_str(&urlencode(scope));
            }
        }
        let body = self.http.get_json(&path, Duration::from_secs(60))?;
        if let Some(error) = body.get("error").and_then(|v| v.as_str()) {
            return Err(error.to_string());
        }
        Ok(body)
    }

    fn function_stats(&self, args: &Value) -> Result<Value, String> {
        let limit = page_limit(args, 15, 40);
        let offset = page_offset(args);
        let order = arg_str(args, "order").unwrap_or("total");
        if !matches!(order, "total" | "count" | "avg" | "max") {
            return Err("order is total, count, avg, or max".into());
        }
        let mut path = format!("/api/functions/stats?offset={offset}&limit={limit}&order={order}");
        if let Some(query) = arg_str(args, "query") {
            if !query.is_empty() {
                path.push_str("&q=");
                path.push_str(&urlencode(query));
            }
        }
        if let Some(pid) = arg_u32(args, "pid") {
            path.push_str(&format!("&pid={pid}"));
        }
        let mut body = self.http.get_json(&path, Duration::from_secs(60))?;
        if let Some(rows) = body.get_mut("rows").and_then(|v| v.as_array_mut()) {
            for row in rows {
                if let Some(name) = row.get("name").and_then(|v| v.as_str()) {
                    let clipped = clip(name);
                    if let Some(map) = row.as_object_mut() {
                        map.insert("name".into(), json!(clipped));
                    }
                }
            }
        }
        Ok(body)
    }

    fn pid_of(&self, args: &Value) -> Result<u32, String> {
        if let Some(pid) = arg_u32(args, "pid") {
            return Ok(pid);
        }
        self.session
            .lock()
            .map_err(|_| "session poisoned".to_string())?
            .pid
            .ok_or_else(|| "no process attached; call perfr_attach or pass pid".into())
    }

    fn capturing(&self) -> Result<bool, String> {
        let status = self.http.get_json("/api/status", Duration::from_secs(2))?;
        Ok(status.get("capturing").and_then(|v| v.as_bool()).unwrap_or(false))
    }

    fn lookup_id(&self, _pid: u32, id: u64) -> Result<Option<Armed>, String> {
        let session = self.session.lock().map_err(|_| "session poisoned".to_string())?;
        if let Some(hit) = session.known.get(&id) {
            return Ok(Some(hit.clone()));
        }
        // The id was not on a page this session searched. Keep it: the
        // service resolves ids itself and skips one it does not know.
        Ok(Some(Armed { function_id: id, name: format!("function {id}"), module: String::new() }))
    }

    fn resolve_name(&self, pid: u32, name: &str, module: &str) -> Result<NameHit, String> {
        let path = format!("/api/functions/search?pid={pid}&q={}&limit=20", urlencode(name));
        let body = self.http.get_json(&path, Duration::from_secs(30))?;
        let functions = body.get("functions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let exact: Vec<&Value> = functions
            .iter()
            .filter(|row| row.get("name").and_then(|v| v.as_str()) == Some(name))
            .filter(|row| module.is_empty() || basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")) == module)
            .collect();
        match exact.as_slice() {
            [] => Ok(NameHit::None),
            [one] => Ok(NameHit::One(armed_from(one))),
            many => Ok(NameHit::Many(
                many.iter()
                    .map(|row| basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")).to_string())
                    .collect(),
            )),
        }
    }
}

enum NameHit {
    One(Armed),
    None,
    Many(Vec<String>),
}

fn armed_from(row: &Value) -> Armed {
    Armed {
        function_id: row.get("function_id").and_then(json_u64).unwrap_or(0),
        name: row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        module: basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")).to_string(),
    }
}

fn armed_json(hook: &Armed) -> Value {
    json!({
        "function_id": hook.function_id,
        "name": clip(&hook.name),
        "module": hook.module,
    })
}

fn feature(supported: bool, detail: &str) -> Value {
    json!({ "supported": supported, "detail": detail })
}

fn unsupported_feature(feature: &str) -> Value {
    json!({
        "supported": false,
        "feature": feature,
        "message": format!(
            "{feature} is not recorded here: this platform, or the one the service runs on, has no support for it. Frida hooks and manual scopes are recorded everywhere; read those with perfr_function_stats."
        ),
    })
}

fn summarize_status(status: &Value) -> Value {
    let instrumentation = status.get("instrumentation").and_then(|v| v.as_str()).unwrap_or("");
    let crash = status.get("hook_crash").and_then(|v| v.as_str()).unwrap_or("");
    json!({
        "capturing": status.get("capturing").and_then(|v| v.as_bool()).unwrap_or(false),
        "target_pid": status.get("target_pid").and_then(|v| v.as_u64()).unwrap_or(0),
        "events": status.get("events_live").and_then(|v| v.as_u64()).unwrap_or(0),
        "dropped": status.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0),
        "instrumentation": clip_len(instrumentation, 300),
        "hook_crash": if crash.is_empty() { Value::Null } else { json!(clip_len(crash, 300)) },
        "capture_start_ns": status.get("capture_start_ns").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

const INSTRUCTIONS: &str = "\
perfr profiles a running process. The service must already be running \
(default http://127.0.0.1:44766). Call perfr_capabilities first. \
Typical loop: perfr_list_processes, perfr_attach, perfr_search_functions, \
perfr_arm_hooks, perfr_start_capture, perfr_stop_capture, then \
perfr_function_stats and, where the platform samples, perfr_sampling_report \
and perfr_callstacks. Change the hook set with perfr_unhook and start again; \
a running capture keeps the hooks it started with. Sampling and the scheduler \
are recorded on Linux, and on Windows when the service runs elevated; \
on macOS those tools say so and do not invent an empty profile. \
This server does not edit source, rebuild, kill processes, or delete files. \
A capture story is a separate JSON document. perfr_story_create or \
perfr_story_load starts one; perfr_story_find_threads, perfr_story_find_scopes, \
perfr_story_hotspots and perfr_story_cpu read the capture so a step can name \
a real track, scope or time range. Numbers from those tools are readings of \
this capture. perfr_story_export writes the document; it does not change the capture. \
The story tools read and write files only inside the story directory (where this \
server was started, or its --story-dir), and replace a file only if it is a story. \
The HTTP API has no authentication.";

fn tool_ok(payload: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": payload.to_string() }],
        "isError": false,
    })
}

fn tool_err(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": json!({"error": message}).to_string() }],
        "isError": true,
    })
}

fn rpc_error(code: i32, message: &str) -> Value {
    json!({ "code": code, "message": message })
}

pub fn tool_specs() -> Vec<Value> {
    vec![
        tool(
            "perfr_capabilities",
            "Read this first. Platform, which profiling features this machine supports, whether perfr is reachable, and the session (attached pid, armed hooks). Sampling and the scheduler are Linux, and Windows when the service runs elevated; kernel uprobes and auto-profiling are Linux-only. macOS supports Frida hooks and manual scopes. Does not modify anything.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_list_processes",
            "List processes perfr can see, busiest first. query is a case-insensitive substring of the process name. One page of pid, name and cpu_percent. Pass a pid to perfr_attach.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Substring of the process name. Empty lists the busiest." },
                    "limit": { "type": "integer", "description": "Page size. Default 20, max 50." },
                    "offset": { "type": "integer", "description": "Rows to skip. Default 0." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_attach",
            "Load symbols for pid and remember it as the target. Waits until symbols are ready or timeout_ms (default 30000, max 120000). Switching pid clears hooks armed for the previous process. Does not start a capture and does not modify the target.",
            json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Process id from perfr_list_processes." },
                    "timeout_ms": { "type": "integer", "description": "How long to wait for symbols. Default 30000." }
                },
                "required": ["pid"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_search_functions",
            "Search the attached process for functions to hook. Returns one page of function_id, name, module, size and hook safety. The service ranks hits; offset pages that ranking and must stay within 200. Pass function_id to perfr_arm_hooks. Requires perfr_attach, or pid.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Symbol text to search for." },
                    "pid": { "type": "integer", "description": "Defaults to the attached process." },
                    "limit": { "type": "integer", "description": "Page size. Default 15, max 40." },
                    "offset": { "type": "integer", "description": "Hits to skip. offset + limit must be <= 200." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_arm_hooks",
            "Choose functions the next capture will instrument. Pass function_ids from perfr_search_functions, or exact names (narrow with module when a name exists in more than one). mode add (default) or replace. At most 16 functions; extras are counted in dropped_over_cap and not armed. Does not modify the running process. A capture already running keeps the set it started with until you stop and start again.",
            json!({
                "type": "object",
                "properties": {
                    "function_ids": { "type": "array", "items": { "type": "integer" }, "description": "Ids from perfr_search_functions." },
                    "names": { "type": "array", "items": { "type": "string" }, "description": "Exact symbol names." },
                    "module": { "type": "string", "description": "Module basename, used when a name is ambiguous." },
                    "mode": { "type": "string", "enum": ["add", "replace"], "description": "add keeps the current set. replace starts over." },
                    "pid": { "type": "integer", "description": "Defaults to the attached process." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_unhook",
            "Remove functions from the set the next capture will instrument. Pass function_ids, exact names, or all:true. Does not kill the target. A capture already running keeps its hooks until perfr_stop_capture and perfr_start_capture.",
            json!({
                "type": "object",
                "properties": {
                    "function_ids": { "type": "array", "items": { "type": "integer" } },
                    "names": { "type": "array", "items": { "type": "string" } },
                    "all": { "type": "boolean", "description": "Clear the whole set." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_start_capture",
            "Start a capture of the attached pid. Instruments the armed hooks with Frida unless instrumentation is kernel_uprobes (Linux, and only when you set it). sampling and scheduler default on where the platform supports them. Asking for an unsupported feature does not start the capture; the result lists what to drop. auto_profile defaults off. Starting replaces the previous capture.",
            json!({
                "type": "object",
                "properties": {
                    "pid": { "type": "integer", "description": "Defaults to the attached process." },
                    "sampling": { "type": "boolean", "description": "Call-stack samples. Default on for Linux and Windows, unsupported on macOS." },
                    "scheduler": { "type": "boolean", "description": "Context switches and thread states. Default on for Linux and Windows, unsupported on macOS." },
                    "samples_per_second": { "type": "number", "description": "Sample rate. Default 1000. Clamped to 1..10000." },
                    "instrumentation": { "type": "string", "enum": ["frida", "kernel_uprobes"], "description": "Default frida." },
                    "auto_profile": { "type": "boolean", "description": "Let the service pick hooks. Linux only. Default false." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_stop_capture",
            "Stop the running capture. Events stay readable until the next perfr_start_capture, which clears them.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "perfr_capture_status",
            "Short capture status: whether it is running, target pid, event count, the instrumentation line, and a hook-crash summary when one was recorded.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "perfr_sampling_report",
            "Hottest functions by sampled self time, one page. Counts and percents, not nanoseconds. function_id is null when the frame cannot be hooked. On macOS the result is supported:false rather than an empty profile.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Page size. Default 15, max 40." },
                    "offset": { "type": "integer", "description": "Functions to skip. Default 0." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_callstacks",
            "The hottest distinct call stacks, outermost frame first, one page. Each stack is a short name list plus how many samples hit it. On macOS the result is supported:false.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Page size. Default 8, max 20." },
                    "offset": { "type": "integer", "description": "Stacks to skip. Default 0." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_logs",
            "Log lines from the current capture. Filter by time range (t0_ns, t1_ns; omit t1_ns for everything after t0) and thread id. Pass scope, the exact name of a manual scope, to keep only the lines logged while that scope was open on the same thread: scopes are how log lines are grouped. With scope, identical lines are folded into one row with a count (grouped is true), and the time range limits which instances of the scope are considered. A line is matched to a scope by timestamp and thread, not by an id stored on the line.",
            json!({
                "type": "object",
                "properties": {
                    "t0_ns": { "type": "integer", "description": "Range start, inclusive. Default 0." },
                    "t1_ns": { "type": "integer", "description": "Range end, inclusive. Omit or 0 for no end." },
                    "tid": { "type": "integer", "description": "Only this thread." },
                    "scope": { "type": "string", "description": "Exact scope name. Lines logged inside its instances, identical lines folded with a count." },
                    "limit": { "type": "integer", "description": "Page size. Default 30, max 100." },
                    "offset": { "type": "integer", "description": "Rows to skip. Default 0." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_function_stats",
            "Timing for hooked functions and manual scopes in the current capture. Times are integer nanoseconds: count, total_ns, avg_ns, min_ns, max_ns, stddev_ns. kind is hook, scope, or track. Sampled call-stack frames are excluded. Filter with query (name substring) and pid. order is total (default), count, avg, or max. This is the reading to use after a dynamic hook, including on macOS.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring of the function name." },
                    "pid": { "type": "integer", "description": "Only rows from this process." },
                    "order": { "type": "string", "enum": ["total", "count", "avg", "max"] },
                    "limit": { "type": "integer", "description": "Page size. Default 15, max 40." },
                    "offset": { "type": "integer", "description": "Rows to skip. Default 0." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_create",
            "Start a new capture story in this MCP process, replacing any story already loaded. The document is perfr.story version 1. capture is the URL the viewer should open when the page did not pass ?capture=. Nothing is written until perfr_story_export.",
            json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "summary": { "type": "string", "description": "What the story is willing to claim. Do not put numbers here that the capture did not measure." },
                    "capture": { "type": "string", "description": "Capture URL, for example captures/box3d.perfr.stream. Empty if the page already loaded one." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_load",
            "Replace the in-memory story with a perfr.story JSON document, either the json string or a file at path. The file is read, not modified. path is relative to the story directory: where this server was started, or its --story-dir.",
            json!({
                "type": "object",
                "properties": {
                    "json": { "type": "string", "description": "The document." },
                    "path": { "type": "string", "description": "A UTF-8 file to read, relative to the story directory." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_get",
            "Return the story loaded in this process: the document and a short list of steps (id, kind, title, overlay count).",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "perfr_story_add_step",
            "Append a step, or insert it at index. kind is context, problem, evidence, root_cause, proposed_fix, applied_fix, or before_after. view uses capture nanoseconds or fractions of the capture span. Overlays are optional here; perfr_story_add_overlay adds them later.",
            json!({
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "description": "Insert position. Default is the end." },
                    "step": { "type": "object", "description": "id, kind, title, body, duration_ms, view, overlays." }
                },
                "required": ["step"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_update_step",
            "Replace fields of one step. Pass id and any of title, body, kind, duration_ms, view, overlays. A present view or overlays replaces the whole value.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "kind": { "type": "string" },
                    "duration_ms": { "type": "integer" },
                    "view": { "type": "object" },
                    "overlays": { "type": "array" }
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_remove_step",
            "Remove the step with this id. Later steps keep their ids and move up to close the gap.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_set_view",
            "Set the camera for one step. t0_ns/t1_ns are capture nanoseconds. t0_frac/t1_frac are fractions of the capture span. fit is content or span. expanded, collapsed and focused are track objects: {kind:scheduler}, {kind:thread,pid,tid}, {kind:process,pid}, {kind:async,pid,tid}, or {kind:name,name}. scroll is {kind:y,y} or {kind:track,track}.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "view": { "type": "object" }
                },
                "required": ["id", "view"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_add_overlay",
            "Add an overlay to a step. kind is arrow, rect, highlight, callout, or code. Anchors use time_ns or time_frac plus a track, not screen pixels. A code overlay is a file and a 1-based line; it is a pointer, not a measurement.",
            json!({
                "type": "object",
                "properties": {
                    "step_id": { "type": "string" },
                    "overlay": { "type": "object" }
                },
                "required": ["step_id", "overlay"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_remove_overlay",
            "Remove one overlay from a step by step id and overlay id. The other overlays stay.",
            json!({
                "type": "object",
                "properties": {
                    "step_id": { "type": "string" },
                    "overlay_id": { "type": "string" }
                },
                "required": ["step_id", "overlay_id"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_reorder",
            "Move a step to a new index. Later steps shift up.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "index": { "type": "integer" }
                },
                "required": ["id", "index"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_validate",
            "Check the loaded story. ok is false with errors when a step or overlay is not usable. An empty step list is a draft and is ok.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ),
        tool(
            "perfr_story_export",
            "Return the story as JSON. path, when set, also writes that file: a path relative to the story directory (where this server was started, or its --story-dir), and an existing file is replaced only if it is a story. The capture is not modified. perfr-web can store this document beside a capture as captures/<id>.perfr.story and open the viewer with ?capture=<stream>&story=<story url>.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Where to write the JSON, relative to the story directory. Omit to only return it." }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_find_threads",
            "Threads in the current capture whose name, process, pid or tid contains query. Use pid and tid in a story track. This does not start a capture.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "t0_ns": { "type": "integer" },
                    "t1_ns": { "type": "integer" }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_find_scopes",
            "Scopes in a time window whose name contains query, hottest total time first. scope_id is the capture's name id, for a scope highlight. Times are nanoseconds inside the window.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "t0_ns": { "type": "integer" },
                    "t1_ns": { "type": "integer" }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_hotspots",
            "Hottest sampled functions in a window. self and inclusive are sample counts. Use them as what this capture measured. On a machine without sampling the result is supported:false.",
            json!({
                "type": "object",
                "properties": {
                    "t0_ns": { "type": "integer", "description": "Window start. Omit for the whole capture." },
                    "t1_ns": { "type": "integer", "description": "Window end." },
                    "limit": { "type": "integer" },
                    "offset": { "type": "integer" }
                },
                "additionalProperties": false,
            }),
        ),
        tool(
            "perfr_story_cpu",
            "Per-core busy fraction in a window, from scheduling slices. busy is 0 to 1. cores_busy counts cores at or above 0.05. This is a reading of the capture. It does not say how many threads the program meant to run.",
            json!({
                "type": "object",
                "properties": {
                    "t0_ns": { "type": "integer" },
                    "t1_ns": { "type": "integer" }
                },
                "additionalProperties": false,
            }),
        ),
    ]
}

impl Mcp {
    fn story_mut(&self) -> Result<std::sync::MutexGuard<'_, Option<perfr_story::Story>>, String> {
        self.story
            .lock()
            .map_err(|_| "session poisoned".to_string())
    }

    fn story_create(&self, args: &Value) -> Result<Value, String> {
        let mut story = perfr_story::Story::default();
        if let Some(title) = arg_str(args, "title") {
            story.title = title.to_string();
        }
        if let Some(summary) = arg_str(args, "summary") {
            story.summary = summary.to_string();
        }
        if let Some(capture) = arg_str(args, "capture") {
            story.capture = capture.to_string();
        }
        let errors = perfr_story::validate(&story);
        if !errors.is_empty() {
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        *self.story_mut()? = Some(story.clone());
        Ok(story_payload(&story))
    }

    fn story_load(&self, args: &Value) -> Result<Value, String> {
        let text = if let Some(path) = arg_str(args, "path") {
            let file = self.story_file(path)?;
            std::fs::read_to_string(file).map_err(|e| format!("could not read {path}: {e}"))?
        } else if let Some(json) = arg_str(args, "json") {
            json.to_string()
        } else {
            return Err("pass json or path".into());
        };
        let story = perfr_story::parse_story(&text)?;
        *self.story_mut()? = Some(story.clone());
        Ok(story_payload(&story))
    }

    fn story_get(&self) -> Result<Value, String> {
        let slot = self.story_mut()?;
        let story = slot
            .as_ref()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        Ok(story_payload(story))
    }

    fn story_add_step(&self, args: &Value) -> Result<Value, String> {
        let step_json = args.get("step").cloned().ok_or("step is required")?;
        let step: perfr_story::Step =
            serde_json::from_value(step_json).map_err(|e| format!("step: {e}"))?;
        let added = step.id.clone();
        let at = arg_usize(args, "index");
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        story.add_step(step, at)?;
        let errors = perfr_story::validate(story);
        if !errors.is_empty() {
            let _ = story.remove_step(&added);
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        Ok(story_payload(story))
    }

    fn story_update_step(&self, args: &Value) -> Result<Value, String> {
        let id = arg_str(args, "id").ok_or("id is required")?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        let backup = story.clone();
        story.update_step(id, args)?;
        let errors = perfr_story::validate(story);
        if !errors.is_empty() {
            *story = backup;
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        Ok(story_payload(story))
    }

    fn story_remove_step(&self, args: &Value) -> Result<Value, String> {
        let id = arg_str(args, "id").ok_or("id is required")?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        story.remove_step(id)?;
        Ok(story_payload(story))
    }

    fn story_set_view(&self, args: &Value) -> Result<Value, String> {
        let id = arg_str(args, "id").ok_or("id is required")?;
        let view = args.get("view").cloned().ok_or("view is required")?;
        let view: perfr_story::View =
            serde_json::from_value(view).map_err(|e| format!("view: {e}"))?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        let backup = story.clone();
        story.set_view(id, view)?;
        let errors = perfr_story::validate(story);
        if !errors.is_empty() {
            *story = backup;
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        Ok(story_payload(story))
    }

    fn story_add_overlay(&self, args: &Value) -> Result<Value, String> {
        let step_id = arg_str(args, "step_id").ok_or("step_id is required")?;
        let overlay = args.get("overlay").cloned().ok_or("overlay is required")?;
        let overlay: perfr_story::Overlay =
            serde_json::from_value(overlay).map_err(|e| format!("overlay: {e}"))?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        let backup = story.clone();
        story.add_overlay(step_id, overlay)?;
        let errors = perfr_story::validate(story);
        if !errors.is_empty() {
            *story = backup;
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        Ok(story_payload(story))
    }

    fn story_remove_overlay(&self, args: &Value) -> Result<Value, String> {
        let step_id = arg_str(args, "step_id").ok_or("step_id is required")?;
        let overlay_id = arg_str(args, "overlay_id").ok_or("overlay_id is required")?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        story.remove_overlay(step_id, overlay_id)?;
        Ok(story_payload(story))
    }

    fn story_reorder(&self, args: &Value) -> Result<Value, String> {
        let id = arg_str(args, "id").ok_or("id is required")?;
        let index = arg_usize(args, "index").ok_or("index is required")?;
        let mut slot = self.story_mut()?;
        let story = slot
            .as_mut()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        story.reorder(id, index)?;
        Ok(story_payload(story))
    }

    fn story_validate(&self) -> Result<Value, String> {
        let slot = self.story_mut()?;
        let story = slot
            .as_ref()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        let errors = perfr_story::validate(story);
        Ok(json!({
            "ok": errors.is_empty(),
            "errors": errors.iter().map(|e| json!({"path": e.path, "message": e.message})).collect::<Vec<_>>(),
        }))
    }

    fn story_export(&self, args: &Value) -> Result<Value, String> {
        let slot = self.story_mut()?;
        let story = slot
            .as_ref()
            .ok_or("no story loaded; call perfr_story_create or perfr_story_load")?;
        let errors = perfr_story::validate(story);
        if !errors.is_empty() {
            return Err(errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; "));
        }
        let text = story.to_json_pretty()?;
        let mut path_out = Value::Null;
        if let Some(path) = arg_str(args, "path") {
            let file = self.story_file(path)?;
            // The one file this server replaces is a story. Anything else
            // at that name is somebody's source or config.
            match std::fs::read(&file) {
                Ok(existing) if is_story_document(&existing) => {}
                Ok(_) => {
                    return Err(format!(
                        "{path} exists and is not a perfr.story document; it was not overwritten. Export to another name."
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{path} exists and could not be read ({e}); it was not overwritten")),
            }
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&file, &text).map_err(|e| format!("could not write {path}: {e}"))?;
            path_out = json!(path);
        }
        Ok(json!({
            "json": text,
            "bytes": text.len(),
            "path": path_out,
            "viewer": {
                "query": "capture=<stream url>&story=<story url>&chrome=compact",
                "perfr_web": "Store the JSON as captures/<id>.perfr.story next to captures/<id>.perfr.stream. A short link can resolve to those two URLs. This server does not upload it.",
            },
        }))
    }

    fn story_find(&self, args: &Value, scopes: bool) -> Result<Value, String> {
        let body = self.targets(args)?;
        if scopes {
            Ok(json!({
                "t0_ns": body.get("t0_ns").cloned().unwrap_or(Value::Null),
                "t1_ns": body.get("t1_ns").cloned().unwrap_or(Value::Null),
                "scopes": body.get("scopes").cloned().unwrap_or(json!([])),
                "total": body.get("scope_total").cloned().unwrap_or(json!(0)),
                "note": body.get("note").cloned().unwrap_or(Value::Null),
            }))
        } else {
            Ok(json!({
                "threads": body.get("threads").cloned().unwrap_or(json!([])),
                "total": body.get("thread_total").cloned().unwrap_or(json!(0)),
                "note": "pid and tid are the ids a story track uses.",
            }))
        }
    }

    fn story_hotspots(&self, args: &Value) -> Result<Value, String> {
        if !self.platform.sampling {
            return Ok(unsupported_feature("sampling"));
        }
        let mut qs = Vec::new();
        if let Some(t0) = arg_u64(args, "t0_ns") {
            qs.push(format!("start_ns={t0}"));
        }
        if let Some(t1) = arg_u64(args, "t1_ns") {
            qs.push(format!("end_ns={t1}"));
        }
        let path = if qs.is_empty() {
            "/api/sampling/report".to_string()
        } else {
            format!("/api/sampling/report?{}", qs.join("&"))
        };
        match self.http.get_raw(&path, Duration::from_secs(60)) {
            Ok(raw) if raw.status == 501 => Ok(unsupported_feature("sampling")),
            Ok(raw) => {
                let body = parse_body(&raw)?;
                let mut shaped = shape_hotspots(&body, args);
                if let Some(obj) = shaped.as_object_mut() {
                    obj.insert(
                        "note".into(),
                        json!(
                            "self and inclusive are sample counts in this window of this capture."
                        ),
                    );
                }
                Ok(shaped)
            }
            Err(message) => Err(message),
        }
    }

    fn story_cpu(&self, args: &Value) -> Result<Value, String> {
        let body = self.targets(args)?;
        Ok(json!({
            "t0_ns": body.get("t0_ns").cloned().unwrap_or(Value::Null),
            "t1_ns": body.get("t1_ns").cloned().unwrap_or(Value::Null),
            "cores": body.get("cores").cloned().unwrap_or(json!([])),
            "core_count": body.get("core_count").cloned().unwrap_or(json!(0)),
            "cores_busy": body.get("cores_busy").cloned().unwrap_or(json!(0)),
            "busy_at": body.get("busy_at").cloned().unwrap_or(json!(0.05)),
            "note": body.get("note").cloned().unwrap_or(Value::Null),
        }))
    }

    fn targets(&self, args: &Value) -> Result<Value, String> {
        let mut qs = Vec::new();
        if let Some(q) = arg_str(args, "query") {
            if !q.is_empty() {
                qs.push(format!("q={}", urlencode(q)));
            }
        }
        if let Some(t0) = arg_u64(args, "t0_ns") {
            qs.push(format!("t0={t0}"));
        }
        if let Some(t1) = arg_u64(args, "t1_ns") {
            qs.push(format!("t1={t1}"));
        }
        let path = if qs.is_empty() {
            "/api/capture/targets".to_string()
        } else {
            format!("/api/capture/targets?{}", qs.join("&"))
        };
        match self.http.get_raw(&path, Duration::from_secs(30)) {
            Ok(raw) if raw.status == 404 => {
                Err("this perfr has no /api/capture/targets. Update the service that is serving the capture.".into())
            }
            Ok(raw) => parse_body(&raw),
            Err(message) => Err(message),
        }
    }
}

/// `path`, taken relative to `dir` (canonical), as a file that is inside
/// `dir` once every symlink on the way is followed. Says why not otherwise.
fn file_inside(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let mut names = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => names.push(name),
            Component::CurDir => {}
            // Resolved here, on the names, so that what is checked below is
            // the path that is then opened.
            Component::ParentDir => {
                if names.pop().is_none() {
                    return Err("is outside the story directory".into());
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err("is an absolute path".into()),
        }
    }
    if names.is_empty() {
        return Err("names no file".into());
    }
    let mut file = dir.to_path_buf();
    for (index, name) in names.iter().enumerate() {
        file.push(name);
        match std::fs::symlink_metadata(&file) {
            // `symlink_metadata`, not `exists`: a dangling symlink does not
            // exist, and a write through it creates the file it names.
            Ok(_) => {
                let real = std::fs::canonicalize(&file).map_err(|e| format!("cannot be resolved ({e})"))?;
                if !real.starts_with(dir) {
                    return Err("leads outside the story directory".into());
                }
                file = real;
            }
            // Nothing here, so nothing below it either: the rest is created
            // as plain directories and a plain file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                file.extend(&names[index + 1..]);
                break;
            }
            Err(e) => return Err(format!("cannot be resolved ({e})")),
        }
    }
    Ok(file)
}

/// Whether a file's content is a story, by the marker every story carries.
/// Not `parse_story`: a story with a step someone broke by hand, or one
/// from another version of the format, is still a story to replace.
fn is_story_document(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .is_ok_and(|doc| doc.get("format").and_then(|f| f.as_str()) == Some(perfr_story::FORMAT))
}

fn story_payload(story: &perfr_story::Story) -> Value {
    json!({
        "format": story.format,
        "version": story.version,
        "title": story.title,
        "summary": story.summary,
        "capture": story.capture,
        "steps": story.steps.iter().map(|step| json!({
            "id": step.id,
            "kind": step.kind,
            "title": step.title,
            "duration_ms": step.duration_ms,
            "overlays": step.overlays.len(),
        })).collect::<Vec<_>>(),
        "story": serde_json::to_value(story).unwrap_or(Value::Null),
    })
}

fn shape_hotspots(body: &Value, args: &Value) -> Value {
    let limit = page_limit(args, 15, 40);
    let offset = page_offset(args);
    let functions = body
        .get("functions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let total = functions.len();
    let hotspots: Vec<Value> = functions
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|row| {
            let id = row.get("function_id").and_then(json_u64).unwrap_or(0);
            json!({
                "name": clip(row.get("name").and_then(|v| v.as_str()).unwrap_or("")),
                "module": basename(row.get("module").and_then(|v| v.as_str()).unwrap_or("")),
                "function_id": if id == 0 { Value::Null } else { json!(id) },
                "self": row.get("self").and_then(|v| v.as_u64()).unwrap_or(0),
                "inclusive": row.get("inclusive").and_then(|v| v.as_u64()).unwrap_or(0),
                "self_percent": round1(row.get("self_percent").and_then(|v| v.as_f64()).unwrap_or(0.0)),
                "inclusive_percent": round1(row.get("inclusive_percent").and_then(|v| v.as_f64()).unwrap_or(0.0)),
            })
        })
        .collect();
    json!({
        "supported": true,
        "samples": body.get("samples").and_then(|v| v.as_u64()).unwrap_or(0),
        "total_functions": total,
        "offset": offset,
        "limit": limit,
        "hotspots": hotspots,
    })
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

fn page_limit(args: &Value, default: usize, max: usize) -> usize {
    arg_usize(args, "limit").unwrap_or(default).clamp(1, max)
}

fn page_offset(args: &Value) -> usize {
    arg_usize(args, "offset").unwrap_or(0)
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    json_u64(args.get(key)?).and_then(|n| u32::try_from(n).ok())
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(json_u64)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    json_u64(args.get(key)?).map(|n| n as usize)
}

fn arg_f64(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|n| n as f64)))
}

fn arg_u64s(args: &Value, key: &str) -> Vec<u64> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(json_u64).collect())
        .unwrap_or_default()
}

fn arg_strs(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64))
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(path)
}

fn clip(text: &str) -> String {
    clip_len(text, 160)
}

fn clip_len(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

struct RawHttp {
    status: u16,
    body: String,
}

struct HttpClient {
    host: String,
    port: u16,
    display: String,
}

impl HttpClient {
    fn parse(url: &str, allow_remote: bool) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("perfr URL must be http:// (got {url})"))?;
        let rest = rest.trim_end_matches('/');
        if rest.contains('/') {
            return Err("the service URL must not include a path".into());
        }
        let (host, port) = if let Some(host) = rest.strip_prefix('[') {
            let (host, port) = host.split_once("]:").ok_or("IPv6 URL is http://[::1]:port")?;
            (host.to_string(), port.parse::<u16>().map_err(|_| "bad port".to_string())?)
        } else {
            let (host, port) = rest.split_once(':').ok_or("URL is http://127.0.0.1:44766")?;
            (host.to_string(), port.parse::<u16>().map_err(|_| "bad port".to_string())?)
        };
        if host.is_empty() || port == 0 {
            return Err("URL needs a host and a port".into());
        }
        let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1");
        if !loopback && !allow_remote {
            return Err(format!(
                "{host} is not loopback. Refusing to attach a profiler with no authentication. \
                 Pass --allow-remote only if you accept that anyone who can reach the service can attach to processes."
            ));
        }
        Ok(Self { display: format!("http://{host}:{port}"), host, port })
    }

    fn get_json(&self, path: &str, timeout: Duration) -> Result<Value, String> {
        parse_body(&self.get_raw(path, timeout)?)
    }

    fn get_raw(&self, path: &str, timeout: Duration) -> Result<RawHttp, String> {
        self.exchange("GET", path, None, timeout)
    }

    fn post_ok(&self, path: &str, body: &Value, timeout: Duration) -> Result<(), String> {
        let raw = self.exchange("POST", path, Some(body.to_string().into_bytes()), timeout)?;
        if (200..300).contains(&raw.status) {
            Ok(())
        } else {
            Err(format!("HTTP {} {}", raw.status, clip_len(raw.body.trim(), 400)))
        }
    }

    fn exchange(&self, method: &str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<RawHttp, String> {
        let connect_host = if self.host == "localhost" { "127.0.0.1" } else { self.host.as_str() };
        let addr = (connect_host, self.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}:{}: {e}", self.host, self.port))?
            .next()
            .ok_or_else(|| format!("no address for {}", self.host))?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| {
            format!(
                "perfr is not reachable at {} ({e}). Start it with `perfr --host 127.0.0.1`.",
                self.display
            )
        })?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.host, self.port
        );
        if let Some(body) = &body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
        if let Some(body) = &body {
            stream.write_all(body).map_err(|e| e.to_string())?;
        }
        read_response(&mut stream).map_err(|e| {
            if e.contains("timed out") {
                format!("{e} while calling {path}")
            } else {
                e
            }
        })
    }
}

fn parse_body(raw: &RawHttp) -> Result<Value, String> {
    if !(200..300).contains(&raw.status) {
        return Err(format!("HTTP {} {}", raw.status, clip_len(raw.body.trim(), 400)));
    }
    let text = raw.body.trim();
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).map_err(|e| format!("bad JSON from perfr: {e}"))
}

fn read_response(stream: &mut TcpStream) -> Result<RawHttp, String> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("perfr closed the connection before a response".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err("response headers too large".into());
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let mut content_length = None;
    let mut chunked = false;
    for line in header_text.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        }
        if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    if chunked {
        while !chunked_complete(&body) {
            let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > 32 * 1024 * 1024 {
                return Err("response larger than 32MB; narrow the request".into());
            }
        }
        let decoded = decode_chunks(&body, RESPONSE_CAP)?.ok_or("truncated chunk")?;
        return Ok(RawHttp { status, body: String::from_utf8_lossy(&decoded).into_owned() });
    }
    if let Some(len) = content_length {
        while body.len() < len {
            let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > 32 * 1024 * 1024 {
                return Err("response larger than 32MB; narrow the request".into());
            }
        }
        body.truncate(len);
    } else {
        loop {
            let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > 32 * 1024 * 1024 {
                return Err("response larger than 32MB; narrow the request".into());
            }
        }
    }
    Ok(RawHttp { status, body: String::from_utf8_lossy(&body).into_owned() })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn chunked_complete(buf: &[u8]) -> bool {
    matches!(decode_chunks(buf, RESPONSE_CAP), Ok(Some(_))) && buf.windows(5).any(|w| w == b"0\r\n\r\n")
}

/// The most the client reads of one reply from the service.
const RESPONSE_CAP: usize = 32 * 1024 * 1024;

/// The body of a chunked message: `None` while its last chunk has not
/// arrived, an error when what is there is not chunked framing or announces
/// more than `limit` bytes. Used on the service's replies and on requests to
/// the HTTP transport, so the sizes are whatever the other end wrote.
pub(super) fn decode_chunks(mut buf: &[u8], limit: usize) -> Result<Option<Vec<u8>>, String> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = buf.windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let line = std::str::from_utf8(&buf[..line_end]).map_err(|_| "chunk size is not utf-8".to_string())?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("bad chunk size {line}"))?;
        buf = &buf[line_end + 2..];
        if size == 0 {
            return Ok(Some(out));
        }
        // Refused on the announcement, not waited for. And compared by
        // subtraction: a size near the top of the range would wrap an
        // addition and then slice past the buffer.
        if size > limit - out.len().min(limit) {
            return Err(format!("chunked body larger than {limit} bytes"));
        }
        if buf.len() < 2 || buf.len() - 2 < size {
            return Ok(None);
        }
        out.extend_from_slice(&buf[..size]);
        buf = &buf[size + 2..];
    }
}
