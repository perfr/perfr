// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The captured machine: topology, caches, and power mode.
//!
//! Linux reads a sysfs and procfs tree. The parsing is the tree, so the
//! tests build a small one per part instead of depending on this host.
//! Windows and macOS fill what those systems report and leave the rest
//! unknown; the Windows buffer parser is tested from records written by
//! hand, and neither the Windows calls nor `pmset` have been run here.
//!
//! The lane marks ([`perfr_live_event::CoreInfo`]) are read from the same
//! walk: [`CpuTopology::core_infos`] of what this module returns.
//!
//! What only another program can say (`powerprofilesctl`, `pmset`) is asked
//! on a thread of its own, after the capture has the rest: a capture does
//! not wait on a D-Bus call to start.

#[cfg(any(test, target_os = "linux"))]
use std::path::Path;
use std::sync::Arc;

use perfr_live_event::topology::CpuTopology;
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
use perfr_live_event::topology::{keep_flags, CacheInstance};
#[cfg(any(test, target_os = "linux"))]
use perfr_live_event::topology::{accept_power_profile, CpufreqPolicy, LogicalCpu, NumaNode, PowerState};
#[cfg(any(test, target_os = "linux"))]
use perfr_live_event::core_kind;
use perfr_live_server::LiveService;

/// This machine, as the system itself says it when asked. No other program
/// is run: see [`ask_programs`]. Empty when nothing could be read.
pub fn read_this_machine() -> CpuTopology {
    #[cfg(target_os = "linux")]
    {
        read_linux_tree(Path::new("/"), "")
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::machine_topology()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        CpuTopology::default()
    }
}

/// Says what this machine is to the capture that has just started: the lane
/// marks and the topology, from one read. Called after the start, where a
/// viewer drops what it holds of the last capture.
///
/// The machine is added to the capture's, not put in their place: on a hub
/// a remote may have said its own already. What another program has to be
/// asked for follows from a thread of its own.
pub fn record(service: &Arc<LiveService>) {
    let machine = read_this_machine();
    service.set_cpu_cores(&machine.core_infos());
    service.adopt_cpu_topology(&machine);
    ask_programs_later(service, machine);
}

/// How long a program asked for the power mode may take. `powerprofilesctl`
/// answers in about a tenth of a second; one that has not by now is waiting
/// on a bus that is not answering.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROGRAM_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Asks the programs on a thread of its own and says the machine again with
/// what they answered. The capture it was asked for may be over by then, and
/// another begun or a file opened: the service drops an answer that is not
/// for the capture it holds.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ask_programs_later(service: &Arc<LiveService>, mut machine: CpuTopology) {
    let capture = service.capture_token();
    let service = service.clone();
    let asked = std::thread::Builder::new().name("perfr-power".into()).spawn(move || {
        if ask_programs(&mut machine) {
            service.amend_cpu_topology(capture, &machine);
        }
    });
    if let Err(error) = asked {
        log::debug!("the power mode was not asked for: {error}");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ask_programs_later(_service: &Arc<LiveService>, _machine: CpuTopology) {}

/// Fills what only another program says, and tells whether anything was
/// filled. Each program is given [`PROGRAM_LIMIT`] and no longer.
///
/// Linux: `powerprofilesctl get`, when that program is installed and prints
/// a profile name. A missing program or a usage message is nothing, not a
/// failure: the platform profile is a file, and is already read.
#[cfg(target_os = "linux")]
fn ask_programs(machine: &mut CpuTopology) -> bool {
    let profile = run_bounded("powerprofilesctl", &["get"], PROGRAM_LIMIT).map(|text| accept_power_profile(&text)).unwrap_or_default();
    if profile.is_empty() || profile == machine.power.daemon_profile {
        return false;
    }
    machine.power.daemon_profile = profile;
    true
}

/// macOS: `pmset -g` and `pmset -g batt`, for low power, high power and AC
/// or battery.
#[cfg(target_os = "macos")]
fn ask_programs(machine: &mut CpuTopology) -> bool {
    let mut text = String::new();
    for args in [&["-g"][..], &["-g", "batt"][..]] {
        if let Some(out) = run_bounded("pmset", args, PROGRAM_LIMIT) {
            text.push_str(&out);
            text.push('\n');
        }
    }
    let (low, high, on_ac) = perfr_live_event::topology::parse_pmset(&text);
    if (low, high, on_ac) == (machine.power.low_power, machine.power.high_power, machine.power.on_ac) {
        return false;
    }
    machine.power.low_power = low;
    machine.power.high_power = high;
    machine.power.on_ac = on_ac;
    true
}

/// What `program` printed, when it ran, succeeded and was done within
/// `limit`. A program still running at the limit is killed. The output is
/// read after the exit, so a program that fills its pipe before it exits
/// runs into the limit: these print a line or a page.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_bounded(program: &str, args: &[&str], limit: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut text = String::new();
                child.stdout.take()?.read_to_string(&mut text).ok()?;
                return status.success().then_some(text);
            }
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// `root` is a filesystem root: `proc/cpuinfo`, `sys/devices` (see
/// [`read_devices`]), `sys/firmware/acpi/platform_profile`.
/// `daemon_profile` is what `powerprofilesctl get` printed, or empty.
#[cfg(any(test, target_os = "linux"))]
pub fn read_linux_tree(root: &Path, daemon_profile: &str) -> CpuTopology {
    let info = parse_cpuinfo(&read_text(&root.join("proc/cpuinfo")));
    let mut topo = read_devices(&root.join("sys/devices"));
    topo.vendor = info.vendor;
    topo.model = info.model;
    topo.family = info.family;
    topo.model_id = info.model_id;
    topo.stepping = info.stepping;
    topo.microcode = info.microcode;
    topo.flags = info.flags;
    // What sysfs did not say, as cpuinfo counts it.
    if topo.sockets == 0 {
        topo.sockets = info.sockets;
    }
    if topo.physical_cores == 0 {
        topo.physical_cores = info.physical_cores;
    }
    if topo.threads == 0 {
        topo.threads = info.threads;
    }
    if topo.cache_line_bytes == 0 {
        topo.cache_line_bytes = info.cache_alignment;
    }
    topo.kernel = trim_text(&read_text(&root.join("proc/sys/kernel/osrelease")));
    topo.hostname = trim_text(&read_text(&root.join("proc/sys/kernel/hostname")));
    topo.ram_bytes = crate::sysinfo::parse_meminfo_total_bytes(&read_text(&root.join("proc/meminfo")));
    if topo.ram_bytes == 0 {
        topo.ram_bytes = topo.numa.iter().map(|n| n.mem_bytes).sum();
    }
    topo.power.platform_profile = trim_text(&read_text(&root.join("sys/firmware/acpi/platform_profile")));
    topo.power.daemon_profile = accept_power_profile(daemon_profile);
    if topo.sockets > 0 && !topo.numa.is_empty() && topo.numa.len() % usize::from(topo.sockets) == 0 {
        let nps = topo.numa.len() / usize::from(topo.sockets);
        if nps > 0 && nps <= usize::from(u8::MAX) {
            topo.nps = nps as u8;
            topo.nps_source = "derived".into();
        }
    }
    let anything = !topo.model.is_empty() || !topo.cpus.is_empty() || !topo.kernel.is_empty() || topo.ram_bytes > 0;
    if anything {
        topo.os = "Linux".into();
    }
    topo.apply_model_table();
    topo
}

