// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr update`: replace this install with a GitHub release of perfr/perfr.
//!
//! With no flags the command downloads the release, verifies the checksum,
//! and replaces the install. `--check` only prints `current`, `latest`, and
//! `status`. It exits [`EXIT_OK`] when this install is current and
//! [`EXIT_UPDATE_AVAILABLE`] when a newer release exists. `--yes` is accepted
//! and ignored.
//!
//! The asset names and the on-disk layout match `tools/install/install.sh`
//! and `tools/install/install.ps1`: a `perfr/` directory of the binary plus
//! the Frida files beside it, checksummed by the `.sha256` asset (sha256sum
//! format) or `SHA256SUMS`. `perfr.h` is installed to the sibling `include`
//! of that directory on Linux and macOS (`~/.local/include`) and to
//! `<install>\include` on Windows. Retired files from the split-library
//! layout are deleted from the install directory. A cargo, Homebrew, or pip
//! install is not overwritten; the command that owns it is printed instead.
//!
//! The files are replaced together or not at all: if one cannot be moved in,
//! the ones already replaced are put back. File capabilities granted by
//! `perfr setup` belong to the old binary's inode and do not carry over;
//! the command says so and names the setup run that grants them again.
//!
//! Exit codes: [`EXIT_OK`] (0) the install is current, this build is newer
//! than the release, or an update was applied; [`EXIT_UPDATE_AVAILABLE`] (1)
//! `--check` found a newer release; [`EXIT_ERROR`] (2) the download, the
//! checksum, or the install failed, including a package-manager install that
//! this command will not overwrite.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::archive::{self, PackedFile};
use crate::build_info;

const LATEST_URL: &str = "https://api.github.com/repos/perfr/perfr/releases/latest";
const TAG_URL: &str = "https://api.github.com/repos/perfr/perfr/releases/tags/";

/// The install is current, this build is newer than the release, or `perfr update`
/// replaced the install.
pub const EXIT_OK: i32 = 0;
/// `perfr update --check` found a newer release. The archive was not downloaded.
pub const EXIT_UPDATE_AVAILABLE: i32 = 1;
/// Download, checksum, or install failure. A package-manager install that this
/// command will not overwrite also exits with this code.
pub const EXIT_ERROR: i32 = 2;

