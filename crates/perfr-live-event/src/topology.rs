// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the captured machine is, beyond the lane marks.
//!
//! [`crate::CoreInfo`] is the small record the scheduler lanes were first
//! marked with: a kind, a top frequency, a level 3 size. This is the rest
//! of the machine, sent once per capture in its own frame and stored with
//! the bundle. Every field is optional. A capture recorded before the frame
//! existed has none of it, and a reader that does not know a field ignores
//! it: the JSON omits what was not known, and missing keys decode as the
//! default.
//!
//! Nothing here is sampled over the capture. A frequency is the number the
//! system reported at the start. A layout sentence comes from a small table
//! of product names and is marked as such; the caches, the nodes and the
//! pairing are whatever the system enumerated.

use serde::{Deserialize, Serialize};

use crate::{core_kind, CoreInfo};

fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}
fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}
fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}
fn is_zero_u8(n: &u8) -> bool {
    *n == 0
}

/// One logical CPU. A missing `Option` and a zero number are a fact the
/// capture does not have.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalCpu {
    pub cpu: u16,
    /// One of [`crate::core_kind`]. Zero is not told.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub kind: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa: Option<u16>,
    /// `topology/die_id`, or the die record on Windows. Not a CCD number
    /// invented from the model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub die: Option<u16>,
    /// `topology/cluster_id` where the kernel has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<u16>,
    /// The physical core, `topology/core_id` or the core record's index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_id: Option<u16>,
    /// Which logical thread of that core this CPU is, counting from 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<u16>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub max_freq_khz: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub base_freq_khz: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub min_freq_khz: u32,
    /// `scaling_cur_freq` at capture start. Not a series.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cur_freq_khz: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub l1i_kb: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub l1d_kb: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub l2_kb: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub l3_kb: u32,
}

/// One NUMA node and the logical CPUs the kernel places on it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumaNode {
    pub id: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpus: Vec<u16>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub mem_bytes: u64,
}

/// One cache instance: the CPUs in `cpus` share it. An empty `cpus` means
/// the size was reported for the machine and the sharing was not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInstance {
    pub level: u8,
    /// `instruction`, `data` or `unified`, as the system spelled the type.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub size_kb: u32,
    /// Ways. `0xFF` is the fully-associative value Windows writes.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub ways: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub line: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpus: Vec<u16>,
}

/// A sentence that did not come from enumerating the machine, or a short
/// measured note that is not a per-CPU fact (Apple's perf levels).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutNote {
    /// `lookup` for the model table, `measured` for something the system
    /// reported in a form that is not a per-CPU field.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// CCD count from the model table. Zero when the table was not applied
    /// or does not say. Not a count of `die` values.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub ccds: u16,
    /// Memory channels from the model table. Zero when not in the table.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub memory_channels: u16,
}

impl LayoutNote {
    fn is_empty(&self) -> bool {
        self.summary.is_empty() && self.ccds == 0 && self.memory_channels == 0
    }
}

/// One cpufreq policy: the CPUs that share a driver and a governor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpufreqPolicy {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpus: Vec<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub driver: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub governor: String,
    /// `energy_performance_preference`, when the driver has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub epp: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub scaling_min_khz: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub scaling_max_khz: u32,
}

/// Power and performance mode at capture start. Whichever platform produced
/// the capture fills the fields it could read and leaves the others out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<CpufreqPolicy>,
    /// `intel_pstate` or `amd_pstate` when that directory exists.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pstate: String,
    /// `active`, `passive`, `guided` or `off`, from the driver's status file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pstate_status: String,
    /// Turbo or core boost enabled. The sense follows `turbo_source`:
    /// `no_turbo` of 0 is on, `boost` of 1 is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turbo: Option<bool>,
    /// `no_turbo` or `boost`: which file `turbo` was read from.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub turbo_source: String,
    /// `/sys/firmware/acpi/platform_profile`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform_profile: String,
    /// What `powerprofilesctl get` printed, when that program ran and
    /// printed one of its three profile names. Empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub daemon_profile: String,
    /// Windows active power plan, friendly name when powrprof returned one,
    /// otherwise the well-known name of the GUID.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan_guid: String,
    /// Windows overlay power mode, when the call succeeded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overlay: String,
    /// Windows processor boost mode, the AC index as a name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub boost_mode: String,
    /// macOS Low Power Mode from `pmset`, when the key was present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_power: Option<bool>,
    /// macOS High Power Mode from `pmset`, when the key was present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high_power: Option<bool>,
    /// macOS: `Some(true)` on AC, `Some(false)` on battery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_ac: Option<bool>,
}

impl PowerState {
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
            && self.pstate.is_empty()
            && self.pstate_status.is_empty()
            && self.turbo.is_none()
            && self.platform_profile.is_empty()
            && self.daemon_profile.is_empty()
            && self.plan.is_empty()
            && self.plan_guid.is_empty()
            && self.overlay.is_empty()
            && self.boost_mode.is_empty()
            && self.low_power.is_none()
            && self.high_power.is_none()
            && self.on_ac.is_none()
    }
}

