// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The Node Health track: what the whole machine was doing while the
//! capture ran, so a stall on the timeline can be read against memory
//! pressure, a saturated disk or a busy GPU.
//!
//! The service samples a handful of kernel counters twice a second and files
//! each one as a [`crate::kind::VALUE`] lane under one reserved process,
//! [`PID`]. This module is what the service and the viewer agree on: the
//! process, its groups (one thread each) and the lanes, with the least each
//! lane's axis may span. The viewer finds a lane's entry by its name, not
//! its id: a hub re-interns the names of a linked machine, and the names
//! are what survive that.

use crate::dev::MachineId;

/// The process the lanes are filed under. No real process can have it: it is
/// above Linux's `pid_max` ceiling of 2^22. It is also below 2^24, so a hub
/// can pack a linked machine's index above it and each machine keeps its
/// own health track under its own header.
pub const PID: u32 = 0x00FF_0001;
const _: () = assert!(PID >= 1 << 22 && PID < 1 << crate::dev::MACHINE_SHIFT);

/// What the process is called on the rail.
pub const PROCESS_NAME: &str = "Node Health";

/// What it was called through perfr 0.1.4. Captures made then carry this
/// name, and a story or a track order file may ask for the track by it.
pub const FORMER_PROCESS_NAME: &str = "machine health";

/// Whether `pid` is the health process, of this machine or a linked one.
pub fn is_health_pid(pid: u32) -> bool {
    MachineId::unpack(pid) == PID
}

/// The groups, one thread each. The ids are the order on the rail.
pub mod group {
    pub const CPU: u32 = 1;
    pub const MEMORY: u32 = 2;
    pub const IO: u32 = 3;
    pub const GPU: u32 = 4;
}

/// Every group and the name of its thread.
pub const GROUPS: &[(u32, &str)] =
    &[(group::CPU, "cpu"), (group::MEMORY, "memory"), (group::IO, "io"), (group::GPU, "gpu")];

/// The name of the group a thread of the health process is, on this
/// machine or a linked one.
pub fn group_name(tid: u32) -> Option<&'static str> {
    let tid = MachineId::unpack(tid);
    GROUPS.iter().find(|(group, _)| *group == tid).map(|(_, name)| *name)
}

/// Name ids of the lanes. Fixed, so a lane keeps its identity across
/// captures, and all in one block of 256: a value lane is keyed by the low
/// sixteen bits of its name id and ordered by the low byte first, so within
/// a group the ids are also the order on the rail.
pub mod lane {
    pub const CPU: u32 = 4_100;
    pub const CPU_PRESSURE: u32 = 4_101;
    pub const CPU_STEAL: u32 = 4_102;
    pub const CPU_TEMPERATURE: u32 = 4_103;
    pub const MEMORY_USED: u32 = 4_110;
    pub const MEMORY_PRESSURE: u32 = 4_111;
    pub const SWAP_USED: u32 = 4_112;
    pub const MAJOR_FAULTS: u32 = 4_113;
    pub const OOM_KILLS: u32 = 4_114;
    pub const IO_PRESSURE: u32 = 4_120;
    pub const DISK_READ: u32 = 4_121;
    pub const DISK_WRITE: u32 = 4_122;
    pub const DISK_BUSY: u32 = 4_123;
    pub const NETWORK_RECEIVE: u32 = 4_124;
    pub const NETWORK_SEND: u32 = 4_125;
    pub const GPU: u32 = 4_130;
    pub const GPU_MEMORY: u32 = 4_131;
}

/// One lane of the track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lane {
    pub id: u32,
    pub group: u32,
    /// The label, unit included. Also how the viewer finds this entry.
    pub name: &'static str,
    /// The least the top of the lane's axis may be. The axis starts at zero
    /// and ends at this or at the largest value in view, whichever is
    /// higher. A graph scaled to its own extremes draws a CPU idling
    /// between 2 and 4 percent as a storm; with the axis pinned, a lane
    /// that looks busy is busy.
    pub top: f32,
}

const fn entry(id: u32, group: u32, name: &'static str, top: f32) -> Lane {
    Lane { id, group, name, top }
}

