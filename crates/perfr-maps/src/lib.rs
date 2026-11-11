// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Parsing of `/proc/[pid]/maps`, and the load bias of an image mapped
//! there ([`load_bias`]), which the unwinder and the symbolizer share.
//!
//! The parser began as a port of the C++ `ParseMaps` and keeps its rules
//! for which lines are skipped; it is the only implementation now, and its
//! tests are what hold it.
//!
//! Everything here operates on bytes rather than `str`. Paths in
//! `/proc/[pid]/maps` are kernel-supplied and are not guaranteed to be
//! UTF-8, so decoding would add a failure mode for no gain.

#![deny(unsafe_code)]

/// `PROT_READ` from `<sys/mman.h>`.
pub const PROT_READ: u64 = 1;
/// `PROT_WRITE` from `<sys/mman.h>`.
pub const PROT_WRITE: u64 = 2;
/// `PROT_EXEC` from `<sys/mman.h>`.
pub const PROT_EXEC: u64 = 4;

/// One entry of `/proc/[pid]/maps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryMapping {
    pub start_address: u64,
    pub end_address: u64,
    pub perms: u64,
    pub offset: u64,
    pub inode: u64,
    /// Raw bytes as the kernel reported them; not necessarily UTF-8.
    pub pathname: Vec<u8>,
}

/// Splits `input` on `sep` at most `max_splits` times, so the result holds at
/// most `max_splits + 1` fields and the last one keeps every remaining
/// separator. Equivalent to `absl::StrSplit(s, absl::MaxSplits(sep, n))`.
fn split_max(input: &[u8], sep: u8, max_splits: usize) -> Vec<&[u8]> {
    // saturating_add, and capped: callers pass small bounds, but a reservation
    // derived from an argument should not be able to overflow or to ask for a
    // gigabyte because someone passed usize::MAX.
    let mut fields = Vec::with_capacity(max_splits.saturating_add(1).min(16));
    let mut rest = input;
    for _ in 0..max_splits {
        match rest.iter().position(|&b| b == sep) {
            Some(i) => {
                fields.push(&rest[..i]);
                rest = &rest[i + 1..];
            }
            None => break,
        }
    }
    fields.push(rest);
    fields
}

/// `absl::StripLeadingAsciiWhitespace`.
fn strip_leading_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let first = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[first..]
}

/// Parses a hexadecimal integer, rejecting an empty string or any trailing
/// byte that is not a hex digit.
///
/// The C++ uses `std::stoull(s, nullptr, 16)`, which *throws* on a string
/// with no leading digits and silently ignores trailing garbage. Returning
/// `None` and skipping the line is a deliberate, documented divergence: no
/// existing test covers malformed hex, and a parser that reads
/// kernel-supplied text should not be able to terminate the process. See
/// https://perfr.dev/blog/.
fn parse_hex_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &b in bytes {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(u64::from(digit))?;
    }
    Some(value)
}

