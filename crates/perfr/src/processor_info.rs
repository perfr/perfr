// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What Windows says of each core, for the viewer's scheduler lanes.
//!
//! `GetLogicalProcessorInformationEx(RelationAll)` fills a buffer with
//! records of varying length, one per core, cache, package, NUMA node and
//! processor group. Two of them tell cores apart: a core's record carries
//! its efficiency class (higher is faster; a machine of one kind of core has
//! one class), and a cache's record its level, its size and the processors
//! that share it. The parsing is here, apart from the call, so it is tested
//! against records written by hand on any machine.
//!
//! Windows has no top frequency per core to read the way Linux has
//! `cpuinfo_max_freq`: `CallNtPowerInformation` reports the nominal
//! frequency, which is not what a core reaches. The frequency is left
//! unknown.

use perfr_live_event::core_kind;
use perfr_live_event::topology::{CacheInstance, CpuTopology, LogicalCpu, NumaNode};

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_PROCESSOR_DIE: u32 = 5;
/// `Relationship` and `Size`, which lead every record.
const RECORD_HEADER: usize = 8;
/// A `GROUP_AFFINITY` on 64-bit Windows: the mask, the group, padding.
const GROUP_AFFINITY_LEN: usize = 16;

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The processors of `count` `GROUP_AFFINITY`s at `at`, as the numbers the
/// scheduler's lanes have: the bit in the group's mask, after the 64 of
/// each group before it. Groups are filled in order, so this is the
/// processor's index on every machine with full groups, which is every
/// machine of 64 processors or fewer.
fn processors(record: &[u8], at: usize, count: usize) -> Vec<u16> {
    let mut cpus = Vec::new();
    for i in 0..count {
        let Some(entry) = record.get(at + i * GROUP_AFFINITY_LEN..at + (i + 1) * GROUP_AFFINITY_LEN) else { break };
        let mask = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let group = u16::from_le_bytes(entry[8..10].try_into().unwrap());
        for bit in 0..64u16 {
            if mask & (1 << bit) != 0 {
                if let Some(cpu) = group.checked_mul(64).and_then(|base| base.checked_add(bit)) {
                    cpus.push(cpu);
                }
            }
        }
    }
    cpus
}