/// The captured machine. One value per machine of a capture; a hub that
/// joined another machine keeps each one, with that machine's CPU numbers
/// shifted onto the lanes it draws as.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuTopology {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub family: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub model_id: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub stepping: u32,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub sockets: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub physical_cores: u16,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub threads: u16,
    /// `Some(true)` when a physical core has more than one logical CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub cache_line_bytes: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub microcode: String,
    /// A short list, not every flag the kernel printed. See [`keep_flags`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kernel: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// The hostname. The capture's `SystemInfo` event already records it;
    /// it is repeated here so a copied summary names the machine. Empty
    /// when it was not read.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hostname: String,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub ram_bytes: u64,
    /// Nodes per socket, `numa.len() / sockets` when that divides evenly.
    /// Zero when it could not be derived. Not taken from the model table:
    /// on the parts where it is a firmware setting, the table does not know
    /// which setting the machine booted with.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub nps: u8,
    /// `derived` when [`Self::nps`] was computed from the node map.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nps_source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub numa: Vec<NumaNode>,
    /// Row-major node distances, `numa.len()` squared, in node-id order.
    /// Empty when a node's distance file was missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub distances: Vec<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpus: Vec<LogicalCpu>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<CacheInstance>,
    #[serde(default, skip_serializing_if = "LayoutNote::is_empty")]
    pub layout: LayoutNote,
    #[serde(default, skip_serializing_if = "PowerState::is_empty")]
    pub power: PowerState,
    /// A product page, set only when the model string and the core and
    /// thread counts matched one table entry that has a URL. Empty
    /// otherwise, including when the layout sentence matched but the URL
    /// was not one that had been checked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reference_url: String,
}