#[derive(Clone, Debug)]
pub struct UpdateOpts {
    pub check: bool,
    pub tag: Option<String>,
    /// Accepted for compatibility. Ignored: `perfr update` already replaces the install.
    pub yes: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
    pub text: String,
    pub code: i32,
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
        Ok(report) => {
            print!("{}", report.text);
            if !report.text.ends_with('\n') {
                println!();
            }
            report.code
        }
        Err(error) => {
            eprintln!("perfr update: {error}");
            EXIT_ERROR
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
) -> Result<UpdateReport, String> {
    // Older scripts passed `--yes` to apply. Applying is the default now.
    let _ = opts.yes;
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
    let current = display_version(build_info::package_version());
    let latest = display_version(&release.tag);
    let relation = compare_versions(build_info::package_version(), &release.tag);
    let asset_name = asset_for(platform)?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .cloned()
        .ok_or_else(|| format!("release {} has no {asset_name}", release.tag))?;
    let checksum_asset = checksum_asset(&release, &asset_name);
    if opts.check {
        return Ok(check_report(&current, &latest, relation, manager.as_ref()));
    }
    if let Some(manager) = manager {
        return Err(format!(
            "this install is managed by {}; not replacing the binary\nupdate with: {}",
            manager.name, manager.command
        ));
    }
    let explicit = opts.tag.is_some();
    if !explicit {
        match relation {
            Relation::UpToDate => {
                return Ok(UpdateReport {
                    text: format!("perfr {current} is up to date\n"),
                    code: EXIT_OK,
                });
            }
            Relation::Ahead => {
                return Ok(UpdateReport {
                    text: format!("perfr {current} is newer than the latest release {latest}\n"),
                    code: EXIT_OK,
                });
            }
            Relation::UpdateAvailable => {}
        }
    }
    let Some(checksum_asset) = checksum_asset else {
        return Err(format!(
            "no checksum published for {}; refusing to replace the binary",
            asset.name
        ));
    };
    let archive = fetch.get(&asset.url)?;
    let sums = fetch.get(&checksum_asset.url)?;
    verify_checksum(&archive, &String::from_utf8_lossy(&sums), &asset.name)?;
    let files = archive::unpack_named(&asset.name, &archive)?;
    let dest = exe.parent().ok_or("the running binary has no directory")?;
    // Read before the rename: afterwards the path names the new inode.
    let capabilities = file_capabilities(exe);
    install_tree(dest, &files, style, platform)?;
    let removed = remove_retired(dest)?;
    let mut text = format!(
        "updated perfr {current} -> {latest} in {}\n",
        dest.display()
    );
    text.push_str("restart any running perfr to use the new version\n");
    if let Some(note) = capabilities_note(capabilities) {
        text.push_str(&note);
    }
    for path in &removed {
        text.push_str(&format!("removed {}\n", path.display()));
    }
    Ok(UpdateReport {
        text,
        code: EXIT_OK,
    })
}

fn check_report(
    current: &str,
    latest: &str,
    relation: Relation,
    manager: Option<&Manager>,
) -> UpdateReport {
    let status = match relation {
        Relation::UpdateAvailable => "update available",
        Relation::UpToDate => "up to date",
        Relation::Ahead => "newer than latest",
    };
    let mut text = format!("current: {current}\nlatest: {latest}\nstatus: {status}\n");
    if let Some(manager) = manager {
        text.push_str(&format!("installed by: {}\n", manager.name));
        text.push_str(&format!("update with: {}\n", manager.command));
    }
    UpdateReport {
        text,
        code: if relation == Relation::UpdateAvailable {
            EXIT_UPDATE_AVAILABLE
        } else {
            EXIT_OK
        },
    }
}

/// `0.1.2` from `0.1.2`, `v0.1.2`, or `perfr-v0.1.2`.
fn display_version(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("perfr-v").unwrap_or(text);
    text.strip_prefix('v').unwrap_or(text).to_string()
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

/// Files a previous install may have left in the install directory.
/// `perfr update`, `tools/install/install.sh`, and `tools/install/install.ps1`
/// delete these when present. Keep the three lists the same.
/// `perfr_api.lib` and `libperfr_api.a` are the static libraries and stay.
const RETIRED_INSTALL_FILES: &[&str] = &[
    "perfr-frida-helper",
    "libperfr_frida_agent.so",
    "libperfr_frida_agent.dylib",
    "libperfr_api.so",
    "libperfr_api.dylib",
    "perfr-gpu-helper",
    "perfr_frida_agent.dll",
    "perfr_api.dll",
    "perfr-frida-helper.exe",
    "perfr-gpu-helper.exe",
    "perfr-frida-helper.pdb",
    "perfr-frida-helper.lib",
    "perfr-gpu-helper.pdb",
    "perfr-gpu-helper.lib",
    "perfr_frida_agent.pdb",
    "perfr_frida_agent.lib",
    "perfr_frida_agent.dll.lib",
    "perfr_api.pdb",
    "perfr_api.dll.lib",
    // Left in the bin dir by older installers. The header now lives in include/.
    "perfr.h",
];

/// Delete retired files that sit directly in `dir`. Prints nothing itself;
/// the caller reports `removed` paths. A name that is not there is skipped.
pub fn remove_retired(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    for name in RETIRED_INSTALL_FILES {
        let path = dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&path)
                    .map_err(|error| format!("cannot remove {}: {error}", path.display()))?;
                removed.push(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot stat {}: {error}", path.display()));
            }
        }
    }
    Ok(removed)
}

/// Where `perfr.h` is written. Linux and macOS use the sibling `include` of
/// the directory that holds the binary. Windows uses `<install>\include`.
pub fn header_install_path(dest_dir: &Path, platform: Platform) -> PathBuf {
    let include = if platform.os == "windows" {
        dest_dir.join("include")
    } else {
        match dest_dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join("include"),
            _ => dest_dir.join("include"),
        }
    };
    include.join("perfr.h")
}

fn installed_path(dest_dir: &Path, relative: &str, platform: Platform) -> PathBuf {
    if relative == "perfr.h" || relative.ends_with("/perfr.h") {
        header_install_path(dest_dir, platform)
    } else {
        dest_dir.join(relative)
    }
}

