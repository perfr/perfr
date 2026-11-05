// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr update`: replace this install with a GitHub release of perfr/perfr.
//!
//! The asset names and the on-disk layout match `tools/install/install.sh`
//! and `tools/install/install.ps1`: a `perfr/` directory of the binary, the
//! function-hook helper and agent, checksummed by the `.sha256` asset
//! (sha256sum format) or `SHA256SUMS`. A cargo, Homebrew, or pip install is
//! not overwritten; the command that owns it is printed instead.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::archive::{self, PackedFile};
use crate::build_info;

const LATEST_URL: &str = "https://api.github.com/repos/perfr/perfr/releases/latest";
const TAG_URL: &str = "https://api.github.com/repos/perfr/perfr/releases/tags/";

#[derive(Clone, Debug)]
pub struct UpdateOpts {
    pub check: bool,
    pub tag: Option<String>,
    pub yes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}

impl Platform {
    pub fn host() -> Self {
        let os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "other"
        };
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "other"
        };
        Self { os, arch }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceStyle {
    /// Unix: `rename` over the destination, which is atomic on one filesystem.
    Atomic,
    /// Windows: a running image cannot be unlinked. Rename it aside, then
    /// move the new file into place, and remove the aside copy if it lets us.
    WindowsAside,
}

impl ReplaceStyle {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::WindowsAside
        } else {
            Self::Atomic
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    UpdateAvailable,
    UpToDate,
    Ahead,
}

#[derive(Clone, Debug)]
pub struct Manager {
    pub name: &'static str,
    pub command: String,
}

pub trait Fetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

pub struct CommandFetch;

impl Fetch for CommandFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        http_get(url)
    }
}

pub fn run(opts: UpdateOpts) -> i32 {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("perfr"));
    match run_with(
        &opts,
        &CommandFetch,
        &exe,
        Platform::host(),
        ReplaceStyle::host(),
        &real_manager_hints(&exe),
    ) {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            0
        }
        Err(error) => {
            eprintln!("perfr update: {error}");
            2
        }
    }
}

pub fn run_with(
    opts: &UpdateOpts,
    fetch: &dyn Fetch,
    exe: &Path,
    platform: Platform,
    style: ReplaceStyle,
    manager: &Option<Manager>,
) -> Result<String, String> {
    let tag = match &opts.tag {
        Some(tag) => Some(normalize_tag(tag)?),
        None => None,
    };
    let url = match &tag {
        Some(tag) => format!("{TAG_URL}{tag}"),
        None => LATEST_URL.to_string(),
    };
    let body = fetch.get(&url).map_err(|error| {
        format!("{error}\n({url})\nNo stable perfr-v* release was downloaded. The v1.* and nightly-* tags are the legacy build.")
    })?;
    let release = parse_release(&String::from_utf8_lossy(&body))?;
    let relation = compare_versions(build_info::package_version(), &release.tag);
    let asset_name = asset_for(platform)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .cloned()
        .ok_or_else(|| format!("release {} has no {asset_name}", release.tag))?;
    let checksum_asset = checksum_asset(&release, &asset_name);
    let mut text = String::new();
    text.push_str(&build_info::version_line());
    text.push('\n');
    text.push_str(&format!("latest release: {}\n", release.tag));
    text.push_str(&format!("asset: {}\n", asset.name));
    text.push_str(&format!(
        "checksum: {}\n",
        if checksum_asset.is_some() {
            "published"
        } else {
            "not published"
        }
    ));
    if let Some(manager) = manager {
        text.push_str(&format!("status: installed by {}\n", manager.name));
        text.push_str(&format!("update with: {}\n", manager.command));
        if opts.yes && !opts.check {
            return Err(format!(
                "this install is managed by {}; not replacing the binary\nupdate with: {}",
                manager.name, manager.command
            ));
        }
        return Ok(text);
    }
    let explicit = opts.tag.is_some();
    let should_install = match relation {
        Relation::UpdateAvailable => true,
        Relation::UpToDate => explicit,
        Relation::Ahead => explicit,
    };
    if !should_install {
        text.push_str(match relation {
            Relation::UpToDate => "status: up to date\n",
            Relation::Ahead => "status: this build is newer than the latest release\n",
            Relation::UpdateAvailable => "status: update available\n",
        });
        return Ok(text);
    }
    text.push_str(match relation {
        Relation::Ahead => "status: downgrade requested\n",
        Relation::UpToDate => "status: reinstall requested\n",
        Relation::UpdateAvailable => "status: update available\n",
    });
    if opts.check || !opts.yes {
        text.push_str("re-run with --yes to replace this install\n");
        return Ok(text);
    }
    if checksum_asset.is_none() {
        return Err(format!(
            "no checksum published for {}; refusing to replace the binary",
            asset.name
        ));
    }
    let archive = fetch.get(&asset.url)?;
    let sums = fetch.get(&checksum_asset.unwrap().url)?;
    verify_checksum(&archive, &String::from_utf8_lossy(&sums), &asset.name)?;
    let files = archive::unpack_named(&asset.name, &archive)?;
    let dest = exe.parent().ok_or("the running binary has no directory")?;
    install_tree(dest, &files, style)?;
    text.push_str(&format!("replaced install in {}\n", dest.display()));
    text.push_str("restart perfr to run the new binary\n");
    Ok(text)
}

