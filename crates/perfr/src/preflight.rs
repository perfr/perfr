// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Permission preflight for sampling and dynamic instrumentation.
//!
//! The service still starts when a feature is missing: the viewer and manual
//! scopes need none of this. The warning names the feature, the current
//! setting, and the command that would change it. `perfr doctor` prints the
//! whole table. `perfr setup` applies the sysctls and the setcap after
//! confirmation.
//!
//! Linux thresholds match `privileges` and docs/manual/troubleshooting.md.
//! Uprobes need `CAP_SYS_ADMIN` even when `perf_event_paranoid` is already
//! low. Tracefs is the id file the scheduler tracepoints read. On Windows
//! the kernel logger needs an elevated token. On macOS this build does not
//! sample; Frida attach needs a task port.

use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::Command;

pub const SETUP_HINT: &str =
    "Run `perfr setup` or ask your agent to use the perfr-setup skill to fix this.";
const SYSCTL_FILE: &str = "/etc/sysctl.d/99-perfr-perf.conf";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OsKind {
    Linux,
    Macos,
    Windows,
    Other,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub os: OsKind,
    pub binary: String,
    pub paranoid: Option<i32>,
    pub kptr_restrict: Option<i32>,
    pub ptrace_scope: Option<i32>,
    pub is_root: bool,
    pub cap_perfmon: bool,
    pub cap_sys_admin: bool,
    pub cap_sys_ptrace: bool,
    pub cap_syslog: bool,
    pub tracefs: Option<String>,
    pub uprobe_pmu: bool,
    pub debugger_entitlement: bool,
    pub sip_enabled: Option<bool>,
    pub windows_elevated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Degraded,
    Unavailable,
    NotApplicable,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Unavailable => "unavailable",
            Status::NotApplicable => "n/a",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub label: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
    /// Launch warning. Passing and not-in-this-build rows stay in `doctor`.
    pub warn: bool,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub checks: Vec<Check>,
    pub os: OsKind,
    pub binary: String,
}

