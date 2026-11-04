// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Unpack the release archives the installers already use.
//!
//! Linux and macOS publish a gzipped ustar whose top directory is `perfr/`.
//! Windows publishes a zip with the same layout. Both are produced by
//! `.github/workflows/release-rust.yml`.

use std::io::Read;

use flate2::read::{DeflateDecoder, GzDecoder};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedFile {
    /// Path inside the archive, using `/`, with the leading `perfr/` kept.
    pub path: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

pub fn unpack_named(name: &str, bytes: &[u8]) -> Result<Vec<PackedFile>, String> {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let tar = gunzip(bytes)?;
        unpack_tar(&tar)
    } else if lower.ends_with(".zip") {
        unpack_zip(bytes)
    } else {
        Err(format!("unsupported archive {name}"))
    }
}

fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| format!("gzip: {error}"))?;
    Ok(out)
}

fn unpack_tar(bytes: &[u8]) -> Result<Vec<PackedFile>, String> {
    let mut offset = 0usize;
    let mut long_name: Option<String> = None;
    let mut files = Vec::new();
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        if !header_checksum_ok(header) {
            return Err("tar header checksum mismatch".into());
        }
        let size = parse_octal(&header[124..136])?;
        let mode = parse_octal(&header[100..108]).unwrap_or(0);
        let typeflag = header[156];
        let name = long_name.take().unwrap_or_else(|| tar_name(header));
        offset += 512;
        let data_len =
            usize::try_from(size).map_err(|_| format!("tar entry {name} is too large"))?;
        if offset + data_len > bytes.len() {
            return Err(format!("truncated tar entry {name}"));
        }
        let data = &bytes[offset..offset + data_len];
        offset += data_len.div_ceil(512) * 512;
        match typeflag {
            b'L' => {
                long_name = Some(
                    String::from_utf8_lossy(data)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'0' | b'\0' => files.push(PackedFile {
                path: name,
                bytes: data.to_vec(),
                executable: mode & 0o111 != 0,
            }),
            b'5' => {}
            _ => {}
        }
    }
    if files.is_empty() {
        return Err("archive contains no files".into());
    }
    Ok(files)
}

fn tar_name(header: &[u8]) -> String {
    let name = c_string(&header[..100]);
    let prefix = c_string(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn parse_octal(bytes: &[u8]) -> Result<u64, String> {
    let text = c_string(bytes);
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text.trim(), 8).map_err(|_| format!("bad octal field {text:?}"))
}

fn header_checksum_ok(header: &[u8]) -> bool {
    let Ok(stored) = parse_octal(&header[148..156]) else {
        return false;
    };
    let mut sum = 0u32;
    for (index, byte) in header.iter().enumerate() {
        if (148..156).contains(&index) {
            sum += u32::from(b' ');
        } else {
            sum += u32::from(*byte);
        }
    }
    stored == u64::from(sum)
}

fn unpack_zip(bytes: &[u8]) -> Result<Vec<PackedFile>, String> {
    let eocd = find_eocd(bytes)?;
    let count = u16::from_le_bytes(bytes[eocd + 10..eocd + 12].try_into().unwrap()) as usize;
    let cd_size = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as usize;
    let cd_offset = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    if cd_offset.saturating_add(cd_size) > bytes.len() {
        return Err("zip central directory is out of range".into());
    }
    let mut cursor = cd_offset;
    let mut files = Vec::new();
    for _ in 0..count {
        if cursor + 46 > bytes.len() || &bytes[cursor..cursor + 4] != b"PK\x01\x02" {
            return Err("bad zip central directory".into());
        }
        let flags = u16::from_le_bytes(bytes[cursor + 8..cursor + 10].try_into().unwrap());
        let method = u16::from_le_bytes(bytes[cursor + 10..cursor + 12].try_into().unwrap());
        let comp_size =
            u32::from_le_bytes(bytes[cursor + 20..cursor + 24].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(bytes[cursor + 28..cursor + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(bytes[cursor + 30..cursor + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(bytes[cursor + 32..cursor + 34].try_into().unwrap()) as usize;
        let local_off =
            u32::from_le_bytes(bytes[cursor + 42..cursor + 46].try_into().unwrap()) as usize;
        let external = u32::from_le_bytes(bytes[cursor + 38..cursor + 42].try_into().unwrap());
        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        if name_end > bytes.len() {
            return Err("zip name is out of range".into());
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_end]).replace('\\', "/");
        cursor = name_end + extra_len + comment_len;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(format!("zip entry {name} is encrypted"));
        }
        let data = zip_entry_bytes(bytes, local_off, comp_size)?;
        let plain = match method {
            0 => data,
            8 => inflate_raw(&data).map_err(|error| format!("zip {name}: {error}"))?,
            other => return Err(format!("zip entry {name} uses compression method {other}")),
        };
        // Unix mode lives in the high 16 bits when the creator did that.
        // A zero external attribute (PowerShell) is a normal file.
        let mode = (external >> 16) & 0o7777;
        files.push(PackedFile {
            path: name,
            bytes: plain,
            executable: mode & 0o111 != 0,
        });
    }
    if files.is_empty() {
        return Err("archive contains no files".into());
    }
    Ok(files)
}

fn zip_entry_bytes(bytes: &[u8], local_off: usize, comp_size: usize) -> Result<Vec<u8>, String> {
    if local_off + 30 > bytes.len() || &bytes[local_off..local_off + 4] != b"PK\x03\x04" {
        return Err("zip local header is missing".into());
    }
    let name_len =
        u16::from_le_bytes(bytes[local_off + 26..local_off + 28].try_into().unwrap()) as usize;
    let extra_len =
        u16::from_le_bytes(bytes[local_off + 28..local_off + 30].try_into().unwrap()) as usize;
    let start = local_off + 30 + name_len + extra_len;
    let end = start + comp_size;
    if end > bytes.len() {
        return Err("zip entry data is out of range".into());
    }
    Ok(bytes[start..end].to_vec())
}

fn inflate_raw(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = DeflateDecoder::new(data);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|error| error.to_string())?;
    Ok(out)
}

fn find_eocd(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < 22 {
        return Err("zip is too small".into());
    }
    let start = bytes.len().saturating_sub(22 + 65_535);
    let mut index = bytes.len() - 22;
    loop {
        if &bytes[index..index + 4] == b"PK\x05\x06" {
            let comment =
                u16::from_le_bytes(bytes[index + 20..index + 22].try_into().unwrap()) as usize;
            if index + 22 + comment == bytes.len() {
                return Ok(index);
            }
        }
        if index == start {
            break;
        }
        index -= 1;
    }
    Err("zip end of central directory not found".into())
}

/// Drop the leading `perfr/` directory the release archives use.
pub fn install_relative(path: &str) -> Option<String> {
    let path = path.trim_start_matches("./").replace('\\', "/");
    let rest = path.strip_prefix("perfr/").unwrap_or(path.as_str());
    if rest.is_empty() || rest.ends_with('/') || rest.split('/').any(|part| part == "..") {
        return None;
    }
    Some(rest.to_string())
}

#[cfg(test)]
pub(crate) fn pack_tar_gz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut raw = Vec::new();
    for (path, bytes, mode) in files {
        raw.extend(tar_header(path, bytes.len() as u64, *mode));
        raw.extend_from_slice(bytes);
        let pad = (512 - (bytes.len() % 512)) % 512;
        raw.extend(std::iter::repeat(0).take(pad));
    }
    raw.extend(std::iter::repeat(0).take(1024));
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&raw).unwrap();
    encoder.finish().unwrap()
}

#[cfg(test)]
fn tar_header(path: &str, size: u64, mode: u32) -> [u8; 512] {
    let mut header = [0u8; 512];
    let (prefix, name) = if path.len() <= 100 {
        ("", path)
    } else {
        let split = path[..path.len() - 100].rfind('/').unwrap();
        (&path[..split], &path[split + 1..])
    };
    header[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut header[100..108], u64::from(mode));
    write_octal(&mut header[124..136], size);
    header[156] = b'0';
    header[257..262].copy_from_slice(b"ustar");
    header[263] = b'0';
    header[264] = b'0';
    if !prefix.is_empty() {
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    }
    let mut sum = 0u32;
    for (index, byte) in header.iter().enumerate() {
        if (148..156).contains(&index) {
            sum += u32::from(b' ');
        } else {
            sum += u32::from(*byte);
        }
    }
    write_octal(&mut header[148..156], u64::from(sum));
    header
}

#[cfg(test)]
fn write_octal(slot: &mut [u8], value: u64) {
    let text = format!("{value:o}");
    let len = slot.len();
    let start = len - 1 - text.len();
    slot[..len - 1].fill(b'0');
    slot[start..start + text.len()].copy_from_slice(text.as_bytes());
    slot[len - 1] = 0;
}

#[cfg(test)]
pub(crate) fn pack_zip_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut locals = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0u32;
    for (path, bytes) in files {
        let name = path.as_bytes();
        let mut local = Vec::new();
        local.extend_from_slice(b"PK\x03\x04");
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u32.to_le_bytes());
        local.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        local.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name);
        local.extend_from_slice(bytes);
        let mut entry = Vec::new();
        entry.extend_from_slice(b"PK\x01\x02");
        entry.extend_from_slice(&20u16.to_le_bytes());
        entry.extend_from_slice(&20u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u32.to_le_bytes());
        entry.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        entry.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        entry.extend_from_slice(&(name.len() as u16).to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u16.to_le_bytes());
        entry.extend_from_slice(&0u32.to_le_bytes());
        entry.extend_from_slice(&offset.to_le_bytes());
        entry.extend_from_slice(name);
        offset += local.len() as u32;
        locals.extend(local);
        central.extend(entry);
    }
    let cd_offset = locals.len() as u32;
    let cd_size = central.len() as u32;
    let mut out = locals;
    out.extend(central);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_gz_round_trip_keeps_the_perfr_layout() {
        let packed = pack_tar_gz(&[
            ("perfr/perfr", b"bin", 0o755),
            ("perfr/libperfr_frida_agent.so", b"so", 0o644),
        ]);
        let files = unpack_named("perfr-linux-x86_64.tar.gz", &packed).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "perfr/perfr");
        assert!(files[0].executable);
        assert_eq!(files[0].bytes, b"bin");
        assert!(!files[1].executable);
        assert_eq!(install_relative(&files[0].path).as_deref(), Some("perfr"));
    }

    #[test]
    fn zip_round_trip_and_rejects_parent_paths() {
        let packed =
            pack_zip_stored(&[("perfr/perfr.exe", b"exe"), ("perfr/include/perfr.h", b"h")]);
        let files = unpack_named("perfr-windows-x86_64.zip", &packed).unwrap();
        assert_eq!(files[0].bytes, b"exe");
        assert_eq!(files[1].bytes, b"h");
        assert_eq!(
            install_relative(&files[1].path).as_deref(),
            Some("include/perfr.h")
        );
        assert_eq!(install_relative("perfr/../../etc/passwd"), None);
    }
}