pub fn parse_release(text: &str) -> Result<Release, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("release JSON: {error}"))?;
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("release JSON has no tag_name")?
        .to_string();
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or("release JSON has no assets")?
        .iter()
        .filter_map(|asset| {
            Some(Asset {
                name: asset.get("name")?.as_str()?.to_string(),
                url: asset.get("browser_download_url")?.as_str()?.to_string(),
            })
        })
        .collect();
    Ok(Release { tag, assets })
}

pub fn asset_for(platform: Platform) -> Result<&'static str, String> {
    match (platform.os, platform.arch) {
        ("linux", "x86_64") => Ok("perfr-linux-x86_64.tar.gz"),
        ("macos", "x86_64" | "aarch64") => Ok("perfr-macos-universal.tar.gz"),
        ("windows", "x86_64" | "aarch64") => Ok("perfr-windows-x86_64.zip"),
        ("linux", "aarch64") => Err(
            "no published Linux arm64 build. The release is a static Linux x86-64 binary and a macOS universal binary."
                .into(),
        ),
        _ => Err(format!("no release asset for {}/{}", platform.os, platform.arch)),
    }
}

fn checksum_asset(release: &Release, asset_name: &str) -> Option<Asset> {
    let sidecar = format!("{asset_name}.sha256");
    if let Some(asset) = release.assets.iter().find(|asset| asset.name == sidecar) {
        return Some(asset.clone());
    }
    release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .cloned()
}

pub fn verify_checksum(bytes: &[u8], sums: &str, asset_name: &str) -> Result<(), String> {
    let want = expected_hash(sums, asset_name)?;
    let got = hex_encode(&sha256(bytes));
    if !want.eq_ignore_ascii_case(&got) {
        return Err(format!(
            "checksum mismatch\n  expected {want}\n  got      {got}"
        ));
    }
    Ok(())
}

fn expected_hash(sums: &str, asset_name: &str) -> Result<String, String> {
    let mut bare = None;
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or("");
        if !is_sha256_hex(hash) {
            continue;
        }
        match parts.next() {
            Some(name) if name == asset_name || name.ends_with(&format!("/{asset_name}")) => {
                return Ok(hash.to_string());
            }
            None => bare = Some(hash.to_string()),
            Some(_) => {}
        }
    }
    bare.ok_or_else(|| format!("checksum file has no SHA-256 for {asset_name}"))
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn compare_versions(current: &str, tag: &str) -> Relation {
    let Some(current) = parse_semver(current) else {
        return Relation::UpdateAvailable;
    };
    let Some(release) = parse_semver(tag) else {
        return Relation::UpdateAvailable;
    };
    match current.cmp(&release) {
        std::cmp::Ordering::Less => Relation::UpdateAvailable,
        std::cmp::Ordering::Equal => Relation::UpToDate,
        std::cmp::Ordering::Greater => Relation::Ahead,
    }
}