/// What the sysfs tree at `devices` (`/sys/devices`) says of the machine:
/// one logical CPU per online `system/cpu/cpuN`, the caches, the nodes of
/// `system/node`, and cpufreq.
///
/// A CPU's kind is what the kernel's hybrid PMUs say (`cpu_core/cpus` and
/// `cpu_atom/cpus` on Intel, with `cpu_lowpower` among the efficiency
/// cores), or else the capacities of an asymmetric machine
/// (`cpu_capacity`, big.LITTLE): the cores of the highest capacity are the
/// performance cores. The top frequency is `cpufreq/cpuinfo_max_freq`, and
/// the level 3 is the size of the `cache/index*` whose level is 3. These
/// are the files a tool that pins a game to "the fast cores" reads.
#[cfg(any(test, target_os = "linux"))]
pub fn read_devices(devices: &Path) -> CpuTopology {
    let cpu_root = devices.join("system/cpu");
    let ids = cpu_ids(&cpu_root);
    let performance = crate::sysinfo::parse_cpu_list(&read_text(&devices.join("cpu_core/cpus")));
    let mut efficiency = crate::sysinfo::parse_cpu_list(&read_text(&devices.join("cpu_atom/cpus")));
    efficiency.extend(crate::sysinfo::parse_cpu_list(&read_text(&devices.join("cpu_lowpower/cpus"))));
    let hybrid = !performance.is_empty() && !efficiency.is_empty();

    let mut drafts: Vec<Draft> = Vec::new();
    let mut saw_siblings = false;
    let mut any_long_siblings = false;
    for cpu in &ids {
        let dir = cpu_root.join(format!("cpu{cpu}"));
        let topo = dir.join("topology");
        let siblings_text = {
            let core = read_text(&topo.join("core_cpus_list"));
            if core.trim().is_empty() { read_text(&topo.join("thread_siblings_list")) } else { core }
        };
        let siblings = if siblings_text.trim().is_empty() {
            vec![*cpu]
        } else {
            saw_siblings = true;
            let list = crate::sysinfo::parse_cpu_list(&siblings_text);
            if list.len() > 1 {
                any_long_siblings = true;
            }
            if list.is_empty() { vec![*cpu] } else { list }
        };
        let freq = dir.join("cpufreq");
        let mut logical = LogicalCpu {
            cpu: *cpu,
            socket: parse_opt_u16(&read_text(&topo.join("physical_package_id"))),
            die: parse_opt_u16(&read_text(&topo.join("die_id"))),
            cluster: parse_opt_u16(&read_text(&topo.join("cluster_id"))),
            core_id: parse_opt_u16(&read_text(&topo.join("core_id"))),
            max_freq_khz: parse_u32(&read_text(&freq.join("cpuinfo_max_freq"))),
            base_freq_khz: parse_u32(&read_text(&freq.join("base_frequency"))),
            min_freq_khz: parse_u32(&read_text(&freq.join("cpuinfo_min_freq"))),
            cur_freq_khz: parse_u32(&read_text(&freq.join("scaling_cur_freq"))),
            ..LogicalCpu::default()
        };
        let mut caches = Vec::new();
        for index in 0..10 {
            let cache = dir.join(format!("cache/index{index}"));
            let level_text = read_text(&cache.join("level"));
            if level_text.trim().is_empty() {
                continue;
            }
            let level: u8 = level_text.trim().parse().unwrap_or(0);
            let kind = cache_kind(&read_text(&cache.join("type")));
            let size_kb = crate::sysinfo::parse_cache_size_kb(&read_text(&cache.join("size")));
            let ways = parse_u32(&read_text(&cache.join("ways_of_associativity"))) as u16;
            let line = parse_u32(&read_text(&cache.join("coherency_line_size"))) as u16;
            let shared_text = read_text(&cache.join("shared_cpu_list"));
            let shared = if shared_text.trim().is_empty() {
                vec![*cpu]
            } else {
                crate::sysinfo::parse_cpu_list(&shared_text)
            };
            match (level, kind.as_str()) {
                (1, "instruction") => logical.l1i_kb = size_kb,
                (1, "data") => logical.l1d_kb = size_kb,
                (2, _) => logical.l2_kb = size_kb,
                (3, _) => logical.l3_kb = size_kb,
                _ => {}
            }
            caches.push(CacheInstance { level, kind, size_kb, ways, line, cpus: shared });
        }
        drafts.push(Draft {
            logical,
            driver: trim_text(&read_text(&freq.join("scaling_driver"))),
            governor: trim_text(&read_text(&freq.join("scaling_governor"))),
            epp: trim_text(&read_text(&freq.join("energy_performance_preference"))),
            scaling_min: parse_u32(&read_text(&freq.join("scaling_min_freq"))),
            scaling_max: parse_u32(&read_text(&freq.join("scaling_max_freq"))),
            siblings,
            caches,
        });
    }

    let capacities: Vec<u32> = ids
        .iter()
        .map(|cpu| parse_u32(&read_text(&cpu_root.join(format!("cpu{cpu}/cpu_capacity")))))
        .collect();
    let top_capacity = capacities.iter().copied().max().unwrap_or(0);
    let asymmetric = capacities.iter().any(|&c| c != 0 && c != top_capacity);
    for (draft, capacity) in drafts.iter_mut().zip(&capacities) {
        let cpu = draft.logical.cpu;
        draft.logical.kind = if hybrid {
            if performance.contains(&cpu) {
                core_kind::PERFORMANCE
            } else if efficiency.contains(&cpu) {
                core_kind::EFFICIENCY
            } else {
                core_kind::UNKNOWN
            }
        } else if asymmetric && *capacity != 0 {
            if *capacity == top_capacity { core_kind::PERFORMANCE } else { core_kind::EFFICIENCY }
        } else {
            core_kind::UNKNOWN
        };
        draft.siblings.sort_unstable();
        draft.siblings.dedup();
        draft.logical.thread = draft.siblings.iter().position(|c| *c == cpu).map(|i| i as u16);
    }

    let mut caches: Vec<CacheInstance> = Vec::new();
    for draft in &drafts {
        for cache in &draft.caches {
            let mut key = cache.clone();
            key.cpus.sort_unstable();
            key.cpus.dedup();
            if !caches.iter().any(|have| have == &key) {
                caches.push(key);
            }
        }
    }
    caches.sort_by(|a, b| (a.level, &a.kind, a.cpus.first().copied().unwrap_or(0)).cmp(&(b.level, &b.kind, b.cpus.first().copied().unwrap_or(0))));

    let numa = read_numa(&devices.join("system/node"));
    for node in &numa {
        for cpu in &node.cpus {
            if let Some(draft) = drafts.iter_mut().find(|d| d.logical.cpu == *cpu) {
                draft.logical.numa = Some(node.id);
            }
        }
    }
    let distances = read_distances(&devices.join("system/node"), &numa);

    let mut policies = read_policies(&cpu_root);
    if policies.is_empty() {
        policies = policies_from_drafts(&drafts);
    }

    let mut power = PowerState { policies, ..PowerState::default() };
    let intel = trim_text(&read_text(&cpu_root.join("intel_pstate/status")));
    let amd = trim_text(&read_text(&cpu_root.join("amd_pstate/status")));
    if !intel.is_empty() {
        power.pstate = "intel_pstate".into();
        power.pstate_status = intel;
    } else if !amd.is_empty() {
        power.pstate = "amd_pstate".into();
        power.pstate_status = amd;
    }
    let no_turbo = trim_text(&read_text(&cpu_root.join("intel_pstate/no_turbo")));
    if no_turbo == "0" || no_turbo == "1" {
        power.turbo = Some(no_turbo == "0");
        power.turbo_source = "no_turbo".into();
    } else {
        let boost = trim_text(&read_text(&cpu_root.join("cpufreq/boost")));
        if boost == "0" || boost == "1" {
            power.turbo = Some(boost == "1");
            power.turbo_source = "boost".into();
        }
    }

    // A physical core is the CPUs the kernel lists as each other's
    // siblings. Not a count of core ids: a device tree numbers the cores of
    // each cluster from zero, so two cores of one socket can share an id.
    let physical = if saw_siblings { distinct(drafts.iter().filter_map(|d| d.siblings.first().copied())) } else { 0 };
    let mut cpus: Vec<LogicalCpu> = drafts.into_iter().map(|d| d.logical).collect();
    cpus.sort_by_key(|c| c.cpu);
    CpuTopology {
        sockets: distinct(cpus.iter().filter_map(|c| c.socket)),
        physical_cores: physical,
        threads: cpus.len() as u16,
        smt: if any_long_siblings { Some(true) } else if saw_siblings { Some(false) } else { None },
        cache_line_bytes: cache_line(&caches),
        numa,
        distances,
        cpus,
        caches,
        power,
        ..CpuTopology::default()
    }
}