impl CpuTopology {
    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
            && self.vendor.is_empty()
            && self.cpus.is_empty()
            && self.numa.is_empty()
            && self.power.is_empty()
            && self.ram_bytes == 0
            && self.kernel.is_empty()
            && self.flags.is_empty()
            && self.layout.is_empty()
    }

    /// The lane marks this topology supports. Empty when no CPU has a kind,
    /// a top frequency or a level 3 size, which is the same bar the older
    /// core list used so a machine that told nothing still sends nothing.
    pub fn core_infos(&self) -> Vec<CoreInfo> {
        let cores: Vec<CoreInfo> = self
            .cpus
            .iter()
            .map(|cpu| CoreInfo {
                cpu: cpu.cpu,
                kind: cpu.kind,
                max_freq_khz: cpu.max_freq_khz,
                l3_kb: cpu.l3_kb,
            })
            .collect();
        if cores.iter().all(|c| c.kind == core_kind::UNKNOWN && c.max_freq_khz == 0 && c.l3_kb == 0) {
            Vec::new()
        } else {
            cores
        }
    }

    /// Move every CPU number up by `base` lanes, dropping a CPU that would
    /// not fit in a lane number (the scheduler's lane id is one byte).
    /// Socket and node ids are left as the machine numbered them: the
    /// diagram draws each machine as its own block.
    pub fn shift_lanes(&self, base: u32) -> CpuTopology {
        if base == 0 {
            return self.clone();
        }
        let map = |cpu: u16| -> Option<u16> {
            u8::try_from(base.saturating_add(u32::from(cpu))).ok().map(u16::from)
        };
        let mut out = self.clone();
        out.cpus = self
            .cpus
            .iter()
            .filter_map(|cpu| {
                let mut cpu = cpu.clone();
                cpu.cpu = map(cpu.cpu)?;
                Some(cpu)
            })
            .collect();
        let remap = |cpus: &[u16]| -> Vec<u16> { cpus.iter().filter_map(|c| map(*c)).collect() };
        for node in &mut out.numa {
            node.cpus = remap(&node.cpus);
        }
        for cache in &mut out.caches {
            cache.cpus = remap(&cache.cpus);
        }
        for policy in &mut out.power.policies {
            policy.cpus = remap(&policy.cpus);
        }
        out
    }

    /// Fill [`Self::layout`] and [`Self::reference_url`] from the model
    /// table when the model string and the counts agree. Measured fields
    /// are not changed. A layout note that is already `measured` is kept
    /// and the table's sentence is appended, so a perf-level list is not
    /// replaced by a product blurb.
    pub fn apply_model_table(&mut self) {
        let Some(known) = lookup_model(&self.model, self.physical_cores, self.threads) else {
            return;
        };
        if self.layout.summary.is_empty() {
            self.layout.source = "lookup".into();
            self.layout.summary = known.note.to_string();
        } else if self.layout.source != "lookup" {
            self.layout.summary = format!("{}\nFrom a model table: {}", self.layout.summary, known.note);
            self.layout.source = "mixed".into();
        }
        self.layout.ccds = known.ccds;
        self.layout.memory_channels = known.memory_channels;
        if !known.url.is_empty() {
            self.reference_url = known.url.to_string();
        }
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_json(bytes: &[u8]) -> Result<CpuTopology, String> {
        serde_json::from_slice(bytes).map_err(|e| e.to_string())
    }

    /// One line for the scheduler header: the governor or the plan, boost,
    /// and nodes per socket. Empty when none of that was recorded.
    pub fn header_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut governors: Vec<&str> = self.power.policies.iter().map(|p| p.governor.as_str()).filter(|g| !g.is_empty()).collect();
        governors.sort_unstable();
        governors.dedup();
        match governors.as_slice() {
            [one] => parts.push((*one).to_string()),
            [_, ..] => parts.push("mixed governors".into()),
            [] => {}
        }
        match self.power.turbo {
            Some(true) => parts.push("boost on".into()),
            Some(false) => parts.push("boost off".into()),
            None => {}
        }
        if !self.power.platform_profile.is_empty() {
            parts.push(self.power.platform_profile.clone());
        }
        if !self.power.daemon_profile.is_empty() && self.power.daemon_profile != self.power.platform_profile {
            parts.push(self.power.daemon_profile.clone());
        }
        if !self.power.plan.is_empty() {
            parts.push(self.power.plan.clone());
        }
        if !self.power.overlay.is_empty() {
            parts.push(self.power.overlay.clone());
        }
        if !self.power.boost_mode.is_empty() {
            parts.push(format!("boost {}", self.power.boost_mode));
        }
        if self.power.low_power == Some(true) {
            parts.push("low power".into());
        }
        if self.power.high_power == Some(true) {
            parts.push("high power".into());
        }
        match self.power.on_ac {
            Some(true) => parts.push("AC".into()),
            Some(false) => parts.push("battery".into()),
            None => {}
        }
        if self.nps > 0 {
            parts.push(format!("NPS{}", self.nps));
        }
        parts.join(" · ")
    }

    /// Plain text for the clipboard. Lookup sentences say they are from the
    /// model table. A frequency taken at the start says so.
    pub fn summary(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let title = if self.model.is_empty() { "CPU".to_string() } else { self.model.clone() };
        lines.push(title);
        if !self.vendor.is_empty() || self.family > 0 {
            let mut id = self.vendor.clone();
            if self.family > 0 || self.model_id > 0 || self.stepping > 0 {
                let extra = format!("family {} model {} stepping {}", self.family, self.model_id, self.stepping);
                if id.is_empty() {
                    id = extra;
                } else {
                    id = format!("{id}, {extra}");
                }
            }
            if !id.is_empty() {
                lines.push(id);
            }
        }
        let mut counts: Vec<String> = Vec::new();
        if self.sockets > 0 {
            counts.push(format!("{} socket{}", self.sockets, plural(self.sockets)));
        }
        if self.physical_cores > 0 {
            counts.push(format!("{} core{}", self.physical_cores, plural(self.physical_cores)));
        }
        if self.threads > 0 {
            counts.push(format!("{} thread{}", self.threads, plural(self.threads)));
        }
        match self.smt {
            Some(true) => counts.push("SMT on".into()),
            Some(false) => counts.push("SMT off".into()),
            None => {}
        }
        if self.cache_line_bytes > 0 {
            counts.push(format!("cache line {} B", self.cache_line_bytes));
        }
        if !counts.is_empty() {
            lines.push(counts.join(", "));
        }
        if self.nps > 0 {
            let how = if self.nps_source.is_empty() { String::new() } else { format!(" ({})", self.nps_source) };
            lines.push(format!("NPS {}{how}", self.nps));
        }
        for node in &self.numa {
            let mut line = format!("NUMA node {}", node.id);
            if !node.cpus.is_empty() {
                line.push_str(&format!(": cpus {}", cpu_span(&node.cpus)));
            }
            if node.mem_bytes > 0 {
                line.push_str(&format!(", {}", bytes_text(node.mem_bytes)));
            }
            lines.push(line);
        }
        if !self.distances.is_empty() && !self.numa.is_empty() {
            lines.push(format!("NUMA distances: {} values", self.distances.len()));
        }
        if !self.caches.is_empty() {
            lines.push("Caches:".into());
            for cache in &self.caches {
                let mut line = format!("  L{} {}", cache.level, cache_kind_word(&cache.kind));
                if cache.size_kb > 0 {
                    line.push_str(&format!(" {}", kib_text(cache.size_kb)));
                }
                if cache.ways == 0xFF {
                    line.push_str(", fully associative");
                } else if cache.ways > 0 {
                    line.push_str(&format!(", {}-way", cache.ways));
                }
                if cache.line > 0 {
                    line.push_str(&format!(", line {} B", cache.line));
                }
                if cache.cpus.is_empty() {
                    line.push_str(", sharing not recorded");
                } else {
                    line.push_str(&format!(", shared by {}", cpu_span(&cache.cpus)));
                }
                lines.push(line);
            }
        }
        if !self.microcode.is_empty() {
            lines.push(format!("Microcode: {}", self.microcode));
        }
        if !self.flags.is_empty() {
            lines.push(format!("Flags: {}", self.flags.join(" ")));
        }
        if self.ram_bytes > 0 {
            lines.push(format!("Memory: {}", bytes_text(self.ram_bytes)));
        }
        let host = match (self.hostname.as_str(), self.os.as_str(), self.kernel.as_str()) {
            ("", "", "") => String::new(),
            (host, os, kernel) => {
                let mut bits = Vec::new();
                if !host.is_empty() {
                    bits.push(host.to_string());
                }
                if !os.is_empty() {
                    bits.push(os.to_string());
                }
                if !kernel.is_empty() {
                    bits.push(format!("kernel {kernel}"));
                }
                bits.join(", ")
            }
        };
        if !host.is_empty() {
            lines.push(host);
        }
        let power = self.power_text();
        if !power.is_empty() {
            lines.push(format!("Power: {power}"));
        }
        if !self.layout.summary.is_empty() {
            let tag = match self.layout.source.as_str() {
                "lookup" => "From a model table, not measured",
                "measured" => "Reported by the system",
                "mixed" => "Layout",
                _ => "Layout",
            };
            lines.push(format!("{tag}: {}", self.layout.summary));
        }
        if !self.reference_url.is_empty() {
            lines.push(self.reference_url.clone());
        }
        lines.join("\n")
    }

    fn power_text(&self) -> String {
        let p = &self.power;
        let mut bits: Vec<String> = Vec::new();
        if !p.pstate.is_empty() {
            let status = if p.pstate_status.is_empty() { String::new() } else { format!(" {}", p.pstate_status) };
            bits.push(format!("{}{status}", p.pstate));
        }
        for policy in &p.policies {
            let mut one = String::new();
            if !policy.driver.is_empty() {
                one.push_str(&policy.driver);
            }
            if !policy.governor.is_empty() {
                if !one.is_empty() {
                    one.push(' ');
                }
                one.push_str(&policy.governor);
            }
            if !policy.epp.is_empty() {
                one.push_str(&format!(" EPP {}", policy.epp));
            }
            if policy.scaling_min_khz > 0 || policy.scaling_max_khz > 0 {
                one.push_str(&format!(
                    " scaling {}–{} GHz",
                    ghz_short(policy.scaling_min_khz),
                    ghz_short(policy.scaling_max_khz)
                ));
            }
            if !policy.cpus.is_empty() && p.policies.len() > 1 {
                one.push_str(&format!(" ({})", cpu_span(&policy.cpus)));
            }
            if !one.is_empty() {
                bits.push(one);
            }
        }
        match p.turbo {
            Some(true) => bits.push(format!("boost on ({})", source_or(p.turbo_source.as_str(), "turbo"))),
            Some(false) => bits.push(format!("boost off ({})", source_or(p.turbo_source.as_str(), "turbo"))),
            None => {}
        }
        if !p.platform_profile.is_empty() {
            bits.push(format!("platform profile {}", p.platform_profile));
        }
        if !p.daemon_profile.is_empty() {
            bits.push(format!("power-profiles-daemon {}", p.daemon_profile));
        }
        if !p.plan.is_empty() || !p.plan_guid.is_empty() {
            let name = if p.plan.is_empty() { p.plan_guid.clone() } else { p.plan.clone() };
            let guid = if p.plan_guid.is_empty() || p.plan == p.plan_guid {
                String::new()
            } else {
                format!(" ({})", p.plan_guid)
            };
            bits.push(format!("plan {name}{guid}"));
        }
        if !p.overlay.is_empty() {
            bits.push(format!("overlay {}", p.overlay));
        }
        if !p.boost_mode.is_empty() {
            bits.push(format!("processor boost {}", p.boost_mode));
        }
        match p.low_power {
            Some(true) => bits.push("low power mode on".into()),
            Some(false) => bits.push("low power mode off".into()),
            None => {}
        }
        match p.high_power {
            Some(true) => bits.push("high power mode on".into()),
            Some(false) => bits.push("high power mode off".into()),
            None => {}
        }
        match p.on_ac {
            Some(true) => bits.push("AC".into()),
            Some(false) => bits.push("battery".into()),
            None => {}
        }
        bits.join("; ")
    }

    /// Everything recorded about one logical CPU, for a lane's tooltip.
    /// None when this topology does not list the CPU and has nothing else
    /// worth attaching to every lane.
    pub fn cpu_detail(&self, cpu: u16) -> Option<String> {
        let logical = self.cpus.iter().find(|c| c.cpu == cpu)?;
        let mut lines: Vec<String> = Vec::new();
        let mut head = match logical.kind {
            core_kind::PERFORMANCE => "Performance core".to_string(),
            core_kind::EFFICIENCY => "Efficiency core".to_string(),
            _ => format!("Core {cpu}"),
        };
        if let Some(core) = logical.core_id {
            let thread = match logical.thread {
                Some(t) => format!(", thread {t}"),
                None => String::new(),
            };
            head.push_str(&format!(" (physical core {core}{thread})"));
        }
        lines.push(head);
        let mut place: Vec<String> = Vec::new();
        if let Some(socket) = logical.socket {
            place.push(format!("socket {socket}"));
        }
        if let Some(node) = logical.numa {
            place.push(format!("NUMA node {node}"));
        }
        if let Some(die) = logical.die {
            place.push(format!("die {die}"));
        }
        if let Some(cluster) = logical.cluster {
            place.push(format!("cluster {cluster}"));
        }
        if !place.is_empty() {
            lines.push(place.join(", "));
        }
        for cache in &self.caches {
            if !cache.cpus.is_empty() && !cache.cpus.contains(&cpu) {
                continue;
            }
            if cache.cpus.is_empty() && logical_has_own_cache(logical, cache.level, &cache.kind) {
                continue;
            }
            lines.push(cache_line(cache));
        }
        let mut freq: Vec<String> = Vec::new();
        if logical.min_freq_khz > 0 {
            freq.push(format!("min {}", ghz(logical.min_freq_khz)));
        }
        if logical.base_freq_khz > 0 {
            freq.push(format!("base {}", ghz(logical.base_freq_khz)));
        }
        if logical.max_freq_khz > 0 {
            freq.push(format!("max {}", ghz(logical.max_freq_khz)));
        }
        if !freq.is_empty() {
            lines.push(format!("{} GHz", freq.join(", ")));
        }
        if logical.cur_freq_khz > 0 {
            lines.push(format!(
                "{} GHz at capture start (scaling_cur_freq, not a trace over the capture)",
                ghz(logical.cur_freq_khz)
            ));
        }
        if self.cache_line_bytes > 0 {
            lines.push(format!("Cache line {} B", self.cache_line_bytes));
        }
        if self.nps > 0 {
            let how = if self.nps_source.is_empty() { "" } else { " (derived from the node map)" };
            lines.push(format!("NPS {}{how}", self.nps));
        }
        if !self.microcode.is_empty() {
            lines.push(format!("Microcode {}", self.microcode));
        }
        if !self.flags.is_empty() {
            lines.push(self.flags.join(" "));
        }
        let power = self.power_text();
        if !power.is_empty() {
            lines.push(format!("Power: {power}"));
        }
        if !self.model.is_empty() {
            lines.push(self.model.clone());
        }
        Some(lines.join("\n"))
    }

    /// `P0 · T0` when this CPU is one thread of a physical core and the
    /// machine has SMT. None when the pairing was not recorded, so the
    /// lane keeps `Core N`.
    pub fn lane_label(&self, cpu: u16) -> Option<String> {
        if self.smt != Some(true) {
            return None;
        }
        let logical = self.cpus.iter().find(|c| c.cpu == cpu)?;
        let core = logical.core_id?;
        let prefix = if self.sockets > 1 {
            format!("S{} ", logical.socket.unwrap_or(0))
        } else {
            String::new()
        };
        let siblings = self.cpus.iter().filter(|c| c.socket == logical.socket && c.core_id == Some(core)).count();
        if siblings > 1 {
            Some(format!("{prefix}P{core} · T{}", logical.thread.unwrap_or(0)))
        } else {
            Some(format!("{prefix}P{core}"))
        }
    }

    /// The base and top frequency as the lane prints them (`3.0–5.8`), or
    /// just the top when the base was not recorded or is the same number.
    pub fn freq_label(&self, cpu: u16) -> Option<String> {
        let logical = self.cpus.iter().find(|c| c.cpu == cpu)?;
        if logical.max_freq_khz == 0 && logical.base_freq_khz == 0 {
            return None;
        }
        let max = ghz_short(logical.max_freq_khz);
        if logical.base_freq_khz > 0 && logical.max_freq_khz > 0 && ghz_short(logical.base_freq_khz) != max {
            Some(format!("{}–{max}", ghz_short(logical.base_freq_khz)))
        } else if logical.max_freq_khz > 0 {
            Some(max)
        } else {
            Some(ghz_short(logical.base_freq_khz))
        }
    }

    /// Current frequency over the top, for the small bar. None without both.
    pub fn cur_ratio(&self, cpu: u16) -> Option<f32> {
        let logical = self.cpus.iter().find(|c| c.cpu == cpu)?;
        if logical.cur_freq_khz == 0 || logical.max_freq_khz == 0 {
            return None;
        }
        Some((logical.cur_freq_khz as f32 / logical.max_freq_khz as f32).clamp(0.0, 1.0))
    }
}