pub fn normalize_tag(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(format!(
            "--version needs a tag such as perfr-v0.1.0, not {tag:?}"
        ));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_')
    {
        return Err(format!(
            "--version needs a tag such as perfr-v0.1.0, not {tag:?}"
        ));
    }
    if let Some(rest) = trimmed.strip_prefix("perfr-v") {
        let _ = parse_semver(rest).ok_or_else(|| format!("not a version tag: {tag}"))?;
        return Ok(trimmed.to_string());
    }
    let bare = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let _ = parse_semver(bare).ok_or_else(|| format!("not a version tag: {tag}"))?;
    Ok(format!("perfr-v{bare}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Semver {
    major: u64,
    minor: u64,
    patch: u64,
    /// Empty means a release. A prerelease sorts before the release.
    pre: Vec<String>,
}

fn parse_semver(text: &str) -> Option<Semver> {
    let text = text.trim().strip_prefix("perfr-v").unwrap_or(text.trim());
    let text = text.strip_prefix('v').unwrap_or(text);
    let (numbers, pre) = text.split_once('-').unwrap_or((text, ""));
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let pre = if pre.is_empty() {
        Vec::new()
    } else {
        pre.split('.').map(str::to_string).collect()
    };
    Some(Semver {
        major,
        minor,
        patch,
        pre,
    })
}

impl PartialOrd for Semver {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Semver {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => cmp_pre(&self.pre, &other.pre),
            })
    }
}

fn cmp_pre(left: &[String], right: &[String]) -> std::cmp::Ordering {
    for (a, b) in left.iter().zip(right.iter()) {
        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(b)) => a.cmp(&b),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.cmp(b),
        };
        if order != std::cmp::Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

pub fn install_tree(
    dest_dir: &Path,
    files: &[PackedFile],
    style: ReplaceStyle,
) -> Result<(), String> {
    install_files(dest_dir, files, style, true)
}

