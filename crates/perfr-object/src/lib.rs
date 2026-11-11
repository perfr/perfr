// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! ELF metadata, replacing the `llvm::object` half of
//! `src/ObjectUtils/ElfFile.cpp`.
//!
//! Scope is deliberately the metadata that `ElfFileImpl::Initialize` computes
//! -- sections, dynamic entries and program headers -- because those are what
//! the first stage of the port covers. Symbol tables, unwind ranges and DWARF
//! line info still delegate to the C++ implementation; see
//! `docs/rust-port-plan.html`.
//!
//! Error strings are perfr's, character for character, because
//! `ElfFileTest.cpp` matches on them and that test file is never edited.

#![deny(unsafe_code)]

use object::elf;
use object::read::elf::{Dyn, FileHeader, ProgramHeader, SectionHeader};
use object::Endianness;

mod coff;
mod coff_symbols;
mod debugfile;
mod debuglink;
#[cfg(test)]
mod fixtures;
mod msvc;
mod pdb_file;
mod pdb_typename;
mod lineinfo;
mod sections;
mod symbols;
mod unwind;
pub use coff::{is_pe, parse_coff_metadata, CoffMetadata, PdbDebugInfo};
pub use coff_symbols::{
    exception_table_symbols, export_table_symbols, has_export_table,
    unwind_ranges, UnwindRange,
};
pub use debugfile::{
    debuginfod_debuginfo, detached_debug_file, detached_debug_file_under, DEFAULT_DEBUG_ROOT,
};
pub use debuglink::GnuDebugLink;
pub use msvc::demangle_msvc;
pub use pdb_file::{load_pdb_symbols, pdb_info, PdbInfo, UNKNOWN_SYMBOL_SIZE};
pub use lineinfo::{line_rows, LineRow};
pub use lineinfo::{declaration_location, line_info, no_line_info_error, DebugInfo, LineInfo};
pub use symbols::{load_symbols, Symbol, SymbolTable};
pub use unwind::{load_unwind_ranges, no_ranges_error};

/// One `PT_LOAD` segment, mirroring `ModuleInfo::ObjectSegment`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObjectSegment {
    pub offset_in_file: u64,
    pub size_in_file: u64,
    pub address: u64,
    pub size_in_memory: u64,
}

/// Everything `ElfFileImpl::Initialize` establishes about a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElfMetadata {
    pub is_64_bit: bool,
    /// The ELF `e_machine` (e.g. `EM_X86_64` = 62, `EM_AARCH64` = 183), so a
    /// tool that only understands one architecture (a decoder, say) can tell
    /// whether it may look at this file's code.
    pub machine: u16,
    /// Lower-case hex of the `NT_GNU_BUILD_ID` note descriptor; empty if absent.
    pub build_id: String,
    /// `DT_SONAME`, or empty when the file has none.
    pub soname: String,
    pub has_symtab: bool,
    pub has_dynsym: bool,
    pub has_debug_info: bool,
    pub has_patchable_function_entries: bool,
    pub gnu_debuglink: Option<GnuDebugLink>,
    pub load_bias: u64,
    pub executable_segment_offset: u64,
    pub executable_segment_size: u64,
    pub image_size: u64,
    pub loadable_segments: Vec<ObjectSegment>,
}

/// Parses the ELF metadata of `data`.
///
/// `file_path` appears in error messages only, and must be the same string the
/// C++ would have used, because the tests match on the result.
pub fn parse_elf_metadata(data: &[u8], file_path: &str) -> Result<ElfMetadata, String> {
    match object::FileKind::parse(data) {
        Ok(object::FileKind::Elf32) => parse_typed::<elf::FileHeader32<Endianness>>(data, file_path),
        Ok(object::FileKind::Elf64) => parse_typed::<elf::FileHeader64<Endianness>>(data, file_path),
        Ok(_) | Err(_) => Err(format!(
            "Unable to load ELF file \"{file_path}\": The file was not recognized as a valid object file"
        )),
    }
}

