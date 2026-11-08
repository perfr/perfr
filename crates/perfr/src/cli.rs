// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Command line shape: grouped subcommands, styled help, and the legacy flags.
//!
//! `perfr`, `perfr --serve`, `perfr --pid`, and `perfr --slice` keep the
//! behavior they had before subcommands. `perfr serve`, `capture`, and
//! `slice` are the same work with help text.

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::preflight::SetupOpts;
use crate::skill::{SkillAction, SkillEnv};
use crate::update::UpdateOpts;

const AFTER_HELP: &str = "\
Examples:
  perfr
  perfr serve --host 127.0.0.1
  perfr capture --pid 1234 --duration-ms 5000 --out capture.pod
  perfr slice in.perfr.zip out.perfr.zip 0 1000000000
  perfr mcp
  perfr skill install
  perfr skill install --dry-run
  perfr doctor
  perfr setup
  perfr update
  perfr update --check

`perfr --serve`, `perfr --pid`, and `perfr --slice` still work.
perfr runs on Linux, macOS, and Windows. Sampling and the scheduler need
Linux, or an elevated Windows prompt. macOS records manual scopes and Frida hooks.";

const SUBCOMMANDS: &[&str] = &[
    "serve", "capture", "slice", "mcp", "skill", "update", "doctor", "setup", "version",
];

#[derive(Clone, Debug)]
pub struct ServeOpts {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub wire: Option<String>,
    pub no_preflight: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureOpts {
    pub pid: Option<i32>,
    pub duration_ms: u64,
    pub freq_hz: u64,
    pub out: Option<String>,
    pub out_arrow: Option<String>,
    pub gpu_helper: Option<String>,
    pub no_preflight: bool,
}

#[derive(Clone, Debug)]
pub struct SliceOpts {
    pub input: String,
    pub output: String,
    pub t0: u64,
    pub t1: u64,
}

#[derive(Debug)]
pub enum Front {
    Exit(i32),
    Legacy,
    Serve(ServeOpts),
    Capture(CaptureOpts),
    Slice(SliceOpts),
    Mcp,
}

pub fn command() -> Command {
    Command::new("perfr")
        .version(version_text())
        .about("Profile a running process, from the terminal or from an agent")
        .long_about(
            "Profile a running process and open the live viewer, or let an agent drive \
             the same session over the Model Context Protocol.\n\n\
             With no command, perfr serves the viewer at http://127.0.0.1:44766/.",
        )
        .after_help(AFTER_HELP)
        .styles(styles())
        .subcommand_required(false)
        .arg_required_else_help(false)
        .disable_help_subcommand(true)
        .arg(no_preflight())
        .subcommand(serve_cmd())
        .subcommand(capture_cmd())
        .subcommand(slice_cmd())
        .subcommand(mcp_cmd())
        .subcommand(skill_cmd())
        .subcommand(doctor_cmd())
        .subcommand(setup_cmd())
        .subcommand(update_cmd())
        .subcommand(version_cmd())
}

/// Top-level help. Clap 4 styles one subcommand heading, so the groups are
/// written here with the same [`Styles`] the subcommand pages use.
pub fn render_top_help(color: bool) -> String {
    let styles = if color { styles() } else { Styles::plain() };
    let header = styles.get_header();
    let literal = styles.get_literal();
    let mut out = String::new();
    out.push_str(
        "Profile a running process and open the live viewer, or let an agent drive \
         the same session over the Model Context Protocol.\n\n\
         With no command, perfr serves the viewer at http://127.0.0.1:44766/.\n\n",
    );
    out.push_str(&format!(
        "{header}Usage:{header:#} perfr [OPTIONS] [COMMAND]\n\n"
    ));
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "Recording",
            &[
                ("serve", "Serve the live viewer"),
                ("capture", "Write a capture file for one process"),
                ("slice", "Cut a saved capture to a time window"),
            ],
        ),
        (
            "Agents",
            &[
                ("mcp", "Run the Model Context Protocol server"),
                ("skill", "Install the perfr skills for coding agents"),
            ],
        ),
        (
            "Maintenance",
            &[
                ("doctor", "Print sampling and instrumentation checks"),
                (
                    "setup",
                    "Apply permission fixes for sampling and instrumentation",
                ),
                (
                    "update",
                    "Replace this install with the latest GitHub release",
                ),
                ("version", "Print the version, git hash, build date, and release tag"),
            ],
        ),
    ];
    for (index, (title, commands)) in groups.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{header}{title}:{header:#}\n"));
        for (name, about) in *commands {
            out.push_str(&format!("  {literal}{name:<8}{literal:#}  {about}\n"));
        }
    }
    out.push_str(&format!("\n{header}Options:{header:#}\n"));
    out.push_str("      --no-preflight  Skip the permission check on startup\n");
    out.push_str("  -h, --help          Print help\n");
    out.push_str("  -V, --version       Print version\n\n");
    out.push_str(&format!("{header}Examples:{header:#}\n"));
    out.push_str(AFTER_HELP.strip_prefix("Examples:\n").unwrap_or(AFTER_HELP));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn front(args: &[String]) -> Front {
    if args.is_empty() {
        return Front::Legacy;
    }
    let head = args[0].as_str();
    let help = args.iter().any(|arg| arg == "-h" || arg == "--help");
    let known = SUBCOMMANDS.contains(&head);
    if !known && help {
        print!("{}", render_top_help(stdout_is_color()));
        return Front::Exit(0);
    }
    if !known && (head == "-V" || head == "--version") {
        println!("{}", crate::build_info::version_line());
        return Front::Exit(0);
    }
    if !known {
        if !args.is_empty() && args.iter().all(|arg| arg == "--no-preflight") {
            return Front::Serve(ServeOpts {
                host: None,
                port: None,
                wire: None,
                no_preflight: true,
            });
        }
        return Front::Legacy;
    }
    clap_front(args)
}