pub fn install_files(
    dest_dir: &Path,
    files: &[PackedFile],
    style: ReplaceStyle,
    require_service: bool,
) -> Result<(), String> {
    let mut staged = Vec::new();
    for file in files {
        let Some(relative) = archive::install_relative(&file.path) else {
            continue;
        };
        let dest = dest_dir.join(&relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let tmp = dest.with_extension(format!("new-{}", std::process::id()));
        {
            let mut out = fs::File::create(&tmp)
                .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
            out.write_all(&file.bytes)
                .map_err(|error| error.to_string())?;
        }
        set_mode(&tmp, file.executable || is_binary_name(&relative));
        staged.push((tmp, dest));
    }
    let has_service = staged.iter().any(|(_, dest)| {
        matches!(
            dest.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            "perfr" | "perfr.exe"
        )
    });
    if staged.is_empty() || (require_service && !has_service) {
        for (tmp, _) in &staged {
            let _ = fs::remove_file(tmp);
        }
        return Err(if require_service {
            "archive has no perfr binary".into()
        } else {
            "archive has no files to install".into()
        });
    }
    // Companions first, the running binary last, so a failure leaves the
    // previous binary in place.
    staged.sort_by_key(|(_, dest)| {
        is_binary_name(&dest.file_name().unwrap_or_default().to_string_lossy())
    });
    for (tmp, dest) in staged {
        if let Err(error) = replace_file(&dest, &tmp, style) {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
    }
    Ok(())
}

fn is_binary_name(name: &str) -> bool {
    matches!(
        name,
        "perfr"
            | "perfr.exe"
            | "perfr-inject-helper"
            | "perfr-inject-helper.exe"
            | "perfr-gpu-helper"
    )
}

pub fn replace_file(dest: &Path, source: &Path, style: ReplaceStyle) -> Result<(), String> {
    match style {
        ReplaceStyle::Atomic => fs::rename(source, dest)
            .map_err(|error| format!("cannot replace {}: {error}", dest.display())),
        ReplaceStyle::WindowsAside => {
            let aside = aside_path(dest);
            let _ = fs::remove_file(&aside);
            if dest.exists() && fs::rename(dest, &aside).is_err() {
                let _ = fs::remove_file(source);
                return Err(format!("cannot move {} aside", dest.display()));
            }
            if let Err(error) = fs::rename(source, dest) {
                let _ = fs::rename(&aside, dest);
                return Err(format!("cannot replace {}: {error}", dest.display()));
            }
            let _ = fs::remove_file(&aside);
            Ok(())
        }
    }
}

fn aside_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    dest.with_file_name(name)
}

fn set_mode(path: &Path, executable: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
}

/// cargo, Homebrew, or pip, when the path and the package metadata agree.
pub fn detect_manager(exe: &Path, crates_index: Option<&str>) -> Option<Manager> {
    let text = exe.to_string_lossy().replace('\\', "/");
    if text.contains("/Cellar/") || text.contains("/homebrew/") || text.contains("/linuxbrew/") {
        return Some(Manager {
            name: "homebrew",
            command: "brew upgrade perfr".into(),
        });
    }
    if text.contains("/pipx/") {
        return Some(Manager {
            name: "pipx",
            command: "pipx upgrade perfr".into(),
        });
    }
    if text.contains("/site-packages/") || ancestor_has(exe, "pyvenv.cfg") {
        return Some(Manager {
            name: "pip",
            command: "python -m pip install --upgrade perfr".into(),
        });
    }
    if text.contains("/.cargo/bin/") {
        if let Some(command) = cargo_command(crates_index) {
            return Some(Manager {
                name: "cargo",
                command,
            });
        }
    }
    None
}

fn ancestor_has(path: &Path, name: &str) -> bool {
    path.ancestors().any(|dir| dir.join(name).is_file())
}

fn cargo_command(index: Option<&str>) -> Option<String> {
    let index = index?;
    let value: Value = serde_json::from_str(index).ok()?;
    let installs = value.get("installs")?.as_object()?;
    for (key, entry) in installs {
        let bins = entry.get("bins").and_then(|v| v.as_array());
        let owns_perfr = bins
            .is_some_and(|bins| bins.iter().any(|bin| bin.as_str() == Some("perfr")))
            || key.starts_with("perfr ");
        if !owns_perfr {
            continue;
        }
        if let Some(git) = key.split("git+").nth(1) {
            let url = git.split(['#', ')']).next().unwrap_or(git);
            return Some(format!("cargo install --git {url} --force perfr"));
        }
        if key.contains("path+") {
            return Some(
                "installed from a local path; rebuild with cargo install --path <dir> --force"
                    .into(),
            );
        }
        return Some("cargo install --force perfr".into());
    }
    None
}

fn real_manager_hints(exe: &Path) -> Option<Manager> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|dir| dir.join(".cargo")));
    let index = cargo_home.and_then(|dir| {
        fs::read_to_string(dir.join(".crates2.json"))
            .ok()
            .or_else(|| fs::read_to_string(dir.join(".crates.toml")).ok())
    });
    detect_manager(exe, index.as_deref())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    if let Some(curl) = which("curl") {
        return run_download(
            &curl,
            &[
                "-fsSL",
                "-A",
                "perfr-update",
                "-H",
                "Accept: application/vnd.github+json",
                url,
            ],
        );
    }
    if let Some(wget) = which("wget") {
        return run_download(&wget, &["-qO-", "--user-agent=perfr-update", url]);
    }
    Err("need curl or wget to download".into())
}

