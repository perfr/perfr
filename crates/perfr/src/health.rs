// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Machine health: what the whole machine was doing while the capture ran.
//!
//! A stall in the target is not always the target's doing. The machine may
//! have been out of memory and reclaiming, a disk may have been saturated,
//! another process may have had every core. Twice a second the capture loop
//! reads a handful of kernel files and turns them into value lanes under
//! one reserved process (`perfr_live_event::health`), which the viewer
//! shows right after the target: the timeline then has the machine's state
//! under the same cursor as the stall.
//!
//! It is `node_exporter` cut down to what explains a stall, and it must
//! never be the cause of one. Every file is opened once and read in place
//! afterwards, the buffers are reused, and nothing is spawned: a sample is
//! about ten `pread` calls.
//!
//! | Lane | From |
//! |---|---|
//! | CPU %, CPU steal % | the `cpu` line of `/proc/stat` |
//! | CPU, memory and IO pressure % | `/proc/pressure/*` (PSI) |
//! | CPU temperature | `temp1_input` of the CPU's hwmon sensor |
//! | memory used %, swap used | `/proc/meminfo` |
//! | major faults, OOM kills | `/proc/vmstat` |
//! | disk read, write and busy % | `/proc/diskstats`, whole physical disks |
//! | network receive and send | `/proc/net/dev`, physical interfaces |
//! | GPU %, GPU memory % | `/sys/class/drm/card*/device` (amdgpu), or the NVML helper's readings |
//!
//! A source the machine does not have (no PSI, no swap, no known sensor, no
//! GPU) leaves its lanes out rather than drawing them flat at zero.

use perfr_live_event::health::{self, lane};
use perfr_live_event::LiveEvent;
use perfr_wire::{Event as WireEvent, METRIC_UNKNOWN_U32, METRIC_UNKNOWN_U64};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// How often the machine is sampled.
pub const INTERVAL_NS: u64 = 500_000_000;

/// The `cpu` line of `/proc/stat`, in clock ticks summed over every core.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CpuTicks {
    total: u64,
    /// Idle, including idle while waiting for IO: the kernel only counts
    /// iowait on a core that had nothing else to run.
    idle: u64,
    /// Time a hypervisor ran something else while this machine wanted to.
    steal: u64,
}

/// `/proc/meminfo`, in kB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Memory {
    total: u64,
    available: u64,
    swap_total: u64,
    swap_free: u64,
}

/// One whole disk's line of `/proc/diskstats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Disk {
    /// Major and minor number: what says two lines are the same disk.
    id: u64,
    sectors_read: u64,
    sectors_written: u64,
    /// Milliseconds during which at least one request was in flight.
    busy_ms: u64,
}

/// `/proc/net/dev`, summed over the physical interfaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Network {
    received_bytes: u64,
    sent_bytes: u64,
}

/// One GPU, as read from sysfs or reported by the NVML helper.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GpuDevice {
    busy_percent: Option<f32>,
    /// Used and total bytes.
    memory: Option<(u64, u64)>,
}

/// Everything read in one pass. The counters are cumulative since boot; a
/// lane is the difference between two of these.
#[derive(Clone, Debug, Default, PartialEq)]
struct Snapshot {
    cpu: Option<CpuTicks>,
    cpu_stall_us: Option<u64>,
    cpu_temperature_c: Option<f32>,
    memory: Option<Memory>,
    memory_stall_us: Option<u64>,
    major_faults: Option<u64>,
    oom_kills: Option<u64>,
    io_stall_us: Option<u64>,
    disks: Vec<Disk>,
    network: Option<Network>,
    /// The busiest GPU.
    gpu_percent: Option<f32>,
    /// Memory in use over memory fitted, every GPU together.
    gpu_memory_percent: Option<f32>,
}

/// The `cpu` line: user, nice, system, idle, iowait, irq, softirq, steal.
/// The guest columns after them are already inside user and nice.
fn parse_cpu(stat: &str) -> Option<CpuTicks> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let mut fields = [0u64; 8];
    let mut seen = 0;
    for (slot, text) in fields.iter_mut().zip(line.split_ascii_whitespace().skip(1)) {
        *slot = text.parse().ok()?;
        seen += 1;
    }
    // Idle is the fourth column; a line that stops before it says nothing.
    if seen < 4 {
        return None;
    }
    Some(CpuTicks { total: fields.iter().sum(), idle: fields[3] + fields[4], steal: fields[7] })
}

/// The number after `key` on its line, for `key value` and `key: value kB`.
fn field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?;
        // `SwapTotal` must not answer for `Swap`, nor `oom_kill` for `oom`.
        if !rest.starts_with([' ', ':', '\t']) {
            return None;
        }
        rest.trim_start_matches(':').split_ascii_whitespace().next()?.parse().ok()
    })
}

fn parse_memory(meminfo: &str) -> Option<Memory> {
    Some(Memory {
        total: field(meminfo, "MemTotal")?,
        available: field(meminfo, "MemAvailable")?,
        swap_total: field(meminfo, "SwapTotal").unwrap_or(0),
        swap_free: field(meminfo, "SwapFree").unwrap_or(0),
    })
}

