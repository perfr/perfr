// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Files that are almost right.
//!
//! The parsers' own tests feed them garbage and truncated files, which a
//! parser rejects at its first read. A file a hostile or merely broken
//! toolchain wrote is different: its structure holds and one field lies --
//! a section that claims to end past the file, a string table at the top of
//! the address space, a count of four billion. That is where an unchecked
//! sum or an index lives, and the service runs with `panic = "abort"`, so
//! one such file among a target's modules would end the capture.
//!
//! So: take each fixture, overwrite a few header fields with the values
//! that break arithmetic, and call everything the service calls on it.
//! Nothing may panic; what is returned does not matter. The mutations are
//! drawn from a fixed seed, so a failure names the fixture and the round
//! and happens again on the next run. This is not a fuzzer -- there is no
//! coverage feedback and the rounds are few enough to run in every `cargo
//! test` -- but test builds check integer overflow, which is the class of
//! mistake it is here to catch.

use std::ops::Range;

fn fixture(name: &str) -> Vec<u8> {
    let dir = std::env::var("PERFR_TESTDATA").unwrap_or_else(|_| format!("{}/testdata", env!("CARGO_MANIFEST_DIR")));
    std::fs::read(format!("{dir}/{name}")).unwrap_or_else(|e| panic!("cannot read {dir}/{name}: {e}"))
}

/// xorshift64*: enough to scatter mutations, and the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn u32_at(data: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize)
}

fn u64_at(data: &[u8], at: usize) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?)).ok()
}

/// Where the fields are: the headers, the section table, and the first
/// bytes of every section (a dynamic table, a symbol table, a note, a
/// DWARF unit header, an export directory, the first RUNTIME_FUNCTIONs).
/// Most of a file is code and strings, and a mutation there tests nothing.
fn structure_of(data: &[u8]) -> Vec<Range<usize>> {
    const SECTION_HEAD: usize = 256;
    let mut hot = vec![0..data.len().min(0x400)];
    let mut section = |offset: usize, size: usize| {
        if offset < data.len() && size > 0 {
            hot.push(offset..data.len().min(offset + size.min(SECTION_HEAD)));
        }
    };
    if data.starts_with(b"\x7fELF") && data.get(4) == Some(&2) {
        // ELF64: e_shoff at 0x28, e_shentsize at 0x3a, e_shnum at 0x3c;
        // in a section header, sh_offset at 0x18 and sh_size at 0x20.
        if let (Some(table), Some(entry), Some(count)) = (u64_at(data, 0x28), u16_at(data, 0x3a), u16_at(data, 0x3c)) {
            for index in 0..count {
                let header = table + index * entry;
                if let (Some(offset), Some(size)) = (u64_at(data, header + 0x18), u64_at(data, header + 0x20)) {
                    section(offset, size);
                }
            }
            section(table, usize::MAX);
            hot.push(table.min(data.len())..data.len().min(table + count * entry));
        }
    } else if data.starts_with(b"MZ") {
        // PE: the section table follows the optional header; in an entry,
        // SizeOfRawData at 16 and PointerToRawData at 20.
        if let Some(pe) = u32_at(data, 0x3c) {
            if let (Some(count), Some(optional)) = (u16_at(data, pe + 6), u16_at(data, pe + 20)) {
                let table = pe + 24 + optional;
                for index in 0..count {
                    let header = table + index * 40;
                    if let (Some(size), Some(offset)) = (u32_at(data, header + 16), u32_at(data, header + 20)) {
                        section(offset, size);
                    }
                }
            }
        }
    }
    hot.retain(|range| !range.is_empty());
    hot
}

