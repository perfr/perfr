// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Which hook crashed the target, and everything known about how.
//!
//! Dynamic instrumentation can kill the process it profiles: a uprobe on a
//! byte that is not an instruction, an inline hook whose relocation went
//! wrong. When the target dies mid-capture with hooks armed, this turns
//! "it crashed" into a named suspect and a file of evidence:
//!
//! - a **journal** written before arming -- every hook, in order, with its
//!   entry bytes, disassembled entry and [`crate::hook_safety`] verdict, plus
//!   a snapshot of the target's `/proc/<pid>/maps`;
//! - the **kernel's own crash line** (`segfault`/`trap`/`general protection`)
//!   read from `/dev/kmsg`, which carries the faulting instruction pointer and
//!   the module it was in;
//! - a **crash report** that names the prime suspect -- the armed hook the
//!   fault points at, or, failing a kernel line, the most dangerous armed hook
//!   by verdict -- and lists every armed hook so nothing is hidden.
//!
//! Reading `/dev/kmsg` needs `CAP_SYS_ADMIN`/`CAP_SYSLOG` (the uprobe path has
//! it); without it the report still names a suspect from the verdicts and says
//! the kernel log was unreadable.

#[cfg(target_os = "linux")]
use std::io::Read;
use std::path::PathBuf;

use crate::hook_safety::{HookSafety, SafetyLevel};
use crate::hooks::HookSpec;

/// One armed hook with the evidence gathered before arming.
#[derive(Clone, Debug)]
pub struct ArmedHook {
    pub spec: HookSpec,
    pub size: u64,
    pub safety: HookSafety,
}

/// The armed set matches the live hooks: one entry per function, gone when
/// it is unhooked. Retries used to append another copy and leave the old one,
/// so the journal grew a function every time it was tried again.
pub fn set_hook(hooks: &mut Vec<ArmedHook>, function_id: u64, hook: Option<ArmedHook>) {
    hooks.retain(|h| h.spec.function_id != function_id);
    if let Some(hook) = hook {
        hooks.push(hook);
    }
}

impl ArmedHook {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "function_id": self.spec.function_id,
            "name": self.spec.name,
            "module": self.spec.module_path.rsplit('/').next().unwrap_or(&self.spec.module_path),
            "module_path": self.spec.module_path,
            "file_offset": self.spec.file_offset,
            "size": self.size,
            "safety": self.safety.level.as_str(),
            "safety_reason": self.safety.reason,
            "entry": self.safety.entry,
            "entry_bytes": self.safety.entry_hex,
        })
    }
}