#[cfg(any(test, target_os = "linux"))]
struct Draft {
    logical: LogicalCpu,
    driver: String,
    governor: String,
    epp: String,
    scaling_min: u32,
    scaling_max: u32,
    siblings: Vec<u16>,
    caches: Vec<CacheInstance>,
}

#[cfg(any(test, target_os = "linux"))]
struct Cpuinfo {
    vendor: String,
    model: String,
    family: u32,
    model_id: u32,
    stepping: u32,
    microcode: String,
    flags: Vec<String>,
    cache_alignment: u16,
    physical_cores: u16,
    threads: u16,
    sockets: u16,
}

#[cfg(any(test, target_os = "linux"))]
fn parse_cpuinfo(content: &str) -> Cpuinfo {
    let mut info = Cpuinfo {
        vendor: String::new(),
        model: String::new(),
        family: 0,
        model_id: 0,
        stepping: 0,
        microcode: String::new(),
        flags: Vec::new(),
        cache_alignment: 0,
        physical_cores: 0,
        threads: 0,
        sockets: 0,
    };
    let mut cores: Vec<(u32, u32)> = Vec::new();
    let mut sockets: Vec<u32> = Vec::new();
    let mut physical: Option<u32> = None;
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        match key {
            "processor" => {
                info.threads += 1;
                physical = None;
            }
            "vendor_id" if info.vendor.is_empty() => info.vendor = value.to_string(),
            "model name" if info.model.is_empty() => info.model = value.to_string(),
            "cpu family" if info.family == 0 => info.family = value.parse().unwrap_or(0),
            "model" if info.model_id == 0 => info.model_id = value.parse().unwrap_or(0),
            "stepping" if info.stepping == 0 => info.stepping = value.parse().unwrap_or(0),
            "microcode" if info.microcode.is_empty() => info.microcode = value.to_string(),
            "flags" if info.flags.is_empty() => info.flags = keep_flags(value),
            "cache_alignment" if info.cache_alignment == 0 => info.cache_alignment = value.parse().unwrap_or(0),
            "physical id" => {
                physical = value.parse().ok();
                if let Some(id) = physical {
                    if !sockets.contains(&id) {
                        sockets.push(id);
                    }
                }
            }
            "core id" => {
                if let Ok(core) = value.parse::<u32>() {
                    let socket = physical.unwrap_or(0);
                    if !cores.contains(&(socket, core)) {
                        cores.push((socket, core));
                    }
                }
            }
            _ => {}
        }
    }
    info.physical_cores = if cores.is_empty() { info.threads } else { cores.len() as u16 };
    info.sockets = sockets.len() as u16;
    info
}

#[cfg(any(test, target_os = "linux"))]
fn read_numa(dir: &Path) -> Vec<NumaNode> {
    let mut ids: Vec<u16> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str().and_then(|s| s.strip_prefix("node")) else { continue };
            if let Ok(id) = name.parse() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let node = dir.join(format!("node{id}"));
            NumaNode {
                id,
                cpus: crate::sysinfo::parse_cpu_list(&read_text(&node.join("cpulist"))),
                mem_bytes: node_mem_total_bytes(&read_text(&node.join("meminfo"))),
            }
        })
        .collect()
}

/// A node's `MemTotal`, in bytes. A node's `meminfo` is `/proc/meminfo`
/// with the node before each key: `Node 0 MemTotal:       96346812 kB`.
#[cfg(any(test, target_os = "linux"))]
fn node_mem_total_bytes(content: &str) -> u64 {
    content
        .lines()
        .find_map(|line| line.split_once("MemTotal:")?.1.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb.saturating_mul(1024))
}

#[cfg(any(test, target_os = "linux"))]
fn read_distances(dir: &Path, nodes: &[NumaNode]) -> Vec<u16> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for node in nodes {
        let text = read_text(&dir.join(format!("node{}/distance", node.id)));
        let row: Vec<u16> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
        if row.len() != nodes.len() {
            return Vec::new();
        }
        rows.extend(row);
    }
    rows
}

#[cfg(any(test, target_os = "linux"))]
fn read_policies(cpu_root: &Path) -> Vec<CpufreqPolicy> {
    let dir = cpu_root.join("cpufreq");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            name.starts_with("policy").then_some(name)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let policy = dir.join(name);
            CpufreqPolicy {
                cpus: crate::sysinfo::parse_cpu_list(&read_text(&policy.join("affected_cpus"))),
                driver: trim_text(&read_text(&policy.join("scaling_driver"))),
                governor: trim_text(&read_text(&policy.join("scaling_governor"))),
                epp: trim_text(&read_text(&policy.join("energy_performance_preference"))),
                scaling_min_khz: parse_u32(&read_text(&policy.join("scaling_min_freq"))),
                scaling_max_khz: parse_u32(&read_text(&policy.join("scaling_max_freq"))),
            }
        })
        .filter(|p| !p.driver.is_empty() || !p.governor.is_empty())
        .collect()
}

