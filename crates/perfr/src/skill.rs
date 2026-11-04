// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! `perfr skill install` writes the embedded skills and, where the agent
//! keeps a small config, registers the MCP server.
//!
//! Locations, from the current docs:
//!
//! - Claude Code skills: `~/.claude/skills/<name>/SKILL.md` and
//!   `.claude/skills/<name>/SKILL.md`
//!   <https://code.claude.com/docs/en/skills>
//! - Claude Code MCP: `~/.claude.json` and `.mcp.json`, key `mcpServers`
//!   <https://code.claude.com/docs/en/mcp>
//! - Codex skills: `$HOME/.agents/skills` (user) and `.agents/skills` (repo).
//!   `~/.codex/skills` is the legacy directory the loader still reads;
//!   new skills go in `.agents/skills`.
//!   <https://learn.chatgpt.com/docs/build-skills.md>
//! - Codex MCP: `~/.codex/config.toml` or `$CODEX_HOME/config.toml`, and
//!   `.codex/config.toml`, table `[mcp_servers.perfr]`
//!   <https://learn.chatgpt.com/docs/extend/mcp.md>
//! - Grok Build skills: `~/.grok/skills` and `.grok/skills`
//!   <https://docs.x.ai/build/features/skills-plugins-marketplaces>
//! - Grok Build MCP: `~/.grok/config.toml` (`$GROK_HOME`) and `.grok/config.toml`
//!   <https://docs.x.ai/build/features/mcp-servers>
//! - OpenCode skills: `~/.config/opencode/skills` and `.opencode/skills`
//!   <https://opencode.ai/docs/skills/>
//! - OpenCode MCP: `~/.config/opencode/opencode.json` and `opencode.json`,
//!   `mcp.servers` (v2) or `mcp.<name>` when the file already uses that shape
//!   <https://dev.opencode.ai/v2/docs/mcp-servers>

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

const SKILL: &str = include_str!("../skills/perfr/SKILL.md");
const SETUP_SKILL: &str = include_str!("../skills/perfr-setup/SKILL.md");

const MCP_BEGIN: &str = "# BEGIN perfr mcp";
const MCP_END: &str = "# END perfr mcp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    Grok,
    OpenCode,
}

impl Agent {
    pub fn all() -> [Agent; 4] {
        [Agent::Claude, Agent::Codex, Agent::Grok, Agent::OpenCode]
    }

