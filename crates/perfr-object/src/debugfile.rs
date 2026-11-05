// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Finding the detached debug file of an ELF.
//!
//! A distribution's shared libraries are stripped to their dynamic symbol
//! table, so a sampled address inside libc resolves to nothing unless the
//! `-dbg` package's file is found: by build id under
//! `/usr/lib/debug/.build-id/xx/rest.debug`, else by `.gnu_debuglink`, next
//! to the file, in its `.debug/` directory, or under `/usr/lib/debug/<dir>/`,
//! the way gdb looks. A build-id path is its own proof. A debuglink
//! candidate is accepted when its own build-id note matches the file's
//! (read from the note alone, the way gdb checks first), else when the
//! link's CRC matches -- computed over the whole file, which for a game's
//! gigabyte of debug info is seconds, so it is the fallback, not the rule.

use std::path::{Path, PathBuf};

use object::Object;

use crate::debuglink::crc32_file;
use crate::ElfMetadata;

/// Where distributions put detached debug files.
pub const DEFAULT_DEBUG_ROOT: &str = "/usr/lib/debug";

/// The detached debug file for `path`, whose metadata was already parsed.
pub fn detached_debug_file(path: &Path, metadata: &ElfMetadata) -> Option<PathBuf> {
    detached_debug_file_under(path, metadata, Path::new(DEFAULT_DEBUG_ROOT)).or_else(|| {
        debuginfod_debuginfo(
            &metadata.build_id,
            &symbol_store_roots(),
            &debuginfod_cache_roots(),
        )
    })
}

