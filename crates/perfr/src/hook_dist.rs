// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Function hooks ship in the core install.
//!
//! `perfr-inject-helper` and the agent library (`libperfr_agent.so`,
//! `libperfr_agent.dylib`, or `perfr_agent.dll`) sit beside `perfr`.
//! `PERFR_INJECT_HELPER` and `PERFR_AGENT` override that lookup.
//! `perfr hooks status` and `perfr doctor` report whether both files are
//! present. There is no separate hooks archive and no install step.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookNames {
    pub helper: &'static str,
    pub agent: &'static str,
}

pub fn names_for(os: &str) -> HookNames {
    match os {
        "windows" => HookNames {
            helper: "perfr-inject-helper.exe",
            agent: "perfr_agent.dll",
        },
        "macos" => HookNames {
            helper: "perfr-inject-helper",
            agent: "libperfr_agent.dylib",
        },
        _ => HookNames {
            helper: "perfr-inject-helper",
            agent: "libperfr_agent.so",
        },
    }
}

pub fn host_os() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub helper: PathBuf,
    pub agent: PathBuf,
    pub from_env: bool,
}

/// Resolve one component. An environment override that does not point at a
/// file is an error, so a typo does not silently fall back to another copy.
pub fn resolve_in(
    env_key: &str,
    env_path: Option<&Path>,
    exe_dir: Option<&Path>,
    name: &str,
) -> Result<PathBuf, String> {
    if let Some(path) = env_path {
        return if path.is_file() {
            Ok(path.to_path_buf())
        } else {
            Err(format!("{env_key} {} is not a file", path.display()))
        };
    }
    let Some(dir) = exe_dir else {
        return Err(format!(
            "{name} was not found. Function hooks ship beside perfr. Set {env_key} to override the path."
        ));
    };
    let path = dir.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{name} is not beside perfr ({}). Function hooks ship with perfr. Set {env_key} to override the path.",
            dir.display()
        ))
    }
}

pub fn locate_in(
    exe_dir: Option<&Path>,
    names: HookNames,
    env_helper: Option<&Path>,
    env_agent: Option<&Path>,
) -> Result<Located, String> {
    let helper = resolve_in("PERFR_INJECT_HELPER", env_helper, exe_dir, names.helper);
    let agent = resolve_in("PERFR_AGENT", env_agent, exe_dir, names.agent);
    match (helper, agent) {
        (Ok(helper), Ok(agent)) => Ok(Located {
            from_env: env_helper.is_some() || env_agent.is_some(),
            helper,
            agent,
        }),
        (Err(helper), Err(agent)) => Err(format!("{helper}\n{agent}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

pub fn locate() -> Result<Located, String> {
    let names = names_for(host_os());
    let helper = env_path("PERFR_INJECT_HELPER");
    let agent = env_path("PERFR_AGENT");
    locate_in(
        exe_dir().as_deref(),
        names,
        helper.as_deref(),
        agent.as_deref(),
    )
}

/// Canonical path for the service to spawn or inject. Same rules as [`locate`].
pub fn resolve(env_key: &str, name: &str) -> Result<PathBuf, String> {
    let override_path = env_path(env_key);
    let path = resolve_in(
        env_key,
        override_path.as_deref(),
        exe_dir().as_deref(),
        name,
    )?;
    path.canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub fn status_text(found: Result<&Located, &str>, names: HookNames) -> String {
    match found {
        Ok(located) => {
            let source = if located.from_env {
                "environment override (PERFR_INJECT_HELPER, PERFR_AGENT)"
            } else {
                "beside perfr"
            };
            format!(
                "function hooks: present\nsource: {source}\nhelper: {}\nagent: {}\n",
                located.helper.display(),
                located.agent.display()
            )
        }
        Err(error) => format!(
            "function hooks: missing\nexpected {} and {}\n{error}\n",
            names.helper, names.agent
        ),
    }
}

/// One row for `perfr doctor`, padded like the other checks.
pub fn doctor_row(found: Result<&Located, &str>) -> String {
    let (status, detail) = match found {
        Ok(located) if located.from_env => {
            ("ok", "PERFR_INJECT_HELPER and PERFR_AGENT".to_string())
        }
        Ok(located) => (
            "ok",
            located
                .helper
                .parent()
                .unwrap_or(Path::new("."))
                .display()
                .to_string(),
        ),
        Err(error) => (
            "missing",
            error
                .lines()
                .next()
                .unwrap_or("not beside perfr")
                .to_string(),
        ),
    };
    format!("{:<28} {:<12} {detail}\n", "function hooks", status)
}

pub fn doctor_line() -> String {
    let found = locate();
    doctor_row(found.as_ref().map_err(String::as_str))
}

pub fn command(name: &str) -> i32 {
    if name != "status" {
        eprintln!(
            "perfr hooks: `{name}` is not a command. Function hooks ship with perfr. Run `perfr hooks status`."
        );
        return 2;
    }
    let names = names_for(host_os());
    let found = locate();
    let text = status_text(found.as_ref().map_err(String::as_str), names);
    print!("{text}");
    if found.is_ok() {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn names_follow_the_platform() {
        assert_eq!(names_for("linux").helper, "perfr-inject-helper");
        assert_eq!(names_for("linux").agent, "libperfr_agent.so");
        assert_eq!(names_for("macos").agent, "libperfr_agent.dylib");
        assert_eq!(names_for("windows").helper, "perfr-inject-helper.exe");
        assert_eq!(names_for("windows").agent, "perfr_agent.dll");
    }

    #[test]
    fn locate_prefers_an_environment_override_and_reports_a_missing_pair() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("bin");
        fs::create_dir_all(&exe).unwrap();
        let names = names_for("linux");
        let missing = locate_in(Some(&exe), names, None, None).unwrap_err();
        assert!(missing.contains("perfr-inject-helper"), "{missing}");
        assert!(missing.contains("libperfr_agent.so"), "{missing}");
        let row = doctor_row(Err(missing.as_str()));
        assert!(row.contains("function hooks"), "{row}");
        assert!(row.contains("missing"), "{row}");
        assert!(!row.contains("hooks install"), "{row}");

        fs::write(exe.join(names.helper), b"helper").unwrap();
        fs::write(exe.join(names.agent), b"agent").unwrap();
        let found = locate_in(Some(&exe), names, None, None).unwrap();
        assert!(!found.from_env);
        assert_eq!(found.helper, exe.join(names.helper));
        let text = status_text(Ok(&found), names);
        assert!(text.contains("function hooks: present"), "{text}");
        assert!(text.contains("beside perfr"), "{text}");

        let env_dir = dir.path().join("env");
        fs::create_dir_all(&env_dir).unwrap();
        let env_helper = env_dir.join(names.helper);
        let env_agent = env_dir.join(names.agent);
        fs::write(&env_helper, b"env-helper").unwrap();
        fs::write(&env_agent, b"env-agent").unwrap();
        let from_env = locate_in(Some(&exe), names, Some(&env_helper), Some(&env_agent)).unwrap();
        assert!(from_env.from_env);
        assert_eq!(from_env.helper, env_helper);
        let bad = env_dir.join("missing-helper");
        let err = locate_in(Some(&exe), names, Some(&bad), Some(&env_agent)).unwrap_err();
        assert!(err.contains("PERFR_INJECT_HELPER"), "{err}");
        assert!(!err.contains("beside perfr"), "{err}");
    }
}
