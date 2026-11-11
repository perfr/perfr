// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Turning sampled program counters into function names.
//!
//! A callstack from the unwinder is a list of addresses; a flame graph needs
//! names. This maps an address back to the module that contains it (from the
//! process's `/proc/<pid>/maps`), then to the function containing it in that
//! module's ELF symbol table -- both readers already ported: `perfr-maps` for
//! the mappings and `perfr-object` for the symbols.
//!
//! Resolution degrades in steps rather than failing: a function name if the
//! symbol table has one, else `module+0x1234`, else the bare address. Every
//! frame gets *some* label, because a flame graph with holes is worse than
//! one with coarse labels.
//!
//! Where the names come from, in order: the module's detached debug file
//! (`/usr/lib/debug`, by build id or `.gnu_debuglink`), which is what turns
//! a stripped libc's internals into names; the module's own `.symtab`; its
//! `.dynsym`. The `[vdso]` is a module too: it has no file, but its image
//! is the same in every process of one kernel, so the service reads its own
//! and applies it at the target's address. That one mapping is where a
//! process that asks the time in a loop spends most of its samples.

use perfr_maps::parse_maps;
#[cfg(not(windows))]
use perfr_maps::{MemoryMapping, PROT_EXEC};
use perfr_object::{detached_debug_file, load_symbols, parse_elf_metadata, ObjectSegment, SymbolTable};
use perfr_object::{is_pe, parse_coff_metadata};
use std::sync::{Arc, Mutex};

use crate::functions::{file_offset_of, function_id};

/// What one file contributes: its loadable segments and its function
/// symbols sorted by address. Loaded once per file and shared by every
/// mapping of it, and kept across captures (see [`FILE_CACHE`]).
struct LoadedFile {
    segments: Vec<ObjectSegment>,
    /// Function symbols sorted by address, for binary search.
    symbols: Vec<(u64, u64, String)>,
    /// The virtual address of the file's first byte: the first PT_LOAD's
    /// vaddr minus its file offset for ELF, `ImageBase` for PE, whose
    /// headers are mapped there.
    image_base: u64,
}

/// One executable mapping, and the symbols of the file behind it.
struct Module {
    start: u64,
    end: u64,
    /// The load bias of the image this mapping is a piece of: subtracting
    /// it from a runtime address gives the file's *virtual* address, which
    /// is what the symbol table is keyed by. Not this mapping's own
    /// `start - offset`, nor that minus the image base: see
    /// [`perfr_maps::load_bias`] for the two ways that went wrong.
    bias: u64,
    name: String,
    /// Absolute path of the file, empty for the vDSO; with the loadable
    /// segments it turns a symbol's address into the file offset the
    /// function index keys hooks by.
    path: String,
    file: Arc<LoadedFile>,
}

impl Module {
    fn symbols(&self) -> &[(u64, u64, String)] {
        &self.file.symbols
    }
}

/// A file as the cache knows it: its path, and the size and modification
/// time that say whether the bytes on disk are still the ones loaded. The
/// vDSO has no file and one image per kernel, so its identity is its name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    key: String,
    len: u64,
    mtime_ns: u128,
}

impl FileIdentity {
    fn of(key: &str) -> FileIdentity {
        let meta = if key == VDSO { None } else { std::fs::metadata(key).ok() };
        FileIdentity {
            key: key.to_string(),
            len: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            mtime_ns: meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        }
    }
}

const VDSO: &str = "[vdso]";

/// The files of the last symbolizer built. A capture rebuilds its symbolizer
/// from scratch, and a game's 1 GB debug file is the same one the previous
/// capture of it read a minute ago: the next build takes each file it still
/// needs from here and reads only what is new or changed on disk. Replaced
/// wholesale on every build, so it never holds more than one process's
/// worth of symbols.
static FILE_CACHE: Mutex<Vec<(FileIdentity, Arc<LoadedFile>)>> = Mutex::new(Vec::new());

/// One executable mapping's coordinates, before its symbols are loaded: the
/// unit of work parallelised across [`crate::par_map`]. Plain data, so `Sync`.
struct ModuleSpec {
    start: u64,
    end: u64,
    bias: u64,
    name: String,
    path: String,
    vdso: bool,
}

impl ModuleSpec {
    /// What the file behind this mapping is keyed by in the cache.
    fn file_key(&self) -> String {
        if self.vdso { VDSO.to_string() } else { self.path.clone() }
    }
}

pub struct Symbolizer {
    modules: Vec<Module>,
}

/// A frame with everything the call trees display, and the id the
/// function index gives the function the address is in, so a report row
/// can be hooked (0 when the address is in no known function).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFrame {
    pub name: String,
    pub module: String,
    pub address: u64,
    pub function_id: u64,
}

impl Symbolizer {
    /// No modules: resolves every address to its hex form. For a capture
    /// without a target process.
    pub fn empty() -> Symbolizer {
        Symbolizer { modules: Vec::new() }
    }

    /// A symbolizer over hand-built modules, for tests and benchmarks.
    #[cfg(test)]
    pub(crate) fn from_parts(modules: Vec<(u64, u64, u64, String, Vec<(u64, u64, String)>)>) -> Symbolizer {
        Symbolizer {
            modules: modules
                .into_iter()
                .map(|(start, end, bias, name, symbols)| Module {
                    start,
                    end,
                    bias,
                    name,
                    path: String::new(),
                    file: Arc::new(LoadedFile { segments: Vec::new(), symbols, image_base: 0 }),
                })
                .collect(),
        }
    }