/// Microseconds in which at least one task was stalled, since boot: the
/// `total=` of the `some` line of a `/proc/pressure` file. The file's own
/// averages are over ten seconds and more, which is no use against a stall
/// of a hundred milliseconds; the difference of two totals is exact over
/// the interval between them.
fn parse_stall_us(pressure: &str) -> Option<u64> {
    let line = pressure.lines().find(|line| line.starts_with("some "))?;
    line.split_ascii_whitespace().find_map(|word| word.strip_prefix("total="))?.parse().ok()
}

/// The lines of `/proc/diskstats` that `is_disk` accepts, into `out`.
fn parse_disks(diskstats: &str, mut is_disk: impl FnMut(&str) -> bool, out: &mut Vec<Disk>) {
    out.clear();
    for line in diskstats.lines() {
        let mut words = line.split_ascii_whitespace();
        let (Some(major), Some(minor), Some(name)) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        if !is_disk(name) {
            continue;
        }
        // After the name: reads, reads merged, sectors read, ms reading,
        // writes, writes merged, sectors written, ms writing, in flight,
        // ms busy.
        let mut counters = [0u64; 10];
        let mut seen = 0;
        for (slot, text) in counters.iter_mut().zip(words) {
            let Ok(value) = text.parse() else { break };
            *slot = value;
            seen += 1;
        }
        let (Ok(major), Ok(minor)) = (major.parse::<u64>(), minor.parse::<u64>()) else { continue };
        if seen < counters.len() {
            continue;
        }
        out.push(Disk {
            id: (major << 32) | minor,
            sectors_read: counters[2],
            sectors_written: counters[6],
            busy_ms: counters[9],
        });
    }
}

/// Bytes received and sent over the interfaces `is_physical` accepts.
fn parse_network(netdev: &str, mut is_physical: impl FnMut(&str) -> bool) -> Option<Network> {
    let mut sum: Option<Network> = None;
    for line in netdev.lines() {
        // The two header lines have no colon after a name.
        let Some((name, counters)) = line.split_once(':') else { continue };
        let name = name.trim();
        if name.is_empty() || name.contains('|') || !is_physical(name) {
            continue;
        }
        let mut words = counters.split_ascii_whitespace();
        // Receive bytes lead; send bytes are the ninth column.
        let (Some(received), Some(sent)) = (words.next(), words.nth(7)) else { continue };
        let (Ok(received), Ok(sent)) = (received.parse::<u64>(), sent.parse::<u64>()) else { continue };
        let sum = sum.get_or_insert_with(Network::default);
        sum.received_bytes += received;
        sum.sent_bytes += sent;
    }
    sum
}

/// The machine's GPUs as two figures: how busy the busiest one is, and how
/// much of all their memory is in use. One card pegged is a saturated GPU
/// for whatever runs on it, so the busiest is the one to show; an average
/// would hide it behind an idle second card.
fn merge_gpus(devices: impl IntoIterator<Item = GpuDevice>) -> (Option<f32>, Option<f32>) {
    let mut busiest: Option<f32> = None;
    let (mut used, mut total) = (0u64, 0u64);
    for device in devices {
        if let Some(busy) = device.busy_percent {
            busiest = Some(busiest.map_or(busy, |so_far| so_far.max(busy)));
        }
        if let Some((device_used, device_total)) = device.memory {
            used += device_used;
            total += device_total;
        }
    }
    (busiest, (total > 0).then(|| (used as f64 / total as f64 * 100.0) as f32))
}

/// What the lanes remember from one sample to the next.
#[derive(Debug, Default)]
struct LaneState {
    /// Steal is only ever non-zero under a hypervisor. The lane appears
    /// with the first stolen tick and stays, instead of sitting flat at
    /// zero on every machine that is not a guest.
    steal_seen: bool,
    /// The kernel's OOM kill count when the capture started: the lane
    /// counts the kills of this capture, and appears with the first.
    oom_kills_at_start: Option<u64>,
}

fn percent(part: f64, whole: f64) -> f32 {
    if whole <= 0.0 {
        return 0.0;
    }
    (part / whole * 100.0).clamp(0.0, 100.0) as f32
}