/// `absl::SimpleAtoi<uint64_t>`: a full-string decimal parse that accepts an
/// optional leading `+` and rejects any trailing byte.
fn parse_dec_u64(bytes: &[u8]) -> Option<u64> {
    let digits = match bytes.first() {
        Some(b'+') => &bytes[1..],
        _ => bytes,
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(value)
}

/// Parses the contents of `/proc/[pid]/maps`.
///
/// Lines that do not match the expected shape are skipped, exactly as the C++
/// does. Never fails: a completely malformed input yields an empty vector.
pub fn parse_maps(content: &[u8]) -> Vec<MemoryMapping> {
    let mut result = Vec::new();

    for line in content.split(|&b| b == b'\n') {
        // The number of spaces between the inode and the path is variable and
        // the path itself can contain spaces, so cap the split and strip the
        // path's leading whitespace separately.
        let tokens = split_max(line, b' ', 5);
        if tokens.len() < 5 {
            continue;
        }
        debug_assert!(tokens.len() == 5 || tokens.len() == 6);

        // absl::StrSplit(tokens[0], '-') splits on every dash, so "1-2-3"
        // yields three fields and is rejected below.
        let mut start_and_end = tokens[0].split(|&b| b == b'-');
        let (Some(start_field), Some(end_field), None) = (
            start_and_end.next(),
            start_and_end.next(),
            start_and_end.next(),
        ) else {
            continue;
        };
        let Some(start_address) = parse_hex_u64(start_field) else {
            continue;
        };
        let Some(end_address) = parse_hex_u64(end_field) else {
            continue;
        };
        let Some(offset) = parse_hex_u64(tokens[2]) else {
            continue;
        };

        if tokens[1].len() < 4 {
            continue;
        }
        let mut perms = 0;
        if tokens[1][0] == b'r' {
            perms |= PROT_READ;
        }
        if tokens[1][1] == b'w' {
            perms |= PROT_WRITE;
        }
        if tokens[1][2] == b'x' {
            perms |= PROT_EXEC;
        }

        let Some(inode) = parse_dec_u64(tokens[4]) else {
            continue;
        };

        let pathname = match tokens.get(5) {
            Some(rest) => strip_leading_ascii_whitespace(rest).to_vec(),
            None => Vec::new(),
        };

        result.push(MemoryMapping {
            start_address,
            end_address,
            perms,
            offset,
            inode,
            pathname,
        });
    }

    result
}

/// The load bias of the image `mapping` is a piece of: what to subtract from
/// a runtime address in it to get the file's virtual address, which is also
/// where the file's virtual address 0 landed. `mappings` is the whole
/// process, `segments` the `(file offset, virtual address)` of each of the
/// file's loadable segments.
///
/// A segment's mapping has `start - offset = bias + (vaddr - offset)`, and
/// `vaddr - offset` is not one number per file. GNU ld pads the file so that
/// the text segment's equals the first segment's; lld does not pad, it bumps
/// the virtual address by a page at every segment boundary, so its text sits
/// a page or more further from its file offset than the headers do. Taking
/// `start - offset` from the text mapping and the correction from the first
/// segment named every sampled function of an lld-linked binary (Chrome,
/// Unreal, Rust since 1.90) after whatever lay 4 KiB below it.
///
/// Which segment a mapping came from cannot be read off its offset: in an lld
/// layout the page at a segment boundary is mapped twice, as the tail of one
/// segment and as the head of the next. So this looks for the first
/// segment's mapping instead: the mapping of the same file whose
/// `start - offset` is below this one's by a step the segment table allows.
/// When there is none (one mapping for the whole image, or a file that is not
/// readable) the mapping is taken to be of the first segment itself.
///
/// The lowest `start - offset` over all of the file's mappings is not the
/// answer either, for two reasons seen in the wild. A later segment can sit
/// *closer* to its offset than the first (the last one of a Go binary such as
/// snapd, 0x24000 closer). And a file can be mapped again away from its
/// image: libffi maps a page of its own text once more per trampoline table.
pub fn load_bias(mappings: &[MemoryMapping], mapping: &MemoryMapping, segments: &[(u64, u64)]) -> u64 {
    let first = segments
        .iter()
        .min_by_key(|(offset, _)| *offset)
        .map(|(offset, vaddr)| vaddr.wrapping_sub(*offset))
        .unwrap_or(0);
    let mapped_at = mapping.start_address.wrapping_sub(mapping.offset);
    // How far above the first segment's mapping this one is. All wrapping:
    // a mapping of a segment that sits closer to its offset than the first
    // is "above" it by a negative step, which as a u64 is still the largest.
    let mut step = 0;
    for other in mappings {
        if other.pathname != mapping.pathname {
            continue;
        }
        let below = mapped_at.wrapping_sub(other.start_address.wrapping_sub(other.offset));
        if below > step && segments.iter().any(|(offset, vaddr)| vaddr.wrapping_sub(*offset).wrapping_sub(first) == below) {
            step = below;
        }
    }
    mapped_at.wrapping_sub(step).wrapping_sub(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(m: &MemoryMapping) -> &str {
        std::str::from_utf8(&m.pathname).unwrap()
    }

    /// Mirrors `TEST(ReadLinuxMaps, ParseMaps)` in
    /// `src/ModuleUtils/ReadLinuxMapsTest.cpp`, assertion for assertion.
    #[test]
    fn parse_maps_matches_cpp_test() {
        const CONTENT: &[u8] = b"\
00400000-00452000 r-xp 00000000 08:02 173521      /usr/bin/dbus-daemon
00e03000-00e24000 rw-p 00000000 00:00 0           [heap]
35b1800000-35b1820000 r-xp 00000000 08:02 135522  /path with spaces
35b1a21000-35b1a22000 rw-p 00000000 00:00 0       
";
        let maps = parse_maps(CONTENT);
        assert_eq!(maps.len(), 4);

        assert_eq!(maps[0].start_address, 0x400000);
        assert_eq!(maps[0].end_address, 0x452000);
        assert_eq!(maps[0].perms, PROT_READ | PROT_EXEC);
        assert_eq!(maps[0].inode, 173521);
        assert_eq!(path(&maps[0]), "/usr/bin/dbus-daemon");

        assert_eq!(maps[1].start_address, 0xe03000);
        assert_eq!(maps[1].end_address, 0xe24000);
        assert_eq!(maps[1].perms, PROT_READ | PROT_WRITE);
        assert_eq!(maps[1].inode, 0);
        assert_eq!(path(&maps[1]), "[heap]");

        assert_eq!(maps[2].start_address, 0x35b1800000);
        assert_eq!(maps[2].end_address, 0x35b1820000);
        assert_eq!(maps[2].perms, PROT_READ | PROT_EXEC);
        assert_eq!(maps[2].inode, 135522);
        assert_eq!(path(&maps[2]), "/path with spaces");

        assert_eq!(maps[3].start_address, 0x35b1a21000);
        assert_eq!(maps[3].end_address, 0x35b1a22000);
        assert_eq!(maps[3].perms, PROT_READ | PROT_WRITE);
        assert_eq!(maps[3].inode, 0);
        assert_eq!(path(&maps[3]), "");
    }

    /// Mirrors `TEST(ReadLinuxMaps, ParseMapsFromInvalidProcPidMapsContent)`.
    #[test]
    fn parse_maps_from_invalid_content() {
        assert_eq!(parse_maps(b"").len(), 0);
        assert_eq!(parse_maps(b"\n\n").len(), 0);
        // Missing inode.
        assert_eq!(parse_maps(b"00400000-00452000 r-xp 00000000 08:02").len(), 0);
        // Unexpected protection format.
        assert_eq!(
            parse_maps(b"00400000-00452000 r-x 00000000 08:02 173521      /usr/bin/dbus-daemon")
                .len(),
            0
        );
        // Non-numeric inode.
        assert_eq!(
            parse_maps(b"00400000-00452000 r-xp 00000000 08:02 173521a      /usr/bin/dbus-daemon\n")
                .len(),
            0
        );
    }

    /// Not covered by the C++ suite: `std::stoull` would throw here.
    #[test]
    fn malformed_hex_is_skipped_not_fatal() {
        assert_eq!(parse_maps(b"zzzz-00452000 r-xp 0 08:02 0 /x").len(), 0);
        assert_eq!(parse_maps(b"00400000-zzzz r-xp 0 08:02 0 /x").len(), 0);
        assert_eq!(parse_maps(b"00400000-00452000 r-xp zz 08:02 0 /x").len(), 0);
        assert_eq!(parse_maps(b"-00452000 r-xp 0 08:02 0 /x").len(), 0);
    }

    /// Not covered by the C++ suite either: the kernel does not promise UTF-8.
    #[test]
    fn non_utf8_pathname_survives() {
        let mut content = b"00400000-00452000 r-xp 00000000 08:02 1 /tmp/".to_vec();
        content.extend_from_slice(&[0xff, 0xfe]);
        let maps = parse_maps(&content);
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].pathname, b"/tmp/\xff\xfe");
    }

    #[test]
    fn address_range_must_have_exactly_two_parts() {
        assert_eq!(parse_maps(b"00400000 r-xp 0 08:02 0 /x").len(), 0);
        assert_eq!(parse_maps(b"1-2-3 r-xp 0 08:02 0 /x").len(), 0);
    }

    #[test]
    fn overflowing_values_are_skipped_not_wrapped() {
        // u64::MAX is 16 hex digits; 17 significant ones cannot fit.
        assert_eq!(
            parse_maps(b"1ffffffffffffffff-1 r-xp 0 08:02 0 /x").len(),
            0
        );
        // Leading zeros are not significant, so this one is fine.
        assert_eq!(parse_maps(b"00000000000000001-2 r-xp 0 08:02 0 /x").len(), 1);
        assert_eq!(
            parse_maps(b"1-2 r-xp 0 08:02 99999999999999999999999 /x").len(),
            0
        );
    }

    /// The bias `load_bias` gives each executable mapping of `path`.
    fn text_biases(maps: &str, path: &str, segments: &[(u64, u64)]) -> Vec<u64> {
        let mappings = parse_maps(maps.as_bytes());
        mappings
            .iter()
            .filter(|m| m.perms & PROT_EXEC != 0 && m.pathname == path.as_bytes())
            .map(|m| load_bias(&mappings, m, segments))
            .collect()
    }

    // The layouts below are `readelf -lW` and `/proc/<pid>/maps` of real
    // processes: one C file linked four ways, then the binaries that broke
    // simpler rules.

    #[test]
    fn gnu_ld_layouts_have_the_bias_of_their_text_mapping() {
        // PIE. GNU ld pads the file, so text is as far from its offset as
        // the headers are; only the data segment is a page further.
        const PIE: &str = "\
60c4121b0000-60c4121b1000 r--p 00000000 00:2f 169005                     /t/hot_bfd
60c4121b1000-60c4121b3000 r-xp 00001000 00:2f 169005                     /t/hot_bfd
60c4121b3000-60c4121b4000 r--p 00003000 00:2f 169005                     /t/hot_bfd
60c4121b4000-60c4121b5000 r--p 00003000 00:2f 169005                     /t/hot_bfd
60c4121b5000-60c4121b6000 rw-p 00004000 00:2f 169005                     /t/hot_bfd
";
        let segments = [(0, 0), (0x1000, 0x1000), (0x3000, 0x3000), (0x3da0, 0x4da0)];
        assert_eq!(text_biases(PIE, "/t/hot_bfd", &segments), [0x60c4121b0000]);

        // Not PIE: linked at 0x400000 and loaded there, so no bias at all.
        const FIXED: &str = "\
00400000-00401000 r--p 00000000 00:2f 189223                             /t/hot_bfd_nopie
00401000-00402000 r-xp 00001000 00:2f 189223                             /t/hot_bfd_nopie
00402000-00403000 r--p 00002000 00:2f 189223                             /t/hot_bfd_nopie
00403000-00404000 r--p 00002000 00:2f 189223                             /t/hot_bfd_nopie
00404000-00405000 rw-p 00003000 00:2f 189223                             /t/hot_bfd_nopie
";
        let segments = [(0, 0x400000), (0x1000, 0x401000), (0x2000, 0x402000), (0x2df8, 0x403df8)];
        assert_eq!(text_biases(FIXED, "/t/hot_bfd_nopie", &segments), [0]);
    }

    #[test]
    fn lld_layouts_have_the_bias_of_their_first_mapping_not_of_their_text() {
        // PIE. lld does not pad: text is at file offset 0xc80 and virtual
        // address 0x1c80, so its mapping starts a page above `bias + offset`.
        const PIE: &str = "\
5992b139a000-5992b139b000 r--p 00000000 00:2f 169012                     /t/hot_lld
5992b139b000-5992b139e000 r-xp 00000000 00:2f 169012                     /t/hot_lld
5992b139e000-5992b139f000 r--p 00002000 00:2f 169012                     /t/hot_lld
5992b139f000-5992b13a0000 rw-p 00002000 00:2f 169012                     /t/hot_lld
";
        let segments = [(0, 0), (0xc80, 0x1c80), (0x2360, 0x4360), (0x2588, 0x5588)];
        assert_eq!(text_biases(PIE, "/t/hot_lld", &segments), [0x5992b139a000]);

        // Not PIE: lld's default image base, 0x200000, and the page bump.
        const FIXED: &str = "\
00200000-00201000 r--p 00000000 00:2f 189230                             /t/hot_lld_nopie
00201000-00203000 r-xp 00000000 00:2f 189230                             /t/hot_lld_nopie
00203000-00204000 r--p 00001000 00:2f 189230                             /t/hot_lld_nopie
00204000-00205000 rw-p 00001000 00:2f 189230                             /t/hot_lld_nopie
";
        let segments = [(0, 0x200000), (0xb80, 0x201b80), (0x1560, 0x203560), (0x1700, 0x204700)];
        assert_eq!(text_biases(FIXED, "/t/hot_lld_nopie", &segments), [0]);

        // An Unreal Shipping binary's program headers, mapped where they
        // ask to be (the maps are written out from the headers, not copied
        // from a process): text is 0x201000 from its offset, the headers
        // 0x200000.
        const UNREAL: &str = "\
00200000-03281000 r--p 00000000 103:02 77                                /t/Game-Linux-Shipping
03282000-0b22a000 r-xp 03081000 103:02 77                                /t/Game-Linux-Shipping
0b22b000-0b6f0000 rw-p 0b029000 103:02 77                                /t/Game-Linux-Shipping
";
        let segments = [(0, 0x200000), (0x3081000, 0x3282000), (0xb029ec0, 0xb22bec0)];
        assert_eq!(text_biases(UNREAL, "/t/Game-Linux-Shipping", &segments), [0]);
    }

    #[test]
    fn every_piece_of_a_split_text_mapping_has_the_same_bias() {
        // The lld PIE above after a hook engine made its second text page
        // writable. The first piece is now one page at file offset 0, the
        // same page the headers' mapping has: its offset cannot say which
        // segment it came from.
        const SPLIT: &str = "\
5992b139a000-5992b139b000 r--p 00000000 00:2f 169012                     /t/hot_lld
5992b139b000-5992b139c000 r-xp 00000000 00:2f 169012                     /t/hot_lld
5992b139c000-5992b139d000 rwxp 00001000 00:2f 169012                     /t/hot_lld
5992b139d000-5992b139e000 r-xp 00002000 00:2f 169012                     /t/hot_lld
5992b139e000-5992b139f000 r--p 00002000 00:2f 169012                     /t/hot_lld
5992b139f000-5992b13a0000 rw-p 00002000 00:2f 169012                     /t/hot_lld
";
        let segments = [(0, 0), (0xc80, 0x1c80), (0x2360, 0x4360), (0x2588, 0x5588)];
        assert_eq!(text_biases(SPLIT, "/t/hot_lld", &segments), [0x5992b139a000; 3]);
    }

    #[test]
    fn a_segment_closer_to_its_offset_than_the_first_does_not_lower_the_bias() {
        // snapd, a Go binary: the file has unloaded sections ahead of its
        // last segment, which is therefore 0x3dc000 from its offset where
        // every other segment is 0x400000. Its mapping has the lowest
        // `start - offset` of the file and is not where the image starts.
        const SNAPD: &str = "\
62ae32757000-62ae3359b000 r-xp 00000000 07:12 256                        /snap/snapd
62ae3359b000-62ae33c5a000 r--p 00e44000 07:12 256                        /snap/snapd
62ae33c5a000-62ae34839000 r--p 01503000 07:12 256                        /snap/snapd
62ae34839000-62ae348ab000 rw-p 020e2000 07:12 256                        /snap/snapd
62ae348e0000-62ae348e1000 rw-p 021ad000 07:12 256                        /snap/snapd
";
        let segments =
            [(0, 0x400000), (0xe44000, 0x1244000), (0x1503000, 0x1903000), (0x20e3000, 0x24e3000), (0x21ad000, 0x2589000)];
        assert_eq!(text_biases(SNAPD, "/snap/snapd", &segments), [0x62ae32757000 - 0x400000]);
        // From that last mapping the first segment's is *above*, and found.
        let mappings = parse_maps(SNAPD.as_bytes());
        assert_eq!(load_bias(&mappings, &mappings[4], &segments), 0x62ae32757000 - 0x400000);
    }

    #[test]
    fn a_file_mapped_again_elsewhere_does_not_move_its_image() {
        // libffi maps one page of its own text a second time for its
        // trampolines, here 58 MB below the library. That page is text at
        // its own address; the library is where the loader put it.
        const LIBFFI: &str = "\
7481b7178000-7481b7179000 r-xp 0000b000 103:02 100825209                 /usr/lib/libffi.so.8
7481baaa5000-7481baaa7000 r--p 00000000 103:02 100825209                 /usr/lib/libffi.so.8
7481baaa7000-7481baab2000 r-xp 00002000 103:02 100825209                 /usr/lib/libffi.so.8
7481baab2000-7481baab4000 r--p 0000d000 103:02 100825209                 /usr/lib/libffi.so.8
7481baab4000-7481baab5000 r--p 0000e000 103:02 100825209                 /usr/lib/libffi.so.8
7481baab5000-7481baab6000 rw-p 0000f000 103:02 100825209                 /usr/lib/libffi.so.8
";
        let segments = [(0, 0), (0x2000, 0x2000), (0xd000, 0xd000), (0xec70, 0xfc70)];
        assert_eq!(
            text_biases(LIBFFI, "/usr/lib/libffi.so.8", &segments),
            [0x7481b7178000 - 0xb000, 0x7481baaa5000]
        );
    }

    #[test]
    fn a_lone_mapping_is_taken_to_be_of_the_first_segment() {
        // Nothing else of the file is mapped, and no segments are known:
        // `start - offset` is all there is.
        const LONE: &str = "7f0000002000-7f0000005000 r-xp 00002000 08:02 7 /lib/x.so\n";
        assert_eq!(text_biases(LONE, "/lib/x.so", &[]), [0x7f0000000000]);
        assert_eq!(text_biases(LONE, "/lib/x.so", &[(0, 0x10000), (0x2000, 0x13000)]), [0x7f0000000000 - 0x10000]);
        // Another file's mapping at the right distance is not this file's.
        const TWO: &str = "\
7f0000000000-7f0000001000 r--p 00000000 08:02 8 /lib/y.so
7f0000002000-7f0000005000 r-xp 00001000 08:02 7 /lib/x.so
";
        assert_eq!(text_biases(TWO, "/lib/x.so", &[(0, 0), (0x1000, 0x2000)]), [0x7f0000001000]);
    }

    #[test]
    fn split_max_keeps_separators_in_the_tail() {
        assert_eq!(split_max(b"a b c d", b' ', 2), vec![&b"a"[..], b"b", b"c d"]);
        assert_eq!(split_max(b"a", b' ', 5), vec![&b"a"[..]]);
        assert_eq!(split_max(b"", b' ', 5), vec![&b""[..]]);
        assert_eq!(split_max(b"a  b", b' ', 5), vec![&b"a"[..], b"", b"b"]);
    }
}