/// The lanes, in rail order. A pressure lane is the share of the interval
/// in which at least one task was stalled on that resource (the kernel's
/// PSI "some" figure), so its axis is ten percent, not a hundred: half a
/// second with a tenth of it stalled is already a stall worth seeing.
pub const LANES: &[Lane] = &[
    entry(lane::CPU, group::CPU, "CPU %", 100.0),
    entry(lane::CPU_PRESSURE, group::CPU, "CPU pressure %", 10.0),
    entry(lane::CPU_STEAL, group::CPU, "CPU steal %", 10.0),
    entry(lane::CPU_TEMPERATURE, group::CPU, "CPU temperature C", 100.0),
    entry(lane::MEMORY_USED, group::MEMORY, "memory used %", 100.0),
    entry(lane::MEMORY_PRESSURE, group::MEMORY, "memory pressure %", 10.0),
    entry(lane::SWAP_USED, group::MEMORY, "swap used MiB", 1.0),
    entry(lane::MAJOR_FAULTS, group::MEMORY, "major faults /s", 10.0),
    entry(lane::OOM_KILLS, group::MEMORY, "OOM kills", 1.0),
    entry(lane::IO_PRESSURE, group::IO, "IO pressure %", 10.0),
    entry(lane::DISK_READ, group::IO, "disk read MiB/s", 1.0),
    entry(lane::DISK_WRITE, group::IO, "disk write MiB/s", 1.0),
    entry(lane::DISK_BUSY, group::IO, "disk busy %", 100.0),
    entry(lane::NETWORK_RECEIVE, group::IO, "network receive MiB/s", 1.0),
    entry(lane::NETWORK_SEND, group::IO, "network send MiB/s", 1.0),
    entry(lane::GPU, group::GPU, "GPU %", 100.0),
    entry(lane::GPU_MEMORY, group::GPU, "GPU memory %", 100.0),
];

/// The lane with this id.
pub fn lane_by_id(id: u32) -> Option<&'static Lane> {
    LANES.iter().find(|lane| lane.id == id)
}

/// The lane with this label.
pub fn lane_by_name(name: &str) -> Option<&'static Lane> {
    LANES.iter().find(|lane| lane.name == name)
}

/// Meaning and Linux collector source, for the viewer's metric tooltip.
/// Lookup uses the stable label so linked machines with re-interned ids work too.
pub fn metric_tooltip(name: &str) -> Option<&'static str> {
    Some(match lane_by_name(name)?.id {
        lane::CPU => "CPU time across all cores that is neither idle nor IO-wait, as a percentage of total CPU time between samples. Includes steal time. Source: /proc/stat, aggregate cpu line.",
        lane::CPU_PRESSURE => "Percentage of the sample interval with at least one runnable task waiting for CPU time. Source: /proc/pressure/cpu, change in the PSI some total counter (not avg10).",
        lane::CPU_STEAL => "Percentage of CPU time taken by the hypervisor while this virtual machine wanted to run. Source: /proc/stat, change in steal ticks divided by total ticks. Appears after the first stolen tick.",
        lane::CPU_TEMPERATURE => "CPU package or die temperature in degrees Celsius. Source: /sys/class/hwmon/hwmon*/temp1_input, divided by 1000, from the first available coretemp, k10temp, zenpower or cpu_thermal sensor.",
        lane::MEMORY_USED => "Percentage of RAM not available for new allocations: (MemTotal - MemAvailable) / MemTotal. Reclaimable memory counted as available is excluded. Source: /proc/meminfo.",
        lane::MEMORY_PRESSURE => "Percentage of the sample interval with at least one task stalled by memory pressure. Source: /proc/pressure/memory, change in the PSI some total counter (not avg10).",
        lane::SWAP_USED => "Swap space in use, in MiB: (SwapTotal - SwapFree) / 1024. Source: /proc/meminfo, whose values are in KiB. Omitted when no swap is configured.",
        lane::MAJOR_FAULTS => "Major page faults per second across the machine: faults requiring a page to be read from storage. Source: /proc/vmstat, change in pgmajfault divided by elapsed time.",
        lane::OOM_KILLS => "Cumulative number of processes killed by the kernel's out-of-memory killer since the capture's first health sample. Source: /proc/vmstat, oom_kill minus its initial value. Appears after the first kill.",
        lane::IO_PRESSURE => "Percentage of the sample interval with at least one task stalled waiting for IO. Source: /proc/pressure/io, change in the PSI some total counter (not avg10).",
        lane::DISK_READ => "Read throughput summed across whole physical disks, in MiB/s. Source: /proc/diskstats, change in sectors read times 512 bytes, divided by elapsed time. Disks are selected by /sys/class/block/<name>/device; partitions and stacked virtual devices are excluded.",
        lane::DISK_WRITE => "Write throughput summed across whole physical disks, in MiB/s. Source: /proc/diskstats, change in sectors written times 512 bytes, divided by elapsed time. Disks are selected by /sys/class/block/<name>/device; partitions and stacked virtual devices are excluded.",
        lane::DISK_BUSY => "Percentage of the sample interval spent doing IO on the busiest physical disk (not an average across disks). Source: /proc/diskstats, change in milliseconds doing IO, for disks with /sys/class/block/<name>/device.",
        lane::NETWORK_RECEIVE => "Received traffic summed across physical network interfaces, in MiB/s. Source: /proc/net/dev, change in receive bytes divided by elapsed time. Interfaces must have /sys/class/net/<name>/device; loopback, bridges and tunnels are excluded.",
        lane::NETWORK_SEND => "Sent traffic summed across physical network interfaces, in MiB/s. Source: /proc/net/dev, change in transmit bytes divided by elapsed time. Interfaces must have /sys/class/net/<name>/device; loopback, bridges and tunnels are excluded.",
        lane::GPU => "Utilization of the busiest reporting GPU, in percent. Source: AMD /sys/class/drm/card*/device/gpu_busy_percent, or NVIDIA NVML utilization readings from the GPU helper.",
        lane::GPU_MEMORY => "Used GPU memory as a percentage of total memory, summed across reporting GPUs. Source: AMD /sys/class/drm/card*/device/mem_info_vram_used and mem_info_vram_total, or NVIDIA NVML memory readings from the GPU helper. This measures memory capacity usage, not memory bandwidth.",
        _ => return None,
    })
}