/// The lanes for `cur`, as `(lane id, value)`. Levels (memory in use, a
/// temperature) need one snapshot. Rates need the one before it and the
/// time between the two, so the first sample of a capture has none.
fn readings(prev: Option<&Snapshot>, cur: &Snapshot, dt_ns: u64, state: &mut LaneState, out: &mut Vec<(u32, f32)>) {
    let prev = prev.filter(|_| dt_ns > 0);
    let dt_s = dt_ns as f64 / 1e9;
    let dt_us = dt_ns as f64 / 1e3;
    let rate = |before: u64, now: u64| (now.saturating_sub(before) as f64 / dt_s) as f32;
    let stalled = |before: Option<u64>, now: Option<u64>| {
        Some(percent(now?.saturating_sub(before?) as f64, dt_us))
    };

    if let (Some(before), Some(now)) = (prev.and_then(|p| p.cpu), cur.cpu) {
        let total = now.total.saturating_sub(before.total);
        if total > 0 {
            let idle = now.idle.saturating_sub(before.idle).min(total);
            out.push((lane::CPU, percent((total - idle) as f64, total as f64)));
            let steal = now.steal.saturating_sub(before.steal).min(total);
            state.steal_seen |= steal > 0;
            if state.steal_seen {
                out.push((lane::CPU_STEAL, percent(steal as f64, total as f64)));
            }
        }
    }
    if let Some(value) = stalled(prev.and_then(|p| p.cpu_stall_us), cur.cpu_stall_us) {
        out.push((lane::CPU_PRESSURE, value));
    }
    if let Some(celsius) = cur.cpu_temperature_c {
        out.push((lane::CPU_TEMPERATURE, celsius));
    }

    if let Some(memory) = cur.memory {
        let used = memory.total.saturating_sub(memory.available);
        out.push((lane::MEMORY_USED, percent(used as f64, memory.total as f64)));
        if memory.swap_total > 0 {
            let swapped_kb = memory.swap_total.saturating_sub(memory.swap_free);
            out.push((lane::SWAP_USED, (swapped_kb as f64 / 1024.0) as f32));
        }
    }
    if let Some(value) = stalled(prev.and_then(|p| p.memory_stall_us), cur.memory_stall_us) {
        out.push((lane::MEMORY_PRESSURE, value));
    }
    if let (Some(before), Some(now)) = (prev.and_then(|p| p.major_faults), cur.major_faults) {
        out.push((lane::MAJOR_FAULTS, rate(before, now)));
    }
    if let Some(kills) = cur.oom_kills {
        let killed = kills.saturating_sub(*state.oom_kills_at_start.get_or_insert(kills));
        if killed > 0 {
            out.push((lane::OOM_KILLS, killed as f32));
        }
    }

    if let Some(value) = stalled(prev.and_then(|p| p.io_stall_us), cur.io_stall_us) {
        out.push((lane::IO_PRESSURE, value));
    }
    if let Some(prev) = prev.filter(|p| !p.disks.is_empty() && !cur.disks.is_empty()) {
        // 512 bytes: the unit of the file whatever the device's own sector.
        let mib_per_s = |before: u64, now: u64| rate(before, now) * 512.0 / (1024.0 * 1024.0);
        let sum = |disks: &[Disk], of: fn(&Disk) -> u64| disks.iter().map(of).sum::<u64>();
        out.push((
            lane::DISK_READ,
            mib_per_s(sum(&prev.disks, |d| d.sectors_read), sum(&cur.disks, |d| d.sectors_read)),
        ));
        out.push((
            lane::DISK_WRITE,
            mib_per_s(sum(&prev.disks, |d| d.sectors_written), sum(&cur.disks, |d| d.sectors_written)),
        ));
        // The busiest disk, for the reason the busiest GPU is the one shown.
        let busiest = cur
            .disks
            .iter()
            .filter_map(|now| {
                let before = prev.disks.iter().find(|d| d.id == now.id)?;
                Some(percent(now.busy_ms.saturating_sub(before.busy_ms) as f64, dt_ns as f64 / 1e6))
            })
            .fold(None, |so_far: Option<f32>, busy| Some(so_far.map_or(busy, |b| b.max(busy))));
        if let Some(busy) = busiest {
            out.push((lane::DISK_BUSY, busy));
        }
    }
    if let (Some(before), Some(now)) = (prev.and_then(|p| p.network), cur.network) {
        let mib_per_s = |before: u64, now: u64| rate(before, now) / (1024.0 * 1024.0);
        out.push((lane::NETWORK_RECEIVE, mib_per_s(before.received_bytes, now.received_bytes)));
        out.push((lane::NETWORK_SEND, mib_per_s(before.sent_bytes, now.sent_bytes)));
    }

    if let Some(busy) = cur.gpu_percent {
        out.push((lane::GPU, busy));
    }
    if let Some(used) = cur.gpu_memory_percent {
        out.push((lane::GPU_MEMORY, used));
    }
}

/// A kernel file kept open. Reading it again from offset zero has the
/// kernel produce it afresh; there is no path lookup and no descriptor to
/// allocate past the first time.
struct Source(File);

impl Source {
    fn open(path: impl AsRef<Path>) -> Option<Source> {
        File::open(path).ok().map(Source)
    }

    /// The whole file. A read returns what the kernel has ready, a page or
    /// so at a time, so this reads on until the end.
    fn read<'a>(&self, buf: &'a mut Vec<u8>) -> Option<&'a str> {
        let mut len = 0usize;
        loop {
            if buf.len() - len < 4096 {
                buf.resize(buf.len() + 16 * 1024, 0);
            }
            match self.0.read_at(&mut buf[len..], len as u64) {
                Ok(0) => break,
                Ok(count) => len += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        ascii(&buf[..len])
    }

    /// The first bytes only. The kernel still produces the whole file, but
    /// `/proc/stat` is one line per core and one number per interrupt, and
    /// the line wanted is the first: this leaves the rest uncopied.
    fn read_head<'a>(&self, buf: &'a mut Vec<u8>) -> Option<&'a str> {
        if buf.len() < 512 {
            buf.resize(512, 0);
        }
        let count = self.0.read_at(&mut buf[..512], 0).ok()?;
        ascii(&buf[..count])
    }

    /// A file that is one number.
    fn read_number(&self, buf: &mut Vec<u8>) -> Option<u64> {
        self.read_head(buf)?.trim().parse().ok()
    }
}

/// The text, up to the first byte that is not UTF-8: a device may be named
/// anything, and one odd name must not cost the lines before it.
fn ascii(bytes: &[u8]) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()]).ok(),
    }
}