impl Report {
    pub fn blocks(&self) -> bool {
        let usable = self.checks.iter().any(|check| {
            (check.label == "viewer" || check.label == "manual scopes")
                && check.status == Status::Ok
        });
        !usable
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub sysctls: Vec<(&'static str, i32)>,
    pub setcap: Option<String>,
    pub notes: Vec<String>,
}

pub struct SetupOpts {
    pub check_only: bool,
    pub yes: bool,
    pub persist: bool,
}

pub fn suppressed(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--no-preflight") || env_suppressed()
}

pub fn env_suppressed() -> bool {
    match std::env::var("PERFR_NO_PREFLIGHT") {
        Ok(value) => !matches!(value.as_str(), "" | "0" | "false" | "no" | "off"),
        Err(_) => false,
    }
}

pub fn emit_warning() {
    let report = inspect_host();
    if let Some(text) = render_warning(&report, color_stderr()) {
        eprint!("{text}");
    }
    if report.blocks() {
        eprintln!("perfr: stopping because nothing can be recorded on this machine.");
        std::process::exit(2);
    }
}

pub fn doctor() -> i32 {
    print!(
        "{}",
        render_doctor(&inspect_host(), color_stdout(), "perfr doctor")
    );
    0
}

pub fn setup(opts: SetupOpts) -> i32 {
    let snap = probe_host();
    let report = evaluate(&snap);
    let interactive = !opts.yes && !opts.check_only && stdin_is_tty();
    let mut talk = StdTalk;
    let mut runner = SystemRunner;
    run_setup(&snap, &report, &opts, interactive, &mut talk, &mut runner)
}

pub fn inspect_host() -> Report {
    evaluate(&probe_host())
}

pub fn probe_host() -> Snapshot {
    let binary = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "perfr".into());
    #[cfg(target_os = "linux")]
    {
        linux_probe(binary)
    }
    #[cfg(target_os = "macos")]
    {
        macos_probe(binary)
    }
    #[cfg(target_os = "windows")]
    {
        windows_probe(binary)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Snapshot::bare(OsKind::Other, binary)
    }
}

pub fn evaluate(snap: &Snapshot) -> Report {
    let mut checks = Vec::new();
    match snap.os {
        OsKind::Linux => linux_checks(snap, &mut checks),
        OsKind::Macos => macos_checks(snap, &mut checks),
        OsKind::Windows => windows_checks(snap, &mut checks),
        OsKind::Other => {
            checks.push(row(
                "viewer",
                Status::Unavailable,
                "this build has no supported platform",
                None,
                false,
            ));
            checks.push(row(
                "manual scopes",
                Status::Unavailable,
                "this build has no supported platform",
                None,
                false,
            ));
        }
    }
    Report {
        checks,
        os: snap.os,
        binary: snap.binary.clone(),
    }
}

pub fn plan_for(snap: &Snapshot) -> Plan {
    let mut plan = Plan {
        sysctls: Vec::new(),
        setcap: None,
        notes: Vec::new(),
    };
    match snap.os {
        OsKind::Linux => {
            let bypass = perf_bypass(snap);
            if snap.paranoid.is_some_and(|value| value > 0 && !bypass) {
                plan.sysctls.push(("kernel.perf_event_paranoid", 0));
            }
            if kptr_degraded(snap) {
                plan.sysctls.push(("kernel.kptr_restrict", 0));
            }
            if ptrace_degraded(snap) {
                plan.sysctls.push(("kernel.yama.ptrace_scope", 0));
            }
            if snap.uprobe_pmu && !snap.is_root && !snap.cap_sys_admin {
                plan.setcap = Some("cap_sys_admin,cap_perfmon,cap_dac_read_search+ep".into());
            } else if snap.tracefs.is_none() && !bypass {
                plan.setcap = Some("cap_perfmon,cap_sys_ptrace+ep".into());
            }
            if !snap.uprobe_pmu {
                plan.notes.push(
                    "this kernel has no uprobe PMU (CONFIG_UPROBE_EVENTS). Use Frida for hooks; setup cannot add the PMU.".into(),
                );
            }
        }
        OsKind::Macos => {
            if !snap.is_root && !snap.debugger_entitlement {
                plan.notes.push(format!(
                    "Frida attach needs a task port. Run `sudo {}`. SIP and the hardened runtime can still refuse system processes. This build does not sample through dtrace.",
                    snap.binary
                ));
            }
        }
        OsKind::Windows => {
            if !snap.windows_elevated {
                plan.notes.push(
                    "Sampling and the scheduler need the kernel logger. Open a terminal with Run as administrator and start perfr there. A normal prompt of an administrator account is not elevated. Manual scopes and Frida hooks need no privilege.".into(),
                );
            }
        }
        OsKind::Other => {
            plan.notes
                .push("no permission changes apply on this platform.".into());
        }
    }
    plan
}

fn linux_checks(snap: &Snapshot, checks: &mut Vec<Check>) {
    checks.push(row(
        "viewer",
        Status::Ok,
        "served on this machine",
        None,
        false,
    ));
    checks.push(row(
        "manual scopes",
        Status::Ok,
        "no privilege required",
        None,
        false,
    ));
    let bypass = perf_bypass(snap);
    let paranoid = snap
        .paranoid
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unreadable".into());
    checks.push(row(
        "perf_event_paranoid",
        if snap.paranoid.is_none() || bypass || snap.paranoid.unwrap_or(2) <= 0 {
            Status::Ok
        } else {
            Status::Degraded
        },
        format!("kernel.perf_event_paranoid = {paranoid}"),
        (!bypass && snap.paranoid.is_some_and(|value| value > 0))
            .then(|| sysctl_fix("kernel.perf_event_paranoid", 0)),
        false,
    ));
    checks.push(row(
        "capabilities",
        Status::Ok,
        format!(
            "root={} CAP_PERFMON={} CAP_SYS_ADMIN={} CAP_SYS_PTRACE={}",
            yes(snap.is_root),
            yes(snap.cap_perfmon),
            yes(snap.cap_sys_admin),
            yes(snap.cap_sys_ptrace)
        ),
        None,
        false,
    ));
    let sampling_ok = bypass || snap.paranoid.unwrap_or(2) <= 2;
    checks.push(row(
        "sampling",
        if sampling_ok { Status::Ok } else { Status::Degraded },
        if sampling_ok {
            format!("kernel.perf_event_paranoid = {paranoid}")
        } else {
            format!("kernel.perf_event_paranoid = {paranoid}; needs <= 2, or CAP_PERFMON / CAP_SYS_ADMIN / root")
        },
        (!sampling_ok).then(|| sysctl_fix("kernel.perf_event_paranoid", 0) + &format!("\nor: sudo setcap cap_perfmon,cap_sys_ptrace+ep {}", snap.binary)),
        !sampling_ok,
    ));
    let scheduler_ok = bypass || snap.paranoid.unwrap_or(2) <= 0;
    checks.push(row(
        "scheduler",
        if scheduler_ok { Status::Ok } else { Status::Degraded },
        if scheduler_ok {
            "context switches allowed".into()
        } else {
            format!("kernel.perf_event_paranoid = {paranoid}; system-wide events need <= 0, or CAP_PERFMON / CAP_SYS_ADMIN / root")
        },
        (!scheduler_ok).then(|| sysctl_fix("kernel.perf_event_paranoid", 0)),
        !scheduler_ok,
    ));
    let kptr_bad = kptr_degraded(snap);
    checks.push(row(
        "kernel symbols",
        if kptr_bad {
            Status::Degraded
        } else {
            Status::Ok
        },
        match snap.kptr_restrict {
            Some(value) if kptr_bad => {
                format!("kernel.kptr_restrict = {value}; kernel addresses are hidden")
            }
            Some(value) => format!("kernel.kptr_restrict = {value}"),
            None => "kernel.kptr_restrict unreadable".into(),
        },
        kptr_bad.then(|| sysctl_fix("kernel.kptr_restrict", 0)),
        kptr_bad,
    ));
    let trace_bad = snap.tracefs.is_none() && !bypass;
    checks.push(row(
        "tracefs",
        if snap.tracefs.is_some() || bypass { Status::Ok } else { Status::Degraded },
        match &snap.tracefs {
            Some(path) => format!("readable at {path}"),
            None if bypass => "id file not readable; CAP_PERFMON or root is held, so the kernel still decides".into(),
            None => "sched_switch id is not readable in /sys/kernel/tracing or /sys/kernel/debug/tracing".into(),
        },
        trace_bad.then(|| format!("sudo setcap cap_perfmon+ep {}", snap.binary)),
        trace_bad,
    ));
    let (uprobe_status, uprobe_detail, uprobe_fix, uprobe_warn) = if !snap.uprobe_pmu {
        (
            Status::Unavailable,
            "this kernel has no uprobe PMU (CONFIG_UPROBE_EVENTS). Frida does not need it"
                .to_string(),
            Some("Switch hooks to Frida. No sysctl adds the uprobe PMU.".to_string()),
            true,
        )
    } else if snap.is_root || snap.cap_sys_admin {
        (Status::Ok, "uprobe PMU present".to_string(), None, false)
    } else {
        (
            Status::Degraded,
            "uprobe PMU present; arming it needs CAP_SYS_ADMIN. Frida does not".to_string(),
            Some(format!(
                "sudo setcap cap_sys_admin,cap_perfmon,cap_dac_read_search+ep {}",
                snap.binary
            )),
            true,
        )
    };
    checks.push(row(
        "uprobes",
        uprobe_status,
        uprobe_detail,
        uprobe_fix,
        uprobe_warn,
    ));
    let ptrace_bad = ptrace_degraded(snap);
    checks.push(row(
        "dynamic instrumentation",
        if ptrace_bad {
            Status::Degraded
        } else {
            Status::Ok
        },
        match snap.ptrace_scope {
            Some(value) if ptrace_bad => format!(
                "kernel.yama.ptrace_scope = {value}; attach to a running process is restricted"
            ),
            Some(value) => format!("kernel.yama.ptrace_scope = {value}"),
            None => "kernel.yama.ptrace_scope unreadable".into(),
        },
        ptrace_bad.then(|| sysctl_fix("kernel.yama.ptrace_scope", 0)),
        ptrace_bad,
    ));
}

fn macos_checks(snap: &Snapshot, checks: &mut Vec<Check>) {
    checks.push(row(
        "viewer",
        Status::Ok,
        "served on this machine",
        None,
        false,
    ));
    checks.push(row(
        "manual scopes",
        Status::Ok,
        "no privilege required",
        None,
        false,
    ));
    checks.push(row(
        "sampling",
        Status::NotApplicable,
        "not in this build",
        None,
        false,
    ));
    checks.push(row(
        "scheduler",
        Status::NotApplicable,
        "not in this build",
        None,
        false,
    ));
    checks.push(row(
        "uprobes",
        Status::NotApplicable,
        "not in this build",
        None,
        false,
    ));
    let sip = match snap.sip_enabled {
        Some(true) => "SIP enabled",
        Some(false) => "SIP disabled",
        None => "SIP status unreadable",
    };
    checks.push(row(
        "dtrace",
        Status::NotApplicable,
        format!("{sip}; this build does not sample through dtrace"),
        None,
        false,
    ));
    let attach_ok = snap.is_root || snap.debugger_entitlement;
    checks.push(row(
        "dynamic instrumentation",
        if attach_ok {
            Status::Ok
        } else {
            Status::Degraded
        },
        if attach_ok {
            "task_for_pid is available to root or a debugger entitlement".into()
        } else {
            format!(
                "not root and {snap_binary_entitlement}; Frida attach needs a task port. {sip}",
                snap_binary_entitlement = "no debugger entitlement"
            )
        },
        (!attach_ok).then(|| format!("sudo {}", snap.binary)),
        !attach_ok,
    ));
}

fn windows_checks(snap: &Snapshot, checks: &mut Vec<Check>) {
    checks.push(row(
        "viewer",
        Status::Ok,
        "served on this machine",
        None,
        false,
    ));
    checks.push(row(
        "manual scopes",
        Status::Ok,
        "no privilege required",
        None,
        false,
    ));
    checks.push(row(
        "dynamic instrumentation",
        Status::Ok,
        "Frida hooks need no privilege for a same-user target",
        None,
        false,
    ));
    checks.push(row(
        "uprobes",
        Status::NotApplicable,
        "not in this build",
        None,
        false,
    ));
    let elevated = snap.windows_elevated;
    let fix = (!elevated).then(|| {
        "Open a terminal with Run as administrator and start perfr there. A UAC-filtered token is not elevated.".into()
    });
    checks.push(row(
        "sampling",
        if elevated {
            Status::Ok
        } else {
            Status::Degraded
        },
        if elevated {
            "elevated token".to_string()
        } else {
            "the kernel logger needs an elevated token".to_string()
        },
        fix.clone(),
        !elevated,
    ));
    checks.push(row(
        "scheduler",
        if elevated {
            Status::Ok
        } else {
            Status::Degraded
        },
        if elevated {
            "elevated token".to_string()
        } else {
            "the kernel logger needs an elevated token".to_string()
        },
        fix,
        !elevated,
    ));
}

fn row(
    label: &str,
    status: Status,
    detail: impl Into<String>,
    fix: Option<String>,
    warn: bool,
) -> Check {
    Check {
        label: label.into(),
        status,
        detail: detail.into(),
        fix,
        warn,
    }
}

fn perf_bypass(snap: &Snapshot) -> bool {
    snap.is_root || snap.cap_perfmon || snap.cap_sys_admin
}

fn kptr_degraded(snap: &Snapshot) -> bool {
    match snap.kptr_restrict {
        None | Some(0) => false,
        Some(1) => !(snap.is_root || snap.cap_syslog),
        Some(_) => !snap.cap_syslog,
    }
}

fn ptrace_degraded(snap: &Snapshot) -> bool {
    match snap.ptrace_scope {
        None | Some(0) => false,
        Some(1 | 2) => !(snap.is_root || snap.cap_sys_ptrace),
        Some(_) => true,
    }
}

fn yes(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn sysctl_fix(key: &str, value: i32) -> String {
    format!("sudo sysctl -w {key}={value}\npersist: echo '{key}={value}' | sudo tee {SYSCTL_FILE}")
}

pub fn render_warning(report: &Report, color: bool) -> Option<String> {
    let items: Vec<&Check> = report.checks.iter().filter(|check| check.warn).collect();
    if items.is_empty() {
        return None;
    }
    let mut body = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            body.push(String::new());
        }
        body.push(format!("{}  {}", item.label, item.status.as_str()));
        body.push(format!("  {}", item.detail));
        if let Some(fix) = &item.fix {
            for (line_index, line) in fix.lines().enumerate() {
                if line_index == 0 {
                    body.push(format!("  fix: {line}"));
                } else {
                    body.push(format!("  {line}"));
                }
            }
        }
    }
    body.push(String::new());
    body.push(SETUP_HINT.to_string());
    if !report.blocks() {
        body.push("The viewer and manual scopes still work.".into());
    }
    Some(frame("perfr: reduced capture", &body, color))
}