fn logical_has_own_cache(cpu: &LogicalCpu, level: u8, kind: &str) -> bool {
    match (level, kind) {
        (1, "instruction") => cpu.l1i_kb > 0,
        (1, "data") => cpu.l1d_kb > 0,
        (2, _) => cpu.l2_kb > 0,
        (3, _) => cpu.l3_kb > 0,
        _ => false,
    }
}

fn cache_line(cache: &CacheInstance) -> String {
    let mut line = format!("L{} {}", cache.level, cache_kind_word(&cache.kind));
    if cache.size_kb > 0 {
        line.push_str(&format!(" {}", kib_text(cache.size_kb)));
    }
    if cache.ways == 0xFF {
        line.push_str(", fully associative");
    } else if cache.ways > 0 {
        line.push_str(&format!(", {}-way", cache.ways));
    }
    if cache.line > 0 {
        line.push_str(&format!(", line {} B", cache.line));
    }
    if cache.cpus.is_empty() {
        line.push_str(", sharing not recorded");
    } else {
        line.push_str(&format!(", shared by {}", cpu_span(&cache.cpus)));
    }
    line
}

fn cache_kind_word(kind: &str) -> &str {
    match kind {
        "instruction" => "instruction",
        "data" => "data",
        "unified" => "unified",
        "" => "cache",
        other => other,
    }
}

