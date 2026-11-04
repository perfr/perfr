// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Optional Frida function hooks.
//!
//! The core release archive does not contain `perfr-frida-helper`,
//! `libperfr_frida_agent`, or the Frida licenses. Those ship as
//! `perfr-hooks-<platform>` for the same perfr version, with a `.sha256`.
//! `perfr hooks install` downloads that archive. The first time a capture
//! needs hooks, an interactive terminal is asked; MCP and any other
//! non-interactive session get an error that names `perfr hooks install`.
//! `PERFR_HOOKS_AUTO_INSTALL=1` and `--hooks-auto` download without asking.
//!
//! Files land next to `perfr` when that directory is writable, otherwise in
//! the hooks data directory (`PERFR_HOOKS_DIR`, or `~/.local/share/perfr/hooks`,
//! or `%LOCALAPPDATA%\perfr\hooks`). Lookup checks the environment, then
//! beside the executable, then the data directory.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::archive::{self, PackedFile};
use crate::build_info;
use crate::update::{
    self, install_files, package_version_of_tag, Fetch, Platform, Release, ReplaceStyle,
};

pub const VERSION_FILE: &str = "perfr-hooks-version";

static AUTO_FLAG: AtomicBool = AtomicBool::new(false);

pub fn set_auto_from_flag() {
    AUTO_FLAG.store(true, Ordering::Relaxed);
}

pub fn auto_enabled() -> bool {
    AUTO_FLAG.load(Ordering::Relaxed) || env_truthy("PERFR_HOOKS_AUTO_INSTALL")
}

fn prompt_suppressed() -> bool {
    env_truthy("PERFR_HOOKS_NO_PROMPT")
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookNames {
    pub helper: &'static str,
    pub agent: &'static str,
}

pub fn names_for(os: &str) -> HookNames {
    match os {
        "windows" => HookNames {
            helper: "perfr-frida-helper.exe",
            agent: "perfr_frida_agent.dll",
        },
        "macos" => HookNames {
            helper: "perfr-frida-helper",
            agent: "libperfr_frida_agent.dylib",
        },
        _ => HookNames {
            helper: "perfr-frida-helper",
            agent: "libperfr_frida_agent.so",
        },
    }
}

pub fn hooks_asset_for(platform: Platform) -> Result<&'static str, String> {
    match (platform.os, platform.arch) {
        ("linux", "x86_64") => Ok("perfr-hooks-linux-x86_64.tar.gz"),
        ("macos", "x86_64" | "aarch64") => Ok("perfr-hooks-macos-universal.tar.gz"),
        ("windows", "x86_64" | "aarch64") => Ok("perfr-hooks-windows-x86_64.zip"),
        ("linux", "aarch64") => Err(
            "no published Linux arm64 hooks build. The release is a static Linux x86-64 binary and a macOS universal binary."
                .into(),
        ),
        _ => Err(format!(
            "no hooks archive for {}/{}",
            platform.os, platform.arch
        )),
    }
}

/// What to do the first time hooks are needed and are not already installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstUse {
    Ready,
    Download,
    Prompt,
    Refuse,
}

pub fn first_use(ready: bool, tty: bool, auto: bool) -> FirstUse {
    if ready {
        FirstUse::Ready
    } else if auto {
        FirstUse::Download
    } else if tty {
        FirstUse::Prompt
    } else {
        FirstUse::Refuse
    }
}

pub fn missing_hooks_message(package: &str) -> String {
    format!(
        "Function hooks are not installed for perfr {package}. Run `perfr hooks install`. \
         Set PERFR_HOOKS_AUTO_INSTALL=1 or pass --hooks-auto to download them on first use."
    )
}

pub fn mismatch_message(found: &str, package: &str) -> String {
    format!("Function hooks are {found} but perfr is {package}. Run `perfr hooks install`.")
}

/// The version file is the cargo package version (`0.1.0`). A release tag
/// (`perfr-v0.1.0` or `perfr-v0.1.0-rc.1`) matches that triple: prerelease
/// tags are built from the same `CARGO_PKG_VERSION`.
pub fn hooks_record_matches(recorded: &str, expected: &str) -> bool {
    let recorded = recorded.trim();
    let expected = expected.trim();
    if recorded.is_empty() || expected.is_empty() {
        return false;
    }
    if recorded == expected {
        return true;
    }
    match (
        package_version_of_tag(recorded).or_else(|| plain_version(recorded)),
        package_version_of_tag(expected).or_else(|| plain_version(expected)),
    ) {
        (Some(left), Some(right)) => version_triple(&left) == version_triple(&right),
        _ => false,
    }
}

