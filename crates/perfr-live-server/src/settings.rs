// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! User settings the service keeps on disk, so a choice made in the viewer
//! holds across browsers, sessions and restarts: `~/.config/perfr/
//! settings.json` (`$XDG_CONFIG_HOME/perfr/settings.json` when that is set;
//! the invoking user's home when the service runs under sudo, and the file
//! is that user's: written with their filesystem identity where the system
//! has one to lend, handed back to them where it does not). `GET /api/settings` reads it, `PUT` replaces
//! it and saves. Unknown keys in an older or newer file are kept as they
//! are; a missing key takes its default.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Calls a second past which a hooked function is switched off mid-capture,
/// by default. A function this hot is not something to time with a hook --
/// the sampling profile already shows it -- and hooked it costs the target
/// about a core (a microsecond of trampoline and record per call) for spans
/// no timeline can show. Ten thousand entries in any 100 ms window trips it.
pub const DEFAULT_MAX_HOOK_CALLS_PER_S: u64 = 100_000;

/// The auto-profiling budget by default: scopes a second across every
/// function it hooks. About what a timeline shows usefully at a glance, and
/// a cost the target does not feel (a microsecond or so per call).
pub const DEFAULT_AUTO_PROFILE_SCOPES_PER_S: u64 = 1_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Switch off a hooked function that fires past `max_hook_calls_per_s`.
    #[serde(default = "default_true")]
    pub auto_unhook: bool,
    #[serde(default = "default_limit")]
    pub max_hook_calls_per_s: u64,
    /// The viewer's track order file: one process-name pattern per line,
    /// matched processes first on the rail in file order. The viewer reads
    /// and writes it; the service only keeps it. Empty means no file.
    #[serde(default)]
    pub track_order: String,
    /// Auto-profiling's budget: the most scopes a second, over every function
    /// it hooks, it aims for (`auto_profile.rs` in the service).
    #[serde(default = "default_auto_profile_budget")]
    pub auto_profile_scopes_per_s: u64,
    /// Keys this build does not know, carried through a load/save cycle.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

fn default_limit() -> u64 {
    DEFAULT_MAX_HOOK_CALLS_PER_S
}

fn default_auto_profile_budget() -> u64 {
    DEFAULT_AUTO_PROFILE_SCOPES_PER_S
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            auto_unhook: true,
            max_hook_calls_per_s: DEFAULT_MAX_HOOK_CALLS_PER_S,
            track_order: String::new(),
            auto_profile_scopes_per_s: DEFAULT_AUTO_PROFILE_SCOPES_PER_S,
            other: Default::default(),
        }
    }
}

impl Settings {
    /// The call-rate limit a capture applies: 0 when auto-unhook is off.
    pub fn effective_max_hook_calls_per_s(&self) -> u64 {
        if self.auto_unhook {
            self.max_hook_calls_per_s
        } else {
            0
        }
    }

    /// Where the file lives for this run.
    pub fn path() -> Option<PathBuf> {
        Some(config_dir()?.join("perfr").join("settings.json"))
    }

    pub fn load() -> Settings {
        // Read as the user whose file it is: root reading whatever the
        // name points at would hand the keys of any JSON file on the
        // machine to `GET /api/settings`.
        for_the_settings_owner(|_| Self::path().map(|p| Self::load_from(&p)).unwrap_or_default())
    }

    pub fn load_from(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(settings) => settings,
                Err(error) => {
                    eprintln!("perfr: {} is not valid settings JSON ({error}); using defaults", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let path = Self::path().ok_or("no home directory to keep settings in")?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())? + "\n";
        for_the_settings_owner(|for_other| write_replacing(path, text.as_bytes(), for_other))
    }
}

/// `$XDG_CONFIG_HOME`, else the home of the user who ran the service (the
/// sudo invoker when it runs privileged, so a setting made from the viewer
/// is the same file whether or not the service was started with sudo).
fn config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    invoker_home().map(|home| home.join(".config"))
}

fn invoker_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Some(user) = std::env::var_os("SUDO_USER") {
            let user = user.to_string_lossy().into_owned();
            if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
                for line in passwd.lines() {
                    let fields: Vec<&str> = line.split(':').collect();
                    if fields.len() >= 6 && fields[0] == user {
                        return Some(PathBuf::from(fields[5]));
                    }
                }
            }
        }
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Who a file is written for when that is not who is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    uid: u32,
    gid: u32,
}

