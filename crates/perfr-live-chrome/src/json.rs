// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Incremental JSON helpers. Scan complete values without allocating a tree.

pub fn skip_ws(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && buf[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// End index (exclusive) of the JSON value starting at `start`, or `None` if
/// the buffer does not yet hold a complete value.
pub fn value_end(buf: &[u8], start: usize) -> Option<usize> {
    let i = skip_ws(buf, start);
    if i >= buf.len() {
        return None;
    }
    match buf[i] {
        b'{' | b'[' => container_end(buf, i),
        b'"' => string_end(buf, i),
        b't' => keyword_end(buf, i, b"true"),
        b'f' => keyword_end(buf, i, b"false"),
        b'n' => keyword_end(buf, i, b"null"),
        b'-' | b'0'..=b'9' => number_end(buf, i),
        _ => None,
    }
}

/// [`value_end`] for a value that arrives in pieces: it keeps its place.
///
/// `value_end` starts over from the value's first byte on every call, so a
/// value much larger than a chunk (a `stackFrames` table, a metadata blob,
/// a `systemTraceEvents` string) was scanned again from the start each time
/// more of it arrived: a 16 MiB value in 64 KiB pieces is 2 GiB of
/// scanning. This reads each byte once.
///
/// Between calls the caller must keep the value where it is (the same
/// `start` for the same bytes, more of them each time), until `end` has
/// returned its end or [`Self::reset`] has given it up.
#[derive(Default)]
pub struct ValueScan {
    /// Bytes from `start` already read; 0 before the value has begun.
    scanned: usize,
    depth: u32,
    in_str: bool,
    escape: bool,
}

impl ValueScan {
    pub fn end(&mut self, buf: &[u8], start: usize) -> Option<usize> {
        let mut i = start + self.scanned;
        if self.scanned == 0 {
            let first = skip_ws(buf, start);
            match buf.get(first)? {
                b'{' | b'[' => self.depth = 1,
                b'"' => self.in_str = true,
                // Numbers and keywords are short: nothing to keep.
                _ => return value_end(buf, start),
            }
            i = first + 1;
        }
        while i < buf.len() {
            let c = buf[i];
            i += 1;
            if self.in_str {
                if self.escape {
                    self.escape = false;
                } else if c == b'\\' {
                    self.escape = true;
                } else if c == b'"' {
                    self.in_str = false;
                }
            } else {
                match c {
                    b'"' => self.in_str = true,
                    b'{' | b'[' => self.depth += 1,
                    // Either closer closes, as in `container_end`.
                    b'}' | b']' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                }
            }
            if self.depth == 0 && !self.in_str {
                self.reset();
                return Some(i);
            }
        }
        self.scanned = i - start;
        None
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// For a value that is only being skipped: how many of the bytes read
    /// so far the caller may drop from the front of its buffer. The scan
    /// then continues from the new front.
    pub fn forget_scanned(&mut self) -> usize {
        // One byte stays: zero bytes read means "not begun".
        let dropped = self.scanned.saturating_sub(1);
        self.scanned -= dropped;
        dropped
    }
}

fn keyword_end(buf: &[u8], start: usize, kw: &[u8]) -> Option<usize> {
    if start + kw.len() > buf.len() {
        return None;
    }
    if &buf[start..start + kw.len()] == kw {
        Some(start + kw.len())
    } else {
        None
    }
}

/// `None` also when the digits run to the end of the buffer: the number may
/// go on in the next chunk, and `12` then `345` is not two values.
fn number_end(buf: &[u8], start: usize) -> Option<usize> {
    let end = number_token_end(buf, start)?;
    (end < buf.len()).then_some(end)
}

fn number_token_end(buf: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    if buf[i] == b'-' {
        i += 1;
        if i >= buf.len() {
            return None;
        }
    }
    let mut saw_digit = false;
    while i < buf.len() && buf[i].is_ascii_digit() {
        saw_digit = true;
        i += 1;
    }
    if !saw_digit {
        return None;
    }
    if i < buf.len() && buf[i] == b'.' {
        i += 1;
        let mut frac = false;
        while i < buf.len() && buf[i].is_ascii_digit() {
            frac = true;
            i += 1;
        }
        if !frac {
            return None;
        }
    }
    if i < buf.len() && (buf[i] == b'e' || buf[i] == b'E') {
        i += 1;
        if i < buf.len() && (buf[i] == b'+' || buf[i] == b'-') {
            i += 1;
        }
        let mut exp = false;
        while i < buf.len() && buf[i].is_ascii_digit() {
            exp = true;
            i += 1;
        }
        if !exp {
            return None;
        }
    }
    Some(i)
}

fn string_end(buf: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(buf[start], b'"');
    let mut i = start + 1;
    let mut escape = false;
    while i < buf.len() {
        let c = buf[i];
        if escape {
            escape = false;
        } else if c == b'\\' {
            escape = true;
        } else if c == b'"' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

fn container_end(buf: &[u8], start: usize) -> Option<usize> {
    let open = buf[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 1u32;
    let mut i = start + 1;
    let mut in_str = false;
    let mut escape = false;
    while i < buf.len() {
        let c = buf[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if c == close {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                } else if (c == b'}' && open == b'[') || (c == b']' && open == b'{') {
                    // mismatched closer — still count down if we opened the other
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Parse a JSON string token (`"..."`) into an owned UTF-8 string.
pub fn parse_json_string(token: &[u8]) -> Option<String> {
    serde_json::from_slice::<String>(token).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_object_and_array() {
        let buf = br#"  {"a":[1,2],"b":"x\"y"}  extra"#;
        let end = value_end(buf, 0).unwrap();
        assert_eq!(&buf[..end], br#"  {"a":[1,2],"b":"x\"y"}"#);
        assert!(value_end(br#"{"a":1"#, 0).is_none());
    }

    #[test]
    fn scans_number_and_kw() {
        assert_eq!(value_end(b"12.5e-2,", 0), Some(7));
        assert_eq!(value_end(b"true,", 0), Some(4));
        assert_eq!(value_end(b"null", 0), Some(4));
    }

    #[test]
    fn a_number_at_the_end_of_the_buffer_may_not_be_all_of_it() {
        assert_eq!(value_end(b"12", 0), None);
        assert_eq!(value_end(b"-1.5e", 0), None);
        assert_eq!(value_end(b"12}", 0), Some(2));
        assert_eq!(value_end(b"12 ", 0), Some(2));
    }

    /// Fed a byte more at a time, from any split point, the scan that keeps
    /// its place ends where the scan of the whole value does.
    #[test]
    fn a_scan_resumed_at_any_byte_ends_where_a_single_scan_does() {
        let values: [&[u8]; 9] = [
            br#"{"a":[1,2,{"b":"x\"y]}"}],"c":"\\"} tail"#,
            br#"  [[],{},"]","[",-1.5e3,null,true] ,"#,
            br#""a string with \" and \\ and ] } in it" :"#,
            br#"{} "#,
            br#""" "#,
            b"12.5e-2,",
            b"false]",
            br#"{"unbalanced":[}} rest"#,
            br#"{"a":"\\\\"},"#,
        ];
        for v in values {
            let whole = value_end(v, 0);
            assert!(whole.is_some(), "{:?}", std::str::from_utf8(v));
            for first in 0..=v.len() {
                let mut scan = ValueScan::default();
                let mut got = scan.end(&v[..first], 0);
                let mut have = first;
                while got.is_none() && have < v.len() {
                    have += 1;
                    got = scan.end(&v[..have], 0);
                }
                assert_eq!(got, whole, "{:?} from {first}", std::str::from_utf8(v));
            }
        }
    }

    #[test]
    fn a_skipped_value_can_be_dropped_as_it_is_scanned() {
        let v = br#"{"k":["a]","}"],"n":{"m":1}} next"#;
        let end = value_end(v, 0).unwrap();
        for step in 1..8 {
            // `buf` holds only what has not been dropped yet.
            let mut scan = ValueScan::default();
            let mut buf: Vec<u8> = Vec::new();
            let mut dropped = 0;
            let mut got = None;
            for piece in v.chunks(step) {
                buf.extend_from_slice(piece);
                if let Some(e) = scan.end(&buf, 0) {
                    got = Some(dropped + e);
                    break;
                }
                let n = scan.forget_scanned();
                buf.drain(..n);
                dropped += n;
                assert!(buf.len() <= step + 1, "kept {} bytes", buf.len());
            }
            assert_eq!(got, Some(end), "in pieces of {step}");
        }
    }
}
