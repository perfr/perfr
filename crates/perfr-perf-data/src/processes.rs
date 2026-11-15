// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Who was running and what they had mapped, as the events of a recording
//! tell it: the address spaces a sampled address is looked up in, and the
//! names of the threads.

use std::collections::HashMap;

use crate::reader::Frame;
use crate::session::{Event, Mmap};

/// A file mapped into a process, or a piece of the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapping {
    pub start: u64,
    pub end: u64,
    /// Offset in the file of the mapping's first byte.
    pub pgoff: u64,
    pub path: String,
    pub build_id: Vec<u8>,
    /// A piece of the kernel: its text, or a module.
    pub kernel: bool,
}

impl Mapping {
    /// The kernel's own text, as against one of its modules.
    pub fn is_kernel_text(&self) -> bool {
        self.kernel && self.path.starts_with("[kernel.kallsyms]")
    }

    /// The name of the kernel module this mapping is, as the kernel
    /// spells it (`nvidia_uvm`). `perf` records a module by that name in
    /// brackets, or by the path of its file when it found one
    /// (`.../nvidia-uvm.ko.zst`): the file's name with the dashes the
    /// kernel turns into underscores.
    pub fn kernel_module(&self) -> Option<String> {
        if !self.kernel || self.is_kernel_text() {
            return None;
        }
        let file = self.path.rsplit('/').next().unwrap_or(&self.path);
        let name = file.trim_matches(['[', ']']);
        let name = name.find(".ko").map_or(name, |at| &name[..at]);
        Some(name.replace('-', "_"))
    }

    /// The name a frame in this mapping is filed under: the file's, or
    /// `[kernel]`, `[vdso]`, `[module]`.
    pub fn module(&self) -> String {
        if self.is_kernel_text() {
            return "[kernel]".into();
        }
        if let Some(module) = self.kernel_module() {
            return format!("[{module}]");
        }
        self.path
            .rsplit('/')
            .next()
            .unwrap_or(&self.path)
            .to_string()
    }

    /// Whether there is a file of a process's behind the mapping.
    /// `[vdso]`, `[heap]`, `//anon` and the kernel's pieces have none.
    pub fn has_file(&self) -> bool {
        !self.kernel && self.path.starts_with('/') && !self.path.starts_with("//")
    }

    /// Where `address` is in the mapped file.
    pub fn file_offset(&self, address: u64) -> u64 {
        address.wrapping_sub(self.start).wrapping_add(self.pgoff)
    }

    /// A frame nobody could name: the module and where in it, which is
    /// what a symbol table would be asked. Memory with no file behind it
    /// (code a JIT wrote) is named by its address alone.
    pub fn label(&self, address: u64) -> String {
        if self.kernel {
            // The kernel's text and its modules are placed afresh at
            // every boot: the offset from the start of each is what
            // holds from one boot to the next.
            return format!("{}+{:#x}", self.module(), address.wrapping_sub(self.start));
        }
        if self.path.starts_with("//") || self.path.is_empty() {
            return format!("{address:#x}");
        }
        format!("{}+{:#x}", self.module(), self.file_offset(address))
    }
}

/// The label of a frame in no known mapping.
pub fn unmapped_label(address: u64) -> String {
    format!("{address:#x}")
}

#[derive(Default)]
pub struct Processes {
    /// Per process, sorted by start and not overlapping.
    maps: HashMap<u32, Vec<Mapping>>,
    kernel: Vec<Mapping>,
    comms: HashMap<u32, String>,
    pids: HashMap<u32, u32>,
}

impl Processes {
    /// Takes in a mapping, a name, a fork. Samples and switches change
    /// nothing here.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Mmap(m) => self.map(m),
            Event::Comm {
                pid,
                tid,
                name,
                exec,
            } => {
                self.pids.insert(*tid, *pid);
                self.comms.insert(*tid, name.clone());
                if *exec {
                    // A new program: nothing of the old one is mapped.
                    self.maps.remove(pid);
                }
            }
            Event::Fork {
                pid,
                ppid,
                tid,
                ptid,
            } => {
                self.pids.insert(*tid, *pid);
                if let Some(name) = self.comms.get(ptid).cloned() {
                    self.comms.entry(*tid).or_insert(name);
                }
                if pid != ppid && !self.maps.contains_key(pid) {
                    if let Some(parent) = self.maps.get(ppid).cloned() {
                        self.maps.insert(*pid, parent);
                    }
                }
            }
            Event::Exit { .. } | Event::Sample(_) | Event::Switch { .. } => {}
        }
    }

    fn map(&mut self, m: &Mmap) {
        let new = Mapping {
            start: m.start,
            end: m.start.saturating_add(m.len),
            pgoff: m.pgoff,
            path: m.path.clone(),
            build_id: m.build_id.clone(),
            kernel: m.kernel,
        };
        if new.end <= new.start {
            return;
        }
        let maps = if m.kernel {
            &mut self.kernel
        } else {
            self.maps.entry(m.pid).or_default()
        };
        // What the new mapping covers is gone; what sticks out on either
        // side of it stays. Unmaps are not recorded, so this is the only
        // way an old mapping goes.
        let mut kept: Vec<Mapping> = Vec::with_capacity(maps.len() + 2);
        for old in maps.drain(..) {
            if old.end <= new.start || old.start >= new.end {
                kept.push(old);
                continue;
            }
            if old.start < new.start {
                kept.push(Mapping {
                    end: new.start,
                    ..old.clone()
                });
            }
            if old.end > new.end {
                kept.push(Mapping {
                    start: new.end,
                    pgoff: old.pgoff.wrapping_add(new.end - old.start),
                    ..old
                });
            }
        }
        kept.push(new);
        kept.sort_unstable_by_key(|m| m.start);
        *maps = kept;
    }

    /// The mapping a frame of process `pid` is in.
    pub fn mapping(&self, pid: u32, frame: Frame) -> Option<&Mapping> {
        let maps = if frame.kernel {
            &self.kernel
        } else {
            self.maps.get(&pid)?
        };
        let at = maps
            .partition_point(|m| m.start <= frame.address)
            .checked_sub(1)?;
        maps.get(at).filter(|m| frame.address < m.end)
    }

    /// The label of a frame without symbols: see [`Mapping::label`].
    pub fn label(&self, pid: u32, frame: Frame) -> String {
        self.mapping(pid, frame)
            .map_or_else(|| unmapped_label(frame.address), |m| m.label(frame.address))
    }

    pub fn mappings(&self, pid: u32) -> &[Mapping] {
        self.maps.get(&pid).map_or(&[], Vec::as_slice)
    }

    pub fn kernel_mappings(&self) -> &[Mapping] {
        &self.kernel
    }

    pub fn thread_name(&self, tid: u32) -> Option<&str> {
        self.comms.get(&tid).map(String::as_str)
    }

    /// A process is named after its first thread.
    pub fn process_name(&self, pid: u32) -> Option<&str> {
        self.thread_name(pid)
    }

    /// The process a thread was last seen in.
    pub fn pid_of(&self, tid: u32) -> Option<u32> {
        self.pids.get(&tid).copied()
    }
}