pub fn install_tree(
    dest_dir: &Path,
    files: &[PackedFile],
    style: ReplaceStyle,
    platform: Platform,
) -> Result<(), String> {
    // `include/perfr.h` is the archive layout. A root `perfr.h` is the older
    // layout; ignore it when both are present so the include copy wins.
    let nested_header = files.iter().any(|file| {
        archive::install_relative(&file.path).is_some_and(|relative| relative.ends_with("/perfr.h"))
    });
    let mut staged = Staged(Vec::new());
    for file in files {
        let Some(relative) = archive::install_relative(&file.path) else {
            continue;
        };
        if relative == "perfr.h" && nested_header {
            continue;
        }
        let dest = installed_path(dest_dir, &relative, platform);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let tmp = dest.with_file_name(format!(
            ".{}.new-{}",
            dest.file_name().unwrap_or_default().to_string_lossy(),
            staged.0.len()
        ));
        {
            let mut out = fs::File::create(&tmp)
                .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
            out.write_all(&file.bytes)
                .map_err(|error| error.to_string())?;
        }
        set_mode(&tmp, file.executable || is_binary_name(&relative));
        staged.0.push((tmp, dest));
    }
    if !staged
        .0
        .iter()
        .any(|(_, dest)| is_binary_name(&dest.file_name().unwrap_or_default().to_string_lossy()))
    {
        return Err("archive has no perfr binary".into());
    }
    // Companions first, the running binary last, so a failure leaves the
    // previous binary in place.
    staged.0.sort_by_key(|(_, dest)| {
        is_binary_name(&dest.file_name().unwrap_or_default().to_string_lossy())
    });
    // All of it or none of it: the binary, the library and the hook helper
    // agree on the scope ring's layout, so a new library beside the old
    // binary is a broken install, not half an update. What each file
    // replaced is kept until the last one is in, and put back if one fails.
    let mut replaced: Vec<(&Path, Old)> = Vec::new();
    for (tmp, dest) in &staged.0 {
        match replace_keeping_old(dest, tmp, style) {
            Ok(old) => replaced.push((dest, old)),
            Err(error) => {
                for (dest, old) in replaced.into_iter().rev() {
                    let _ = match old {
                        Old::Kept(old) => fs::rename(&old, dest),
                        Old::Absent => fs::remove_file(dest),
                        Old::Gone => Ok(()),
                    };
                }
                return Err(error);
            }
        }
    }
    for (_, old) in replaced {
        if let Old::Kept(old) = old {
            let _ = fs::remove_file(old);
        }
    }
    Ok(())
}

/// What became of the file a new one replaced.
#[derive(Debug, PartialEq, Eq)]
enum Old {
    /// There was none: undoing the install deletes the new file.
    Absent,
    /// It is here, to delete once the install is whole or to rename back.
    Kept(PathBuf),
    /// It was replaced and no copy could be kept. The new file stays.
    Gone,
}

/// The files written beside their destinations and not yet moved in.
/// Dropping it deletes whatever is still there, so no way out of
/// `install_tree` leaves `.NAME.new-N` files in the install directory.
struct Staged(Vec<(PathBuf, PathBuf)>);

impl Drop for Staged {
    fn drop(&mut self) {
        for (tmp, _) in &self.0 {
            let _ = fs::remove_file(tmp);
        }
    }
}

fn is_binary_name(name: &str) -> bool {
    matches!(
        name,
        "perfr" | "perfr.exe" | "perfr-hook" | "perfr-hook.exe" | "perfr-gpu"
    )
}

/// Moves `source` into `dest`, keeping the file that was there under another
/// name for the caller to delete or to rename back.
fn replace_keeping_old(dest: &Path, source: &Path, style: ReplaceStyle) -> Result<Old, String> {
    let cannot = |error: std::io::Error| format!("cannot replace {}: {error}", dest.display());
    if fs::symlink_metadata(dest).is_err() {
        return fs::rename(source, dest).map(|()| Old::Absent).map_err(cannot);
    }
    match style {
        ReplaceStyle::Atomic => {
            // A second name for the old file, so the rename below stays the
            // one step in which `dest` changes. A filesystem without hard
            // links keeps no copy, and the replace goes ahead as it used to.
            let aside = free_aside_path(dest, true);
            let old = if fs::hard_link(dest, &aside).is_ok() { Old::Kept(aside) } else { Old::Gone };
            match fs::rename(source, dest) {
                Ok(()) => Ok(old),
                Err(error) => {
                    if let Old::Kept(aside) = old {
                        let _ = fs::remove_file(aside);
                    }
                    Err(cannot(error))
                }
            }
        }
        ReplaceStyle::WindowsAside => {
            let aside = free_aside_path(dest, false);
            if fs::rename(dest, &aside).is_err() {
                return Err(format!("cannot move {} aside", dest.display()));
            }
            if let Err(error) = fs::rename(source, dest) {
                let _ = fs::rename(&aside, dest);
                return Err(cannot(error));
            }
            Ok(Old::Kept(aside))
        }
    }
}