fn clap_front(args: &[String]) -> Front {
    let matches = match command()
        .try_get_matches_from(std::iter::once("perfr".to_string()).chain(args.iter().cloned()))
    {
        Ok(matches) => matches,
        Err(error) => {
            let code = error.exit_code();
            error.print().ok();
            return Front::Exit(code);
        }
    };
    match matches.subcommand() {
        Some(("serve", sub)) => Front::Serve(serve_opts(sub)),
        Some(("capture", sub)) => Front::Capture(capture_opts(sub)),
        Some(("slice", sub)) => Front::Slice(slice_opts(sub)),
        Some(("mcp", _)) => Front::Mcp,
        Some(("skill", sub)) => Front::Exit(run_skill(sub)),
        Some(("update", sub)) => Front::Exit(crate::update::run(update_opts(sub))),
        Some(("doctor", _)) => Front::Exit(crate::preflight::doctor()),
        Some(("setup", sub)) => Front::Exit(crate::preflight::setup(setup_opts(sub))),
        Some(("version", _)) => {
            println!("{}", crate::build_info::version_line());
            Front::Exit(0)
        }
        _ => Front::Legacy,
    }
}

fn serve_opts(matches: &ArgMatches) -> ServeOpts {
    ServeOpts {
        host: matches.get_one::<String>("host").cloned(),
        port: matches.get_one::<u16>("port").copied(),
        wire: matches.get_one::<String>("wire").cloned(),
        no_preflight: matches.get_flag("no-preflight"),
    }
}

fn capture_opts(matches: &ArgMatches) -> CaptureOpts {
    CaptureOpts {
        pid: matches.get_one::<i32>("pid").copied(),
        duration_ms: matches
            .get_one::<u64>("duration-ms")
            .copied()
            .unwrap_or(500),
        freq_hz: matches.get_one::<u64>("freq-hz").copied().unwrap_or(1000),
        out: matches.get_one::<String>("out").cloned(),
        out_arrow: matches.get_one::<String>("out-arrow").cloned(),
        gpu_helper: matches.get_one::<String>("gpu-helper").cloned(),
        no_preflight: matches.get_flag("no-preflight"),
    }
}

