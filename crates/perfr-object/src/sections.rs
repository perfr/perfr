// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reading a named section, decompressing it when the file says to.
//!
//! Debug sections are routinely stored with `SHF_COMPRESSED` -- Ubuntu's
//! `libc.debug` compresses `.debug_info`, `.debug_line` and `.debug_str` -- and
//! handing the raw bytes to a DWARF parser silently produces nothing rather
//! than an error.
//!
//! This is the same problem the Bazel port hit from the other side: LLVM
//! needed a patch to turn `LLVM_ENABLE_ZLIB` on before it could read these
//! sections at all. See docs/bazel-port.html.

use std::borrow::Cow;

use object::read::elf::{CompressionHeader, FileHeader, SectionHeader};
use object::{elf, CompressedFileRange, CompressionFormat, Endianness};

/// The DWARF sections the line and declaration walks read: units and their
/// abbreviations, the line programs, address ranges, and the string and
/// address tables those refer into. gimli asks `Dwarf::load` for more
/// (locations, macros, type units), and a section not listed reads as
/// empty, which is what an absent one does anyway. They are left out
/// because nothing here follows a reference into them, and a compressed
/// `.debug_loc` would be inflated on every load for no reader.
const DWARF_SECTION_IDS: &[gimli::SectionId] = &[
    gimli::SectionId::DebugAbbrev,
    gimli::SectionId::DebugAddr,
    gimli::SectionId::DebugInfo,
    gimli::SectionId::DebugLine,
    gimli::SectionId::DebugLineStr,
    gimli::SectionId::DebugRanges,
    gimli::SectionId::DebugRngLists,
    gimli::SectionId::DebugStr,
    gimli::SectionId::DebugStrOffsets,
];

/// Returns the contents of the section whose name, after stripping any leading
/// run of `.`, `_` and `z`, equals `wanted`, decompressed if necessary.
///
/// The name trimming matches what LLVM does before comparing section names, so
/// that `.eh_frame`, `eh_frame` and the old `.zdebug_*` spellings all resolve.
///
/// Returns `None` when no such section exists; returns an empty slice when it
/// exists but cannot be read, so that callers can tell "absent" from "empty".
pub fn section_bytes<'data, Elf>(
    header: &Elf,
    endian: Endianness,
    data: &'data [u8],
    wanted: &[u8],
) -> Option<(Cow<'data, [u8]>, u64)>
where
    Elf: FileHeader<Endian = Endianness>,
{
    let sections = header.sections(endian, data).ok()?;
    for section in sections.iter() {
        // A section whose name cannot be read is skipped, not treated as the
        // end of the search.
        let Ok(name) = sections.section_name(endian, section) else {
            continue;
        };
        let trimmed = name
            .iter()
            .position(|b| !matches!(b, b'.' | b'_' | b'z'))
            .map_or(name, |start| &name[start..]);
        if trimmed != wanted {
            continue;
        }

        let address: u64 = section.sh_addr(endian).into();
        // The spelling that the trim above folds into the plain name is
        // also what says the contents may be in the old GNU format.
        let gnu_compressed = name.starts_with(b".zdebug");
        return Some((decompressed_bytes(section, endian, data, gnu_compressed), address));
    }
    None
}

/// The section's contents: borrowed from the file as they are, or inflated
/// into a buffer when `SHF_COMPRESSED` is set, or when the section is a
/// `.zdebug_*` one in the format that predates the flag (`gnu_compressed`).
/// Borrowing matters: a game's
/// debug file carries a gigabyte of DWARF, and copying it out of a mapping
/// reads every page of it to answer a question about one function.
///
/// Returns an empty slice rather than an error for anything unreadable: the
/// callers all treat a section they cannot parse as one that describes
/// nothing, which is what LLVM does too.
fn decompressed_bytes<'data, S>(
    section: &S,
    endian: Endianness,
    data: &'data [u8],
    gnu_compressed: bool,
) -> Cow<'data, [u8]>
where
    S: SectionHeader<Endian = Endianness>,
{
    match section.compression(endian, data) {
        Ok(Some((header, offset, compressed_size))) => {
            let format = match header.ch_type(endian) {
                elf::ELFCOMPRESS_ZLIB => CompressionFormat::Zlib,
                elf::ELFCOMPRESS_ZSTD => CompressionFormat::Zstandard,
                _ => return Cow::Borrowed(&[]),
            };
            let range = CompressedFileRange {
                format,
                offset,
                compressed_size,
                uncompressed_size: header.ch_size(endian).into(),
            };
            range
                .data(data)
                .and_then(|compressed| compressed.decompress())
                .unwrap_or_default()
        }
        Ok(None) => {
            let bytes = section.data(endian, data).unwrap_or(&[]);
            match gnu_compressed.then(|| gnu_zlib(bytes)).flatten() {
                Some(compressed) => compressed.decompress().unwrap_or_default(),
                None => Cow::Borrowed(bytes),
            }
        }
        Err(_) => Cow::Borrowed(&[]),
    }
}

/// The contents of a `.zdebug_*` section as binutils wrote them before
/// `SHF_COMPRESSED` existed: the four bytes `ZLIB`, the uncompressed size
/// as a big-endian `u64`, then the zlib stream. Handed to gimli as they
/// are, such a section parses as nothing, with no error to say why. `None`
/// when the magic is not there: the section is then what its bytes say.
fn gnu_zlib(bytes: &[u8]) -> Option<object::CompressedData<'_>> {
    let rest = bytes.strip_prefix(b"ZLIB")?;
    let (size, stream) = rest.split_first_chunk::<8>()?;
    Some(object::CompressedData {
        format: CompressionFormat::Zlib,
        data: stream,
        uncompressed_size: u64::from_be_bytes(*size),
    })
}

/// The DWARF sections of one file, located once and decompressed where the
/// file stores them compressed.
///
/// gimli borrows from what it is given, so the decompressed buffers have to
/// outlive the `Dwarf` built over them; loading them into a table first is the
/// simplest way to arrange that.
pub(crate) struct DwarfSections<'data> {
    sections: std::collections::HashMap<&'static str, Cow<'data, [u8]>>,
}

impl<'data> DwarfSections<'data> {
    pub(crate) fn load<Elf>(header: &Elf, endian: Endianness, data: &'data [u8]) -> Self
    where
        Elf: FileHeader<Endian = Endianness>,
    {
        let mut sections = std::collections::HashMap::new();
        for &id in DWARF_SECTION_IDS {
            // gimli names sections with the leading dot; section_bytes matches
            // on the trimmed form.
            let wanted = id.name().trim_start_matches(['.', '_', 'z']);
            if let Some((bytes, _address)) =
                section_bytes(header, endian, data, wanted.as_bytes())
            {
                sections.insert(id.name(), bytes);
            }
        }
        Self { sections }
    }

    pub(crate) fn get(&self, id: gimli::SectionId) -> &[u8] {
        self.sections.get(id.name()).map_or(&[], |bytes| &bytes[..])
    }
}
