#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Layout tests, plus an objcopy split when gcc is installed."""

from __future__ import annotations

import shutil
import struct
import subprocess
import tempfile
import unittest
from pathlib import Path

import split_debug


class LayoutTests(unittest.TestCase):
    def test_guidage_is_symsrv_mixed_endian(self) -> None:
        guid = struct.pack("<IHH", 0x01234567, 0x89AB, 0xCDEF) + bytes.fromhex("0123456789ABCDEF")
        self.assertEqual(split_debug.guidage(guid, 1), "0123456789ABCDEF0123456789ABCDEF1")
        self.assertEqual(split_debug.guidage(guid, 10), "0123456789ABCDEF0123456789ABCDEFa")

    def test_store_paths(self) -> None:
        self.assertEqual(
            split_debug.debuginfod_rel("D12D54BC5b72"),
            "buildid/d12d54bc5b72/debuginfo",
        )
        self.assertEqual(
            split_debug.symsrv_rel(r"C:\build\perfr.pdb", "ABCDEF1"),
            "perfr.pdb/ABCDEF1/perfr.pdb",
        )
        with self.assertRaises(ValueError):
            split_debug.debuginfod_rel("zz")
        with self.assertRaises(ValueError):
            split_debug.symsrv_rel("perfr.pdb", "aa/bb")

    def test_elf_note_and_pe_rsds_and_macho_uuid(self) -> None:
        ident = bytes.fromhex("aabbccdd")
        elf = _elf_with_build_id(ident)
        self.assertEqual(split_debug.elf_build_id(elf), "aabbccdd")
        self.assertIsNone(split_debug.elf_build_id(b"not elf"))

        pe = _pe_with_rsds(b"perfr.pdb\x00", ident.ljust(16, b"\x11"), 1)
        name, signature = split_debug.pe_codeview(pe)
        self.assertEqual(name, "perfr.pdb")
        self.assertTrue(signature.endswith("1"))
        self.assertIsNone(split_debug.pe_codeview(b"MZ" + b"\x00" * 80))

        uuid = bytes(range(16))
        macho = _macho_with_uuid(uuid)
        self.assertEqual(
            split_debug.macho_uuids(macho),
            ["00010203-0405-0607-0809-0a0b0c0d0e0f"],
        )


class ObjcopyTests(unittest.TestCase):
    def test_split_keeps_build_id_and_moves_the_line_table(self) -> None:
        if not shutil.which("gcc") or not shutil.which("objcopy") or not shutil.which("readelf"):
            self.skipTest("gcc, objcopy, and readelf are required")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "hello.c"
            source.write_text('int main(void){return 0;}\n')
            binary = root / "hello"
            subprocess.check_call(
                ["gcc", "-g", "-O0", "-Wl,--build-id=sha1", "-o", str(binary), str(source)]
            )
            before = binary.stat().st_size
            debug = root / "debug"
            store = root / "symstore"
            stripped = root / "stripped"
            split_debug.split_tree("linux", [binary], debug, store, stripped)
            out = stripped / "hello"
            self.assertLess(out.stat().st_size, before)
            build_id = split_debug.elf_build_id(out.read_bytes())
            self.assertEqual(build_id, split_debug.elf_build_id(binary.read_bytes()))
            debug_file = debug / "hello.debug"
            self.assertEqual(split_debug.elf_build_id(debug_file.read_bytes()), build_id)
            sections = subprocess.check_output(["readelf", "-S", str(out)], text=True)
            self.assertIn(".gnu_debuglink", sections)
            self.assertNotIn(".debug_line", sections)
            debug_sections = subprocess.check_output(
                ["readelf", "-S", str(debug_file)], text=True, stderr=subprocess.STDOUT
            )
            self.assertIn(".debug_line", debug_sections)
            stored = store / "buildid" / build_id / "debuginfo"
            self.assertEqual(stored.read_bytes(), debug_file.read_bytes())
            self.assertTrue((debug / "README.txt").is_file())


