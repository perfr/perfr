// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The per-process unwinder: modules from the process's maps, frames from a
//! copied stack slice.

use crate::modules::load_module;
use framehop::{UnwindIterator, Unwinder};
use perfr_maps::{parse_maps, MemoryMapping, PROT_EXEC};
use std::collections::HashMap;

#[cfg(target_arch = "x86_64")]
type ArchUnwinder = framehop::x86_64::UnwinderX86_64<Vec<u8>>;
#[cfg(target_arch = "x86_64")]
type ArchCache = framehop::x86_64::CacheX86_64;
#[cfg(target_arch = "aarch64")]
type ArchUnwinder = framehop::aarch64::UnwinderAarch64<Vec<u8>>;
#[cfg(target_arch = "aarch64")]
type ArchCache = framehop::aarch64::CacheAarch64;

/// The registers unwinding starts from, in capture terms. On x86_64 `link`
/// is unused; on aarch64 it is LR.
#[derive(Clone, Copy, Debug)]
pub struct StartRegs {
    pub ip: u64,
    pub sp: u64,
    pub frame_pointer: u64,
    pub link: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindOutcome {
    /// Frame addresses, outermost last: `frames[0]` is the sampled ip, the
    /// rest are return addresses minus one -- the address INSIDE the call
    /// instruction, which is libunwindstack's `FrameData::pc` convention and
    /// what perfr's symbolization consumes. (The differential caught this:
    /// raw return addresses diverged from the C++ by exactly one on every
    /// frame past the first.)
    pub frames: Vec<u64>,
    /// None on a clean walk to the root, the framehop error otherwise.
    pub error: Option<String>,
}

impl UnwindOutcome {
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

pub struct ProcessUnwinder {
    unwinder: ArchUnwinder,
    cache: ArchCache,
    modules_loaded: usize,
    /// Every mapped file with text that a maps snapshot has shown, by path.
    files: HashMap<Vec<u8>, KnownFile>,
}

/// A mapped file as the unwinder last saw it.
#[derive(Debug)]
struct KnownFile {
    /// The span of its mappings.
    span: std::ops::Range<u64>,
    /// Where its text was mapped: with the span, what tells one mapping of
    /// the file from a later one somewhere else.
    text_start: u64,
    /// The address range of the module it became. None for a file that did
    /// not parse as an object (deleted, not an ELF), which is remembered all
    /// the same so that it is not read again at every refresh.
    module: Option<std::ops::Range<u64>>,
}

/// What a refresh from a newer maps snapshot changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModulesChange {
    pub added: usize,
    pub removed: usize,
}

/// One mapped file's aggregate: its address span and its text.
#[derive(Debug)]
struct FileSpan<'a> {
    start: u64,
    end: u64,
    /// The file's largest executable mapping, None when it has none. A
    /// module has one base, so it has to be that of the loader's image, and
    /// a file can have executable mappings that are not part of it: libffi
    /// maps a page of its own text again for each table of trampolines.
    text: Option<&'a MemoryMapping>,
}

/// The files mapped into a process, by path.
fn file_spans(mappings: &[MemoryMapping]) -> HashMap<&[u8], FileSpan<'_>> {
    let mut files: HashMap<&[u8], FileSpan> = HashMap::new();
    for mapping in mappings {
        if !is_file_backed(mapping) {
            continue;
        }
        let entry = files.entry(&mapping.pathname).or_insert(FileSpan {
            start: mapping.start_address,
            end: mapping.end_address,
            text: None,
        });
        entry.start = entry.start.min(mapping.start_address);
        entry.end = entry.end.max(mapping.end_address);
        let size = |m: &MemoryMapping| m.end_address.saturating_sub(m.start_address);
        if mapping.perms & PROT_EXEC != 0 && entry.text.is_none_or(|text| size(mapping) > size(text)) {
            entry.text = Some(mapping);
        }
    }
    files
}

impl ProcessUnwinder {
    /// Builds an unwinder from the content of a `/proc/<pid>/maps`.
    pub fn from_maps_content(maps_content: &[u8]) -> ProcessUnwinder {
        let mut unwinder = ProcessUnwinder {
            unwinder: ArchUnwinder::new(),
            cache: ArchCache::new(),
            modules_loaded: 0,
            files: HashMap::new(),
        };
        unwinder.refresh_from_maps_content(maps_content);
        unwinder
    }