/// The axis of a health lane whose largest value in view is `observed_max`.
pub fn axis(lane: &Lane, observed_max: f32) -> (f32, f32) {
    (0.0, observed_max.max(lane.top))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LiveEvent;

    #[test]
    fn the_pid_is_no_real_process_and_survives_a_hub() {
        assert!(is_health_pid(PID));
        let on_machine_3 = (3 << crate::dev::MACHINE_SHIFT) | PID;
        assert!(is_health_pid(on_machine_3));
        assert_eq!(MachineId::from_pid(on_machine_3), MachineId::Peer(3));
        assert!(!is_health_pid(1234));
        assert!(!is_health_pid(crate::AGENT_PID));
        assert!(!is_health_pid(crate::dev::VIEWER_PID));
        // The hub packs a linked machine's tids the same way.
        assert_eq!(group_name(group::IO), Some("io"));
        assert_eq!(group_name((3 << crate::dev::MACHINE_SHIFT) | group::IO), Some("io"));
        assert_eq!(group_name(99), None);
    }

    #[test]
    fn every_lane_is_a_graph_of_its_own_in_rail_order() {
        let mut keys = Vec::new();
        for lane in LANES {
            assert!(GROUPS.iter().any(|(tid, _)| *tid == lane.group), "{}", lane.name);
            assert_eq!(lane_by_name(lane.name), Some(lane));
            assert_eq!(lane_by_id(lane.id), Some(lane));
            assert!(metric_tooltip(lane.name).is_some(), "{} needs a tooltip", lane.name);
            keys.push(LiveEvent::from_value(0, PID, lane.group, lane.id, 0.0).lane_key());
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, keys, "distinct lane keys, and the table is in the order they sort");
    }

    #[test]
    fn the_axis_starts_at_zero_and_only_grows() {
        let cpu = lane_by_id(lane::CPU).unwrap();
        assert_eq!(axis(cpu, 3.0), (0.0, 100.0));
        let pressure = lane_by_id(lane::IO_PRESSURE).unwrap();
        assert_eq!(axis(pressure, 0.2), (0.0, 10.0));
        assert_eq!(axis(pressure, 64.0), (0.0, 64.0));
    }
}