def _elf_with_build_id(desc: bytes) -> bytes:
    """A tiny little-endian ELF64 whose only section is a GNU build-id note."""
    name = b"GNU\x00"
    note = struct.pack("<III", len(name), len(desc), 3) + name + desc
    note += b"\x00" * ((4 - (len(note) % 4)) % 4)
    # ELF header (64) + one program header (56) + note bytes.
    e_phoff = 64
    e_phentsize = 56
    e_phnum = 1
    note_off = e_phoff + e_phentsize
    ident = b"\x7fELF" + bytes([2, 1, 1]) + b"\x00" * 9
    header = ident
    header += struct.pack("<HHI", 3, 0x3E, 1)  # ET_DYN, EM_X86_64
    header += struct.pack("<QQQ", 0, e_phoff, 0)  # entry, phoff, shoff
    header += struct.pack("<IHHHHHH", 0, 64, e_phentsize, e_phnum, 0, 0, 0)
    # PT_NOTE, flags, offset, vaddr, paddr, filesz, memsz, align
    phdr = struct.pack("<IIQQQQQQ", 4, 4, note_off, 0, 0, len(note), len(note), 4)
    return header + phdr + note


def _pe_with_rsds(pdb_name: bytes, guid: bytes, age: int) -> bytes:
    """A PE32+ whose debug directory points at an RSDS record in the file."""
    assert len(guid) == 16
    rsds = b"RSDS" + guid + struct.pack("<I", age) + pdb_name
    # Layout: DOS stub, PE headers, one section, debug dir, RSDS.
    # Keep the numbers small and explicit.
    dos = bytearray(0x80)
    dos[:2] = b"MZ"
    e_lfanew = 0x80
    struct.pack_into("<I", dos, 0x3C, e_lfanew)
    pe = bytearray()
    pe += b"PE\x00\x00"
    # File header: machine, nsections=1, timestamp, sym, nsym, opt size, characteristics
    opt_size = 240  # PE32+ optional header with 16 data directories: 112 + 128
    pe += struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, opt_size, 0x22)
    opt = bytearray(opt_size)
    struct.pack_into("<H", opt, 0, 0x20B)
    # NumberOfRvaAndSizes at 108, 16 directories. Debug is index 6.
    struct.pack_into("<I", opt, 108, 16)
    debug_rva = 0x1000
    debug_size = 28
    struct.pack_into("<II", opt, 112 + 6 * 8, debug_rva, debug_size)
    pe += opt
    # Section: name, vsize, va, raw size, raw ptr
    raw_ptr = 0x200
    section = bytearray(40)
    section[:6] = b".rdata"
    struct.pack_into("<IIII", section, 8, 0x100, debug_rva, 0x100, raw_ptr)
    pe += section
    blob = bytearray(dos) + pe
    if len(blob) > raw_ptr:
        raise AssertionError(len(blob))
    blob += b"\x00" * (raw_ptr - len(blob))
    # IMAGE_DEBUG_DIRECTORY at raw_ptr, RSDS just after it.
    debug_dir = bytearray(28)
    struct.pack_into("<I", debug_dir, 12, 2)  # CODEVIEW
    struct.pack_into("<I", debug_dir, 16, len(rsds))
    struct.pack_into("<I", debug_dir, 20, debug_rva + 28)  # AddressOfRawData
    struct.pack_into("<I", debug_dir, 24, raw_ptr + 28)  # PointerToRawData
    blob += debug_dir + rsds
    return bytes(blob)


def _macho_with_uuid(uuid: bytes) -> bytes:
    assert len(uuid) == 16
    # MH_MAGIC_64, cputype, cpusubtype, filetype, ncmds, sizeofcmds, flags, reserved
    header = struct.pack("<IIIIIIII", 0xFEEDFACF, 0x01000007, 0, 2, 1, 24, 0, 0)
    cmd = struct.pack("<II", 0x1B, 24) + uuid
    return header + cmd


if __name__ == "__main__":
    unittest.main()
