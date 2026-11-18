// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The scheduler's picture of the captured machine.
//!
//! The lanes carry a frequency, a NUMA tint and an SMT label. This module
//! is the diagram those lanes open: sockets, nodes, dies or the level 3
//! they share, and the logical threads of each physical core. A sentence
//! that came from the model table is drawn as text and says so. Nothing
//! here invents a CCD a capture did not record.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use perfr_live_event::topology::{kib_text, CpuTopology, LogicalCpu};
use perfr_live_event::{core_kind, CoreInfo};

use crate::fonts;
use crate::theme;

/// Where the diagram groups one machine. Built without egui so the grouping
/// can be tested on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawnSocket {
    pub label: String,
    pub nodes: Vec<DrawnNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawnNode {
    pub label: String,
    pub id: Option<u16>,
    pub groups: Vec<DrawnGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawnGroup {
    pub label: String,
    pub cores: Vec<DrawnCore>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawnCore {
    pub cpus: Vec<u16>,
    pub kind: u8,
    pub core_id: Option<u16>,
}

pub fn drawn(topo: &CpuTopology) -> Vec<DrawnSocket> {
    if topo.cpus.is_empty() {
        return Vec::new();
    }
    let mut sockets: Vec<Option<u16>> = topo.cpus.iter().map(|c| c.socket).collect();
    sockets.sort();
    sockets.dedup();
    sockets
        .into_iter()
        .map(|socket| {
            let cpus: Vec<&LogicalCpu> = topo.cpus.iter().filter(|c| c.socket == socket).collect();
            let label = match socket {
                Some(id) if sockets_len(topo) > 1 || topo.sockets > 1 => format!("socket {id}"),
                Some(id) => format!("socket {id}"),
                None => "socket".into(),
            };
            DrawnSocket { label, nodes: nodes_of(topo, &cpus) }
        })
        .collect()
}

fn sockets_len(topo: &CpuTopology) -> usize {
    let mut ids: Vec<Option<u16>> = topo.cpus.iter().map(|c| c.socket).collect();
    ids.sort();
    ids.dedup();
    ids.len()
}

fn nodes_of(topo: &CpuTopology, cpus: &[&LogicalCpu]) -> Vec<DrawnNode> {
    let mut ids: Vec<Option<u16>> = cpus.iter().map(|c| c.numa).collect();
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let in_node: Vec<&LogicalCpu> = cpus.iter().copied().filter(|c| c.numa == id).collect();
            let mem = id.and_then(|id| topo.numa.iter().find(|n| n.id == id)).map(|n| n.mem_bytes).unwrap_or(0);
            let label = match id {
                Some(id) if mem > 0 => format!("node {id} · {}", perfr_live_event::topology::bytes_text(mem)),
                Some(id) => format!("node {id}"),
                None => "no NUMA node".into(),
            };
            DrawnNode { label, id, groups: groups_of(topo, &in_node) }
        })
        .collect()
}

