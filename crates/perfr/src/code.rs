// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The code views' data: a function disassembled with the source line of
//! every instruction, and source files. What C++ perfr's `Disassembler`
//! (Capstone), `AnnotateDisassemblyWithSourceCode` and
//! `PerfrMainWindow::LoadSourceCode` produce, as JSON for the viewer.
//!
//! The bytes come from the module file on disk at the function's file
//! offset, not from the process's memory: the same bytes, and no ptrace.
//! The source line of an address comes from the module's DWARF line table
//! or its detached debug file, through `perfr_object::line_info`, which
//! follows LLVM's rules for inlined code. Release archives strip DWARF out
//! of the binary and publish it separately. The lookup is `.gnu_debuglink`
//! (next to the file, in `.debug/`, or under `/usr/lib/debug`), then
//! `/usr/lib/debug/.build-id`, then a debuginfod store at
//! `PERFR_SYMBOL_PATH/buildid/<id>/debuginfo` or the debuginfod client
//! cache. A local `cargo build --release` still has the line table in the
//! binary, because cargo is not asked to strip. A source file is served only if
//! a disassembly named it or it sits under `PERFR_SOURCE_ROOTS`: the
//! service often runs as root, and a file endpoint that took any path
//! would be a way to read anything. Both tests are made on the file a path
//! resolves to, not on its spelling, so `..` and symlinks cannot walk a
//! request out of what was allowed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use iced_x86::{Decoder, DecoderOptions, FlowControl, Formatter, Instruction, IntelFormatter, OpKind};
use perfr_object::{detached_debug_file, parse_elf_metadata, ObjectSegment};

use crate::functions::{file_offset_of, FunctionIndex, InstrumentableFunction};
use crate::symbolize::{map_file, try_map_file, FileBytes};

/// Instructions past which a function is cut, with a last line saying so:
/// a 60,000-instruction function is not something to read.
const MAX_INSTRUCTIONS: usize = 8_000;
/// Source files above this are refused: a generated file, not code.
const MAX_SOURCE_BYTES: u64 = 8 << 20;

/// The source files a disassembly has named, and so may be read. Each is
/// held as the canonical path it resolved to when it was named: debug info
/// spells files through `..` (an include directory relative to the
/// compilation directory) and through symlinked checkouts, and a request is
/// matched after the same resolution. A name that resolved to nothing is
/// kept as written, only so the request for it can say the file is missing.
pub type SourceAllowList = Arc<Mutex<HashSet<PathBuf>>>;