/// A GPU with its figures in sysfs, which is amdgpu's interface.
struct SysfsGpu {
    busy_percent: Source,
    memory_used: Option<Source>,
    memory_total: Option<u64>,
}

/// The hwmon drivers that report the processor's own temperature, with the
/// package (or the die) as their first sensor.
const CPU_SENSORS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal"];

/// The files, opened once, and which devices count.
struct Sources {
    stat: Option<Source>,
    meminfo: Option<Source>,
    vmstat: Option<Source>,
    pressure_cpu: Option<Source>,
    pressure_memory: Option<Source>,
    pressure_io: Option<Source>,
    diskstats: Option<Source>,
    netdev: Option<Source>,
    cpu_temperature: Option<Source>,
    gpus: Vec<SysfsGpu>,
    /// Whether a block device is a whole physical disk, by name. Partitions
    /// would count every byte twice, and so would the device-mapper and
    /// loop devices stacked on the disks; none of them has a `device` link.
    disks: HashMap<String, bool>,
    /// Whether an interface is hardware. Bridges, tunnels and container
    /// pairs carry traffic that a physical interface carries as well.
    interfaces: HashMap<String, bool>,
}

/// Looks `name` up in `known`, asking sysfs once for a name not seen yet.
fn has_device(known: &mut HashMap<String, bool>, class: &str, name: &str) -> bool {
    if let Some(answer) = known.get(name) {
        return *answer;
    }
    let answer = Path::new(class).join(name).join("device").exists();
    known.insert(name.to_string(), answer);
    answer
}

impl Sources {
    fn open() -> Sources {
        Sources {
            stat: Source::open("/proc/stat"),
            meminfo: Source::open("/proc/meminfo"),
            vmstat: Source::open("/proc/vmstat"),
            pressure_cpu: Source::open("/proc/pressure/cpu"),
            pressure_memory: Source::open("/proc/pressure/memory"),
            pressure_io: Source::open("/proc/pressure/io"),
            diskstats: Source::open("/proc/diskstats"),
            netdev: Source::open("/proc/net/dev"),
            cpu_temperature: cpu_temperature_source(),
            gpus: sysfs_gpus(),
            disks: HashMap::new(),
            interfaces: HashMap::new(),
        }
    }

    fn read(&mut self, into: &mut Snapshot, buf: &mut Vec<u8>) {
        into.cpu = self.stat.as_ref().and_then(|f| f.read_head(buf)).and_then(parse_cpu);
        into.memory = self.meminfo.as_ref().and_then(|f| f.read(buf)).and_then(parse_memory);
        let vmstat = self.vmstat.as_ref().and_then(|f| f.read(buf));
        into.major_faults = vmstat.and_then(|text| field(text, "pgmajfault"));
        into.oom_kills = vmstat.and_then(|text| field(text, "oom_kill"));
        let stall = |source: &Option<Source>, buf: &mut Vec<u8>| {
            source.as_ref().and_then(|f| f.read(buf)).and_then(parse_stall_us)
        };
        into.cpu_stall_us = stall(&self.pressure_cpu, buf);
        into.memory_stall_us = stall(&self.pressure_memory, buf);
        into.io_stall_us = stall(&self.pressure_io, buf);
        into.disks.clear();
        if let Some(text) = self.diskstats.as_ref().and_then(|f| f.read(buf)) {
            let known = &mut self.disks;
            parse_disks(text, |name| has_device(known, "/sys/block", name), &mut into.disks);
        }
        into.network = self.netdev.as_ref().and_then(|f| f.read(buf)).and_then(|text| {
            let known = &mut self.interfaces;
            parse_network(text, |name| has_device(known, "/sys/class/net", name))
        });
        // Millidegrees.
        into.cpu_temperature_c =
            self.cpu_temperature.as_ref().and_then(|f| f.read_number(buf)).map(|milli| milli as f32 / 1000.0);
    }

    fn gpus(&self, buf: &mut Vec<u8>, out: &mut Vec<GpuDevice>) {
        for gpu in &self.gpus {
            let used = gpu.memory_used.as_ref().and_then(|f| f.read_number(buf));
            out.push(GpuDevice {
                busy_percent: gpu.busy_percent.read_number(buf).map(|busy| busy as f32),
                memory: used.zip(gpu.memory_total),
            });
        }
    }
}

fn cpu_temperature_source() -> Option<Source> {
    let mut sensors: Vec<_> = std::fs::read_dir("/sys/class/hwmon").ok()?.flatten().map(|entry| entry.path()).collect();
    sensors.sort();
    sensors.into_iter().find_map(|sensor| {
        let name = std::fs::read_to_string(sensor.join("name")).ok()?;
        CPU_SENSORS.contains(&name.trim()).then(|| Source::open(sensor.join("temp1_input")))?
    })
}

fn sysfs_gpus() -> Vec<SysfsGpu> {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    let mut cards: Vec<_> = cards
        .flatten()
        .map(|entry| entry.path())
        // `card0`, not its connectors (`card0-DP-1`).
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("card"))
                .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    cards.sort();
    let mut buf = Vec::new();
    cards
        .into_iter()
        .filter_map(|card| {
            let device = card.join("device");
            Some(SysfsGpu {
                busy_percent: Source::open(device.join("gpu_busy_percent"))?,
                memory_used: Source::open(device.join("mem_info_vram_used")),
                memory_total: Source::open(device.join("mem_info_vram_total"))
                    .and_then(|f| f.read_number(&mut buf))
                    .filter(|total| *total > 0),
            })
        })
        .collect()
}

