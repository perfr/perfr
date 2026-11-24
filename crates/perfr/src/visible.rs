// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Which processes get rows in the viewer.
//!
//! Scheduling is traced machine-wide, because what a core was doing includes
//! the processes competing with the target. That is the right data and the
//! wrong default view: projecting every slice onto its thread spawns a
//! process row and a thread row for every process on the machine, and the
//! target drowns in a few hundred rows of things nobody asked about.
//!
//! So the trace stays system-wide and the *projection* is filtered. Core lanes
//! are capture-global -- a `SCHEDULING_SLICE` lanes by its core, never by its
//! pid -- so the Scheduler track keeps showing everything either way. What
//! this decides is only which processes grow thread rows of their own:
//!
//! - the targets (a capture can be started on several processes), and
//!   anything they spawned, because work routinely lives in a child process
//!   and filtering to the exact pid once meant showing nothing;
//! - any process carrying instrumentation, dynamic or manual, since a span
//!   somebody asked for is by definition of interest;
//! - the service itself.
//!
//! Never the whole machine: on a 72-core box that was hundreds of rows and
//! the thread-state work to fill them, for processes nobody asked about.
//! The scheduler track already shows every core's occupant. A capture may
//! also have no target at all -- then the rows are the service and whatever
//! is instrumenting itself.
//!
//! The viewer's own lanes are untouched: self-profiling and GPU events use
//! sentinel pids and never come through here.

use std::collections::HashSet;
#[cfg(target_os = "macos")]
use crate::macos::read_parent_map;
#[cfg(target_os = "windows")]
use crate::windows::read_parent_map;

/// How often the descendant set is rebuilt from `/proc` during a capture.
/// Children appear at any time; a second is well under human patience and
/// costs one directory walk.
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub struct VisibleProcesses {
    /// The processes the capture was started on. Empty when it has none.
    targets: Vec<u32>,
    /// The targets and everything they spawned: what is sampled. It only
    /// ever grows, like `allowed`, which also holds what instruments itself.
    family: HashSet<u32>,
    allowed: HashSet<u32>,
    last_refresh: std::time::Instant,
    /// A rebuild of the descendant set that is under way on another thread.
    refreshing: Option<std::sync::mpsc::Receiver<HashSet<u32>>>,
}