fn plain_version(text: &str) -> Option<String> {
    let text = text.trim().strip_prefix('v').unwrap_or(text.trim());
    let numbers = text.split('-').next().unwrap_or(text);
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{major}.{minor}.{patch}"))
}

fn version_triple(version: &str) -> String {
    version.split('-').next().unwrap_or(version).to_string()
}

#[derive(Clone, Debug)]
pub struct Located {
    pub dir: PathBuf,
    pub helper: PathBuf,
    pub agent: PathBuf,
    pub version: Option<String>,
    pub from_env: bool,
}

pub fn locate_in(
    exe_dir: Option<&Path>,
    data_dir: &Path,
    names: HookNames,
    env_helper: Option<&Path>,
    env_agent: Option<&Path>,
) -> Option<Located> {
    if let (Some(helper), Some(agent)) = (env_helper, env_agent) {
        if helper.is_file() && agent.is_file() {
            let dir = helper.parent().unwrap_or(Path::new(".")).to_path_buf();
            return Some(Located {
                version: read_version(&dir),
                dir,
                helper: helper.to_path_buf(),
                agent: agent.to_path_buf(),
                from_env: true,
            });
        }
    }
    for dir in [exe_dir, Some(data_dir)].into_iter().flatten() {
        let helper = dir.join(names.helper);
        let agent = dir.join(names.agent);
        if helper.is_file() && agent.is_file() {
            return Some(Located {
                version: read_version(dir),
                dir: dir.to_path_buf(),
                helper,
                agent,
                from_env: false,
            });
        }
    }
    None
}

fn read_version(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(VERSION_FILE)).ok()?;
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

pub fn version_ready(located: &Located, package: &str) -> bool {
    if located.from_env {
        return true;
    }
    match &located.version {
        None => true,
        Some(found) => hooks_record_matches(found, package),
    }
}

pub fn status_text(located: Option<&Located>, package: &str) -> String {
    let Some(located) = located else {
        return format!(
            "function hooks: not installed\nperfr {package}\nRun `perfr hooks install`.\n"
        );
    };
    if located.from_env {
        return format!(
            "function hooks: installed\ndirectory: {}\nversion: environment override (PERFR_FRIDA_AGENT, PERFR_FRIDA_HELPER)\nhelper: {}\nagent: {}\n",
            located.dir.display(),
            located.helper.display(),
            located.agent.display()
        );
    }
    match &located.version {
        Some(found) if !hooks_record_matches(found, package) => format!(
            "function hooks: version mismatch\ndirectory: {}\nversion: {found} (perfr is {package})\nRun `perfr hooks install`.\n",
            located.dir.display()
        ),
        Some(found) => format!(
            "function hooks: installed\ndirectory: {}\nversion: {found} (matches perfr {package})\nhelper: {}\nagent: {}\n",
            located.dir.display(),
            located.helper.display(),
            located.agent.display()
        ),
        None => format!(
            "function hooks: installed\ndirectory: {}\nversion: not recorded\nhelper: {}\nagent: {}\n",
            located.dir.display(),
            located.helper.display(),
            located.agent.display()
        ),
    }
}

/// One row for `perfr doctor`, padded like the other checks.
pub fn doctor_row(located: Option<&Located>, package: &str) -> String {
    let (status, detail) = match located {
        None => ("missing", "Run `perfr hooks install`.".to_string()),
        Some(located) if located.from_env => {
            ("ok", "PERFR_FRIDA_AGENT and PERFR_FRIDA_HELPER".to_string())
        }
        Some(located) => match &located.version {
            Some(found) if !hooks_record_matches(found, package) => (
                "mismatch",
                format!("hooks {found}, perfr {package}. Run `perfr hooks install`."),
            ),
            Some(found) => ("ok", format!("{found} in {}", located.dir.display())),
            None => (
                "ok",
                format!("version not recorded, in {}", located.dir.display()),
            ),
        },
    };
    format!("{:<28} {:<12} {detail}\n", "function hooks", status)
}

pub fn hooks_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PERFR_HOOKS_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows) {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("perfr").join("hooks");
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("perfr").join("hooks");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local")
        .join("share")
        .join("perfr")
        .join("hooks")
}