/// The user who started a service that now runs as root (`sudo perfr`):
/// the settings are that user's, in that user's directory, and a file left
/// there as root's is one the next unprivileged service can read but not
/// update. `None` when this process is not root on someone's behalf.
fn sudo_invoker() -> Option<Owner> {
    #[cfg(unix)]
    {
        // SAFETY: no arguments, cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            return None;
        }
        let id = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
        Some(Owner { uid: id("SUDO_UID")?, gid: id("SUDO_GID")? })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// This thread's filesystem identity switched to another user's, until
/// dropped: every path lookup and every file created is then judged, and
/// owned, as that user's. Root writing into a directory the invoking user
/// controls is otherwise a way for anything running as that user to have
/// root create, overwrite or chown a file of its choosing, by making the
/// directory or the file a link: with the user's identity the kernel
/// refuses exactly what it would refuse the user. Linux only; per thread,
/// so the rest of the service keeps its privileges meanwhile.
///
/// The supplementary groups are still root's (they cannot be changed for
/// one thread), which matters only for files writable by a group root is
/// in.
#[cfg(target_os = "linux")]
struct FsIdentity {
    previous: Owner,
}

#[cfg(target_os = "linux")]
impl FsIdentity {
    fn assume(who: Owner) -> Option<FsIdentity> {
        // SAFETY: both calls take an id and touch only this thread's
        // credentials.
        let previous = unsafe {
            let gid = libc::setfsgid(who.gid) as u32;
            let uid = libc::setfsuid(who.uid) as u32;
            Owner { uid, gid }
        };
        let identity = FsIdentity { previous };
        // Neither call reports failure. Asking for an id that cannot be
        // (-1) changes nothing and returns the current one.
        // SAFETY: as above.
        let now = unsafe {
            Owner { uid: libc::setfsuid(u32::MAX) as u32, gid: libc::setfsgid(u32::MAX) as u32 }
        };
        (now == who).then_some(identity)
    }
}

#[cfg(target_os = "linux")]
impl Drop for FsIdentity {
    fn drop(&mut self) {
        // SAFETY: as in `assume`. The uid first: going back to 0 restores
        // the capabilities the gid change may need.
        unsafe {
            libc::setfsuid(self.previous.uid);
            libc::setfsgid(self.previous.gid);
        }
    }
}

/// Runs `f` on behalf of the user the settings belong to. `f` is told who
/// that is only when it still has to take care itself: it runs as root, for
/// someone else, and could not take that user's identity.
fn for_the_settings_owner<T>(f: impl FnOnce(Option<Owner>) -> T) -> T {
    let Some(invoker) = sudo_invoker() else {
        return f(None);
    };
    #[cfg(target_os = "linux")]
    if let Some(_identity) = FsIdentity::assume(invoker) {
        return f(None);
    }
    f(Some(invoker))
}

static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The `n`th temporary name next to `path` in this process.
fn temp_path(path: &Path, n: u64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}.{n}", std::process::id()));
    path.with_file_name(name)
}