fn groups_of(topo: &CpuTopology, cpus: &[&LogicalCpu]) -> Vec<DrawnGroup> {
    let by_die = cpus.iter().any(|c| c.die.is_some());
    let mut keys: Vec<GroupKey> = Vec::new();
    for cpu in cpus {
        let key = if by_die {
            GroupKey::Die(cpu.die)
        } else {
            GroupKey::Share(l3_sharers(topo, cpu.cpu))
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.into_iter()
        .map(|key| {
            let in_group: Vec<&LogicalCpu> = cpus.iter().copied().filter(|c| key.matches(c, topo)).collect();
            let l3 = shared_l3_kb(&in_group);
            let label = match &key {
                GroupKey::Die(Some(id)) => match l3 {
                    Some(kb) => format!("die {id} · L3 {}", kib_text(kb)),
                    None => format!("die {id}"),
                },
                GroupKey::Die(None) => "no die id".into(),
                GroupKey::Share(_) => match l3 {
                    Some(kb) => format!("L3 {}", kib_text(kb)),
                    None => String::new(),
                },
            };
            DrawnGroup { label, cores: cores_of(&in_group) }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GroupKey {
    Die(Option<u16>),
    Share(Vec<u16>),
}

impl GroupKey {
    fn matches(&self, cpu: &LogicalCpu, topo: &CpuTopology) -> bool {
        match self {
            GroupKey::Die(id) => cpu.die == *id,
            GroupKey::Share(cpus) => &l3_sharers(topo, cpu.cpu) == cpus,
        }
    }
}

fn l3_sharers(topo: &CpuTopology, cpu: u16) -> Vec<u16> {
    topo.caches
        .iter()
        .find(|c| c.level == 3 && c.cpus.contains(&cpu))
        .map(|c| {
            let mut cpus = c.cpus.clone();
            cpus.sort_unstable();
            cpus
        })
        .unwrap_or_else(|| vec![cpu])
}

fn shared_l3_kb(cpus: &[&LogicalCpu]) -> Option<u32> {
    let sizes: Vec<u32> = cpus.iter().filter_map(|c| (c.l3_kb > 0).then_some(c.l3_kb)).collect();
    if sizes.is_empty() {
        return None;
    }
    let first = sizes[0];
    sizes.iter().all(|s| *s == first).then_some(first)
}

fn cores_of(cpus: &[&LogicalCpu]) -> Vec<DrawnCore> {
    let mut keys: Vec<(Option<u16>, Option<u16>)> = Vec::new();
    for cpu in cpus {
        // A CPU with no core id is its own core.
        let key = core_key(cpu);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.into_iter()
        .map(|key| {
            let mut threads: Vec<&LogicalCpu> = cpus.iter().copied().filter(|c| core_key(c) == key).collect();
            threads.sort_by_key(|c| c.thread.unwrap_or(0));
            DrawnCore {
                cpus: threads.iter().map(|c| c.cpu).collect(),
                kind: threads.first().map(|c| c.kind).unwrap_or(0),
                core_id: threads.first().and_then(|c| c.core_id),
            }
        })
        .collect()
}

fn core_key(cpu: &LogicalCpu) -> (Option<u16>, Option<u16>) {
    if cpu.core_id.is_some() { (cpu.socket, cpu.core_id) } else { (Some(cpu.cpu), None) }
}

/// A lane mark's frequency, preferring the topology's base-and-top form.
pub fn freq_text(machines: &[CpuTopology], cpu: u16) -> Option<String> {
    machines.iter().find_map(|m| m.freq_label(cpu))
}

pub fn cur_ratio(machines: &[CpuTopology], cpu: u16) -> Option<f32> {
    machines.iter().find_map(|m| m.cur_ratio(cpu))
}

pub fn lane_text(machines: &[CpuTopology], cpu: u16) -> Option<String> {
    machines.iter().find_map(|m| m.lane_label(cpu))
}

pub fn numa_of(machines: &[CpuTopology], cpu: u16) -> Option<u16> {
    machines.iter().find_map(|m| m.cpus.iter().find(|c| c.cpu == cpu).and_then(|c| c.numa))
}

pub fn numa_count(machines: &[CpuTopology]) -> usize {
    let mut ids: Vec<u16> = machines.iter().flat_map(|m| m.cpus.iter().filter_map(|c| c.numa)).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

/// More than one node is what makes a tint mean something.
pub fn numa_varies(machines: &[CpuTopology]) -> bool {
    numa_count(machines) > 1
}

pub fn header_line(machines: &[CpuTopology]) -> String {
    let mut parts: Vec<String> = machines.iter().map(|m| m.header_line()).filter(|l| !l.is_empty()).collect();
    parts.dedup();
    parts.join("  ·  ")
}

pub fn cpu_detail(machines: &[CpuTopology], cpu: u16) -> Option<String> {
    machines.iter().find_map(|m| m.cpu_detail(cpu))
}

pub fn copy_text(machines: &[CpuTopology], marks: &[CoreInfo]) -> String {
    if !machines.is_empty() {
        return machines.iter().map(|m| m.summary()).collect::<Vec<_>>().join("\n\n");
    }
    if marks.is_empty() {
        return "This capture has no CPU topology.".into();
    }
    let mut lines = vec!["Lane marks only. This capture has no topology frame.".to_string()];
    for core in marks {
        let mut line = format!("Core {}", core.cpu);
        match core.kind {
            core_kind::PERFORMANCE => line.push_str(" performance"),
            core_kind::EFFICIENCY => line.push_str(" efficiency"),
            _ => {}
        }
        if core.max_freq_khz > 0 {
            line.push_str(&format!(" up to {:.2} GHz", f64::from(core.max_freq_khz) / 1e6));
        }
        if core.l3_kb > 0 {
            line.push_str(&format!(" L3 {}", kib_text(core.l3_kb)));
        }
        lines.push(line);
    }
    lines.join("\n")
}

pub fn search_model(machines: &[CpuTopology]) -> Option<String> {
    machines.iter().find_map(|m| perfr_live_event::topology::search_url(&m.model))
}

pub fn reference_url(machines: &[CpuTopology]) -> Option<String> {
    machines.iter().find_map(|m| (!m.reference_url.is_empty()).then(|| m.reference_url.clone()))
}

/// A quiet tint, one per node, cycling four. Visible against the rail and
/// not a second colour scheme.
pub fn numa_tint(node: u16) -> Color32 {
    const TINTS: [(u8, u8, u8); 4] = [(0x5E, 0x81, 0xAC), (0xA3, 0xBE, 0x8C), (0xEB, 0xCB, 0x8B), (0xB4, 0x8E, 0xAD)];
    let (r, g, b) = TINTS[usize::from(node) % TINTS.len()];
    Color32::from_rgba_unmultiplied(r, g, b, 28)
}

/// The diagram, in a window or under the scheduler header's hover.
pub fn paint_cpu_panel(ui: &mut egui::Ui, machines: &[CpuTopology], marks: &[CoreInfo], max_height: f32) {
    if machines.is_empty() && marks.is_empty() {
        ui.label(egui::RichText::new("This capture has no CPU topology.").color(theme::MUTED()).size(12.0));
        return;
    }
    let owned = if machines.is_empty() { vec![from_marks(marks)] } else { Vec::new() };
    let shown: &[CpuTopology] = if machines.is_empty() { &owned } else { machines };
    egui::ScrollArea::vertical().max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
        for (i, machine) in shown.iter().enumerate() {
            if i > 0 {
                ui.add_space(10.0);
            }
            paint_machine(ui, machine);
        }
    });
}

fn paint_machine(ui: &mut egui::Ui, topo: &CpuTopology) {
    let title = if topo.model.is_empty() { "CPU".to_string() } else { topo.model.clone() };
    ui.label(egui::RichText::new(title).color(theme::TEXT()).size(14.0).strong());
    let mut bits: Vec<String> = Vec::new();
    if topo.sockets > 0 {
        bits.push(format!("{} socket{}", topo.sockets, if topo.sockets == 1 { "" } else { "s" }));
    }
    if topo.physical_cores > 0 {
        bits.push(format!("{} core{}", topo.physical_cores, if topo.physical_cores == 1 { "" } else { "s" }));
    }
    if topo.threads > 0 {
        bits.push(format!("{} thread{}", topo.threads, if topo.threads == 1 { "" } else { "s" }));
    }
    match topo.smt {
        Some(true) => bits.push("SMT on".into()),
        Some(false) => bits.push("SMT off".into()),
        None => {}
    }
    if topo.cache_line_bytes > 0 {
        bits.push(format!("cache line {} B", topo.cache_line_bytes));
    }
    if topo.nps > 0 {
        let how = if topo.nps_source == "derived" { ", derived" } else { "" };
        bits.push(format!("NPS {}{how}", topo.nps));
    }
    if !bits.is_empty() {
        ui.label(egui::RichText::new(bits.join("   ·   ")).color(theme::MUTED()).size(11.0));
    }
    let power = topo.header_line();
    if !power.is_empty() {
        ui.label(egui::RichText::new(power).color(theme::TEXT()).size(11.5));
    }
    if !topo.hostname.is_empty() || !topo.os.is_empty() || !topo.kernel.is_empty() {
        let mut host = Vec::new();
        if !topo.hostname.is_empty() {
            host.push(topo.hostname.clone());
        }
        if !topo.os.is_empty() {
            host.push(topo.os.clone());
        }
        if !topo.kernel.is_empty() {
            host.push(topo.kernel.clone());
        }
        ui.label(egui::RichText::new(host.join("  ·  ")).color(theme::MUTED()).size(10.5));
    }
    ui.add_space(6.0);
    let sockets = drawn(topo);
    if sockets.is_empty() {
        ui.label(egui::RichText::new("No per-CPU topology was recorded.").color(theme::MUTED()).size(11.0));
    }
    for socket in &sockets {
        let socket_rect = ui.scope(|ui| {
            ui.label(egui::RichText::new(&socket.label).color(theme::TEXT()).size(12.0));
            for node in &socket.nodes {
                let tint = node.id.map(numa_tint).unwrap_or(Color32::TRANSPARENT);
                egui::Frame::NONE
                    .fill(tint)
                    .inner_margin(egui::Margin::same(6))
                    .corner_radius(4)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(&node.label).color(theme::MUTED()).size(10.5));
                        for group in &node.groups {
                            if !group.label.is_empty() {
                                ui.label(egui::RichText::new(&group.label).color(theme::TEXT()).size(10.5));
                            }
                            paint_cores(ui, topo, &group.cores);
                        }
                    });
                ui.add_space(4.0);
            }
        });
        ui.painter().rect_stroke(
            socket_rect.response.rect.expand(3.0),
            6.0,
            Stroke::new(1.0, theme::MUTED().gamma_multiply(0.55)),
            StrokeKind::Outside,
        );
        ui.add_space(8.0);
    }
    if topo.layout.memory_channels > 0 {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("{} memory channels", topo.layout.memory_channels)).color(theme::MUTED()).size(10.5));
            ui.label(egui::RichText::new("model table").color(theme::ACCENT()).size(10.0));
        });
        ui.horizontal(|ui| {
            for _ in 0..topo.layout.memory_channels.min(16) {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 8.0), Sense::hover());
                ui.painter().rect_filled(rect, 2.0, theme::ACCENT().gamma_multiply(0.35));
                ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, theme::ACCENT().gamma_multiply(0.8)), StrokeKind::Inside);
            }
        });
    }
    if !topo.layout.summary.is_empty() {
        ui.add_space(4.0);
        let tag = match topo.layout.source.as_str() {
            "lookup" => "From a model table, not measured",
            "measured" => "Reported by the system",
            "mixed" => "Layout",
            _ => "Layout",
        };
        ui.label(egui::RichText::new(tag).color(theme::ACCENT()).size(10.0));
        ui.label(egui::RichText::new(&topo.layout.summary).color(theme::MUTED()).size(11.0));
    }
    paint_legend(ui, topo);
}