pub fn choose_install_dir(
    exe: &Path,
    requested: Option<&Path>,
    data_dir: &Path,
) -> Result<PathBuf, String> {
    if let Some(dir) = requested {
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        return Ok(dir.to_path_buf());
    }
    if let Some(parent) = exe.parent() {
        if dir_is_writable(parent) {
            return Ok(parent.to_path_buf());
        }
    }
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("cannot create {}: {error}", data_dir.display()))?;
    Ok(data_dir.to_path_buf())
}

fn dir_is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".perfr-hooks-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn recorded_version(files: &[PackedFile]) -> Result<String, String> {
    let file = files
        .iter()
        .find(|file| archive::install_relative(&file.path).as_deref() == Some(VERSION_FILE));
    let Some(file) = file else {
        return Err("hooks archive has no perfr-hooks-version".into());
    };
    let text = String::from_utf8(file.bytes.clone())
        .map_err(|_| "perfr-hooks-version is not utf-8".to_string())?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("perfr-hooks-version is empty".into());
    }
    Ok(text)
}

pub fn install_release(
    fetch: &dyn Fetch,
    release: &Release,
    dest: &Path,
    platform: Platform,
    style: ReplaceStyle,
    expected_version: &str,
) -> Result<String, String> {
    let asset_name = hooks_asset_for(platform)?;
    let bytes = update::fetch_verified(fetch, release, asset_name)?;
    let files = archive::unpack_named(asset_name, &bytes)?;
    let recorded = recorded_version(&files)?;
    if !hooks_record_matches(&recorded, expected_version) {
        return Err(format!(
            "hooks archive is {recorded}, perfr {expected_version} needs a matching hooks archive"
        ));
    }
    fs::create_dir_all(dest)
        .map_err(|error| format!("cannot create {}: {error}", dest.display()))?;
    install_files(dest, &files, style, false)?;
    Ok(format!(
        "function hooks: installed {recorded} in {}\n",
        dest.display()
    ))
}

/// Directories that already contain a hook file. `perfr update` refreshes
/// each of them and leaves a machine with no hooks alone.
pub fn installed_dirs(exe_dir: Option<&Path>, data_dir: &Path, names: HookNames) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in [exe_dir, Some(data_dir)].into_iter().flatten() {
        if dir.join(names.helper).is_file()
            || dir.join(names.agent).is_file()
            || dir.join(VERSION_FILE).is_file()
        {
            let path = dir.to_path_buf();
            if !dirs.contains(&path) {
                dirs.push(path);
            }
        }
    }
    dirs
}

pub fn update_installed(
    fetch: &dyn Fetch,
    release: &Release,
    exe_dir: &Path,
    data_dir: &Path,
    platform: Platform,
    style: ReplaceStyle,
) -> Result<String, String> {
    let names = names_for(platform.os);
    let dirs = installed_dirs(Some(exe_dir), data_dir, names);
    if dirs.is_empty() {
        return Ok("function hooks: not installed\n".into());
    }
    let expected = package_version_of_tag(&release.tag)
        .ok_or_else(|| format!("release tag {} has no version", release.tag))?;
    let asset_name = hooks_asset_for(platform)?;
    let bytes = update::fetch_verified(fetch, release, asset_name).map_err(|error| {
        format!("the core install was updated, but function hooks were not: {error}")
    })?;
    let files = archive::unpack_named(asset_name, &bytes)?;
    let recorded = recorded_version(&files)?;
    if !hooks_record_matches(&recorded, &expected) {
        return Err(format!(
            "the core install was updated, but the hooks archive is {recorded} and {} is {expected}",
            release.tag
        ));
    }
    for dir in &dirs {
        install_files(dir, &files, style, false)?;
    }
    let listed = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("function hooks: updated in {listed}\n"))
}

pub fn uninstall_in(exe_dir: Option<&Path>, data_dir: &Path, names: HookNames) -> String {
    let mut removed = Vec::new();
    for dir in installed_dirs(exe_dir, data_dir, names) {
        for name in [names.helper, names.agent, VERSION_FILE] {
            let path = dir.join(name);
            if path.is_file() && fs::remove_file(&path).is_ok() {
                removed.push(path.display().to_string());
            }
        }
        let licenses = dir.join("licenses");
        if licenses.is_dir() && fs::remove_dir_all(&licenses).is_ok() {
            removed.push(format!("{}/", licenses.display()));
        }
    }
    if removed.is_empty() {
        "function hooks: not installed\n".into()
    } else {
        format!("removed:\n{}\n", removed.join("\n"))
    }
}