/// The directory diagnostics are written to (`$PERFR_STATE_DIR`, else the
/// system temp dir). One journal and one crash file per target pid.
fn state_dir() -> PathBuf {
    std::env::var_os("PERFR_STATE_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

pub fn journal_path(target_pid: i32) -> PathBuf {
    state_dir().join(format!("perfr-hooks-{target_pid}.json"))
}

pub fn crash_path(target_pid: i32) -> PathBuf {
    state_dir().join(format!("perfr-hook-crash-{target_pid}.json"))
}

/// The target's `comm` (short name), for matching kernel crash lines.
pub fn read_comm(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The ids of the process's threads, from `/proc/<pid>/task`. Empty once it
/// is gone.
fn thread_ids(pid: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    entries.flatten().filter_map(|entry| entry.file_name().to_str()?.parse().ok()).collect()
}

/// Write the pre-arming journal: the intended hooks, the engine, and a
/// snapshot of the target's maps and threads (the process is alive now; after
/// a crash it is gone, and the kernel names the thread that crashed, not the
/// process). Returns the path, or None if it could not be written
/// (diagnostics are best-effort and never block a capture).
pub fn write_journal(target_pid: i32, engine: &str, hooks: &[ArmedHook]) -> Option<PathBuf> {
    let maps = std::fs::read_to_string(format!("/proc/{target_pid}/maps")).unwrap_or_default();
    let doc = serde_json::json!({
        "target_pid": target_pid,
        "comm": read_comm(target_pid),
        "engine": engine,
        "armed": hooks.iter().map(ArmedHook::to_json).collect::<Vec<_>>(),
        "tids": thread_ids(target_pid),
        "maps": maps,
    });
    let path = journal_path(target_pid);
    std::fs::write(&path, serde_json::to_vec_pretty(&doc).ok()?).ok()?;
    Some(path)
}

/// A crash line the kernel logged for the target.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelCrash {
    /// `segfault`, `trap invalid opcode`, `general protection fault`, ...
    pub kind: String,
    /// The faulting instruction pointer, 0 if the line had none.
    pub ip: u64,
    pub sp: u64,
    /// The module the ip was in, from the `in <module>[...]` clause.
    pub module: String,
    /// Where the mapping the ip was in starts, from the same clause. Not the
    /// module's load base: a text mapping rarely begins at file offset 0.
    pub module_base: u64,
    /// The ip as an offset into the module's file, which is what a hook's
    /// placement is: printed by current kernels, else worked out from the
    /// journal's maps snapshot. `None` when neither was to be had.
    pub file_offset: Option<u64>,
    /// The whole line, verbatim.
    pub raw: String,
}

/// Parse a hex or decimal integer that may carry a `0x` prefix.
fn parse_int(token: &str) -> Option<u64> {
    let t = token.trim_end_matches([':', ',']);
    if let Some(hex) = t.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        // Kernel segfault lines print ip/sp as bare hex without 0x.
        u64::from_str_radix(t, 16).ok().or_else(|| t.parse().ok())
    }
}

/// Whether the line is about one of `tids`. It carries `comm[tid]:` or, on
/// a `traps:` line, `comm[tid] trap ...`; a comm may itself hold spaces and
/// brackets, and the module clause ends in a bracket too, so only a number
/// followed the way the kernel follows the thread id counts.
fn names_a_thread(line: &str, tids: &[i32]) -> bool {
    line.match_indices('[').any(|(open, _)| {
        let rest = &line[open + 1..];
        let Some(close) = rest.find(']') else { return false };
        let after = &rest[close + 1..];
        (after.starts_with(':') || after.starts_with(" trap ") || after.starts_with(" general protection"))
            && rest[..close].parse::<i32>().is_ok_and(|tid| tids.contains(&tid))
    })
}

/// Extract the crash facts from one kernel line about one of the target's
/// threads. The number in brackets is the id of the thread that faulted, and
/// its comm, so a crash on a worker thread carries neither the process's pid
/// nor its name. The x86 forms:
///   `comm[tid]: segfault at ADDR ip IP sp SP error N in MOD[OFFSET,START+SIZE] likely on CPU ...`
///   `comm[tid]: segfault at ADDR ip IP sp SP error N in MOD[START+SIZE]` (older kernels)
///   `traps: comm[tid] trap invalid opcode ip:IP sp:SP error:N in MOD[...]`
///   `comm[tid]: segfault ... ` / `general protection fault ...`
/// The `in` clause is absent when the ip was not in a file mapping.
pub fn parse_kernel_line(tids: &[i32], line: &str) -> Option<KernelCrash> {
    if !names_a_thread(line, tids) {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    let kind = if lower.contains("segfault") {
        "segfault"
    } else if lower.contains("general protection") {
        "general protection fault"
    } else if lower.contains("trap invalid opcode") || lower.contains("invalid opcode") {
        "trap invalid opcode"
    } else if lower.contains("trap int3") {
        "trap int3"
    } else if lower.contains(" trap ") {
        "trap"
    } else {
        return None;
    };
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let after = |key: &str| -> Option<u64> {
        // `ip IP` (segfault) or `ip:IP` (traps).
        tokens.iter().position(|t| *t == key).and_then(|i| tokens.get(i + 1)).and_then(|t| parse_int(t))
            .or_else(|| tokens.iter().find_map(|t| t.strip_prefix(&format!("{key}:")).and_then(parse_int)))
    };
    let (module, module_base, file_offset) = tokens
        .iter()
        .position(|t| *t == "in")
        .and_then(|i| tokens.get(i + 1))
        .and_then(|clause| {
            // `libfoo.so[1234,7f1234560000+2000]`: the ip's offset in the
            // file, then the mapping. Older kernels print the mapping alone,
            // `libfoo.so[7f1234560000+2000]`.
            let (name, rest) = clause.split_once('[')?;
            let inside = rest.split(']').next().unwrap_or("");
            let (offset, mapping) = match inside.split_once(',') {
                Some((offset, mapping)) => (u64::from_str_radix(offset, 16).ok(), mapping),
                None => (None, inside),
            };
            let start = mapping.split('+').next().and_then(|t| u64::from_str_radix(t, 16).ok()).unwrap_or(0);
            Some((name.to_string(), start, offset))
        })
        .unwrap_or_default();
    Some(KernelCrash {
        kind: kind.to_string(),
        ip: after("ip").unwrap_or(0),
        sp: after("sp").unwrap_or(0),
        module,
        module_base,
        file_offset,
        raw: line.trim().to_string(),
    })
}

/// The file offset of `ip`, given the mapping it was in starts at
/// `mapping_start`: the offset into that mapping plus the mapping's own
/// offset in the file, from a `/proc/<pid>/maps` snapshot.
fn file_offset_from_maps(maps: &str, mapping_start: u64, ip: u64) -> Option<u64> {
    maps.lines().find_map(|line| {
        // `start-end perms offset dev inode path`
        let mut fields = line.split_whitespace();
        let (start, _end) = fields.next()?.split_once('-')?;
        if u64::from_str_radix(start, 16).ok()? != mapping_start {
            return None;
        }
        let offset = u64::from_str_radix(fields.nth(1)?, 16).ok()?;
        ip.checked_sub(mapping_start)?.checked_add(offset)
    })
}

/// The last crash line among `lines` about one of `tids`, with the ip's
/// file offset filled in from `maps` when the kernel did not print it.
fn last_crash_in<'a>(lines: impl Iterator<Item = &'a str>, tids: &[i32], maps: &str) -> Option<KernelCrash> {
    let mut crash = lines.filter_map(|line| parse_kernel_line(tids, line)).last()?;
    if crash.file_offset.is_none() && crash.ip != 0 && !crash.module.is_empty() {
        crash.file_offset = file_offset_from_maps(maps, crash.module_base, crash.ip);
    }
    Some(crash)
}

/// The threads a crash line may name and the maps to place its ip in: what
/// the journal recorded while the target was alive, plus whatever `/proc`
/// still has. By the time a crash is noticed the process is gone or a
/// zombie, and neither can be asked.
fn threads_and_maps(pid: i32) -> (Vec<i32>, String) {
    let journal: serde_json::Value = std::fs::read(journal_path(pid))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut tids = vec![pid];
    if journal["target_pid"].as_i64() == Some(pid as i64) {
        tids.extend(journal["tids"].as_array().into_iter().flatten().filter_map(|t| t.as_i64()).map(|t| t as i32));
    }
    tids.extend(thread_ids(pid));
    tids.sort_unstable();
    tids.dedup();
    (tids, journal["maps"].as_str().unwrap_or_default().to_string())
}

/// Read what `/dev/kmsg` currently holds and return the last crash line for
/// a thread of `pid`. Non-blocking: it drains the buffer and stops at the end
/// rather than waiting for new messages. None when the device is unreadable
/// (no privilege) or holds no matching line.
pub fn scan_kernel_crash(pid: i32) -> Option<KernelCrash> {
    let (tids, maps) = threads_and_maps(pid);
    match read_kmsg_crash(&tids, &maps) {
        Ok(found) => found,
        Err(errno) => {
            // Once: reading the kernel log is how the faulting instruction is
            // recovered. dmesg_restrict gates it on CAP_SYSLOG; the uprobe
            // path's CAP_SYS_ADMIN is not always accepted. Say so, with the fix.
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "cannot read /dev/kmsg (errno {errno}); a hook crash will be \
                     reported without the faulting instruction. For that, grant the service \
                     CAP_SYSLOG or set kernel.dmesg_restrict=0."
                );
            }
            None
        }
    }
}