/// What a `GetLogicalProcessorInformationEx(RelationAll)` buffer tells of
/// the machine. The lane marks are [`CpuTopology::core_infos`] of it: the
/// kind from the efficiency classes when the machine has more than one, and
/// the size of the level 3 cache.
///
/// Frequencies are not in it. Die numbers are the order of the die records,
/// not a CCD id from the silicon. A record cut short ends the walk, and a
/// record too short for its own fields is passed over.
pub fn topology_from_processor_information(bytes: &[u8]) -> CpuTopology {
    struct Group {
        class: u8,
        cpus: Vec<u16>,
    }
    let mut core_groups: Vec<Group> = Vec::new();
    let mut packages: Vec<Vec<u16>> = Vec::new();
    let mut dies: Vec<Vec<u16>> = Vec::new();
    let mut caches: Vec<CacheInstance> = Vec::new();
    let mut numa: Vec<NumaNode> = Vec::new();
    let mut at = 0;
    while let (Some(relationship), Some(size)) = (u32_at(bytes, at), u32_at(bytes, at + 4)) {
        let size = size as usize;
        let Some(record) = bytes.get(at..at.saturating_add(size)).filter(|_| size >= RECORD_HEADER) else { break };
        match relationship {
            // PROCESSOR_RELATIONSHIP: Flags, EfficiencyClass, 20 reserved
            // bytes, GroupCount, GroupMask[].
            RELATION_PROCESSOR_CORE | RELATION_PROCESSOR_PACKAGE | RELATION_PROCESSOR_DIE => {
                if let (Some(&class), Some(count)) = (record.get(9), u16_at(record, 30)) {
                    let cpus = processors(record, 32, usize::from(count));
                    match relationship {
                        RELATION_PROCESSOR_PACKAGE => packages.push(cpus),
                        RELATION_PROCESSOR_DIE => dies.push(cpus),
                        _ => core_groups.push(Group { class, cpus }),
                    }
                }
            }
            // CACHE_RELATIONSHIP: Level, Associativity, LineSize, CacheSize,
            // Type, 18 reserved bytes, GroupCount, GroupMask[]. Before
            // Windows 10 20H2 the count's two bytes were reserved and zero,
            // with one mask after them.
            RELATION_CACHE => {
                if let (Some(&level), Some(&ways), Some(line), Some(cache_bytes), Some(kind), Some(count)) =
                    (record.get(8), record.get(9), u16_at(record, 10), u32_at(record, 12), u32_at(record, 16), u16_at(record, 38))
                {
                    let kind = match kind {
                        1 => "instruction",
                        2 => "data",
                        _ => "unified",
                    };
                    if level > 0 {
                        caches.push(CacheInstance {
                            level,
                            kind: kind.into(),
                            size_kb: cache_bytes / 1024,
                            ways: u16::from(ways),
                            line,
                            cpus: processors(record, 40, usize::from(count.max(1))),
                        });
                    }
                }
            }
            RELATION_NUMA_NODE => {
                if let Some(id) = u32_at(record, 8) {
                    let count = u16_at(record, 30).unwrap_or(1);
                    let node_id = u16::try_from(id).unwrap_or(u16::MAX);
                    numa.push(NumaNode {
                        id: node_id,
                        cpus: processors(record, 32, usize::from(count.max(1))),
                        mem_bytes: 0,
                    });
                }
            }
            _ => {}
        }
        at += size;
    }
    let top_class = core_groups.iter().map(|g| g.class).max().unwrap_or(0);
    let hybrid = core_groups.iter().any(|g| g.class != top_class);
    let mut cpus = Vec::new();
    for (index, group) in core_groups.iter().enumerate() {
        let socket = packages.iter().position(|p| group.cpus.iter().any(|cpu| p.contains(cpu)));
        let die = dies.iter().position(|d| group.cpus.iter().any(|cpu| d.contains(cpu)));
        let core_id = core_groups
            .iter()
            .take(index)
            .filter(|other| {
                let other_socket = packages.iter().position(|p| other.cpus.iter().any(|cpu| p.contains(cpu)));
                other_socket == socket
            })
            .count() as u16;
        let kind = if hybrid && group.class == top_class {
            core_kind::PERFORMANCE
        } else if hybrid {
            core_kind::EFFICIENCY
        } else {
            core_kind::UNKNOWN
        };
        for (thread, cpu) in group.cpus.iter().enumerate() {
            let mut logical = LogicalCpu {
                cpu: *cpu,
                kind,
                socket: socket.map(|s| s as u16),
                die: die.map(|d| d as u16),
                core_id: Some(core_id),
                thread: Some(thread as u16),
                ..LogicalCpu::default()
            };
            for cache in &caches {
                if !cache.cpus.contains(cpu) {
                    continue;
                }
                match (cache.level, cache.kind.as_str()) {
                    (1, "instruction") => logical.l1i_kb = cache.size_kb,
                    (1, "data") => logical.l1d_kb = cache.size_kb,
                    (2, _) => logical.l2_kb = cache.size_kb,
                    (3, _) => logical.l3_kb = cache.size_kb,
                    _ => {}
                }
            }
            cpus.push(logical);
        }
    }
    for node in &numa {
        for cpu in &node.cpus {
            if let Some(logical) = cpus.iter_mut().find(|c| c.cpu == *cpu) {
                logical.numa = Some(node.id);
            }
        }
    }
    cpus.sort_by_key(|c| c.cpu);
    let smt = if core_groups.is_empty() {
        None
    } else {
        Some(core_groups.iter().any(|g| g.cpus.len() > 1))
    };
    let line = caches.iter().find(|c| c.level == 1 && c.line > 0).or_else(|| caches.iter().find(|c| c.line > 0)).map(|c| c.line).unwrap_or(0);
    CpuTopology {
        sockets: packages.len() as u16,
        physical_cores: core_groups.len() as u16,
        threads: cpus.len() as u16,
        smt,
        cache_line_bytes: line,
        numa,
        cpus,
        caches,
        ..CpuTopology::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_live_event::CoreInfo;

    /// The lane marks of a buffer.
    fn cores_from_processor_information(bytes: &[u8]) -> Vec<CoreInfo> {
        topology_from_processor_information(bytes).core_infos()
    }

    fn group_affinity(mask: u64, group: u16) -> Vec<u8> {
        let mut bytes = mask.to_le_bytes().to_vec();
        bytes.extend_from_slice(&group.to_le_bytes());
        bytes.extend_from_slice(&[0; 6]);
        bytes
    }

    fn record(relationship: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = relationship.to_le_bytes().to_vec();
        bytes.extend_from_slice(&((RECORD_HEADER + body.len()) as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    /// A core of efficiency class `class` whose logical processors are
    /// `mask` of `group`.
    fn core(class: u8, mask: u64, group: u16) -> Vec<u8> {
        let mut body = vec![1, class];
        body.extend_from_slice(&[0; 20]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&group_affinity(mask, group));
        record(RELATION_PROCESSOR_CORE, &body)
    }

    /// A cache of `level` and `size` bytes shared by `mask` of group 0, with
    /// the group count as `count` (zero is the layout before 20H2).
    fn cache(level: u8, size: u32, mask: u64, count: u16) -> Vec<u8> {
        let mut body = vec![level, 8];
        body.extend_from_slice(&64u16.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0; 18]);
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&group_affinity(mask, 0));
        record(RELATION_CACHE, &body)
    }

    #[test]
    fn a_hybrid_machine_has_its_classes_named_and_its_cache_sized() {
        // A performance core of two threads, two efficiency cores, a level
        // 2 cache each side and one level 3 under all four; a package
        // record (relationship 3) between them is passed over.
        let mut buffer = Vec::new();
        buffer.extend(core(1, 0b0011, 0));
        buffer.extend(cache(2, 2 << 20, 0b0011, 1));
        buffer.extend(record(3, &[0; 40]));
        buffer.extend(core(0, 0b0100, 0));
        buffer.extend(core(0, 0b1000, 0));
        buffer.extend(cache(2, 4 << 20, 0b1100, 1));
        buffer.extend(cache(3, 36 << 20, 0b1111, 1));
        assert_eq!(
            cores_from_processor_information(&buffer),
            [
                CoreInfo { cpu: 0, kind: core_kind::PERFORMANCE, max_freq_khz: 0, l3_kb: 36_864 },
                CoreInfo { cpu: 1, kind: core_kind::PERFORMANCE, max_freq_khz: 0, l3_kb: 36_864 },
                CoreInfo { cpu: 2, kind: core_kind::EFFICIENCY, max_freq_khz: 0, l3_kb: 36_864 },
                CoreInfo { cpu: 3, kind: core_kind::EFFICIENCY, max_freq_khz: 0, l3_kb: 36_864 },
            ]
        );
    }

    #[test]
    fn a_machine_of_one_class_names_no_kind_and_keeps_the_caches_that_differ() {
        // An X3D Ryzen: one class, the larger cache under the first core.
        // The caches are in the layout older Windows writes, with no count.
        let mut buffer = Vec::new();
        buffer.extend(core(0, 0b01, 0));
        buffer.extend(core(0, 0b10, 0));
        buffer.extend(cache(3, 96 << 20, 0b01, 0));
        buffer.extend(cache(3, 32 << 20, 0b10, 0));
        assert_eq!(
            cores_from_processor_information(&buffer),
            [
                CoreInfo { cpu: 0, kind: core_kind::UNKNOWN, max_freq_khz: 0, l3_kb: 98_304 },
                CoreInfo { cpu: 1, kind: core_kind::UNKNOWN, max_freq_khz: 0, l3_kb: 32_768 },
            ]
        );
    }

    #[test]
    fn a_second_processor_group_is_numbered_after_the_first() {
        let mut buffer = Vec::new();
        buffer.extend(core(1, 1 << 63, 0));
        buffer.extend(core(0, 0b1, 1));
        let cores = cores_from_processor_information(&buffer);
        assert_eq!(cores.iter().map(|c| (c.cpu, c.kind)).collect::<Vec<_>>(), [(63, core_kind::PERFORMANCE), (64, core_kind::EFFICIENCY)]);
    }

    #[test]
    fn a_buffer_that_tells_nothing_or_is_cut_short_is_read_without_harm() {
        assert!(cores_from_processor_information(&[]).is_empty());
        // Cores of one class and no cache: nothing to mark a lane with.
        let mut plain = core(0, 0b01, 0);
        plain.extend(core(0, 0b10, 0));
        assert!(cores_from_processor_information(&plain).is_empty());
        // Cut anywhere, a hybrid buffer gives what its whole records say
        // and never panics; a record claiming less than its header ends it.
        let mut buffer = core(1, 0b01, 0);
        buffer.extend(core(0, 0b10, 0));
        buffer.extend(cache(3, 36 << 20, 0b11, 1));
        for cut in 0..buffer.len() {
            let _ = cores_from_processor_information(&buffer[..cut]);
        }
        let mut bad = buffer.clone();
        bad[4..8].copy_from_slice(&3u32.to_le_bytes());
        assert!(cores_from_processor_information(&bad).is_empty());
        let mut huge = buffer;
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(cores_from_processor_information(&huge).is_empty());
    }

    fn group(relationship: u32, mask: u64) -> Vec<u8> {
        let mut body = vec![0, 0];
        body.extend_from_slice(&[0; 20]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&group_affinity(mask, 0));
        record(relationship, &body)
    }

    fn numa(id: u32, mask: u64) -> Vec<u8> {
        let mut body = id.to_le_bytes().to_vec();
        body.extend_from_slice(&[0; 18]);
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&group_affinity(mask, 0));
        record(1, &body)
    }

    #[test]
    fn the_buffer_names_smt_siblings_caches_nodes_and_the_line_size() {
        // One package, one die, a performance core of two threads, an
        // efficiency core of one, an instruction cache on the first thread,
        // a shared level 3, and one NUMA node over all three threads.
        let mut buffer = Vec::new();
        buffer.extend(group(3, 0b0111));
        buffer.extend(group(5, 0b0111));
        buffer.extend(core(1, 0b0011, 0));
        buffer.extend(core(0, 0b0100, 0));
        buffer.extend(cache(1, 32 << 10, 0b0001, 1));
        // The level 1 record above is unified (type 0) in the helper. The
        // instruction one is written out so its type is 1.
        let mut l1 = cache(1, 32 << 10, 0b0010, 1);
        // Type sits at record offset 16.
        l1[16..20].copy_from_slice(&1u32.to_le_bytes());
        buffer.extend(l1);
        buffer.extend(cache(3, 36 << 20, 0b0111, 1));
        buffer.extend(numa(0, 0b0111));
        let topo = topology_from_processor_information(&buffer);
        assert_eq!(topo.sockets, 1);
        assert_eq!(topo.physical_cores, 2);
        assert_eq!(topo.threads, 3);
        assert_eq!(topo.smt, Some(true));
        assert_eq!(topo.cache_line_bytes, 64);
        assert_eq!(topo.cpus.len(), 3);
        assert_eq!(topo.cpus[0].kind, core_kind::PERFORMANCE);
        assert_eq!(topo.cpus[0].core_id, Some(0));
        assert_eq!(topo.cpus[0].thread, Some(0));
        assert_eq!(topo.cpus[1].thread, Some(1));
        assert_eq!(topo.cpus[1].core_id, Some(0));
        assert_eq!(topo.cpus[2].kind, core_kind::EFFICIENCY);
        assert_eq!(topo.cpus[2].core_id, Some(1));
        assert_eq!(topo.cpus[2].thread, Some(0));
        assert!(topo.cpus.iter().all(|c| c.socket == Some(0) && c.die == Some(0) && c.numa == Some(0)));
        assert_eq!(topo.cpus[0].l3_kb, 36_864);
        assert_eq!(topo.numa[0].cpus, vec![0, 1, 2]);
        assert_eq!(topo.lane_label(0).as_deref(), Some("C0 · T0"));
        assert_eq!(topo.lane_label(2).as_deref(), Some("C1"));
        let marks = cores_from_processor_information(&buffer);
        assert_eq!(marks.len(), 3);
        assert_eq!(marks[0].kind, topo.cpus[0].kind);
        assert_eq!(marks[0].l3_kb, topo.cpus[0].l3_kb);
        assert!(topology_from_processor_information(&[]).is_empty());
        // Cut anywhere, the buffer gives what its whole records say and
        // never a cache with no size or a panic.
        for cut in 0..buffer.len() {
            let cut = topology_from_processor_information(&buffer[..cut]);
            assert!(cut.caches.iter().all(|cache| cache.size_kb > 0 && !cache.cpus.is_empty()));
        }
    }
}
