// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Machine context for the head of every capture.
//!
//! A trace outlives the machine it was taken on, and "is this slow?" is
//! unanswerable without knowing what it ran on. So every capture opens with a
//! `SystemInfo` event (CPU model, cores, RAM, kernel, hostname) and a
//! `GpuInfo` event per GPU (vendor, model, VRAM).
//!
//! Everything here reads `/proc` and `/sys` -- plain files, no libraries, so
//! it works unchanged in the fully static binary. The parsing is split from
//! the file reading so it can be tested against fixture text rather than
//! whatever this machine happens to be.
//!
//! The viewer is told one more thing, per capture: what each core is (its
//! kind, its top frequency, its level 3 cache), so the scheduler's lanes
//! can say which of them a thread ran on. See [`cpu_cores`].

use perfr_live_event::{core_kind, CoreInfo};
use perfr_wire::Event;

/// Parsed `/proc/cpuinfo`: the model string, physical cores, and logical
/// threads. Cores come from the distinct (physical id, core id) pairs, which
/// is how a multi-socket or SMT machine reports them; threads are the
/// processor entries.
pub fn parse_cpuinfo(content: &str) -> (Vec<u8>, u32, u32) {
    let mut model = Vec::new();
    let mut threads = 0u32;
    let mut core_ids: Vec<(u32, u32)> = Vec::new();
    let mut physical_id: Option<u32> = None;

    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        let value = value.trim();
        match key {
            "processor" => {
                threads += 1;
                physical_id = None;
            }
            "model name" if model.is_empty() => model = value.as_bytes().to_vec(),
            "physical id" => physical_id = value.parse().ok(),
            "core id" => {
                if let Ok(core) = value.parse::<u32>() {
                    let socket = physical_id.unwrap_or(0);
                    if !core_ids.contains(&(socket, core)) {
                        core_ids.push((socket, core));
                    }
                }
            }
            _ => {}
        }
    }
    // Some kernels and VMs omit core id entirely; fall back to thread count.
    let cores = if core_ids.is_empty() { threads } else { core_ids.len() as u32 };
    (model, cores, threads)
}

/// Total RAM in bytes from `/proc/meminfo` (`MemTotal` is in kB).
pub fn parse_meminfo_total_bytes(content: &str) -> u64 {
    for line in content.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else { continue };
        let kb: u64 = rest.trim().trim_end_matches(" kB").trim().parse().unwrap_or(0);
        return kb * 1024;
    }
    0
}

/// A hex sysfs id like "0x10de" (PCI vendor/device files).
pub fn parse_hex_id(content: &str) -> u32 {
    let trimmed = content.trim();
    let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u32::from_str_radix(digits, 16).unwrap_or(0)
}

/// The well-known PCI vendors, so a capture says "NVIDIA" rather than
/// "0x10de" without shipping a copy of the PCI id database.
pub fn vendor_name(pci_vendor_id: u32) -> &'static [u8] {
    match pci_vendor_id {
        0x10de => b"NVIDIA",
        0x1002 | 0x1022 => b"AMD",
        0x8086 => b"Intel",
        _ => b"",
    }
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Vec<u8> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().as_bytes().to_vec())
        .unwrap_or_default()
}

/// Builds the `SystemInfo` event for this machine and moment.
#[cfg(target_os = "linux")]
pub fn system_info(capture_start_unix_ns: u64, capture_start_monotonic_ns: u64) -> Event {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let (cpu_model, cpu_cores, cpu_threads) = parse_cpuinfo(&cpuinfo);
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    // SAFETY: sysconf is always safe to call.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Event::SystemInfo {
        capture_start_unix_ns,
        capture_start_monotonic_ns,
        hostname: read_trimmed("/proc/sys/kernel/hostname"),
        kernel_release: read_trimmed("/proc/sys/kernel/osrelease"),
        cpu_model,
        cpu_cores,
        cpu_threads,
        ram_total_bytes: parse_meminfo_total_bytes(&meminfo),
        page_size_bytes: if page_size > 0 { page_size as u64 } else { 4096 },
    }
}