pub fn render_doctor(report: &Report, color: bool, title: &str) -> String {
    let platform = match report.os {
        OsKind::Linux => "linux",
        OsKind::Macos => "macos",
        OsKind::Windows => "windows",
        OsKind::Other => "other",
    };
    let mut lines = vec![
        title.to_string(),
        String::new(),
        format!("binary: {}", report.binary),
        format!("platform: {platform}"),
        String::new(),
        format!("{:<28} {:<12} detail", "check", "status"),
    ];
    for check in &report.checks {
        let status = paint(
            check.status.as_str(),
            color && check.status != Status::Ok && check.status != Status::NotApplicable,
        );
        lines.push(format!(
            "{:<28} {:<12} {}",
            check.label, status, check.detail
        ));
        if let Some(fix) = &check.fix {
            for line in fix.lines() {
                lines.push(format!("    {line}"));
            }
        }
    }
    if report.checks.iter().any(|check| check.warn) {
        lines.push(String::new());
        lines.push(SETUP_HINT.to_string());
    }
    lines.push(String::new());
    lines.join("\n")
}

fn frame(title: &str, lines: &[String], color: bool) -> String {
    let width = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 2);
    let mut out = String::new();
    let dashes = "─".repeat(width - title.chars().count());
    out.push_str(&format!("┌ {title} {dashes}┐\n"));
    for line in lines {
        let pad = width - line.chars().count();
        out.push_str(&format!("│ {line}{} │\n", " ".repeat(pad)));
    }
    out.push_str(&format!("└{}┘\n", "─".repeat(width + 2)));
    if color {
        format!("\x1b[33;1m{out}\x1b[0m")
    } else {
        out
    }
}