fn parse_typed<Elf>(data: &[u8], file_path: &str) -> Result<ElfMetadata, String>
where
    Elf: FileHeader<Endian = Endianness>,
{
    let header = Elf::parse(data).map_err(|e| {
        format!("Unable to load ELF file \"{file_path}\": {e}")
    })?;
    let endian = header
        .endian()
        .map_err(|e| format!("Unable to load ELF file \"{file_path}\": {e}"))?;

    if endian != Endianness::Little {
        return Err(format!(
            "Unable to load \"{file_path}\": Big-endian architectures are not supported."
        ));
    }

    let mut metadata = ElfMetadata {
        is_64_bit: header.is_type_64(),
        machine: header.e_machine(endian),
        ..Default::default()
    };

    // The program headers are the file: without its PT_LOADs nothing maps
    // an address to a file offset, and that stays an error. What the
    // sections and the dynamic table add (build id, debug link, soname) is
    // wanted when it is there and well formed, and absent otherwise. One
    // odd DT_SONAME used to fail the whole call, and the module then
    // offered no hookable function and looked up no debug file.
    init_program_headers(&mut metadata, header, endian, data, file_path)?;
    init_sections(&mut metadata, header, endian, data);
    let _ = init_dynamic_entries(&mut metadata, header, endian, data, file_path);

    Ok(metadata)
}

/// Mirrors `ElfFileImpl::InitSections`, except that nothing here fails the
/// file: an unreadable section table leaves every field at "absent", and
/// a section that cannot be read is skipped for the ones after it.
fn init_sections<Elf>(metadata: &mut ElfMetadata, header: &Elf, endian: Endianness, data: &[u8])
where
    Elf: FileHeader<Endian = Endianness>,
{
    let Ok(sections) = header.sections(endian, data) else {
        return;
    };

    for section in sections.iter() {
        let Ok(name) = sections.section_name(endian, section) else {
            continue;
        };

        match name {
            b".symtab" => {
                metadata.has_symtab = true;
                continue;
            }
            b"__patchable_function_entries" => {
                metadata.has_patchable_function_entries = true;
                continue;
            }
            b".debug_info" => {
                metadata.has_debug_info = true;
                continue;
            }
            _ => {}
        }

        if section.sh_type(endian) == elf::SHT_DYNSYM {
            metadata.has_dynsym = true;
            continue;
        }

        if name == b".note.gnu.build-id" && section.sh_type(endian) == elf::SHT_NOTE {
            read_build_id::<Elf>(metadata, section, endian, data);
            continue;
        }

        if name == b".gnu_debuglink" {
            // A link that does not parse is no link: the build id may
            // still find the debug file.
            metadata.gnu_debuglink =
                section.data(endian, data).ok().and_then(|contents| debuglink::parse_gnu_debuglink(contents).ok());
            continue;
        }
    }
}

fn read_build_id<Elf>(
    metadata: &mut ElfMetadata,
    section: &Elf::SectionHeader,
    endian: Endianness,
    data: &[u8],
) where
    Elf: FileHeader<Endian = Endianness>,
{
    let Ok(Some(mut notes)) = section.notes(endian, data) else {
        // An absent note section, or one that does not parse, leaves the
        // build id empty.
        return;
    };

    // A note that does not parse ends the walk; an id read before it stands.
    while let Ok(Some(note)) = notes.next() {
        if note.n_type(endian) != elf::NT_GNU_BUILD_ID {
            continue;
        }
        for byte in note.desc() {
            use std::fmt::Write as _;
            // absl::StrAppend(&build_id_, absl::Hex(byte, absl::kZeroPad2))
            let _ = write!(metadata.build_id, "{byte:02x}");
        }
    }
}