/// A sysfs CPU list ("0-15", "0-3,8,10-11") as the CPUs it names. A part
/// that is not a number or a range is left out.
pub fn parse_cpu_list(content: &str) -> Vec<u16> {
    let mut cpus = Vec::new();
    for part in content.trim().split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let (Ok(first), Ok(last)) = (first.trim().parse::<u16>(), last.trim().parse::<u16>()) else { continue };
        cpus.extend(first..=last);
    }
    cpus
}

/// A sysfs cache size ("36864K", "32M") in KiB; zero when it is neither.
pub fn parse_cache_size_kb(content: &str) -> u32 {
    let text = content.trim();
    let (digits, scale) = match text.as_bytes().last() {
        Some(b'K') => (&text[..text.len() - 1], 1),
        Some(b'M') => (&text[..text.len() - 1], 1024),
        Some(b'G') => (&text[..text.len() - 1], 1024 * 1024),
        _ => (text, 1),
    };
    digits.parse::<u32>().ok().and_then(|n| n.checked_mul(scale)).unwrap_or(0)
}

/// What each core of the machine is, from the sysfs tree at `devices`
/// (`/sys/devices`): one entry per `system/cpu/cpuN`, in the order of their
/// numbers. Nothing when no core has anything to tell, as in a container
/// with no cpufreq and no cache directory.
///
/// The kind is what the kernel's hybrid PMUs say (`cpu_core/cpus` and
/// `cpu_atom/cpus` on Intel, with `cpu_lowpower` among the efficiency
/// cores), or else the capacities of an asymmetric machine
/// (`cpu_capacity`, big.LITTLE): the cores of the highest capacity are the
/// performance cores. The top frequency is `cpufreq/cpuinfo_max_freq`, and
/// the cache is the size of the `cache/index*` whose level is 3. These are
/// the files a tool that pins a game to "the fast cores" reads.
pub fn cpu_cores_from_sysfs(devices: &std::path::Path) -> Vec<CoreInfo> {
    let read = |path: std::path::PathBuf| std::fs::read_to_string(path).unwrap_or_default();
    let cpu_dir = devices.join("system/cpu");
    let Ok(entries) = std::fs::read_dir(&cpu_dir) else { return Vec::new() };
    let mut cpus: Vec<u16> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_prefix("cpu")?.parse().ok())
        .collect();
    cpus.sort_unstable();
    cpus.dedup();

    let performance = parse_cpu_list(&read(devices.join("cpu_core/cpus")));
    let mut efficiency = parse_cpu_list(&read(devices.join("cpu_atom/cpus")));
    efficiency.extend(parse_cpu_list(&read(devices.join("cpu_lowpower/cpus"))));
    let hybrid = !performance.is_empty() && !efficiency.is_empty();
    let capacities: Vec<u32> = cpus
        .iter()
        .map(|cpu| read(cpu_dir.join(format!("cpu{cpu}/cpu_capacity"))).trim().parse().unwrap_or(0))
        .collect();
    let top_capacity = capacities.iter().copied().max().unwrap_or(0);
    let asymmetric = capacities.iter().any(|&c| c != 0 && c != top_capacity);

    let cores: Vec<CoreInfo> = cpus
        .iter()
        .zip(&capacities)
        .map(|(&cpu, &capacity)| {
            let dir = cpu_dir.join(format!("cpu{cpu}"));
            let kind = if hybrid {
                if performance.contains(&cpu) {
                    core_kind::PERFORMANCE
                } else if efficiency.contains(&cpu) {
                    core_kind::EFFICIENCY
                } else {
                    core_kind::UNKNOWN
                }
            } else if asymmetric && capacity != 0 {
                if capacity == top_capacity { core_kind::PERFORMANCE } else { core_kind::EFFICIENCY }
            } else {
                core_kind::UNKNOWN
            };
            let l3_kb = (0..10)
                .map(|index| dir.join(format!("cache/index{index}")))
                .find(|cache| read(cache.join("level")).trim() == "3")
                .map(|cache| parse_cache_size_kb(&read(cache.join("size"))))
                .unwrap_or(0);
            CoreInfo {
                cpu,
                kind,
                max_freq_khz: read(dir.join("cpufreq/cpuinfo_max_freq")).trim().parse().unwrap_or(0),
                l3_kb,
            }
        })
        .collect();
    if cores.iter().all(|c| c.kind == core_kind::UNKNOWN && c.max_freq_khz == 0 && c.l3_kb == 0) {
        return Vec::new();
    }
    cores
}