fn source_or<'a>(source: &'a str, fallback: &'a str) -> &'a str {
    if source.is_empty() { fallback } else { source }
}

fn plural(n: u16) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `0-3,8` for a sorted list, the way a CPU list is read.
pub fn cpu_span(cpus: &[u16]) -> String {
    if cpus.is_empty() {
        return String::new();
    }
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for &cpu in &sorted[1..] {
        if cpu == prev + 1 {
            prev = cpu;
            continue;
        }
        parts.push(span(start, prev));
        start = cpu;
        prev = cpu;
    }
    parts.push(span(start, prev));
    parts.join(",")
}

fn span(start: u16, end: u16) -> String {
    if start == end { format!("{start}") } else { format!("{start}-{end}") }
}

pub fn kib_text(kb: u32) -> String {
    if kb >= 1024 && kb % 1024 == 0 {
        format!("{} MB", kb / 1024)
    } else if kb >= 1024 {
        format!("{:.1} MB", f64::from(kb) / 1024.0)
    } else {
        format!("{kb} KB")
    }
}

pub fn bytes_text(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn ghz(khz: u32) -> String {
    format!("{:.2}", f64::from(khz) / 1_000_000.0)
}

pub fn ghz_short(khz: u32) -> String {
    format!("{:.1}", f64::from(khz) / 1_000_000.0)
}

/// Flags worth naming on a lane. The rest of the flags line is not kept:
/// a capture is not a copy of `/proc/cpuinfo`.
const INTERESTING_FLAGS: &[&str] = &[
    "sse4_1",
    "sse4_2",
    "aes",
    "avx",
    "avx2",
    "fma",
    "f16c",
    "bmi1",
    "bmi2",
    "avx512f",
    "avx512dq",
    "avx512cd",
    "avx512bw",
    "avx512vl",
    "avx512vnni",
    "avx512_bf16",
    "avx_vnni",
    "amx_tile",
    "amx_int8",
    "amx_bf16",
    "sha_ni",
    "vaes",
    "vpclmulqdq",
    "gfni",
    "vmx",
    "svm",
    "hypervisor",
];

/// Keep [`INTERESTING_FLAGS`] in the order `flags` lists them. `flags` is
/// the cpuinfo line or a space-separated sysctl feature list.
pub fn keep_flags(flags: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in flags.split_whitespace() {
        let name = normalize_flag(raw);
        if INTERESTING_FLAGS.contains(&name.as_str()) && !out.iter().any(|k: &String| k == &name) {
            out.push(name);
        }
    }
    out
}

fn normalize_flag(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase().replace('.', "_");
    match lower.as_str() {
        "avx1_0" | "avx1" => "avx".into(),
        "sha" => "sha_ni".into(),
        other => other.to_string(),
    }
}

/// A web search for the model string. None when there is no model.
pub fn search_url(model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let mut q = String::new();
    for b in model.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => q.push(b as char),
            b' ' => q.push('+'),
            _ => q.push_str(&format!("%{b:02X}")),
        }
    }
    Some(format!("https://www.google.com/search?q={q}"))
}