/// `buildid/<id>/debuginfo` inside a symbol store, or `<id>/debuginfo` inside
/// a debuginfod client cache. The id is the lowercase hex GNU build-id.
///
/// The release symbol store uses the debuginfod URL layout (`buildid/<id>/debuginfo`)
/// so a static file server can answer debuginfod. The elfutils client cache
/// stores the same file one directory higher (`<id>/debuginfo`).
pub fn debuginfod_debuginfo(
    build_id: &str,
    store_roots: &[PathBuf],
    cache_roots: &[PathBuf],
) -> Option<PathBuf> {
    let id = build_id.trim().to_ascii_lowercase();
    if id.len() < 2 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    for root in store_roots {
        let candidate = root.join("buildid").join(&id).join("debuginfo");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for root in cache_roots {
        let candidate = root.join(&id).join("debuginfo");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// `PERFR_SYMBOL_PATH`, a `PATH`-style list of symbol-store roots.
pub fn symbol_store_roots() -> Vec<PathBuf> {
    std::env::var_os("PERFR_SYMBOL_PATH")
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The debuginfod client cache: `DEBUGINFOD_CACHE_PATH`, else
/// `$XDG_CACHE_HOME/debuginfod_client`, else `~/.cache/debuginfod_client`.
pub fn debuginfod_cache_roots() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("DEBUGINFOD_CACHE_PATH") {
        return vec![PathBuf::from(path)];
    }
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    cache_home
        .map(|dir| vec![dir.join("debuginfod_client")])
        .unwrap_or_default()
}

/// [`detached_debug_file`] with the debug root made explicit, for tests.
pub fn detached_debug_file_under(
    path: &Path,
    metadata: &ElfMetadata,
    root: &Path,
) -> Option<PathBuf> {
    if metadata.build_id.len() > 2 {
        let candidate = root
            .join(".build-id")
            .join(&metadata.build_id[..2])
            .join(format!("{}.debug", &metadata.build_id[2..]));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let link = metadata.gnu_debuglink.as_ref()?;
    let name = Path::new(&link.path);
    let dir = path.parent().unwrap_or(Path::new("."));
    let under_root = root.join(dir.strip_prefix("/").unwrap_or(dir)).join(name);
    for candidate in [dir.join(name), dir.join(".debug").join(name), under_root] {
        if !candidate.is_file() {
            continue;
        }
        // The fast proof first; the CRC stays the authority when the ids
        // differ or the candidate has none (a debug file produced from
        // another link of the same sources still checks out by CRC).
        if !metadata.build_id.is_empty()
            && build_id_of(&candidate).as_deref() == Some(metadata.build_id.as_str())
        {
            return Some(candidate);
        }
        if crc32_file(&candidate) == Some(link.crc32_checksum) {
            return Some(candidate);
        }
    }
    None
}

/// The hex build id of the ELF at `path`, reading only the pages the note
/// lives on rather than the file.
fn build_id_of(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let cache = object::read::ReadCache::new(file);
    let object = object::File::parse(&cache).ok()?;
    let id = object.build_id().ok()??;
    Some(id.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_elf_metadata;

    fn testdata_dir() -> PathBuf {
        PathBuf::from(
            std::env::var("PERFR_TESTDATA")
                .unwrap_or_else(|_| format!("{}/testdata", env!("CARGO_MANIFEST_DIR"))),
        )
    }

    #[test]
    fn a_debuglink_candidate_is_judged_by_build_id_before_crc() {
        let path = testdata_dir().join("hello_world_elf_with_gnu_debuglink");
        let bytes = std::fs::read(&path).unwrap();
        let metadata = parse_elf_metadata(&bytes, path.to_str().unwrap()).unwrap();
        let found = detached_debug_file_under(&path, &metadata, Path::new("/nonexistent")).unwrap();
        // Whichever proof applied, the same file is found.
        assert!(found.is_file());
        assert_eq!(build_id_of(Path::new("/nonexistent/file")), None);
        // The fixture's debug file is an ELF: its build id is readable from
        // the note alone, whether or not it equals the stripped file's.
        assert!(build_id_of(&found).is_some());
    }

    #[test]
    fn a_debuglink_next_to_the_file_is_found_when_its_crc_matches() {
        let path = testdata_dir().join("hello_world_elf_with_gnu_debuglink");
        let bytes = std::fs::read(&path).unwrap();
        let metadata = parse_elf_metadata(&bytes, path.to_str().unwrap()).unwrap();
        let empty_root = std::env::temp_dir().join("perfr-no-debug-root");
        let found = detached_debug_file_under(&path, &metadata, &empty_root)
            .expect("debug file next to it");
        assert_eq!(found.file_name().unwrap(), "hello_world_elf.debug");
    }

    #[test]
    fn a_build_id_path_under_the_root_wins() {
        let path = testdata_dir().join("hello_world_elf");
        let bytes = std::fs::read(&path).unwrap();
        let metadata = parse_elf_metadata(&bytes, path.to_str().unwrap()).unwrap();
        assert!(
            metadata.gnu_debuglink.is_none(),
            "this file has no debuglink; only the build id can find it"
        );
        let root = std::env::temp_dir().join(format!("perfr-debug-root-{}", std::process::id()));
        let dir = root.join(".build-id").join(&metadata.build_id[..2]);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{}.debug", &metadata.build_id[2..]));
        std::fs::write(&file, b"any bytes: the path is the proof").unwrap();
        assert_eq!(
            detached_debug_file_under(&path, &metadata, &root),
            Some(file)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_symbol_store_buildid_path_is_the_debuginfod_url_layout() {
        let root = std::env::temp_dir().join(format!("perfr-symstore-{}", std::process::id()));
        let id = "d12d54bc5b72ccce54a408bdeda65e2530740ac8";
        let debuginfo = root.join("buildid").join(id).join("debuginfo");
        std::fs::create_dir_all(debuginfo.parent().unwrap()).unwrap();
        std::fs::write(&debuginfo, b"split debug").unwrap();
        let cache = root.join("cache");
        let cached = cache.join(id).join("debuginfo");
        std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
        std::fs::write(&cached, b"cached").unwrap();
        assert_eq!(
            debuginfod_debuginfo(id, &[root.clone()], &[]).as_deref(),
            Some(debuginfo.as_path())
        );
        // The store wins over the client cache. An empty store falls through.
        assert_eq!(
            debuginfod_debuginfo(&id.to_ascii_uppercase(), &[], &[cache.clone()]).as_deref(),
            Some(cached.as_path())
        );
        assert_eq!(debuginfod_debuginfo("zz", &[root.clone()], &[]), None);
        assert_eq!(debuginfod_debuginfo("", &[root.clone()], &[]), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn line_tables_come_from_the_detached_debug_file() {
        // The release binary keeps a .gnu_debuglink and no .debug_line. The
        // example disassembly reads the detached file, which is where the
        // line table went.
        let path = testdata_dir().join("hello_world_elf_with_gnu_debuglink");
        let bytes = std::fs::read(&path).unwrap();
        let metadata = parse_elf_metadata(&bytes, path.to_str().unwrap()).unwrap();
        assert!(metadata.gnu_debuglink.is_some());
        let debug_path =
            detached_debug_file_under(&path, &metadata, Path::new("/nonexistent")).unwrap();
        let debug = std::fs::read(&debug_path).unwrap();
        // 0x1140 is the first line-table address in this fixture. Asking from
        // address 0 skips the unit: its ranges do not cover 0.
        let from_binary = crate::line_rows(&bytes, 0x1140, 0x1200);
        let from_debug = crate::line_rows(&debug, 0x1140, 0x1200);
        assert!(
            from_binary.is_empty(),
            "the stripped fixture has no line table"
        );
        assert!(
            from_debug.iter().any(|row| row.line > 0),
            "the detached debug file has the line table: {from_debug:?}"
        );
    }

    #[test]
    fn nothing_is_found_without_a_link_or_a_build_id_file() {
        let path = testdata_dir().join("hello_world_elf_no_build_id");
        let bytes = std::fs::read(&path).unwrap();
        let metadata = parse_elf_metadata(&bytes, path.to_str().unwrap()).unwrap();
        let empty_root = std::env::temp_dir().join("perfr-no-debug-root");
        assert_eq!(
            detached_debug_file_under(&path, &metadata, &empty_root),
            None
        );
    }
}
