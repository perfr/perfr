// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure formatting for the version line. `build.rs` bakes the inputs;
//! `perfr --version` and the viewer's About window both call these.

/// `perfr-vX.Y.Z` or `perfr-vX.Y.Z` plus a `.`/`-` suffix of letters, digits
/// and dots. The same shape `release-rust.yml` accepts.
pub fn is_release_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix("perfr-v") else {
        return false;
    };
    let bytes = rest.as_bytes();
    let mut i = 0;
    for dot in [true, true, false] {
        i = match take_digits(bytes, i) {
            Some(next) => next,
            None => return false,
        };
        if dot {
            if bytes.get(i) != Some(&b'.') {
                return false;
            }
            i += 1;
        }
    }
    if i == bytes.len() {
        return true;
    }
    let sep = bytes[i];
    if sep != b'-' && sep != b'.' {
        return false;
    }
    let suffix = &rest[i + 1..];
    !suffix.is_empty()
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

fn take_digits(bytes: &[u8], mut i: usize) -> Option<usize> {
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        None
    } else {
        Some(i)
    }
}

/// A git object name `commit_url` is willing to link. Short or full.
pub fn is_commit_hash(hash: &str) -> bool {
    (7..=40).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn commit_url(hash: &str) -> Option<String> {
    if is_commit_hash(hash) {
        Some(format!("https://github.com/perfr/perfr/commit/{hash}"))
    } else {
        None
    }
}

/// What the version field shows. A release build uses the `perfr` package
/// version. Anything else is `dev` plus the short hash.
pub fn version_label(package_version: &str, hash: &str, release_tag: &str) -> String {
    if release_tag.is_empty() {
        format!("dev+{hash}")
    } else {
        package_version.to_string()
    }
}

/// `dev+abc1234def56 (abc1234def56, 2026-11-08)` locally, or
/// `0.1.3 (abc1234def56, 2026-11-08, perfr-v0.1.3)` for a release tag.
pub fn format_long_version(
    package_version: &str,
    hash: &str,
    date: &str,
    release_tag: &str,
) -> String {
    let label = version_label(package_version, hash, release_tag);
    if release_tag.is_empty() {
        format!("{label} ({hash}, {date})")
    } else {
        format!("{label} ({hash}, {date}, {release_tag})")
    }
}

/// What `perfr --version` prints, and the line in the About window.
pub fn format_version_line(
    package_version: &str,
    hash: &str,
    date: &str,
    release_tag: &str,
) -> String {
    format!(
        "perfr {}",
        format_long_version(package_version, hash, date, release_tag)
    )
}

/// UTC calendar date from a Unix timestamp. Howard Hinnant's civil_from_days.
pub fn format_unix_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}