struct KnownModel {
    token: &'static str,
    /// Another token that must also be present, so `9654` does not match a
    /// model that merely contains those digits.
    also: Option<&'static str>,
    cores: u16,
    threads: u16,
    ccds: u16,
    memory_channels: u16,
    /// Empty when no product page was checked for this entry.
    url: &'static str,
    note: &'static str,
}

/// Product pages that were opened and layouts that are the published
/// core counts of those parts. A model matches only when its string
/// contains the token and the captured core and thread counts are the
/// ones listed here. A VM that reports the host's name and its own smaller
/// counts does not match.
const KNOWN_MODELS: &[KnownModel] = &[
    KnownModel {
        token: "i9-13900k",
        also: None,
        cores: 24,
        threads: 32,
        ccds: 0,
        memory_channels: 2,
        url: "https://www.intel.com/content/www/us/en/products/sku/230496/intel-core-i913900k-processor-36m-cache-up-to-5-80-ghz/specifications.html",
        note: "Raptor Lake: 8 performance cores with two threads each and 16 efficiency cores with one, the efficiency cores in clusters of 4, 36 MB L3, two memory channels.",
    },
    KnownModel {
        token: "7950x3d",
        also: None,
        cores: 16,
        threads: 32,
        ccds: 2,
        memory_channels: 2,
        url: "https://www.amd.com/en/products/processors/desktops/ryzen/7000-series/amd-ryzen-9-7950x3d.html",
        note: "Zen 4: two CCDs of 8 cores and an I/O die. One CCD has a 96 MB stacked L3 and the other 32 MB; which cores those are is the measured cache, not this table. Two DDR5 channels.",
    },
    KnownModel {
        token: "7800x3d",
        also: None,
        cores: 8,
        threads: 16,
        ccds: 1,
        memory_channels: 2,
        url: "",
        note: "Zen 4: one CCD of 8 cores with a 96 MB stacked L3, and an I/O die. Two DDR5 channels. No product page is linked for this part.",
    },
    KnownModel {
        token: "9654",
        also: Some("epyc"),
        cores: 96,
        threads: 192,
        ccds: 12,
        memory_channels: 12,
        url: "",
        note: "Genoa: 12 CCDs of 8 cores and an I/O die, 12 DDR5 channels. Nodes per socket is a firmware setting and is not taken from this table. No product page is linked for this part.",
    },
];

fn lookup_model(model: &str, physical_cores: u16, threads: u16) -> Option<&'static KnownModel> {
    let tokens = tokenize(model);
    if tokens.is_empty() {
        return None;
    }
    KNOWN_MODELS.iter().find(|known| {
        let name = tokens.iter().any(|t| t == known.token)
            && known.also.is_none_or(|extra| tokens.iter().any(|t| t == extra));
        if !name {
            return false;
        }
        let cores_known = physical_cores > 0;
        let threads_known = threads > 0;
        if !cores_known && !threads_known {
            return false;
        }
        if cores_known && physical_cores != known.cores {
            return false;
        }
        if threads_known && threads != known.threads {
            return false;
        }
        true
    })
}

fn tokenize(model: &str) -> Vec<String> {
    model
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// `powerprofilesctl get` prints a profile name. Anything else, including
/// a usage message, is not a profile.
pub fn accept_power_profile(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    match line {
        "power-saver" | "balanced" | "performance" => line.to_string(),
        _ => String::new(),
    }
}

/// The names `powercfg` prints for the four built-in scheme GUIDs. A GUID
/// that is not one of these has no name here.
pub fn known_power_plan(guid: &str) -> Option<&'static str> {
    match guid.trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase().as_str() {
        "381b4222-f694-41f0-9685-ff5bb260df2e" => Some("Balanced"),
        "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c" => Some("High performance"),
        "a1841308-3541-4fab-bc81-f71556f20b4a" => Some("Power saver"),
        "e9a42b02-d5df-448d-aa00-03f14749eb61" => Some("Ultimate performance"),
        _ => None,
    }
}