#[cfg(any(test, target_os = "linux"))]
fn policies_from_drafts(drafts: &[Draft]) -> Vec<CpufreqPolicy> {
    let mut out: Vec<CpufreqPolicy> = Vec::new();
    for draft in drafts {
        if draft.driver.is_empty() && draft.governor.is_empty() {
            continue;
        }
        if let Some(policy) = out.iter_mut().find(|p| {
            p.driver == draft.driver && p.governor == draft.governor && p.epp == draft.epp && p.scaling_min_khz == draft.scaling_min && p.scaling_max_khz == draft.scaling_max
        }) {
            policy.cpus.push(draft.logical.cpu);
        } else {
            out.push(CpufreqPolicy {
                cpus: vec![draft.logical.cpu],
                driver: draft.driver.clone(),
                governor: draft.governor.clone(),
                epp: draft.epp.clone(),
                scaling_min_khz: draft.scaling_min,
                scaling_max_khz: draft.scaling_max,
            });
        }
    }
    out
}

#[cfg(any(test, target_os = "linux"))]
fn cache_line(caches: &[CacheInstance]) -> u16 {
    let mut lines: Vec<u16> = caches.iter().filter(|c| c.level <= 1 && c.line > 0).map(|c| c.line).collect();
    if lines.is_empty() {
        lines = caches.iter().filter(|c| c.line > 0).map(|c| c.line).collect();
    }
    lines.sort_unstable();
    lines.dedup();
    lines.first().copied().unwrap_or(0)
}

#[cfg(any(test, target_os = "linux"))]
fn cache_kind(text: &str) -> String {
    match text.trim().to_ascii_lowercase().as_str() {
        "instruction" => "instruction".into(),
        "data" => "data".into(),
        "unified" => "unified".into(),
        other => other.to_string(),
    }
}

#[cfg(any(test, target_os = "linux"))]
fn distinct<T: Ord>(items: impl Iterator<Item = T>) -> u16 {
    let mut v: Vec<T> = items.collect();
    v.sort();
    v.dedup();
    v.len() as u16
}

/// The CPUs that are online, in the order of their numbers. A CPU taken
/// offline, and the second thread of every core under `nosmt`, keeps its
/// `cpuN` directory with no topology, no cache and no cpufreq in it: it is
/// not a thread of the machine as it ran. `online` is the kernel's list;
/// without it, every `cpuN` whose own `online` does not read 0.
#[cfg(any(test, target_os = "linux"))]
fn cpu_ids(cpu_root: &Path) -> Vec<u16> {
    let mut ids = crate::sysinfo::parse_cpu_list(&read_text(&cpu_root.join("online")));
    if ids.is_empty() {
        let Ok(entries) = std::fs::read_dir(cpu_root) else { return Vec::new() };
        ids = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_str()?.strip_prefix("cpu")?.to_string();
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                name.parse().ok()
            })
            .filter(|cpu: &u16| read_text(&cpu_root.join(format!("cpu{cpu}/online"))).trim() != "0")
            .collect();
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(any(test, target_os = "linux"))]
fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(any(test, target_os = "linux"))]
fn trim_text(text: &str) -> String {
    text.trim().to_string()
}

