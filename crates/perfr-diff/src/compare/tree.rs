// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The call tree of both sides folded into one, each node with what it
//! holds on either side: where in the program a change is, which a flat
//! list of functions cannot say.
//!
//! The samples are walked twice. The first walk counts every call path,
//! and the paths that are under the share a row needs on both sides are
//! dropped; the second fills the per-batch tables of the ones kept, which
//! is what their intervals are made from. Counting per batch for every
//! path would cost the memory of the capture several times over.

use super::*;
use crate::model::TreeNode;

const NONE: u32 = u32::MAX;

pub(super) fn build(
    inputs: [&[CaptureInput]; 2],
    summaries: [&[Summary]; 2],
    keys: &Keys,
    design: &Design<'_>,
) -> Option<TreeNode> {
    // Node 0 is the root; a node is its parent and its function.
    let mut index: HashMap<(u32, u32), u32> = HashMap::new();
    let mut nodes: Vec<(u32, u32)> = vec![(NONE, NONE)];
    let mut counts: [Vec<u64>; 2] = [vec![0], vec![0]];
    for side in 0..2 {
        for (input, summary) in inputs[side].iter().zip(summaries[side]) {
            for sample in &input.samples {
                let mut node = 0u32;
                counts[side][0] += 1;
                for frame in sample.frames.iter().rev().take(MAX_TREE_DEPTH) {
                    let Some(key) = summary.frame_keys.get(frame) else {
                        break;
                    };
                    let next = match index.get(&(node, *key)) {
                        Some(next) => *next,
                        None if nodes.len() >= MAX_TREE_NODES => break,
                        None => {
                            let next = nodes.len() as u32;
                            nodes.push((node, *key));
                            index.insert((node, *key), next);
                            counts[0].push(0);
                            counts[1].push(0);
                            next
                        }
                    };
                    counts[side][next as usize] += 1;
                    node = next;
                }
            }
        }
    }
    let totals = [counts[0][0].max(1) as f64, counts[1][0].max(1) as f64];
    if counts[0][0] == 0 || counts[1][0] == 0 {
        return None;
    }

    // A node under the share on both sides is dropped, and its children
    // with it: they hold no more than it does.
    let mut slot = vec![NONE; nodes.len()];
    let mut kept: Vec<u32> = Vec::new();
    for (node, slot) in slot.iter_mut().enumerate() {
        let (base, new) = (counts[0][node], counts[1][node]);
        let enough = base.max(new) >= MIN_SAMPLES
            && (base as f64 / totals[0]).max(new as f64 / totals[1]) >= design.options.min_share;
        if node == 0 || enough {
            *slot = kept.len() as u32;
            kept.push(node as u32);
        }
    }

    // Samples of each kept node per capture and time batch.
    let mut tables: [Vec<Vec<u32>>; 2] = [Vec::new(), Vec::new()];
    for side in 0..2 {
        for (input, summary) in inputs[side].iter().zip(summaries[side]) {
            let mut table = vec![0u32; kept.len() * TIME_BATCHES];
            for sample in &input.samples {
                let batch = summary.batch_of(sample.timestamp_ns);
                let mut node = 0u32;
                table[batch] += 1;
                for frame in sample.frames.iter().rev().take(MAX_TREE_DEPTH) {
                    let next = summary.frame_keys.get(frame).and_then(|key| index.get(&(node, *key)));
                    let Some(next) = next.filter(|next| slot[**next as usize] != NONE) else {
                        break;
                    };
                    table[slot[*next as usize] as usize * TIME_BATCHES + batch] += 1;
                    node = *next;
                }
            }
            tables[side].push(table);
        }
    }

    let share = design.options.per == Per::Sample;
    let mut metrics: Vec<Metric> = (0..kept.len())
        .map(|place| {
            let units = |side: usize| {
                design.units_of(summaries[side], |capture, summary, batch| {
                    let samples = tables[side][capture][place * TIME_BATCHES + batch];
                    (f64::from(samples) * summary.fn_scale, summary.norm_fn[batch])
                })
            };
            // All samples as a share of all samples is 1 on both sides.
            design.metric(&units(0), &units(1), place != 0 || !share)
        })
        .collect();
    design.judge(&mut metrics.iter_mut().collect::<Vec<_>>());

    let mut children: Vec<Vec<u32>> = vec![Vec::new(); kept.len()];
    for (place, node) in kept.iter().enumerate().skip(1) {
        let parent = slot[nodes[*node as usize].0 as usize];
        children[parent as usize].push(place as u32);
    }
    for list in &mut children {
        list.sort_by(|a, b| metrics[*b as usize].delta.abs().total_cmp(&metrics[*a as usize].delta.abs()));
    }
    Some(nest(0, &kept, &nodes, &counts, &metrics, &children, keys))
}

fn nest(
    place: u32,
    kept: &[u32],
    nodes: &[(u32, u32)],
    counts: &[Vec<u64>; 2],
    metrics: &[Metric],
    children: &[Vec<u32>],
    keys: &Keys,
) -> TreeNode {
    let node = kept[place as usize] as usize;
    let (name, module) = match nodes[node].1 {
        NONE => ("all samples".to_string(), String::new()),
        key => keys.functions.keys[key as usize].clone(),
    };
    TreeNode {
        name,
        module,
        samples: (counts[0][node], counts[1][node]),
        inclusive: metrics[place as usize],
        children: children[place as usize]
            .iter()
            .map(|child| nest(*child, kept, nodes, counts, metrics, children, keys))
            .collect(),
    }
}