/// The four overlay GUIDs `powercfg /overlay` lists. `Balanced` is the
/// all-zero GUID that tool prints; an overlay the tool does not list is
/// not given a name.
pub fn known_overlay(guid: &str) -> Option<&'static str> {
    match guid.trim().trim_matches(|c| c == '{' || c == '}').to_ascii_lowercase().as_str() {
        "961cc777-2547-4f9d-8174-7d86181b8a7a" => Some("Best power efficiency"),
        "00000000-0000-0000-0000-000000000000" => Some("Balanced"),
        "3af9b8d9-7c97-431d-ad78-34a8bfea439f" => Some("Better performance"),
        "ded574b5-45a0-4f42-8737-46345c09c238" => Some("Best performance"),
        _ => None,
    }
}

/// Windows' AC processor-boost index. An index outside 0..=6 is returned
/// as `index N` so the number is not dressed up as a mode.
pub fn boost_mode_name(index: u32) -> String {
    match index {
        0 => "disabled".into(),
        1 => "enabled".into(),
        2 => "aggressive".into(),
        3 => "efficient".into(),
        4 => "efficient aggressive".into(),
        5 => "aggressive at guaranteed".into(),
        6 => "efficient aggressive at guaranteed".into(),
        other => format!("index {other}"),
    }
}

/// `pmset -g` and `pmset -g batt` run together. A key that is absent stays
/// `None`; a printed 0 is off, not missing. `(low power, high power, on AC)`.
pub fn parse_pmset(text: &str) -> (Option<bool>, Option<bool>, Option<bool>) {
    let mut low = None;
    let mut high = None;
    let mut on_ac = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("lowpowermode") {
            low = flag_bit(rest);
        } else if let Some(rest) = trimmed.strip_prefix("highpowermode") {
            high = flag_bit(rest);
        }
        if trimmed.contains("Now drawing from 'AC Power'") {
            on_ac = Some(true);
        } else if trimmed.contains("Now drawing from 'Battery Power'") {
            on_ac = Some(false);
        }
    }
    (low, high, on_ac)
}