/// Reads the machine and turns each reading into viewer events.
pub struct HealthSampler {
    sources: Sources,
    /// The sample before the newest, and the newest. Swapped rather than
    /// copied, so the disk list is allocated once.
    prev: Snapshot,
    cur: Snapshot,
    /// When `cur` was read; `None` before the first sample.
    last_ns: Option<u64>,
    state: LaneState,
    /// The newest reading the NVML helper sent for each device since the
    /// last sample. NVIDIA has no figures in sysfs; the helper has them.
    helper_gpus: Vec<(u32, GpuDevice)>,
    gpus: Vec<GpuDevice>,
    readings: Vec<(u32, f32)>,
    buf: Vec<u8>,
}

impl HealthSampler {
    pub fn new() -> HealthSampler {
        HealthSampler {
            sources: Sources::open(),
            prev: Snapshot::default(),
            cur: Snapshot::default(),
            last_ns: None,
            state: LaneState::default(),
            helper_gpus: Vec::new(),
            gpus: Vec::new(),
            readings: Vec::new(),
            buf: Vec::new(),
        }
    }

    /// Whether [`INTERVAL_NS`] has passed since the last sample.
    pub fn due(&self, now_ns: u64) -> bool {
        self.last_ns.is_none_or(|last| now_ns.saturating_sub(last) >= INTERVAL_NS)
    }

    /// Keeps what the NVML helper says about a GPU for the next sample.
    pub fn note_gpu(&mut self, event: &WireEvent) {
        let WireEvent::GpuMetrics {
            device_index, gpu_utilization_percent, memory_used_bytes, memory_total_bytes, ..
        } = event
        else {
            return;
        };
        let reading = GpuDevice {
            busy_percent: (*gpu_utilization_percent != METRIC_UNKNOWN_U32).then_some(*gpu_utilization_percent as f32),
            memory: (*memory_used_bytes != METRIC_UNKNOWN_U64
                && *memory_total_bytes != METRIC_UNKNOWN_U64
                && *memory_total_bytes > 0)
                .then_some((*memory_used_bytes, *memory_total_bytes)),
        };
        match self.helper_gpus.iter_mut().find(|(device, _)| device == device_index) {
            Some((_, latest)) => *latest = reading,
            None => self.helper_gpus.push((*device_index, reading)),
        }
    }

