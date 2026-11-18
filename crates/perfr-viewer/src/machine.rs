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
//!
//! [`Machines`] is the capture's machines with what the lanes draw of them
//! worked out when a machine is said. A lane is painted every frame and a
//! machine is said once: nothing here is searched for per lane.

use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use perfr_live_event::topology::{kib_text, CpuDetail, CpuTopology, LogicalCpu};
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
    /// The core's number as the lanes say it (`CpuTopology::core_numbers`).
    pub number: Option<u16>,
}

pub fn drawn(topo: &CpuTopology) -> Vec<DrawnSocket> {
    if topo.cpus.is_empty() {
        return Vec::new();
    }
    let mut sockets: Vec<Option<u16>> = topo.cpus.iter().map(|c| c.socket).collect();
    sockets.sort();
    sockets.dedup();
    let mut sockets: Vec<DrawnSocket> = sockets
        .into_iter()
        .map(|socket| {
            let cpus: Vec<&LogicalCpu> = topo.cpus.iter().filter(|c| c.socket == socket).collect();
            let label = match socket {
                Some(id) => format!("socket {id}"),
                None => "socket".into(),
            };
            DrawnSocket { label, nodes: nodes_of(topo, &cpus) }
        })
        .collect();
    // Each core by the number its lanes have.
    let numbers = topo.core_numbers();
    for core in sockets.iter_mut().flat_map(|s| &mut s.nodes).flat_map(|n| &mut n.groups).flat_map(|g| &mut g.cores) {
        core.number = core.cpus.first().and_then(|cpu| numbers.get(cpu).copied());
    }
    sockets
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
    let mut keys: Vec<CoreKey> = Vec::new();
    for cpu in cpus {
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
                number: None,
            }
        })
        .collect()
}

/// Which physical core a CPU is a thread of. A CPU with no core id is its
/// own core; with one, the socket, die and cluster go with it, as in
/// [`LogicalCpu::same_core`]: clusters and sockets each number their own.
type CoreKey = (Option<u16>, Option<u16>, Option<u16>, Option<u16>, Option<u16>);

fn core_key(cpu: &LogicalCpu) -> CoreKey {
    if cpu.core_id.is_some() { (None, cpu.socket, cpu.die, cpu.cluster, cpu.core_id) } else { (Some(cpu.cpu), None, None, None, None) }
}

/// Where a lane stands among the lanes of its physical core, when they are
/// next to each other: the bracket in the rail that joins them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bracket {
    /// The core has one thread, or its threads' lanes are not adjacent:
    /// the label says the pairing and no bracket could.
    #[default]
    None,
    First,
    Middle,
    Last,
}

/// What a scheduler lane draws of its CPU.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Lane {
    /// The machine the CPU is in, as an index into [`Machines::list`].
    machine: usize,
    /// The lane's name where the capture recorded the pairing, in the two
    /// columns it is drawn in: the physical core (`C0`), counted from 0, and
    /// the thread of it (`T1`) where the core has more than one.
    pub core: Option<String>,
    pub thread: Option<String>,
    /// `3.0–5.8`, base and top.
    pub freq: Option<String>,
    pub numa: Option<u16>,
    pub bracket: Bracket,
}

/// The machines of the capture (`CpuTopology`), and what its lanes and its
/// diagram draw of them. Empty when the capture did not record one.
#[derive(Default)]
pub struct Machines {
    list: Vec<CpuTopology>,
    lanes: HashMap<u16, Lane>,
    drawn: Vec<Vec<DrawnSocket>>,
    numa_varies: bool,
    header: String,
    generation: u64,
}

impl Machines {
    /// Takes a machine as the capture says it: in the place of what was
    /// said of the same machine before, or beside the others.
    pub fn adopt(&mut self, topology: CpuTopology) {
        perfr_live_event::topology::adopt_machine(&mut self.list, topology);
        self.rebuild();
    }