    pub fn for_pid(pid: i32) -> std::io::Result<ProcessUnwinder> {
        let content = std::fs::read(format!("/proc/{pid}/maps"))?;
        Ok(Self::from_maps_content(&content))
    }

    /// Brings the modules in line with the process's maps as they are now:
    /// see [`ProcessUnwinder::refresh_from_maps_content`].
    pub fn refresh(&mut self, pid: i32) -> std::io::Result<ModulesChange> {
        let content = std::fs::read(format!("/proc/{pid}/maps"))?;
        Ok(self.refresh_from_maps_content(&content))
    }

    /// Brings the modules in line with a newer `/proc/<pid>/maps`: files
    /// mapped since the last snapshot are loaded, files no longer mapped (or
    /// mapped somewhere else now) are dropped, and the rest are left alone
    /// without being read again.
    ///
    /// A process keeps mapping code after a capture starts -- plugins, the
    /// extension modules of an interpreter, a game's level code -- and an
    /// address in a module this does not know is unwound by guessing at
    /// frame pointers: right only for code that keeps them, and otherwise a
    /// stack cut short or wrong from that frame up.
    pub fn refresh_from_maps_content(&mut self, maps_content: &[u8]) -> ModulesChange {
        let mappings = parse_maps(maps_content);
        let spans = file_spans(&mappings);
        let mut change = ModulesChange::default();

        let unwinder = &mut self.unwinder;
        self.files.retain(|path, known| {
            let still_there = spans.get(&path[..]).is_some_and(|span| {
                span.text.is_some_and(|text| {
                    known.span == (span.start..span.end) && known.text_start == text.start_address
                })
            });
            if !still_there {
                if let Some(module) = &known.module {
                    unwinder.remove_module(module.start);
                    change.removed += 1;
                }
            }
            still_there
        });

        for (path, span) in &spans {
            let Some(text) = span.text else { continue };
            if self.files.contains_key(*path) {
                continue;
            }
            let loaded = std::str::from_utf8(path).ok().and_then(|path| load_module(path, &mappings, text));
            let module = loaded.map(|(module, range)| {
                self.unwinder.add_module(module);
                change.added += 1;
                range
            });
            self.files.insert(
                path.to_vec(),
                KnownFile { span: span.start..span.end, text_start: text.start_address, module },
            );
        }
        self.modules_loaded = self.modules_loaded + change.added - change.removed;
        change
    }

    /// Whether `start..end` lies inside the mappings of `path` as the last
    /// snapshot showed them. The kernel announces an executable mapping
    /// again whenever its protection changes (a hook engine does that to
    /// every page it patches), so most announcements are of files already
    /// here and need no refresh.
    pub fn covers(&self, path: &[u8], start: u64, end: u64) -> bool {
        self.files.get(path).is_some_and(|known| known.span.start <= start && end <= known.span.end)
    }

    pub fn modules_loaded(&self) -> usize {
        self.modules_loaded
    }

    /// The address range of the module loaded for `path`, if it is one.
    pub fn module_range(&self, path: &[u8]) -> Option<std::ops::Range<u64>> {
        self.files.get(path).and_then(|known| known.module.clone())
    }