impl VisibleProcesses {
    /// `_all` is the capture's "every process" request, kept for the wire
    /// format; it no longer widens the rows (see the module doc).
    pub fn new(targets: &[u32], _all: bool) -> VisibleProcesses {
        let mut visible = VisibleProcesses {
            targets: targets.iter().copied().filter(|pid| *pid != 0).collect(),
            family: HashSet::new(),
            allowed: HashSet::new(),
            last_refresh: std::time::Instant::now(),
            refreshing: None,
        };
        visible.refresh_now();
        visible
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn len(&self) -> usize {
        self.allowed.len()
    }

    /// Adds a process because something is instrumenting it. Permanent for
    /// the rest of the capture: a hooked process stays interesting even
    /// during the stretches where it is idle.
    pub fn add_instrumented(&mut self, pid: u32) {
        self.allowed.insert(pid);
    }

    #[cfg(any(test, target_os = "linux", target_os = "windows"))]
    pub fn contains(&self, pid: u32) -> bool {
        self.allowed.contains(&pid)
    }

    /// Whether `pid` is a target or was spawned by one.
    #[cfg(test)]
    pub fn in_family(&self, pid: u32) -> bool {
        self.family.contains(&pid)
    }

    /// The targets and what they spawned, in pid order.
    #[cfg(any(test, target_os = "linux"))]
    pub fn family(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self.family.iter().copied().collect();
        pids.sort_unstable();
        pids
    }

    /// Adds a process to the family ahead of the next walk: one whose birth
    /// the capture was told of as it happened.
    #[cfg(any(test, target_os = "linux"))]
    pub fn add_child(&mut self, pid: u32) {
        self.family.insert(pid);
        self.allowed.insert(pid);
    }

    /// The processes of interest -- the targets, their descendants, and anything
    /// instrumented. Both the rows and the set whose thread states are traced.
    pub fn pids(&self) -> Vec<u32> {
        let mut pids: Vec<u32> = self.allowed.iter().copied().collect();
        pids.sort_unstable();
        pids
    }

    /// Rebuilds the descendant set if enough time has passed. Cheap to call
    /// every loop iteration.
    ///
    /// The rebuild reads the parent of every process on the machine, which
    /// is milliseconds for a few hundred of them and tens for a few
    /// thousand, and the caller is the capture thread: the one that has to
    /// be back at the kernel's rings every few milliseconds. So the walk
    /// runs on a thread of its own and this takes its result on a later
    /// pass; a new child gets its row that much later, well inside the
    /// second it could already wait.
    pub fn maybe_refresh(&mut self) {
        if let Some(result) = &self.refreshing {
            match result.try_recv() {
                Ok(descendants) => {
                    self.refreshing = None;
                    self.allow(descendants);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                // The walk died without an answer; the next one is due on
                // the usual schedule.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.refreshing = None,
            }
        }
        if self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.last_refresh = std::time::Instant::now();
        if self.targets.is_empty() {
            return;
        }
        let targets = self.targets.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let walk = std::thread::Builder::new().name("perfr-visible".to_string()).spawn(move || {
            let _ = sender.send(descendants_of_all(&targets, &read_parent_map()));
        });
        match walk {
            Ok(_) => self.refreshing = Some(receiver),
            // No thread to be had: do it here, as it used to be done.
            Err(_) => self.refresh_now(),
        }
    }

    fn refresh_now(&mut self) {
        self.last_refresh = std::time::Instant::now();
        if self.targets.is_empty() {
            return;
        }
        let descendants = descendants_of_all(&self.targets, &read_parent_map());
        self.allow(descendants);
    }

    /// Instrumented pids added by hand must survive a refresh; only the
    /// descendant half is recomputed, and it only ever grows.
    fn allow(&mut self, descendants: HashSet<u32>) {
        self.family.extend(descendants);
        self.family.extend(self.targets.iter().copied());
        self.allowed.extend(self.family.iter().copied());
    }
}

/// Every process on the machine, mapped to its parent.
#[cfg(target_os = "linux")]
fn read_parent_map() -> Vec<(u32, u32)> {
    let mut pairs = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else { return pairs };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        if let Some(parent) = parent_from_stat(&stat) {
            pairs.push((pid, parent));
        }
    }
    pairs
}

/// The `ppid` field of `/proc/<pid>/stat`, which is the fourth.
///
/// Parsing starts after the last `)` rather than splitting on whitespace from
/// the front: the second field is the executable name in parentheses, and it
/// can contain both spaces and parentheses.
#[cfg(any(test, target_os = "linux"))]
pub fn parent_from_stat(stat: &str) -> Option<u32> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Everything reachable downwards from `root`, root excluded.
pub fn descendants_of(root: u32, parents: &[(u32, u32)]) -> HashSet<u32> {
    let mut found = HashSet::new();
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for (child, parent) in parents {
            // The insert guards against a cycle: /proc is read one file at a
            // time and can hand back a parent that has since been reparented.
            if *parent == pid && *child != root && found.insert(*child) {
                frontier.push(*child);
            }
        }
    }
    found
}