#[cfg(any(test, target_os = "linux"))]
fn parse_u32(text: &str) -> u32 {
    text.trim().parse().unwrap_or(0)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_opt_u16(text: &str) -> Option<u16> {
    let text = text.trim();
    if text.is_empty() { None } else { text.parse().ok() }
}

/// This Mac, from sysctl. `pmset` is asked afterwards ([`ask_programs`]).
#[cfg(target_os = "macos")]
fn read_macos() -> CpuTopology {
    fn sysctl_raw(name: &std::ffi::CStr) -> Vec<u8> {
        let mut len = 0usize;
        unsafe {
            if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 {
                return Vec::new();
            }
            let mut value = vec![0u8; len];
            if libc::sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) != 0 {
                return Vec::new();
            }
            value.truncate(len);
            value
        }
    }
    fn sysctl_string(name: &std::ffi::CStr) -> String {
        let mut bytes = sysctl_raw(name);
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
    fn sysctl_u64(name: &std::ffi::CStr) -> u64 {
        match sysctl_raw(name).as_slice() {
            b if b.len() == 4 => u32::from_ne_bytes(b.try_into().unwrap()) as u64,
            b if b.len() == 8 => u64::from_ne_bytes(b.try_into().unwrap()),
            _ => 0,
        }
    }
    fn named(name: &str) -> std::ffi::CString {
        std::ffi::CString::new(name).unwrap_or_else(|_| std::ffi::CString::new("").unwrap())
    }

    let mut topo = CpuTopology { model: sysctl_string(c"machdep.cpu.brand_string"), ..CpuTopology::default() };
    if topo.model.is_empty() {
        topo.model = sysctl_string(c"hw.model");
    }
    topo.vendor = sysctl_string(c"machdep.cpu.vendor");
    topo.family = sysctl_u64(c"machdep.cpu.family") as u32;
    topo.model_id = sysctl_u64(c"machdep.cpu.model") as u32;
    topo.stepping = sysctl_u64(c"machdep.cpu.stepping") as u32;
    topo.physical_cores = sysctl_u64(c"hw.physicalcpu") as u16;
    topo.threads = sysctl_u64(c"hw.logicalcpu") as u16;
    topo.sockets = sysctl_u64(c"hw.packages") as u16;
    topo.cache_line_bytes = sysctl_u64(c"hw.cachelinesize") as u16;
    topo.ram_bytes = sysctl_u64(c"hw.memsize");
    topo.kernel = sysctl_string(c"kern.osrelease");
    let product = sysctl_string(c"kern.osproductversion");
    topo.os = if product.is_empty() { "macOS".into() } else { format!("macOS {product}") };
    topo.hostname = sysctl_string(c"kern.hostname");
    topo.microcode = sysctl_string(c"machdep.cpu.microcode_version");
    let mut flag_text = String::new();
    for key in [c"machdep.cpu.features", c"machdep.cpu.leaf7_features", c"machdep.cpu.extfeatures"] {
        let part = sysctl_string(key);
        if !part.is_empty() {
            if !flag_text.is_empty() {
                flag_text.push(' ');
            }
            flag_text.push_str(&part);
        }
    }
    topo.flags = keep_flags(&flag_text);
    topo.smt = match (topo.physical_cores, topo.threads) {
        (cores, threads) if cores > 0 && threads > cores => Some(true),
        (cores, threads) if cores > 0 && threads == cores => Some(false),
        _ => None,
    };
    for (level, key, kind) in [
        (1u8, c"hw.l1icachesize", "instruction"),
        (1, c"hw.l1dcachesize", "data"),
        (2, c"hw.l2cachesize", "unified"),
        (3, c"hw.l3cachesize", "unified"),
    ] {
        let bytes = sysctl_u64(key);
        if bytes == 0 {
            continue;
        }
        topo.caches.push(CacheInstance {
            level,
            kind: kind.into(),
            size_kb: (bytes / 1024) as u32,
            ways: 0,
            line: topo.cache_line_bytes,
            cpus: Vec::new(),
        });
    }
    let levels = sysctl_u64(c"hw.nperflevels");
    if levels > 0 {
        let mut notes = Vec::new();
        for i in 0..levels.min(4) {
            let name = sysctl_string(&named(&format!("hw.perflevel{i}.name")));
            let cores = sysctl_u64(&named(&format!("hw.perflevel{i}.physicalcpu")));
            let threads = sysctl_u64(&named(&format!("hw.perflevel{i}.logicalcpu")));
            if name.is_empty() && cores == 0 {
                continue;
            }
            notes.push(format!("{name}: {cores} cores, {threads} threads"));
        }
        if !notes.is_empty() {
            topo.layout.source = "measured".into();
            topo.layout.summary = format!("perf levels ({})", notes.join("; "));
        }
    }
    topo.apply_model_table();
    topo
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::core_kind;

    struct Tree(std::path::PathBuf);

    impl Tree {
        fn new(name: &str) -> Tree {
            let dir = std::env::temp_dir().join(format!("perfr-topo-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tree(dir)
        }

        fn file(&self, rel: &str, content: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cpuinfo_block(cpu: u16, vendor: &str, model: &str, family: u32, id: u32, stepping: u32, microcode: &str, socket: u32, core: u32, flags: &str) -> String {
        format!(
            "processor\t: {cpu}\nvendor_id\t: {vendor}\ncpu family\t: {family}\nmodel\t\t: {id}\nmodel name\t: {model}\nstepping\t: {stepping}\nmicrocode\t: {microcode}\ncache_alignment\t: 64\nphysical id\t: {socket}\ncore id\t\t: {core}\nflags\t\t: {flags}\n\n"
        )
    }

    /// One logical CPU: SMT siblings, die, frequencies and a four-level cache.
    fn add_cpu(tree: &Tree, cpu: u16, socket: u16, die: u16, core: u16, siblings: &str, max: &str, base: &str, min: &str, cur: &str, l3: &str, l3_shared: &str, l2_shared: &str) {
        let dir = format!("sys/devices/system/cpu/cpu{cpu}");
        tree.file(&format!("{dir}/topology/physical_package_id"), &format!("{socket}\n"));
        tree.file(&format!("{dir}/topology/die_id"), &format!("{die}\n"));
        tree.file(&format!("{dir}/topology/core_id"), &format!("{core}\n"));
        tree.file(&format!("{dir}/topology/thread_siblings_list"), &format!("{siblings}\n"));
        tree.file(&format!("{dir}/cpufreq/cpuinfo_max_freq"), &format!("{max}\n"));
        tree.file(&format!("{dir}/cpufreq/base_frequency"), &format!("{base}\n"));
        tree.file(&format!("{dir}/cpufreq/cpuinfo_min_freq"), &format!("{min}\n"));
        tree.file(&format!("{dir}/cpufreq/scaling_cur_freq"), &format!("{cur}\n"));
        tree.file(&format!("{dir}/cpufreq/scaling_driver"), "intel_pstate\n");
        tree.file(&format!("{dir}/cpufreq/scaling_governor"), "powersave\n");
        tree.file(&format!("{dir}/cpufreq/energy_performance_preference"), "balance_performance\n");
        tree.file(&format!("{dir}/cpufreq/scaling_min_freq"), &format!("{min}\n"));
        tree.file(&format!("{dir}/cpufreq/scaling_max_freq"), &format!("{max}\n"));
        let cache = |index: u8, level: u8, kind: &str, size: &str, ways: u8, shared: &str| {
            let c = format!("{dir}/cache/index{index}");
            tree.file(&format!("{c}/level"), &format!("{level}\n"));
            tree.file(&format!("{c}/type"), &format!("{kind}\n"));
            tree.file(&format!("{c}/size"), &format!("{size}\n"));
            tree.file(&format!("{c}/ways_of_associativity"), &format!("{ways}\n"));
            tree.file(&format!("{c}/coherency_line_size"), "64\n");
            tree.file(&format!("{c}/shared_cpu_list"), &format!("{shared}\n"));
        };
        cache(0, 1, "Data", "32K", 8, &cpu.to_string());
        cache(1, 1, "Instruction", "32K", 8, &cpu.to_string());
        cache(2, 2, "Unified", "256K", 4, l2_shared);
        cache(3, 3, "Unified", l3, 12, l3_shared);
    }

    fn common_head(tree: &Tree, cpuinfo: &str, mem_kb: u64) {
        tree.file("proc/cpuinfo", cpuinfo);
        tree.file("proc/meminfo", &format!("MemTotal:       {mem_kb} kB\n"));
        tree.file("proc/sys/kernel/osrelease", "6.8.0-test\n");
        tree.file("proc/sys/kernel/hostname", "fixture\n");
    }

    #[test]
    fn a_uniform_intel_keeps_frequency_smt_numa_and_power_when_nothing_differs() {
        let tree = Tree::new("uniform");
        let model = "Intel(R) Core(TM) i7-8700 CPU @ 3.20GHz";
        let flags = "fpu sse4_2 avx avx2 hypervisor";
        let mut cpuinfo = String::new();
        for (cpu, core) in [(0u16, 0u32), (1, 0), (2, 1), (3, 1)] {
            cpuinfo.push_str(&cpuinfo_block(cpu, "GenuineIntel", model, 6, 158, 10, "0xf0", 0, core, flags));
        }
        common_head(&tree, &cpuinfo, 16_777_216);
        for (cpu, core, sibs) in [(0u16, 0u16, "0-1"), (1, 0, "0-1"), (2, 1, "2-3"), (3, 1, "2-3")] {
            add_cpu(&tree, cpu, 0, 0, core, sibs, "4600000", "3200000", "800000", "3200000", "12288K", "0-3", sibs);
        }
        tree.file("sys/devices/system/cpu/cpufreq/policy0/affected_cpus", "0-3\n");
        tree.file("sys/devices/system/cpu/cpufreq/policy0/scaling_driver", "intel_pstate\n");
        tree.file("sys/devices/system/cpu/cpufreq/policy0/scaling_governor", "powersave\n");
        tree.file("sys/devices/system/cpu/cpufreq/policy0/energy_performance_preference", "balance_performance\n");
        tree.file("sys/devices/system/cpu/cpufreq/policy0/scaling_min_freq", "800000\n");
        tree.file("sys/devices/system/cpu/cpufreq/policy0/scaling_max_freq", "4600000\n");
        tree.file("sys/devices/system/cpu/intel_pstate/status", "active\n");
        tree.file("sys/devices/system/cpu/intel_pstate/no_turbo", "0\n");
        tree.file("sys/firmware/acpi/platform_profile", "balanced\n");
        tree.file("sys/devices/system/node/node0/cpulist", "0-3\n");
        tree.file("sys/devices/system/node/node0/meminfo", "Node 0 MemTotal:       16777216 kB\nNode 0 MemFree:         1234567 kB\n");
        tree.file("sys/devices/system/node/node0/distance", "10\n");
        let topo = read_linux_tree(&tree.0, "balanced\n");
        assert_eq!(topo.model, model);
        assert_eq!(topo.vendor, "GenuineIntel");
        assert_eq!((topo.family, topo.model_id, topo.stepping), (6, 158, 10));
        assert_eq!(topo.microcode, "0xf0");
        assert_eq!(topo.flags, ["sse4_2", "avx", "avx2", "hypervisor"]);
        assert_eq!(topo.sockets, 1);
        assert_eq!(topo.physical_cores, 2);
        assert_eq!(topo.threads, 4);
        assert_eq!(topo.smt, Some(true));
        assert_eq!(topo.cache_line_bytes, 64);
        assert_eq!(topo.nps, 1);
        assert_eq!(topo.nps_source, "derived");
        assert_eq!(topo.numa[0].mem_bytes, 16_777_216 * 1024, "a node's meminfo names the node before the key");
        assert!(topo.summary().contains("NUMA node 0: cpus 0-3, 16.0 GB"));
        assert!(topo.cpus.iter().all(|c| c.kind == core_kind::UNKNOWN));
        assert!(topo.cpus.iter().all(|c| c.max_freq_khz == 4_600_000 && c.l3_kb == 12_288));
        assert_eq!(topo.lane_label(0).as_deref(), Some("C0 · T0"));
        assert_eq!(topo.lane_label(1).as_deref(), Some("C0 · T1"));
        assert_eq!(topo.freq_label(0).as_deref(), Some("3.2–4.6"));
        assert_eq!(topo.cpus[2].cur_freq_khz, 3_200_000);
        assert_eq!(topo.power.pstate_status, "active");
        assert_eq!(topo.power.turbo, Some(true));
        assert_eq!(topo.power.policies[0].governor, "powersave");
        assert_eq!(topo.power.policies[0].epp, "balance_performance");
        assert_eq!(topo.power.platform_profile, "balanced");
        assert_eq!(topo.power.daemon_profile, "balanced");
        assert!(topo.header_line().contains("powersave"));
        assert!(topo.header_line().contains("boost on"));
        assert!(topo.header_line().contains("NPS1"));
        assert!(topo.summary().contains("cache line 64 B"));
        assert!(topo.layout.summary.is_empty());
        assert_eq!(topo.product_page(), None);
        let l1i = topo.caches.iter().find(|c| c.level == 1 && c.kind == "instruction").unwrap();
        assert_eq!(l1i.ways, 8);
        assert_eq!(l1i.line, 64);
        assert_eq!(l1i.cpus, vec![0]);
    }

    #[test]
    fn a_raptor_lake_lists_performance_and_efficiency_threads_without_inventing_the_full_part() {
        // Two performance cores (four threads) and two efficiency cores,
        // the shape of a 13900K cut down so the model table must refuse it.
        let tree = Tree::new("raptor");
        let model = "Intel(R) Core(TM) i9-13900K CPU @ 3.00GHz";
        let mut cpuinfo = String::new();
        for (cpu, core) in [(0u16, 0u32), (1, 0), (2, 1), (3, 1), (4, 2), (5, 3)] {
            cpuinfo.push_str(&cpuinfo_block(cpu, "GenuineIntel", model, 6, 183, 1, "0x129", 0, core, "avx2"));
        }
        common_head(&tree, &cpuinfo, 32_000_000);
        for (cpu, core, sibs, max) in [(0u16, 0u16, "0-1", "5800000"), (1, 0, "0-1", "5800000"), (2, 1, "2-3", "5500000"), (3, 1, "2-3", "5500000")] {
            add_cpu(&tree, cpu, 0, 0, core, sibs, max, "3000000", "800000", max, "36864K", "0-5", sibs);
        }
        for (cpu, core) in [(4u16, 2u16), (5, 3)] {
            add_cpu(&tree, cpu, 0, 0, core, &cpu.to_string(), "4300000", "2200000", "800000", "4300000", "36864K", "0-5", &cpu.to_string());
        }
        tree.file("sys/devices/cpu_core/cpus", "0-3\n");
        tree.file("sys/devices/cpu_atom/cpus", "4-5\n");
        tree.file("sys/devices/system/cpu/intel_pstate/status", "active\n");
        tree.file("sys/devices/system/cpu/intel_pstate/no_turbo", "0\n");
        tree.file("sys/devices/system/node/node0/cpulist", "0-5\n");
        tree.file("sys/devices/system/node/node0/distance", "10\n");
        tree.file("sys/devices/system/node/node0/meminfo", "Node 0 MemTotal: 32000000 kB\n");
        let topo = read_linux_tree(&tree.0, "");
        assert!(topo.cpus[..4].iter().all(|c| c.kind == core_kind::PERFORMANCE));
        assert!(topo.cpus[4..].iter().all(|c| c.kind == core_kind::EFFICIENCY));
        assert_eq!(topo.lane_label(0).as_deref(), Some("C0 · T0"));
        assert_eq!(topo.lane_label(1).as_deref(), Some("C0 · T1"));
        assert_eq!(topo.lane_label(4).as_deref(), Some("C2"));
        assert_eq!(topo.smt, Some(true));
        assert_eq!(topo.physical_cores, 4);
        assert!(topo.layout.summary.is_empty(), "four cores are not a 13900K's 24, so the table stays out");
        assert_eq!(topo.product_page(), None);
        assert_eq!(topo.power.turbo, Some(true));
        assert_eq!(topo.cpu_detail(4).unwrap().title, "CPU 4 · Efficiency core");
    }

    #[test]
    fn a_two_ccd_x3d_records_the_dies_and_the_two_level_3_sizes() {
        let tree = Tree::new("x3d");
        let model = "AMD Ryzen 9 7950X3D 16-Core Processor";
        let mut cpuinfo = String::new();
        for (cpu, core) in [(0u16, 0u32), (1, 0), (2, 8), (3, 8)] {
            cpuinfo.push_str(&cpuinfo_block(cpu, "AuthenticAMD", model, 25, 97, 2, "0x0a601206", 0, core, "avx avx2 avx512f"));
        }
        common_head(&tree, &cpuinfo, 64_000_000);
        for (cpu, core, sibs, l3, shared, die, max) in [
            (0u16, 0u16, "0-1", "98304K", "0-1", 0u16, "5050000"),
            (1, 0, "0-1", "98304K", "0-1", 0, "5050000"),
            (2, 8, "2-3", "32768K", "2-3", 1, "5750000"),
            (3, 8, "2-3", "32768K", "2-3", 1, "5750000"),
        ] {
            add_cpu(&tree, cpu, 0, die, core, sibs, max, "4200000", "400000", "4200000", l3, shared, sibs);
        }
        tree.file("sys/devices/system/cpu/amd_pstate/status", "active\n");
        tree.file("sys/devices/system/cpu/cpufreq/boost", "1\n");
        tree.file("sys/devices/system/node/node0/cpulist", "0-3\n");
        tree.file("sys/devices/system/node/node0/distance", "10\n");
        tree.file("sys/devices/system/node/node0/meminfo", "Node 0 MemTotal: 64000000 kB\n");
        let topo = read_linux_tree(&tree.0, "nope\n");
        assert_eq!(topo.power.daemon_profile, "");
        assert_eq!(topo.power.pstate, "amd_pstate");
        assert_eq!(topo.power.turbo, Some(true));
        assert_eq!(topo.power.turbo_source, "boost");
        assert_eq!(topo.cpus[0].die, Some(0));
        assert_eq!(topo.cpus[0].l3_kb, 98_304);
        assert_eq!(topo.cpus[2].die, Some(1));
        assert_eq!(topo.cpus[2].l3_kb, 32_768);
        // The system calls the second core 8; the lanes count them.
        assert_eq!(topo.lane_label(3).as_deref(), Some("C1 · T1"));
        assert_eq!(topo.cpu_detail(3).unwrap().value("Physical core"), Some("1, thread 1 (system core id 8)"));
        let mut l3s: Vec<_> = topo.caches.iter().filter(|c| c.level == 3).map(|c| c.size_kb).collect();
        l3s.sort_unstable();
        assert_eq!(l3s, vec![32_768, 98_304]);
        assert!(topo.layout.summary.is_empty(), "two cores do not get the 16-core CCD sentence");
    }

    #[test]
    fn a_dual_socket_epyc_derives_nps4_from_the_node_map() {
        // Two sockets, four nodes each, one logical CPU per node. Small on
        // purpose: the distances and the ratio are what NPS4 is.
        let tree = Tree::new("epyc");
        let model = "AMD EPYC 9654 96-Core Processor";
        let mut cpuinfo = String::new();
        for cpu in 0..8u16 {
            let socket = u32::from(cpu / 4);
            cpuinfo.push_str(&cpuinfo_block(cpu, "AuthenticAMD", model, 25, 17, 1, "0x0a10113e", socket, u32::from(cpu % 4), "avx2"));
        }
        common_head(&tree, &cpuinfo, 256_000_000);
        for cpu in 0..8u16 {
            let socket = cpu / 4;
            add_cpu(&tree, cpu, socket, cpu % 4, cpu % 4, &cpu.to_string(), "3700000", "2400000", "1500000", "2400000", "32768K", &cpu.to_string(), &cpu.to_string());
            // One thread per core: the sibling list is the CPU itself, so
            // SMT is off on this fixture even though a real 9654 has it.
            // The test is the node map.
            let _ = cpu;
        }
        for node in 0..8u16 {
            let mut distance = Vec::new();
            for other in 0..8u16 {
                let local = if other == node { 10 } else if other / 4 == node / 4 { 16 } else { 32 };
                distance.push(local.to_string());
            }
            tree.file(&format!("sys/devices/system/node/node{node}/cpulist"), &format!("{node}\n"));
            tree.file(&format!("sys/devices/system/node/node{node}/distance"), &format!("{}\n", distance.join(" ")));
            tree.file(&format!("sys/devices/system/node/node{node}/meminfo"), &format!("Node {node} MemTotal:       32000000 kB\n"));
        }
        let topo = read_linux_tree(&tree.0, "");
        assert_eq!(topo.sockets, 2);
        assert_eq!(topo.numa.len(), 8);
        assert!(topo.numa.iter().all(|node| node.mem_bytes == 32_000_000 * 1024));
        assert_eq!(topo.nps, 4);
        assert_eq!(topo.nps_source, "derived");
        assert_eq!(topo.distances.len(), 64);
        assert_eq!(topo.distances[0], 10);
        assert_eq!(topo.distances[1], 16);
        assert_eq!(topo.distances[4], 32);
        assert_eq!(topo.cpus[0].socket, Some(0));
        assert_eq!(topo.cpus[0].numa, Some(0));
        assert_eq!(topo.cpus[5].socket, Some(1));
        assert_eq!(topo.cpus[5].numa, Some(5));
        assert_eq!(topo.smt, Some(false));
        assert!(topo.lane_label(0).is_none(), "SMT is off, so the lane keeps Core N");
        assert!(topo.layout.summary.is_empty(), "eight threads are not a 96-core Genoa");
        assert!(topo.summary().contains("NPS 4 (derived)"));
        assert_eq!(topo.hostname, "fixture");
        assert_eq!(topo.os, "Linux");
    }

    #[test]
    fn an_empty_tree_is_an_empty_machine() {
        let tree = Tree::new("empty");
        assert!(read_linux_tree(&tree.0, "").is_empty());
    }

    #[test]
    fn cpus_that_are_offline_are_not_threads_of_the_machine() {
        // Two cores of two threads, booted with `nosmt`: the second thread
        // of each keeps its directory, with `online` and nothing else.
        let tree = Tree::new("nosmt");
        let model = "AMD Ryzen 5 5600X 6-Core Processor";
        let mut cpuinfo = String::new();
        for (cpu, core) in [(0u16, 0u32), (1, 1)] {
            cpuinfo.push_str(&cpuinfo_block(cpu, "AuthenticAMD", model, 25, 33, 0, "0xa201016", 0, core, "avx2"));
        }
        common_head(&tree, &cpuinfo, 16_000_000);
        for (cpu, core) in [(0u16, 0u16), (1, 1)] {
            add_cpu(&tree, cpu, 0, 0, core, &cpu.to_string(), "4650000", "3700000", "2200000", "3700000", "32768K", "0-1", &cpu.to_string());
        }
        for cpu in [2u16, 3] {
            tree.file(&format!("sys/devices/system/cpu/cpu{cpu}/online"), "0\n");
        }
        tree.file("sys/devices/system/cpu/online", "0-1\n");
        tree.file("sys/devices/system/cpu/offline", "2-3\n");
        let topo = read_linux_tree(&tree.0, "");
        assert_eq!(topo.cpus.iter().map(|c| c.cpu).collect::<Vec<_>>(), [0, 1]);
        assert_eq!((topo.sockets, topo.physical_cores, topo.threads), (1, 2, 2));
        assert_eq!(topo.smt, Some(false));
        assert!(topo.cpus.iter().all(|c| c.socket == Some(0)), "no CPU without a socket to draw as a second one");
        // A tree with no list of its own: each CPU's `online` says.
        std::fs::remove_file(tree.0.join("sys/devices/system/cpu/online")).unwrap();
        let topo = read_linux_tree(&tree.0, "");
        assert_eq!(topo.cpus.iter().map(|c| c.cpu).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(topo.threads, 2);
    }

    #[test]
    fn cores_are_counted_by_their_siblings_where_clusters_number_them_alike() {
        // A big.LITTLE part as a device tree describes it: one package, two
        // clusters, and each cluster's first core is core 0.
        let tree = Tree::new("clusters");
        for (cpu, cluster, capacity) in [(0u16, 0u16, "430"), (1, 0, "430"), (2, 1, "1024"), (3, 1, "1024")] {
            let dir = format!("sys/devices/system/cpu/cpu{cpu}");
            tree.file(&format!("{dir}/topology/physical_package_id"), "0\n");
            tree.file(&format!("{dir}/topology/cluster_id"), &format!("{cluster}\n"));
            tree.file(&format!("{dir}/topology/core_id"), &format!("{}\n", cpu % 2));
            tree.file(&format!("{dir}/topology/core_cpus_list"), &format!("{cpu}\n"));
            tree.file(&format!("{dir}/cpu_capacity"), &format!("{capacity}\n"));
        }
        let topo = read_devices(&tree.0.join("sys/devices"));
        assert_eq!((topo.sockets, topo.physical_cores, topo.threads), (1, 4, 4));
        assert_eq!(topo.smt, Some(false));
        assert_eq!(topo.cpus[0].kind, core_kind::EFFICIENCY);
        assert_eq!(topo.cpus[2].kind, core_kind::PERFORMANCE);
        assert!(!topo.cpus[0].same_core(&topo.cpus[2]), "core 0 of one cluster is not core 0 of the other");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_program_is_asked_for_no_longer_than_its_limit() {
        use std::time::{Duration, Instant};
        assert_eq!(run_bounded("sh", &["-c", "echo balanced"], Duration::from_secs(10)).as_deref(), Some("balanced\n"));
        // One that fails, and one that is not there, say nothing.
        assert_eq!(run_bounded("sh", &["-c", "echo no; exit 3"], Duration::from_secs(10)), None);
        assert_eq!(run_bounded("perfr-no-such-program", &[], Duration::from_secs(10)), None);
        // One that hangs is given up on and killed.
        let began = Instant::now();
        assert_eq!(run_bounded("sh", &["-c", "sleep 30"], Duration::from_millis(50)), None);
        assert!(began.elapsed() < Duration::from_secs(5), "waited {:?}", began.elapsed());
    }

    /// The lane marks of a tree of `devices`' files.
    fn marks(name: &str, files: &[(String, &str)]) -> Vec<perfr_live_event::CoreInfo> {
        let tree = Tree::new(name);
        for (path, content) in files {
            tree.file(path, content);
        }
        read_devices(&tree.0).core_infos()
    }

    /// One core's files: its top frequency and its caches, a level 2 at
    /// `index2` and the level 3 at `index3`.
    fn core_files(cpu: u16, max_freq: &'static str, l3: &'static str) -> Vec<(String, &'static str)> {
        let dir = format!("system/cpu/cpu{cpu}");
        vec![
            (format!("{dir}/cpufreq/cpuinfo_max_freq"), max_freq),
            (format!("{dir}/cache/index2/level"), "2\n"),
            (format!("{dir}/cache/index2/size"), "2048K\n"),
            (format!("{dir}/cache/index3/level"), "3\n"),
            (format!("{dir}/cache/index3/size"), l3),
        ]
    }

    fn mark(cpu: u16, kind: u8, max_freq_khz: u32, l3_kb: u32) -> perfr_live_event::CoreInfo {
        perfr_live_event::CoreInfo { cpu, kind, max_freq_khz, l3_kb }
    }

    #[test]
    fn a_hybrid_machine_tells_its_performance_cores_from_its_efficiency_cores() {
        // Two P-cores, one of them favoured with a higher top frequency,
        // and two E-cores, as a 13th generation Core lays them out. cpu10
        // is listed after cpu2 by name and is the last by number.
        let mut files = Vec::new();
        files.extend(core_files(0, "5500000\n", "36864K\n"));
        files.extend(core_files(1, "5800000\n", "36864K\n"));
        files.extend(core_files(2, "4300000\n", "36864K\n"));
        files.extend(core_files(10, "4300000\n", "36864K\n"));
        files.push(("cpu_core/cpus".into(), "0-1\n"));
        files.push(("cpu_atom/cpus".into(), "2,10\n"));
        // What is in the directory beside the cores.
        files.push(("system/cpu/cpufreq/policy0/scaling_governor".into(), "powersave\n"));
        files.push(("system/cpu/online".into(), "0-2,10\n"));
        assert_eq!(
            marks("hybrid", &files),
            [
                mark(0, core_kind::PERFORMANCE, 5_500_000, 36_864),
                mark(1, core_kind::PERFORMANCE, 5_800_000, 36_864),
                mark(2, core_kind::EFFICIENCY, 4_300_000, 36_864),
                mark(10, core_kind::EFFICIENCY, 4_300_000, 36_864),
            ]
        );
    }

    #[test]
    fn a_machine_of_one_kind_of_core_names_no_kind_and_keeps_what_differs() {
        // An X3D Ryzen: the stacked cache on the first core, the higher
        // frequency on the second, and no kind for the kernel to name.
        let mut files = Vec::new();
        files.extend(core_files(0, "5050000\n", "98304K\n"));
        files.extend(core_files(1, "5759000\n", "32768K\n"));
        assert_eq!(marks("x3d-marks", &files), [mark(0, core_kind::UNKNOWN, 5_050_000, 98_304), mark(1, core_kind::UNKNOWN, 5_759_000, 32_768)]);
    }

    #[test]
    fn the_capacities_of_an_asymmetric_machine_name_its_big_and_little_cores() {
        let files = vec![
            ("system/cpu/cpu0/cpu_capacity".to_string(), "430\n"),
            ("system/cpu/cpu0/cpufreq/cpuinfo_max_freq".to_string(), "1800000\n"),
            ("system/cpu/cpu1/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu1/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
        ];
        assert_eq!(marks("biglittle", &files), [mark(0, core_kind::EFFICIENCY, 1_800_000, 0), mark(1, core_kind::PERFORMANCE, 2_400_000, 0)]);
        // The same capacity everywhere is one kind of core, not named.
        let files = vec![
            ("system/cpu/cpu0/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu0/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
            ("system/cpu/cpu1/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu1/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
        ];
        assert!(marks("symmetric", &files).iter().all(|c| c.kind == core_kind::UNKNOWN));
    }

    #[test]
    fn a_machine_that_tells_nothing_of_its_cores_has_none_to_show() {
        // A container: the cores are there, with no frequency and no cache.
        let files = vec![("system/cpu/cpu0/online".to_string(), "1\n"), ("system/cpu/cpu1/online".to_string(), "1\n")];
        assert!(marks("bare", &files).is_empty());
        assert!(read_devices(std::path::Path::new("/nonexistent/perfr")).core_infos().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_marks_are_this_machines_or_none() {
        let cores = read_this_machine().core_infos();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        // One entry per CPU that is online, in order; a restricted affinity
        // makes `threads` the smaller of the two.
        assert!(cores.is_empty() || cores.len() >= threads, "{} cores for {threads} threads", cores.len());
        assert!(cores.windows(2).all(|w| w[0].cpu < w[1].cpu));
    }
}