/// Reads what `/dev/kmsg` currently holds and returns the last crash line for
/// one of `tids`. `Err(errno)` when the device could not be opened (so the
/// caller can tell "no crash line" from "not allowed to look").
#[cfg(target_os = "linux")]
fn read_kmsg_crash(tids: &[i32], maps: &str) -> Result<Option<KernelCrash>, i32> {
    // O_NONBLOCK so a read past the end returns EAGAIN instead of blocking for
    // the next kernel message.
    let fd = unsafe { libc::open(c"/dev/kmsg".as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(unsafe { *libc::__errno_location() });
    }
    let mut file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };
    let mut found = None;
    let mut buf = [0u8; 8192];
    // The buffer is bounded, but cap the walk so a pathological log cannot
    // spin here.
    for _ in 0..200_000 {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Each record is `prio,seq,ts,flags;message`.
                if let Ok(text) = std::str::from_utf8(&buf[..n]) {
                    if let Some(msg) = text.split_once(';').map(|(_, m)| m) {
                        // The message is the record's first line; what
                        // follows are its `KEY=value` continuation lines.
                        if let Some(crash) = last_crash_in(msg.lines().take(1), tids, maps) {
                            found = Some(crash);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            // ENOBUFS/EPIPE: records were overwritten between reads; the next
            // read resumes at the oldest still-present record, so keep going.
            Err(_) => continue,
        }
    }
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
fn read_kmsg_crash(_tids: &[i32], _maps: &str) -> Result<Option<KernelCrash>, i32> {
    Ok(None)
}

/// Is the process still running? A gone target has no `/proc/<pid>`; a
/// *crashed* one that its parent has not reaped is a zombie, whose
/// `/proc/<pid>` still exists -- so a plain existence check would miss the
/// very crash we are watching for. Read the state and treat zombie/dead as
/// not running.
#[cfg(windows)]
pub fn process_alive(pid: i32) -> bool {
    crate::windows::process_alive(pid)
}

#[cfg(not(windows))]
pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state ...`; comm may hold spaces and parens, so the state
    // is the first token after the final ')'.
    match stat.rfind(')') {
        Some(paren) => {
            let state = stat[paren + 1..].trim_start().as_bytes().first().copied().unwrap_or(b'?');
            state != b'Z' && state != b'X' && state != b'x'
        }
        None => true,
    }
}

/// The crash report: a named suspect and every fact gathered.
pub struct CrashReport {
    pub summary: String,
    pub json: serde_json::Value,
    pub path: Option<PathBuf>,
}

/// Build (and write) the report once the target has died with hooks armed.
/// `kernel` is the kernel crash line if one was found (scan it with
/// [`scan_kernel_crash`] before calling, so the caller can decide whether the
/// death is worth blaming on a hook at all).
///
/// The prime suspect is, in order of confidence: the armed hook the kernel's
/// faulting ip lands in (module + offset match); else the most dangerous
/// armed hook by verdict (unsafe before risky); else the last hook armed.
pub fn build_and_write(target_pid: i32, engine: &str, hooks: &[ArmedHook], kernel: Option<KernelCrash>) -> CrashReport {
    // By kernel ip: its offset in the faulting module's file, matched to a
    // hook in that module whose [offset, offset+size) covers it.
    let by_ip = kernel.as_ref().and_then(|k| {
        if k.ip == 0 || k.module.is_empty() {
            return None;
        }
        let module_offset = k.file_offset?;
        hooks.iter().find(|h| {
            h.spec.module_path.rsplit('/').next().unwrap_or("") == k.module
                && module_offset >= h.spec.file_offset
                && module_offset < h.spec.file_offset + h.size.max(1)
        })
    });

    // By verdict: the worst-rated armed hook (unsafe, then risky).
    let by_verdict = hooks
        .iter()
        .filter(|h| matches!(h.safety.level, SafetyLevel::Unsafe | SafetyLevel::Risky))
        .min_by_key(|h| match h.safety.level {
            SafetyLevel::Unsafe => 0,
            SafetyLevel::Risky => 1,
            _ => 2,
        });

    let (suspect, basis) = match (by_ip, by_verdict) {
        (Some(h), _) => (Some(h), "the kernel's faulting instruction points into it"),
        (None, Some(h)) => (Some(h), "it is the most dangerous hook that was armed"),
        (None, None) => (hooks.last(), "it was the last hook armed (no stronger signal)"),
    };

    // Honest wording: a kernel line proves a crash and its signal; without one
    // we only know the target vanished mid-capture and can name a likely
    // suspect.
    let signal = kernel.as_ref().map(|k| k.kind.clone()).unwrap_or_else(|| "gone (kernel log unreadable)".to_string());
    let summary = match (suspect, &kernel) {
        (Some(h), Some(k)) => format!(
            "target {target_pid} crashed ({}) with {} hook(s) armed; prime suspect: {} ({}) -- {basis}",
            k.kind, hooks.len(), h.spec.name, h.safety.level.as_str(),
        ),
        (Some(h), None) => format!(
            "target {target_pid} ended during the capture with {} hook(s) armed; if it crashed, the likely suspect is {} ({}) -- {basis}",
            hooks.len(), h.spec.name, h.safety.level.as_str(),
        ),
        (None, _) => format!("target {target_pid} ended; no hooks were armed"),
    };

    let json = serde_json::json!({
        "target_pid": target_pid,
        "engine": engine,
        "signal": signal,
        "kernel": kernel.as_ref().map(|k| serde_json::json!({
            "kind": k.kind, "ip": k.ip, "sp": k.sp,
            "module": k.module, "module_base": k.module_base, "file_offset": k.file_offset, "raw": k.raw,
        })),
        "kernel_log_readable": kernel.is_some(),
        "suspect": suspect.map(|h| serde_json::json!({
            "function_id": h.spec.function_id,
            "name": h.spec.name,
            "module_path": h.spec.module_path,
            "file_offset": h.spec.file_offset,
            "safety": h.safety.level.as_str(),
            "safety_reason": h.safety.reason,
            "entry": h.safety.entry,
            "entry_bytes": h.safety.entry_hex,
            "basis": basis,
        })),
        "armed": hooks.iter().map(ArmedHook::to_json).collect::<Vec<_>>(),
        "summary": summary,
    });

    let path = crash_path(target_pid);
    let written = serde_json::to_vec_pretty(&json).ok().and_then(|b| std::fs::write(&path, b).ok()).map(|_| path);
    CrashReport { summary, json, path: written }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(name: &str, offset: u64, size: u64, level: SafetyLevel) -> ArmedHook {
        ArmedHook {
            spec: HookSpec { function_id: offset, module_path: "/lib/libfoo.so".into(), file_offset: offset, name: name.into() },
            size,
            safety: HookSafety { level, reason: format!("{level:?}"), entry: String::new(), entry_hex: String::new() },
        }
    }

    #[test]
    fn the_armed_set_drops_a_function_when_it_is_unhooked_and_keeps_one_copy() {
        let mut armed = vec![
            hook("run_frame", 1, 64, SafetyLevel::Safe),
            hook("run_frame", 1, 64, SafetyLevel::Safe),
            hook("burn_us", 2, 32, SafetyLevel::Risky),
        ];
        set_hook(
            &mut armed,
            1,
            Some(hook("run_frame", 1, 64, SafetyLevel::Safe)),
        );
        assert_eq!(
            armed.len(),
            2,
            "a retried function is one entry, not a second copy"
        );
        assert_eq!(armed.iter().filter(|h| h.spec.function_id == 1).count(), 1);
        set_hook(&mut armed, 1, None);
        assert_eq!(
            armed
                .iter()
                .map(|h| h.spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["burn_us"]
        );
        set_hook(&mut armed, 2, None);
        assert!(armed.is_empty());
    }

    #[test]
    fn rewriting_the_journal_drops_an_unhooked_function() {
        let pid = (std::process::id() as i32).saturating_add(1_000_000);
        let live = hook("live", 11, 8, SafetyLevel::Safe);
        let gone = hook("gone", 12, 8, SafetyLevel::Safe);
        let path = write_journal(pid, "kernel_uprobes", &[live.clone(), gone.clone()])
            .expect("journal writes");
        let mut armed = vec![live, gone];
        set_hook(&mut armed, 12, None);
        write_journal(pid, "kernel_uprobes", &armed).expect("journal rewrites on removal");
        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let names: Vec<&str> = value["armed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["live"]);
        let _ = std::fs::remove_file(path);
    }

    // Two lines as this kernel (7.0) logged them for a program whose worker
    // thread faulted: process 672042 and thread 672043, then 672058 and
    // 672059. The text mapping is at file offset 0x1000.
    const SEGFAULT: &str = "crashprog[672043]: segfault at 0 ip 00005eda761e51e9 sp 000073ab54ffee70 error 6 in crashprog[11e9,5eda761e5000+1000] likely on CPU 17 (core 33, socket 0)";
    const INVALID_OPCODE: &str = "traps: crashprog[672059] trap invalid opcode ip:5ae48dbc21db sp:7bf47a5fee70 error:0 in crashprog[11db,5ae48dbc2000+1000]";
    // The first of them as older kernels print the clause, without the
    // offset, and the maps the journal would have held.
    const SEGFAULT_OLD: &str = "crashprog[672043]: segfault at 0 ip 00005eda761e51e9 sp 000073ab54ffee70 error 6 in crashprog[5eda761e5000+1000]";
    const MAPS: &str = "\
5eda761e4000-5eda761e5000 r--p 00000000 103:02 1234 /tmp/crashprog
5eda761e5000-5eda761e6000 r-xp 00001000 103:02 1234 /tmp/crashprog
5eda761e6000-5eda761e7000 r--p 00002000 103:02 1234 /tmp/crashprog
73ab54800000-73ab55000000 rw-p 00000000 00:00 0
";

    fn hook_in(module_path: &str, name: &str, offset: u64, size: u64, level: SafetyLevel) -> ArmedHook {
        let mut hook = hook(name, offset, size, level);
        hook.spec.module_path = module_path.into();
        hook
    }

    #[test]
    fn a_segfault_line_is_parsed() {
        let c = parse_kernel_line(&[672043], SEGFAULT).expect("parsed");
        assert_eq!(c.kind, "segfault");
        assert_eq!(c.ip, 0x5eda761e51e9);
        assert_eq!(c.sp, 0x73ab54ffee70);
        assert_eq!(c.module, "crashprog");
        assert_eq!(c.module_base, 0x5eda761e5000);
        assert_eq!(c.file_offset, Some(0x11e9));
        // A line for another process's threads is ignored.
        assert!(parse_kernel_line(&[9999], SEGFAULT).is_none());
    }

    #[test]
    fn a_traps_invalid_opcode_line_is_parsed() {
        let c = parse_kernel_line(&[672059], INVALID_OPCODE).expect("parsed");
        assert_eq!(c.kind, "trap invalid opcode");
        assert_eq!(c.ip, 0x5ae48dbc21db);
        assert_eq!(c.module, "crashprog");
        assert_eq!(c.file_offset, Some(0x11db));
    }

    #[test]
    fn a_crash_on_a_worker_thread_is_found_by_the_threads_the_process_had() {
        // The kernel names the thread; the capture knows the process.
        assert!(parse_kernel_line(&[672042], SEGFAULT).is_none(), "the pid alone does not match");
        assert!(parse_kernel_line(&[672042, 672043, 672044], SEGFAULT).is_some());
        // A comm with spaces and a line with no module clause, as logged
        // here for a fault in anonymous memory.
        let anonymous = "Async Compute T[3513688]: segfault at 77996e002510 ip 000077996e002510 sp 0000779996bf9e28 error 15 likely on CPU 7 (core 12, socket 0)";
        let c = parse_kernel_line(&[3513600, 3513688], anonymous).expect("parsed");
        assert_eq!((c.ip, c.module.as_str(), c.file_offset), (0x77996e002510, "", None));
        // The mapping's numbers are not thread ids.
        assert!(parse_kernel_line(&[1000], "x[7]: segfault at 0 ip 1 sp 2 error 4 in x[1000]").is_none());
    }

    #[test]
    fn the_ip_names_the_hook_it_lands_in() {
        // Two hooks in the program; the fault is inside the second. A file
        // offset, not an offset into the mapping: the mapping starts 0x1000
        // into the file, and ip - start (0x1e9) is in neither hook.
        let hooks = vec![
            hook_in("/tmp/crashprog", "safe_fn", 0x1100, 0x40, SafetyLevel::Safe),
            hook_in("/tmp/crashprog", "bad_fn", 0x11c0, 0x40, SafetyLevel::Safe),
            hook_in("/lib/libfoo.so", "elsewhere", 0x11e0, 0x40, SafetyLevel::Unsafe),
        ];
        for (line, maps) in [(SEGFAULT, ""), (SEGFAULT_OLD, MAPS)] {
            let kernel = last_crash_in([line].into_iter(), &[672042, 672043], maps).expect("found");
            assert_eq!(kernel.file_offset, Some(0x11e9), "{line}");
            let report = build_and_write(-2, "kernel_uprobes", &hooks, Some(kernel));
            assert_eq!(report.json["suspect"]["name"], "bad_fn", "{line}");
            assert_eq!(report.json["suspect"]["basis"], "the kernel's faulting instruction points into it");
            assert_eq!(report.json["kernel"]["file_offset"], 0x11e9);
        }
        // An old-form line and no maps to place it with: no guess by address,
        // the verdicts decide.
        let kernel = last_crash_in([SEGFAULT_OLD].into_iter(), &[672043], "").expect("found");
        assert_eq!(kernel.file_offset, None);
        let report = build_and_write(-2, "kernel_uprobes", &hooks, Some(kernel));
        assert_eq!(report.json["suspect"]["name"], "elsewhere");
        let _ = std::fs::remove_file(crash_path(-2));
    }

    #[test]
    fn the_last_crash_line_of_the_process_wins_and_other_lines_are_skipped() {
        let lines = [
            "usb 1-2: new high-speed USB device number 5 using xhci_hcd",
            SEGFAULT,
            "other[672043]: something that is not a crash",
            INVALID_OPCODE,
        ];
        let c = last_crash_in(lines.into_iter(), &[672043, 672059], "").expect("found");
        assert_eq!(c.kind, "trap invalid opcode");
        assert!(last_crash_in(lines.into_iter(), &[1], "").is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_journal_records_the_threads_a_crash_line_may_name() {
        // This process, journalled while it runs: the thread running this
        // test is not its main thread, and is what a crash line would name.
        let pid = std::process::id() as i32;
        let tid = unsafe { libc::gettid() };
        assert_ne!(pid, tid);
        let path = write_journal(pid, "kernel_uprobes", &[]).expect("journal writes");
        let (tids, maps) = threads_and_maps(pid);
        let _ = std::fs::remove_file(path);
        assert!(tids.contains(&tid) && tids.contains(&pid), "{tids:?}");
        assert!(maps.contains("r-xp"), "the maps snapshot comes back with them");
        // With no journal there is still the pid itself to look for.
        assert_eq!(threads_and_maps(-3).0, vec![-3]);
    }

    #[test]
    fn without_a_kernel_line_the_worst_verdict_is_the_suspect() {
        let hooks = vec![
            hook("a", 0x1000, 0x40, SafetyLevel::Safe),
            hook("b", 0x1100, 0x40, SafetyLevel::Risky),
            hook("c", 0x1200, 0x40, SafetyLevel::Unsafe),
        ];
        // No kernel line: the report falls back to the worst verdict.
        let report = build_and_write(-1, "kernel_uprobes", &hooks, None);
        assert!(report.summary.contains("likely suspect is c"), "{}", report.summary);
        assert_eq!(report.json["suspect"]["name"], "c");
        assert_eq!(report.json["kernel_log_readable"], false);
        // clean up the file the report wrote for pid -1, if any.
        let _ = std::fs::remove_file(crash_path(-1));
    }
}