/// Mirrors `ElfFileImpl::InitDynamicEntries`.
///
/// Like the C++, a file with no dynamic section is not an error -- the soname
/// is simply left empty. The errors say why a soname that is declared could
/// not be read; the caller treats them as "no soname" too.
fn init_dynamic_entries<Elf>(
    metadata: &mut ElfMetadata,
    header: &Elf,
    endian: Endianness,
    data: &[u8],
    file_path: &str,
) -> Result<(), String>
where
    Elf: FileHeader<Endian = Endianness>,
{
    let Ok(segments) = header.program_headers(endian, data) else {
        return Ok(());
    };

    let mut entries = None;
    for segment in segments {
        if let Ok(Some(dynamic)) = segment.dynamic(endian, data) {
            entries = Some(dynamic);
            break;
        }
    }
    let Some(entries) = entries else {
        return Ok(());
    };

    let mut soname_offset = None;
    let mut strtab_addr = None;
    let mut strtab_size = None;
    for entry in entries {
        // d_tag is target-width, so compare as u64 rather than matching on the
        // u32 constants.
        let tag: u64 = entry.d_tag(endian).into();
        let value: u64 = entry.d_val(endian).into();
        if tag == u64::from(elf::DT_SONAME) {
            soname_offset = Some(value);
        } else if tag == u64::from(elf::DT_STRTAB) {
            strtab_addr = Some(value);
        } else if tag == u64::from(elf::DT_STRSZ) {
            strtab_size = Some(value);
        }
    }

    let (Some(soname_offset), Some(strtab_addr), Some(strtab_size)) =
        (soname_offset, strtab_addr, strtab_size)
    else {
        return Ok(());
    };

    if soname_offset >= strtab_size {
        return Err(format!(
            "Soname offset is out of bounds of the string table (file=\"{file_path}\", \
             offset={soname_offset} strtab size={strtab_size})"
        ));
    }

    // llvm::object::ELFFile::toMappedAddr: find the PT_LOAD segment covering a
    // virtual address and convert it to a file offset.
    let to_mapped_offset = |addr: u64| -> Option<u64> {
        for segment in segments {
            if segment.p_type(endian) != elf::PT_LOAD {
                continue;
            }
            let vaddr: u64 = segment.p_vaddr(endian).into();
            let filesz: u64 = segment.p_filesz(endian).into();
            if addr >= vaddr && addr - vaddr < filesz {
                let offset: u64 = segment.p_offset(endian).into();
                return offset.checked_add(addr - vaddr);
            }
        }
        None
    };

    // `strtab_size` is at least one here: it is above `soname_offset`. The
    // sum is the file's to make overflow.
    let Some(last_byte_offset) = strtab_addr.checked_add(strtab_size - 1).and_then(to_mapped_offset) else {
        return Err(format!(
            "Unable to get last byte address of dynamic string table \"{file_path}\""
        ));
    };
    if data.get(last_byte_offset as usize).copied() != Some(0) {
        return Err(format!(
            "Dynamic string table is not null-termintated (file=\"{file_path}\")"
        ));
    }

    let Some(strtab_offset) = to_mapped_offset(strtab_addr) else {
        return Err(format!(
            "Unable to get dynamic string table from DT_STRTAB in \"{file_path}\""
        ));
    };

    let soname_start = strtab_offset.checked_add(soname_offset).and_then(|start| usize::try_from(start).ok());
    let tail = soname_start.and_then(|start| data.get(start..)).unwrap_or(&[]);
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    metadata.soname = String::from_utf8_lossy(&tail[..end]).into_owned();

    Ok(())
}