    /// Reads the machine and appends one value event per lane, stamped
    /// `now_ns`.
    pub fn sample(&mut self, now_ns: u64, out: &mut Vec<LiveEvent>) {
        std::mem::swap(&mut self.prev, &mut self.cur);
        self.sources.read(&mut self.cur, &mut self.buf);
        self.gpus.clear();
        self.sources.gpus(&mut self.buf, &mut self.gpus);
        // A helper that stopped reporting must not leave its last figure on
        // the lane for the rest of the capture: each reading is used once.
        self.gpus.extend(self.helper_gpus.drain(..).map(|(_, reading)| reading));
        (self.cur.gpu_percent, self.cur.gpu_memory_percent) = merge_gpus(self.gpus.iter().copied());

        let dt_ns = self.last_ns.map_or(0, |last| now_ns.saturating_sub(last));
        self.readings.clear();
        readings(self.last_ns.map(|_| &self.prev), &self.cur, dt_ns, &mut self.state, &mut self.readings);
        self.last_ns = Some(now_ns);
        for &(id, value) in &self.readings {
            if let Some(lane) = health::lane_by_id(id).filter(|_| value.is_finite()) {
                out.push(LiveEvent::from_value(now_ns, health::PID, lane.group, id, value));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::kind;

    const STAT: &str = "cpu  1000 10 500 8000 400 0 90 0 0 0\n\
                        cpu0 500 5 250 4000 200 0 45 0 0 0\n\
                        intr 12345 0 0\nctxt 99\nprocs_running 3\nprocs_blocked 1\n";

    #[test]
    fn the_cpu_line_counts_iowait_as_idle_and_leaves_the_guest_columns_out() {
        assert_eq!(parse_cpu(STAT), Some(CpuTicks { total: 10_000, idle: 8_400, steal: 0 }));
        // Guest time is already inside user: adding it again would count it twice.
        let guest = parse_cpu("cpu  100 0 0 100 0 0 0 7 50 0\n").unwrap();
        assert_eq!(guest, CpuTicks { total: 207, idle: 100, steal: 7 });
        // A kernel old enough to stop at iowait still has a line worth reading.
        assert_eq!(parse_cpu("cpu  1 2 3 4 5\n"), Some(CpuTicks { total: 15, idle: 9, steal: 0 }));
        assert_eq!(parse_cpu("cpu0 1 2 3 4\n"), None);
        assert_eq!(parse_cpu("cpu  1 2 x 4\n"), None);
        assert_eq!(parse_cpu(""), None);
    }

    const MEMINFO: &str = "MemTotal:       96346812 kB\n\
                           MemFree:         8749364 kB\n\
                           MemAvailable:   78549484 kB\n\
                           SwapCached:       124376 kB\n\
                           SwapTotal:       8388604 kB\n\
                           SwapFree:        6743152 kB\n";

    #[test]
    fn meminfo_gives_what_is_available_not_what_is_free() {
        let memory = parse_memory(MEMINFO).unwrap();
        assert_eq!(
            memory,
            Memory { total: 96_346_812, available: 78_549_484, swap_total: 8_388_604, swap_free: 6_743_152 }
        );
        // A machine with no swap has the two lines at zero, or not at all.
        let no_swap = parse_memory("MemTotal: 100 kB\nMemAvailable: 40 kB\n").unwrap();
        assert_eq!(no_swap.swap_total, 0);
        assert_eq!(parse_memory("MemTotal: 100 kB\n"), None);
    }

    #[test]
    fn a_key_is_a_whole_word() {
        let vmstat = "pgmajfault_extra 7\npgmajfault 1261731\noom_kill 6\n";
        assert_eq!(field(vmstat, "pgmajfault"), Some(1_261_731));
        assert_eq!(field(vmstat, "oom_kill"), Some(6));
        assert_eq!(field(vmstat, "oom"), None);
        assert_eq!(field(MEMINFO, "Swap"), None);
    }

    #[test]
    fn pressure_is_read_from_the_total_of_the_some_line() {
        let io = "some avg10=17.70 avg60=20.82 avg300=20.84 total=2494028904330\n\
                  full avg10=15.73 avg60=18.56 avg300=18.61 total=2451427753833\n";
        assert_eq!(parse_stall_us(io), Some(2_494_028_904_330));
        assert_eq!(parse_stall_us("full avg10=0.00 avg60=0.00 avg300=0.00 total=5\n"), None);
        assert_eq!(parse_stall_us(""), None);
    }

    const DISKSTATS: &str = "   7       0 loop0 3835 0 48256 113 0 0 0 0 0 434 113 0 0 0 0 0 0\n\
         259       0 nvme0n1 3766808 798742 277447391 2397718645 22791857 25515177 1920263594 1231135862 0 2567521 3629593918 434802 0 2820165136 98696 2081023 640713\n\
         259       2 nvme0n1p2 3763158 793405 277346538 2397717906 22791853 25515177 1920263584 1231135862 0 2883943 3628952439 434796 0 2807058976 98671 0 0\n\
           8       0 sda 424 0 14148 1733 0 0 0 0 0 1669 1733 0 0 0 0 0 0\n\
           8      16 short 1 2 3\n";

    #[test]
    fn only_whole_disks_are_counted() {
        let mut disks = vec![Disk::default()];
        parse_disks(DISKSTATS, |name| matches!(name, "nvme0n1" | "sda" | "short"), &mut disks);
        assert_eq!(
            disks,
            vec![
                Disk {
                    id: (259 << 32),
                    sectors_read: 277_447_391,
                    sectors_written: 1_920_263_594,
                    busy_ms: 2_567_521
                },
                Disk { id: (8 << 32), sectors_read: 14_148, sectors_written: 0, busy_ms: 1_669 },
            ],
            "the loop device and the partition are refused by name, the short line for what it lacks"
        );
    }

    const NETDEV: &str = "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    \
         lo: 33729349558 18632965    0    0    0     0          0         0 33729349558 18632965    0    0    0     0       0          0\n  \
         wlo1: 20815704248 23960662    0 2878    0     0          0         0 2872704972 9313242    0  433    0     0       0          0\n\
         enp4s0:     100       1    0    0    0     0          0         0       20       1    0    0    0     0       0          0\n\
         tailscale0: 2898615   46635    0    0    0     0          0         0 64400048  199527    0    0    0     0       0          0\n";

    #[test]
    fn network_bytes_are_summed_over_the_physical_interfaces() {
        let network = parse_network(NETDEV, |name| matches!(name, "wlo1" | "enp4s0")).unwrap();
        assert_eq!(network, Network { received_bytes: 20_815_704_348, sent_bytes: 2_872_704_992 });
        assert_eq!(parse_network(NETDEV, |_| false), None, "no interface, no lane");
    }

    #[test]
    fn the_busiest_gpu_is_the_one_shown_and_memory_is_all_of_them() {
        let gib = 1u64 << 30;
        let devices = [
            GpuDevice { busy_percent: Some(12.0), memory: Some((2 * gib, 8 * gib)) },
            GpuDevice { busy_percent: Some(97.0), memory: Some((6 * gib, 8 * gib)) },
            GpuDevice { busy_percent: None, memory: None },
        ];
        assert_eq!(merge_gpus(devices), (Some(97.0), Some(50.0)));
        assert_eq!(merge_gpus([]), (None, None));
        assert_eq!(merge_gpus([GpuDevice { busy_percent: Some(5.0), memory: None }]), (Some(5.0), None));
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            cpu: Some(CpuTicks { total: 10_000, idle: 8_000, steal: 0 }),
            cpu_stall_us: Some(1_000_000),
            cpu_temperature_c: Some(48.0),
            memory: Some(Memory { total: 1_000, available: 250, swap_total: 4_096, swap_free: 2_048 }),
            memory_stall_us: Some(0),
            major_faults: Some(100),
            oom_kills: Some(6),
            io_stall_us: Some(5_000_000),
            disks: vec![
                Disk { id: 1, sectors_read: 0, sectors_written: 0, busy_ms: 0 },
                Disk { id: 2, sectors_read: 0, sectors_written: 0, busy_ms: 0 },
            ],
            network: Some(Network { received_bytes: 0, sent_bytes: 0 }),
            gpu_percent: Some(30.0),
            gpu_memory_percent: Some(40.0),
        }
    }

    fn value(out: &[(u32, f32)], id: u32) -> Option<f32> {
        out.iter().find(|(lane, _)| *lane == id).map(|(_, value)| *value)
    }

    #[test]
    fn the_first_sample_has_levels_and_no_rates() {
        let mut out = Vec::new();
        readings(None, &snapshot(), 0, &mut LaneState::default(), &mut out);
        assert_eq!(value(&out, lane::MEMORY_USED), Some(75.0));
        assert_eq!(value(&out, lane::SWAP_USED), Some(2.0));
        assert_eq!(value(&out, lane::CPU_TEMPERATURE), Some(48.0));
        assert_eq!(value(&out, lane::GPU), Some(30.0));
        assert_eq!(value(&out, lane::GPU_MEMORY), Some(40.0));
        for rate in [
            lane::CPU,
            lane::CPU_PRESSURE,
            lane::MEMORY_PRESSURE,
            lane::IO_PRESSURE,
            lane::MAJOR_FAULTS,
            lane::DISK_READ,
            lane::DISK_BUSY,
            lane::NETWORK_SEND,
        ] {
            assert_eq!(value(&out, rate), None, "lane {rate} needs an interval");
        }
    }

    #[test]
    fn rates_are_over_the_time_that_actually_passed() {
        let before = snapshot();
        let mut now = snapshot();
        // Half a second later: 400 ticks, 100 of them idle.
        now.cpu = Some(CpuTicks { total: 10_400, idle: 8_100, steal: 0 });
        now.cpu_stall_us = Some(1_050_000);
        now.memory_stall_us = Some(5_000);
        now.io_stall_us = Some(5_250_000);
        now.major_faults = Some(150);
        // 4096 sectors of 512 bytes are 2 MiB.
        now.disks[0].sectors_read = 4_096;
        now.disks[1].sectors_written = 8_192;
        now.disks[0].busy_ms = 100;
        now.disks[1].busy_ms = 450;
        now.network = Some(Network { received_bytes: 1 << 20, sent_bytes: 1 << 19 });
        let mut out = Vec::new();
        readings(Some(&before), &now, 500_000_000, &mut LaneState::default(), &mut out);
        assert_eq!(value(&out, lane::CPU), Some(75.0));
        assert_eq!(value(&out, lane::CPU_PRESSURE), Some(10.0));
        assert_eq!(value(&out, lane::MEMORY_PRESSURE), Some(1.0));
        assert_eq!(value(&out, lane::IO_PRESSURE), Some(50.0));
        assert_eq!(value(&out, lane::MAJOR_FAULTS), Some(100.0));
        assert_eq!(value(&out, lane::DISK_READ), Some(4.0));
        assert_eq!(value(&out, lane::DISK_WRITE), Some(8.0));
        assert_eq!(value(&out, lane::DISK_BUSY), Some(90.0), "the busier of the two disks");
        assert_eq!(value(&out, lane::NETWORK_RECEIVE), Some(2.0));
        assert_eq!(value(&out, lane::NETWORK_SEND), Some(1.0));

        // The same counters over a full second are half the rate.
        out.clear();
        readings(Some(&before), &now, 1_000_000_000, &mut LaneState::default(), &mut out);
        assert_eq!(value(&out, lane::IO_PRESSURE), Some(25.0));
        assert_eq!(value(&out, lane::DISK_READ), Some(2.0));
        assert_eq!(value(&out, lane::DISK_BUSY), Some(45.0));
    }

    #[test]
    fn a_counter_that_went_backwards_reads_as_nothing_not_as_a_spike() {
        let before = snapshot();
        let mut now = snapshot();
        // A disk was unplugged: the sums drop, and so does the stall total
        // of a kernel that reset it.
        now.disks.pop();
        now.io_stall_us = Some(0);
        now.major_faults = Some(0);
        let mut out = Vec::new();
        readings(Some(&before), &now, 500_000_000, &mut LaneState::default(), &mut out);
        assert_eq!(value(&out, lane::IO_PRESSURE), Some(0.0));
        assert_eq!(value(&out, lane::MAJOR_FAULTS), Some(0.0));
        assert_eq!(value(&out, lane::DISK_READ), Some(0.0));
    }

    #[test]
    fn lanes_for_what_the_machine_lacks_are_left_out() {
        let before = Snapshot { cpu: Some(CpuTicks { total: 100, idle: 100, steal: 0 }), ..Snapshot::default() };
        let now = Snapshot {
            cpu: Some(CpuTicks { total: 200, idle: 150, steal: 0 }),
            memory: Some(Memory { total: 1_000, available: 500, swap_total: 0, swap_free: 0 }),
            ..Snapshot::default()
        };
        let mut out = Vec::new();
        readings(Some(&before), &now, 500_000_000, &mut LaneState::default(), &mut out);
        let lanes: Vec<u32> = out.iter().map(|(lane, _)| *lane).collect();
        assert_eq!(lanes, [lane::CPU, lane::MEMORY_USED], "no PSI, no swap, no sensor, no disk, no GPU");
    }

    #[test]
    fn steal_and_oom_kills_appear_when_they_first_happen_and_stay() {
        let mut state = LaneState::default();
        let mut out = Vec::new();
        let first = snapshot();
        readings(None, &first, 0, &mut state, &mut out);
        let mut second = snapshot();
        second.cpu = Some(CpuTicks { total: 10_100, idle: 8_050, steal: 0 });
        readings(Some(&first), &second, 500_000_000, &mut state, &mut out);
        assert_eq!(value(&out, lane::CPU_STEAL), None);
        assert_eq!(value(&out, lane::OOM_KILLS), None, "six kills before the capture are not this capture's");

        let mut third = second.clone();
        third.cpu = Some(CpuTicks { total: 10_200, idle: 8_100, steal: 25 });
        third.oom_kills = Some(8);
        out.clear();
        readings(Some(&second), &third, 500_000_000, &mut state, &mut out);
        assert_eq!(value(&out, lane::CPU_STEAL), Some(25.0));
        assert_eq!(value(&out, lane::OOM_KILLS), Some(2.0));

        let mut fourth = third.clone();
        fourth.cpu = Some(CpuTicks { total: 10_300, idle: 8_150, steal: 25 });
        out.clear();
        readings(Some(&third), &fourth, 500_000_000, &mut state, &mut out);
        assert_eq!(value(&out, lane::CPU_STEAL), Some(0.0), "back to zero is a reading, once the lane exists");
        assert_eq!(value(&out, lane::OOM_KILLS), Some(2.0));
    }

    #[test]
    fn a_helper_reading_is_used_once() {
        let mut sampler = HealthSampler::new();
        let metrics = |utilization: u32| WireEvent::GpuMetrics {
            timestamp_ns: 1,
            device_index: 0,
            gpu_utilization_percent: utilization,
            memory_utilization_percent: METRIC_UNKNOWN_U32,
            memory_used_bytes: 1 << 30,
            memory_total_bytes: 4 << 30,
            process_memory_used_bytes: METRIC_UNKNOWN_U64,
            temperature_celsius: METRIC_UNKNOWN_U32,
            power_milliwatts: METRIC_UNKNOWN_U32,
            sm_clock_mhz: METRIC_UNKNOWN_U32,
            memory_clock_mhz: METRIC_UNKNOWN_U32,
        };
        // Two readings between samples: the newer one stands.
        sampler.note_gpu(&metrics(10));
        sampler.note_gpu(&metrics(60));
        assert_eq!(sampler.helper_gpus.len(), 1);
        let reading = sampler.helper_gpus[0].1;
        assert_eq!(reading, GpuDevice { busy_percent: Some(60.0), memory: Some((1 << 30, 4 << 30)) });
        let mut events = Vec::new();
        sampler.sample(1_000, &mut events);
        assert!(sampler.helper_gpus.is_empty(), "a helper that goes quiet leaves no stale figure behind");
    }

    #[test]
    fn this_machine_is_sampled_into_value_events_on_the_health_process() {
        let mut sampler = HealthSampler::new();
        assert!(sampler.due(0));
        let mut events = Vec::new();
        sampler.sample(1_000_000_000, &mut events);
        assert!(!sampler.due(1_000_000_000 + INTERVAL_NS - 1));
        assert!(sampler.due(1_000_000_000 + INTERVAL_NS));
        std::thread::sleep(std::time::Duration::from_millis(20));
        events.clear();
        sampler.sample(1_020_000_000, &mut events);
        assert!(!events.is_empty(), "every Linux has /proc/stat and /proc/meminfo");
        for event in &events {
            assert_eq!(event.kind, kind::VALUE);
            assert_eq!(event.pid, health::PID);
            assert_eq!(event.start_ns, 1_020_000_000);
            let lane = health::lane_by_id(event.name_id).expect("a lane of the table");
            assert_eq!(event.tid, lane.group);
            let value = event.value_f32().unwrap();
            assert!(value.is_finite() && value >= 0.0, "{} = {value}", lane.name);
            if lane.name.ends_with('%') {
                assert!(value <= 100.0, "{} = {value}", lane.name);
            }
        }
        let has = |id: u32| events.iter().any(|event| event.name_id == id);
        assert!(has(lane::CPU) && has(lane::MEMORY_USED));
    }

    /// What one sample costs the capture loop, and what this machine reads.
    /// Run with `cargo test --release -p perfr health_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn health_bench() {
        let mut sampler = HealthSampler::new();
        let mut events = Vec::new();
        sampler.sample(crate::now_monotonic_ns(), &mut events);
        const ROUNDS: u32 = 2_000;
        let started = std::time::Instant::now();
        for _ in 0..ROUNDS {
            events.clear();
            sampler.sample(crate::now_monotonic_ns(), &mut events);
        }
        let per_sample = started.elapsed() / ROUNDS;
        println!("one sample: {per_sample:?}, {} lanes", events.len());
        std::thread::sleep(std::time::Duration::from_millis(500));
        events.clear();
        sampler.sample(crate::now_monotonic_ns(), &mut events);
        for event in &events {
            let lane = health::lane_by_id(event.name_id).unwrap();
            println!("  {:<24}{:>10.2}", lane.name, event.value_f32().unwrap());
        }
    }
}