    pub fn parse(name: &str) -> Option<Agent> {
        match name {
            "claude" | "claude-code" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            "grok" | "grok-cli" => Some(Agent::Grok),
            "opencode" => Some(Agent::OpenCode),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Grok => "grok",
            Agent::OpenCode => "opencode",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
}

#[derive(Clone, Debug)]
pub struct SkillEnv {
    pub home: PathBuf,
    pub project: PathBuf,
    pub commands: BTreeSet<String>,
    pub grok_home: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

impl SkillEnv {
    pub fn from_process() -> Self {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
        let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            home,
            project,
            commands: commands_on_path(),
            grok_home: std::env::var_os("GROK_HOME").map(PathBuf::from),
            codex_home: std::env::var_os("CODEX_HOME").map(PathBuf::from),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SkillAction {
    pub target: String,
    pub dry_run: bool,
    pub force: bool,
    pub project: bool,
    pub mcp: bool,
}

pub fn install(env: &SkillEnv, action: &SkillAction) -> Result<String, String> {
    act(env, action, Op::Install)
}

pub fn uninstall(env: &SkillEnv, action: &SkillAction) -> Result<String, String> {
    act(env, action, Op::Uninstall)
}

pub fn list(env: &SkillEnv, action: &SkillAction) -> String {
    let Ok(agents) = selected(action) else {
        return format!("unknown --target {}\n", action.target);
    };
    let mut lines = Vec::new();
    for agent in agents {
        let detected = agent_detected(agent, env, action.project);
        let scope = if action.project {
            Scope::Project
        } else {
            Scope::User
        };
        for skill in skill_paths(agent, env, scope) {
            let state = if skill.is_file() {
                "installed"
            } else {
                "absent"
            };
            lines.push(format!(
                "{:<10} {:<12} {:<10} {}",
                agent.name(),
                if detected { "detected" } else { "not found" },
                state,
                skill.display()
            ));
        }
    }
    if lines.is_empty() {
        return "no skill targets\n".into();
    }
    lines.join("\n") + "\n"
}

#[derive(Clone, Copy)]
enum Op {
    Install,
    Uninstall,
}

fn act(env: &SkillEnv, action: &SkillAction, op: Op) -> Result<String, String> {
    let agents = selected(action)?;
    let mut lines = Vec::new();
    let mut wrote = false;
    for agent in agents {
        let detected = agent_detected(agent, env, action.project);
        let explicit = action.target != "all";
        if !detected && !explicit {
            lines.push(format!("{}: not installed; skipped", agent.name()));
            continue;
        }
        if !detected && explicit {
            lines.push(format!(
                "{}: not detected; {} because --target {} was set",
                agent.name(),
                match op {
                    Op::Install => "installing",
                    Op::Uninstall => "uninstalling",
                },
                agent.name()
            ));
        }
        let scope = if action.project {
            Scope::Project
        } else {
            Scope::User
        };
        for path in skill_paths(agent, env, scope) {
            match op {
                Op::Install => lines.push(write_skill(path, action)?),
                Op::Uninstall => lines.push(remove_skill(path, action)),
            }
            wrote = true;
        }
        if action.mcp {
            match op {
                Op::Install => lines.push(register_mcp(agent, env, scope, action)?),
                Op::Uninstall => lines.push(unregister_mcp(agent, env, scope, action)?),
            }
        }
    }
    if !wrote && action.target == "all" {
        lines.push(
            "no detected agents. pass --target <claude|codex|grok|opencode> to install anyway."
                .into(),
        );
    }
    Ok(lines.join("\n") + "\n")
}

fn selected(action: &SkillAction) -> Result<Vec<Agent>, String> {
    if action.target == "all" {
        return Ok(Agent::all().to_vec());
    }
    Agent::parse(&action.target)
        .map(|agent| vec![agent])
        .ok_or_else(|| {
            format!(
                "unknown --target {}. use all, claude, codex, grok, or opencode",
                action.target
            )
        })
}

fn skill_paths(agent: Agent, env: &SkillEnv, scope: Scope) -> [PathBuf; 2] {
    let root = skill_root(agent, env, scope);
    [
        root.join("perfr").join("SKILL.md"),
        root.join("perfr-setup").join("SKILL.md"),
    ]
}

fn skill_root(agent: Agent, env: &SkillEnv, scope: Scope) -> PathBuf {
    match (agent, scope) {
        (Agent::Claude, Scope::User) => env.home.join(".claude").join("skills"),
        (Agent::Claude, Scope::Project) => env.project.join(".claude").join("skills"),
        (Agent::Codex, Scope::User) => env.home.join(".agents").join("skills"),
        (Agent::Codex, Scope::Project) => env.project.join(".agents").join("skills"),
        (Agent::Grok, Scope::User) => grok_home(env).join("skills"),
        (Agent::Grok, Scope::Project) => env.project.join(".grok").join("skills"),
        (Agent::OpenCode, Scope::User) => env.home.join(".config").join("opencode").join("skills"),
        (Agent::OpenCode, Scope::Project) => env.project.join(".opencode").join("skills"),
    }
}

fn body_for(path: &Path) -> &'static str {
    if path.ends_with(Path::new("perfr-setup").join("SKILL.md")) {
        SETUP_SKILL
    } else {
        SKILL
    }
}

fn write_skill(path: PathBuf, action: &SkillAction) -> Result<String, String> {
    let body = body_for(&path);
    let verb = if action.dry_run {
        "would write"
    } else {
        "wrote"
    };
    if path.is_file() {
        let current = fs::read_to_string(&path).unwrap_or_default();
        if current == body {
            return Ok(format!(
                "{}: up to date {}",
                agent_of_path(&path),
                path.display()
            ));
        }
        if !action.force {
            return Ok(format!(
                "{}: {} exists and differs; pass --force to replace it",
                agent_of_path(&path),
                path.display()
            ));
        }
    }
    if !action.dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        fs::write(&path, body)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(format!(
        "{}: {verb} {}",
        agent_of_path(&path),
        path.display()
    ))
}

fn remove_skill(path: PathBuf, action: &SkillAction) -> String {
    let verb = if action.dry_run {
        "would remove"
    } else {
        "removed"
    };
    if !path.is_file() {
        return format!("{}: absent {}", agent_of_path(&path), path.display());
    }
    if !action.dry_run {
        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    format!("{}: {verb} {}", agent_of_path(&path), path.display())
}

fn agent_of_path(path: &Path) -> &'static str {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.contains("/.claude/") {
        "claude"
    } else if text.contains("/.agents/") {
        "codex"
    } else if text.contains("/.grok/") {
        "grok"
    } else if text.contains("/opencode/") {
        "opencode"
    } else {
        "skill"
    }
}

fn grok_home(env: &SkillEnv) -> PathBuf {
    env.grok_home
        .clone()
        .unwrap_or_else(|| env.home.join(".grok"))
}

fn codex_home(env: &SkillEnv) -> PathBuf {
    env.codex_home
        .clone()
        .unwrap_or_else(|| env.home.join(".codex"))
}

pub fn agent_detected(agent: Agent, env: &SkillEnv, project_scope: bool) -> bool {
    let command = match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Grok => "grok",
        Agent::OpenCode => "opencode",
    };
    if env.commands.contains(command) {
        return true;
    }
    match agent {
        Agent::Claude => {
            env.home.join(".claude").is_dir()
                || (project_scope
                    && (env.project.join(".claude").is_dir()
                        || env.project.join(".mcp.json").is_file()))
        }
        Agent::Codex => {
            codex_home(env).is_dir() || (project_scope && env.project.join(".codex").is_dir())
        }
        Agent::Grok => {
            grok_home(env).is_dir() || (project_scope && env.project.join(".grok").is_dir())
        }
        Agent::OpenCode => {
            env.home.join(".config").join("opencode").is_dir()
                || env.home.join(".opencode").is_dir()
                || (project_scope
                    && (env.project.join(".opencode").is_dir()
                        || env.project.join("opencode.json").is_file()))
        }
    }
}

fn mcp_path(agent: Agent, env: &SkillEnv, scope: Scope) -> PathBuf {
    match (agent, scope) {
        (Agent::Claude, Scope::User) => env.home.join(".claude.json"),
        (Agent::Claude, Scope::Project) => env.project.join(".mcp.json"),
        (Agent::Codex, Scope::User) => codex_home(env).join("config.toml"),
        (Agent::Codex, Scope::Project) => env.project.join(".codex").join("config.toml"),
        (Agent::Grok, Scope::User) => grok_home(env).join("config.toml"),
        (Agent::Grok, Scope::Project) => env.project.join(".grok").join("config.toml"),
        (Agent::OpenCode, Scope::User) => env
            .home
            .join(".config")
            .join("opencode")
            .join("opencode.json"),
        (Agent::OpenCode, Scope::Project) => env.project.join("opencode.json"),
    }
}

fn register_mcp(
    agent: Agent,
    env: &SkillEnv,
    scope: Scope,
    action: &SkillAction,
) -> Result<String, String> {
    let path = mcp_path(agent, env, scope);
    match agent {
        Agent::Claude | Agent::OpenCode => {
            let outcome = merge_json(&path, agent, action)?;
            Ok(format!("{}: {outcome} {}", agent.name(), path.display()))
        }
        Agent::Codex | Agent::Grok => {
            let outcome = merge_toml(&path, action)?;
            Ok(format!("{}: {outcome} {}", agent.name(), path.display()))
        }
    }
}

fn unregister_mcp(
    agent: Agent,
    env: &SkillEnv,
    scope: Scope,
    action: &SkillAction,
) -> Result<String, String> {
    let path = mcp_path(agent, env, scope);
    if !path.is_file() {
        return Ok(format!(
            "{}: no MCP config at {}",
            agent.name(),
            path.display()
        ));
    }
    match agent {
        Agent::Claude | Agent::OpenCode => remove_json(&path, agent, action),
        Agent::Codex | Agent::Grok => Ok(remove_toml(&path, action)),
    }
}

fn mcp_stdio() -> Value {
    serde_json::json!({
        "command": "perfr",
        "args": ["mcp", "--url", "http://127.0.0.1:44766"]
    })
}

fn opencode_server() -> Value {
    serde_json::json!({
        "type": "local",
        "command": ["perfr", "mcp", "--url", "http://127.0.0.1:44766"],
        "enabled": true
    })
}

fn merge_json(path: &Path, agent: Agent, action: &SkillAction) -> Result<String, String> {
    let existing = if path.is_file() {
        match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Value::Object(Map::new()),
            Ok(text) => serde_json::from_str(&text).map_err(|_| {
                format!(
                    "could not parse {} as JSON; left it unchanged. Add the perfr server by hand",
                    path.display()
                )
            })?,
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        }
    } else {
        Value::Object(Map::new())
    };
    if !existing.is_object() {
        return Ok(format!(
            "could not parse {} as a JSON object; left it unchanged",
            path.display()
        ));
    }
    let mut root = existing;
    let desired = match agent {
        Agent::OpenCode => opencode_entry(&root),
        _ => mcp_stdio(),
    };
    let current = mcp_value(&root, agent);
    if current.as_ref() == Some(&desired) {
        return Ok("MCP already registered in".into());
    }
    if current.is_some() && !action.force {
        return Ok("MCP entry exists and differs; pass --force. left".into());
    }
    insert_mcp(&mut root, agent, desired);
    if !action.dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let text = serde_json::to_string_pretty(&root).map_err(|error| error.to_string())? + "\n";
        fs::write(path, text)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(if action.dry_run {
        "would register MCP in".to_string()
    } else {
        "registered MCP in".to_string()
    })
}

fn opencode_entry(root: &Value) -> Value {
    if root.get("mcp").and_then(|mcp| mcp.get("servers")).is_some() || root.get("mcp").is_none() {
        opencode_server()
    } else if mcp_looks_like_v1(root.get("mcp")) {
        opencode_server()
    } else {
        opencode_server()
    }
}

fn mcp_looks_like_v1(mcp: Option<&Value>) -> bool {
    let Some(mcp) = mcp.and_then(|value| value.as_object()) else {
        return false;
    };
    if mcp.contains_key("servers") {
        return false;
    }
    mcp.values()
        .any(|value| value.get("command").is_some() || value.get("type").is_some())
}

fn mcp_value(root: &Value, agent: Agent) -> Option<Value> {
    match agent {
        Agent::Claude => root.pointer("/mcpServers/perfr").cloned(),
        Agent::OpenCode => {
            if root.pointer("/mcp/servers").is_some() || !mcp_looks_like_v1(root.get("mcp")) {
                root.pointer("/mcp/servers/perfr").cloned()
            } else {
                root.pointer("/mcp/perfr").cloned()
            }
        }
        _ => None,
    }
}

fn insert_mcp(root: &mut Value, agent: Agent, server: Value) {
    let obj = root.as_object_mut().unwrap();
    match agent {
        Agent::Claude => {
            let servers = obj
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Map::new()));
            if !servers.is_object() {
                *servers = Value::Object(Map::new());
            }
            servers
                .as_object_mut()
                .unwrap()
                .insert("perfr".into(), server);
        }
        Agent::OpenCode => {
            let v1 = mcp_looks_like_v1(obj.get("mcp"));
            let mcp = obj
                .entry("mcp")
                .or_insert_with(|| Value::Object(Map::new()));
            if !mcp.is_object() {
                *mcp = Value::Object(Map::new());
            }
            if v1 {
                mcp.as_object_mut().unwrap().insert("perfr".into(), server);
            } else {
                let servers = mcp
                    .as_object_mut()
                    .unwrap()
                    .entry("servers")
                    .or_insert_with(|| Value::Object(Map::new()));
                if !servers.is_object() {
                    *servers = Value::Object(Map::new());
                }
                servers
                    .as_object_mut()
                    .unwrap()
                    .insert("perfr".into(), server);
            }
        }
        _ => {}
    }
}