fn paint_cores(ui: &mut egui::Ui, topo: &CpuTopology, cores: &[DrawnCore]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
        for core in cores {
            let threads = core.cpus.len().max(1);
            let width = threads as f32 * 18.0 + (threads.saturating_sub(1) as f32) * 2.0 + 8.0;
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::hover());
            let painter = ui.painter();
            let pair = threads > 1;
            if pair {
                painter.rect_stroke(
                    Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.top() + 18.0)).shrink(1.0),
                    3.0,
                    Stroke::new(1.0, theme::MUTED().gamma_multiply(0.7)),
                    StrokeKind::Inside,
                );
            }
            for (i, cpu) in core.cpus.iter().enumerate() {
                let x = rect.left() + 4.0 + i as f32 * 20.0;
                let cell = Rect::from_min_size(Pos2::new(x, rect.top() + 2.0), Vec2::new(16.0, 14.0));
                painter.rect_filled(cell, 2.0, kind_fill(core.kind));
                painter.text(
                    cell.center(),
                    Align2::CENTER_CENTER,
                    format!("{cpu}"),
                    FontId::new(8.0, fonts::medium()),
                    Color32::from_rgb(0x2E, 0x34, 0x40),
                );
            }
            let name = match core.core_id {
                Some(id) => format!("P{id}"),
                None => "core".into(),
            };
            painter.text(
                Pos2::new(rect.center().x, rect.bottom() - 1.0),
                Align2::CENTER_BOTTOM,
                name,
                FontId::new(8.5, fonts::medium()),
                theme::MUTED(),
            );
            if let Some(detail) = topo.cpu_detail(core.cpus[0]) {
                let text = if core.cpus.len() > 1 {
                    format!("{detail}\n\nshares this core with CPU {}", core.cpus[1])
                } else {
                    detail
                };
                response.on_hover_text(text);
            }
        }
    });
}