/// Overwrites one to four fields. A field is one, two, four or eight
/// bytes, and its new value is one of those that sums and bounds trip on.
fn mutate(good: &[u8], hot: &[Range<usize>], rng: &mut Rng) -> Vec<u8> {
    let mut data = good.to_vec();
    for _ in 0..1 + rng.below(4) {
        let width = [1usize, 2, 4, 8][rng.below(4)];
        let at = if rng.below(8) == 0 {
            rng.below(data.len())
        } else {
            let range = &hot[rng.below(hot.len())];
            range.start + rng.below(range.len())
        };
        let Some(field) = data.get_mut(at..at + width) else { continue };
        let mut current = [0u8; 8];
        current[..width].copy_from_slice(field);
        let current = u64::from_le_bytes(current);
        let value = match rng.below(8) {
            0 => 0,
            1 => u64::MAX,
            2 => u64::MAX >> 1,
            3 => 1 << (width * 8 - 1),
            4 => current.wrapping_add(1),
            5 => current.wrapping_sub(1),
            6 => current.wrapping_mul(2),
            _ => rng.next(),
        };
        field.copy_from_slice(&value.to_le_bytes()[..width]);
    }
    data
}

/// Says which round failed when a parser panics inside it.
struct Round<'a> {
    fixture: &'a str,
    round: usize,
}

impl Drop for Round<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("panicked on {} with the mutations of round {}", self.fixture, self.round);
        }
    }
}

fn survive(name: &str, rounds: usize, parse: impl Fn(&[u8])) {
    let good = fixture(name);
    let hot = structure_of(&good);
    // Seeded by the name, so adding a fixture does not reshuffle the others.
    let mut rng = Rng(name.bytes().fold(0x9e37_79b9_7f4a_7c15, |seed, byte| (seed ^ u64::from(byte)).rotate_left(9)));
    for round in 0..rounds {
        let _round = Round { fixture: name, round };
        parse(&mutate(&good, &hot, &mut rng));
    }
}

/// Everything the service does with a module that is an ELF file.
fn elf(data: &[u8]) {
    let _ = perfr_object::parse_elf_metadata(data, "mutated");
    let _ = perfr_object::load_symbols(data, perfr_object::SymbolTable::Debug);
    let _ = perfr_object::load_symbols(data, perfr_object::SymbolTable::Dynamic);
    let _ = perfr_object::load_unwind_ranges(data);
}

/// And with its DWARF, at an address inside the fixture's code.
fn dwarf(data: &[u8], address: u64) {
    if let Some(info) = perfr_object::DebugInfo::parse(data) {
        let _ = info.line_rows(address, address + 0x40);
        let _ = info.declaration_location(address);
        let _ = info.line_info(address);
    }
}

fn pe(data: &[u8]) {
    let _ = perfr_object::is_pe(data);
    let _ = perfr_object::parse_coff_metadata(data, "mutated");
    let _ = perfr_object::export_table_symbols(data);
    let _ = perfr_object::exception_table_symbols(data);
}

#[test]
fn elf_files_with_hostile_fields_are_errors_not_panics() {
    for name in ["hello_world_elf", "hello_world_elf_with_gnu_debuglink", "libtest-1.0.so", "no_symbols_elf", "debug_frame"] {
        survive(name, 1500, elf);
    }
    survive("hello_world_static_elf", 60, elf);
}

#[test]
fn dwarf_with_hostile_fields_is_no_line_info_not_a_panic() {
    survive("hello_world_elf_with_debug_info", 1500, |data| {
        elf(data);
        dwarf(data, 0x1140);
    });
    survive("line_info_test_binary", 1500, |data| dwarf(data, 0x401140));
    // Compressed sections: each round inflates them, so fewer rounds.
    survive("libc.debug", 24, |data| dwarf(data, 0x10a0e0));
}

#[test]
fn pe_images_with_hostile_fields_are_errors_not_panics() {
    survive("libtest.dll", 1500, pe);
    for name in ["exports_one_by_ordinal.dll", "no_export_table.exe"] {
        survive(name, 400, pe);
    }
    survive("dllmain.dll", 100, pe);
}

#[test]
fn a_pdb_with_hostile_fields_is_an_error_not_a_panic() {
    // The MSF superblock and the stream directory it points at are in the
    // first pages; the file is 9 MiB and every round reads all its symbols.
    survive("dllmain.pdb", 12, |data| {
        let _ = perfr_object::pdb_info(data);
        let _ = perfr_object::load_pdb_symbols(data, 0x1_8000_0000);
    });
}