fn remove_json(path: &Path, agent: Agent, action: &SkillAction) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut root: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return Ok(format!(
                "{}: could not parse {}; left it unchanged",
                agent.name(),
                path.display()
            ));
        }
    };
    let removed = take_perfr(&mut root, agent);
    if !removed {
        return Ok(format!(
            "{}: no perfr MCP entry in {}",
            agent.name(),
            path.display()
        ));
    }
    if !action.dry_run {
        let text = serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".into()) + "\n";
        fs::write(path, text).map_err(|error| error.to_string())?;
    }
    let verb = if action.dry_run {
        "would remove MCP from"
    } else {
        "removed MCP from"
    };
    Ok(format!("{}: {verb} {}", agent.name(), path.display()))
}

fn take_perfr(root: &mut Value, agent: Agent) -> bool {
    match agent {
        Agent::Claude => root
            .get_mut("mcpServers")
            .and_then(|servers| servers.as_object_mut())
            .and_then(|servers| servers.remove("perfr"))
            .is_some_and(command_is_perfr),
        Agent::OpenCode => {
            let v1 = mcp_looks_like_v1(root.get("mcp"));
            if v1 {
                return root
                    .get_mut("mcp")
                    .and_then(|mcp| mcp.as_object_mut())
                    .and_then(|mcp| mcp.remove("perfr"))
                    .is_some_and(command_is_perfr);
            }
            root.pointer_mut("/mcp/servers")
                .and_then(|servers| servers.as_object_mut())
                .and_then(|servers| servers.remove("perfr"))
                .is_some_and(command_is_perfr)
        }
        _ => false,
    }
}

