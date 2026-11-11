// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Build identity shared by `perfr --version` and the live viewer's About window.
//!
//! `build.rs` stamps the `perfr` package version, the short git hash, the UTC
//! build date and, on a release, the `perfr-v*` tag. A build with no release
//! tag reports `dev` plus that hash.

// The build script's; compiled here only so its test runs.
#[cfg(test)]
mod git_paths;
mod stamp;

pub use stamp::{
    commit_url, format_long_version, format_unix_date, format_version_line, is_commit_hash,
    is_release_tag, version_label,
};

/// `crates/perfr` package version. `perfr update` compares this, not the
/// `dev+hash` label a local build shows.
pub fn package_version() -> &'static str {
    env!("PERFR_PKG_VERSION")
}

pub fn git_hash() -> &'static str {
    env!("PERFR_GIT_HASH")
}

pub fn build_date() -> &'static str {
    env!("PERFR_BUILD_DATE")
}

/// Empty when this build is not a release.
pub fn release_tag() -> &'static str {
    env!("PERFR_RELEASE_TAG")
}

pub fn version_label_str() -> String {
    version_label(package_version(), git_hash(), release_tag())
}

pub fn long_version() -> String {
    format_long_version(package_version(), git_hash(), build_date(), release_tag())
}

pub fn version_line() -> String {
    format_version_line(package_version(), git_hash(), build_date(), release_tag())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_version_is_dev_plus_the_hash() {
        let line = format_version_line("0.1.3", "abc1234def56", "2026-11-08", "");
        assert_eq!(line, "perfr dev+abc1234def56 (abc1234def56, 2026-11-08)");
    }

    #[test]
    fn release_version_carries_the_tag() {
        let line = format_version_line("0.1.3", "abc1234def56", "2026-11-08", "perfr-v0.1.3");
        assert_eq!(line, "perfr 0.1.3 (abc1234def56, 2026-11-08, perfr-v0.1.3)");
        assert_eq!(
            version_label("0.1.3", "abc1234def56", "perfr-v0.1.3"),
            "0.1.3"
        );
    }

    #[test]
    fn prerelease_tag_is_a_release_stamp() {
        assert!(is_release_tag("perfr-v0.1.3-rc.1"));
        assert!(is_release_tag("perfr-v1.2.3.beta.1"));
        let line = format_version_line("0.1.3", "abc1234def56", "2026-11-08", "perfr-v0.1.3-rc.1");
        assert_eq!(
            line,
            "perfr 0.1.3 (abc1234def56, 2026-11-08, perfr-v0.1.3-rc.1)"
        );
    }

    #[test]
    fn other_tags_are_not_releases() {
        for tag in [
            "",
            "dev",
            "v0.1.3",
            "perfr-v0.1",
            "perfr-v0.1.3-rc-1",
            "nightly-1",
        ] {
            assert!(!is_release_tag(tag), "{tag}");
        }
    }

    #[test]
    fn commit_link_uses_the_short_hash() {
        assert_eq!(
            commit_url("abc1234def56").as_deref(),
            Some("https://github.com/perfr/perfr/commit/abc1234def56")
        );
        assert_eq!(
            commit_url("ABCDEF123456").as_deref(),
            Some("https://github.com/perfr/perfr/commit/ABCDEF123456")
        );
        assert!(commit_url("unknown").is_none());
        assert!(commit_url("nogit").is_none());
        assert!(commit_url("abc").is_none());
    }

    #[test]
    fn unix_epoch_is_1970() {
        assert_eq!(format_unix_date(0), "1970-01-01");
        assert_eq!(format_unix_date(86_400), "1970-01-02");
    }

    #[test]
    fn package_version_matches_the_perfr_manifest() {
        let text = include_str!("../../perfr/Cargo.toml");
        let version = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        });
        assert_eq!(Some(package_version()), version);
    }

    #[test]
    fn stamped_line_matches_the_baked_parts() {
        let line = version_line();
        assert_eq!(
            line,
            format_version_line(package_version(), git_hash(), build_date(), release_tag())
        );
        assert!(line.starts_with("perfr "), "{line}");
        assert!(line.contains(git_hash()), "{line}");
        assert!(line.contains(build_date()), "{line}");
        assert_eq!(build_date().len(), 10, "{}", build_date());
        if release_tag().is_empty() {
            assert!(line.contains(&format!("dev+{}", git_hash())), "{line}");
        } else {
            assert!(is_release_tag(release_tag()), "{}", release_tag());
            assert!(line.contains(package_version()), "{line}");
            assert!(line.contains(release_tag()), "{line}");
        }
    }
}
