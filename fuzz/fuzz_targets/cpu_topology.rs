// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! The machine a capture says it was recorded on: the JSON of a
//! `CpuTopology` frame, and then everything the viewer and the hub do with
//! one, for every CPU it lists and one it does not.
#![no_main]

use libfuzzer_sys::fuzz_target;
use perfr_live_event::CpuTopology;

fuzz_target!(|data: &[u8]| {
    let Ok(topology) = CpuTopology::from_json(data) else { return };
    let _ = topology.summary();
    let _ = topology.header_line();
    let _ = topology.core_infos();
    let _ = topology.product_page();
    let numbers = topology.core_numbers();
    let cpus: Vec<u16> = topology.cpus.iter().map(|cpu| cpu.cpu).chain([u16::MAX]).collect();
    for cpu in cpus {
        let _ = topology.cpu_detail(cpu).map(|detail| detail.text());
        let _ = topology.lane_parts(cpu, &numbers);
        let _ = topology.freq_label(cpu);
    }
    // A hub moves a remote's CPUs onto its own lanes.
    let shifted = topology.shift_lanes(200);
    let _ = shifted.same_machine(&topology);
    let _ = CpuTopology::from_json(&shifted.to_json());
});