fn slice_opts(matches: &ArgMatches) -> SliceOpts {
    SliceOpts {
        input: matches
            .get_one::<String>("input")
            .cloned()
            .unwrap_or_default(),
        output: matches
            .get_one::<String>("output")
            .cloned()
            .unwrap_or_default(),
        t0: matches.get_one::<u64>("t0").copied().unwrap_or(0),
        t1: matches.get_one::<u64>("t1").copied().unwrap_or(u64::MAX),
    }
}

fn update_opts(matches: &ArgMatches) -> UpdateOpts {
    UpdateOpts {
        check: matches.get_flag("check"),
        tag: matches.get_one::<String>("tag").cloned(),
        yes: matches.get_flag("yes"),
    }
}

fn setup_opts(matches: &ArgMatches) -> SetupOpts {
    SetupOpts {
        check_only: matches.get_flag("check"),
        yes: matches.get_flag("yes"),
        persist: matches.get_flag("persist"),
    }
}

fn run_skill(matches: &ArgMatches) -> i32 {
    let Some((name, sub)) = matches.subcommand() else {
        eprintln!("perfr skill: expected install, list, or uninstall");
        return 2;
    };
    let action = SkillAction {
        target: sub
            .get_one::<String>("target")
            .cloned()
            .unwrap_or_else(|| "all".into()),
        dry_run: sub.get_flag("dry-run"),
        force: sub.get_flag("force"),
        project: sub.get_flag("project"),
        mcp: !sub.get_flag("no-mcp"),
    };
    let env = SkillEnv::from_process();
    let result = match name {
        "install" => crate::skill::install(&env, &action),
        "uninstall" => crate::skill::uninstall(&env, &action),
        "list" => Ok(crate::skill::list(&env, &action)),
        other => Err(format!("unknown skill command {other}")),
    };
    match result {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(error) => {
            eprintln!("perfr skill: {error}");
            2
        }
    }
}

fn stdout_is_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn version_text() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(crate::build_info::long_version).as_str()
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .literal(AnsiColor::Green.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default() | Effects::BOLD)
        .valid(AnsiColor::Green.on_default() | Effects::BOLD)
        .invalid(AnsiColor::Yellow.on_default() | Effects::BOLD)
}

fn no_preflight() -> Arg {
    Arg::new("no-preflight")
        .long("no-preflight")
        .global(true)
        .action(ArgAction::SetTrue)
        .help("Skip the permission check on startup")
}

fn serve_cmd() -> Command {
    Command::new("serve")
        .about("Serve the live viewer")
        .after_help(
            "With no --host, this binds 127.0.0.1, same as running perfr with no arguments.\n\
             `perfr --serve` still binds 0.0.0.0 when --host is omitted.\n\n\
             Examples:\n  \
             perfr\n  \
             perfr serve --host 127.0.0.1 --port 44766",
        )
        .arg(
            Arg::new("host")
                .long("host")
                .value_name("ADDR")
                .help("Bind address [default: 127.0.0.1]"),
        )
        .arg(
            Arg::new("port")
                .long("port")
                .value_name("PORT")
                .value_parser(clap::value_parser!(u16))
                .help("Listen port [default: 44766]"),
        )
        .arg(
            Arg::new("wire")
                .long("wire")
                .value_name("FORMAT")
                .help("WebSocket batches: raw, packed, or deflate"),
        )
}