/// Prompt, download, or refuse. `download` runs only when the decision says to.
pub fn ensure_with(
    ready: bool,
    tty: bool,
    auto: bool,
    message: &str,
    mut prompt: impl FnMut(&str) -> bool,
    mut download: impl FnMut() -> Result<String, String>,
) -> Result<String, String> {
    match first_use(ready, tty, auto) {
        FirstUse::Ready => Ok("function hooks: installed\n".into()),
        FirstUse::Download => download(),
        FirstUse::Prompt => {
            if prompt(message) {
                download()
            } else {
                Err(message.to_string())
            }
        }
        FirstUse::Refuse => Err(message.to_string()),
    }
}

pub fn find_installed(name: &str) -> Option<PathBuf> {
    for dir in search_dirs() {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    let data = hooks_data_dir();
    if !dirs.contains(&data) {
        dirs.push(data);
    }
    dirs
}

pub fn installed_version_ok() -> bool {
    let package = build_info::package_version();
    for dir in search_dirs() {
        if let Some(found) = read_version(&dir) {
            if !hooks_record_matches(&found, package) {
                return false;
            }
        }
    }
    true
}

fn hooks_ready(names: HookNames) -> bool {
    find_installed(names.helper).is_some()
        && find_installed(names.agent).is_some()
        && installed_version_ok()
}

pub fn ensure_for_launch() -> Result<(), String> {
    let names = names_for(Platform::host().os);
    if hooks_ready(names) {
        return Ok(());
    }
    let package = build_info::package_version();
    let message = mismatch_or_missing(package);
    let tty = crate::preflight::stdin_is_tty() && !prompt_suppressed();
    ensure_with(false, tty, auto_enabled(), &message, prompt_yes, || {
        download_for_package(package)
    })
    .map(|_| ())
}

fn mismatch_or_missing(package: &str) -> String {
    for dir in search_dirs() {
        if let Some(found) = read_version(&dir) {
            if !hooks_record_matches(&found, package) {
                return mismatch_message(&found, package);
            }
        }
    }
    missing_hooks_message(package)
}

fn prompt_yes(message: &str) -> bool {
    eprintln!("{message}");
    eprint!("Download function hooks for this perfr version? [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn download_for_package(package: &str) -> Result<String, String> {
    let tag = format!("perfr-v{package}");
    let fetch = update::CommandFetch;
    let release = update::release_for_tag(&fetch, &tag).map_err(|error| {
        format!("{error}\nNo hooks archive for {tag}. Run `perfr hooks install` once a perfr-v* release publishes perfr-hooks.")
    })?;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("perfr"));
    let data = hooks_data_dir();
    let dest = choose_install_dir(&exe, None, &data)?;
    install_release(
        &fetch,
        &release,
        &dest,
        Platform::host(),
        ReplaceStyle::host(),
        package,
    )
}

pub fn doctor_line() -> String {
    let names = names_for(Platform::host().os);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let located = locate_in(
        exe_dir.as_deref(),
        &hooks_data_dir(),
        names,
        std::env::var_os("PERFR_FRIDA_HELPER")
            .as_deref()
            .map(Path::new),
        std::env::var_os("PERFR_FRIDA_AGENT")
            .as_deref()
            .map(Path::new),
    );
    doctor_row(located.as_ref(), build_info::package_version())
}

#[derive(Clone, Debug)]
pub struct HooksOpts {
    pub dir: Option<PathBuf>,
}

pub fn command(name: &str, opts: &HooksOpts) -> i32 {
    let names = names_for(Platform::host().os);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("perfr"));
    let exe_dir = exe.parent().map(Path::to_path_buf);
    let data = hooks_data_dir();
    let result = match name {
        "status" => {
            let located = locate_in(
                exe_dir.as_deref(),
                &data,
                names,
                std::env::var_os("PERFR_FRIDA_HELPER")
                    .as_deref()
                    .map(Path::new),
                std::env::var_os("PERFR_FRIDA_AGENT")
                    .as_deref()
                    .map(Path::new),
            );
            let ready = located
                .as_ref()
                .is_some_and(|found| version_ready(found, build_info::package_version()));
            let text = status_text(located.as_ref(), build_info::package_version());
            print!("{text}");
            return if ready { 0 } else { 1 };
        }
        "uninstall" => Ok(uninstall_in(exe_dir.as_deref(), &data, names)),
        "install" => install_command(&exe, opts),
        other => Err(format!("unknown hooks command {other}")),
    };
    match result {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(error) => {
            eprintln!("perfr hooks: {error}");
            2
        }
    }
}

