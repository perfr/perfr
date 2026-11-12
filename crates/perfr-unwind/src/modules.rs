// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Turning a profiled process's memory map into framehop modules.

use framehop::{Module, ModuleSectionInfo};
use object::ObjectSegment as _;
use object::{Object, ObjectSection};
use perfr_maps::{load_bias, MemoryMapping};
use std::ops::Range;

/// Feeds framehop the sections it asks for (`.eh_frame`, `.eh_frame_hdr`,
/// `.debug_frame`, `.text`, `.got`) out of an ELF file. Section data is
/// copied out (`uncompressed_data`), which also transparently handles
/// SHF_COMPRESSED debug sections.
struct ObjectSectionInfo<'data, 'file> {
    file: &'file object::File<'data>,
}

impl ModuleSectionInfo<Vec<u8>> for ObjectSectionInfo<'_, '_> {
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

/// Loads one mapped file as a framehop module. `avma_range` is the span of
/// the file's mappings, `text` one of them that is of the image to unwind
/// through, and `mappings` the whole process. Returns None for files that
/// don't parse as objects (deleted files, special mappings).
///
/// framehop's `base_avma` is where the file's virtual address 0 landed: the
/// image's load bias, which no single mapping's `start - offset` gives.
/// Unreal's binaries (and anything linked with an image base) have their
/// first PT_LOAD at 0x200000, so every `.eh_frame` lookup was off by that
/// much and the unwinder walked no frame of the game itself. The rule is
/// [`load_bias`], the one the symbolizer names these frames by.
pub fn load_module(
    path: &str,
    avma_range: Range<u64>,
    mappings: &[MemoryMapping],
    text: &MemoryMapping,
) -> Option<Module<Vec<u8>>> {
    let contents = std::fs::read(path).ok()?;
    let file = object::File::parse(&contents[..]).ok()?;
    let segments: Vec<(u64, u64)> =
        file.segments().map(|segment| (segment.file_range().0, segment.address())).collect();
    let base_avma = load_bias(mappings, text, &segments);
    Some(Module::new(
        path.to_string(),
        avma_range,
        base_avma,
        ObjectSectionInfo { file: &file },
    ))
}