fn capture_cmd() -> Command {
    Command::new("capture")
        .about("Write a capture file for one process")
        .after_help(
            "On macOS and Windows the output path ends in .perfr.zip.\n\
             --freq-hz is fixed at 1000 on Windows and is unused on macOS.\n\n\
             Examples:\n  \
             perfr capture --pid 1234 --duration-ms 5000 --out capture.pod",
        )
        .arg(
            Arg::new("pid")
                .long("pid")
                .value_name("PID")
                .value_parser(clap::value_parser!(i32))
                .help("Thread or process to sample"),
        )
        .arg(
            Arg::new("duration-ms")
                .long("duration-ms")
                .value_name("MS")
                .value_parser(clap::value_parser!(u64))
                .default_value("500")
                .help("How long to record"),
        )
        .arg(
            Arg::new("freq-hz")
                .long("freq-hz")
                .value_name("HZ")
                .value_parser(clap::value_parser!(u64))
                .default_value("1000")
                .help("Sample rate"),
        )
        .arg(
            Arg::new("out")
                .long("out")
                .value_name("PATH")
                .help("Capture file to write"),
        )
        .arg(
            Arg::new("out-arrow")
                .long("out-arrow")
                .value_name("DIR")
                .help("Also write an Arrow dataset"),
        )
        .arg(
            Arg::new("gpu-helper")
                .long("gpu-helper")
                .value_name("PATH")
                .help("Helper that streams GPU telemetry"),
        )
}

fn slice_cmd() -> Command {
    Command::new("slice")
        .about("Cut a saved capture to a time window")
        .after_help("Example:\n  perfr slice in.perfr.zip out.perfr.zip 0 1000000000")
        .arg(
            Arg::new("input")
                .value_name("IN")
                .required(true)
                .help("Source .perfr.zip"),
        )
        .arg(
            Arg::new("output")
                .value_name("OUT")
                .required(true)
                .help("Destination .perfr.zip"),
        )
        .arg(
            Arg::new("t0")
                .value_name("T0_NS")
                .required(true)
                .value_parser(clap::value_parser!(u64))
                .help("Window start, nanoseconds"),
        )
        .arg(
            Arg::new("t1")
                .value_name("T1_NS")
                .required(true)
                .value_parser(clap::value_parser!(u64))
                .help("Window end, nanoseconds"),
        )
}

fn mcp_cmd() -> Command {
    Command::new("mcp")
        .about("Run the Model Context Protocol server")
        .after_help(
            "The service has to be running already (`perfr`). JSON-RPC uses stdin.\n\
             --http stays on loopback: the profiling API has no authentication.\n\
             Warnings from the permission check go to stderr, never stdout.\n\n\
             Examples:\n  \
             perfr mcp\n  \
             perfr mcp --http 127.0.0.1:44767 --url http://127.0.0.1:44766",
        )
        .arg(
            Arg::new("url")
                .long("url")
                .value_name("URL")
                .help("perfr to drive [default: http://127.0.0.1:44766]"),
        )
        .arg(
            Arg::new("http")
                .long("http")
                .value_name("ADDR")
                .num_args(0..=1)
                .default_missing_value("127.0.0.1:44767")
                .help("Serve streamable HTTP on a loopback address"),
        )
        .arg(
            Arg::new("allow-remote")
                .long("allow-remote")
                .action(ArgAction::SetTrue)
                .help("Allow a non-loopback --url"),
        )
}

fn skill_cmd() -> Command {
    Command::new("skill")
        .about("Install the perfr skills for coding agents")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(skill_verb(
            "install",
            "Install the skills and register the MCP server",
        ))
        .subcommand(skill_verb("list", "Show skill paths and what is installed"))
        .subcommand(skill_verb(
            "uninstall",
            "Remove the skills and the MCP entry",
        ))
}

