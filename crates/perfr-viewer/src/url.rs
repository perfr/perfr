// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Query-string decoding shared by the viewer's page and `perfr-client`.

/// Undoes percent-encoding. The decoded bytes are read as UTF-8, so
/// `%C3%A9` is one character; a `%` not followed by two hex digits stays as
/// it is, and so does `+`: a `+` is a space only in form encoding, and a
/// path or a signed URL can hold a literal one. Works on bytes throughout:
/// slicing the text at `%` plus two could land inside a character and panic.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Digit by digit: `u8::from_str_radix` also takes a sign, which
            // read `%+5` as the byte 0x05.
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_escapes_are_decoded_as_utf8() {
        assert_eq!(percent_decode("%2Ftraces%2Fa%20b.json"), "/traces/a b.json");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("%2f%2F"), "//");
    }

    #[test]
    fn a_plus_is_not_a_space() {
        assert_eq!(percent_decode("runs/build+debug.perfr.stream"), "runs/build+debug.perfr.stream");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
    }

    #[test]
    fn a_percent_without_two_hex_digits_stays() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        // A sign is not a hex digit.
        assert_eq!(percent_decode("%+5"), "%+5");
        assert_eq!(percent_decode("%-1"), "%-1");
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é0"), "%é0");
        assert_eq!(percent_decode("é%"), "é%");
    }
}