fn paint(text: &str, color: bool) -> String {
    if color {
        format!("\x1b[33;1m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub trait Talk {
    fn say(&mut self, line: &str);
    fn confirm(&mut self, prompt: &str) -> bool;
}

pub trait Runner {
    fn run(&mut self, argv: &[String], stdin: Option<&[u8]>) -> Result<(), String>;
}

pub fn run_setup(
    snap: &Snapshot,
    report: &Report,
    opts: &SetupOpts,
    interactive: bool,
    talk: &mut dyn Talk,
    runner: &mut dyn Runner,
) -> i32 {
    let title = if opts.check_only {
        "perfr setup --check"
    } else {
        "perfr setup"
    };
    talk.say(&render_doctor(report, false, title));
    if opts.check_only {
        return 0;
    }
    let plan = plan_for(snap);
    if plan.sysctls.is_empty() && plan.setcap.is_none() && plan.notes.is_empty() {
        talk.say("nothing to change.");
        return 0;
    }
    for note in &plan.notes {
        talk.say(note);
    }
    if plan.sysctls.is_empty() && plan.setcap.is_none() {
        return 0;
    }
    if !opts.yes && !interactive {
        talk.say("re-run with --yes to apply the runtime changes. Add --persist to write /etc/sysctl.d/99-perfr-perf.conf.");
        for (key, value) in &plan.sysctls {
            talk.say(&format!("sudo sysctl -w {key}={value}"));
        }
        if let Some(caps) = &plan.setcap {
            talk.say(&format!("sudo setcap {caps} {}", snap.binary));
        }
        return 0;
    }
    if !plan.sysctls.is_empty() {
        let apply = opts.yes || talk.confirm("Apply these sysctl changes now?");
        if apply {
            for (key, value) in &plan.sysctls {
                let argv = with_sudo(
                    snap,
                    vec!["sysctl".into(), "-w".into(), format!("{key}={value}")],
                );
                if let Err(error) = runner.run(&argv, None) {
                    talk.say(&format!("failed: {error}"));
                    return 2;
                }
                talk.say(&format!("applied {key}={value}"));
            }
        }
        let persist = opts.persist
            || (interactive
                && talk.confirm(&format!("Write {SYSCTL_FILE} so this survives reboot?")));
        if persist {
            let body = plan
                .sysctls
                .iter()
                .map(|(key, value)| format!("{key}={value}\n"))
                .collect::<String>();
            let argv = with_sudo(snap, vec!["tee".into(), SYSCTL_FILE.into()]);
            if let Err(error) = runner.run(&argv, Some(body.as_bytes())) {
                talk.say(&format!("failed: {error}"));
                return 2;
            }
            talk.say(&format!("wrote {SYSCTL_FILE}"));
        }
    }
    if let Some(caps) = &plan.setcap {
        let prompt = if caps.contains("cap_sys_admin") {
            format!(
                "Grant {caps} to {}? CAP_SYS_ADMIN is root in all but name.",
                snap.binary
            )
        } else {
            format!("Grant {caps} to {}?", snap.binary)
        };
        if opts.yes || talk.confirm(&prompt) {
            if caps.contains("cap_sys_admin") && opts.yes {
                talk.say("CAP_SYS_ADMIN is root in all but name.");
            }
            let argv = with_sudo(
                snap,
                vec!["setcap".into(), caps.clone(), snap.binary.clone()],
            );
            if let Err(error) = runner.run(&argv, None) {
                talk.say(&format!("failed: {error}"));
                return 2;
            }
            talk.say(&format!("setcap {caps}"));
        }
    }
    0
}

fn with_sudo(snap: &Snapshot, args: Vec<String>) -> Vec<String> {
    if snap.is_root {
        args
    } else {
        let mut command = vec!["sudo".into()];
        command.extend(args);
        command
    }
}

struct StdTalk;

impl Talk for StdTalk {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }
    fn confirm(&mut self, prompt: &str) -> bool {
        print!("{prompt} [y/N] ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
    }
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, argv: &[String], stdin_bytes: Option<&[u8]>) -> Result<(), String> {
        if argv.is_empty() {
            return Err("empty command".into());
        }
        let mut child = Command::new(&argv[0]);
        child.args(&argv[1..]);
        if stdin_bytes.is_some() {
            child.stdin(std::process::Stdio::piped());
        }
        let mut child = child
            .spawn()
            .map_err(|error| format!("{}: {error}", argv[0]))?;
        if let Some(bytes) = stdin_bytes {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(bytes).map_err(|error| error.to_string())?;
            }
        }
        let status = child.wait().map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{} exited {status}", argv[0]))
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_probe(binary: String) -> Snapshot {
    let access = crate::privileges::probe();
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mask = crate::privileges::parse_cap_eff(&status).unwrap_or(0);
    let mut snap = Snapshot::bare(OsKind::Linux, binary);
    snap.paranoid = access.paranoid;
    snap.is_root = access.is_root;
    snap.cap_perfmon = access.has_cap_perfmon;
    snap.cap_sys_admin = access.has_cap_sys_admin;
    snap.cap_sys_ptrace = access.has_cap_sys_ptrace;
    snap.cap_syslog = mask & (1u64 << 34) != 0;
    snap.kptr_restrict = read_i32("/proc/sys/kernel/kptr_restrict");
    snap.ptrace_scope = read_i32("/proc/sys/kernel/yama/ptrace_scope");
    snap.tracefs = readable_tracefs();
    snap.uprobe_pmu = uprobe_pmu();
    snap
}

#[cfg(target_os = "macos")]
fn macos_probe(binary: String) -> Snapshot {
    let mut snap = Snapshot::bare(OsKind::Macos, binary);
    snap.is_root = unsafe { libc::geteuid() } == 0;
    snap.debugger_entitlement = debugger_entitlement();
    snap.sip_enabled = sip_enabled();
    snap
}

#[cfg(target_os = "windows")]
fn windows_probe(binary: String) -> Snapshot {
    let mut snap = Snapshot::bare(OsKind::Windows, binary);
    snap.windows_elevated = perfr_etw::is_elevated();
    snap
}

impl Snapshot {
    fn bare(os: OsKind, binary: String) -> Self {
        Self {
            os,
            binary,
            paranoid: None,
            kptr_restrict: None,
            ptrace_scope: None,
            is_root: false,
            cap_perfmon: false,
            cap_sys_admin: false,
            cap_sys_ptrace: false,
            cap_syslog: false,
            tracefs: None,
            uprobe_pmu: false,
            debugger_entitlement: false,
            sip_enabled: None,
            windows_elevated: false,
        }
    }
}

#[cfg(target_os = "linux")]
fn read_i32(path: &str) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

#[cfg(target_os = "linux")]
fn readable_tracefs() -> Option<String> {
    for root in ["/sys/kernel/tracing", "/sys/kernel/debug/tracing"] {
        let path = format!("{root}/events/sched/sched_switch/id");
        if std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
            .is_some()
        {
            return Some(root.to_string());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn uprobe_pmu() -> bool {
    Path::new("/sys/bus/event_source/devices/uprobe").exists()
        || Path::new("/sys/kernel/tracing/uprobe_events").exists()
        || Path::new("/sys/kernel/debug/tracing/uprobe_events").exists()
}

#[cfg(target_os = "macos")]
fn debugger_entitlement() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let Ok(output) = Command::new("codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(&exe)
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("com.apple.security.cs.debugger") || text.contains("task_for_pid-allow")
}

#[cfg(target_os = "macos")]
fn sip_enabled() -> Option<bool> {
    let output = Command::new("csrutil").arg("status").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if text.contains("disabled") {
        Some(false)
    } else if text.contains("enabled") {
        Some(true)
    } else {
        None
    }
}

fn color_stderr() -> bool {
    std::env::var_os("NO_COLOR").is_none() && stderr_is_tty()
}

fn color_stdout() -> bool {
    std::env::var_os("NO_COLOR").is_none() && stdout_is_tty()
}

fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        console_is_tty(-10)
    }
}

fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        console_is_tty(-11)
    }
}

fn stderr_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        console_is_tty(-12)
    }
}