fn install_command(exe: &Path, opts: &HooksOpts) -> Result<String, String> {
    let package = build_info::package_version();
    download_into(package, exe, opts.dir.as_deref())
}

fn download_into(package: &str, exe: &Path, requested: Option<&Path>) -> Result<String, String> {
    let tag = format!("perfr-v{package}");
    let fetch = update::CommandFetch;
    let release = update::release_for_tag(&fetch, &tag)
        .map_err(|error| format!("{error}\nNo hooks archive for {tag}."))?;
    let dest = choose_install_dir(exe, requested, &hooks_data_dir())?;
    install_release(
        &fetch,
        &release,
        &dest,
        Platform::host(),
        ReplaceStyle::host(),
        package,
    )
}

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

    fn release_with(tag: &str, names: &[&str]) -> Release {
        let assets = names
            .iter()
            .map(|name| update::Asset {
                name: (*name).to_string(),
                url: format!("https://example.test/{name}"),
            })
            .collect();
        Release {
            tag: tag.to_string(),
            assets,
        }
    }

    #[test]
    fn first_use_prompts_on_a_tty_and_refuses_otherwise() {
        assert_eq!(first_use(true, true, true), FirstUse::Ready);
        assert_eq!(first_use(false, true, true), FirstUse::Download);
        assert_eq!(first_use(false, true, false), FirstUse::Prompt);
        assert_eq!(first_use(false, false, false), FirstUse::Refuse);
        let message = missing_hooks_message("0.1.0");
        assert!(message.contains("perfr hooks install"), "{message}");
        assert!(message.contains("PERFR_HOOKS_AUTO_INSTALL"), "{message}");
        let mismatch = mismatch_message("0.0.9", "0.1.0");
        assert!(mismatch.contains("perfr hooks install"), "{mismatch}");
    }

    #[test]
    fn ensure_with_refuses_without_downloading_when_there_is_no_tty() {
        let mut downloads = 0;
        let err = ensure_with(
            false,
            false,
            false,
            "Run `perfr hooks install`.",
            |_| panic!("non-interactive must not prompt"),
            || {
                downloads += 1;
                Ok("nope".into())
            },
        )
        .unwrap_err();
        assert_eq!(downloads, 0);
        assert!(err.contains("perfr hooks install"), "{err}");
        let mut prompted = false;
        let text = ensure_with(
            false,
            true,
            false,
            "need hooks",
            |_| {
                prompted = true;
                true
            },
            || Ok("downloaded\n".into()),
        )
        .unwrap();
        assert!(prompted);
        assert_eq!(text, "downloaded\n");
    }

    #[test]
    fn a_version_record_matches_the_package_and_the_tag() {
        assert!(hooks_record_matches("0.1.0", "0.1.0"));
        assert!(hooks_record_matches("0.1.0", "perfr-v0.1.0"));
        assert!(hooks_record_matches("0.1.0", "perfr-v0.1.0-rc.1"));
        assert!(!hooks_record_matches("0.1.0", "perfr-v0.2.0"));
        assert!(!hooks_record_matches("", "0.1.0"));
        assert_eq!(
            hooks_asset_for(Platform {
                os: "linux",
                arch: "x86_64"
            })
            .unwrap(),
            "perfr-hooks-linux-x86_64.tar.gz"
        );
        assert_eq!(
            hooks_asset_for(Platform {
                os: "windows",
                arch: "aarch64"
            })
            .unwrap(),
            "perfr-hooks-windows-x86_64.zip"
        );
    }

    #[test]
    fn locate_prefers_the_environment_then_the_executable_directory() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin");
        let data = dir.path().join("data");
        fs::create_dir_all(&exe).unwrap();
        fs::create_dir_all(&data).unwrap();
        let names = names_for("linux");
        fs::write(exe.join(names.helper), b"helper").unwrap();
        fs::write(exe.join(names.agent), b"agent").unwrap();
        fs::write(exe.join(VERSION_FILE), b"0.1.0\n").unwrap();
        let found = locate_in(Some(&exe), &data, names, None, None).unwrap();
        assert_eq!(found.dir, exe);
        assert!(version_ready(&found, "0.1.0"));
        assert!(!version_ready(
            &Located {
                version: Some("0.2.0".into()),
                from_env: false,
                ..found.clone()
            },
            "0.1.0"
        ));
        let env_helper = data.join(names.helper);
        let env_agent = data.join(names.agent);
        fs::write(&env_helper, b"env-helper").unwrap();
        fs::write(&env_agent, b"env-agent").unwrap();
        let from_env = locate_in(
            Some(&exe),
            &data,
            names,
            Some(&env_helper),
            Some(&env_agent),
        )
        .unwrap();
        assert!(from_env.from_env);
        assert!(version_ready(&from_env, "9.9.9"));
        let row = doctor_row(None, "0.1.0");
        assert!(row.contains("function hooks"), "{row}");
        assert!(row.contains("perfr hooks install"), "{row}");
    }

    fn hooks_archive(version: &str, helper: &[u8]) -> Vec<u8> {
        archive::pack_tar_gz(&[
            ("perfr-hooks/perfr-frida-helper", helper, 0o755),
            ("perfr-hooks/libperfr_frida_agent.so", b"agent", 0o644),
            ("perfr-hooks/licenses/NOTICE", b"frida", 0o644),
            ("perfr-hooks/perfr-hooks-version", version.as_bytes(), 0o644),
        ])
    }

    #[test]
    fn install_checks_the_checksum_and_the_version() {
        let archive = hooks_archive("0.2.0", b"helper-2");
        let hash = update::hex_encode(&update::sha256(&archive));
        let name = "perfr-hooks-linux-x86_64.tar.gz";
        let sums = format!("{hash}  {name}\n");
        let fetch = MapFetch {
            files: HashMap::from([
                (format!("https://example.test/{name}"), archive.clone()),
                (
                    format!("https://example.test/{name}.sha256"),
                    sums.into_bytes(),
                ),
            ]),
        };
        let release = release_with(
            "perfr-v0.2.0",
            &[name, "perfr-hooks-linux-x86_64.tar.gz.sha256"],
        );
        let dir = tempfile::tempdir().unwrap();
        let err = install_release(
            &fetch,
            &release,
            dir.path(),
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::Atomic,
            "0.1.0",
        )
        .unwrap_err();
        assert!(err.contains("0.2.0"), "{err}");
        assert!(!dir.path().join("perfr-frida-helper").exists());
        let text = install_release(
            &fetch,
            &release,
            dir.path(),
            Platform {
                os: "linux",
                arch: "x86_64",
            },
            ReplaceStyle::Atomic,
            "0.2.0",
        )
        .unwrap();
        assert!(text.contains("installed 0.2.0"), "{text}");
        assert_eq!(
            fs::read(dir.path().join("perfr-frida-helper")).unwrap(),
            b"helper-2"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(VERSION_FILE)).unwrap(),
            "0.2.0"
        );
        assert_eq!(
            fs::read(dir.path().join("licenses").join("NOTICE")).unwrap(),
            b"frida"
        );
    }

    #[test]
    fn update_refreshes_hooks_only_when_they_are_already_installed() {
        let archive = hooks_archive("9.9.9\n", b"new-helper");
        let hash = update::hex_encode(&update::sha256(&archive));
        let name = "perfr-hooks-linux-x86_64.tar.gz";
        let fetch = MapFetch {
            files: HashMap::from([
                (format!("https://example.test/{name}"), archive),
                (
                    format!("https://example.test/{name}.sha256"),
                    format!("{hash}  {name}\n").into_bytes(),
                ),
            ]),
        };
        let release = release_with("perfr-v9.9.9", &[name, &format!("{name}.sha256")]);
        let platform = Platform {
            os: "linux",
            arch: "x86_64",
        };
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let absent = update_installed(
            &fetch,
            &release,
            dir.path(),
            &data,
            platform,
            ReplaceStyle::Atomic,
        )
        .unwrap();
        assert!(absent.contains("not installed"), "{absent}");

        fs::write(dir.path().join("perfr-frida-helper"), b"old").unwrap();
        fs::write(dir.path().join("libperfr_frida_agent.so"), b"old-agent").unwrap();
        let text = update_installed(
            &fetch,
            &release,
            dir.path(),
            &data,
            platform,
            ReplaceStyle::Atomic,
        )
        .unwrap();
        assert!(text.contains("updated"), "{text}");
        assert_eq!(
            fs::read(dir.path().join("perfr-frida-helper")).unwrap(),
            b"new-helper"
        );
        let removed = uninstall_in(Some(dir.path()), &data, names_for("linux"));
        assert!(removed.contains("perfr-frida-helper"), "{removed}");
        assert!(!dir.path().join("perfr-frida-helper").exists());
        assert!(!dir.path().join("licenses").exists());
    }
}