/// Replaces the file at `path` with `bytes` in one step: written to a
/// temporary file next to it, flushed, and renamed over it. Whoever reads
/// the path sees the old file or the new one, never a truncated one (a
/// crash or a full disk mid-write used to leave that, and the next start
/// read it as "not valid settings JSON" and went on with the defaults), and
/// two writers cannot interleave.
///
/// With `for_other`, this process is root writing for that user in a
/// directory the user controls, and nothing may go through a link the user
/// could have planted: a linked directory or file is refused, and what is
/// created is handed to the user by descriptor, not by name. Without it the
/// caller acts as the owner, whose own link to the file is followed and
/// kept, as a plain write would.
fn write_replacing(path: &Path, bytes: &[u8], for_other: Option<Owner>) -> Result<(), String> {
    use std::io::Write;
    let shown = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let refuse_link = |p: &Path| {
        format!("{} is a symbolic link: not writing settings through it as root for another user", p.display())
    };
    if let Some(dir) = path.parent() {
        if for_other.is_some() && is_link(dir) {
            return Err(refuse_link(dir));
        }
        // The directories that are new are the ones to hand over.
        let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.as_os_str().is_empty() && !d.exists()).collect();
        std::fs::create_dir_all(dir).map_err(|e| shown(dir, e))?;
        #[cfg(unix)]
        if let Some(owner) = for_other {
            for created in missing {
                // Never through a link: if the name was swapped for one
                // since, it is the link that changes hands.
                let _ = std::os::unix::fs::lchown(created, Some(owner.uid), Some(owner.gid));
            }
        }
        #[cfg(not(unix))]
        let _ = missing;
    }
    let target = if is_link(path) {
        if for_other.is_some() {
            return Err(refuse_link(path));
        }
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let temp = temp_path(&target, TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    // A leftover of that name, or a link put there, goes: removing a link
    // removes the link, and `create_new` opens nothing that exists.
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).map_err(|e| shown(&temp, e))?;
    let written = (|| {
        #[cfg(unix)]
        if let Some(owner) = for_other {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        }
        // A file the user had closed to others stays closed.
        if let Ok(existing) = std::fs::metadata(&target) {
            file.set_permissions(existing.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &target)
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(shown(path, error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_on_at_a_hundred_thousand() {
        let s = Settings::default();
        assert!(s.auto_unhook);
        assert_eq!(s.effective_max_hook_calls_per_s(), 100_000);
    }

    #[test]
    fn off_means_no_limit_whatever_the_value() {
        let s = Settings { auto_unhook: false, max_hook_calls_per_s: 5, ..Default::default() };
        assert_eq!(s.effective_max_hook_calls_per_s(), 0);
    }

    #[test]
    fn round_trips_through_a_file_and_keeps_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("perfr-settings-test-{}", std::process::id()));
        let path = dir.join("perfr").join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Settings::load_from(&path), Settings::default(), "no file: defaults");
        let mut s = Settings { auto_unhook: false, max_hook_calls_per_s: 250_000, ..Default::default() };
        s.other.insert("future_knob".into(), serde_json::json!({"a": 1}));
        s.save_to(&path).unwrap();
        let back = Settings::load_from(&path);
        assert_eq!(back, s);
        assert_eq!(back.other["future_knob"]["a"], 1);
        // An older file that predates a key: the key takes its default.
        std::fs::write(&path, r#"{"max_hook_calls_per_s": 42}"#).unwrap();
        let older = Settings::load_from(&path);
        assert!(older.auto_unhook);
        assert_eq!(older.max_hook_calls_per_s, 42);
        // Garbage: defaults, not a crash.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("perfr-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_reader_never_sees_a_half_written_file_however_many_writers_there_are() {
        let dir = scratch("atomic");
        let path = dir.join("perfr").join("settings.json");
        let short = Settings { max_hook_calls_per_s: 1, ..Default::default() };
        let mut long = Settings { max_hook_calls_per_s: 2, track_order: "x".repeat(64 << 10), ..Default::default() };
        long.other.insert("future_knob".into(), serde_json::json!({"a": 1}));
        short.save_to(&path).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..4)
                .map(|n| {
                    let (path, body) = (&path, if n % 2 == 0 { &short } else { &long });
                    scope.spawn(move || {
                        for _ in 0..200 {
                            body.save_to(path).unwrap();
                        }
                    })
                })
                .collect();
            // What the next start of the service would do at any moment:
            // a file caught truncated reads as "not valid settings JSON",
            // and the save after that replaces the user's settings with
            // the defaults.
            let reader = scope.spawn(|| {
                let mut reads = 0u32;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let read: Settings = serde_json::from_str(&text)
                        .unwrap_or_else(|e| panic!("read {} torn bytes of settings: {e}", text.len()));
                    assert!(read == short || read == long, "a mix of two saves");
                    reads += 1;
                }
                reads
            });
            for writer in writers {
                writer.join().unwrap();
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(reader.join().unwrap() > 0);
        });
        // Nothing is left behind next to the file.
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("settings.json")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_users_own_link_to_their_settings_is_written_through_and_kept() {
        // A dotfiles checkout: the file in the config directory is a link.
        let dir = scratch("own-link");
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        std::fs::create_dir_all(dir.join("perfr")).unwrap();
        let real = dir.join("dotfiles").join("perfr.json");
        std::fs::write(&real, "{}").unwrap();
        let path = dir.join("perfr").join("settings.json");
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let s = Settings { max_hook_calls_per_s: 7, ..Default::default() };
        s.save_to(&path).unwrap();
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink(), "the link was replaced by a file");
        assert_eq!(Settings::load_from(&real), s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn written_for_someone_else_nothing_goes_through_a_link() {
        // Root saving into the invoking user's directory, where that user
        // decides what every name points at.
        let me = Owner { uid: unsafe { libc::geteuid() }, gid: unsafe { libc::getegid() } };
        let body = b"{}\n";
        let dir = scratch("for-other");
        let victim = dir.join("victim");
        std::fs::write(&victim, "precious").unwrap();

        // settings.json is a link to a file the user could not write.
        let config = dir.join("a").join("perfr");
        std::fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(&victim, config.join("settings.json")).unwrap();
        let error = write_replacing(&config.join("settings.json"), body, Some(me)).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");

        // The directory is a link to one the user could not write into.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("b").join("perfr")).unwrap();
        let error = write_replacing(&dir.join("b").join("perfr").join("settings.json"), body, Some(me)).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "a file was created through the link");

        // A link waiting where the temporary file goes is not followed.
        let config = dir.join("c").join("perfr");
        std::fs::create_dir_all(&config).unwrap();
        let path = config.join("settings.json");
        for n in 0..64 {
            let _ = std::os::unix::fs::symlink(&victim, temp_path(&path, n));
        }
        write_replacing(&path, body, Some(me)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");

        // Nothing in the way: written, and the new directories are theirs.
        let path = dir.join("d").join("perfr").join("settings.json");
        write_replacing(&path, body, Some(me)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_filesystem_identity_is_only_taken_when_it_really_changed() {
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        if uid == 0 {
            return;
        }
        // Becoming who we already are works and changes nothing.
        assert!(FsIdentity::assume(Owner { uid, gid }).is_some());
        // Without the privilege the kernel leaves the ids as they were and
        // reports no error: that must read as "did not happen".
        assert!(FsIdentity::assume(Owner { uid: 0, gid: 0 }).is_none());
        assert!(FsIdentity::assume(Owner { uid: uid + 1, gid }).is_none());
        // And the thread is itself again afterwards.
        let dir = scratch("identity");
        std::fs::write(dir.join("still-mine"), "x").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