fn skill_verb(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .after_help(
            "Targets: claude, codex, grok, opencode, or all.\n\
             --target all skips agents that are not installed.\n\n\
             Examples:\n  \
             perfr skill install --dry-run\n  \
             perfr skill install --target claude --project",
        )
        .arg(
            Arg::new("target")
                .long("target")
                .value_name("NAME")
                .default_value("all")
                .help("Agent to update: claude, codex, grok, opencode, or all"),
        )
        .arg(
            Arg::new("project")
                .long("project")
                .action(ArgAction::SetTrue)
                .help("Write repo-local skill directories instead of the user directory"),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue)
                .help("Print the paths and do not write"),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .help("Replace a skill or MCP entry that differs"),
        )
        .arg(
            Arg::new("no-mcp")
                .long("no-mcp")
                .action(ArgAction::SetTrue)
                .help("Do not register the MCP server"),
        )
}

fn doctor_cmd() -> Command {
    Command::new("doctor")
        .about("Print sampling and instrumentation checks")
        .after_help("Example:\n  perfr doctor\n  perfr setup --check")
}

fn setup_cmd() -> Command {
    Command::new("setup")
        .about("Apply permission fixes for sampling and instrumentation")
        .after_help(
            "Asks before each change when stdin is a terminal.\n\
             --yes applies the runtime sysctls and setcap. --persist also writes\n\
             /etc/sysctl.d/99-perfr-perf.conf. CAP_SYS_ADMIN, when offered, is root\n\
             in all but name.\n\n\
             Examples:\n  \
             perfr setup\n  \
             perfr setup --check\n  \
             perfr setup --yes --persist",
        )
        .arg(
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue)
                .help("Print the checks and do not change anything"),
        )
        .arg(
            Arg::new("yes")
                .long("yes")
                .action(ArgAction::SetTrue)
                .help("Apply fixes without prompting"),
        )
        .arg(
            Arg::new("persist")
                .long("persist")
                .action(ArgAction::SetTrue)
                .help("Write sysctls to /etc/sysctl.d/99-perfr-perf.conf"),
        )
}

fn update_cmd() -> Command {
    Command::new("update")
        .about("Replace this install with the latest GitHub release")
        .after_help(
            "Downloads a perfr-v* release of perfr/perfr, checks its SHA-256, and\n\
             replaces this install. Linux and macOS write perfr.h to the sibling\n\
             include directory (~/.local/include). Windows writes it to\n\
             <install>\\include. Retired files from older installs are deleted.\n\
             A cargo, Homebrew, or pip install is not replaced; the package command\n\
             is printed instead. --yes is accepted and ignored.\n\n\
             --check prints current, latest, and status, and does not download the\n\
             archive. It exits 0 when this install is current or newer than the\n\
             release, and 1 when an update is available. Other failures exit 2.\n\n\
             Examples:\n  \
             perfr update\n  \
             perfr update --check\n  \
             perfr update --version perfr-v0.2.0",
        )
        .arg(
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue)
                .help("Print current, latest, and status; do not replace the install"),
        )
        .arg(
            Arg::new("tag")
                .long("version")
                .value_name("TAG")
                .help("Install this tag instead of the latest release"),
        )
        .arg(
            Arg::new("yes")
                .long("yes")
                .action(ArgAction::SetTrue)
                .hide(true)
                .help("Deprecated no-op; perfr update already replaces the install"),
        )
}

