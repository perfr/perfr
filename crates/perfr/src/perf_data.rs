// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opening a `perf.data`, the file Linux `perf record` writes, as a
//! capture: its samples on the timeline and in the sampling report, with
//! the names `perf report` would give them.
//!
//! A `perf.data` holds no symbols. It has addresses, and for each address
//! the file it was in, where that file was mapped, and (from a `perf` of
//! the last few years) the file's build id. `perf report` reads the
//! symbols from the files on the machine it runs on, and so does this:
//!
//! - a frame in a file: the file at the recorded path, if it is the build
//!   that was recorded (its build id is compared when the recording has
//!   one; a recording without build ids is taken at its word). Its
//!   detached debug file is used as for a live capture.
//! - a frame in the kernel: `/proc/kallsyms`, if this is the kernel that
//!   was recorded (build id, or else release). The kernel's text moves at
//!   every boot; the recording says where it was, and the difference is
//!   applied. Modules move independently and are only named within the
//!   boot that was recorded.
//! - a frame in the vDSO: this kernel's, on the same condition.
//!
//! A frame that none of that names is `module+0xoffset`, as the viewer
//! shows a `perf.data` it opens with no service. Nothing is ever named
//! from a file of another build: a wrong name is worse than none.
//!
//! A recording made with `--call-graph dwarf` has no call chain for user
//! code. Each sample carries the registers and a copy of the top of the
//! stack, and the chain is unwound from them here with the unwind tables
//! of the same files, as the capture loop unwinds its own samples.
//!
//! The reading of the file itself is `perfr-perf-data`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use perfr_live_event::{extra, kind, thread_state, LiveEvent};
use perfr_live_server::LiveService;
use perfr_perf_data::{
    Event, Frame, Info, Mapping, Processes, Sample, Session, Span, Switches, UserStack,
};
use perfr_unwind::unwinder::StartRegs;
use perfr_unwind::ProcessUnwinder;

use crate::report::{FrameInfo, SampleStore, StoredSample};
use crate::symbolize::FileSymbols;

/// The width of a sampled frame when the recording does not say how far
/// apart its samples are (an event counted in cycles or misses, not time).
const UNKNOWN_PERIOD_NS: u64 = 1_000;
/// A frame's depth on the timeline is a byte; `perf` itself stops at 127.
const MAX_FRAMES: usize = 255;
/// The key of the kernel's frames in the name cache: no process has it.
const KERNEL: u32 = u32::MAX;
/// `PERF_SAMPLE_REGS_ABI_64`.
const REGS_ABI_64: u64 = 2;

/// A `perf.data` as a capture, before any of it is handed to the service.
pub(crate) struct Imported {
    pub events: Vec<LiveEvent>,
    pub samples: Vec<StoredSample>,
    pub frames: Vec<(u32, FrameInfo)>,
    pub threads: Vec<(u32, u32, String)>,
    pub processes: Vec<(u32, String)>,
    /// The process with the most samples.
    pub target_pid: u32,
    pub time_bounds: Option<(u64, u64)>,
    pub counts: Counts,
}

/// What became of the recording's frames and files, for the log and the
/// summary the import answers with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub samples: u64,
    /// Distinct frames, and how many of them got a function's name.
    pub frames: u64,
    pub named_frames: u64,
    /// Samples whose user stack was unwound from its copy.
    pub unwound: u64,
    /// Files that were read for symbols, that were not found or not
    /// readable, and that are there but are another build.
    pub files_read: u64,
    pub files_missing: u64,
    pub files_other_build: u64,
    pub kernel_symbols: bool,
    /// Records the kernel dropped while recording.
    pub lost: u64,
}

/// Where the names come from. The files and the kernel of the machine the
/// service runs on, or a test's own.
pub(crate) trait Host {
    fn file(&mut self, path: &str) -> Option<FileSymbols>;
    fn vdso(&mut self) -> Option<FileSymbols>;
    /// The release of the running kernel, as `uname -r` prints it.
    fn release(&mut self) -> Option<String>;
    /// The running kernel: its release, its build id, and its function
    /// symbols as `(address, name, module)` (`module` empty for the
    /// kernel proper; none at all where the addresses are hidden).
    fn kernel(&mut self) -> Option<RunningKernel>;
}

pub(crate) struct RunningKernel {
    pub release: String,
    pub build_id: Vec<u8>,
    pub symbols: Vec<(u64, String, String)>,
}

/// This machine.
pub(crate) struct ThisHost;

impl Host for ThisHost {
    fn file(&mut self, path: &str) -> Option<FileSymbols> {
        FileSymbols::load(path)
    }

    fn vdso(&mut self) -> Option<FileSymbols> {
        FileSymbols::vdso()
    }

    fn release(&mut self) -> Option<String> {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
        Some(release.trim().to_string())
    }

    fn kernel(&mut self) -> Option<RunningKernel> {
        let release = self.release()?;
        let build_id = std::fs::read("/sys/kernel/notes")
            .ok()
            .and_then(|notes| gnu_build_id(&notes))
            .unwrap_or_default();
        let symbols =
            parse_kallsyms(&std::fs::read_to_string("/proc/kallsyms").unwrap_or_default());
        Some(RunningKernel {
            release,
            build_id,
            symbols,
        })
    }
}