fn flag_bit(rest: &str) -> Option<bool> {
    let token = rest.split_whitespace().next().unwrap_or("");
    match token {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ryzen() -> CpuTopology {
        let mut topo = CpuTopology {
            vendor: "AuthenticAMD".into(),
            model: "AMD Ryzen 9 7950X3D 16-Core Processor".into(),
            family: 25,
            model_id: 97,
            stepping: 2,
            sockets: 1,
            physical_cores: 16,
            threads: 32,
            smt: Some(true),
            cache_line_bytes: 64,
            microcode: "0x0a601206".into(),
            flags: vec!["avx2".into(), "avx512f".into()],
            kernel: "6.8.0".into(),
            os: "Linux".into(),
            hostname: "box".into(),
            ram_bytes: 64 << 30,
            nps: 1,
            nps_source: "derived".into(),
            numa: vec![NumaNode { id: 0, cpus: (0..32).collect(), mem_bytes: 64 << 30 }],
            cpus: vec![LogicalCpu {
                cpu: 0,
                socket: Some(0),
                numa: Some(0),
                die: Some(0),
                core_id: Some(0),
                thread: Some(0),
                max_freq_khz: 5_750_000,
                base_freq_khz: 4_200_000,
                min_freq_khz: 400_000,
                cur_freq_khz: 4_200_000,
                l1i_kb: 32,
                l1d_kb: 32,
                l2_kb: 1024,
                l3_kb: 98_304,
                ..LogicalCpu::default()
            }],
            caches: vec![CacheInstance {
                level: 3,
                kind: "unified".into(),
                size_kb: 98_304,
                ways: 16,
                line: 64,
                cpus: (0..16).collect(),
            }],
            power: PowerState {
                policies: vec![CpufreqPolicy {
                    cpus: (0..32).collect(),
                    driver: "amd-pstate".into(),
                    governor: "powersave".into(),
                    epp: "balance_performance".into(),
                    scaling_min_khz: 400_000,
                    scaling_max_khz: 5_750_000,
                }],
                pstate: "amd_pstate".into(),
                pstate_status: "active".into(),
                turbo: Some(true),
                turbo_source: "boost".into(),
                platform_profile: "balanced".into(),
                ..PowerState::default()
            },
            ..CpuTopology::default()
        };
        topo.apply_model_table();
        topo
    }

    #[test]
    fn json_drops_what_is_unknown_and_reads_it_back_as_unknown() {
        let topo = ryzen();
        let bytes = topo.to_json();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.contains("4200000"));
        // The logical CPU's kind was not told, so it is omitted. The cache's
        // kind is a word and stays.
        assert!(text.contains("\"kind\":\"unified\""));
        assert!(!text.contains("\"cluster\""));
        let back = CpuTopology::from_json(&bytes).unwrap();
        assert_eq!(back, topo);
        let extra = br#"{"model":"plain","future_field":1,"cpus":[{"cpu":3,"max_freq_khz":1000}]}"#;
        let parsed = CpuTopology::from_json(extra).unwrap();
        assert_eq!(parsed.model, "plain");
        assert_eq!(parsed.cpus[0].cpu, 3);
        assert_eq!(parsed.cpus[0].socket, None);
        assert!(CpuTopology::from_json(b"{}").unwrap().is_empty());
        assert!(CpuTopology::from_json(b"not json").is_err());
    }

    #[test]
    fn the_model_table_fills_a_layout_only_when_the_counts_agree() {
        let topo = ryzen();
        assert_eq!(topo.layout.source, "lookup");
        assert!(topo.layout.summary.contains("two CCDs"));
        assert_eq!(topo.layout.ccds, 2);
        assert_eq!(topo.layout.memory_channels, 2);
        assert!(topo.reference_url.contains("amd.com"));
        assert!(topo.summary().contains("From a model table, not measured"));
        assert!(topo.header_line().contains("powersave"));
        assert!(topo.header_line().contains("NPS1"));
        assert!(topo.header_line().contains("boost on"));

        let mut small = topo.clone();
        small.physical_cores = 2;
        small.threads = 4;
        small.layout = LayoutNote::default();
        small.reference_url.clear();
        small.apply_model_table();
        assert!(small.layout.summary.is_empty(), "a smaller machine that wears the name is not given the part's dies");
        assert!(small.reference_url.is_empty());

        let mut unnamed = CpuTopology { model: "AMD Custom 0000".into(), physical_cores: 16, threads: 32, ..CpuTopology::default() };
        unnamed.apply_model_table();
        assert!(unnamed.reference_url.is_empty());
        assert!(unnamed.layout.summary.is_empty());

        let mut epyc = CpuTopology {
            model: "AMD EPYC 9654 96-Core Processor".into(),
            physical_cores: 96,
            threads: 192,
            ..CpuTopology::default()
        };
        epyc.apply_model_table();
        assert_eq!(epyc.layout.ccds, 12);
        assert_eq!(epyc.layout.memory_channels, 12);
        assert!(epyc.reference_url.is_empty(), "no product page was checked for this part");
        assert!(epyc.layout.summary.contains("not taken from this table"));

        let mut no_counts = CpuTopology { model: epyc.model.clone(), ..CpuTopology::default() };
        no_counts.apply_model_table();
        assert!(no_counts.layout.summary.is_empty());
    }

    #[test]
    fn a_lane_is_named_for_its_physical_core_and_says_what_was_recorded() {
        let mut topo = ryzen();
        topo.cpus.push(LogicalCpu { cpu: 1, thread: Some(1), ..topo.cpus[0].clone() });
        assert_eq!(topo.lane_label(0).as_deref(), Some("P0 · T0"));
        assert_eq!(topo.lane_label(1).as_deref(), Some("P0 · T1"));
        assert_eq!(topo.freq_label(0).as_deref(), Some("4.2–5.8"));
        assert_eq!(topo.cur_ratio(0), Some(4_200_000.0 / 5_750_000.0));
        let detail = topo.cpu_detail(0).unwrap();
        assert!(detail.contains("physical core 0, thread 0"));
        assert!(detail.contains("NUMA node 0"));
        assert!(detail.contains("96 MB"));
        assert!(detail.contains("not a trace"));
        assert!(detail.contains("NPS 1"));
        assert!(detail.contains("avx512f"));
        assert!(topo.cpu_detail(9).is_none());
        let shifted = topo.shift_lanes(8);
        assert_eq!(shifted.cpus[0].cpu, 8);
        assert_eq!(shifted.lane_label(8).as_deref(), Some("P0 · T0"));
        assert!(shifted.shift_lanes(300).cpus.is_empty());
    }

    #[test]
    fn flags_power_plans_and_pmset_keep_only_what_they_were_given() {
        assert_eq!(keep_flags("fpu sse4_2 avx avx2 avx512f hypervisor"), ["sse4_2", "avx", "avx2", "avx512f", "hypervisor"]);
        assert_eq!(keep_flags("AVX1.0 AVX2 FMA SHA"), ["avx", "avx2", "fma", "sha_ni"]);
        assert_eq!(known_power_plan("{381B4222-F694-41F0-9685-FF5BB260DF2E}"), Some("Balanced"));
        assert_eq!(known_power_plan("e9a42b02-d5df-448d-aa00-03f14749eb61"), Some("Ultimate performance"));
        assert_eq!(known_power_plan("nope"), None);
        assert_eq!(known_overlay("ded574b5-45a0-4f42-8737-46345c09c238"), Some("Best performance"));
        assert_eq!(boost_mode_name(2), "aggressive");
        assert_eq!(boost_mode_name(9), "index 9");
        assert_eq!(accept_power_profile("balanced\n"), "balanced");
        assert_eq!(accept_power_profile("Usage: powerprofilesctl"), "");
        let text = "\
 lowpowermode         1
 highpowermode        0
Now drawing from 'Battery Power'
";
        assert_eq!(parse_pmset(text), (Some(true), Some(false), Some(false)));
        assert_eq!(parse_pmset("Now drawing from 'AC Power'\n"), (None, None, Some(true)));
        assert_eq!(search_url("").as_deref(), None);
        assert_eq!(
            search_url("AMD Ryzen 9 7950X3D").as_deref(),
            Some("https://www.google.com/search?q=AMD+Ryzen+9+7950X3D")
        );
    }

    #[test]
    fn core_infos_are_the_lane_marks_and_nothing_when_the_cpus_say_nothing() {
        let topo = ryzen();
        assert_eq!(topo.core_infos()[0].l3_kb, 98_304);
        let bare = CpuTopology { cpus: vec![LogicalCpu { cpu: 0, ..LogicalCpu::default() }], ..CpuTopology::default() };
        assert!(bare.core_infos().is_empty());
        assert!(!bare.is_empty());
    }
}