    /// Unwinds from `regs` over the copied stack slice starting at
    /// `stack_base` (the captured stack pointer). Reads outside the slice
    /// fail, like `offline_memory_only` unwinding in the C++.
    pub fn unwind(
        &mut self,
        regs: StartRegs,
        stack_base: u64,
        stack: &[u8],
        max_frames: usize,
    ) -> UnwindOutcome {
        let mut read_stack = |address: u64| -> Result<u64, ()> {
            let offset = address.checked_sub(stack_base).ok_or(())? as usize;
            let bytes = stack.get(offset..offset.checked_add(8).ok_or(())?).ok_or(())?;
            Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
        };

        #[cfg(target_arch = "x86_64")]
        let arch_regs =
            framehop::x86_64::UnwindRegsX86_64::new(regs.ip, regs.sp, regs.frame_pointer);
        #[cfg(target_arch = "aarch64")]
        let arch_regs =
            framehop::aarch64::UnwindRegsAarch64::new(regs.link, regs.sp, regs.frame_pointer);

        let mut iterator =
            UnwindIterator::new(&self.unwinder, regs.ip, arch_regs, &mut self.cache, &mut read_stack);
        let mut outcome = UnwindOutcome { frames: Vec::new(), error: None };
        loop {
            match iterator.next() {
                Ok(Some(frame)) => {
                    outcome.frames.push(frame.address_for_lookup());
                    if outcome.frames.len() >= max_frames {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    outcome.error = Some(error.to_string());
                    break;
                }
            }
        }
        outcome
    }
}

fn is_file_backed(mapping: &MemoryMapping) -> bool {
    mapping.inode != 0 && mapping.pathname.first() == Some(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_perf_records::reader::{parse_record_sample, SampleFlags};
    use perfr_perf_records::{record_type, PerfEventHeader};

    #[test]
    fn a_files_text_is_its_largest_executable_mapping() {
        // libffi in a process that uses closures: one page of its text is
        // mapped a second time, below the library. The lowest
        // `start - offset` of the file is that page's, 58 MB from where
        // the library's unwind tables are.
        let mappings = parse_maps(
            b"\
7481b7178000-7481b7179000 r-xp 0000b000 103:02 100825209                 /usr/lib/libffi.so.8
7481baaa5000-7481baaa7000 r--p 00000000 103:02 100825209                 /usr/lib/libffi.so.8
7481baaa7000-7481baab2000 r-xp 00002000 103:02 100825209                 /usr/lib/libffi.so.8
7481baab2000-7481baab4000 r--p 0000d000 103:02 100825209                 /usr/lib/libffi.so.8
7481baab5000-7481baab6000 rw-p 0000f000 103:02 100825209                 /usr/lib/libffi.so.8
7481bab00000-7481bab01000 r--p 00000000 103:02 100825300                 /usr/share/data.bin
7481bab01000-7481bab02000 rw-p 00000000 00:00 0                          [heap]
",
        );
        let files = file_spans(&mappings);
        assert_eq!(files.len(), 2, "the heap is not a file");
        let libffi = &files[&b"/usr/lib/libffi.so.8"[..]];
        assert_eq!((libffi.start, libffi.end), (0x7481b7178000, 0x7481baab6000));
        let text = libffi.text.expect("libffi has text");
        assert_eq!(text.start_address, 0x7481baaa7000);
        let segments = [(0, 0), (0x2000, 0x2000), (0xd000, 0xd000), (0xec70, 0xfc70)];
        assert_eq!(perfr_maps::load_bias(&mappings, text, &segments), 0x7481baaa5000);
        assert!(files[&b"/usr/share/data.bin"[..]].text.is_none(), "nothing to unwind through");
    }

    /// The maps of this test process, split into the lines of the test
    /// executable and everything else.
    fn own_maps() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        let all = std::fs::read("/proc/self/maps").unwrap();
        let (mut own, mut others) = (Vec::new(), Vec::new());
        for line in all.split_inclusive(|b| *b == b'\n') {
            if line.strip_suffix(b"\n").unwrap_or(line).ends_with(exe.as_bytes()) {
                own.extend_from_slice(line);
            } else {
                others.extend_from_slice(line);
            }
        }
        (all, own, others)
    }

    #[test]
    fn a_file_mapped_after_the_unwinder_was_built_is_loaded_by_a_refresh() {
        let (all, own, _) = own_maps();
        // As if the capture had started before the libraries were mapped.
        let mut unwinder = ProcessUnwinder::from_maps_content(&own);
        assert_eq!(unwinder.modules_loaded(), 1, "the test executable alone");
        let libc = parse_maps(&all)
            .into_iter()
            .find(|m| m.perms & PROT_EXEC != 0 && m.pathname.windows(4).any(|w| w == b"libc"))
            .expect("the test process maps libc");
        assert!(!unwinder.covers(&libc.pathname, libc.start_address, libc.end_address));

        let change = unwinder.refresh_from_maps_content(&all);
        assert!(change.added >= 1, "libc was not loaded: {change:?}");
        assert_eq!(change.removed, 0);
        assert_eq!(unwinder.modules_loaded(), 1 + change.added);
        assert!(unwinder.covers(&libc.pathname, libc.start_address, libc.end_address));

        // The same maps again: nothing to do, and nothing read twice.
        assert_eq!(unwinder.refresh_from_maps_content(&all), ModulesChange::default());
    }

    #[test]
    fn a_file_that_was_unmapped_or_moved_is_dropped_by_a_refresh() {
        let (all, own, _) = own_maps();
        let mut unwinder = ProcessUnwinder::from_maps_content(&all);
        let loaded = unwinder.modules_loaded();
        assert!(loaded > 1);
        let change = unwinder.refresh_from_maps_content(&own);
        assert_eq!(change, ModulesChange { added: 0, removed: loaded - 1 });
        assert_eq!(unwinder.modules_loaded(), 1);

        // The executable "mapped again" 1 GiB higher: the module at the old
        // addresses goes and one at the new addresses comes.
        let moved: Vec<u8> = parse_maps(&own)
            .iter()
            .map(|m| {
                format!(
                    "{:x}-{:x} r{}p {:08x} 00:00 {} {}\n",
                    m.start_address + (1 << 30),
                    m.end_address + (1 << 30),
                    if m.perms & PROT_EXEC != 0 { "-x" } else { "--" },
                    m.offset,
                    m.inode,
                    String::from_utf8_lossy(&m.pathname),
                )
            })
            .collect::<String>()
            .into_bytes();
        assert_eq!(unwinder.refresh_from_maps_content(&moved), ModulesChange { added: 1, removed: 1 });
    }

    #[test]
    fn a_mapped_file_that_is_not_an_object_is_not_read_at_every_refresh() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("perfr-unwind-not-an-object-{}", std::process::id()));
        std::fs::write(&path, b"this is not an ELF file").unwrap();
        let maps = format!("7f0000000000-7f0000001000 r-xp 00000000 00:00 42 {}\n", path.display());
        let mut unwinder = ProcessUnwinder::from_maps_content(maps.as_bytes());
        assert_eq!(unwinder.modules_loaded(), 0);
        assert!(unwinder.covers(path.to_str().unwrap().as_bytes(), 0x7f0000000000, 0x7f0000001000));
        // Had the refresh tried the file again it would now find an object.
        std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
        assert_eq!(unwinder.refresh_from_maps_content(maps.as_bytes()), ModulesChange::default());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_module_spans_its_image_not_every_mapping_of_its_file() {
        // The test executable, and one page of its text mapped a second
        // time far below it, the way libffi maps a page of itself per
        // table of trampolines.
        let (_, own, _) = own_maps();
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();
        let image_start = parse_maps(&own).iter().map(|m| m.start_address).min().unwrap();
        let image_end = parse_maps(&own).iter().map(|m| m.end_address).max().unwrap();
        let mut maps = format!("10000000-10001000 r-xp 00001000 00:00 42 {exe}\n").into_bytes();
        maps.extend_from_slice(&own);
        let unwinder = ProcessUnwinder::from_maps_content(&maps);
        let module = unwinder.module_range(exe.as_bytes()).expect("the executable is a module");
        // A module that began at the stray page would cover everything the
        // process maps between the two, and the unwinder looks an address up
        // by the nearest module start below it: any library in between hid
        // the image's own text from its own unwind tables.
        assert_eq!(module.start, image_start, "the module starts where the image does");
        // Its end is the end of the last segment: inside the last page the
        // file is mapped on, or beyond it by the size of the bss.
        assert!(
            module.end > image_end - 4096 && module.end < image_end + (64 << 20),
            "{module:x?} against mappings that end at {image_end:x}"
        );
    }

