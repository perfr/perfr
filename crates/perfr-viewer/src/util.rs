// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Small string helpers more than one module needs. Each of these existed
//! in two or three copies that had started to disagree.

// Several of the callers are wasm-only.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

/// A query value with everything but unreserved characters percent-encoded.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub use crate::url::percent_decode;

/// A size in decimal units: `12.4 MB` is 12.4 million bytes. Under 1000
/// bytes the unit is `B` with no decimal.
pub fn fmt_bytes(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let n = bytes as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_encoded_down_to_unreserved_characters() {
        assert_eq!(percent_encode("Abc-_.~09"), "Abc-_.~09");
        assert_eq!(percent_encode("std::map<int, T>&x=1/é"), "std%3A%3Amap%3Cint%2C%20T%3E%26x%3D1%2F%C3%A9");
    }

    #[test]
    fn decoding_reads_the_bytes_as_utf8_and_keeps_what_is_not_an_escape() {
        assert_eq!(percent_decode("%2Ftraces%2Fa%20b.json"), "/traces/a b.json");
        // Two escapes, one character. A decoder that pushed each byte as a
        // char of its own turned this into "cafÃ©".
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        assert_eq!(percent_decode("a+b"), "a+b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode(&percent_encode("std::map<int, T>&x=1/é")), "std::map<int, T>&x=1/é");
    }

    #[test]
    fn a_percent_sign_before_a_wide_character_does_not_panic() {
        // "%a" + the first byte of "é" is not text, and not a char boundary.
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é0"), "%é0");
        assert_eq!(percent_decode("é%"), "é%");
    }

    #[test]
    fn sizes_are_decimal_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_000), "1.0 KB");
        assert_eq!(fmt_bytes(12_400_000), "12.4 MB");
        assert_eq!(fmt_bytes(1_500_000_000), "1.5 GB");
    }
}
