// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `.gnu_debuglink` parsing and its CRC-32.
//!
//! Ports `ReadGnuDebuglinkSection` and `ElfFile::CalculateDebuglinkChecksum`
//! from `src/ObjectUtils/ElfFile.cpp`, the latter of which calls `llvm::crc32`.
//! The polynomial is the standard reflected CRC-32 (0xEDB88320) that the ELF
//! specification names for this section, which is also what zlib and
//! `llvm::crc32` compute.

/// The contents of a `.gnu_debuglink` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GnuDebugLink {
    pub path: String,
    pub crc32_checksum: u32,
}

/// Parses a `.gnu_debuglink` section body: a NUL-terminated path, padding to a
/// four-byte boundary, then a little-endian CRC-32.
///
/// Error strings match `ReadGnuDebuglinkSection` so that
/// `ElfFileTest.CalculateDebuglinkChecksum*` keeps passing unchanged.
pub fn parse_gnu_debuglink(contents: &[u8]) -> Result<GnuDebugLink, String> {
    const CHECKSUM_SIZE: usize = std::mem::size_of::<u32>();
    const MINIMUM_PATH_LENGTH: usize = 1;
    const ONE_HUNDRED_KIB: usize = 100 * 1024;

    if contents.len() < MINIMUM_PATH_LENGTH + CHECKSUM_SIZE {
        return Err("Section is too short.".to_owned());
    }
    if contents.len() > ONE_HUNDRED_KIB {
        return Err("Section is longer than 100KiB. Something is not right.".to_owned());
    }

    let path_len = contents
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(contents.len());
    if path_len > contents.len() - CHECKSUM_SIZE {
        return Err("No CRC32 checksum found".to_owned());
    }

    let checksum_bytes = &contents[contents.len() - CHECKSUM_SIZE..];
    let crc32_checksum = u32::from_le_bytes([
        checksum_bytes[0],
        checksum_bytes[1],
        checksum_bytes[2],
        checksum_bytes[3],
    ]);

    Ok(GnuDebugLink {
        path: String::from_utf8_lossy(&contents[..path_len]).into_owned(),
        crc32_checksum,
    })
}

/// The debuglink CRC of a whole file: the reflected CRC-32 (polynomial
/// 0xEDB88320) the ELF specification names for `.gnu_debuglink`, which is
/// zlib's and `llvm::crc32`'s. Streamed in 4 MiB pieces so a gigabyte of
/// debug info is never held in memory. None if unreadable.
///
/// `crc32fast` does the arithmetic. It is in the build regardless, and a
/// byte-at-a-time table loop here took 1.6 s over a game's gigabyte of
/// debug info where it takes a fraction of that.
pub fn crc32_file(path: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 4 << 20];
    let mut hasher = crc32fast::Hasher::new();
    loop {
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            return Some(hasher.finalize());
        }
        hasher.update(&buffer[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::path as testdata_path;

    /// The values are zlib's `crc32` of the same files. The PDB is 9.4 MiB,
    /// so it is read in three pieces: the checksum must carry across them.
    #[test]
    fn a_files_checksum_is_zlibs_crc32_of_it_however_many_pieces_it_is_read_in() {
        assert_eq!(crc32_file(&testdata_path("textfile.txt")), Some(0xcc37_976b));
        assert_eq!(crc32_file(&testdata_path("hello_world_elf.debug")), Some(0x2bf8_87bf));
        assert_eq!(crc32_file(&testdata_path("dllmain.pdb")), Some(0x14fa_84c9));
        assert_eq!(crc32_file(&testdata_path("no such file")), None);
    }

    /// What the checksum is for: the link in the stripped file names the
    /// debug file and carries its CRC.
    #[test]
    fn the_checksum_of_a_debug_file_is_the_one_its_debuglink_carries() {
        let stripped = std::fs::read(testdata_path("hello_world_elf_with_gnu_debuglink")).unwrap();
        let link = crate::parse_elf_metadata(&stripped, "x").unwrap().gnu_debuglink.expect("a debuglink");
        assert_eq!(link.path, "hello_world_elf.debug");
        assert_eq!(crc32_file(&testdata_path(&link.path)), Some(link.crc32_checksum));
    }

    #[test]
    fn parses_a_well_formed_section() {
        // "libfoo.debug\0" padded to 16 bytes, then a little-endian checksum.
        let mut section = b"libfoo.debug\0\0\0\0".to_vec();
        section.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        let info = parse_gnu_debuglink(&section).unwrap();
        assert_eq!(info.path, "libfoo.debug");
        assert_eq!(info.crc32_checksum, 0x1234_5678);
    }

    #[test]
    fn rejects_a_short_section() {
        assert_eq!(
            parse_gnu_debuglink(b"abcd").unwrap_err(),
            "Section is too short."
        );
    }

    #[test]
    fn rejects_a_huge_section() {
        let section = vec![0u8; 100 * 1024 + 1];
        assert_eq!(
            parse_gnu_debuglink(&section).unwrap_err(),
            "Section is longer than 100KiB. Something is not right."
        );
    }

    #[test]
    fn rejects_a_path_that_leaves_no_room_for_a_checksum() {
        // No NUL at all, so the path runs to the end of the section.
        assert_eq!(
            parse_gnu_debuglink(b"aaaaaaaaaa").unwrap_err(),
            "No CRC32 checksum found"
        );
    }
}