fn command_is_perfr(value: Value) -> bool {
    let command = value.get("command");
    match command {
        Some(Value::String(text)) => text == "perfr",
        Some(Value::Array(items)) => items.first().and_then(|item| item.as_str()) == Some("perfr"),
        _ => false,
    }
}

fn toml_block() -> String {
    format!(
        "{MCP_BEGIN}\n[mcp_servers.perfr]\ncommand = \"perfr\"\nargs = [\"mcp\", \"--url\", \"http://127.0.0.1:44766\"]\n{MCP_END}\n"
    )
}

fn merge_toml(path: &Path, action: &SkillAction) -> Result<String, String> {
    let current = if path.is_file() {
        fs::read_to_string(path).map_err(|error| error.to_string())?
    } else {
        String::new()
    };
    if current.contains(MCP_BEGIN) || current.contains("[mcp_servers.perfr]") {
        if current.contains(MCP_BEGIN) && current.contains("command = \"perfr\"") {
            return Ok("MCP already registered in".into());
        }
        if current.contains("[mcp_servers.perfr]") && !action.force {
            return Ok("MCP entry exists; pass --force. left".into());
        }
    }
    if !action.dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut next = current;
        if next.contains(MCP_BEGIN) {
            next = remove_marked_block(&next);
        }
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        if !next.is_empty() {
            next.push('\n');
        }
        next.push_str(&toml_block());
        fs::write(path, next).map_err(|error| error.to_string())?;
    }
    Ok(if action.dry_run {
        "would register MCP in".to_string()
    } else {
        "registered MCP in".to_string()
    })
}