fn run_download(program: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{}: {error}", program.display()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            Err(format!(
                "download failed: {program:?} status {}",
                output.status
            ))
        } else {
            Err(format!("download failed: {detail}"))
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).saturating_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(add);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapFetch {
        files: HashMap<String, Vec<u8>>,
    }

    impl Fetch for MapFetch {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.files
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no fixture for {url}"))
        }
    }

    fn release_json(tag: &str, names: &[&str]) -> Vec<u8> {
        let assets: Vec<Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "browser_download_url": format!("https://example.test/{name}"),
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({"tag_name": tag, "assets": assets})).unwrap()
    }

    #[test]
    fn sha256_matches_the_empty_and_abc_vectors() {
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn versions_ignore_the_perfr_tag_prefix_and_sort_prereleases_first() {
        assert_eq!(
            compare_versions("0.1.0", "perfr-v0.1.0"),
            Relation::UpToDate
        );
        assert_eq!(
            compare_versions("0.1.0", "perfr-v0.2.0"),
            Relation::UpdateAvailable
        );
        assert_eq!(compare_versions("0.2.0", "perfr-v0.1.0"), Relation::Ahead);
        assert_eq!(
            compare_versions("0.1.0-rc.1", "perfr-v0.1.0"),
            Relation::UpdateAvailable
        );
        assert_eq!(
            compare_versions("0.1.0-rc.2", "perfr-v0.1.0-rc.1"),
            Relation::Ahead
        );
        assert_eq!(normalize_tag("0.2.0").unwrap(), "perfr-v0.2.0");
        assert_eq!(normalize_tag("perfr-v0.2.0").unwrap(), "perfr-v0.2.0");
        assert!(normalize_tag("../etc").is_err());
    }

    #[test]
    fn asset_names_follow_the_installers() {
        assert_eq!(
            asset_for(Platform {
                os: "linux",
                arch: "x86_64"
            })
            .unwrap(),
            "perfr-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_for(Platform {
                os: "macos",
                arch: "aarch64"
            })
            .unwrap(),
            "perfr-macos-universal.tar.gz"
        );
        assert_eq!(
            asset_for(Platform {
                os: "windows",
                arch: "x86_64"
            })
            .unwrap(),
            "perfr-windows-x86_64.zip"
        );
        assert!(asset_for(Platform {
            os: "linux",
            arch: "aarch64"
        })
        .unwrap_err()
        .contains("arm64"));
    }

    #[test]
    fn checksum_accepts_sha256sum_lines_and_a_bare_hash() {
        let bytes = b"perfr";
        let hash = hex_encode(&sha256(bytes));
        verify_checksum(
            bytes,
            &format!("{hash}  perfr-linux-x86_64.tar.gz\n"),
            "perfr-linux-x86_64.tar.gz",
        )
        .unwrap();
        verify_checksum(bytes, &hash, "perfr-linux-x86_64.tar.gz").unwrap();
        assert!(verify_checksum(b"nope", &hash, "perfr-linux-x86_64.tar.gz").is_err());
    }

    #[test]
    fn check_reports_an_update_without_downloading_the_archive() {
        let json = release_json(
            "perfr-v9.9.9",
            &[
                "perfr-linux-x86_64.tar.gz",
                "perfr-linux-x86_64.tar.gz.sha256",
                "SHA256SUMS",
            ],
        );
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let text = run_with(
            &UpdateOpts {
                check: true,
                tag: None,
                yes: false,
            },
            &fetch,
            Path::new("/usr/local/bin/perfr"),
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::Atomic,
            &None,
        )
        .unwrap();
        assert!(text.contains("latest release: perfr-v9.9.9"), "{text}");
        assert!(text.contains("asset: perfr-linux-x86_64.tar.gz"), "{text}");
        assert!(text.contains("checksum: published"), "{text}");
        assert!(text.contains("status: update available"), "{text}");
        assert!(text.contains("--yes"), "{text}");
    }

    #[test]
    fn a_package_manager_is_named_instead_of_replacing_the_binary() {
        let cargo = detect_manager(
            Path::new("/home/me/.cargo/bin/perfr"),
            Some(r#"{"installs":{"perfr 0.1.0 (git+https://github.com/perfr/perfr#abc)":{"bins":["perfr"]}}}"#),
        )
        .unwrap();
        assert_eq!(cargo.name, "cargo");
        assert!(cargo
            .command
            .contains("cargo install --git https://github.com/perfr/perfr"));
        let brew = detect_manager(
            Path::new("/opt/homebrew/Cellar/perfr/0.1.0/bin/perfr"),
            None,
        )
        .unwrap();
        assert_eq!(brew.command, "brew upgrade perfr");
        let pip = detect_manager(Path::new("/tmp/venv/lib/site-packages/perfr"), None).unwrap();
        assert_eq!(pip.name, "pip");
        assert!(detect_manager(
            Path::new("/home/me/.cargo/bin/perfr"),
            Some(r#"{"installs":{}}"#)
        )
        .is_none());

        let json = release_json("perfr-v9.9.9", &["perfr-linux-x86_64.tar.gz"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let err = run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: true,
            },
            &fetch,
            Path::new("/opt/homebrew/Cellar/perfr/0.1.0/bin/perfr"),
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::Atomic,
            &Some(brew),
        )
        .unwrap_err();
        assert!(err.contains("brew upgrade perfr"), "{err}");
    }

    #[test]
    fn yes_verifies_the_checksum_and_replaces_the_tree() {
        let archive = archive::pack_tar_gz(&[
            ("perfr/perfr", b"new-binary", 0o755),
            ("perfr/libperfr_api.so", b"api", 0o644),
            ("perfr/perfr-inject-helper", b"helper", 0o755),
            ("perfr/libperfr_agent.so", b"agent", 0o644),
        ]);
        let hash = hex_encode(&sha256(&archive));
        let sums = format!("{hash}  perfr-linux-x86_64.tar.gz\n");
        let json = release_json(
            "perfr-v9.9.9",
            &[
                "perfr-linux-x86_64.tar.gz",
                "perfr-linux-x86_64.tar.gz.sha256",
            ],
        );
        let fetch = MapFetch {
            files: HashMap::from([
                (LATEST_URL.to_string(), json),
                (
                    "https://example.test/perfr-linux-x86_64.tar.gz".into(),
                    archive,
                ),
                (
                    "https://example.test/perfr-linux-x86_64.tar.gz.sha256".into(),
                    sums.into_bytes(),
                ),
            ]),
        };
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfr");
        fs::write(&exe, b"old").unwrap();
        let text = run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: true,
            },
            &fetch,
            &exe,
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::WindowsAside,
            &None,
        )
        .unwrap();
        assert!(text.contains("replaced install"), "{text}");
        assert!(!text.contains("function hooks:"), "{text}");
        assert_eq!(fs::read(dir.path().join("perfr")).unwrap(), b"new-binary");
        assert_eq!(
            fs::read(dir.path().join("libperfr_api.so")).unwrap(),
            b"api"
        );
        assert_eq!(
            fs::read(dir.path().join("perfr-inject-helper")).unwrap(),
            b"helper"
        );
        assert_eq!(
            fs::read(dir.path().join("libperfr_agent.so")).unwrap(),
            b"agent"
        );
        assert!(!dir.path().join("perfr.exe.old").exists());
        assert!(!dir.path().join("perfr.old").exists());
    }

    #[test]
    fn a_checksum_mismatch_leaves_the_binary_alone() {
        let archive = archive::pack_tar_gz(&[("perfr/perfr", b"new", 0o755)]);
        let json = release_json(
            "perfr-v9.9.9",
            &[
                "perfr-linux-x86_64.tar.gz",
                "perfr-linux-x86_64.tar.gz.sha256",
            ],
        );
        let fetch = MapFetch {
            files: HashMap::from([
                (LATEST_URL.to_string(), json),
                ("https://example.test/perfr-linux-x86_64.tar.gz".into(), archive),
                (
                    "https://example.test/perfr-linux-x86_64.tar.gz.sha256".into(),
                    b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  perfr-linux-x86_64.tar.gz\n".to_vec(),
                ),
            ]),
        };
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfr");
        fs::write(&exe, b"old").unwrap();
        let err = run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: true,
            },
            &fetch,
            &exe,
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::Atomic,
            &None,
        )
        .unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert_eq!(fs::read(&exe).unwrap(), b"old");
    }
}