/// Everything reachable downwards from any of `roots`. A root that another
/// root spawned is in the result; one that nothing here spawned is not.
fn descendants_of_all(roots: &[u32], parents: &[(u32, u32)]) -> HashSet<u32> {
    let mut found = HashSet::new();
    for root in roots {
        found.extend(descendants_of(*root, parents));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_is_read_past_a_comm_full_of_punctuation() {
        // The real hazard: a process named with spaces and parentheses.
        let stat = "1234 (weird (name) here) S 42 1234 1234 0 -1 4194304";
        assert_eq!(parent_from_stat(stat), Some(42));
        assert_eq!(parent_from_stat("7 (sh) S 1 7 7 0"), Some(1));
    }

    #[test]
    fn descendants_are_found_through_generations() {
        let parents = vec![(2, 1), (3, 2), (4, 3), (9, 8)];
        let found = descendants_of(1, &parents);
        assert_eq!(found, HashSet::from([2, 3, 4]), "grandchildren count too");
        assert!(!found.contains(&9), "an unrelated tree stays out");
    }

    #[test]
    fn several_targets_each_bring_what_they_spawned() {
        // 1 spawned 2, 5 spawned 6 and 7, and 9 is nobody's.
        let parents = vec![(2, 1), (6, 5), (7, 6), (9, 8)];
        assert_eq!(descendants_of_all(&[1, 5], &parents), HashSet::from([2, 6, 7]));
        // A target another target spawned is found as its child too.
        assert_eq!(descendants_of_all(&[5, 6], &parents), HashSet::from([6, 7]));
        assert!(descendants_of_all(&[], &parents).is_empty());
    }

    #[test]
    fn the_family_is_the_targets_and_their_children_and_not_what_instruments_itself() {
        let me = std::process::id();
        let mut visible = VisibleProcesses::new(&[me, 0], false);
        assert!(visible.in_family(me));
        assert!(!visible.in_family(0), "no pid is not a target");
        visible.add_instrumented(4_242);
        assert!(visible.contains(4_242) && !visible.in_family(4_242));
        // A child the capture heard of before the walk found it.
        visible.add_child(4_243);
        assert!(visible.contains(4_243) && visible.in_family(4_243));
        assert!(visible.family().contains(&me) && visible.family().contains(&4_243));
    }

    #[test]
    fn a_parent_cycle_does_not_hang() {
        // Not physically possible, but /proc is read incrementally and can
        // report a snapshot no single instant ever had.
        let parents = vec![(2, 3), (3, 2), (2, 1)];
        let found = descendants_of(1, &parents);
        assert!(found.contains(&2));
    }

    #[test]
    fn asking_for_everything_does_not_widen_the_rows() {
        // The request is honoured for the scheduler track, which is always
        // machine-wide; rows stay with the target set.
        let visible = VisibleProcesses::new(&[1], true);
        assert!(visible.contains(1));
        assert!(!visible.contains(999_999));
    }

    #[test]
    fn a_capture_without_a_target_shows_only_what_instruments_itself() {
        let mut visible = VisibleProcesses::new(&[], false);
        assert!(visible.pids().is_empty());
        assert!(!visible.contains(1));
        visible.add_instrumented(4_242);
        assert!(visible.contains(4_242));
        assert_eq!(visible.pids(), vec![4_242]);
    }

    #[test]
    fn an_instrumented_process_stays_visible() {
        let mut visible = VisibleProcesses::new(&[1], false);
        assert!(!visible.contains(4_242));
        visible.add_instrumented(4_242);
        assert!(visible.contains(4_242));
        // And survives a rebuild of the descendant half.
        visible.last_refresh = std::time::Instant::now() - REFRESH_INTERVAL * 2;
        visible.maybe_refresh();
        assert!(visible.contains(4_242), "hand-added pids are not recomputed away");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_started_during_the_capture_becomes_visible_within_a_refresh() {
        let me = std::process::id();
        let mut visible = VisibleProcesses::new(&[me], false);
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!visible.contains(child.id()), "it did not exist when the set was built");
        // The capture loop calls this every pass; a refresh comes due once a
        // second. A few passes after one is due, the child is there.
        visible.last_refresh = std::time::Instant::now() - REFRESH_INTERVAL * 2;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !visible.contains(child.id()) && std::time::Instant::now() < deadline {
            visible.maybe_refresh();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let seen = visible.contains(child.id()) && visible.in_family(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(seen, "a process the target spawned never got its row");
    }

    /// What a pass pays when a refresh is due:
    /// `cargo test --release -p perfr -- --ignored --nocapture cost_of_a_refresh`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "a measurement, not a check"]
    fn cost_of_a_refresh_that_is_due() {
        let mut visible = VisibleProcesses::new(&[std::process::id()], false);
        let processes = read_parent_map().len();
        for _ in 0..5 {
            visible.last_refresh = std::time::Instant::now() - REFRESH_INTERVAL * 2;
            let start = std::time::Instant::now();
            visible.maybe_refresh();
            eprintln!("maybe_refresh with {processes} processes on the machine: {:?} on the calling thread", start.elapsed());
            std::thread::sleep(std::time::Duration::from_millis(50));
            visible.maybe_refresh();
        }
    }

    #[test]
    fn this_process_sees_itself_and_not_the_whole_machine() {
        let me = std::process::id();
        let visible = VisibleProcesses::new(&[me], false);
        assert!(visible.contains(me));
        // pid 1 is not a descendant of a test binary.
        assert!(!visible.contains(1), "the machine should not be visible by default");
    }
}