    /// Builds a symbolizer for a process from its module list (Toolhelp) and
    /// the PDB or export table of every module.
    #[cfg(windows)]
    pub fn for_pid(pid: i32) -> Symbolizer {
        let specs: Vec<ModuleSpec> = crate::windows::modules_of(pid.max(0) as u32)
            .into_iter()
            .map(|m| ModuleSpec {
                start: m.base,
                end: m.base.saturating_add(m.size),
                bias: m.base,
                name: basename(&m.path).to_string(),
                path: m.path,
                vdso: false,
            })
            .collect();
        let mut keys: Vec<String> = Vec::new();
        for spec in &specs {
            let key = spec.file_key();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let files = Self::load_files(&keys);
        let modules: Vec<Module> = specs
            .into_iter()
            .filter_map(|spec| {
                let file = files.iter().find(|(key, _)| *key == spec.file_key())?.1.clone();
                // Loaded at `start`, linked at `image_base`: the difference
                // is what turns a runtime address into the PDB's.
                let bias = spec.start.wrapping_sub(file.image_base);
                Some(Module { start: spec.start, end: spec.end, bias, name: spec.name, path: spec.path, file })
            })
            .collect();
        Symbolizer { modules }
    }

    /// Builds a symbolizer for a process by reading its maps and loading the
    /// symbol table of every executable file mapped into it.
    #[cfg(not(windows))]
    pub fn for_pid(pid: i32) -> Symbolizer {
        let Ok(content) = std::fs::read(format!("/proc/{pid}/maps")) else {
            return Symbolizer { modules: Vec::new() };
        };
        Symbolizer { modules: Self::modules_of(&parse_maps(&content), Self::load_files) }
    }

    /// The modules of a process with these mappings; `load` is where the
    /// files behind them come from ([`Self::load_files`], or a test's own).
    #[cfg(not(windows))]
    fn modules_of(
        mappings: &[MemoryMapping],
        load: impl FnOnce(&[String]) -> Vec<(String, Arc<LoadedFile>)>,
    ) -> Vec<Module> {
        // One spec per executable mapping (the resolver keeps a module per
        // mapping, each with its own bias), then load the symbols in parallel:
        // a big split debug file dominates and the modules are independent.
        let mut specs: Vec<ModuleSpec> = Vec::new();
        // The mapping each spec was made from, for its bias.
        let mut mapped: Vec<&MemoryMapping> = Vec::new();
        for mapping in mappings {
            if mapping.perms & PROT_EXEC == 0 {
                continue;
            }
            let Ok(path) = std::str::from_utf8(&mapping.pathname) else { continue };
            if path == "[vdso]" {
                mapped.push(mapping);
                specs.push(ModuleSpec {
                    start: mapping.start_address,
                    end: mapping.end_address,
                    bias: mapping.start_address,
                    name: "[vdso]".to_string(),
                    path: String::new(),
                    vdso: true,
                });
                continue;
            }
            if path.is_empty() || !path.starts_with('/') {
                continue;
            }
            mapped.push(mapping);
            specs.push(ModuleSpec {
                start: mapping.start_address,
                end: mapping.end_address,
                bias: mapping.start_address.wrapping_sub(mapping.offset),
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                path: path.to_string(),
                vdso: false,
            });
        }
        // One load per *file*, shared by every mapping of it. A file is
        // usually one executable mapping, but not always: a hook engine that
        // patches function entries makes their pages writable one at a time,
        // and every patched page splits the text mapping in two. Fourteen
        // hooks in an Unreal game turned its one text mapping into fifteen,
        // and a load per mapping read the 1 GB debug file fifteen times over
        // -- 8 million symbols in memory for 600 thousand distinct ones, and
        // the samples held until the load finished arrived seconds late.
        let mut keys: Vec<String> = Vec::new();
        for spec in &specs {
            let key = spec.file_key();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        let files = load(&keys);
        specs
            .into_iter()
            .zip(mapped)
            .filter_map(|(spec, mapping)| {
                let file = files.iter().find(|(key, _)| *key == spec.file_key())?.1.clone();
                // The vDSO's bias is its mapping's start. A file's is its
                // image's, which takes the file's other mappings to find:
                // this one's `start - offset` is a page or more above it
                // for anything lld linked (see `perfr_maps::load_bias`).
                let bias = if spec.vdso { spec.bias } else { file_load_bias(mappings, mapping, &file) };
                Some(Module { start: spec.start, end: spec.end, bias, name: spec.name, path: spec.path, file })
            })
            .collect()
    }

    /// The symbol tables of `keys` (file paths, or [`VDSO`]): from the cache
    /// where the file on disk is unchanged since it was read, loaded in
    /// parallel otherwise. The cache is left holding exactly these.
    fn load_files(keys: &[String]) -> Vec<(String, Arc<LoadedFile>)> {
        let identities: Vec<FileIdentity> = keys.iter().map(|key| FileIdentity::of(key)).collect();
        let previous = std::mem::take(&mut *FILE_CACHE.lock().unwrap_or_else(|e| e.into_inner()));
        let mut files: Vec<(FileIdentity, Arc<LoadedFile>)> = Vec::with_capacity(identities.len());
        let mut missing: Vec<FileIdentity> = Vec::new();
        for identity in identities {
            match previous.iter().find(|(known, _)| *known == identity) {
                Some((_, file)) => files.push((identity, file.clone())),
                None => missing.push(identity),
            }
        }
        // One scope around the whole load: it launches the workers and blocks
        // here until they return, so its span is the total symbol-loading time.
        // The per-file "load symbols: <file>" scopes run on the workers under it.
        if !missing.is_empty() {
            let _total = perfr_api::scope(format!(
                "load symbols ({} files, {} cached)",
                missing.len(),
                files.len()
            ));
            // `par_map` hands results back in completion order, so each
            // carries the identity it was loaded for.
            files.extend(crate::par_map(&missing, |identity| {
                (identity.clone(), Arc::new(Self::load_file(&identity.key)))
            }));
        }
        *FILE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = files.clone();
        files.into_iter().map(|(identity, file)| (identity.key, file)).collect()
    }

    /// Loads one file's symbols, on a worker thread. A self-profile scope
    /// names the file so the cost of each load shows on the service's track.
    fn load_file(key: &str) -> LoadedFile {
        let name = basename(key);
        let _load = perfr_api::scope(format!("load symbols: {name}"));
        if key == VDSO {
            let symbols = vdso_image().map(|image| sorted_symbols(&image, None)).unwrap_or_default();
            return LoadedFile { segments: Vec::new(), symbols, image_base: 0 };
        }
        let bytes = map_file(key);
        let symbols = sorted_symbols(&bytes, Some(key));
        let (segments, image_base) = module_layout(&bytes, key);
        LoadedFile { segments, symbols, image_base }
    }

    /// Executable mappings, one per mapping in the process.
    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    /// Distinct files behind the mappings.
    pub fn file_count(&self) -> usize {
        let mut seen: Vec<*const LoadedFile> = Vec::new();
        for module in &self.modules {
            let ptr = Arc::as_ptr(&module.file);
            if !seen.contains(&ptr) {
                seen.push(ptr);
            }
        }
        seen.len()
    }

    /// Symbols loaded, each file counted once however many times it is mapped.
    pub fn symbol_count(&self) -> usize {
        let mut seen: Vec<*const LoadedFile> = Vec::new();
        let mut total = 0;
        for module in &self.modules {
            let ptr = Arc::as_ptr(&module.file);
            if !seen.contains(&ptr) {
                seen.push(ptr);
                total += module.file.symbols.len();
            }
        }
        total
    }

    /// The best label available for an address, with the module it came from
    /// and the address itself. The call trees show all three: a name alone
    /// cannot tell two same-named static functions apart, and the address is
    /// what you paste into a disassembler.
    pub fn resolve_frame(&self, address: u64) -> ResolvedFrame {
        let module = self
            .modules
            .iter()
            .find(|module| address >= module.start && address < module.end);
        ResolvedFrame {
            name: self.resolve(address),
            module: module.map(|m| m.name.clone()).unwrap_or_default(),
            address,
            function_id: module.map(|m| m.function_id_at(address)).unwrap_or(0),
        }
    }

    /// The best label available for an address.
    pub fn resolve(&self, address: u64) -> String {
        let Some(module) = self
            .modules
            .iter()
            .find(|module| address >= module.start && address < module.end)
        else {
            return format!("{address:#x}");
        };
        // Addresses in the file are the runtime address minus the load bias.
        let file_address = address.wrapping_sub(module.bias);
        if let Some(name) = find_symbol(module.symbols(), file_address) {
            return demangle(name);
        }
        format!("{}+{:#x}", module.name, file_address)
    }
}

impl Module {
    /// The function index's id for the function containing `address`: the
    /// hash of the module path and the symbol's file offset, the same
    /// arithmetic `FunctionIndex::for_pid` does, so a report row and a
    /// search hit for one function agree. 0 for the vDSO and for addresses
    /// outside any symbol.
    fn function_id_at(&self, address: u64) -> u64 {
        if self.path.is_empty() {
            return 0;
        }
        let file_address = address.wrapping_sub(self.bias);
        let Some((start, _, _)) = symbol_at(self.symbols(), file_address) else {
            return 0;
        };
        file_offset_of(&self.file.segments, *start)
            .map(|offset| function_id(&self.path, offset))
            .unwrap_or(0)
    }
}

/// The last path component, whichever separator the OS uses.
pub(crate) fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A module file's loadable segments (ELF) or sections (PE), and the
/// virtual address its symbols are keyed from.
pub(crate) fn module_layout(bytes: &[u8], path: &str) -> (Vec<ObjectSegment>, u64) {
    if is_pe(bytes) {
        return parse_coff_metadata(bytes, path).map(|m| (m.sections, m.image_base)).unwrap_or_default();
    }
    let segments = parse_elf_metadata(bytes, path).map(|m| m.loadable_segments).unwrap_or_default();
    let base = image_base(&segments);
    (segments, base)
}

/// The load bias of the image of `file` that `mapping` is a piece of.
#[cfg(not(windows))]
fn file_load_bias(mappings: &[MemoryMapping], mapping: &MemoryMapping, file: &LoadedFile) -> u64 {
    // File offset 0 is at `image_base` in both formats -- an ELF's first
    // PT_LOAD says so, a PE's headers are mapped there -- and the segments
    // (sections, for PE) say how much further from its offset each of the
    // file's other mappings can be.
    let layout: Vec<(u64, u64)> = std::iter::once((0, file.image_base))
        .chain(file.segments.iter().map(|segment| (segment.offset_in_file, segment.address)))
        .collect();
    perfr_maps::load_bias(mappings, mapping, &layout)
}

/// The first PT_LOAD's virtual address minus its file offset: where the
/// file's virtual address space starts relative to its bytes. Zero for the
/// usual layout, 0x200000 for a binary linked with an image base (Unreal).
fn image_base(segments: &[ObjectSegment]) -> u64 {
    segments
        .iter()
        .min_by_key(|segment| segment.offset_in_file)
        .map(|segment| segment.address.wrapping_sub(segment.offset_in_file))
        .unwrap_or(0)
}

/// The name of the symbol `address` is in; see [`symbol_at`].
fn find_symbol(symbols: &[(u64, u64, String)], address: u64) -> Option<&str> {
    symbol_at(symbols, address).map(|(_, _, name)| name.as_str())
}

/// How many symbols before the last one starting at or below an address
/// are also asked whether they contain it.
const ENCLOSING_SYMBOLS: usize = 4;

/// The symbol containing `address`, of a table in [`sort_symbols`] order:
/// the last one starting at or before the address when the address falls
/// inside it, else the nearest before that one which reaches the address.
/// Sizes of zero are common (assembly stubs), so a zero-sized symbol only
/// matches its exact address.
///
/// The step back is for hand-written assembly, where a label with its own
/// symbol sits inside a function: an address past the label's short extent
/// is still in the function around it. It is a few entries, not a search,
/// because a symbol further back that reached this far would overlap
/// everything in between.
fn symbol_at(symbols: &[(u64, u64, String)], address: u64) -> Option<&(u64, u64, String)> {
    let index = symbols.partition_point(|(start, _, _)| *start <= address);
    symbols[..index].iter().rev().take(1 + ENCLOSING_SYMBOLS).find(|(start, size, _)| {
        if *size == 0 {
            *start == address
        } else {
            // Not `address < start + size`: the size is the file's
            // `st_size`, and the sum of a hostile one overflows.
            // `start <= address` here.
            address - start < *size
        }
    })
}

/// Rust and Itanium C++ names made readable, lazily and memoized; see
/// `demangle.rs` for the rule (the C++ shims that once did this with
/// `__cxa_demangle` are gone with LLVM, so this is the only demangler).
fn demangle(name: &str) -> String {
    crate::demangle::pretty_cached(name)
}

/// The function symbols of one ELF image, sorted by address: from its
/// detached debug file when `path` names a file that has one, else its own
/// `.symtab`, else its `.dynsym`.
pub(crate) fn symbol_source(bytes: &[u8], path: Option<&str>) -> Result<Vec<perfr_object::Symbol>, String> {
    if is_pe(bytes) {
        return pe_symbols(bytes, path.unwrap_or_default());
    }
    if let Some(path) = path {
        if let Ok(metadata) = parse_elf_metadata(bytes, path) {
            if let Some(debug) = detached_debug_file(std::path::Path::new(path), &metadata) {
                // Mapped, not read: an Unreal Shipping build's .debug is a
                // gigabyte, of which the symbol and string tables are a few
                // tens of megabytes. Reading all of it was most of the
                // seconds between Record and the first sample on screen.
                let debug_bytes = map_file(debug.to_str().unwrap_or_default());
                if let Ok(symbols) = load_symbols(&debug_bytes, SymbolTable::Debug) {
                    return Ok(symbols);
                }
            }
        }
    }
    load_symbols(bytes, SymbolTable::Debug).or_else(|_| load_symbols(bytes, SymbolTable::Dynamic))
}

/// The function symbols of a PE image: its PDB when one can be found and
/// matches, plus the export table, plus the exception table's function
/// ranges (`[function@0x...]`) for every function neither named -- the
/// same three sources C++ perfr's `CoffFile` read, merged by address with
/// the named entry winning.
fn pe_symbols(bytes: &[u8], path: &str) -> Result<Vec<perfr_object::Symbol>, String> {
    let metadata = parse_coff_metadata(bytes, path)?;
    let mut by_address: std::collections::BTreeMap<u64, perfr_object::Symbol> = std::collections::BTreeMap::new();
    let mut add = |symbols: Vec<perfr_object::Symbol>, named: bool| {
        for symbol in symbols {
            if symbol.address == 0 || symbol.mangled_name.is_empty() {
                continue;
            }
            match by_address.get_mut(&symbol.address) {
                // Keep a name over a range, and a known size over none.
                Some(existing) => {
                    if named && existing.mangled_name.starts_with("[function@") {
                        let size = if existing.size != 0 && existing.size != perfr_object::UNKNOWN_SYMBOL_SIZE { existing.size } else { symbol.size };
                        *existing = perfr_object::Symbol { size, ..symbol };
                    } else if (existing.size == 0 || existing.size == perfr_object::UNKNOWN_SYMBOL_SIZE) && symbol.size != 0 {
                        existing.size = symbol.size;
                    }
                }
                None => {
                    by_address.insert(symbol.address, symbol);
                }
            }
        }
    };
    if let Some(info) = &metadata.pdb_debug_info {
        if let Some(pdb_path) = find_pdb(path, info) {
            let pdb_bytes = map_file(&pdb_path);
            match perfr_object::load_pdb_symbols(&pdb_bytes, metadata.image_base) {
                Ok(symbols) => add(symbols, true),
                Err(error) => log::debug!("{pdb_path}: {error}"),
            }
        }
    }
    if let Ok(exports) = perfr_object::export_table_symbols(bytes) {
        add(exports, true);
    }
    if let Ok(ranges) = perfr_object::exception_table_symbols(bytes) {
        add(ranges, false);
    }
    let mut symbols: Vec<perfr_object::Symbol> = by_address.into_values().collect();
    for symbol in &mut symbols {
        if symbol.size == perfr_object::UNKNOWN_SYMBOL_SIZE {
            symbol.size = 0;
        }
    }
    Ok(symbols)
}

/// Where a module's PDB is: the path its CodeView record names, else a
/// file of that name beside the module, else the module's own name with
/// `.pdb`; each candidate must carry the record's GUID and age.
pub(crate) fn find_pdb(module_path: &str, info: &perfr_object::PdbDebugInfo) -> Option<String> {
    let module = std::path::Path::new(module_path);
    let recorded = std::path::PathBuf::from(&info.pdb_file_path);
    let mut candidates: Vec<std::path::PathBuf> = vec![recorded.clone()];
    if let Some(dir) = module.parent() {
        if let Some(name) = recorded.file_name() {
            candidates.push(dir.join(name));
        }
        candidates.push(module.with_extension("pdb"));
    }
    if let Some(cache) = std::env::var_os("PERFR_PDB_DIR") {
        if let Some(name) = recorded.file_name() {
            candidates.push(std::path::PathBuf::from(cache).join(name));
        }
    }
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let path = candidate.to_string_lossy().into_owned();
        let bytes = map_file(&path);
        match perfr_object::pdb_info(&bytes) {
            Ok(found) if found.guid == info.guid && found.age == info.age => return Some(path),
            Ok(found) => log::debug!("{path}: GUID/age {:x?}/{} do not match the module's {:x?}/{}", found.guid, found.age, info.guid, info.age),
            Err(error) => log::debug!("{path}: {error}"),
        }
    }
    None
}

/// A file's bytes as a read-only mapping, so parsing a large image touches
/// only the pages it needs; an empty buffer when it cannot be opened, which
/// every parser here treats as "no symbols".
pub(crate) fn map_file(path: &str) -> FileBytes {
    try_map_file(path).unwrap_or(FileBytes::Empty)
}

/// [`map_file`] for a caller that says why a file could not be read.
pub(crate) fn try_map_file(path: &str) -> std::io::Result<FileBytes> {
    let file = std::fs::File::open(path)?;
    // An empty file reads as no bytes; mapping nothing is an error instead.
    if file.metadata()?.len() == 0 {
        return Ok(FileBytes::Empty);
    }
    // SAFETY: the mapping is private and read-only. A file that changes
    // underneath a mapping can yield torn bytes; the parsers bounds-check
    // every read and treat garbage as an unparsable image, never as memory
    // unsafety, and the files here are the target's own executables and
    // their debug files, which nothing writes while it runs.
    unsafe { memmap2::Mmap::map(&file) }.map(FileBytes::Mapped)
}

pub(crate) enum FileBytes {
    Mapped(memmap2::Mmap),
    Empty,
}

impl std::ops::Deref for FileBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FileBytes::Mapped(map) => &map[..],
            FileBytes::Empty => &[],
        }
    }
}