/// A name beside `dest` that nothing occupies: `NAME.old`, or `NAME.old-N`
/// when that one cannot be deleted. On Windows it cannot while a service
/// started before the previous update still runs from it, and moving the
/// current binary onto a running image fails; that used to stop the update
/// after the companions had been replaced. Copies left by earlier updates
/// are deleted on the way, when they let go.
fn free_aside_path(dest: &Path, hidden: bool) -> PathBuf {
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let dot = if hidden { "." } else { "" };
    let mut free = None;
    for n in 0..16 {
        let suffix = if n == 0 { String::new() } else { format!("-{n}") };
        let aside = dest.with_file_name(format!("{dot}{name}.old{suffix}"));
        let _ = fs::remove_file(&aside);
        if free.is_none() && fs::symlink_metadata(&aside).is_err() {
            free = Some(aside);
        }
    }
    free.unwrap_or_else(|| dest.with_file_name(format!("{dot}{name}.old-{}", std::process::id())))
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

/// The permitted set of a `security.capability` value (`struct vfs_cap_data`:
/// a magic word, then permitted and inheritable, low words then high; the
/// first revision has the low pair only). `None` for a value cut short or
/// one that grants nothing.
#[cfg(any(target_os = "linux", test))]
fn parse_file_capabilities(value: &[u8]) -> Option<u64> {
    let word = |at: usize| value.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    word(8)?;
    let permitted = u64::from(word(4)?) | (u64::from(word(12).unwrap_or(0)) << 32);
    (permitted != 0).then_some(permitted)
}

/// The file capabilities on `path`, as `perfr setup` leaves them on the
/// binary. They belong to the inode, so a binary renamed into its place
/// starts without them.
fn file_capabilities(path: &Path) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut value = [0u8; 64];
        // SAFETY: both strings are NUL-terminated and the kernel writes at
        // most `value.len()` bytes into `value`.
        let len = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"security.capability".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        parse_file_capabilities(value.get(..usize::try_from(len).ok()?)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// What to tell the user when the binary that was replaced held file
/// capabilities. Putting them back takes root, so the update says which
/// command does it rather than leave the next capture to come up without
/// samples, scheduler or uprobes and no word why.
fn capabilities_note(permitted: Option<u64>) -> Option<String> {
    const CAP_SYS_ADMIN: u64 = 1 << 21;
    let command = if permitted? & CAP_SYS_ADMIN != 0 {
        "perfr setup --uprobes"
    } else {
        "perfr setup"
    };
    Some(format!(
        "the replaced binary held file capabilities and the new one does not; run `{command}` to grant them again\n"
    ))
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

fn hex_encode(bytes: &[u8]) -> String {
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
        let report = run_with(
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
        let text = report.text;
        assert_eq!(report.code, EXIT_UPDATE_AVAILABLE);
        assert!(
            text.contains(&format!("current: {}", build_info::package_version())),
            "{text}"
        );
        assert!(text.contains("latest: 9.9.9\n"), "{text}");
        assert!(text.contains("status: update available\n"), "{text}");
        assert!(!text.contains("--yes"), "{text}");
        assert!(!text.contains("updated perfr"), "{text}");
    }

    #[test]
    fn check_exits_zero_when_current_or_newer() {
        let current = format!("perfr-v{}", build_info::package_version());
        let json = release_json(&current, &["perfr-linux-x86_64.tar.gz"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let report = run_with(
            &UpdateOpts {
                check: true,
                tag: None,
                yes: true,
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
        assert_eq!(report.code, EXIT_OK);
        assert_eq!(
            report.text,
            format!(
                "current: {v}\nlatest: {v}\nstatus: up to date\n",
                v = build_info::package_version()
            )
        );

        let older = release_json("perfr-v0.0.1", &["perfr-macos-universal.tar.gz"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), older)]),
        };
        let report = run_with(
            &UpdateOpts {
                check: true,
                tag: None,
                yes: false,
            },
            &fetch,
            Path::new("/usr/local/bin/perfr"),
            Platform {
                os: "macos",
                arch: "aarch64",
            },
            ReplaceStyle::Atomic,
            &None,
        )
        .unwrap();
        assert_eq!(report.code, EXIT_OK);
        assert!(
            report.text.contains("status: newer than latest\n"),
            "{report:?}"
        );
        assert!(report.text.contains("latest: 0.0.1\n"), "{report:?}");
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
        for yes in [false, true] {
            let err = run_with(
                &UpdateOpts {
                    check: false,
                    tag: None,
                    yes,
                },
                &fetch,
                Path::new("/opt/homebrew/Cellar/perfr/0.1.0/bin/perfr"),
                Platform {
                    os: "linux",
                    arch: "x86_64",
                },
                ReplaceStyle::Atomic,
                &Some(brew.clone()),
            )
            .unwrap_err();
            assert!(err.contains("not replacing the binary"), "{err}");
            assert!(err.contains("brew upgrade perfr"), "{err}");
        }

        let json = release_json("perfr-v9.9.9", &["perfr-windows-x86_64.zip"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let report = run_with(
            &UpdateOpts {
                check: true,
                tag: None,
                yes: true,
            },
            &fetch,
            Path::new("/opt/homebrew/Cellar/perfr/0.1.0/bin/perfr"),
            Platform {
                os: "windows",
                arch: "aarch64",
            },
            ReplaceStyle::WindowsAside,
            &Some(brew),
        )
        .unwrap();
        assert_eq!(report.code, EXIT_UPDATE_AVAILABLE);
        assert!(
            report.text.contains("status: update available\n"),
            "{report:?}"
        );
        assert!(
            report.text.contains("installed by: homebrew\n"),
            "{report:?}"
        );
        assert!(
            report.text.contains("update with: brew upgrade perfr\n"),
            "{report:?}"
        );
        assert!(!report.text.contains("updated perfr"), "{report:?}");
    }

    #[test]
    fn a_current_or_newer_install_is_not_replaced() {
        let version = build_info::package_version();
        let json = release_json(&format!("perfr-v{version}"), &["perfr-linux-x86_64.tar.gz"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfr");
        fs::write(&exe, b"old").unwrap();
        let report = run_with(
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
        .unwrap();
        assert_eq!(report.code, EXIT_OK);
        assert_eq!(report.text, format!("perfr {version} is up to date\n"));
        assert_eq!(fs::read(&exe).unwrap(), b"old");

        let json = release_json("perfr-v0.0.1", &["perfr-windows-x86_64.zip"]);
        let fetch = MapFetch {
            files: HashMap::from([(LATEST_URL.to_string(), json)]),
        };
        let exe = dir.path().join("perfr.exe");
        fs::write(&exe, b"old-exe").unwrap();
        let report = run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: false,
            },
            &fetch,
            &exe,
            Platform {
                os: "windows",
                arch: "x86_64",
            },
            ReplaceStyle::WindowsAside,
            &None,
        )
        .unwrap();
        assert_eq!(report.code, EXIT_OK);
        assert_eq!(
            report.text,
            format!("perfr {version} is newer than the latest release 0.0.1\n")
        );
        assert_eq!(fs::read(&exe).unwrap(), b"old-exe");
        assert!(!dir.path().join("perfr.exe.old").exists());
    }

    #[test]
    fn an_explicit_version_replaces_the_install_on_windows() {
        let archive = archive::pack_zip_stored(&[
            ("perfr/perfr.exe", b"older-exe"),
            ("perfr/include/perfr.h", b"hdr"),
        ]);
        let hash = hex_encode(&sha256(&archive));
        let asset = "perfr-windows-x86_64.zip";
        let sums = format!("{hash}  {asset}\n");
        let tag = "perfr-v0.0.1";
        let json = release_json(tag, &[asset, &format!("{asset}.sha256")]);
        let fetch = MapFetch {
            files: HashMap::from([
                (format!("{TAG_URL}{tag}"), json),
                (format!("https://example.test/{asset}"), archive),
                (
                    format!("https://example.test/{asset}.sha256"),
                    sums.into_bytes(),
                ),
            ]),
        };
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("perfr");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("perfr.exe");
        fs::write(&exe, b"current").unwrap();
        fs::write(install.join("perfr.h"), b"stale").unwrap();

        let checked = run_with(
            &UpdateOpts {
                check: true,
                tag: Some("v0.0.1".into()),
                yes: false,
            },
            &fetch,
            &exe,
            Platform {
                os: "windows",
                arch: "aarch64",
            },
            ReplaceStyle::WindowsAside,
            &None,
        )
        .unwrap();
        assert_eq!(checked.code, EXIT_OK);
        assert_eq!(
            checked.text,
            format!(
                "current: {}\nlatest: 0.0.1\nstatus: newer than latest\n",
                build_info::package_version()
            )
        );
        assert_eq!(fs::read(&exe).unwrap(), b"current");

        let report = run_with(
            &UpdateOpts {
                check: false,
                tag: Some("0.0.1".into()),
                yes: true,
            },
            &fetch,
            &exe,
            Platform {
                os: "windows",
                arch: "x86_64",
            },
            ReplaceStyle::WindowsAside,
            &None,
        )
        .unwrap();
        assert_eq!(report.code, EXIT_OK);
        assert_eq!(
            report.text,
            format!(
                "updated perfr {} -> 0.0.1 in {}\nrestart any running perfr to use the new version\nremoved {}\n",
                build_info::package_version(),
                install.display(),
                install.join("perfr.h").display()
            )
        );
        assert_eq!(fs::read(&exe).unwrap(), b"older-exe");
        assert_eq!(
            fs::read(install.join("include").join("perfr.h")).unwrap(),
            b"hdr"
        );
        assert!(!install.join("perfr.h").exists());
        assert!(!install.join("perfr.exe.old").exists());
    }

    fn packed(path: &str, bytes: &[u8]) -> PackedFile {
        PackedFile { path: path.into(), bytes: bytes.to_vec(), executable: false }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn an_install_that_fails_part_way_puts_the_replaced_files_back() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bin");
        fs::create_dir_all(install.join("blocked")).unwrap();
        // A directory with something in it: no file can be renamed over it.
        fs::write(install.join("blocked").join("keep"), b"x").unwrap();
        fs::write(install.join("perfr"), b"old-binary").unwrap();
        fs::write(install.join("libperfr.so"), b"old-lib").unwrap();
        let files = [
            packed("perfr/libperfr.so", b"new-lib"),
            packed("perfr/fresh.txt", b"new-file"),
            packed("perfr/blocked", b"new-blocked"),
            packed("perfr/perfr", b"new-binary"),
        ];
        let platform = Platform { os: "linux", arch: "x86_64" };
        let error = install_tree(&install, &files, ReplaceStyle::Atomic, platform).unwrap_err();
        assert!(error.contains("blocked"), "{error}");
        // The library and the binary are a pair (the scope ring's layout
        // version): a new one beside an old one is worse than no update.
        assert_eq!(fs::read(install.join("libperfr.so")).unwrap(), b"old-lib");
        assert_eq!(fs::read(install.join("perfr")).unwrap(), b"old-binary");
        // Nothing staged, nothing kept aside, and the file that was new is gone.
        assert_eq!(names_in(&install), ["blocked", "libperfr.so", "perfr"]);
    }

    #[test]
    fn a_finished_install_keeps_no_copy_of_what_it_replaced() {
        for style in [ReplaceStyle::Atomic, ReplaceStyle::WindowsAside] {
            let dir = tempfile::tempdir().unwrap();
            let install = dir.path().join("bin");
            fs::create_dir_all(&install).unwrap();
            fs::write(install.join("perfr"), b"old-binary").unwrap();
            fs::write(install.join("libperfr.so"), b"old-lib").unwrap();
            let files = [packed("perfr/libperfr.so", b"new-lib"), packed("perfr/perfr", b"new-binary")];
            let platform = Platform { os: "linux", arch: "x86_64" };
            install_tree(&install, &files, style, platform).unwrap();
            assert_eq!(fs::read(install.join("libperfr.so")).unwrap(), b"new-lib", "{style:?}");
            assert_eq!(fs::read(install.join("perfr")).unwrap(), b"new-binary", "{style:?}");
            assert_eq!(names_in(&install), ["libperfr.so", "perfr"], "{style:?}");
        }
    }

    #[test]
    fn staging_that_fails_part_way_removes_what_it_staged() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("bin");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("perfr"), b"old-binary").unwrap();
        // `share` is a file, so the directory for `share/doc.txt` cannot be made.
        fs::write(install.join("share"), b"in the way").unwrap();
        let files = [
            packed("perfr/perfr", b"new-binary"),
            packed("perfr/libperfr.so", b"new-lib"),
            packed("perfr/share/doc.txt", b"doc"),
        ];
        let platform = Platform { os: "linux", arch: "x86_64" };
        install_tree(&install, &files, ReplaceStyle::Atomic, platform).unwrap_err();
        assert_eq!(names_in(&install), ["perfr", "share"]);
        assert_eq!(fs::read(install.join("perfr")).unwrap(), b"old-binary");
    }

    #[test]
    fn an_aside_copy_that_is_still_in_use_does_not_block_the_next_update() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfr.exe");
        fs::write(&exe, b"current").unwrap();
        // The service started before the last update still runs from
        // `perfr.exe.old`, and Windows will not delete or replace a running
        // image. A directory stands in for it: it cannot be unlinked either.
        fs::create_dir_all(dir.path().join("perfr.exe.old").join("held")).unwrap();
        let staged = dir.path().join(".perfr.exe.new-0");
        fs::write(&staged, b"next").unwrap();
        let old = replace_keeping_old(&exe, &staged, ReplaceStyle::WindowsAside).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"next");
        assert_eq!(old, Old::Kept(dir.path().join("perfr.exe.old-1")));
        assert_eq!(fs::read(dir.path().join("perfr.exe.old-1")).unwrap(), b"current");
    }

    #[test]
    fn file_capabilities_are_read_from_the_xattr_layout() {
        // `setcap cap_sys_admin,cap_perfmon,cap_dac_read_search+ep`, as
        // `getfattr -e hex -n security.capability` prints it: revision 2 with
        // the effective bit, then permitted and inheritable, low and high.
        let uprobes = [
            0x01, 0x00, 0x00, 0x02, 0x04, 0x00, 0x20, 0x00, 0, 0, 0, 0, 0x40, 0x00, 0x00, 0x00, 0, 0, 0, 0,
        ];
        let permitted = parse_file_capabilities(&uprobes).unwrap();
        assert_eq!(permitted, (1 << 2) | (1 << 21) | (1 << 38));
        assert_eq!(parse_file_capabilities(&uprobes[..8]), None, "cut short");
        assert_eq!(parse_file_capabilities(&[0u8; 20]), None, "an empty set is no grant");

        let note = capabilities_note(Some(permitted)).unwrap();
        assert!(note.contains("perfr setup --uprobes"), "{note}");
        // `cap_perfmon,cap_sys_ptrace`: what plain `perfr setup` grants.
        let note = capabilities_note(Some((1 << 38) | (1 << 19))).unwrap();
        assert!(note.contains("`perfr setup`"), "{note}");
        assert!(!note.contains("--uprobes"), "{note}");
        assert_eq!(capabilities_note(None), None);
    }

    #[test]
    fn a_binary_without_file_capabilities_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("perfr");
        fs::write(&exe, b"plain").unwrap();
        assert_eq!(file_capabilities(&exe), None);
        assert_eq!(file_capabilities(&dir.path().join("absent")), None);
    }

    #[test]
    fn update_verifies_the_checksum_and_replaces_the_tree() {
        let archive = archive::pack_tar_gz(&[
            ("perfr/perfr", b"new-binary", 0o755),
            ("perfr/perfr-hook", b"helper", 0o755),
            ("perfr/libperfr.so", b"agent", 0o644),
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
        let expected = format!(
            "updated perfr {} -> 9.9.9 in {}\nrestart any running perfr to use the new version\n",
            build_info::package_version(),
            dir.path().display()
        );
        for yes in [false, true] {
            fs::write(&exe, b"old").unwrap();
            let report = run_with(
                &UpdateOpts {
                    check: false,
                    tag: None,
                    yes,
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
            assert_eq!(report.code, EXIT_OK);
            assert_eq!(report.text, expected);
            assert_eq!(fs::read(dir.path().join("perfr")).unwrap(), b"new-binary");
            assert_eq!(fs::read(dir.path().join("perfr-hook")).unwrap(), b"helper");
            assert!(!dir.path().join("perfr.exe.old").exists());
            assert!(!dir.path().join("perfr.old").exists());
        }
    }

    #[test]
    fn update_deletes_retired_files_and_prints_them() {
        let archive = archive::pack_tar_gz(&[
            ("perfr/perfr", b"new-binary", 0o755),
            ("perfr/perfr-hook", b"helper", 0o755),
            ("perfr/libperfr.so", b"agent", 0o644),
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
        for name in RETIRED_INSTALL_FILES {
            fs::write(dir.path().join(name), b"stale").unwrap();
        }
        // The static libraries still ship. A cleanup must not take them.
        fs::write(dir.path().join("perfr_api.lib"), b"static-lib").unwrap();
        fs::write(dir.path().join("libperfr_api.a"), b"static-a").unwrap();
        fs::create_dir(dir.path().join("include")).unwrap();
        fs::write(dir.path().join("include").join("perfr.h"), b"header").unwrap();

        let report = run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: false,
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
        .unwrap();
        let text = report.text;
        assert_eq!(report.code, EXIT_OK);
        assert!(
            text.starts_with(&format!(
                "updated perfr {} -> 9.9.9 in {}\nrestart any running perfr to use the new version\n",
                build_info::package_version(),
                dir.path().display()
            )),
            "{text}"
        );
        for name in RETIRED_INSTALL_FILES {
            assert!(
                text.contains(&format!("removed {}", dir.path().join(name).display())),
                "missing removal line for {name} in {text}"
            );
            assert!(!dir.path().join(name).exists(), "{name} still present");
        }
        assert_eq!(fs::read(dir.path().join("perfr")).unwrap(), b"new-binary");
        assert_eq!(fs::read(dir.path().join("libperfr.so")).unwrap(), b"agent");
        assert_eq!(
            fs::read(dir.path().join("perfr_api.lib")).unwrap(),
            b"static-lib"
        );
        assert_eq!(
            fs::read(dir.path().join("libperfr_api.a")).unwrap(),
            b"static-a"
        );
        assert_eq!(
            fs::read(dir.path().join("include").join("perfr.h")).unwrap(),
            b"header"
        );
    }

    #[test]
    fn update_says_nothing_when_no_retired_file_is_present() {
        let removed = remove_retired(tempfile::tempdir().unwrap().path()).unwrap();
        assert!(removed.is_empty());
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
                yes: false,
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

    fn apply_release(exe: &Path, platform: Platform, asset: &str, archive: Vec<u8>) -> String {
        let hash = hex_encode(&sha256(&archive));
        let sums = format!("{hash}  {asset}\n");
        let sidecar = format!("{asset}.sha256");
        let json = release_json("perfr-v9.9.9", &[asset, sidecar.as_str()]);
        let fetch = MapFetch {
            files: HashMap::from([
                (LATEST_URL.to_string(), json),
                (format!("https://example.test/{asset}"), archive),
                (format!("https://example.test/{sidecar}"), sums.into_bytes()),
            ]),
        };
        run_with(
            &UpdateOpts {
                check: false,
                tag: None,
                yes: false,
            },
            &fetch,
            exe,
            platform,
            ReplaceStyle::Atomic,
            &None,
        )
        .unwrap()
        .text
    }

    #[test]
    fn unix_update_installs_the_header_beside_the_bin_dir() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join(".local").join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("perfr");
        fs::write(&exe, b"old").unwrap();
        fs::write(bin.join("perfr.h"), b"stale").unwrap();
        let archive = archive::pack_tar_gz(&[
            ("perfr/perfr", b"new-binary", 0o755),
            ("perfr/perfr.h", b"from-root", 0o644),
            ("perfr/include/perfr.h", b"from-include", 0o644),
            ("perfr/libperfr.so", b"agent", 0o644),
        ]);
        let text = apply_release(
            &exe,
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            "perfr-linux-x86_64.tar.gz",
            archive,
        );
        let header = root.path().join(".local").join("include").join("perfr.h");
        assert_eq!(fs::read(&header).unwrap(), b"from-include");
        assert!(!bin.join("perfr.h").exists());
        assert!(
            text.contains(&format!(
                "updated perfr {} -> 9.9.9 in {}\n",
                build_info::package_version(),
                bin.display()
            )),
            "{text}"
        );
        assert!(
            text.contains("restart any running perfr to use the new version\n"),
            "{text}"
        );
        assert!(text.contains("removed "), "{text}");
        assert!(text.contains("perfr.h"), "{text}");
        assert_eq!(fs::read(bin.join("libperfr.so")).unwrap(), b"agent");

        // An older archive carries the header only at perfr/perfr.h.
        fs::write(bin.join("perfr.h"), b"stale-again").unwrap();
        let archive = archive::pack_tar_gz(&[
            ("perfr/perfr", b"newer", 0o755),
            ("perfr/perfr.h", b"only-root", 0o644),
        ]);
        apply_release(
            &exe,
            Platform {
                os: "macos",
                arch: "aarch64",
            },
            "perfr-macos-universal.tar.gz",
            archive,
        );
        assert_eq!(fs::read(&header).unwrap(), b"only-root");
        assert!(!bin.join("perfr.h").exists());
    }

    #[test]
    fn windows_update_installs_the_header_under_the_install_dir() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("perfr");
        fs::create_dir_all(&install).unwrap();
        let exe = install.join("perfr.exe");
        fs::write(&exe, b"old").unwrap();
        fs::write(install.join("perfr.h"), b"stale").unwrap();
        let archive = archive::pack_zip_stored(&[
            ("perfr/perfr.exe", b"exe"),
            ("perfr/perfr.h", b"from-root"),
            ("perfr/include/perfr.h", b"win-header"),
        ]);
        let text = apply_release(
            &exe,
            Platform {
                os: "windows",
                arch: "x86_64",
            },
            "perfr-windows-x86_64.zip",
            archive,
        );
        assert_eq!(
            fs::read(install.join("include").join("perfr.h")).unwrap(),
            b"win-header"
        );
        assert!(!install.join("perfr.h").exists());
        assert!(
            text.contains(&format!(
                "updated perfr {} -> 9.9.9 in {}\nrestart any running perfr to use the new version\n",
                build_info::package_version(),
                install.display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("removed {}", install.join("perfr.h").display())),
            "{text}"
        );
        assert_eq!(fs::read(&exe).unwrap(), b"exe");

        fs::write(install.join("perfr.h"), b"stale-again").unwrap();
        let archive = archive::pack_zip_stored(&[
            ("perfr/perfr.exe", b"exe2"),
            ("perfr/perfr.h", b"only-root"),
        ]);
        apply_release(
            &exe,
            Platform {
                os: "windows",
                arch: "aarch64",
            },
            "perfr-windows-x86_64.zip",
            archive,
        );
        assert_eq!(
            fs::read(install.join("include").join("perfr.h")).unwrap(),
            b"only-root"
        );
        assert!(!install.join("perfr.h").exists());
    }
}
