#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Split debug info out of release binaries and lay out a symbol store.

The binaries in the download archive are stripped of DWARF. The debug info
is not deleted:

* Linux: ``objcopy --only-keep-debug``, then ``--strip-debug`` (the symbol
  table and the GNU build-id note stay), then ``--add-gnu-debuglink``.
* macOS: ``dsymutil`` writes a ``.dSYM``, then ``strip -S``.
* Windows: cargo already wrote a PDB. The PE keeps its RSDS pointer.

The symbol store is the debuginfod URL layout ``buildid/<id>/debuginfo``
and the SymSrv layout ``<name>/<GUIDAGE>/<name>``. macOS dSYMs are stored
under ``dsym/<uuid>/``.
"""

from __future__ import annotations

import argparse
import shutil
import struct
import subprocess
import sys
from pathlib import Path

README = """\
perfr debug symbols
===================

These files are the debug info that was split out of the matching release
archive. It was not stripped away.

Linux
  Each binary was processed with objcopy --only-keep-debug, then
  objcopy --strip-debug (the symbol table and the GNU build-id stay), then
  objcopy --add-gnu-debuglink. The detached file is <name>.debug.
  perfr's own disassembly example reads .debug_line from that file: put
  perfr.debug next to perfr, or unpack the symstore archive and point
  PERFR_SYMBOL_PATH at it (buildid/<id>/debuginfo). A debuginfod server
  can serve that directory as its document root.

macOS
  dsymutil produced the .dSYM bundles. strip -S then removed debug symbols
  from the binaries. Live disassembly is not in the macOS build.

Windows
  The PDBs are what the linker wrote. SymSrv keys are
  <filename>/<GUID><age>/<filename>, age in lowercase hex with no padding.
  Disassembly on Windows does not read source lines from a PDB yet.