fn sorted_symbols(bytes: &[u8], path: Option<&str>) -> Vec<(u64, u64, String)> {
    let mut symbols: Vec<(u64, u64, String)> = symbol_source(bytes, path)
        .unwrap_or_default()
        .into_iter()
        .filter(|symbol| symbol.address != 0)
        .map(|symbol| (symbol.address, symbol.size, symbol.mangled_name))
        .collect();
    sort_symbols(&mut symbols);
    symbols
}

/// By address, and by size where several symbols share one: the lookup
/// asks the last symbol at or below an address first, and of two names for
/// one entry point that has to be the one with the extent. A zero-sized
/// alias sorted after the function it labels matched only the entry
/// address and left the rest of the function without a name. Symbols equal
/// in both keep the file's order.
fn sort_symbols(symbols: &mut [(u64, u64, String)]) {
    symbols.sort_by_key(|(address, size, _)| (*address, *size));
}

/// This process's vDSO image, copied out of its own mapping. Every process
/// on one kernel maps the same image, so its symbol table serves the
/// target's `[vdso]` at the target's address.
fn vdso_image() -> Option<Vec<u8>> {
    let content = std::fs::read("/proc/self/maps").ok()?;
    let mapping = parse_maps(&content)
        .into_iter()
        .find(|m| m.pathname.as_slice() == b"[vdso]")?;
    let len = mapping.end_address.checked_sub(mapping.start_address)? as usize;
    // SAFETY: the mapping is this process's own vDSO, readable for the life
    // of the process; the kernel never unmaps it.
    let bytes = unsafe { std::slice::from_raw_parts(mapping.start_address as *const u8, len) };
    Some(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols() -> Vec<(u64, u64, String)> {
        vec![
            (0x1000, 0x100, "alpha".to_string()),
            (0x2000, 0, "stub".to_string()),
            (0x3000, 0x50, "beta".to_string()),
        ]
    }

    #[test]
    fn an_address_inside_a_symbol_resolves_to_it() {
        assert_eq!(find_symbol(&symbols(), 0x1000), Some("alpha"));
        assert_eq!(find_symbol(&symbols(), 0x10ff), Some("alpha"));
        assert_eq!(find_symbol(&symbols(), 0x3010), Some("beta"));
    }

    #[test]
    fn an_address_past_a_symbols_end_does_not_resolve_to_it() {
        // 0x1100 is one past alpha, and before the next symbol.
        assert_eq!(find_symbol(&symbols(), 0x1100), None);
        assert_eq!(find_symbol(&symbols(), 0x3050), None);
    }

    #[test]
    fn zero_sized_symbols_match_only_exactly() {
        assert_eq!(find_symbol(&symbols(), 0x2000), Some("stub"));
        assert_eq!(find_symbol(&symbols(), 0x2001), None);
    }

    #[test]
    fn a_symbol_whose_size_runs_past_the_address_space_still_contains_its_addresses() {
        // Sizes come straight from `st_size`; start + size must not be
        // computed, it need not fit.
        let symbols = vec![(0x1000, 0x100, "alpha".to_string()), (0x4000, u64::MAX, "endless".to_string())];
        assert_eq!(find_symbol(&symbols, 0x4000), Some("endless"));
        assert_eq!(find_symbol(&symbols, 0x9000_0000), Some("endless"));
        assert_eq!(find_symbol(&symbols, 0x3fff), None);
        // The function id asks the same question of a module with a path;
        // 0 because no segment says where in the file the symbol is.
        let file = Arc::new(LoadedFile { segments: Vec::new(), symbols, image_base: 0 });
        let module = Module { start: 0, end: u64::MAX, bias: 0, name: "x".into(), path: "/t/x".into(), file };
        let frame = Symbolizer { modules: vec![module] }.resolve_frame(0x5000);
        assert_eq!((frame.name.as_str(), frame.function_id), ("endless", 0));
    }

    /// The file's order, as `load_symbols` hands the table over.
    fn sorted(mut symbols: Vec<(u64, u64, String)>) -> Vec<(u64, u64, String)> {
        sort_symbols(&mut symbols);
        symbols
    }

    #[test]
    fn a_zero_sized_alias_does_not_hide_the_function_it_labels() {
        // An assembly entry label after the function it is the start of:
        // sorted by address alone, the label stayed last and was the only
        // candidate for every address in the function.
        for order in [[0usize, 1], [1, 0]] {
            let table = [(0x1000u64, 0x100u64, "copy".to_string()), (0x1000, 0, "copy_entry".to_string())];
            let symbols = sorted(order.iter().map(|&i| table[i].clone()).collect());
            assert_eq!(find_symbol(&symbols, 0x1080), Some("copy"), "file order {order:?}");
            assert_eq!(find_symbol(&symbols, 0x1000), Some("copy"), "file order {order:?}");
            assert_eq!(find_symbol(&symbols, 0x1100), None);
        }
    }

    #[test]
    fn an_address_past_a_nested_symbol_falls_back_to_the_one_around_it() {
        let symbols = sorted(vec![
            (0x1000, 0x100, "outer".to_string()),
            (0x1010, 0x10, "inner".to_string()),
            (0x3000, 0x50, "beta".to_string()),
        ]);
        assert_eq!(find_symbol(&symbols, 0x1018), Some("inner"));
        assert_eq!(find_symbol(&symbols, 0x1050), Some("outer"));
        assert_eq!(find_symbol(&symbols, 0x1100), None);
        // Stepping back is for an enclosing symbol, not for the previous
        // function: beta's neighbour does not reach it.
        assert_eq!(find_symbol(&symbols, 0x2fff), None);
        // The function id agrees with the name.
        let segments = vec![ObjectSegment { offset_in_file: 0, size_in_file: 0x4000, address: 0, size_in_memory: 0x4000 }];
        let file = Arc::new(LoadedFile { segments, symbols, image_base: 0 });
        let module = Module { start: 0, end: 0x4000, bias: 0, name: "x".into(), path: "/t/x".into(), file };
        assert_eq!(module.function_id_at(0x1050), function_id("/t/x", 0x1000));
        assert_eq!(module.function_id_at(0x1018), function_id("/t/x", 0x1010));
        assert_eq!(module.function_id_at(0x1100), 0);
    }

    #[test]
    fn an_address_below_everything_resolves_to_nothing() {
        assert_eq!(find_symbol(&symbols(), 0x0), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_symbolizes_its_own_code() {
        let started = std::time::Instant::now();
        let symbolizer = Symbolizer::for_pid(std::process::id() as i32);
        log::info!(
            "SYMBOLIZER_LOAD modules={} symbols={} ms={:.1}",
            symbolizer.module_count(),
            symbolizer.symbol_count(),
            started.elapsed().as_secs_f64() * 1e3
        );
        assert!(symbolizer.module_count() > 0, "no executable modules found");
        // Our own text address must land in a module, so it must not come
        // back as a bare hex address.
        let here = this_process_symbolizes_its_own_code as usize as u64;
        let label = symbolizer.resolve(here);
        assert!(!label.starts_with("0x"), "unresolved: {label}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_vdso_is_a_module_with_names() {
        let symbolizer = Symbolizer::for_pid(std::process::id() as i32);
        let vdso = symbolizer.modules.iter().find(|m| m.name == "[vdso]").expect("a [vdso] module");
        assert!(!vdso.symbols().is_empty(), "the vDSO image has a dynamic symbol table");
        // clock_gettime is what a program asking the time in a loop samples in.
        let (offset, _, name) = vdso
            .symbols()
            .iter()
            .find(|(_, _, n)| n.contains("clock_gettime"))
            .expect("clock_gettime in the vDSO");
        let frame = symbolizer.resolve_frame(vdso.start + offset);
        // `clock_gettime` and `__vdso_clock_gettime` alias one address;
        // either name is the right answer.
        assert!(frame.name.contains("clock_gettime"), "{} resolved to {}", name, frame.name);
        assert_eq!(frame.module, "[vdso]");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stripped_libc_gets_its_internals_from_the_detached_debug_file() {
        let symbolizer = Symbolizer::for_pid(std::process::id() as i32);
        let Some(libc) = symbolizer.modules.iter().find(|m| m.name.starts_with("libc.so")) else {
            return; // a static test binary: nothing to check
        };
        let path = format!("/usr/lib/x86_64-linux-gnu/{}", libc.name);
        let bytes = std::fs::read(&path).unwrap_or_default();
        let has_debug_file = parse_elf_metadata(&bytes, &path)
            .ok()
            .and_then(|m| detached_debug_file(std::path::Path::new(&path), &m))
            .is_some();
        let dynsym = load_symbols(&bytes, SymbolTable::Dynamic).map(|s| s.len()).unwrap_or(0);
        if has_debug_file {
            assert!(
                libc.symbols().len() > dynsym,
                "with libc6-dbg installed the module has more than its {dynsym} exported names, got {}",
                libc.symbols().len()
            );
        } else {
            assert_eq!(libc.symbols().len(), dynsym, "without a debug file the dynamic table is all there is");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_frames_function_id_is_the_function_indexs_id_for_it() {
        let pid = std::process::id() as i32;
        let symbolizer = Symbolizer::for_pid(pid);
        let here = a_frames_function_id_is_the_function_indexs_id_for_it as usize as u64;
        let frame = symbolizer.resolve_frame(here + 3);
        assert_ne!(frame.function_id, 0, "our own code has a function id: {frame:?}");
        let index = crate::functions::FunctionIndex::for_pid(pid);
        let hit = index.by_id(frame.function_id).expect("the id is one the function index knows");
        assert!(
            hit.name.contains("a_frames_function_id_is_the_function_indexs_id_for_it"),
            "index names it {}",
            hit.name
        );
        // A hand-built module has no path: nothing to hook.
        let sym = Symbolizer::from_parts(vec![(0x1000, 0x2000, 0x1000, "x".into(), symbols())]);
        assert_eq!(sym.resolve_frame(0x1010).function_id, 0);
    }

    /// One linked file as a process maps it: its `/proc/<pid>/maps` lines,
    /// its PT_LOADs as `(offset, vaddr, file size)`, where it was loaded, and
    /// a function of it as `(vaddr, size, file offset)`.
    #[cfg(not(windows))]
    struct Layout {
        maps: &'static str,
        path: &'static str,
        segments: &'static [(u64, u64, u64)],
        bias: u64,
        function: (u64, u64, u64),
    }

    #[cfg(not(windows))]
    impl Layout {
        /// The modules the symbolizer builds for it, the file holding the
        /// function and the function that sits a page below it.
        fn modules(&self) -> Vec<Module> {
            let (address, size, _) = self.function;
            let segments: Vec<ObjectSegment> = self
                .segments
                .iter()
                .map(|&(offset_in_file, address, size)| ObjectSegment {
                    offset_in_file,
                    size_in_file: size,
                    address,
                    size_in_memory: size,
                })
                .collect();
            let file = Arc::new(LoadedFile {
                image_base: image_base(&segments),
                segments,
                symbols: vec![(address - 0x1000, size, "a_page_below".to_string()), (address, size, "sampled".to_string())],
            });
            Symbolizer::modules_of(&parse_maps(self.maps.as_bytes()), |keys| {
                assert_eq!(keys, [self.path.to_string()]);
                vec![(self.path.to_string(), file)]
            })
        }

        /// Every module has the file's bias, and an address in the function
        /// gets the function's name and the id the function index gives it.
        fn check(&self) {
            let symbolizer = Symbolizer { modules: self.modules() };
            let (address, _, file_offset) = self.function;
            let frame = symbolizer.resolve_frame(self.bias + address + 8);
            assert_eq!(frame.name, "sampled", "{}", self.path);
            assert_eq!(frame.function_id, function_id(self.path, file_offset), "{}", self.path);
            for module in &symbolizer.modules {
                assert_eq!(module.bias, self.bias, "{}: bias of the mapping at {:#x}", self.path, module.start);
            }
        }
    }

    // The four layouts are one C file (forty padding functions, so a page
    // below `pad_39` is another function) linked by GNU ld and by lld, as
    // PIE and not: program headers from `readelf -lW`, mappings from
    // `/proc/<pid>/maps` of the running binary, `pad_39` from `nm -S`.

    #[cfg(not(windows))]
    #[test]
    fn a_gnu_ld_binary_resolves_with_the_bias_of_its_text_mapping() {
        // GNU ld pads the file so text has the headers' `vaddr - offset`.
        Layout {
            maps: "\
60c4121b0000-60c4121b1000 r--p 00000000 00:2f 169005                     /t/hot_bfd
60c4121b1000-60c4121b3000 r-xp 00001000 00:2f 169005                     /t/hot_bfd
60c4121b3000-60c4121b4000 r--p 00003000 00:2f 169005                     /t/hot_bfd
60c4121b4000-60c4121b5000 r--p 00003000 00:2f 169005                     /t/hot_bfd
60c4121b5000-60c4121b6000 rw-p 00004000 00:2f 169005                     /t/hot_bfd
",
            path: "/t/hot_bfd",
            segments: &[(0, 0, 0x710), (0x1000, 0x1000, 0x1705), (0x3000, 0x3000, 0x5c8), (0x3da0, 0x4da0, 0x270)],
            bias: 0x60c4121b0000,
            function: (0x2600, 0x30, 0x2600),
        }
        .check();
        // Not PIE: linked at 0x400000 and loaded there.
        Layout {
            maps: "\
00400000-00401000 r--p 00000000 00:2f 189223                             /t/hot_bfd_nopie
00401000-00402000 r-xp 00001000 00:2f 189223                             /t/hot_bfd_nopie
00402000-00403000 r--p 00002000 00:2f 189223                             /t/hot_bfd_nopie
00403000-00404000 r--p 00002000 00:2f 189223                             /t/hot_bfd_nopie
00404000-00405000 rw-p 00003000 00:2f 189223                             /t/hot_bfd_nopie
",
            path: "/t/hot_bfd_nopie",
            segments: &[(0, 0x400000, 0x5e8), (0x1000, 0x401000, 0x9d5), (0x2000, 0x402000, 0x5c0), (0x2df8, 0x403df8, 0x238)],
            bias: 0,
            function: (0x401920, 0x30, 0x1920),
        }
        .check();
    }

    #[cfg(not(windows))]
    #[test]
    fn an_lld_binary_resolves_with_the_bias_of_its_image_not_of_its_text_mapping() {
        // lld does not pad: text is at offset 0xc80 and vaddr 0x1c80, so
        // its mapping's `start - offset` is a page above the image's start.
        // With that as the bias, a sample in `pad_39` (0x3180) was named
        // after `pad_07` (0x2180) and carried pad_07's function id.
        Layout {
            maps: "\
5992b139a000-5992b139b000 r--p 00000000 00:2f 169012                     /t/hot_lld
5992b139b000-5992b139e000 r-xp 00000000 00:2f 169012                     /t/hot_lld
5992b139e000-5992b139f000 r--p 00002000 00:2f 169012                     /t/hot_lld
5992b139f000-5992b13a0000 rw-p 00002000 00:2f 169012                     /t/hot_lld
",
            path: "/t/hot_lld",
            segments: &[(0, 0, 0xc30), (0xc80, 0x1c80, 0x16e0), (0x2360, 0x4360, 0x228), (0x2588, 0x5588, 0x10)],
            bias: 0x5992b139a000,
            function: (0x3180, 0x30, 0x2180),
        }
        .check();
        // Not PIE: lld's default image base, 0x200000, and the same bump.
        Layout {
            maps: "\
00200000-00201000 r--p 00000000 00:2f 189230                             /t/hot_lld_nopie
00201000-00203000 r-xp 00000000 00:2f 189230                             /t/hot_lld_nopie
00203000-00204000 r--p 00001000 00:2f 189230                             /t/hot_lld_nopie
00204000-00205000 rw-p 00001000 00:2f 189230                             /t/hot_lld_nopie
",
            path: "/t/hot_lld_nopie",
            segments: &[(0, 0x200000, 0xb80), (0xb80, 0x201b80, 0x9e0), (0x1560, 0x203560, 0x1a0), (0x1700, 0x204700, 0x48)],
            bias: 0,
            function: (0x2023f0, 0x30, 0x13f0),
        }
        .check();
        // An Unreal Shipping binary's program headers: first PT_LOAD at
        // 0x200000, text 0x201000 from its file offset. (The mappings are
        // written out from the headers, not copied from a process.)
        Layout {
            maps: "\
00200000-03281000 r--p 00000000 103:02 77                                /t/Game-Linux-Shipping
03282000-0b22a000 r-xp 03081000 103:02 77                                /t/Game-Linux-Shipping
0b22b000-0b6f0000 rw-p 0b029000 103:02 77                                /t/Game-Linux-Shipping
",
            path: "/t/Game-Linux-Shipping",
            segments: &[(0, 0x200000, 0x3080200), (0x3081000, 0x3282000, 0x7fa8000), (0xb029ec0, 0xb22bec0, 0x4c4000)],
            bias: 0,
            function: (0x3283000, 0x40, 0x3082000),
        }
        .check();
    }

    #[cfg(not(windows))]
    #[test]
    fn the_pieces_of_a_text_mapping_split_by_hooks_share_one_bias() {
        // The lld PIE above with its second text page made writable by a
        // hook engine: three modules, of which the first is one page at
        // file offset 0 -- the page the headers' mapping has too.
        let layout = Layout {
            maps: "\
5992b139a000-5992b139b000 r--p 00000000 00:2f 169012                     /t/hot_lld
5992b139b000-5992b139c000 r-xp 00000000 00:2f 169012                     /t/hot_lld
5992b139c000-5992b139d000 rwxp 00001000 00:2f 169012                     /t/hot_lld
5992b139d000-5992b139e000 r-xp 00002000 00:2f 169012                     /t/hot_lld
5992b139e000-5992b139f000 r--p 00002000 00:2f 169012                     /t/hot_lld
5992b139f000-5992b13a0000 rw-p 00002000 00:2f 169012                     /t/hot_lld
",
            path: "/t/hot_lld",
            segments: &[(0, 0, 0xc30), (0xc80, 0x1c80, 0x16e0), (0x2360, 0x4360, 0x228), (0x2588, 0x5588, 0x10)],
            bias: 0x5992b139a000,
            function: (0x3180, 0x30, 0x2180),
        };
        assert_eq!(layout.modules().len(), 3);
        layout.check();
    }

    /// A fixture from `testdata/` running as a child process, killed and
    /// removed when the test is done with it.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    struct RunningFixture {
        child: std::process::Child,
        path: std::path::PathBuf,
        /// Where the process itself sees its `marker` function.
        marker: u64,
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl RunningFixture {
        fn start(name: &str, image: &[u8]) -> RunningFixture {
            use std::io::Read;
            use std::os::unix::fs::PermissionsExt;
            // Beside the test binary: a directory that is known to allow
            // execution, which the temporary directory may not.
            let path = std::env::current_exe().unwrap().with_file_name(format!("{name}-{}", std::process::id()));
            std::fs::write(&path, image).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut tries = 0;
            let mut child = loop {
                match std::process::Command::new(&path).stdout(std::process::Stdio::piped()).spawn() {
                    Ok(child) => break child,
                    // ETXTBSY: another test forked while the file was open
                    // for writing, and its child holds that descriptor
                    // until it execs.
                    Err(error) if error.raw_os_error() == Some(26) && tries < 200 => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("{}: {error}", path.display()),
                }
            };
            let mut address = [0u8; 8];
            let read = child.stdout.take().unwrap().read_exact(&mut address);
            let fixture = RunningFixture { child, path, marker: u64::from_le_bytes(address) };
            read.expect("the fixture reports its marker's address");
            fixture
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    impl Drop for RunningFixture {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn a_running_lld_linked_process_gets_the_names_and_ids_of_its_own_functions() {
        // Real lld output run as real processes (see testdata/lld_layout.s):
        // the address is the one the process computed for itself, so nothing
        // here depends on this file's idea of a load bias.
        let fixtures: [(&str, &[u8]); 2] = [
            ("lld_layout_pie", include_bytes!("../testdata/lld_layout_pie")),
            ("lld_layout_image_base", include_bytes!("../testdata/lld_layout_image_base")),
        ];
        for (name, image) in fixtures {
            let fixture = RunningFixture::start(name, image);
            let pid = fixture.child.id() as i32;
            let symbolizer = Symbolizer::for_pid(pid);
            let frame = symbolizer.resolve_frame(fixture.marker + 2);
            assert_eq!(frame.name, "marker", "{name}: {frame:x?}");
            let index = crate::functions::FunctionIndex::for_pid(pid);
            let hit = index.by_id(frame.function_id).unwrap_or_else(|| panic!("{name}: no function has the id of {frame:x?}"));
            assert_eq!(hit.name, "marker", "{name}");
        }
    }

    #[test]
    fn rust_and_cpp_names_are_demangled_and_others_pass_through() {
        assert_eq!(demangle("_ZN4core3ptr13drop_in_place17h1234567890abcdefE"), "core::ptr::drop_in_place");
        assert_eq!(demangle("_ZN3app6module8functionEv"), "app::module::function");
        assert_eq!(demangle("clock_gettime"), "clock_gettime");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_resolved_frame_carries_its_module_and_address() {
        let symbolizer = Symbolizer::for_pid(std::process::id() as i32);
        let here = a_resolved_frame_carries_its_module_and_address as usize as u64;
        let frame = symbolizer.resolve_frame(here);
        assert_eq!(frame.address, here);
        assert!(!frame.module.is_empty(), "the running binary is a module");
        assert_eq!(frame.name, symbolizer.resolve(here), "same name either way");
    }

    /// Baseline for the per-sample frame resolution the capture loop does.
    /// Run with `cargo test --release symbolize_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn symbolize_bench() {
        // 120 modules of 4,000 symbols each: a mid-sized C++ process.
        let mut modules = Vec::new();
        for m in 0..120u64 {
            let base = 0x1000_0000 + m * 0x100_0000;
            let syms: Vec<(u64, u64, String)> = (0..4_000u64)
                .map(|i| (base + i * 64, 64, format!("_ZN3app6module{m}8function{i}Ev")))
                .collect();
            modules.push((base, base + 0x100_0000, base, format!("libmodule{m}.so"), syms));
        }
        let sym = Symbolizer::from_parts(modules);
        // 50k samples x 24 frames, pcs drawn from a 3,000-address hot set:
        // real stacks repeat the same few thousand addresses over and over.
        let mut seed = 0x9E37_79B9u64;
        let mut pcs = Vec::with_capacity(50_000 * 24);
        for _ in 0..50_000 * 24 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let hot = (seed >> 33) % 3_000;
            let m = hot % 120;
            let i = (hot / 120) % 4_000;
            pcs.push(0x1000_0000 + m * 0x100_0000 + i * 64 + 8);
        }
        let t = std::time::Instant::now();
        let mut total_len = 0usize;
        for pc in &pcs {
            total_len += sym.resolve_frame(*pc).name.len();
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "SYMBOLIZE_BENCH frames={} resolve_frame_ms={ms:.1} ns_per_frame={:.0} (checksum {total_len})",
            pcs.len(),
            ms * 1e6 / pcs.len() as f64
        );
        // The capture loop's path now: resolve once per distinct pc, then a
        // map lookup. Same name-keyed id table as FrameNames behind it.
        let mut ids: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut pc_ids: crate::report::FastMap<u64, u32> = crate::report::FastMap::default();
        let t = std::time::Instant::now();
        let mut sum = 0u64;
        for pc in &pcs {
            let id = *pc_ids.entry(*pc).or_insert_with(|| {
                let f = sym.resolve_frame(*pc);
                let n = ids.len() as u32;
                *ids.entry(f.name).or_insert(n)
            });
            sum += u64::from(id);
        }
        let ms2 = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "SYMBOLIZE_BENCH cached_ms={ms2:.1} ns_per_frame={:.0} distinct_pcs={} (checksum {sum})",
            ms2 * 1e6 / pcs.len() as f64,
            pc_ids.len()
        );
    }
}

#[cfg(test)]
mod load_timing {
    /// `PERFR_SYMBOL_BENCH=<executable> cargo test --release -- --ignored
    /// symbol_load_timing --nocapture`: where the seconds between Record and
    /// the first sample go, for one image and its detached debug file.
    #[test]
    #[ignore]
    fn symbol_load_timing() {
        let Ok(path) = std::env::var("PERFR_SYMBOL_BENCH") else { return };
        let t = std::time::Instant::now();
        let bytes = super::map_file(&path);
        eprintln!("map: {:?} ({} MB)", t.elapsed(), bytes.len() / 1_048_576);
        let t = std::time::Instant::now();
        let metadata = perfr_object::parse_elf_metadata(&bytes, &path).ok();
        eprintln!("parse_elf_metadata: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let debug = metadata.as_ref().and_then(|m| perfr_object::detached_debug_file(std::path::Path::new(&path), m));
        eprintln!("detached_debug_file: {:?} -> {:?}", t.elapsed(), debug);
        let t = std::time::Instant::now();
        let symbols = super::symbol_source(&bytes, Some(&path)).unwrap_or_default();
        eprintln!("symbol_source: {:?} ({} symbols)", t.elapsed(), symbols.len());
        let t = std::time::Instant::now();
        let sorted = super::sorted_symbols(&bytes, Some(&path));
        eprintln!("sorted_symbols (again, incl. sort): {:?} ({})", t.elapsed(), sorted.len());
    }

    /// `PERFR_SYMBOL_BENCH_PID=<pid> cargo test --release -- --ignored
    /// symbol_load_timing_pid --nocapture`: the whole `Symbolizer::for_pid`
    /// a capture builds, with the modules that carry the most symbols, for
    /// a process that is running now.
    #[test]
    #[ignore]
    fn symbol_load_timing_pid() {
        let Ok(pid) = std::env::var("PERFR_SYMBOL_BENCH_PID") else { return };
        let pid: i32 = pid.trim().parse().expect("PERFR_SYMBOL_BENCH_PID is a pid");
        let t = std::time::Instant::now();
        let symbolizer = super::Symbolizer::for_pid(pid);
        eprintln!(
            "for_pid: {:?} ({} files, {} mappings, {} symbols)",
            t.elapsed(),
            symbolizer.file_count(),
            symbolizer.module_count(),
            symbolizer.symbol_count()
        );
        let t = std::time::Instant::now();
        let again = super::Symbolizer::for_pid(pid);
        eprintln!("for_pid again, from the file cache: {:?} ({} symbols)", t.elapsed(), again.symbol_count());
        let mut by_size: Vec<(usize, &str)> =
            symbolizer.modules.iter().map(|m| (m.symbols().len(), m.path.as_str())).collect();
        by_size.sort_unstable_by(|a, b| b.cmp(a));
        for (count, path) in by_size.iter().take(12) {
            eprintln!("  {count:>9}  {path}");
        }
    }
}