/// The `NT_GNU_BUILD_ID` descriptor among ELF notes laid end to end, as
/// `/sys/kernel/notes` has them: name size, descriptor size, type, then
/// the name and the descriptor, each padded to four bytes.
fn gnu_build_id(mut notes: &[u8]) -> Option<Vec<u8>> {
    const NT_GNU_BUILD_ID: u32 = 3;
    let word = |b: &[u8], at: usize| {
        b.get(at..at + 4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
    };
    loop {
        let (name_len, desc_len, kind) = (
            word(notes, 0)? as usize,
            word(notes, 4)? as usize,
            word(notes, 8)?,
        );
        let desc_at = 12usize.checked_add(name_len.checked_next_multiple_of(4)?)?;
        let end = desc_at.checked_add(desc_len.checked_next_multiple_of(4)?)?;
        if kind == NT_GNU_BUILD_ID && notes.get(12..12 + name_len) == Some(b"GNU\0") {
            return notes
                .get(desc_at..desc_at.checked_add(desc_len)?)
                .map(<[u8]>::to_vec);
        }
        notes = notes.get(end..)?;
    }
}

/// The function symbols of a `/proc/kallsyms`: `address type name`, and
/// `[module]` after a tab for a module's. Sorted by address. Empty when
/// the addresses are hidden (they all read as zero).
fn parse_kallsyms(text: &str) -> Vec<(u64, String, String)> {
    let mut symbols: Vec<(u64, String, String)> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let address = u64::from_str_radix(fields.next()?, 16).ok()?;
            let kind = fields.next()?;
            let name = fields.next()?;
            let module = fields
                .next()
                .map(|m| m.trim_matches(['[', ']']))
                .unwrap_or("");
            (address != 0 && matches!(kind, "t" | "T" | "w" | "W"))
                .then(|| (address, name.to_string(), module.to_string()))
        })
        .collect();
    symbols.sort_by_key(|(address, _, _)| *address);
    symbols
}

/// The running kernel's symbols, placed where the recording had them.
struct Kernel {
    symbols: Vec<(u64, String, String)>,
    /// Added to an address of the recording's kernel text, gives the
    /// address the same code has in this boot.
    slide: u64,
}

impl Kernel {
    /// `text` is the recording's mapping of the kernel's text, whose path
    /// ends in the symbol it was placed by (`[kernel.kallsyms]_text`) and
    /// whose offset is that symbol's address then.
    fn of(running: RunningKernel, text: &Mapping, info: &Info) -> Option<Kernel> {
        let recorded = if text.build_id.is_empty() {
            info.build_ids
                .get("[kernel.kallsyms]")
                .cloned()
                .unwrap_or_default()
        } else {
            text.build_id.clone()
        };
        let same = if !recorded.is_empty() && !running.build_id.is_empty() {
            recorded == running.build_id
        } else {
            !info.os_release.is_empty() && info.os_release == running.release
        };
        if !same {
            return None;
        }
        let anchor = text
            .path
            .rsplit(']')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("_text");
        let now = running
            .symbols
            .iter()
            .find(|(_, name, module)| name == anchor && module.is_empty())?
            .0;
        Some(Kernel {
            symbols: running.symbols,
            slide: now.wrapping_sub(text.pgoff),
        })
    }

    fn name_at(&self, mapping: &Mapping, address: u64) -> Option<String> {
        let (address, module) = match mapping.kernel_module() {
            None => (address.wrapping_add(self.slide), String::new()),
            // A module is placed afresh at every boot; an unmoved kernel
            // is the sign that this is the boot that was recorded.
            Some(module) if self.slide == 0 => (address, module),
            Some(_) => return None,
        };
        let at = self
            .symbols
            .partition_point(|(start, _, _)| *start <= address)
            .checked_sub(1)?;
        let (_, name, owner) = &self.symbols[at];
        (*owner == module).then(|| name.clone())
    }
}

/// A file of the recording, once looked for.
enum File {
    Symbols(FileSymbols),
    /// Not there, not readable, or another build: nothing is named from it.
    Unusable,
}

struct Importer<'a, H: Host> {
    host: &'a mut H,
    info: Info,
    processes: Processes,
    switches: Switches,
    files: HashMap<String, File>,
    vdso: Option<Option<FileSymbols>>,
    kernel: Option<Option<Kernel>>,
    /// Frame ids are per name: two addresses in one function share a row
    /// in the report and a box on the timeline.
    ids: HashMap<String, u32>,
    frames: Vec<(u32, FrameInfo)>,
    /// The next frame id, and the last of the range: the one every name
    /// that no longer fits shares.
    next_id: u32,
    last_id: u32,
    overflowed: bool,
    /// Address to frame id, per process; forgotten when the process maps
    /// something new.
    resolved: HashMap<u32, HashMap<u64, u32>>,
    /// Per process: its unwinder, and the `(path, bias)` already in it.
    unwinders: HashMap<u32, (ProcessUnwinder, HashSet<(String, u64)>)>,
    shown: HashSet<(u32, u32)>,
    per_pid: HashMap<u32, u64>,
    events: Vec<LiveEvent>,
    samples: Vec<StoredSample>,
    bounds: Option<(u64, u64)>,
    last_ns: u64,
    counts: Counts,
}

/// Reads `bytes`, a whole `perf.data`, naming its frames from `host`.
pub(crate) fn import<H: Host>(bytes: &[u8], host: &mut H) -> Result<Imported, String> {
    import_into(bytes, host, crate::serve::FRAME_NAME_ID_OTHER)
}

/// [`import`], with the frame ids ending at `last_id`.
fn import_into<H: Host>(bytes: &[u8], host: &mut H, last_id: u32) -> Result<Imported, String> {
    // In a file the features (which kernel, which builds) follow the data.
    let info = Info::read(bytes)?;
    let mut session = Session::new(true);
    let mut importer = Importer {
        host,
        info,
        processes: Processes::default(),
        switches: Switches::default(),
        files: HashMap::new(),
        vdso: None,
        kernel: None,
        ids: HashMap::new(),
        frames: Vec::new(),
        next_id: crate::serve::FRAME_NAME_ID_BASE,
        last_id,
        overflowed: false,
        resolved: HashMap::new(),
        unwinders: HashMap::new(),
        shown: HashSet::new(),
        per_pid: HashMap::new(),
        events: Vec::new(),
        samples: Vec::new(),
        bounds: None,
        last_ns: 0,
        counts: Counts::default(),
    };
    let mut at = 0;
    while !session.is_done() {
        match session.step(&bytes[at..], true)? {
            Some(used) => at += used,
            None => return Err("perf.data: the file ends in the middle of a table".into()),
        }
        if session.stats.untimed_samples > 0 {
            return Err("perf.data: its samples carry no time, so there is no timeline to put them on. Record with `perf record -T`.".into());
        }
        while let Some((time, event)) = session.next_event() {
            importer.on_event(time, event, &session);
        }
    }
    for span in importer.switches.finish(importer.last_ns) {
        importer.on_span(&span);
    }
    importer.counts.lost = session.stats.lost;
    Ok(importer.finish())
}