fn address_of_offset(segments: &[ObjectSegment], offset: u64) -> Option<u64> {
    segments
        .iter()
        .find(|s| offset >= s.offset_in_file && offset < s.offset_in_file + s.size_in_file)
        .map(|s| s.address + (offset - s.offset_in_file))
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Disassembles the function `function_id` of `index`, with the source
/// line of every instruction where the module's DWARF says.
pub fn disassemble_json(index: &FunctionIndex, function_id: u64, allow: &SourceAllowList) -> Result<String, String> {
    let function = index.by_id(function_id).ok_or_else(|| format!("no function with id {function_id:#x}"))?;
    disassemble_function(index, function, allow)
}

fn disassemble_function(
    index: &FunctionIndex,
    function: &InstrumentableFunction,
    allow: &SourceAllowList,
) -> Result<String, String> {
    let path = &function.module_path;
    // Mapped, not read, as the symbolizer does: a game's module is hundreds
    // of megabytes and its debug file a gigabyte, of which one function's
    // bytes and the DWARF sections are wanted.
    let bytes = try_map_file(path).map_err(|e| format!("{path}: {e}"))?;
    // A PE image's sections stand in for an ELF's loadable segments; its
    // line table is in a PDB, which `line_rows` does not read, so a PE
    // disassembly has no source interleave.
    let (segments, is_64_bit, debug_bytes): (Vec<ObjectSegment>, bool, Option<FileBytes>) = if perfr_object::is_pe(&bytes) {
        let metadata = perfr_object::parse_coff_metadata(&bytes, path)?;
        (metadata.sections, metadata.is_64_bit, None)
    } else {
        let metadata = parse_elf_metadata(&bytes, path)?;
        let debug = detached_debug_file(Path::new(path), &metadata).and_then(|p| p.to_str().map(map_file));
        (metadata.loadable_segments, metadata.is_64_bit, debug)
    };
    let segments = &segments;
    let address = address_of_offset(segments, function.file_offset)
        .ok_or_else(|| format!("{}: file offset {:#x} is in no loadable segment", function.name, function.file_offset))?;
    let start = function.file_offset as usize;
    let end = start.saturating_add(function.size as usize).min(bytes.len());
    if start >= end {
        return Err(format!("{}: no bytes for the function in {path}", function.name));
    }
    let code = &bytes[start..end];
    // The line table lives in the module, or in its detached debug file
    // when the distribution ships one.
    let debug_data: &[u8] = debug_bytes.as_deref().unwrap_or(&bytes);

    let bitness = if is_64_bit { 64 } else { 32 };
    let mut decoder = Decoder::with_ip(bitness, code, address, DecoderOptions::NONE);
    let mut formatter = IntelFormatter::new();
    {
        let options = formatter.options_mut();
        options.set_first_operand_char_index(8);
        options.set_hex_prefix("0x");
        options.set_hex_suffix("");
        options.set_uppercase_hex(false);
        options.set_space_after_operand_separator(true);
        options.set_branch_leading_zeros(false);
    }
    // Functions of this module by address, for naming call targets.
    let same_module: Vec<&InstrumentableFunction> =
        index.functions().filter(|f| f.module_path == function.module_path).collect();
    let name_at = |target: u64| -> String {
        let Some(offset) = file_offset_of(segments, target) else { return String::new() };
        same_module
            .iter()
            .find(|f| offset >= f.file_offset && offset < f.file_offset + f.size.max(1))
            .map(|f| {
                if offset == f.file_offset {
                    f.name.clone()
                } else {
                    format!("{}+{:#x}", f.name, offset - f.file_offset)
                }
            })
            .unwrap_or_default()
    };

    // Opened once for both questions asked of it below: opening inflates
    // every compressed DWARF section, seconds for a game's debug file.
    let debug_info = perfr_object::DebugInfo::parse(debug_data);
    // The line table for the whole function, one walk; the row in force at
    // an instruction is the last one at or below it.
    let rows = debug_info.as_ref().map(|info| info.line_rows(address, address + function.size.max(1))).unwrap_or_default();
    let row_at = |ip: u64| -> Option<&perfr_object::LineRow> {
        let at = rows.partition_point(|r| r.address <= ip);
        (at > 0).then(|| &rows[at - 1])
    };
    let mut lines = String::new();
    let mut files: Vec<String> = Vec::new();
    let mut instruction = Instruction::default();
    let mut text = String::new();
    let mut count = 0usize;
    let mut last_file_line: Option<(String, u32)> = None;
    while decoder.can_decode() {
        decoder.decode_out(&mut instruction);
        count += 1;
        if count > MAX_INSTRUCTIONS {
            break;
        }
        text.clear();
        formatter.format(&instruction, &mut text);
        let ip = instruction.ip();
        let offset = ip - address;
        let len = instruction.len();
        let raw = &code[offset as usize..(offset as usize + len).min(code.len())];
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
        let target = match instruction.flow_control() {
            FlowControl::Call | FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
                if matches!(instruction.op0_kind(), OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16) =>
            {
                name_at(instruction.near_branch_target())
            }
            _ => String::new(),
        };
        let (file, line) = match row_at(ip) {
            Some(r) if r.line > 0 && !r.file.is_empty() => (r.file.clone(), r.line),
            _ => (String::new(), 0),
        };
        if !file.is_empty() && !files.contains(&file) {
            files.push(file.clone());
        }
        let same_as_last = last_file_line.as_ref().is_some_and(|(f, l)| *f == file && *l == line);
        last_file_line = Some((file.clone(), line));
        if !lines.is_empty() {
            lines.push(',');
        }
        lines.push_str(&format!(
            "{{\"address\":{ip},\"offset\":{offset},\"bytes\":{},\"text\":{},\"target\":{},\"file\":{},\"line\":{line},\"new_line\":{}}}",
            json_str(&hex),
            json_str(&text),
            json_str(&target),
            json_str(&file),
            !same_as_last
        ));
    }
    let truncated = count > MAX_INSTRUCTIONS;
    allow_named(allow, &files);
    let files_json: Vec<String> = files.iter().map(|f| json_str(f)).collect();
    // Where the function is declared, for the source view to open on.
    let (decl_file, decl_line) = match debug_info.as_ref().map(|info| info.declaration_location(address)) {
        Some(Ok(info)) => (info.source_file, info.source_line),
        _ => files.first().cloned().map(|f| (f, 0)).unwrap_or_default(),
    };
    Ok(format!(
        "{{\"function\":{{\"id\":{},\"name\":{},\"module\":{},\"module_path\":{},\"address\":{address},\"size\":{},\"file\":{},\"line\":{decl_line}}},\"arch\":\"{}\",\"truncated\":{truncated},\"files\":[{}],\"lines\":[{lines}]}}",
        function.id,
        json_str(&function.name),
        json_str(&function.module),
        json_str(&function.module_path),
        function.size,
        json_str(&decl_file),
        if is_64_bit { "x86-64" } else { "x86" },
        files_json.join(",")
    ))
}

/// The roots under which any source file may be read, from
/// `PERFR_SOURCE_ROOTS` (colon-separated).
fn source_roots() -> Vec<String> {
    std::env::var("PERFR_SOURCE_ROOTS")
        .ok()
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

fn allow_named(allow: &SourceAllowList, files: &[String]) {
    let resolved: Vec<PathBuf> =
        files.iter().map(|f| std::fs::canonicalize(f).unwrap_or_else(|_| PathBuf::from(f))).collect();
    if let Ok(mut set) = allow.lock() {
        set.extend(resolved);
    }
}

/// A source file, as `{"path","text"}`, if a disassembly named it or it is
/// under a configured root.
pub fn source_json(path: &str, allow: &SourceAllowList) -> Result<String, String> {
    source_json_under(path, allow, &source_roots())
}

fn source_json_under(path: &str, allow: &SourceAllowList, roots: &[String]) -> Result<String, String> {
    let named = |file: &Path| allow.lock().map(|s| s.contains(file)).unwrap_or(false);
    let not_served = || format!("{path}: not a file a disassembly named, and not under PERFR_SOURCE_ROOTS");
    // The file the path leads to is what gets checked and what gets read:
    // a string test on the path as written let `<root>/../../etc/passwd`
    // and a symlink under a root through.
    let file = match std::fs::canonicalize(path) {
        Ok(file) => file,
        // Sources named by a binary built elsewhere are usually not on this
        // machine, and the viewer shows this error. Any other path gets the
        // answer a refused one gets, so the endpoint does not tell a caller
        // which paths exist.
        Err(e) => return Err(if named(Path::new(path)) { format!("{path}: {e}") } else { not_served() }),
    };
    // `Path::starts_with` compares whole components: `/home/me/src` is not
    // a root of `/home/me/src-private`.
    let allowed =
        named(&file) || roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).any(|r| file.starts_with(&r));
    if !allowed {
        return Err(not_served());
    }
    let meta = std::fs::metadata(&file).map_err(|e| format!("{path}: {e}"))?;
    // A line table can name a device or a FIFO; reading one never ends or
    // never starts.
    if !meta.is_file() {
        return Err(format!("{path}: not a regular file"));
    }
    if meta.len() > MAX_SOURCE_BYTES {
        return Err(format!("{path}: {} bytes is not a source file", meta.len()));
    }
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{path}: {e}"))?;
    // The path as asked for: the viewer matches it against the file name
    // on each instruction.
    Ok(format!("{{\"path\":{},\"text\":{}}}", json_str(path), json_str(&text)))
}

/// A disassembly to look at with nothing captured: a function of this
/// service's own binary. Source lines come from `.debug_line` in that
/// binary or, once the release packager has split debug info out, from the
/// detached file named by `.gnu_debuglink` (`perfr.debug` beside the
/// binary, or the debuginfod `buildid/<id>/debuginfo` file). The line table
/// is not left in the release binary. Without the detached file the
/// instructions still disassemble; the source column is empty.
/// `perfr::uprobes` is the module it looks for, a function big enough to
/// read; failing that, the largest function.
pub fn example_disassembly_json(allow: &SourceAllowList) -> Result<String, String> {
    if cfg!(target_os = "macos") {
        return Err("Live Mach-O disassembly is not yet supported on macOS".into());
    }
    static OWN_INDEX: OnceLock<FunctionIndex> = OnceLock::new();
    let index = OWN_INDEX.get_or_init(|| FunctionIndex::for_pid(std::process::id() as i32));
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let own = |f: &&InstrumentableFunction| f.module_path.eq_ignore_ascii_case(&exe);
    let pick = index
        .functions()
        .filter(own)
        .filter(|f| f.name.contains("uprobes::UprobeSession::poll") || f.name.contains("UprobeSession::drain_up_to"))
        .max_by_key(|f| f.size)
        .or_else(|| index.functions().filter(own).filter(|f| f.size > 400 && f.size < 4000).max_by_key(|f| f.size))
        .ok_or_else(|| "no function of the service's own binary to disassemble".to_string())?;
    disassemble_function(index, pick, allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `src/a.c`, `src/sub/`, `include/x.h`, `src-private/secret.c` and
    /// `secret.txt`. The directory's path is used as the system gave it:
    /// canonical, it is a verbatim path on Windows, where the `/` and `..`
    /// these tests put in a request are not resolved.
    fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        for sub in ["src/sub", "include", "src-private"] {
            std::fs::create_dir_all(base.join(sub)).unwrap();
        }
        std::fs::write(base.join("src/a.c"), "int a;\n").unwrap();
        std::fs::write(base.join("include/x.h"), "int x;\n").unwrap();
        std::fs::write(base.join("src-private/secret.c"), "private\n").unwrap();
        std::fs::write(base.join("secret.txt"), "secret\n").unwrap();
        (dir, base)
    }

    fn at(base: &Path, relative: &str) -> String {
        format!("{}/{relative}", base.display())
    }

    fn served(result: Result<String, String>) -> (String, String) {
        let value: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        (value["path"].as_str().unwrap().to_string(), value["text"].as_str().unwrap().to_string())
    }

    fn refused(result: Result<String, String>) -> String {
        match result {
            Ok(body) => panic!("served: {body}"),
            Err(error) => error,
        }
    }

    #[test]
    fn a_path_that_climbs_out_of_a_source_root_is_refused() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        let roots = [at(&base, "src")];
        let climbing = at(&base, "src/../secret.txt");
        let error = refused(source_json_under(&climbing, &allow, &roots));
        assert!(error.contains("not under PERFR_SOURCE_ROOTS"), "{error}");
        // `..` that stays inside the root is still that file, and the reply
        // carries the spelling that was asked for: the viewer matches on it.
        let inside = at(&base, "src/sub/../a.c");
        assert_eq!(served(source_json_under(&inside, &allow, &roots)), (inside.clone(), "int a;\n".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_under_a_root_that_points_outside_it_is_refused() {
        let (_dir, base) = tree();
        std::os::unix::fs::symlink(base.join("secret.txt"), base.join("src/link.c")).unwrap();
        let allow = SourceAllowList::default();
        let error = refused(source_json_under(&at(&base, "src/link.c"), &allow, &[at(&base, "src")]));
        assert!(error.contains("not under PERFR_SOURCE_ROOTS"), "{error}");
    }

    #[test]
    fn a_root_admits_whole_components_not_a_string_prefix() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        let sibling = at(&base, "src-private/secret.c");
        let error = refused(source_json_under(&sibling, &allow, &[at(&base, "src")]));
        assert!(error.contains("not under PERFR_SOURCE_ROOTS"), "{error}");
        // Both sides are resolved, so a root may be spelled with a trailing
        // slash or with `..` in it.
        for root in [at(&base, "src/"), at(&base, "include/../src")] {
            let (_, text) = served(source_json_under(&at(&base, "src/a.c"), &allow, &[root]));
            assert_eq!(text, "int a;\n");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_symlink_admits_the_files_it_leads_to() {
        let (_dir, base) = tree();
        std::os::unix::fs::symlink(base.join("src"), base.join("checkout")).unwrap();
        let allow = SourceAllowList::default();
        let roots = [at(&base, "checkout")];
        for path in [at(&base, "src/a.c"), at(&base, "checkout/a.c")] {
            let (echoed, text) = served(source_json_under(&path, &allow, &roots));
            assert_eq!((echoed, text.as_str()), (path.clone(), "int a;\n"));
        }
    }

    #[test]
    fn only_a_regular_file_is_served() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        let error = refused(source_json_under(&at(&base, "src/sub"), &allow, &[at(&base, "src")]));
        assert!(error.contains("not a regular file"), "{error}");
        #[cfg(unix)]
        {
            // What a line table can name and `read` would happily drain.
            allow_named(&allow, &["/dev/null".to_string()]);
            let error = refused(source_json_under("/dev/null", &allow, &[]));
            assert!(error.contains("not a regular file"), "{error}");
        }
    }

    #[test]
    fn a_file_named_with_dot_dot_segments_is_served_under_that_name() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        // An include directory relative to the compilation directory.
        let named = at(&base, "src/../include/x.h");
        allow_named(&allow, &[named.clone()]);
        assert_eq!(served(source_json_under(&named, &allow, &[])), (named.clone(), "int x;\n".to_string()));
        // The list holds the file, not the spelling.
        let plain = at(&base, "include/x.h");
        assert_eq!(served(source_json_under(&plain, &allow, &[])), (plain.clone(), "int x;\n".to_string()));
        let error = refused(source_json_under(&at(&base, "src/../secret.txt"), &allow, &[]));
        assert!(error.contains("not a file a disassembly named"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_named_file_replaced_by_a_symlink_is_no_longer_the_file_that_was_named() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        let named = at(&base, "src/a.c");
        allow_named(&allow, &[named.clone()]);
        std::fs::remove_file(&named).unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), &named).unwrap();
        let error = refused(source_json_under(&named, &allow, &[]));
        assert!(error.contains("not a file a disassembly named"), "{error}");
    }

    #[test]
    fn a_named_file_that_is_not_on_this_machine_says_so_and_no_other_path_does() {
        let (_dir, base) = tree();
        let allow = SourceAllowList::default();
        // A build-machine path: the common case for a distribution binary.
        let named = at(&base, "build/glibc/../sysdeps/x.c");
        allow_named(&allow, &[named.clone()]);
        let error = refused(source_json_under(&named, &allow, &[]));
        assert!(!error.contains("not a file a disassembly named"), "{error}");
        // A path nothing named gets one answer whether or not it exists.
        let missing = refused(source_json_under(&at(&base, "build/other.c"), &allow, &[]));
        let present = refused(source_json_under(&at(&base, "secret.txt"), &allow, &[]));
        assert_eq!(missing.replace("build/other.c", "secret.txt"), present);
    }
}