#[cfg(not(unix))]
fn console_is_tty(std_handle: i32) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
    }
    unsafe {
        let handle = GetStdHandle(std_handle as u32);
        let mut mode = 0u32;
        GetConsoleMode(handle, &mut mode) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Script {
        answers: Vec<bool>,
        index: usize,
        pub lines: Vec<String>,
    }

    impl Talk for Script {
        fn say(&mut self, line: &str) {
            self.lines.push(line.to_string());
        }
        fn confirm(&mut self, _prompt: &str) -> bool {
            let answer = self.answers.get(self.index).copied().unwrap_or(false);
            self.index += 1;
            answer
        }
    }

    struct Rec {
        pub calls: Vec<(String, Option<String>)>,
    }

    impl Runner for Rec {
        fn run(&mut self, argv: &[String], stdin: Option<&[u8]>) -> Result<(), String> {
            self.calls.push((
                argv.join(" "),
                stdin.map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
            ));
            Ok(())
        }
    }

    fn locked() -> Snapshot {
        let mut snap = Snapshot::bare(OsKind::Linux, "/usr/local/bin/perfr".into());
        snap.paranoid = Some(3);
        snap.kptr_restrict = Some(1);
        snap.ptrace_scope = Some(1);
        snap.uprobe_pmu = true;
        snap
    }

    #[test]
    fn a_locked_down_linux_warns_and_still_runs() {
        let report = evaluate(&locked());
        assert!(!report.blocks(), "viewer and manual scopes still work");
        let text = render_warning(&report, false).unwrap();
        assert!(text.starts_with('┌'), "{text}");
        assert!(text.contains("perfr: reduced capture"), "{text}");
        assert!(text.contains("sampling"), "{text}");
        assert!(
            text.contains("sudo sysctl -w kernel.perf_event_paranoid=0"),
            "{text}"
        );
        assert!(text.contains(SYSCTL_FILE), "{text}");
        assert!(
            text.contains(
                "sudo setcap cap_sys_admin,cap_perfmon,cap_dac_read_search+ep /usr/local/bin/perfr"
            ),
            "{text}"
        );
        assert!(text.contains("kernel.yama.ptrace_scope = 1"), "{text}");
        assert!(text.contains("kernel.kptr_restrict = 1"), "{text}");
        assert!(text.contains("sched_switch id is not readable"), "{text}");
        assert!(text.contains(SETUP_HINT), "{text}");
        assert!(
            text.contains("The viewer and manual scopes still work."),
            "{text}"
        );
        assert!(!text.contains("multi-machine"), "{text}");
        let colored = render_warning(&report, true).unwrap();
        assert!(colored.contains("\x1b[33;1m"), "{colored}");
        let doctor = render_doctor(&report, false, "perfr doctor");
        assert!(doctor.contains("perf_event_paranoid"), "{doctor}");
        assert!(doctor.contains("capabilities"), "{doctor}");
        assert!(doctor.contains("tracefs"), "{doctor}");
        assert!(doctor.contains("uprobes"), "{doctor}");
    }

    #[test]
    fn open_permissions_print_nothing_at_launch() {
        let mut snap = locked();
        snap.paranoid = Some(-1);
        snap.kptr_restrict = Some(0);
        snap.ptrace_scope = Some(0);
        snap.is_root = true;
        snap.tracefs = Some("/sys/kernel/tracing".into());
        let report = evaluate(&snap);
        assert!(render_warning(&report, false).is_none());
        assert!(plan_for(&snap).sysctls.is_empty());
        assert!(plan_for(&snap).setcap.is_none());
    }

    #[test]
    fn paranoid_one_degrades_the_scheduler_only() {
        let mut snap = locked();
        snap.paranoid = Some(1);
        snap.kptr_restrict = Some(0);
        snap.ptrace_scope = Some(0);
        snap.tracefs = Some("/sys/kernel/tracing".into());
        snap.uprobe_pmu = false;
        let report = evaluate(&snap);
        let sampling = report
            .checks
            .iter()
            .find(|check| check.label == "sampling")
            .unwrap();
        let scheduler = report
            .checks
            .iter()
            .find(|check| check.label == "scheduler")
            .unwrap();
        assert_eq!(sampling.status, Status::Ok);
        assert_eq!(scheduler.status, Status::Degraded);
        assert!(!sampling.warn);
        assert!(scheduler.warn);
    }

    #[test]
    fn macos_warns_about_task_for_pid_and_not_about_sampling() {
        let mut snap = Snapshot::bare(OsKind::Macos, "/usr/local/bin/perfr".into());
        snap.sip_enabled = Some(true);
        let report = evaluate(&snap);
        let text = render_warning(&report, false).unwrap();
        assert!(text.contains("sudo /usr/local/bin/perfr"), "{text}");
        assert!(!text.contains("sampling  degraded"), "{text}");
        let sampling = report
            .checks
            .iter()
            .find(|check| check.label == "sampling")
            .unwrap();
        assert_eq!(sampling.status, Status::NotApplicable);
        let dtrace = report
            .checks
            .iter()
            .find(|check| check.label == "dtrace")
            .unwrap();
        assert!(dtrace.detail.contains("SIP enabled"), "{dtrace:?}");
        assert!(!dtrace.warn);
    }

    #[test]
    fn windows_without_elevation_degrades_the_kernel_logger_only() {
        let snap = Snapshot::bare(OsKind::Windows, r"C:\perfr\perfr.exe".into());
        let report = evaluate(&snap);
        let text = render_warning(&report, false).unwrap();
        assert!(text.contains("Run as administrator"), "{text}");
        let hooks = report
            .checks
            .iter()
            .find(|check| check.label == "dynamic instrumentation")
            .unwrap();
        assert_eq!(hooks.status, Status::Ok);
        assert!(!report.blocks());
    }

    #[test]
    fn an_unsupported_platform_blocks() {
        let snap = Snapshot::bare(OsKind::Other, "perfr".into());
        assert!(evaluate(&snap).blocks());
    }

    #[test]
    fn setup_applies_sysctl_and_persists_only_when_asked() {
        let snap = locked();
        let report = evaluate(&snap);
        let mut talk = Script {
            answers: vec![true, true, false],
            index: 0,
            lines: Vec::new(),
        };
        let mut runner = Rec { calls: Vec::new() };
        let code = run_setup(
            &snap,
            &report,
            &SetupOpts {
                check_only: false,
                yes: false,
                persist: false,
            },
            true,
            &mut talk,
            &mut runner,
        );
        assert_eq!(code, 0);
        assert!(
            runner
                .calls
                .iter()
                .any(|(cmd, _)| cmd.contains("sysctl -w kernel.perf_event_paranoid=0")),
            "{:?}",
            runner.calls
        );
        assert!(
            runner.calls.iter().any(|(cmd, body)| cmd.contains("tee")
                && body
                    .as_deref()
                    .unwrap_or("")
                    .contains("kernel.yama.ptrace_scope=0")),
            "{:?}",
            runner.calls
        );
        assert!(
            !runner.calls.iter().any(|(cmd, _)| cmd.contains("setcap")),
            "declined setcap: {:?}",
            runner.calls
        );
    }

    #[test]
    fn setup_check_and_noninteractive_do_not_change_the_system() {
        let snap = locked();
        let report = evaluate(&snap);
        let mut talk = Script {
            answers: vec![true],
            index: 0,
            lines: Vec::new(),
        };
        let mut runner = Rec { calls: Vec::new() };
        assert_eq!(
            run_setup(
                &snap,
                &report,
                &SetupOpts {
                    check_only: true,
                    yes: false,
                    persist: false
                },
                true,
                &mut talk,
                &mut runner
            ),
            0
        );
        assert!(runner.calls.is_empty());
        let mut talk = Script {
            answers: Vec::new(),
            index: 0,
            lines: Vec::new(),
        };
        assert_eq!(
            run_setup(
                &snap,
                &report,
                &SetupOpts {
                    check_only: false,
                    yes: false,
                    persist: false
                },
                false,
                &mut talk,
                &mut runner
            ),
            0
        );
        assert!(runner.calls.is_empty());
        assert!(
            talk.lines.iter().any(|line| line.contains("--yes")),
            "{:?}",
            talk.lines
        );
    }

    #[test]
    fn preflight_can_be_suppressed() {
        assert!(suppressed(&["--no-preflight".into()]));
        assert!(!suppressed(&["--serve".into()]));
    }

    #[test]
    fn host_probe_does_not_panic() {
        let report = inspect_host();
        assert!(!report.checks.is_empty());
    }
}