fn version_cmd() -> Command {
    Command::new("version").about("Print the version, git hash, build date, and release tag")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ColorChoice;

    fn render(args: &[&str]) -> String {
        let cmd = command().color(ColorChoice::Never);
        match cmd.try_get_matches_from(std::iter::once("perfr").chain(args.iter().copied())) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("expected help or an error for {args:?}"),
        }
    }

    fn index(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in:\n{text}"))
    }

    #[test]
    fn top_help_is_grouped_and_does_not_lead_with_linux() {
        let help = render_top_help(false);
        let colored = render_top_help(true);
        assert!(colored.contains("\x1b["), "{colored}");
        assert!(!help.contains("\x1b["), "{help}");
        let first = help.lines().find(|line| !line.trim().is_empty()).unwrap();
        assert!(first.starts_with("Profile a running process"), "{first}");
        assert!(!first.to_lowercase().starts_with("linux"), "{first}");
        assert!(help.contains("Examples:"), "{help}");
        assert!(help.contains("perfr skill install --dry-run"), "{help}");
        assert!(help.contains("perfr update --check"), "{help}");
        assert!(help.contains("--version"), "{help}");
        assert!(!help.contains("multi-machine"), "{help}");
        assert!(!help.contains("annotation"), "{help}");
        assert!(!help.contains("capture sharing"), "{help}");
        for heading in ["Recording", "Agents", "Maintenance"] {
            assert!(help.contains(heading), "missing {heading}:\n{help}");
        }
        let order = [
            "serve", "capture", "slice", "mcp", "skill", "doctor", "setup", "update", "version",
        ];
        let mut at = 0;
        for name in order {
            let found = index(&help, name);
            assert!(found >= at, "{name} out of order in:\n{help}");
            at = found;
        }
    }

    #[test]
    fn subcommand_help_lists_the_flags() {
        let skill = render(&["skill", "install", "--help"]);
        assert!(skill.contains("--target"), "{skill}");
        assert!(skill.contains("--project"), "{skill}");
        assert!(skill.contains("--dry-run"), "{skill}");
        assert!(skill.contains("--force"), "{skill}");
        let update = render(&["update", "--help"]);
        assert!(update.contains("--check"), "{update}");
        assert!(update.contains("--version"), "{update}");
        assert!(update.contains("perfr update\n"), "{update}");
        assert!(update.contains("exits 0"), "{update}");
        assert!(update.contains("1 when an update is available"), "{update}");
        assert!(update.contains("--yes is accepted and ignored"), "{update}");
        assert!(
            !update.contains("Replace the install without prompting"),
            "{update}"
        );
        let parsed = command()
            .try_get_matches_from(["perfr", "update", "--yes", "--check"])
            .unwrap();
        let (_, sub) = parsed.subcommand().unwrap();
        let opts = update_opts(sub);
        assert!(opts.yes);
        assert!(opts.check);
        let mcp = render(&["mcp", "--help"]);
        assert!(mcp.contains("--url"), "{mcp}");
        assert!(mcp.contains("--http"), "{mcp}");
        assert!(mcp.contains("--allow-remote"), "{mcp}");
        assert!(mcp.contains("stderr"), "{mcp}");
        let setup = render(&["setup", "--help"]);
        assert!(setup.contains("--check"), "{setup}");
        assert!(setup.contains("--persist"), "{setup}");
    }

    #[test]
    fn legacy_flags_stay_out_of_the_subcommand_parser() {
        assert!(matches!(front(&[]), Front::Legacy));
        assert!(matches!(
            front(&["--pid".into(), "1".into()]),
            Front::Legacy
        ));
        match front(&[
            "serve".into(),
            "--port".into(),
            "9".into(),
            "--no-preflight".into(),
        ]) {
            Front::Serve(opts) => {
                assert_eq!(opts.port, Some(9));
                assert!(opts.no_preflight);
                assert!(opts.host.is_none());
            }
            other => panic!("expected serve, got {other:?}"),
        }
    }

    #[test]
    fn version_line_matches_the_viewer_stamp() {
        let line = crate::build_info::version_line();
        assert_eq!(
            line,
            perfr_build_info::format_version_line(
                perfr_build_info::package_version(),
                perfr_build_info::git_hash(),
                perfr_build_info::build_date(),
                perfr_build_info::release_tag(),
            )
        );
        assert!(line.starts_with("perfr "), "{line}");
        assert!(line.contains(perfr_build_info::git_hash()), "{line}");
        assert!(line.contains(perfr_build_info::build_date()), "{line}");
        if perfr_build_info::release_tag().is_empty() {
            assert!(line.contains(&format!("dev+{}", perfr_build_info::git_hash())), "{line}");
        } else {
            assert!(line.contains(perfr_build_info::package_version()), "{line}");
            assert!(line.contains(perfr_build_info::release_tag()), "{line}");
        }
    }
}