/// What each core of this machine is, for the viewer's scheduler lanes;
/// empty where the system does not say. Windows has its own
/// (`windows::cpu_cores`); macOS has no scheduler track to mark.
#[cfg(target_os = "linux")]
pub fn cpu_cores() -> Vec<CoreInfo> {
    cpu_cores_from_sysfs(std::path::Path::new("/sys/devices"))
}

// The Windows file capture does not write a SystemInfo event yet; the
// function lives in windows.rs and is named here for the test below, and
// for the day it does.
#[cfg(target_os = "windows")]
pub use crate::windows::system_info;

/// Read a public sysctl value without assuming its string or integer width.
#[cfg(target_os = "macos")]
fn sysctl(name: &std::ffi::CStr) -> Vec<u8> {
    let mut len = 0;
    unsafe {
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len,
                             std::ptr::null_mut(), 0) != 0 { return Vec::new(); }
        let mut value = vec![0u8; len];
        if libc::sysctlbyname(name.as_ptr(), value.as_mut_ptr().cast(), &mut len,
                             std::ptr::null_mut(), 0) != 0 { return Vec::new(); }
        value.truncate(len);
        value
    }
}

#[cfg(target_os = "macos")]
pub fn system_info(capture_start_unix_ns: u64, capture_start_monotonic_ns: u64) -> Event {
    let string = |name| {
        let mut bytes = sysctl(name);
        while bytes.last() == Some(&0) { bytes.pop(); }
        bytes
    };
    let number = |name| match sysctl(name).as_slice() {
        bytes if bytes.len() == 4 => u32::from_ne_bytes(bytes.try_into().unwrap()) as u64,
        bytes if bytes.len() == 8 => u64::from_ne_bytes(bytes.try_into().unwrap()),
        _ => 0,
    };
    let mut cpu_model = string(c"machdep.cpu.brand_string");
    if cpu_model.is_empty() { cpu_model = string(c"hw.model"); }
    Event::SystemInfo {
        capture_start_unix_ns, capture_start_monotonic_ns,
        hostname: string(c"kern.hostname"), kernel_release: string(c"kern.osrelease"),
        cpu_model, cpu_cores: number(c"hw.physicalcpu") as u32,
        cpu_threads: number(c"hw.logicalcpu") as u32,
        ram_total_bytes: number(c"hw.memsize"),
        page_size_bytes: unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64,
    }
}