    #[test]
    fn a_mapping_of_something_that_is_not_a_regular_file_is_not_read() {
        // Read to its end, /dev/zero has none.
        let unwinder = ProcessUnwinder::from_maps_content(b"7f0000000000-7f0000001000 r-xp 00000000 00:05 5 /dev/zero\n");
        assert_eq!(unwinder.modules_loaded(), 0);
    }

    /// What building the unwinder of a live process costs:
    /// `PERFR_UNWIND_BENCH_PID=<pid> cargo test --release -p perfr-unwind -- --ignored --nocapture building_cost`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn building_cost_for_a_live_process() {
        let Some(pid) = std::env::var("PERFR_UNWIND_BENCH_PID").ok().and_then(|pid| pid.parse::<i32>().ok()) else {
            eprintln!("set PERFR_UNWIND_BENCH_PID");
            return;
        };
        let file_bytes: u64 = file_spans(&parse_maps(&std::fs::read(format!("/proc/{pid}/maps")).unwrap()))
            .iter()
            .filter(|(_, span)| span.text.is_some())
            .filter_map(|(path, _)| std::fs::metadata(std::str::from_utf8(path).ok()?).ok())
            .map(|meta| meta.len())
            .sum();
        for round in 0..3 {
            let start = std::time::Instant::now();
            let unwinder = ProcessUnwinder::for_pid(pid).unwrap();
            eprintln!(
                "round {round}: {} modules ({} MB of files) in {:.1} ms",
                unwinder.modules_loaded(),
                file_bytes >> 20,
                start.elapsed().as_secs_f64() * 1e3
            );
        }
    }

    // Deep, non-inlinable recursion so samples land under a real call chain.
    #[inline(never)]
    fn burn(depth: u32, spin: u64) -> u64 {
        let mut acc = std::hint::black_box(spin);
        if depth > 0 {
            for j in 0..(1u64 << 12) {
                acc = acc.wrapping_add(j * j);
            }
            acc = acc.wrapping_add(burn(depth - 1, acc));
        } else {
            for j in 0..(1u64 << 18) {
                acc = acc.wrapping_add(j * j);
            }
        }
        std::hint::black_box(acc)
    }

    // Live end-to-end: sample this thread with full regs + stack, unwind
    // each sample with framehop over the stack copy, and require deep,
    // mostly-successful unwinds. Skips where perf_event_open is denied.
    #[test]
    fn unwinds_own_samples_deeply() {
        #[allow(unsafe_code)]
        let tid = unsafe { libc::gettid() };
        let mut ring = match perfr_perf_ring::ring::open_stack_sample(200_000, 64000, tid, -1, 8192)
        {
            Ok(ring) => ring,
            Err(error) => {
                eprintln!("skipping: perf_event_open not permitted here ({error})");
                return;
            }
        };
        ring.enable().unwrap();
        // Interleave work and draining so the ring never overflows while
        // still accumulating a few hundred milliseconds of samples. Both the
        // record count and the wall clock are capped: each record carries a
        // 64 KB stack copy, and a machine whose kernel delivers samples
        // faster than this thread drains them once grew this Vec to 70 GB.
        const MAX_RECORDS: usize = 4_000;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut records = Vec::new();
        for round in 0..200u64 {
            burn(24, round);
            while records.len() < MAX_RECORDS {
                match ring.read_record().unwrap() {
                    Some(record) => records.push(record),
                    None => break,
                }
            }
            if records.len() >= MAX_RECORDS || std::time::Instant::now() >= deadline {
                break;
            }
        }

        let mut unwinder = ProcessUnwinder::for_pid(std::process::id() as i32).unwrap();
        assert!(unwinder.modules_loaded() > 0, "no modules loaded from own maps");

        let mut errors: std::collections::HashMap<(String, usize), u64> = Default::default();
        let mut samples = 0u64;
        let mut successes = 0u64;
        let mut deep = 0u64;
        for record in records {
            let header = PerfEventHeader::parse(&record).unwrap();
            if { header.kind } != record_type::SAMPLE {
                continue;
            }
            let sample = parse_record_sample(&record, SampleFlags::stack_sample(), true).unwrap();
            let Some(regs) = sample.regs.as_deref() else { continue };
            let Some(stack) = sample.stack_data.as_deref() else { continue };
            // kSampleRegsUserAll order on x86_64: ax,bx,cx,dx,si,di,bp,sp,ip,..
            #[cfg(target_arch = "x86_64")]
            let start = StartRegs { ip: regs[8], sp: regs[7], frame_pointer: regs[6], link: 0 };
            #[cfg(target_arch = "aarch64")]
            let start = StartRegs { ip: regs[32], sp: regs[31], frame_pointer: regs[29], link: regs[30] };

            samples += 1;
            let outcome = unwinder.unwind(start, start.sp, stack, 256);
            if outcome.is_success() {
                successes += 1;
            } else if let Some(error) = &outcome.error {
                *errors.entry((error.clone(), outcome.frames.len().min(3))).or_insert(0u64) += 1;
            }
            if outcome.frames.len() >= 10 {
                deep += 1;
            }
        }

        let mut error_list: Vec<_> = errors.into_iter().collect();
        error_list.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        for ((error, frames), count) in error_list.iter().take(6) {
            eprintln!("error {count}x at >={frames} frames: {error}");
        }
        assert!(samples > 10, "only {samples} samples");
        assert!(
            successes * 10 >= samples * 5,
            "success rate too low: {successes}/{samples}"
        );
        assert!(deep > 0, "no sample unwound 10+ frames ({samples} samples)");
        eprintln!("samples={samples} successes={successes} deep={deep}");
    }
}