fn kind_fill(kind: u8) -> Color32 {
    match kind {
        core_kind::PERFORMANCE => theme::ACCENT(),
        core_kind::EFFICIENCY => Color32::from_rgb(0xA3, 0xBE, 0x8C),
        _ => theme::MUTED(),
    }
}

fn paint_legend(ui: &mut egui::Ui, topo: &CpuTopology) {
    let kinds: Vec<u8> = {
        let mut k: Vec<u8> = topo.cpus.iter().map(|c| c.kind).filter(|k| *k != 0).collect();
        k.sort_unstable();
        k.dedup();
        k
    };
    if kinds.is_empty() && topo.smt != Some(true) {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if kinds.contains(&core_kind::PERFORMANCE) {
            swatch(ui, kind_fill(core_kind::PERFORMANCE), "performance");
        }
        if kinds.contains(&core_kind::EFFICIENCY) {
            swatch(ui, kind_fill(core_kind::EFFICIENCY), "efficiency");
        }
        if topo.smt == Some(true) {
            ui.label(egui::RichText::new("bracket: one physical core").color(theme::MUTED()).size(10.0));
        }
    });
}

fn swatch(ui: &mut egui::Ui, color: Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
    ui.painter().rect_filled(rect, 2.0, color);
    ui.label(egui::RichText::new(label).color(theme::MUTED()).size(10.0));
}