The companion symstore archive (perfr-<version>-<target>-symstore.tar.gz)
is the same bytes in the symbol-store layout. Upload it to the symbol
bucket, or leave it as a release asset when that bucket is not configured.
See docs/releasing.md.
"""


def elf_build_id(data: bytes) -> str | None:
    """Hex GNU build-id from an ELF note, or None."""
    if len(data) < 16 or data[:4] != b"\x7fELF":
        return None
    elf_class = data[4]
    if elf_class not in (1, 2) or data[5] not in (1, 2):
        return None
    little = data[5] == 1
    endian = "<" if little else ">"
    if elf_class == 2:
        if len(data) < 64:
            return None
        e_phoff, e_shoff = struct.unpack_from(endian + "QQ", data, 32)
        e_phentsize, e_phnum, e_shentsize, e_shnum = struct.unpack_from(
            endian + "HHHH", data, 54
        )
        note_ranges = _elf_note_ranges(
            data, endian, True, e_phoff, e_phentsize, e_phnum, e_shoff, e_shentsize, e_shnum
        )
    else:
        if len(data) < 52:
            return None
        e_phoff, e_shoff = struct.unpack_from(endian + "II", data, 28)
        e_phentsize, e_phnum, e_shentsize, e_shnum = struct.unpack_from(
            endian + "HHHH", data, 42
        )
        note_ranges = _elf_note_ranges(
            data, endian, False, e_phoff, e_phentsize, e_phnum, e_shoff, e_shentsize, e_shnum
        )
    for start, size in note_ranges:
        found = _gnu_build_id_in_notes(data[start : start + size], endian)
        if found:
            return found
    return None


def _elf_note_ranges(
    data: bytes,
    endian: str,
    elf64: bool,
    phoff: int,
    phentsize: int,
    phnum: int,
    shoff: int,
    shentsize: int,
    shnum: int,
) -> list[tuple[int, int]]:
    ranges: list[tuple[int, int]] = []
    # PT_NOTE = 4, SHT_NOTE = 7.
    if phentsize and phoff:
        for index in range(phnum):
            off = phoff + index * phentsize
            if off + phentsize > len(data):
                break
            p_type = struct.unpack_from(endian + "I", data, off)[0]
            if elf64:
                # p_offset is at 8; p_filesz is at 32 (p_vaddr and p_paddr sit between).
                p_offset = struct.unpack_from(endian + "Q", data, off + 8)[0]
                p_filesz = struct.unpack_from(endian + "Q", data, off + 32)[0]
            else:
                p_offset = struct.unpack_from(endian + "I", data, off + 4)[0]
                p_filesz = struct.unpack_from(endian + "I", data, off + 16)[0]
            if p_type == 4 and p_filesz:
                ranges.append((p_offset, p_filesz))
    if shentsize and shoff:
        for index in range(shnum):
            off = shoff + index * shentsize
            if off + shentsize > len(data):
                break
            sh_type = struct.unpack_from(endian + "I", data, off + 4)[0]
            if elf64:
                sh_offset, sh_size = struct.unpack_from(endian + "QQ", data, off + 24)
            else:
                sh_offset, sh_size = struct.unpack_from(endian + "II", data, off + 16)
            if sh_type == 7 and sh_size:
                ranges.append((sh_offset, sh_size))
    return ranges


def _gnu_build_id_in_notes(notes: bytes, endian: str) -> str | None:
    cursor = 0
    while cursor + 12 <= len(notes):
        namesz, descsz, ntype = struct.unpack_from(endian + "III", notes, cursor)
        cursor += 12
        if namesz > len(notes) or descsz > len(notes):
            return None
        name = notes[cursor : cursor + namesz]
        cursor += (namesz + 3) & ~3
        desc = notes[cursor : cursor + descsz]
        cursor += (descsz + 3) & ~3
        if name.startswith(b"GNU") and ntype == 3 and desc:
            return desc.hex()
    return None


def guidage(guid: bytes, age: int) -> str:
    """SymSrv ``GUIDAGE``: mixed-endian GUID plus age in unpadded lowercase hex.

    ``guid`` is the 16 bytes of an RSDS record, which is Data1/Data2/Data3
    little-endian followed by the eight Data4 bytes.
    """
    if len(guid) != 16:
        raise ValueError("a CodeView GUID is 16 bytes")
    data1, data2, data3 = struct.unpack_from("<IHH", guid, 0)
    data4 = guid[8:]
    head = f"{data1:08X}{data2:04X}{data3:04X}" + "".join(f"{byte:02X}" for byte in data4)
    return head + format(age, "x")


def symsrv_rel(name: str, signature: str) -> str:
    """``<filename>/<GUIDAGE>/<filename>``."""
    base = Path(name.replace("\\", "/")).name
    if not base or base in (".", "..") or "/" in signature or "\\" in signature:
        raise ValueError(f"bad symsrv key {name!r} {signature!r}")
    return f"{base}/{signature}/{base}"


def debuginfod_rel(build_id: str) -> str:
    """``buildid/<id>/debuginfo``, id lowercase hex."""
    ident = build_id.strip().lower()
    if len(ident) < 2 or any(ch not in "0123456789abcdef" for ch in ident):
        raise ValueError(f"bad build-id {build_id!r}")
    return f"buildid/{ident}/debuginfo"


def pe_codeview(data: bytes) -> tuple[str, str] | None:
    """``(pdb filename, GUIDAGE)`` from the first RSDS debug directory."""
    if len(data) < 0x40 or data[:2] != b"MZ":
        return None
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if e_lfanew + 24 > len(data) or data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        return None
    file_header = e_lfanew + 4
    nsections = struct.unpack_from("<H", data, file_header + 2)[0]
    opt_size = struct.unpack_from("<H", data, file_header + 16)[0]
    opt = file_header + 20
    if opt + 2 > len(data):
        return None
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic == 0x20B:  # PE32+
        dd_off = opt + 112
        num_dd = struct.unpack_from("<I", data, opt + 108)[0]
    elif magic == 0x10B:  # PE32
        dd_off = opt + 96
        num_dd = struct.unpack_from("<I", data, opt + 92)[0]
    else:
        return None
    if num_dd <= 6 or dd_off + 7 * 8 > len(data):
        return None
    debug_rva, debug_size = struct.unpack_from("<II", data, dd_off + 6 * 8)
    if debug_rva == 0 or debug_size < 28:
        return None
    sections = opt + opt_size

    def rva_to_off(rva: int) -> int | None:
        for index in range(nsections):
            sh = sections + index * 40
            if sh + 24 > len(data):
                return None
            virtual_size, virtual_addr, raw_size, raw_ptr = struct.unpack_from("<IIII", data, sh + 8)
            span = max(virtual_size, raw_size)
            if virtual_addr <= rva < virtual_addr + span:
                return raw_ptr + (rva - virtual_addr)
        return None

    debug_off = rva_to_off(debug_rva)
    if debug_off is None:
        return None
    for index in range(debug_size // 28):
        ent = debug_off + index * 28
        if ent + 28 > len(data):
            return None
        kind = struct.unpack_from("<I", data, ent + 12)[0]
        size = struct.unpack_from("<I", data, ent + 16)[0]
        raw = struct.unpack_from("<I", data, ent + 24)[0]  # PointerToRawData
        if kind != 2 or raw + 24 > len(data):
            continue
        if data[raw : raw + 4] != b"RSDS":
            continue
        guid = data[raw + 4 : raw + 20]
        age = struct.unpack_from("<I", data, raw + 20)[0]
        end = data.find(b"\x00", raw + 24, raw + 24 + max(size, 1))
        if end < 0:
            end = raw + 24 + size
        name = data[raw + 24 : end].decode("utf-8", "replace")
        base = Path(name.replace("\\", "/")).name
        if not base:
            return None
        return base, guidage(guid, age)
    return None


def macho_uuids(data: bytes) -> list[str]:
    """Canonical lowercase UUIDs from LC_UUID, including fat binaries."""
    if len(data) < 8:
        return []
    magic = struct.unpack_from(">I", data, 0)[0]
    if magic in (0xCAFEBABE, 0xBEBAFECA):
        # Fat headers are big-endian in the file even for CIGAM, which swaps
        # the magic but the fields are still defined as big-endian on disk
        # for FAT_MAGIC. FAT_CIGAM is the byte-swapped magic; fields follow.
        little = magic == 0xBEBAFECA
        endian = "<" if little else ">"
        nfat = struct.unpack_from(endian + "I", data, 4)[0]
        found: list[str] = []
        for index in range(nfat):
            off = 8 + index * 20
            if off + 20 > len(data):
                break
            arch_off, arch_size = struct.unpack_from(endian + "II", data, off + 8)
            found.extend(macho_uuids(data[arch_off : arch_off + arch_size]))
        return found
    return _thin_macho_uuids(data)


def _thin_macho_uuids(data: bytes) -> list[str]:
    if len(data) < 28:
        return []
    magic = struct.unpack_from("<I", data, 0)[0]
    if magic == 0xFEEDFACF:
        endian = "<"
        header = 32
    elif magic == 0xCFFAEDFE:
        endian = ">"
        header = 32
    elif magic == 0xFEEDFACE:
        endian = "<"
        header = 28
    elif magic == 0xCEFAEDFE:
        endian = ">"
        header = 28
    else:
        return []
    if len(data) < header:
        return []
    ncmds = struct.unpack_from(endian + "I", data, 16)[0]
    cursor = header
    found: list[str] = []
    for _ in range(ncmds):
        if cursor + 8 > len(data):
            break
        cmd, cmdsize = struct.unpack_from(endian + "II", data, cursor)
        if cmdsize < 8 or cursor + cmdsize > len(data):
            break
        if cmd == 0x1B and cmdsize >= 24:
            found.append(_format_uuid(data[cursor + 8 : cursor + 24]))
        cursor += cmdsize
    return found


def _format_uuid(raw: bytes) -> str:
    text = raw.hex()
    return f"{text[0:8]}-{text[8:12]}-{text[12:16]}-{text[16:20]}-{text[20:32]}"


def _run(cmd: list[str]) -> None:
    subprocess.check_call(cmd)


def split_elf(src: Path, dest_bin: Path, dest_debug: Path) -> str:
    """Split one ELF. Returns the lowercase build-id. ``src`` is not modified."""
    dest_bin.parent.mkdir(parents=True, exist_ok=True)
    dest_debug.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest_bin)
    before = elf_build_id(dest_bin.read_bytes())
    if not before:
        raise SystemExit(
            f"{src} has no GNU build-id. Link with -Wl,--build-id=sha1 "
            "(the release workflow sets that RUSTFLAGS)."
        )
    _run(["objcopy", "--only-keep-debug", str(dest_bin), str(dest_debug)])
    _run(["objcopy", "--strip-debug", str(dest_bin)])
    # --only-keep-debug sometimes turns the note into NOBITS. Put the note
    # back before the debuglink CRC is computed, so debuginfod can read the
    # id from the debug file itself.
    if elf_build_id(dest_debug.read_bytes()) != before:
        note = dest_debug.with_suffix(dest_debug.suffix + ".note")
        _run(["objcopy", "--dump-section", f".note.gnu.build-id={note}", str(dest_bin)])
        try:
            _run(["objcopy", "--update-section", f".note.gnu.build-id={note}", str(dest_debug)])
        except subprocess.CalledProcessError:
            _run(
                [
                    "objcopy",
                    "--add-section",
                    f".note.gnu.build-id={note}",
                    "--set-section-flags",
                    ".note.gnu.build-id=readonly",
                    str(dest_debug),
                ]
            )
        note.unlink(missing_ok=True)
        restored = elf_build_id(dest_debug.read_bytes())
        if restored != before:
            raise SystemExit(f"{dest_debug} build-id is {restored}, binary is {before}")
    _run(["objcopy", "--add-gnu-debuglink", str(dest_debug), str(dest_bin)])
    after = elf_build_id(dest_bin.read_bytes())
    if after != before:
        raise SystemExit(f"build-id changed for {dest_bin.name}: {before} -> {after}")
    return before


def split_macho(src: Path, dest_bin: Path, dsym_out: Path) -> list[str]:
    """Write a .dSYM and strip debug symbols. Returns LC_UUID strings."""
    dest_bin.parent.mkdir(parents=True, exist_ok=True)
    if dsym_out.exists():
        shutil.rmtree(dsym_out)
    shutil.copy2(src, dest_bin)
    _run(["dsymutil", str(dest_bin), "-o", str(dsym_out)])
    _run(["strip", "-S", str(dest_bin)])
    return macho_uuids(dest_bin.read_bytes())


def place_file(root: Path, relative: str, src: Path) -> None:
    dest = root.joinpath(*relative.split("/"))
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest)


def _copy_tree(src: Path, dest: Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    shutil.copytree(src, dest)


def split_tree(os_name: str, binaries: list[Path], debug_dir: Path, symstore_dir: Path, stripped_dir: Path) -> None:
    """Split every binary into ``stripped_dir``, ``debug_dir``, and ``symstore_dir``."""
    debug_dir.mkdir(parents=True, exist_ok=True)
    symstore_dir.mkdir(parents=True, exist_ok=True)
    stripped_dir.mkdir(parents=True, exist_ok=True)
    (debug_dir / "README.txt").write_text(README)
    for src in binaries:
        if not src.is_file():
            raise SystemExit(f"missing binary {src}")
        name = src.name
        dest_bin = stripped_dir / name
        if os_name == "linux":
            dest_debug = debug_dir / f"{name}.debug"
            build_id = split_elf(src, dest_bin, dest_debug)
            place_file(symstore_dir, debuginfod_rel(build_id), dest_debug)
        elif os_name == "macos":
            dsym = debug_dir / f"{name}.dSYM"
            uuids = split_macho(src, dest_bin, dsym)
            if not uuids:
                uuids = ["unknown"]
            for uuid in uuids:
                _copy_tree(dsym, symstore_dir / "dsym" / uuid / dsym.name)
        elif os_name == "windows":
            shutil.copy2(src, dest_bin)
            info = pe_codeview(src.read_bytes())
            if info is None:
                raise SystemExit(f"{src} has no RSDS PDB record")
            pdb_name, signature = info
            pdb = src.with_name(pdb_name)
            if not pdb.is_file():
                raise SystemExit(f"{src} names {pdb_name}, but {pdb} is missing")
            shutil.copy2(pdb, debug_dir / pdb_name)
            place_file(symstore_dir, symsrv_rel(pdb_name, signature), pdb)
            place_file(symstore_dir, symsrv_rel(name, signature), dest_bin)
        else:
            raise SystemExit(f"unknown os {os_name}")


def directory_bytes(path: Path) -> int:
    total = 0
    if not path.exists():
        return 0
    for file in path.rglob("*"):
        if file.is_file():
            total += file.stat().st_size
    return total


def measure(args: argparse.Namespace) -> None:
    lines = [f"target={args.label}"]

    def add(name: str, path: Path | None, kind: str) -> None:
        if path is None:
            return
        if not path.exists():
            lines.append(f"{name}=missing")
            return
        size = path.stat().st_size if kind == "file" else directory_bytes(path)
        lines.append(f"{name}={size}")

    add("core_archive_bytes", args.core_archive, "file")
    add("hooks_archive_bytes", args.hooks_archive, "file")
    add("debug_archive_bytes", args.debug_archive, "file")
    add("symstore_archive_bytes", args.symstore_archive, "file")
    add("core_installed_bytes", args.core_dir, "dir")
    add("hooks_installed_bytes", args.hooks_dir, "dir")
    core = directory_bytes(args.core_dir) if args.core_dir else 0
    hooks = directory_bytes(args.hooks_dir) if args.hooks_dir else 0
    lines.append(f"core_plus_hooks_installed_bytes={core + hooks}")
    if args.core_dir:
        for file in sorted(args.core_dir.rglob("*")):
            if file.is_file():
                lines.append(f"core_file:{file.relative_to(args.core_dir)}={file.stat().st_size}")
    if args.hooks_dir:
        for file in sorted(args.hooks_dir.rglob("*")):
            if file.is_file():
                lines.append(f"hooks_file:{file.relative_to(args.hooks_dir)}={file.stat().st_size}")
    text = "\n".join(lines) + "\n"
    if args.out:
        args.out.write_text(text)
    sys.stdout.write(text)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    split = sub.add_parser("split", help="split binaries into stripped, debug, and symstore dirs")
    split.add_argument("--os", required=True, choices=("linux", "macos", "windows"))
    split.add_argument("--debug-dir", type=Path, required=True)
    split.add_argument("--symstore-dir", type=Path, required=True)
    split.add_argument("--stripped-dir", type=Path, required=True)
    split.add_argument("binaries", nargs="+", type=Path)

    measured = sub.add_parser("measure", help="write archive and installed sizes")
    measured.add_argument("--label", required=True)
    measured.add_argument("--out", type=Path)
    measured.add_argument("--core-archive", type=Path)
    measured.add_argument("--hooks-archive", type=Path)
    measured.add_argument("--debug-archive", type=Path)
    measured.add_argument("--symstore-archive", type=Path)
    measured.add_argument("--core-dir", type=Path)
    measured.add_argument("--hooks-dir", type=Path)

    args = parser.parse_args(argv)
    if args.cmd == "split":
        split_tree(args.os, args.binaries, args.debug_dir, args.symstore_dir, args.stripped_dir)
        return 0
    measure(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