    pub fn clear(&mut self) {
        if !self.list.is_empty() {
            self.list.clear();
            self.rebuild();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    #[cfg(test)]
    pub fn list(&self) -> &[CpuTopology] {
        &self.list
    }

    /// Changes whenever the machines do. What is measured from them (the
    /// width of the widest lane label) is kept under it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn rebuild(&mut self) {
        self.generation += 1;
        self.lanes.clear();
        for (index, machine) in self.list.iter().enumerate() {
            let numbers = machine.core_numbers();
            for (at, cpu) in machine.cpus.iter().enumerate() {
                // The threads of a core are bracketed where their lanes are
                // next to each other, which is where their numbers are.
                let beside = |other: Option<&LogicalCpu>, number: Option<u16>| {
                    machine.smt == Some(true) && other.is_some_and(|other| Some(other.cpu) == number && other.same_core(cpu))
                };
                let above = beside(at.checked_sub(1).and_then(|i| machine.cpus.get(i)), cpu.cpu.checked_sub(1));
                let below = beside(machine.cpus.get(at + 1), cpu.cpu.checked_add(1));
                let bracket = match (above, below) {
                    (false, true) => Bracket::First,
                    (true, true) => Bracket::Middle,
                    (true, false) => Bracket::Last,
                    (false, false) => Bracket::None,
                };
                // A CPU number is one machine's; the first to say it keeps it.
                let (core, thread) = machine.lane_parts(cpu.cpu, &numbers).map_or((None, None), |(core, thread)| (Some(core), thread));
                self.lanes.entry(cpu.cpu).or_insert_with(|| Lane {
                    machine: index,
                    core,
                    thread,
                    freq: machine.freq_label(cpu.cpu),
                    numa: cpu.numa,
                    bracket,
                });
            }
        }
        self.drawn = self.list.iter().map(drawn).collect();
        // More than one node is what makes a tint mean something.
        let mut nodes: Vec<u16> = self.lanes.values().filter_map(|lane| lane.numa).collect();
        nodes.sort_unstable();
        nodes.dedup();
        self.numa_varies = nodes.len() > 1;
        let mut parts: Vec<String> = self.list.iter().map(|m| m.header_line()).filter(|l| !l.is_empty()).collect();
        parts.dedup();
        self.header = parts.join("  ·  ");
    }

    /// What the lane of CPU `cpu` draws; none when no machine lists it.
    pub fn lane(&self, cpu: u16) -> Option<&Lane> {
        self.lanes.get(&cpu)
    }

    /// The NUMA node of `cpu`'s lane, where the capture has more than one
    /// node: the lanes are tinted by it.
    pub fn tinted_node(&self, cpu: u16) -> Option<u16> {
        if !self.numa_varies {
            return None;
        }
        self.lanes.get(&cpu)?.numa
    }

    /// The power line of the scheduler row.
    pub fn header_line(&self) -> &str {
        &self.header
    }

    /// Everything recorded of `cpu`, for the lane's tooltip. Built when it
    /// is asked for, which is when the pointer is on the lane.
    pub fn detail(&self, cpu: u16) -> Option<CpuDetail> {
        self.list.get(self.lanes.get(&cpu)?.machine)?.cpu_detail(cpu)
    }

    pub fn copy_text(&self, marks: &[CoreInfo]) -> String {
        if !self.list.is_empty() {
            return self.list.iter().map(|m| m.summary()).collect::<Vec<_>>().join("\n\n");
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

    /// A web search for the first machine's model string.
    pub fn search_model(&self) -> Option<String> {
        self.list.iter().find_map(|m| perfr_live_event::topology::search_url(&m.model))
    }

    /// The product page of the first machine that has one in this build's
    /// table. Never a link out of the capture: see
    /// [`CpuTopology::product_page`].
    pub fn product_page(&self) -> Option<&'static str> {
        self.list.iter().find_map(|m| m.product_page())
    }
}

/// A quiet tint, one per node, cycling four: a wash over the rail, deeper
/// on a light scheme, where a pale wash would not show.
pub fn numa_tint(node: u16) -> Color32 {
    const ON_DARK: [(u8, u8, u8); 4] = [(0x5E, 0x81, 0xAC), (0xA3, 0xBE, 0x8C), (0xEB, 0xCB, 0x8B), (0xB4, 0x8E, 0xAD)];
    const ON_LIGHT: [(u8, u8, u8); 4] = [(0x1F, 0x5F, 0xA8), (0x3F, 0x7D, 0x2A), (0xA8, 0x6F, 0x00), (0x7F, 0x3F, 0x8C)];
    let light = perfr_live_event::theme::active().light;
    let (r, g, b) = if light { ON_LIGHT } else { ON_DARK }[usize::from(node) % 4];
    Color32::from_rgba_unmultiplied(r, g, b, if light { 34 } else { 28 })
}

/// The line between the lanes of two nodes, and the hairlines of the
/// diagram: the scheme's muted text, thinned.
pub fn divider() -> Color32 {
    theme::MUTED().gamma_multiply(0.5)
}

/// A core's detail as a table: the title, then each group's rows with the
/// labels in one column. `note` is the last line, for what a click does.
pub fn paint_detail(ui: &mut egui::Ui, detail: &CpuDetail, note: Option<(&str, Color32)>) {
    const LABEL_W: f32 = 128.0;
    ui.spacing_mut().item_spacing = Vec2::new(8.0, 2.0);
    ui.label(RichText::new(&detail.title).family(fonts::medium()).size(12.5).color(theme::TEXT()));
    for group in &detail.groups {
        if !group.heading.is_empty() {
            ui.add_space(5.0);
            ui.label(RichText::new(group.heading.to_uppercase()).family(fonts::medium()).size(9.0).color(theme::ACCENT()));
        }
        for (label, value) in &group.rows {
            ui.horizontal_top(|ui| {
                let (cell, _) = ui.allocate_exact_size(Vec2::new(LABEL_W, 14.0), Sense::hover());
                ui.painter().text(cell.left_center(), Align2::LEFT_CENTER, label, FontId::new(11.0, egui::FontFamily::Proportional), theme::MUTED());
                ui.add(egui::Label::new(RichText::new(value).size(11.0).color(theme::TEXT())).wrap());
            });
        }
    }
    if let Some((note, color)) = note {
        ui.add_space(5.0);
        ui.label(RichText::new(note).font(FontId::monospace(10.5)).color(color));
    }
}

/// A lane's tooltip, at the pointer for as long as it is on the lane: the
/// core's detail, and whether a click has just copied it.
pub fn show_core_tooltip(ui: &egui::Ui, detail: &CpuDetail, copied: bool) {
    let _ = egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), egui::Id::new("perfr-core-tip"), egui::PopupAnchor::Pointer)
        .at_pointer()
        .gap(8.0)
        .show(|ui| {
            ui.set_max_width(470.0);
            let note = if copied { ("copied to the clipboard", theme::ACCENT()) } else { ("click to copy", theme::MUTED()) };
            paint_detail(ui, detail, Some(note));
        });
}

