// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The GPU telemetry helper: polls NVML and writes pod events to stdout.
//!
//! This exists as a separate process because the capture service ships as a
//! fully static musl binary, and static musl has no `dlopen`. The helper is
//! dynamically linked, loads `libnvidia-ml` at runtime, and streams pod
//! `GpuMetrics` events over the pipe the service reads -- so the service
//! keeps zero runtime dependencies while still reporting GPU telemetry.
//!
//! It degrades quietly at every level: no library, no driver, no devices, or
//! an unsupported metric all reduce what is reported rather than failing.
//!
//! Usage: perfr-gpu [--interval-ms <n>] [--pid <pid>] [--devices <n>]

mod nvml_sys;

use nvml_sys::Nvml;
use perfr_tracing_state::nvml::NvmlSampler;
use perfr_wire::{Event, Writer, METRIC_UNKNOWN_U64};
use std::io::Write;
use std::time::Duration;

fn stdout_handle() -> std::io::Stdout {
    std::io::stdout()
}

fn now_ns() -> u64 {
    let mut timespec = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime into a local. CLOCK_MONOTONIC matches the clock
    // the capture service stamps perf events with.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timespec);
    }
    timespec.tv_sec as u64 * 1_000_000_000 + timespec.tv_nsec as u64
}

/// The memory attributed to the target process, or "unknown" when the
/// helper was not told which process that is. Attributing to no pid finds
/// nothing, and the resulting 0 was plotted as a real reading: a flat lane
/// at zero under a process that held gigabytes.
fn process_memory_or_unknown(attributed_bytes: u64, target_pid: Option<i32>) -> u64 {
    match target_pid {
        Some(_) => attributed_bytes,
        None => METRIC_UNKNOWN_U64,
    }
}

fn main() {
    let mut interval = Duration::from_millis(100);
    let mut target_pid: Option<i32> = None;
    let mut max_devices = u32::MAX;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--interval-ms" => {
                if let Some(ms) = iter.next().and_then(|v| v.parse().ok()) {
                    interval = Duration::from_millis(ms);
                }
            }
            "--pid" => target_pid = iter.next().and_then(|v| v.parse().ok()).filter(|pid| *pid > 0),
            "--devices" => {
                max_devices = iter.next().and_then(|v| v.parse().ok()).unwrap_or(u32::MAX);
            }
            "--help" | "-h" => {
                eprintln!("perfr-gpu [--interval-ms <n>] [--pid <pid>] [--devices <n>]");
                return;
            }
            other => eprintln!("perfr-gpu: ignoring unknown argument {other}"),
        }
    }

    // No library, no driver: exit quietly. The service treats a helper that
    // produces nothing exactly like no helper at all.
    let Some(nvml) = Nvml::load() else {
        return;
    };
    let device_count = nvml.device_count().min(max_devices);
    if device_count == 0 {
        eprintln!("perfr-gpu: NVML reports no devices");
        return;
    }
    eprintln!("perfr-gpu: {device_count} device(s), polling every {interval:?}");

    // Lead with GpuInfo metadata: the model name, VRAM and driver version a
    // sysfs scan cannot supply. The service writes these into the capture
    // head alongside its own SystemInfo.
    {
        let mut writer = Writer::new();
        for index in 0..device_count {
            let Some(info) = nvml.device_info(index) else { continue };
            writer.write(&Event::GpuInfo {
                device_index: index,
                pci_vendor_id: 0x10de,
                pci_device_id: 0,
                vram_total_bytes: info.vram_total_bytes,
                name: info.name,
                driver_version: info.driver_version,
            });
        }
        let mut lock = stdout_handle().lock();
        if lock.write_all(writer.as_bytes()).is_err() || lock.flush().is_err() {
            return;
        }
    }

    let mut sampler = NvmlSampler::new(interval, target_pid.unwrap_or(0));
    let stdout = stdout_handle();
    loop {
        let now = now_ns();
        if !sampler.is_due(now) {
            // To the next sample, not a millisecond at a time: this runs
            // inside the machine being profiled, and a thousand wakeups a
            // second to take ten samples showed on its scheduler track.
            std::thread::sleep(sampler.until_due(now));
            continue;
        }
        let mut writer = Writer::new();
        for index in 0..device_count {
            let Some(sample) = nvml.sample_device(index, now) else { continue };
            let metrics = sampler.ingest(&sample);
            writer.write(&Event::GpuMetrics {
                timestamp_ns: metrics.timestamp_ns,
                device_index: metrics.device_index,
                gpu_utilization_percent: metrics.gpu_utilization_percent,
                memory_utilization_percent: metrics.memory_utilization_percent,
                memory_used_bytes: metrics.memory_used_bytes,
                memory_total_bytes: metrics.memory_total_bytes,
                process_memory_used_bytes: process_memory_or_unknown(metrics.process_memory_used_bytes, target_pid),
                temperature_celsius: metrics.temperature_celsius,
                power_milliwatts: metrics.power_milliwatts,
                sm_clock_mhz: metrics.sm_clock_mhz,
                memory_clock_mhz: metrics.memory_clock_mhz,
            });
        }
        sampler.mark_sampled(now);
        // A closed pipe means the service stopped: exit rather than spin.
        let mut lock = stdout.lock();
        if lock.write_all(writer.as_bytes()).is_err() || lock.flush().is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_memory_without_a_target_is_unknown_not_zero() {
        assert_eq!(process_memory_or_unknown(0, None), METRIC_UNKNOWN_U64);
        // With a target, what was attributed stands -- zero included: a
        // process that holds no GPU memory is a real answer.
        assert_eq!(process_memory_or_unknown(0, Some(4321)), 0);
        assert_eq!(process_memory_or_unknown(1 << 30, Some(4321)), 1 << 30);
    }
}