impl<H: Host> Importer<'_, H> {
    fn on_event(&mut self, time: u64, event: Event, session: &Session) {
        self.last_ns = self.last_ns.max(time);
        self.processes.apply(&event);
        match event {
            Event::Mmap(m) => {
                self.resolved.remove(&if m.kernel { KERNEL } else { m.pid });
            }
            Event::Comm { pid, exec, .. } => {
                if exec {
                    // A new program in the same process: none of what was
                    // mapped, named or loaded for the old one holds.
                    self.resolved.remove(&pid);
                    self.unwinders.remove(&pid);
                }
            }
            Event::Fork { pid, .. } => {
                self.resolved.remove(&pid);
            }
            Event::Exit { .. } => {}
            Event::Sample(sample) => {
                let attr = session.attrs().get(sample.attr);
                let period = attr
                    .and_then(|a| a.period_ns())
                    .unwrap_or(UNKNOWN_PERIOD_NS);
                let regs_mask = attr.map_or(0, |a| a.sample_regs_user);
                self.on_sample(time, sample, period, regs_mask);
            }
            Event::Switch {
                pid,
                tid,
                cpu,
                out,
                preempt,
            } => {
                if let Some(span) = self.switches.switch(time, pid, tid, cpu, out, preempt) {
                    self.on_span(&span);
                }
            }
        }
    }

    fn on_sample(&mut self, time: u64, sample: Sample, period: u64, regs_mask: u64) {
        let frames = self.stack_of(&sample, regs_mask);
        if frames.is_empty() {
            return;
        }
        self.counts.samples += 1;
        *self.per_pid.entry(sample.pid).or_default() += 1;
        self.shown.insert((sample.pid, sample.tid));
        // Innermost first.
        let ids: Vec<u32> = frames
            .iter()
            .take(MAX_FRAMES)
            .map(|frame| self.frame_id(sample.pid, *frame))
            .collect();
        let event = LiveEvent {
            start_ns: time,
            duration_ns: period,
            tid: sample.tid,
            pid: sample.pid,
            ..LiveEvent::default()
        };
        self.push(LiveEvent {
            kind: kind::SAMPLE,
            name_id: ids[0],
            ..event
        });
        for (depth, id) in ids.iter().rev().enumerate() {
            self.push(LiveEvent {
                kind: kind::FUNCTION_CALL,
                depth: depth as u8,
                extra: extra::SAMPLED_FRAME,
                name_id: *id,
                ..event
            });
        }
        self.samples.push(StoredSample {
            timestamp_ns: time,
            tid: sample.tid,
            frames: ids,
        });
    }

    /// The frames of a sample, innermost first: its call chain, with the
    /// user part unwound from the copy of the stack when the sample has
    /// one. A recording with stack copies has no user chain to speak of
    /// (`perf` leaves it out, or the kernel's frame-pointer walk gives a
    /// frame or two of a program built without frame pointers).
    fn stack_of(&mut self, sample: &Sample, regs_mask: u64) -> Vec<Frame> {
        let Some(user) = &sample.user else {
            return sample.frames.clone();
        };
        let Some(unwound) = self.unwind(sample.pid, user, regs_mask) else {
            return sample.frames.clone();
        };
        self.counts.unwound += 1;
        let kernel = sample.frames.iter().filter(|f| f.kernel).copied();
        kernel
            .chain(unwound.into_iter().map(|address| Frame {
                address,
                kernel: false,
            }))
            .collect()
    }

    /// The user stack of a sample, from its registers and stack copy:
    /// addresses innermost first, callers already as the address inside
    /// their call. `None` when it cannot be unwound here: registers of
    /// another architecture, or not the ones a walk starts from.
    fn unwind(&mut self, pid: u32, user: &UserStack, regs_mask: u64) -> Option<Vec<u64>> {
        if user.abi != REGS_ABI_64 || !recorded_on_this_architecture(&self.info) {
            return None;
        }
        let start = start_regs(regs_mask, &user.regs)?;
        self.load_modules(pid);
        let (unwinder, _) = self.unwinders.get_mut(&pid)?;
        let outcome = unwinder.unwind(start, start.sp, &user.stack, MAX_FRAMES);
        (!outcome.frames.is_empty()).then_some(outcome.frames)
    }

    /// Gives the process's unwinder the files mapped into it that it does
    /// not have yet, each at the load bias its mapping implies.
    fn load_modules(&mut self, pid: u32) {
        let mappings: Vec<(String, Vec<u8>, u64, u64)> = self
            .processes
            .mappings(pid)
            .iter()
            .filter(|m| m.has_file())
            .map(|m| (m.path.clone(), m.build_id.clone(), m.start, m.pgoff))
            .collect();
        // With no module at all the walk still follows frame pointers.
        self.unwinders
            .entry(pid)
            .or_insert_with(|| (ProcessUnwinder::empty(), HashSet::new()));
        for (path, build_id, start, pgoff) in mappings {
            let Some(bias) = self
                .file(&path, &build_id)
                .map(|symbols| symbols.load_bias(start, pgoff))
            else {
                continue;
            };
            let Some((unwinder, loaded)) = self.unwinders.get_mut(&pid) else {
                continue;
            };
            if loaded.insert((path.clone(), bias)) {
                unwinder.add_module_at(&path, bias);
            }
        }
    }

    /// The id of the frame's name, resolving the address the first time
    /// the process shows it.
    fn frame_id(&mut self, pid: u32, frame: Frame) -> u32 {
        let key = if frame.kernel { KERNEL } else { pid };
        if let Some(id) = self
            .resolved
            .get(&key)
            .and_then(|known| known.get(&frame.address))
        {
            return *id;
        }
        let (name, module, named) = self.name_of(pid, frame);
        let id = match self.ids.get(&name) {
            Some(id) => *id,
            // Names are per function when there are symbols and per
            // address without them, and a long recording of a machine
            // can hold more distinct addresses than the range of frame
            // ids, past which the ids are those of scope names. What
            // does not fit shares one row, and is not remembered.
            None if self.next_id >= self.last_id => {
                if !self.overflowed {
                    self.overflowed = true;
                    log::warn!("perf.data: more distinct frame names than there are frame ids; further ones are shown as \"(other)\"");
                    let other = FrameInfo {
                        name: "(other)".to_string(),
                        ..FrameInfo::default()
                    };
                    self.frames.push((self.last_id, other));
                }
                return self.last_id;
            }
            None => {
                let id = self.next_id;
                self.next_id += 1;
                self.counts.frames += 1;
                self.counts.named_frames += u64::from(named);
                self.ids.insert(name.clone(), id);
                self.frames.push((
                    id,
                    FrameInfo {
                        name,
                        module,
                        address: frame.address,
                        function_id: 0,
                    },
                ));
                id
            }
        };
        self.resolved
            .entry(key)
            .or_default()
            .insert(frame.address, id);
        id
    }

    /// The name of a frame, its module, and whether the name is a
    /// function's (rather than the module and an offset).
    fn name_of(&mut self, pid: u32, frame: Frame) -> (String, String, bool) {
        let Some(mapping) = self.processes.mapping(pid, frame).cloned() else {
            return (
                perfr_perf_data::unmapped_label(frame.address),
                String::new(),
                false,
            );
        };
        let module = mapping.module();
        let name = if frame.kernel {
            self.kernel()
                .and_then(|kernel| kernel.name_at(&mapping, frame.address))
        } else if mapping.path == "[vdso]" {
            // The image is mapped whole: an offset in it is an address in it.
            self.vdso()
                .and_then(|vdso| vdso.name_at(frame.address - mapping.start))
        } else if mapping.has_file() {
            self.file(&mapping.path, &mapping.build_id)
                .and_then(|file| file.name_at(mapping.file_offset(frame.address)))
        } else {
            None
        };
        match name {
            Some(name) => (name, module, true),
            None => (mapping.label(frame.address), module, false),
        }
    }

    /// The symbols of the file at `path`, if it is there and is the build
    /// the recording saw mapped (`build_id`, or the recording's table).
    fn file(&mut self, path: &str, build_id: &[u8]) -> Option<&FileSymbols> {
        if !self.files.contains_key(path) {
            let recorded = if build_id.is_empty() {
                self.info
                    .build_ids
                    .get(path)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
            } else {
                build_id
            };
            let recorded = hex(recorded);
            let file = match self.host.file(path) {
                None => {
                    self.counts.files_missing += 1;
                    File::Unusable
                }
                // A file with no build id cannot be told apart; neither
                // can a recording without one. Both are taken as they are.
                Some(file)
                    if !recorded.is_empty()
                        && !file.build_id().is_empty()
                        && file.build_id() != recorded =>
                {
                    self.counts.files_other_build += 1;
                    log::info!("perf.data: {path} is build {}, the recording has {recorded}: not used for names", file.build_id());
                    File::Unusable
                }
                Some(file) => {
                    self.counts.files_read += 1;
                    File::Symbols(file)
                }
            };
            self.files.insert(path.to_string(), file);
        }
        match self.files.get(path) {
            Some(File::Symbols(symbols)) => Some(symbols),
            _ => None,
        }
    }

    fn vdso(&mut self) -> Option<&FileSymbols> {
        if self.vdso.is_none() {
            // One image per kernel build; the release is what the
            // recording has to tell this kernel from another.
            let same = self.host.release().is_some_and(|release| {
                !self.info.os_release.is_empty() && release == self.info.os_release
            });
            self.vdso = Some(if same { self.host.vdso() } else { None });
        }
        self.vdso.as_ref().and_then(Option::as_ref)
    }

    fn kernel(&mut self) -> Option<&Kernel> {
        if self.kernel.is_none() {
            let text = self
                .processes
                .kernel_mappings()
                .iter()
                .find(|m| m.is_kernel_text())
                .cloned();
            let kernel = text.and_then(|text| Kernel::of(self.host.kernel()?, &text, &self.info));
            self.counts.kernel_symbols = kernel.is_some();
            self.kernel = Some(kernel);
        }
        self.kernel.as_ref().and_then(Option::as_ref)
    }

    /// A span of a thread's time: a state on its bar, and under a running
    /// one a slice on its CPU's row when the recording says which CPU.
    fn on_span(&mut self, span: &Span) {
        self.shown.insert((span.pid, span.tid));
        let event = LiveEvent {
            start_ns: span.start,
            duration_ns: span.end - span.start,
            tid: span.tid,
            pid: span.pid,
            ..LiveEvent::default()
        };
        let state = if span.running {
            thread_state::RUNNING
        } else {
            thread_state::RUNNABLE
        };
        self.push(LiveEvent {
            kind: kind::THREAD_STATE,
            extra: state,
            ..event
        });
        if let (true, Some(cpu)) = (span.running, span.cpu) {
            self.push(LiveEvent {
                kind: kind::SCHEDULING_SLICE,
                extra: cpu.min(255) as u8,
                ..event
            });
        }
    }

    fn push(&mut self, event: LiveEvent) {
        let (start, end) = (event.start_ns, event.end_ns());
        self.bounds = Some(
            self.bounds
                .map_or((start, end), |(a, b)| (a.min(start), b.max(end))),
        );
        self.events.push(event);
    }

    fn finish(self) -> Imported {
        // `perf` writes no comm for the idle threads; it calls them this.
        let name = |tid: u32| {
            self.processes
                .thread_name(tid)
                .or((tid == 0).then_some("swapper"))
                .map(str::to_string)
        };
        let mut threads: Vec<(u32, u32, String)> = self
            .shown
            .iter()
            .filter_map(|(pid, tid)| Some((*pid, *tid, name(*tid)?)))
            .collect();
        threads.sort();
        let mut pids: Vec<u32> = self.shown.iter().map(|(pid, _)| *pid).collect();
        pids.sort_unstable();
        pids.dedup();
        let processes = pids
            .into_iter()
            .filter_map(|pid| Some((pid, name(pid)?)))
            .collect();
        // The process most of the samples are of; the lowest pid among
        // equals, so the answer does not depend on the order of a map.
        let target_pid = self
            .per_pid
            .iter()
            .max_by_key(|(pid, count)| (**count, std::cmp::Reverse(**pid)))
            .map_or(0, |(pid, _)| *pid);
        Imported {
            events: self.events,
            samples: self.samples,
            frames: self.frames,
            threads,
            processes,
            target_pid,
            time_bounds: self.bounds,
            counts: self.counts,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The unwinder is built for the architecture the service runs on.
fn recorded_on_this_architecture(info: &Info) -> bool {
    info.arch == std::env::consts::ARCH
}

/// The registers a walk starts from, out of the ones a sample carries:
/// `mask` says which registers those are, in ascending order of their
/// number in the kernel's `perf_regs.h`.
fn start_regs(mask: u64, regs: &[u64]) -> Option<StartRegs> {
    let reg = |number: u32| -> Option<u64> {
        if mask >> number & 1 == 0 {
            return None;
        }
        regs.get((mask & ((1 << number) - 1)).count_ones() as usize)
            .copied()
    };
    // PERF_REG_X86_BP, _SP, _IP.
    #[cfg(target_arch = "x86_64")]
    let start = StartRegs {
        ip: reg(8)?,
        sp: reg(7)?,
        frame_pointer: reg(6)?,
        link: 0,
    };
    // PERF_REG_ARM64_X29, _LR, _SP, _PC.
    #[cfg(target_arch = "aarch64")]
    let start = StartRegs {
        ip: reg(32)?,
        sp: reg(31)?,
        frame_pointer: reg(29)?,
        link: reg(30)?,
    };
    Some(start)
}

/// Opens `bytes`, a `perf.data`, as the service's current capture, the
/// way a `.perfr.zip` is opened: the ring, the names and the sample store
/// are replaced by what the recording holds.
pub(crate) fn open_perf_data(
    service: &Arc<LiveService>,
    store: &Arc<SampleStore>,
    target: &Arc<AtomicI32>,
    bytes: &[u8],
) -> Result<String, String> {
    let _import = perfr_api::scope("open perf.data");
    let imported = import(bytes, &mut ThisHost)?;
    if imported.events.is_empty() {
        return Err("perf.data: no samples in it".into());
    }
    let counts = imported.counts;
    log::info!(
        "perf.data: {} samples ({} unwound from stack copies), {} of {} frames named; {} files read, {} not found, {} of another build; kernel symbols: {}; {} records lost while recording",
        counts.samples,
        counts.unwound,
        counts.named_frames,
        counts.frames,
        counts.files_read,
        counts.files_missing,
        counts.files_other_build,
        if counts.kernel_symbols { "yes" } else { "no" },
        counts.lost,
    );
    service.clear_ring()?;
    service.clear_names();
    let start = imported.time_bounds.map_or(0, |(start, _)| start);
    target.store(imported.target_pid as i32, Ordering::Relaxed);
    service.mark_capture_started(imported.target_pid, start);
    // Frame ids are intern ids too: the timeline and the report name
    // their rows through the same table.
    for (id, frame) in &imported.frames {
        service.intern_id(*id, &frame.name);
    }
    for (pid, tid, name) in &imported.threads {
        service.set_thread_name(*pid, *tid, name);
    }
    for (pid, name) in &imported.processes {
        service.set_process_name(*pid, name);
    }
    for chunk in imported.events.chunks(4096) {
        service.restore_events(chunk);
    }
    let summary = serde_json::json!({
        "source": "perf.data",
        "events": imported.events.len(),
        "samples": imported.samples.len(),
        "frames": imported.frames.len(),
        "named_frames": counts.named_frames,
        "unwound_samples": counts.unwound,
        "files_read": counts.files_read,
        "files_missing": counts.files_missing,
        "files_other_build": counts.files_other_build,
        "kernel_symbols": counts.kernel_symbols,
        "lost": counts.lost,
        "threads": imported.threads.len(),
        "processes": imported.processes.len(),
        "target_pid": imported.target_pid,
        "time_bounds_ns": imported.time_bounds.map(|(a, b)| serde_json::json!({"start": a, "end": b})),
    });
    store.replace(imported.samples, imported.frames);
    service.mark_capture_finished();
    service.broadcast_status();
    Ok(summary.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_perf_data::sample_type::{
        CALLCHAIN, IDENTIFIER, IP, PERIOD, REGS_USER, STACK_USER, TID, TIME,
    };
    use perfr_perf_data::testing::*;
    use perfr_perf_data::{feature, record};

    const KERNEL_TEXT: u64 = 0xffff_ffff_8000_0000;

    /// A machine with one program on it, linked the way lld links: its
    /// text is the second segment, a page further from its file offset
    /// than the first.
    #[derive(Default)]
    struct OtherHost {
        build_id: &'static str,
        kernel: Option<(&'static str, u64)>,
        asked: Vec<String>,
    }

    impl Host for OtherHost {
        fn file(&mut self, path: &str) -> Option<FileSymbols> {
            self.asked.push(path.to_string());
            (path == "/opt/app").then(|| {
                let symbols = vec![
                    (0x2010, 0x100, "hot".to_string()),
                    (0x2200, 0x100, "_ZN3app6callerEv".to_string()),
                ];
                FileSymbols::from_parts(
                    &[(0, 0x1000, 0), (0x1000, 0x1000, 0x2000)],
                    symbols,
                    self.build_id,
                )
            })
        }

        fn release(&mut self) -> Option<String> {
            self.kernel.map(|(release, _)| release.to_string())
        }

        fn vdso(&mut self) -> Option<FileSymbols> {
            Some(FileSymbols::from_parts(
                &[],
                vec![(0x700, 0x80, "__vdso_clock_gettime".to_string())],
                "",
            ))
        }

        fn kernel(&mut self) -> Option<RunningKernel> {
            let (release, text) = self.kernel?;
            let symbol =
                |at: u64, name: &str, module: &str| (at, name.to_string(), module.to_string());
            Some(RunningKernel {
                release: release.to_string(),
                build_id: Vec::new(),
                symbols: vec![
                    symbol(text, "_text", ""),
                    symbol(text + 0x100, "do_work", ""),
                    symbol(0xffff_ffff_c000_0000, "mod_fn", "gpu"),
                ],
            })
        }
    }

    fn mmap2_with_id(
        e: &Event1,
        pid: u32,
        start: u64,
        len: u64,
        pgoff: u64,
        path: &str,
        id: Option<[u8; 20]>,
    ) -> Vec<u8> {
        let mut record = e.mmap2(pid, start, len, pgoff, path, 1);
        if let Some(id) = id {
            // The build id in place of the device and inode, and the
            // flag that says so.
            record[4..6].copy_from_slice(&(2u16 | 1 << 14).to_le_bytes());
            record[40] = 20;
            record[44..64].copy_from_slice(&id);
        }
        record
    }

    /// A program sampled twice in `hot`, called from `app::caller`, the
    /// second time from inside the kernel.
    fn recording(build_id: Option<[u8; 20]>, release: &str, kernel_text: u64) -> Vec<u8> {
        let e = Event1 {
            sample_type: CALLCHAIN_SAMPLE,
            id: 7,
        };
        let records = vec![
            e.kernel_mmap(kernel_text, 0x100_0000, "[kernel.kallsyms]_text"),
            e.kernel_mmap(0xffff_ffff_c000_0000, 0x1000, "[gpu]"),
            e.comm(10, 10, "app", true, 1),
            mmap2_with_id(&e, 10, 0x40_0000, 0x1000, 0x1000, "/opt/app", build_id),
            e.mmap2(10, 0x7000_0000, 0x2000, 0, "[vdso]", 1),
            e.sample(
                10,
                10,
                300,
                0x40_0010,
                &[CONTEXT_USER, 0x40_0010, 0x40_0201],
            ),
            e.sample(
                10,
                10,
                400,
                kernel_text + 0x120,
                &[
                    CONTEXT_KERNEL,
                    kernel_text + 0x120,
                    CONTEXT_USER,
                    0x40_0020,
                    0x40_0201,
                ],
            ),
            e.sample(
                10,
                10,
                500,
                0x7000_0710,
                &[CONTEXT_USER, 0x7000_0710, 0x40_0201],
            ),
            e.sample(
                10,
                10,
                600,
                0xffff_ffff_c000_0010,
                &[CONTEXT_KERNEL, 0xffff_ffff_c000_0010],
            ),
            finished_round(),
        ];
        let features = vec![
            (feature::OSRELEASE, string_feature(release)),
            (feature::ARCH, string_feature(std::env::consts::ARCH)),
        ];
        file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL | FREQ, 1000), vec![7])],
            &records,
            &features,
        )
    }

    /// The stack of the sample at `time`, outermost first, by name.
    fn stack(imported: &Imported, time: u64) -> Vec<String> {
        let names: HashMap<u32, &str> = imported
            .frames
            .iter()
            .map(|(id, frame)| (*id, frame.name.as_str()))
            .collect();
        let mut frames: Vec<&LiveEvent> = imported
            .events
            .iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL && e.start_ns == time)
            .collect();
        frames.sort_by_key(|e| e.depth);
        assert!(frames
            .iter()
            .all(|e| e.extra == extra::SAMPLED_FRAME && e.duration_ns == 1_000_000));
        frames
            .iter()
            .map(|e| names[&e.name_id].to_string())
            .collect()
    }

    #[test]
    fn frames_are_named_from_the_file_at_the_recorded_path() {
        let mut host = OtherHost {
            build_id: "abababababababababababababababababababab",
            kernel: Some(("6.1.0", KERNEL_TEXT)),
            ..Default::default()
        };
        let imported = import(
            &recording(Some([0xab; 20]), "6.1.0", KERNEL_TEXT),
            &mut host,
        )
        .unwrap();
        // The text is at file offset 0x1000 and address 0x2000: a name
        // taken at the offset, or a page below it, is some other function.
        assert_eq!(stack(&imported, 300), ["app::caller", "hot"]);
        assert_eq!(stack(&imported, 400), ["app::caller", "hot", "do_work"]);
        assert_eq!(
            stack(&imported, 500),
            ["app::caller", "__vdso_clock_gettime"]
        );
        assert_eq!(stack(&imported, 600), ["mod_fn"]);
        assert_eq!(host.asked, ["/opt/app"], "a file is read once");

        // The report's rows: innermost first, one id per function.
        assert_eq!(imported.samples.len(), 4);
        let hot = imported
            .frames
            .iter()
            .find(|(_, f)| f.name == "hot")
            .unwrap();
        assert_eq!((hot.1.module.as_str(), hot.1.address), ("app", 0x40_0010));
        assert_eq!(imported.samples[0].frames[0], hot.0);
        assert_eq!(
            imported.samples[1].frames[1], hot.0,
            "another address in the same function"
        );
        assert!(imported
            .frames
            .iter()
            .all(|(id, _)| *id >= crate::serve::FRAME_NAME_ID_BASE));

        let ticks: Vec<_> = imported
            .events
            .iter()
            .filter(|e| e.kind == kind::SAMPLE)
            .map(|e| (e.start_ns, e.pid, e.tid))
            .collect();
        assert_eq!(
            ticks,
            [(300, 10, 10), (400, 10, 10), (500, 10, 10), (600, 10, 10)]
        );
        assert_eq!(imported.threads, [(10, 10, "app".to_string())]);
        assert_eq!(imported.processes, [(10, "app".to_string())]);
        assert_eq!(imported.target_pid, 10);
        assert_eq!(imported.time_bounds, Some((300, 1_000_600)));
        let counts = imported.counts;
        assert_eq!(
            (counts.samples, counts.frames, counts.named_frames),
            (4, 5, 5)
        );
        assert_eq!(
            (
                counts.files_read,
                counts.files_missing,
                counts.files_other_build,
                counts.kernel_symbols
            ),
            (1, 0, 0, true)
        );
    }

    /// The same path on another machine, or after a rebuild, is another
    /// program. Its names would be confidently wrong.
    #[test]
    fn a_file_of_another_build_names_nothing() {
        let mut host = OtherHost {
            build_id: "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            ..Default::default()
        };
        let imported = import(
            &recording(Some([0xab; 20]), "6.1.0", KERNEL_TEXT),
            &mut host,
        )
        .unwrap();
        assert_eq!(stack(&imported, 300), ["app+0x1200", "app+0x1010"]);
        assert_eq!(imported.counts.files_other_build, 1);

        // A recording from before perf stamped build ids has nothing to
        // compare, and is taken at its word, as `perf report` takes it.
        let imported = import(&recording(None, "6.1.0", KERNEL_TEXT), &mut host).unwrap();
        assert_eq!(stack(&imported, 300), ["app::caller", "hot"]);

        // No file at all.
        let e = Event1 {
            sample_type: CALLCHAIN_SAMPLE,
            id: 7,
        };
        let records = vec![
            e.mmap2(3, 0x1000, 0x1000, 0, "/gone/lib.so", 1),
            e.sample(3, 3, 10, 0x1234, &[CONTEXT_USER, 0x1234]),
        ];
        let bytes = file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
            &records,
            &[],
        );
        let imported = import(&bytes, &mut host).unwrap();
        assert_eq!(stack_any_period(&imported, 10), ["lib.so+0x234"]);
        assert_eq!(imported.counts.files_missing, 1);
        assert!(
            imported.threads.is_empty(),
            "no comm in the recording: the viewer's `tid N` stands"
        );
    }

    fn stack_any_period(imported: &Imported, time: u64) -> Vec<String> {
        let names: HashMap<u32, &str> = imported
            .frames
            .iter()
            .map(|(id, frame)| (*id, frame.name.as_str()))
            .collect();
        let mut frames: Vec<&LiveEvent> = imported
            .events
            .iter()
            .filter(|e| e.kind == kind::FUNCTION_CALL && e.start_ns == time)
            .collect();
        frames.sort_by_key(|e| e.depth);
        frames
            .iter()
            .map(|e| names[&e.name_id].to_string())
            .collect()
    }

    /// The kernel's text is at another address after every boot. The
    /// recording says where it was; this kernel's symbols are where they
    /// are now.
    #[test]
    fn kernel_frames_are_named_across_a_reboot_but_only_by_the_same_kernel() {
        let moved = KERNEL_TEXT + 0x20_0000;
        let mut host = OtherHost {
            kernel: Some(("6.1.0", moved)),
            ..Default::default()
        };
        let imported = import(&recording(None, "6.1.0", KERNEL_TEXT), &mut host).unwrap();
        assert_eq!(
            stack(&imported, 400).last().map(String::as_str),
            Some("do_work")
        );
        // A module is somewhere else too, and nothing says where.
        assert_eq!(stack(&imported, 600), ["[gpu]+0x10"]);
        // The vDSO is this kernel's, whatever the boot.
        assert_eq!(
            stack(&imported, 500).last().map(String::as_str),
            Some("__vdso_clock_gettime")
        );

        let mut host = OtherHost {
            kernel: Some(("6.5.0", KERNEL_TEXT)),
            ..Default::default()
        };
        let imported = import(&recording(None, "6.1.0", KERNEL_TEXT), &mut host).unwrap();
        assert_eq!(
            stack(&imported, 400).last().map(String::as_str),
            Some("[kernel]+0x120")
        );
        assert_eq!(
            stack(&imported, 500).last().map(String::as_str),
            Some("[vdso]+0x710")
        );
        assert!(!imported.counts.kernel_symbols);
    }

    /// `--call-graph dwarf`: no user chain, the registers and the top of
    /// the stack instead. With no unwind tables to hand (the file is not
    /// on this machine) the walk follows the frame pointers in the copy.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn a_stack_copy_is_unwound_into_the_frames_of_the_sample() {
        let sample_type =
            IDENTIFIER | IP | TID | TIME | PERIOD | CALLCHAIN | REGS_USER | STACK_USER;
        let mut a = attr(sample_type, SAMPLE_ID_ALL | FREQ, 1000);
        // BP, SP and IP of the x86-64 registers: bits 6, 7 and 8.
        a[80..88].copy_from_slice(&(0b111u64 << 6).to_le_bytes());
        // Three frames, each a saved frame pointer and a return address.
        const SP: u64 = 0x7ffd_0000_1000;
        let mut stack = Bytes::default();
        for (next, ret) in [
            (SP + 16, 0x40_0201u64),
            (SP + 32, 0x40_0301),
            (0, 0x40_0401),
        ] {
            stack = stack.u64(next).u64(ret);
        }
        let kernel_ip = KERNEL_TEXT + 0x120;
        let body = Bytes::default()
            .u64(7)
            .u64(kernel_ip)
            .u32(10)
            .u32(10)
            .u64(300)
            .u64(1)
            .u64(2) // the chain: the kernel's side only
            .u64(CONTEXT_KERNEL)
            .u64(kernel_ip)
            .u64(2) // abi 64, then bp, sp, ip
            .u64(SP)
            .u64(SP)
            .u64(0x40_0010)
            .u64(stack.0.len() as u64)
            .raw(&stack.0)
            .u64(stack.0.len() as u64);
        let e = Event1 { sample_type, id: 7 };
        let records = vec![
            e.mmap2(10, 0x40_0000, 0x1000, 0, "/gone/app", 1),
            record(record::SAMPLE, MISC_KERNEL, &body.0),
        ];
        let features = vec![(feature::ARCH, string_feature("x86_64"))];
        let bytes = file(&[(a.clone(), vec![7])], &records, &features);

        let imported = import(&bytes, &mut OtherHost::default()).unwrap();
        assert_eq!(imported.counts.unwound, 1);
        // Outermost first: the three callers (each named inside its call),
        // the pc the program entered the kernel at, the kernel's frame.
        assert_eq!(
            stack_any_period(&imported, 300),
            [
                "app+0x400",
                "app+0x300",
                "app+0x200",
                "app+0x10",
                "0xffffffff80000120"
            ]
        );

        // Registers of another architecture are not walked: the sample
        // keeps what its chain has.
        let other = if cfg!(target_arch = "x86_64") {
            "aarch64"
        } else {
            "x86_64"
        };
        let bytes = file(
            &[(a, vec![7])],
            &records,
            &[(feature::ARCH, string_feature(other))],
        );
        let imported = import(&bytes, &mut OtherHost::default()).unwrap();
        assert_eq!(imported.counts.unwound, 0);
        assert_eq!(stack_any_period(&imported, 300), ["0xffffffff80000120"]);
    }

    /// Frame ids are a range, and the ids after it are other things'
    /// names. A recording with more distinct frames than the range has
    /// ids keeps its samples and files the overflow under one name.
    #[test]
    fn frames_past_the_range_of_ids_share_one() {
        let e = Event1 {
            sample_type: CALLCHAIN_SAMPLE,
            id: 7,
        };
        let records: Vec<Vec<u8>> = (0..5u64)
            .map(|i| e.sample(1, 1, 10 + i, 0x1000 + i, &[CONTEXT_USER, 0x1000 + i]))
            .collect();
        let bytes = file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
            &records,
            &[],
        );
        let base = crate::serve::FRAME_NAME_ID_BASE;
        let imported = import_into(&bytes, &mut OtherHost::default(), base + 2).unwrap();
        assert_eq!(imported.samples.len(), 5);
        let leaves: Vec<u32> = imported.samples.iter().map(|s| s.frames[0]).collect();
        assert_eq!(leaves, [base, base + 1, base + 2, base + 2, base + 2]);
        let names: Vec<&str> = imported
            .frames
            .iter()
            .map(|(_, f)| f.name.as_str())
            .collect();
        assert_eq!(names, ["0x1000", "0x1001", "(other)"]);
    }

    #[test]
    fn switch_records_become_thread_states() {
        let e = Event1 {
            sample_type: CALLCHAIN_SAMPLE,
            id: 7,
        };
        let records = vec![
            e.comm(4, 5, "worker", false, 1),
            e.switch(4, 5, 100, false, false),
            e.sample(4, 5, 150, 0x10, &[CONTEXT_USER, 0x10]),
            e.switch(4, 5, 200, true, true),
            e.switch(4, 5, 260, false, false),
        ];
        let bytes = file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
            &records,
            &[],
        );
        let imported = import(&bytes, &mut OtherHost::default()).unwrap();
        let mut states: Vec<_> = imported
            .events
            .iter()
            .filter(|e| e.kind == kind::THREAD_STATE)
            .map(|e| (e.start_ns, e.duration_ns, e.extra))
            .collect();
        states.sort();
        assert_eq!(
            states,
            [
                (100, 100, thread_state::RUNNING),
                (200, 60, thread_state::RUNNABLE)
            ]
        );
        assert_eq!(imported.threads, [(4, 5, "worker".to_string())]);
    }

    #[test]
    fn the_kernels_notes_and_symbols_are_read() {
        let note = |kind: u32, name: &[u8], desc: &[u8]| {
            let pad = |b: &[u8]| [b, &[0u8; 3][..(4 - b.len() % 4) % 4]].concat();
            Bytes::default()
                .u32(name.len() as u32)
                .u32(desc.len() as u32)
                .u32(kind)
                .raw(&pad(name))
                .raw(&pad(desc))
                .0
        };
        let notes = [
            note(1, b"Linux\0", &[1, 2, 3]),
            note(3, b"GNU\0", &[0xaa; 20]),
            note(3, b"Go\0", &[0xbb; 4]),
        ]
        .concat();
        assert_eq!(gnu_build_id(&notes), Some(vec![0xaa; 20]));
        assert_eq!(gnu_build_id(&notes[..20]), None);

        let text = "ffffffff81000000 T _text\nffffffff81000100 t helper\nffffffff82000000 D some_data\nffffffffc0000000 t mod_fn\t[gpu]\n";
        assert_eq!(
            parse_kallsyms(text),
            [
                (0xffff_ffff_8100_0000, "_text".to_string(), String::new()),
                (0xffff_ffff_8100_0100, "helper".to_string(), String::new()),
                (
                    0xffff_ffff_c000_0000,
                    "mod_fn".to_string(),
                    "gpu".to_string()
                ),
            ]
        );
        // Without the privilege to see them, every address reads as zero.
        assert!(parse_kallsyms("0000000000000000 T _text\n0000000000000000 t helper\n").is_empty());
    }

    #[inline(never)]
    fn a_function_of_this_test_binary() -> u64 {
        std::hint::black_box(42)
    }

    /// The real thing: this test binary, where the loader mapped it, as a
    /// recording would have it; and its own symbol table on disk.
    #[test]
    fn this_binary_is_named_from_its_own_file() {
        let address = a_function_of_this_test_binary as usize as u64;
        let maps = std::fs::read("/proc/self/maps").unwrap();
        let mapping = perfr_maps::parse_maps(&maps)
            .into_iter()
            .find(|m| m.start_address <= address && address < m.end_address)
            .expect("the mapping of this code");
        let path = String::from_utf8(mapping.pathname.clone()).unwrap();
        let e = Event1 {
            sample_type: CALLCHAIN_SAMPLE,
            id: 7,
        };
        let records = vec![
            e.comm(1, 1, "test", true, 1),
            e.mmap2(
                1,
                mapping.start_address,
                mapping.end_address - mapping.start_address,
                mapping.offset,
                &path,
                2,
            ),
            e.sample(1, 1, 10, address + 1, &[CONTEXT_USER, address + 1]),
        ];
        let bytes = file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
            &records,
            &[],
        );
        let imported = import(&bytes, &mut ThisHost).unwrap();
        let name = &stack_any_period(&imported, 10)[0];
        assert!(name.contains("a_function_of_this_test_binary"), "{name}");
        assert_eq!(imported.counts.files_read, 1);
    }
}