/// Mirrors `ElfFileImpl::InitProgramHeaders`.
fn init_program_headers<Elf>(
    metadata: &mut ElfMetadata,
    header: &Elf,
    endian: Endianness,
    data: &[u8],
    file_path: &str,
) -> Result<(), String>
where
    Elf: FileHeader<Endian = Endianness>,
{
    let segments = header.program_headers(endian, data).map_err(|e| {
        format!(
            "Unable to get load bias of ELF file: \"{file_path}\". \
             Error loading program headers: {e}"
        )
    })?;

    let mut first_loadable_vaddr: Option<u64> = None;
    for segment in segments {
        if segment.p_type(endian) != elf::PT_LOAD {
            continue;
        }

        let vaddr: u64 = segment.p_vaddr(endian).into();
        let memsz: u64 = segment.p_memsz(endian).into();

        metadata.loadable_segments.push(ObjectSegment {
            offset_in_file: segment.p_offset(endian).into(),
            size_in_file: segment.p_filesz(endian).into(),
            address: vaddr,
            size_in_memory: memsz,
        });

        // image_size is the span from the first loadable segment's start to the
        // furthest loadable segment's end, gaps included -- SizeOfImage for PEs.
        let first = *first_loadable_vaddr.get_or_insert(vaddr);
        metadata.image_size = metadata
            .image_size
            .max(vaddr.wrapping_add(memsz).wrapping_sub(first));
    }

    for segment in segments {
        if segment.p_type(endian) != elf::PT_LOAD {
            continue;
        }
        if segment.p_flags(endian) & elf::PF_X == 0 {
            continue;
        }

        let vaddr: u64 = segment.p_vaddr(endian).into();
        let offset: u64 = segment.p_offset(endian).into();
        metadata.load_bias = vaddr.wrapping_sub(offset);
        metadata.executable_segment_offset = offset;
        metadata.executable_segment_size = segment.p_memsz(endian).into();
        return Ok(());
    }

    Err(format!(
        "Unable to get load bias of ELF file: \"{file_path}\". \
         No executable PT_LOAD segment found."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::read as testdata;

    fn parse(name: &str) -> ElfMetadata {
        parse_elf_metadata(&testdata(name), name).expect("should parse")
    }

    /// TEST(ElfFile, LoadBiasAndExecutableSegmentOffsetAndImageSize)
    #[test]
    fn load_bias_executable_offset_and_image_size() {
        let m = parse("hello_world_elf");
        assert_eq!(m.load_bias, 0x0);
        assert_eq!(m.executable_segment_offset, 0x1000);
        assert_eq!(m.image_size, 0x4038);
    }

    /// TEST(ElfFile, LoadBiasAndExecutableSegmentOffsetAndImageSizeStatic)
    #[test]
    fn load_bias_executable_offset_and_image_size_static() {
        let m = parse("hello_world_static_elf");
        assert_eq!(m.load_bias, 0x400000);
        assert_eq!(m.executable_segment_offset, 0x1000);
        assert_eq!(m.image_size, 0xaaaa0);
    }

    /// TEST(ElfFile, ObjectSegments)
    #[test]
    fn object_segments() {
        let m = parse("hello_world_elf");
        assert_eq!(m.loadable_segments.len(), 4);
        assert_eq!(
            m.loadable_segments[0],
            ObjectSegment { offset_in_file: 0, size_in_file: 0x568, address: 0, size_in_memory: 0x568 }
        );
        assert_eq!(
            m.loadable_segments[1],
            ObjectSegment { offset_in_file: 0x1000, size_in_file: 0x1cd, address: 0x1000, size_in_memory: 0x1cd }
        );
        assert_eq!(
            m.loadable_segments[2],
            ObjectSegment { offset_in_file: 0x2000, size_in_file: 0x160, address: 0x2000, size_in_memory: 0x160 }
        );
        assert_eq!(
            m.loadable_segments[3],
            ObjectSegment { offset_in_file: 0x2de8, size_in_file: 0x248, address: 0x3de8, size_in_memory: 0x250 }
        );
    }

    /// TEST(ElfFile, ObjectSegmentsStatic)
    #[test]
    fn object_segments_static() {
        let m = parse("hello_world_static_elf");
        assert_eq!(m.loadable_segments.len(), 4);
        assert_eq!(
            m.loadable_segments[0],
            ObjectSegment { offset_in_file: 0, size_in_file: 0x4a8, address: 0x400000, size_in_memory: 0x4a8 }
        );
        assert_eq!(
            m.loadable_segments[1],
            ObjectSegment { offset_in_file: 0x1000, size_in_file: 0x7b4e1, address: 0x401000, size_in_memory: 0x7b4e1 }
        );
        assert_eq!(
            m.loadable_segments[2],
            ObjectSegment { offset_in_file: 0x7d000, size_in_file: 0x257f0, address: 0x47d000, size_in_memory: 0x257f0 }
        );
        assert_eq!(
            m.loadable_segments[3],
            ObjectSegment { offset_in_file: 0xa3060, size_in_file: 0x5270, address: 0x4a4060, size_in_memory: 0x6a40 }
        );
    }

    /// Every fixture of this crate was linked by GNU ld, which pads the
    /// file so that all segments sit the same distance from their offsets.
    /// lld does not pad, it moves the next segment a page up, and that is
    /// the layout of most of what gets profiled (Rust, Chromium, Unreal).
    /// The two files are the symbolizer's (`crates/perfr/testdata`, with
    /// their source and the link line): what is pinned here is that this
    /// crate reports each segment's own offset and address, so a caller
    /// that assumes one distance for the whole file has the numbers to
    /// know better.
    #[test]
    fn lld_linked_files_report_segments_at_different_distances_from_their_offsets() {
        let lld = |name: &str| {
            let path = format!("{}/../perfr/testdata/{name}", env!("CARGO_MANIFEST_DIR"));
            let data = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
            let symbols = load_symbols(&data, SymbolTable::Debug).expect("a .symtab");
            let marker = symbols.iter().find(|s| s.mangled_name == "marker").expect("marker").address;
            (parse_elf_metadata(&data, name).expect("should parse"), marker)
        };
        let segment = |offset_in_file, size_in_file, address, size_in_memory| ObjectSegment {
            offset_in_file,
            size_in_file,
            address,
            size_in_memory,
        };

        let (pie, marker) = lld("lld_layout_pie");
        assert_eq!(
            pie.loadable_segments,
            vec![
                segment(0, 0x26b, 0, 0x26b),
                segment(0x270, 0x45, 0x1270, 0x45),
                segment(0x2b8, 0x90, 0x22b8, 0xd48),
            ]
        );
        assert_eq!(marker, 0x12b0);
        // The executable segment is a page further from its offset than
        // the first one: the field named load_bias is the text's distance.
        assert_eq!((pie.load_bias, pie.executable_segment_offset), (0x1000, 0x270));

        let (fixed, marker) = lld("lld_layout_image_base");
        assert_eq!(
            fixed.loadable_segments,
            vec![segment(0, 0x17e, 0x200000, 0x17e), segment(0x180, 0x45, 0x201180, 0x45)]
        );
        assert_eq!(marker, 0x2011c0);
        assert_eq!((fixed.load_bias, fixed.executable_segment_offset), (0x201000, 0x180));
    }

    /// TEST(ElfFile, CalculateLoadBiasNoProgramHeaders) -- the error string is
    /// matched verbatim by the C++ suite, so it is matched verbatim here.
    #[test]
    fn no_executable_load_segment_is_an_error() {
        let data = testdata("hello_world_elf_no_program_headers");
        let err = parse_elf_metadata(&data, "/some/path").unwrap_err();
        assert_eq!(
            err,
            "Unable to get load bias of ELF file: \"/some/path\". \
             No executable PT_LOAD segment found."
        );
    }

    /// TEST(ElfFile, GetBuildId)
    #[test]
    fn build_id() {
        assert_eq!(
            parse("hello_world_elf").build_id,
            "d12d54bc5b72ccce54a408bdeda65e2530740ac8"
        );
        assert_eq!(parse("hello_world_elf_no_build_id").build_id, "");
    }

    /// TEST(ElfFile, HasDebugInfo) and DoesNotHaveDebugInfo
    #[test]
    fn has_debug_info() {
        assert!(parse("hello_world_elf_with_debug_info").has_debug_info);
        assert!(!parse("hello_world_elf").has_debug_info);
    }

    /// TEST(ElfFile, HasDebugSymbols)
    #[test]
    fn has_debug_symbols() {
        assert!(parse("hello_world_elf").has_symtab);
        assert!(!parse("no_symbols_elf").has_symtab);
    }

    /// TEST(ElfFile, HasDynsym)
    #[test]
    fn has_dynsym() {
        assert!(parse("libtest-1.0.so").has_dynsym);
        assert!(!parse("hello_world_static_elf").has_dynsym);
    }

    /// TEST(ElfFile, GetSonameSmoke) and GetNameForFileWithoutSoname
    #[test]
    fn soname() {
        assert_eq!(parse("libtest-1.0.so").soname, "libtest.so");
        assert_eq!(parse("hello_world_elf").soname, "");
    }

    /// TEST(ElfFile, HasGnuDebugLink) and HasNoGnuDebugLink
    #[test]
    fn gnu_debuglink() {
        let with = parse("hello_world_elf_with_gnu_debuglink");
        let link = with.gnu_debuglink.expect("should have a debuglink");
        assert_eq!(link.path, "hello_world_elf.debug");
        assert!(parse("hello_world_elf").gnu_debuglink.is_none());
    }

    #[test]
    fn is_64_bit() {
        assert!(parse("hello_world_elf").is_64_bit);
    }

    fn u64_at(data: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
    }

    /// File offset and size of the 64-bit section header `index`, and of
    /// the contents it describes.
    fn section_header_at(data: &[u8], index: usize) -> usize {
        let e_shoff = u64_at(data, 0x28) as usize;
        let e_shentsize = u16::from_le_bytes(data[0x3a..0x3c].try_into().unwrap()) as usize;
        e_shoff + index * e_shentsize
    }

    fn section_named(data: &[u8], wanted: &[u8]) -> (usize, usize) {
        let header = elf::FileHeader64::<Endianness>::parse(data).unwrap();
        let endian = header.endian().unwrap();
        let sections = header.sections(endian, data).unwrap();
        let section = sections
            .iter()
            .find(|section| sections.section_name(endian, *section).ok() == Some(wanted))
            .expect("the fixture has the section");
        (section.sh_offset(endian) as usize, section.sh_size(endian) as usize)
    }

    /// The four PT_LOADs are what every caller wants: they turn a symbol's
    /// address into the file offset a hook goes at. A debug link the file
    /// got wrong must not take them away, only the debug link.
    #[test]
    fn a_malformed_debuglink_costs_the_debuglink_and_nothing_else() {
        let mut data = testdata("hello_world_elf_with_gnu_debuglink");
        let (offset, size) = section_named(&data, b".gnu_debuglink");
        // No NUL anywhere: a path with no room left for its checksum.
        data[offset..offset + size].fill(b'a');
        let m = parse_elf_metadata(&data, "x").expect("the rest of the file is fine");
        assert!(m.gnu_debuglink.is_none());
        assert_eq!(m.loadable_segments.len(), 4);
        assert_eq!(m.build_id, parse("hello_world_elf_with_gnu_debuglink").build_id);
        assert!(!m.build_id.is_empty());
    }

    #[test]
    fn a_section_with_an_unreadable_name_is_skipped_and_the_rest_are_read() {
        let mut data = testdata("hello_world_elf");
        // sh_name is the first field of a section header; section 1 comes
        // before the build-id note and the symbol table.
        let sh_name = section_header_at(&data, 1);
        data[sh_name..sh_name + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let m = parse_elf_metadata(&data, "x").expect("one bad name is not a bad file");
        assert_eq!(m.loadable_segments.len(), 4);
        assert_eq!(m.build_id, "d12d54bc5b72ccce54a408bdeda65e2530740ac8");
        assert!(m.has_symtab);
    }

    /// DT_STRTAB and DT_STRSZ are the file's to choose. Their sum must not
    /// be computed unchecked: it wraps in a release build and aborts a
    /// debug one.
    #[test]
    fn a_dynamic_string_table_at_the_top_of_the_address_space_is_no_soname_not_an_overflow() {
        let mut data = testdata("libtest-1.0.so");
        let (offset, size) = section_named(&data, b".dynamic");
        let mut patched = false;
        for entry in (offset..offset + size).step_by(16) {
            if u64_at(&data, entry) == u64::from(elf::DT_STRTAB) {
                data[entry + 8..entry + 16].copy_from_slice(&(u64::MAX - 1).to_le_bytes());
                patched = true;
            }
        }
        assert!(patched, "the fixture has a DT_STRTAB");
        let m = parse_elf_metadata(&data, "x").expect("an odd soname is not a bad file");
        assert_eq!(m.soname, "");
        assert_eq!(m.loadable_segments, parse("libtest-1.0.so").loadable_segments);
    }

    /// Malformed input must not panic. The C++ has a fuzzer for exactly this.
    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(parse_elf_metadata(b"", "x").is_err());
        assert!(parse_elf_metadata(b"not an elf file at all", "x").is_err());
        assert!(parse_elf_metadata(&[0x7f, b'E', b'L', b'F'], "x").is_err());

        // Truncate a real file at every power of two and check none of them panic.
        let good = testdata("hello_world_elf");
        let mut len = 1;
        while len < good.len() {
            let _ = parse_elf_metadata(&good[..len], "truncated");
            len *= 2;
        }
    }
}
