// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Turning a profiled process's memory map into framehop modules.

use framehop::{Module, ModuleSectionInfo};
use object::ObjectSegment as _;
use object::{Object, ObjectSection, ReadCache, ReadRef};
use perfr_maps::{load_bias, MemoryMapping};
use std::ops::Range;

/// Feeds framehop the sections it asks for out of an ELF file: for ELF that
/// is `.eh_frame` and `.eh_frame_hdr`, or `.debug_frame`. Section data is
/// copied out (`uncompressed_data`), which also transparently handles
/// SHF_COMPRESSED debug sections.
struct ObjectSectionInfo<'data, 'file, R: ReadRef<'data>> {
    file: &'file object::File<'data, R>,
}

impl<'data, R: ReadRef<'data>> ModuleSectionInfo<Vec<u8>> for ObjectSectionInfo<'data, '_, R> {
    fn base_svma(&self) -> u64 {
        // For ELF objects the stated base is zero (framehop's convention);
        // relocation is expressed entirely through base_avma.
        0
    }

    fn section_svma_range(&mut self, name: &[u8]) -> Option<Range<u64>> {
        let section = self.file.section_by_name_bytes(name)?;
        Some(section.address()..section.address() + section.size())
    }

    fn section_data(&mut self, name: &[u8]) -> Option<Vec<u8>> {
        let section = self.file.section_by_name_bytes(name)?;
        Some(section.uncompressed_data().ok()?.into_owned())
    }
}

/// Where an image with `segments` (the virtual address and size in memory of
/// each loadable segment) lies once loaded at `base_avma`: from its lowest
/// segment to the end of its highest. `None` for a file with no segments.
///
/// This, not the span of the file's mappings, is the module's range. A file
/// can be mapped again far from its image -- libffi maps a page of its own
/// text once more per table of trampolines -- and a module stretched over
/// both covers every library the process maps in between. The unwinder
/// finds a module by the nearest start at or below an address, so each of
/// those libraries then hid the image's text from its own unwind tables.
pub fn image_range(base_avma: u64, segments: &[(u64, u64)]) -> Option<Range<u64>> {
    let start = segments.iter().map(|(address, _)| *address).min()?;
    let end = segments.iter().map(|(address, size)| address.saturating_add(*size)).max()?;
    Some(base_avma.wrapping_add(start)..base_avma.wrapping_add(end))
}

/// Loads one mapped file as a framehop module, with the address range it
/// occupies. `text` is a mapping of the file that is of the image to unwind
/// through, and `mappings` the whole process. Returns None for files that
/// don't parse as objects (deleted files, special mappings).
///
/// framehop's `base_avma` is where the file's virtual address 0 landed: the
/// image's load bias, which no single mapping's `start - offset` gives.
/// Unreal's binaries (and anything linked with an image base) have their
/// first PT_LOAD at 0x200000, so every `.eh_frame` lookup was off by that
/// much and the unwinder walked no frame of the game itself. The rule is
/// [`load_bias`], the one the symbolizer names these frames by.
///
/// The file is read through a cache that fetches what is asked for: the
/// headers, the section table, and the unwind sections. Reading the file
/// whole to get at those put a gigabyte binary on the heap, one module
/// after another, on the capture thread, before the first pass.
pub fn load_module(
    path: &str,
    mappings: &[MemoryMapping],
    text: &MemoryMapping,
) -> Option<(Module<Vec<u8>>, Range<u64>)> {
    let handle = std::fs::File::open(path).ok()?;
    // A path in the maps need not be a regular file, and a device mapped
    // executable has no end to read up to.
    if !handle.metadata().ok()?.is_file() {
        return None;
    }
    let contents = ReadCache::new(handle);
    let file = object::File::parse(&contents).ok()?;
    let segments: Vec<(u64, u64)> =
        file.segments().map(|segment| (segment.file_range().0, segment.address())).collect();
    let base_avma = load_bias(mappings, text, &segments);
    let extents: Vec<(u64, u64)> = file.segments().map(|segment| (segment.address(), segment.size())).collect();
    let avma_range = image_range(base_avma, &extents)?;
    let module = Module::new(path.to_string(), avma_range.clone(), base_avma, ObjectSectionInfo { file: &file });
    Some((module, avma_range))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_runs_from_its_lowest_segment_to_the_end_of_its_highest() {
        // A position-independent library: segments from 0.
        let pie = [(0, 0x2000), (0x2000, 0xb000), (0xd000, 0x2000), (0xfc70, 0x1400)];
        assert_eq!(image_range(0x7481baaa5000, &pie), Some(0x7481baaa5000..0x7481baab6070));
        // Linked at an image base, loaded with no bias: the range starts at
        // the base, not at zero.
        let based = [(0x200000, 0x1000), (0x201000, 0x5000)];
        assert_eq!(image_range(0, &based), Some(0x200000..0x206000));
        assert_eq!(image_range(0x1000, &[]), None, "nothing loadable, nothing to unwind through");
    }
}