/// Discovers GPUs from sysfs. This is the vendor-neutral fallback that works
/// with no driver library at all: every DRM card exposes its PCI ids, and
/// amdgpu additionally exposes VRAM size. A telemetry helper, when one runs,
/// reports richer info (model name, driver version) for its own devices.
#[cfg(target_os = "linux")]
pub fn gpu_info_from_sysfs() -> Vec<Event> {
    let mut events = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return events };
    let mut cards: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // "card0", not "card0-DP-1" (those are connectors, not devices).
            (name.starts_with("card") && !name.contains('-')).then_some(name)
        })
        .collect();
    cards.sort();

    for (index, card) in cards.iter().enumerate() {
        let device_dir = format!("/sys/class/drm/{card}/device");
        let vendor = parse_hex_id(&String::from_utf8_lossy(&read_trimmed(&format!(
            "{device_dir}/vendor"
        ))));
        if vendor == 0 {
            continue;
        }
        let device = parse_hex_id(&String::from_utf8_lossy(&read_trimmed(&format!(
            "{device_dir}/device"
        ))));
        // amdgpu publishes VRAM here; other drivers do not, and a helper
        // fills it in for them.
        let vram = String::from_utf8_lossy(&read_trimmed(&format!(
            "{device_dir}/mem_info_vram_total"
        )))
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
        events.push(Event::GpuInfo {
            device_index: index as u32,
            pci_vendor_id: vendor,
            pci_device_id: device,
            vram_total_bytes: vram,
            name: vendor_name(vendor).to_vec(),
            driver_version: Vec::new(),
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    const CPUINFO: &str = "\
processor\t: 0
model name\t: AMD Ryzen 9 7950X 16-Core Processor
physical id\t: 0
core id\t\t: 0

processor\t: 1
model name\t: AMD Ryzen 9 7950X 16-Core Processor
physical id\t: 0
core id\t\t: 0

processor\t: 2
model name\t: AMD Ryzen 9 7950X 16-Core Processor
physical id\t: 0
core id\t\t: 1
";

    #[test]
    fn cpuinfo_yields_model_cores_and_threads() {
        let (model, cores, threads) = parse_cpuinfo(CPUINFO);
        assert_eq!(model, b"AMD Ryzen 9 7950X 16-Core Processor");
        assert_eq!(threads, 3); // three processor entries
        assert_eq!(cores, 2); // core 0 (twice, SMT) and core 1
    }

    #[test]
    fn cpuinfo_without_core_ids_falls_back_to_threads() {
        let content = "processor\t: 0\nmodel name\t: Some vCPU\n\nprocessor\t: 1\n";
        let (model, cores, threads) = parse_cpuinfo(content);
        assert_eq!(model, b"Some vCPU");
        assert_eq!(threads, 2);
        assert_eq!(cores, 2);
    }

    #[test]
    fn two_sockets_do_not_collapse_their_core_ids() {
        let content = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0

processor\t: 1
physical id\t: 1
core id\t\t: 0
";
        let (_, cores, threads) = parse_cpuinfo(content);
        assert_eq!(threads, 2);
        // Same core id on different sockets is two distinct cores.
        assert_eq!(cores, 2);
    }

    #[test]
    fn cpu_lists_and_cache_sizes_parse_as_sysfs_writes_them() {
        assert_eq!(parse_cpu_list("0-3\n"), [0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("0-1,8,10-11"), [0, 1, 8, 10, 11]);
        assert_eq!(parse_cpu_list("7"), [7]);
        assert_eq!(parse_cpu_list(""), [] as [u16; 0]);
        assert_eq!(parse_cpu_list("a-b,2"), [2]);
        assert_eq!(parse_cache_size_kb("36864K\n"), 36_864);
        assert_eq!(parse_cache_size_kb("96M"), 98_304);
        assert_eq!(parse_cache_size_kb("512"), 512);
        assert_eq!(parse_cache_size_kb("big"), 0);
    }

    /// A sysfs tree in a fresh directory: `(path under devices, content)`.
    struct Sysfs(std::path::PathBuf);

    impl Sysfs {
        fn new(name: &str, files: &[(String, &str)]) -> Sysfs {
            let dir = std::env::temp_dir().join(format!("perfr-sysfs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            for (path, content) in files {
                let path = dir.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }
            Sysfs(dir)
        }
    }

    impl Drop for Sysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
        let sysfs = Sysfs::new("hybrid", &files);
        assert_eq!(
            cpu_cores_from_sysfs(&sysfs.0),
            [
                CoreInfo { cpu: 0, kind: core_kind::PERFORMANCE, max_freq_khz: 5_500_000, l3_kb: 36_864 },
                CoreInfo { cpu: 1, kind: core_kind::PERFORMANCE, max_freq_khz: 5_800_000, l3_kb: 36_864 },
                CoreInfo { cpu: 2, kind: core_kind::EFFICIENCY, max_freq_khz: 4_300_000, l3_kb: 36_864 },
                CoreInfo { cpu: 10, kind: core_kind::EFFICIENCY, max_freq_khz: 4_300_000, l3_kb: 36_864 },
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
        let sysfs = Sysfs::new("x3d", &files);
        assert_eq!(
            cpu_cores_from_sysfs(&sysfs.0),
            [
                CoreInfo { cpu: 0, kind: core_kind::UNKNOWN, max_freq_khz: 5_050_000, l3_kb: 98_304 },
                CoreInfo { cpu: 1, kind: core_kind::UNKNOWN, max_freq_khz: 5_759_000, l3_kb: 32_768 },
            ]
        );
    }

    #[test]
    fn the_capacities_of_an_asymmetric_machine_name_its_big_and_little_cores() {
        let files = vec![
            ("system/cpu/cpu0/cpu_capacity".to_string(), "430\n"),
            ("system/cpu/cpu0/cpufreq/cpuinfo_max_freq".to_string(), "1800000\n"),
            ("system/cpu/cpu1/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu1/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
        ];
        let sysfs = Sysfs::new("biglittle", &files);
        assert_eq!(
            cpu_cores_from_sysfs(&sysfs.0),
            [
                CoreInfo { cpu: 0, kind: core_kind::EFFICIENCY, max_freq_khz: 1_800_000, l3_kb: 0 },
                CoreInfo { cpu: 1, kind: core_kind::PERFORMANCE, max_freq_khz: 2_400_000, l3_kb: 0 },
            ]
        );
        // The same capacity everywhere is one kind of core, not named.
        let files = vec![
            ("system/cpu/cpu0/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu0/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
            ("system/cpu/cpu1/cpu_capacity".to_string(), "1024\n"),
            ("system/cpu/cpu1/cpufreq/cpuinfo_max_freq".to_string(), "2400000\n"),
        ];
        let sysfs = Sysfs::new("symmetric", &files);
        assert!(cpu_cores_from_sysfs(&sysfs.0).iter().all(|c| c.kind == core_kind::UNKNOWN));
    }

    #[test]
    fn a_machine_that_tells_nothing_of_its_cores_has_none_to_show() {
        // A container: the cores are there, with no frequency and no cache.
        let files = vec![
            ("system/cpu/cpu0/online".to_string(), "1\n"),
            ("system/cpu/cpu1/online".to_string(), "1\n"),
        ];
        let sysfs = Sysfs::new("bare", &files);
        assert!(cpu_cores_from_sysfs(&sysfs.0).is_empty());
        assert!(cpu_cores_from_sysfs(std::path::Path::new("/nonexistent/perfr")).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_cores_are_this_machines_or_none() {
        let cores = cpu_cores();
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        // One entry per CPU the kernel lists, in order; a restricted
        // affinity makes `threads` the smaller of the two.
        assert!(cores.is_empty() || cores.len() >= threads, "{} cores for {threads} threads", cores.len());
        assert!(cores.windows(2).all(|w| w[0].cpu < w[1].cpu));
    }

    #[test]
    fn meminfo_total_is_converted_from_kb() {
        let content = "MemTotal:       65597284 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo_total_bytes(content), 65_597_284 * 1024);
        assert_eq!(parse_meminfo_total_bytes("nothing here"), 0);
    }

    #[test]
    fn hex_ids_parse_with_or_without_prefix() {
        assert_eq!(parse_hex_id("0x10de"), 0x10de);
        assert_eq!(parse_hex_id("10de\n"), 0x10de);
        assert_eq!(parse_hex_id("garbage"), 0);
    }

    #[test]
    fn known_vendors_get_names() {
        assert_eq!(vendor_name(0x10de), b"NVIDIA");
        assert_eq!(vendor_name(0x1002), b"AMD");
        assert_eq!(vendor_name(0x8086), b"Intel");
        assert_eq!(vendor_name(0xbeef), b"");
    }

    #[test]
    fn system_info_reports_this_machine() {
        let Event::SystemInfo { cpu_model, cpu_threads, ram_total_bytes, page_size_bytes, .. } =
            system_info(1, 2)
        else {
            panic!("wrong event");
        };
        assert!(!cpu_model.is_empty(), "no CPU model read");
        assert!(cpu_threads > 0);
        assert!(ram_total_bytes > 0);
        assert!(page_size_bytes >= 4096);
    }
}