fn remove_toml(path: &Path, action: &SkillAction) -> String {
    let Ok(current) = fs::read_to_string(path) else {
        return format!("skill: cannot read {}", path.display());
    };
    if !current.contains(MCP_BEGIN) {
        return format!(
            "skill: no perfr MCP block in {} (left unmarked config alone)",
            path.display()
        );
    }
    if !action.dry_run {
        let _ = fs::write(path, remove_marked_block(&current));
    }
    let verb = if action.dry_run {
        "would remove MCP from"
    } else {
        "removed MCP from"
    };
    format!("skill: {verb} {}", path.display())
}

fn remove_marked_block(text: &str) -> String {
    let Some(start) = text.find(MCP_BEGIN) else {
        return text.to_string();
    };
    let Some(end_rel) = text[start..].find(MCP_END) else {
        return text.to_string();
    };
    let end = start + end_rel + MCP_END.len();
    let mut next = String::new();
    next.push_str(text[..start].trim_end_matches('\n'));
    let rest = text[end..].trim_start_matches('\n');
    if !next.is_empty() && !rest.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(rest);
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn commands_on_path() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Some(path) = std::env::var_os("PATH") else {
        return found;
    };
    for name in ["claude", "codex", "grok", "opencode"] {
        for dir in std::env::split_paths(&path) {
            if dir.join(name).is_file()
                || dir.join(format!("{name}.exe")).is_file()
                || dir.join(format!("{name}.cmd")).is_file()
            {
                found.insert(name.to_string());
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(home: &Path, project: &Path) -> SkillEnv {
        SkillEnv {
            home: home.to_path_buf(),
            project: project.to_path_buf(),
            commands: BTreeSet::new(),
            grok_home: None,
            codex_home: None,
        }
    }

    fn action(target: &str, project: bool, dry_run: bool, force: bool) -> SkillAction {
        SkillAction {
            target: target.into(),
            dry_run,
            force,
            project,
            mcp: true,
        }
    }

    #[test]
    fn embedded_skills_name_themselves_and_skip_unshipped_claims() {
        for body in [SKILL, SETUP_SKILL] {
            assert!(body.starts_with("---\n"), "{body}");
            assert!(body.contains("name: perfr"));
            assert!(!body.to_lowercase().starts_with("linux"));
            assert!(!body.contains("multi-machine"));
            assert!(!body.contains("annotation"));
        }
        assert!(SETUP_SKILL.contains("name: perfr-setup"));
        assert!(SETUP_SKILL.contains("perfr doctor"));
        assert!(SKILL.contains("perfr_capabilities"));
    }

    #[test]
    fn dry_run_skips_agents_that_are_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with(dir.path(), dir.path());
        let text = install(&env, &action("all", false, true, false)).unwrap();
        assert!(text.contains("claude: not installed; skipped"), "{text}");
        assert!(text.contains("codex: not installed; skipped"), "{text}");
        assert!(text.contains("grok: not installed; skipped"), "{text}");
        assert!(text.contains("opencode: not installed; skipped"), "{text}");
        assert!(text.contains("--target"), "{text}");
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn install_writes_both_skills_and_mcp_for_a_detected_agent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(&project).unwrap();
        let mut env = env_with(&home, &project);
        env.commands.insert("codex".into());
        let text = install(&env, &action("all", false, false, false)).unwrap();
        let claude_skill = home.join(".claude/skills/perfr/SKILL.md");
        let setup = home.join(".claude/skills/perfr-setup/SKILL.md");
        assert_eq!(fs::read_to_string(&claude_skill).unwrap(), SKILL);
        assert_eq!(fs::read_to_string(&setup).unwrap(), SETUP_SKILL);
        let claude_json: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(claude_json["mcpServers"]["perfr"]["command"], "perfr");
        let codex = home.join(".codex/config.toml");
        let toml = fs::read_to_string(&codex).unwrap();
        assert!(toml.contains("[mcp_servers.perfr]"), "{toml}");
        assert!(toml.contains(MCP_BEGIN), "{toml}");
        let codex_skill = home.join(".agents/skills/perfr/SKILL.md");
        assert!(codex_skill.is_file(), "{text}");
        assert!(text.contains("grok: not installed; skipped"), "{text}");

        let again = install(&env, &action("claude", false, false, false)).unwrap();
        assert!(again.contains("up to date"), "{again}");
        assert!(again.contains("MCP already registered"), "{again}");

        let list_text = list(&env, &action("claude", false, false, false));
        assert!(list_text.contains("installed"), "{list_text}");
        assert!(list_text.contains("perfr-setup"), "{list_text}");

        let removed = uninstall(&env, &action("claude", false, false, false)).unwrap();
        assert!(!claude_skill.exists(), "{removed}");
        assert!(!setup.exists(), "{removed}");
        let after: Value =
            serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert!(after["mcpServers"].get("perfr").is_none(), "{after}");
        let toml_after = fs::read_to_string(&codex).unwrap();
        assert!(
            toml_after.contains("[mcp_servers.perfr]"),
            "codex block stays until codex is uninstalled: {toml_after}"
        );
        let _ = uninstall(&env, &action("codex", false, false, false)).unwrap();
        let toml_gone = fs::read_to_string(&codex).unwrap();
        assert!(!toml_gone.contains(MCP_BEGIN), "{toml_gone}");
    }

    #[test]
    fn project_scope_uses_repo_local_directories() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("repo");
        fs::create_dir_all(project.join(".grok")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let env = env_with(&home, &project);
        let text = install(&env, &action("all", true, true, false)).unwrap();
        assert!(text.contains(".grok/skills/perfr/SKILL.md"), "{text}");
        assert!(text.contains(".grok/config.toml"), "{text}");
        assert!(text.contains("claude: not installed; skipped"), "{text}");
        assert!(!project.join(".grok/skills/perfr/SKILL.md").exists());
    }

    #[test]
    fn explicit_target_installs_when_the_agent_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_with(dir.path(), dir.path());
        let text = install(&env, &action("opencode", false, false, false)).unwrap();
        let skill = dir.path().join(".config/opencode/skills/perfr/SKILL.md");
        assert!(skill.is_file(), "{text}");
        let cfg: Value = serde_json::from_str(
            &fs::read_to_string(dir.path().join(".config/opencode/opencode.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["mcp"]["servers"]["perfr"]["command"][0], "perfr");
        assert!(text.contains("not detected"), "{text}");
    }

    #[test]
    fn opencode_v1_config_is_extended_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("opencode.json");
        fs::write(
            &path,
            r#"{"mcp":{"other":{"type":"local","command":["other"]}}}"#,
        )
        .unwrap();
        let mut env = env_with(dir.path(), dir.path());
        env.commands.insert("opencode".into());
        let _ = install(
            &env,
            &SkillAction {
                target: "opencode".into(),
                dry_run: false,
                force: false,
                project: true,
                mcp: true,
            },
        )
        .unwrap();
        let cfg: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg["mcp"]["other"]["command"][0], "other");
        assert_eq!(cfg["mcp"]["perfr"]["type"], "local");
        assert!(cfg["mcp"].get("servers").is_none(), "{cfg}");
    }

    #[test]
    fn a_different_skill_is_kept_until_force() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let path = dir.path().join(".claude/skills/perfr/SKILL.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\n").unwrap();
        let env = env_with(dir.path(), dir.path());
        let kept = install(&env, &action("claude", false, false, false)).unwrap();
        assert!(kept.contains("--force"), "{kept}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\n");
        let replaced = install(&env, &action("claude", false, false, true)).unwrap();
        assert!(replaced.contains("wrote"), "{replaced}");
        assert_eq!(fs::read_to_string(&path).unwrap(), SKILL);
    }
}