/// The diagram, in a window or under the scheduler header's hover.
pub fn paint_cpu_panel(ui: &mut egui::Ui, machines: &Machines, marks: &[CoreInfo], max_height: f32) {
    if machines.is_empty() && marks.is_empty() {
        ui.label(egui::RichText::new("This capture has no CPU topology.").color(theme::MUTED()).size(12.0));
        return;
    }
    egui::ScrollArea::vertical().max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
        if machines.is_empty() {
            // A capture from before the topology frame: its lane marks, as
            // the one machine they are of.
            let machine = from_marks(marks);
            paint_machine(ui, &machine, &drawn(&machine));
            return;
        }
        for (i, (machine, sockets)) in machines.list.iter().zip(&machines.drawn).enumerate() {
            if i > 0 {
                ui.add_space(10.0);
            }
            paint_machine(ui, machine, sockets);
        }
    });
}

fn paint_machine(ui: &mut egui::Ui, topo: &CpuTopology, sockets: &[DrawnSocket]) {
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
    if sockets.is_empty() {
        ui.label(egui::RichText::new("No per-CPU topology was recorded.").color(theme::MUTED()).size(11.0));
    }
    for socket in sockets {
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
            Stroke::new(1.0, divider()),
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
                    Stroke::new(1.0, divider()),
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
                    // The number is cut out of the cell in the colour
                    // behind it, which reads on a dark scheme and a light.
                    theme::RAIL(),
                );
            }
            let name = match core.number {
                Some(number) => format!("C{number}"),
                None => "core".into(),
            };
            painter.text(
                Pos2::new(rect.center().x, rect.bottom() - 1.0),
                Align2::CENTER_BOTTOM,
                name,
                FontId::new(8.5, fonts::medium()),
                theme::MUTED(),
            );
            // Built when the pointer is on the core, not for each core of
            // each frame.
            response.on_hover_ui(|ui| {
                let Some(detail) = topo.cpu_detail(core.cpus[0]) else { return };
                ui.set_max_width(470.0);
                let others: Vec<String> = core.cpus[1..].iter().map(|cpu| format!("CPU {cpu}")).collect();
                let shares = (!others.is_empty()).then(|| format!("shares this core with {}", others.join(", ")));
                paint_detail(ui, &detail, shares.as_deref().map(|text| (text, theme::MUTED())));
            });
        }
    });
}