fn from_marks(marks: &[CoreInfo]) -> CpuTopology {
    CpuTopology {
        threads: marks.len() as u16,
        cpus: marks
            .iter()
            .map(|core| LogicalCpu {
                cpu: core.cpu,
                kind: core.kind,
                max_freq_khz: core.max_freq_khz,
                l3_kb: core.l3_kb,
                ..LogicalCpu::default()
            })
            .collect(),
        layout: perfr_live_event::topology::LayoutNote {
            source: "measured".into(),
            summary: "Lane marks only. This capture has no topology frame.".into(),
            ..Default::default()
        },
        ..CpuTopology::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::topology::{CacheInstance, LogicalCpu, NumaNode};

    fn cpu(id: u16, socket: u16, die: u16, core: u16, thread: u16, kind: u8, l3: u32) -> LogicalCpu {
        LogicalCpu {
            cpu: id,
            kind,
            socket: Some(socket),
            numa: Some(socket),
            die: Some(die),
            core_id: Some(core),
            thread: Some(thread),
            l3_kb: l3,
            ..LogicalCpu::default()
        }
    }

    #[test]
    fn an_x3d_draws_two_dies_and_a_raptor_pairs_its_performance_threads() {
        let topo = CpuTopology {
            sockets: 1,
            smt: Some(true),
            cpus: vec![
                cpu(0, 0, 0, 0, 0, 0, 98_304),
                cpu(1, 0, 0, 0, 1, 0, 98_304),
                cpu(2, 0, 1, 8, 0, 0, 32_768),
                cpu(3, 0, 1, 8, 1, 0, 32_768),
            ],
            numa: vec![NumaNode { id: 0, cpus: vec![0, 1, 2, 3], mem_bytes: 32 << 30 }],
            ..CpuTopology::default()
        };
        let sockets = drawn(&topo);
        assert_eq!(sockets.len(), 1);
        assert_eq!(sockets[0].nodes.len(), 1);
        assert!(sockets[0].nodes[0].label.contains("32.0 GB") || sockets[0].nodes[0].label.contains("GB"));
        let groups = &sockets[0].nodes[0].groups;
        assert_eq!(groups.len(), 2);
        assert!(groups[0].label.contains("die 0"));
        assert!(groups[0].label.contains("96"));
        assert_eq!(groups[0].cores.len(), 1);
        assert_eq!(groups[0].cores[0].cpus, vec![0, 1]);
        assert_eq!(groups[1].cores[0].cpus, vec![2, 3]);

        let mut raptor = CpuTopology {
            smt: Some(true),
            sockets: 1,
            cpus: vec![
                cpu(0, 0, 0, 0, 0, core_kind::PERFORMANCE, 36_864),
                cpu(1, 0, 0, 0, 1, core_kind::PERFORMANCE, 36_864),
                cpu(4, 0, 0, 2, 0, core_kind::EFFICIENCY, 36_864),
            ],
            caches: vec![CacheInstance {
                level: 3,
                kind: "unified".into(),
                size_kb: 36_864,
                cpus: vec![0, 1, 4],
                ..CacheInstance::default()
            }],
            ..CpuTopology::default()
        };
        // One die, so the group is the die, and the two physical cores stay apart.
        raptor.cpus[2].die = Some(0);
        let groups = &drawn(&raptor)[0].nodes[0].groups;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cores.len(), 2);
        assert_eq!(groups[0].cores[0].cpus, vec![0, 1]);
        assert_eq!(groups[0].cores[1].cpus, vec![4]);
        assert_eq!(groups[0].cores[1].kind, core_kind::EFFICIENCY);
    }
}
