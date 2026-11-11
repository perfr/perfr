// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Symbol-table loading, replacing `ElfFileImpl::LoadDebugSymbols` and
//! `LoadSymbolsFromDynsym`.
//!
//! The filter is deliberately identical to `CreateSymbolInfo`: skip undefined
//! symbols, keep only `STT_FUNC`, and record address and size.

use object::elf;
use object::read::elf::{FileHeader, SectionHeader, Sym};
use object::Endianness;

/// Which symbol table to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolTable {
    /// `.symtab`, via `LoadDebugSymbols`.
    Debug,
    /// `.dynsym`, via `LoadSymbolsFromDynsym`.
    Dynamic,
}

/// One entry of `ModuleSymbols::symbol_infos`, before demangling.
///
/// The name is left as the linker wrote it. Demangling happens in the C++ shim
/// through `abi::__cxa_demangle`, which is libstdc++ rather than LLVM -- see
/// https://perfr.dev/blog/ for why the Rust demangler did not survive contact with real
/// symbols.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub mangled_name: String,
    pub address: u64,
    pub size: u64,
}

/// Reads one symbol table, keeping only defined function symbols.
///
/// The two error strings are `ElfFileImpl`'s, verbatim, because `ElfFileTest`
/// matches on them.
pub fn load_symbols(
    data: &[u8],
    table: SymbolTable,
) -> Result<Vec<Symbol>, String> {
    match object::FileKind::parse(data) {
        Ok(object::FileKind::Elf32) => {
            load_typed::<elf::FileHeader32<Endianness>>(data, table)
        }
        Ok(object::FileKind::Elf64) => {
            load_typed::<elf::FileHeader64<Endianness>>(data, table)
        }
        _ => Err("ELF file could not be parsed.".to_owned()),
    }
}

fn missing_section_error(table: SymbolTable) -> String {
    match table {
        SymbolTable::Debug => "ELF file does not have a .symtab section.".to_owned(),
        SymbolTable::Dynamic => "ELF file does not have a .dynsym section.".to_owned(),
    }
}

fn empty_result_error(table: SymbolTable) -> String {
    match table {
        SymbolTable::Debug => {
            "Unable to load symbols from ELF file: not even a single symbol of type function found."
                .to_owned()
        }
        SymbolTable::Dynamic => {
            "Unable to load symbols from .dynsym section: not even a single symbol of type \
             function found."
                .to_owned()
        }
    }
}

fn load_typed<Elf>(data: &[u8], table: SymbolTable) -> Result<Vec<Symbol>, String>
where
    Elf: FileHeader<Endian = Endianness>,
{
    let header = Elf::parse(data).map_err(|e| e.to_string())?;
    let endian = header.endian().map_err(|e| e.to_string())?;
    let sections = header
        .sections(endian, data)
        .map_err(|e| format!("Unable to load sections: {e}"))?;

    // `.symtab` is matched by name and `.dynsym` by type, exactly as
    // ElfFileImpl::InitSections decides has_symtab_section_/has_dynsym_section_.
    let symbols = match table {
        SymbolTable::Debug => {
            let mut found = None;
            for (index, section) in sections.iter().enumerate() {
                if sections.section_name(endian, section).ok() == Some(b".symtab") {
                    found = sections
                        .symbol_table_by_index(endian, data, object::read::SectionIndex(index))
                        .ok();
                    break;
                }
            }
            found
        }
        SymbolTable::Dynamic => {
            let mut found = None;
            for (index, section) in sections.iter().enumerate() {
                if section.sh_type(endian) == elf::SHT_DYNSYM {
                    found = sections
                        .symbol_table_by_index(endian, data, object::read::SectionIndex(index))
                        .ok();
                    break;
                }
            }
            found
        }
    };

    let Some(symbols) = symbols else {
        return Err(missing_section_error(table));
    };

    let mut result = Vec::new();
    for symbol in symbols.iter() {
        // SF_Undefined: defined in another object file.
        if symbol.st_shndx(endian) == elf::SHN_UNDEF {
            continue;
        }
        // Functions only. Sections, variables and files are ignored, which is
        // what llvm::object::SymbolRef::ST_Function selects for.
        if symbol.st_type() != elf::STT_FUNC {
            continue;
        }

        let name = symbols
            .symbol_name(endian, symbol)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let address: u64 = symbol.st_value(endian).into();

        result.push(Symbol {
            mangled_name: name,
            address,
            size: symbol.st_size(endian).into(),
        });
    }

    if result.is_empty() {
        return Err(empty_result_error(table));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn testdata(name: &str) -> Vec<u8> {
        let dir = std::env::var("PERFR_TESTDATA")
            .unwrap_or_else(|_| format!("{}/testdata", env!("CARGO_MANIFEST_DIR")));
        std::fs::read(format!("{dir}/{name}")).expect("testdata should be readable")
    }

    fn named<'a>(symbols: &'a [Symbol], name: &str) -> &'a Symbol {
        symbols.iter().find(|s| s.mangled_name == name).unwrap_or_else(|| panic!("no symbol {name}"))
    }

    #[test]
    fn the_symtab_gives_every_defined_function_with_its_address_and_size() {
        let symbols = load_symbols(&testdata("hello_world_elf"), SymbolTable::Debug).unwrap();
        assert_eq!(symbols.len(), 10);
        let main = named(&symbols, "main");
        assert_eq!((main.address, main.size), (0x1135, 35));
        // A size of zero is kept as the file has it, not guessed.
        let stub = named(&symbols, "deregister_tm_clones");
        assert_eq!((stub.address, stub.size), (0x1080, 0));

        let symbols = load_symbols(&testdata("hello_world_static_elf"), SymbolTable::Debug).unwrap();
        // 1125 of type STT_FUNC; the indirect ones (STT_GNU_IFUNC) name
        // resolvers, not code a sample lands in under that name.
        assert_eq!(symbols.len(), 1125);
        let fopen = named(&symbols, "fopen64");
        assert_eq!((fopen.address, fopen.size), (0x40f9a0, 10));
    }

    #[test]
    fn the_dynsym_gives_the_exported_functions_and_skips_the_imported_ones() {
        let data = testdata("libtest-1.0.so");
        let dynamic = load_symbols(&data, SymbolTable::Dynamic).unwrap();
        assert_eq!(dynamic.len(), 1);
        assert_eq!(
            (dynamic[0].mangled_name.as_str(), dynamic[0].address, dynamic[0].size),
            ("PrintHelloWorld", 0x1110, 12)
        );
        assert_eq!(load_symbols(&data, SymbolTable::Debug).unwrap().len(), 7);
    }

    #[test]
    fn a_missing_or_functionless_table_is_an_error_that_says_which() {
        let stripped = testdata("no_symbols_elf");
        assert_eq!(
            load_symbols(&stripped, SymbolTable::Debug).unwrap_err(),
            "ELF file does not have a .symtab section."
        );
        assert_eq!(
            load_symbols(&testdata("hello_world_static_elf"), SymbolTable::Dynamic).unwrap_err(),
            "ELF file does not have a .dynsym section."
        );
        assert_eq!(load_symbols(b"not an elf", SymbolTable::Debug).unwrap_err(), "ELF file could not be parsed.");
    }

    #[test]
    fn truncated_files_do_not_panic() {
        let good = testdata("hello_world_elf");
        let mut len = 1;
        while len < good.len() {
            let _ = load_symbols(&good[..len], SymbolTable::Debug);
            let _ = load_symbols(&good[..len], SymbolTable::Dynamic);
            len *= 2;
        }
    }
}