/// The colours the lanes' chips have: the stronger kind in the accent, the
/// other and a core of no known kind muted. A machine has kinds or it has
/// none, so the two muted ones never stand side by side.
fn kind_fill(kind: u8) -> Color32 {
    match kind {
        core_kind::PERFORMANCE => theme::ACCENT(),
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
        assert_eq!((groups[0].cores[0].number, groups[1].cores[0].number), (Some(0), Some(1)), "the system calls the second core 8");

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

    #[test]
    fn the_lanes_are_worked_out_when_a_machine_is_said() {
        let mut machines = Machines::default();
        assert!(machines.is_empty() && machines.lane(0).is_none() && machines.header_line().is_empty());
        // Two cores of two threads on two nodes, the threads of a core
        // numbered together; and a core with one thread after them.
        let mut topo = CpuTopology {
            sockets: 1,
            smt: Some(true),
            hostname: "box".into(),
            cpus: vec![cpu(0, 0, 0, 0, 0, 0, 0), cpu(1, 0, 0, 0, 1, 0, 0), cpu(2, 0, 0, 1, 0, 0, 0), cpu(3, 0, 0, 1, 1, 0, 0), cpu(4, 0, 0, 2, 0, 0, 0)],
            ..CpuTopology::default()
        };
        for (at, node) in [0u16, 0, 1, 1, 1].into_iter().enumerate() {
            topo.cpus[at].numa = Some(node);
            topo.cpus[at].max_freq_khz = 4_600_000;
        }
        let before = machines.generation();
        machines.adopt(topo.clone());
        assert_ne!(machines.generation(), before);
        let lane = |cpu| machines.lane(cpu).unwrap();
        let name = |cpu| (lane(cpu).core.as_deref(), lane(cpu).thread.as_deref());
        assert_eq!(name(0), (Some("C0"), Some("T0")));
        assert_eq!(name(3), (Some("C1"), Some("T1")));
        assert_eq!(name(4), (Some("C2"), None));
        assert_eq!(lane(0).freq.as_deref(), Some("4.6"));
        assert_eq!([0, 1, 2, 3, 4].map(|cpu| lane(cpu).bracket), [Bracket::First, Bracket::Last, Bracket::First, Bracket::Last, Bracket::None]);
        assert_eq!((machines.tinted_node(1), machines.tinted_node(2)), (Some(0), Some(1)));
        assert_eq!(machines.detail(4).unwrap().title, "CPU 4");
        assert!(machines.detail(9).is_none());
        // The same machine again takes its place; one node is no tint.
        for cpu in &mut topo.cpus {
            cpu.numa = Some(0);
        }
        machines.adopt(topo.clone());
        assert_eq!(machines.list().len(), 1);
        assert_eq!(machines.tinted_node(2), None);
        // Siblings numbered apart, as Linux numbers an AMD part: the label
        // says the pairing and no bracket joins lanes that are not adjacent.
        let apart = CpuTopology {
            sockets: 1,
            smt: Some(true),
            cpus: vec![cpu(0, 0, 0, 0, 0, 0, 0), cpu(1, 0, 0, 1, 0, 0, 0), cpu(2, 0, 0, 0, 1, 0, 0), cpu(3, 0, 0, 1, 1, 0, 0)],
            ..CpuTopology::default()
        };
        let mut machines = Machines::default();
        machines.adopt(apart);
        assert_eq!((machines.lane(2).unwrap().core.as_deref(), machines.lane(2).unwrap().thread.as_deref()), (Some("C0"), Some("T1")));
        assert!((0..4).all(|cpu| machines.lane(cpu).unwrap().bracket == Bracket::None));
        machines.clear();
        assert!(machines.is_empty() && machines.lane(0).is_none());
    }

    #[test]
    fn cores_numbered_alike_in_two_clusters_are_drawn_apart() {
        let mut little = cpu(0, 0, 0, 0, 0, core_kind::EFFICIENCY, 0);
        let mut big = cpu(1, 0, 0, 0, 0, core_kind::PERFORMANCE, 0);
        (little.cluster, big.cluster) = (Some(0), Some(1));
        let topo = CpuTopology { sockets: 1, cpus: vec![little, big], ..CpuTopology::default() };
        let cores = &drawn(&topo)[0].nodes[0].groups[0].cores;
        assert_eq!(cores.iter().map(|core| core.cpus.clone()).collect::<Vec<_>>(), [vec![0], vec![1]]);
        assert_eq!(cores.iter().map(|core| core.number).collect::<Vec<_>>(), [Some(0), Some(1)], "counted, not the id each cluster gives its first");
    }
}
