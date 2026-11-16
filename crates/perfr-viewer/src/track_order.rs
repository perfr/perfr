// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A track order you can share as a text file.
//!
//! One pattern per line, matched against a process's name (`comm`), the
//! basename of its executable, and its full path; a pattern with a `/` in
//! it is tried against the path only. `*` matches any run of characters
//! and `?` one; matching ignores case. Blank lines and lines starting with
//! `#` are comments. Every process a line matches comes first on the rail,
//! in the order of the lines (the first matching line counts); everything
//! else keeps the viewer's own order behind them. The text lives in the
//! service's settings (`track_order`), so it holds across sessions, and it
//! loads from and saves to a plain file, which is what makes the layout of
//! a big trace something to hand to a colleague.

/// The parsed file: the patterns, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    patterns: Vec<String>,
}

impl Rules {
    pub fn parse(text: &str) -> Rules {
        Rules {
            patterns: text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| line.to_string())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// The position of the first line matching one of a process's names,
    /// or `None` when the file says nothing about it. `names` is whatever
    /// the process is known by: comm, exe basename, full path.
    pub fn rank<'a>(&self, names: impl IntoIterator<Item = &'a str> + Clone) -> Option<usize> {
        self.patterns.iter().position(|pattern| {
            let path_only = pattern.contains('/');
            names.clone().into_iter().any(|name| {
                if path_only && !name.contains('/') {
                    return false;
                }
                glob_match(pattern, name)
            })
        })
    }
}

/// `*` any run, `?` one character, case-insensitive; no character classes.
/// Iterative with backtracking to the last `*`, so a long name and a
/// pattern with several stars cost their product, not an exponential.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_chars(&lowered(pattern), &lowered(text))
}

/// A name as [`glob_chars`] takes it: its characters, lower-cased.
fn lowered(text: &str) -> Vec<char> {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// [`glob_match`] on names already lower-cased, for matching many names
/// against many patterns without preparing each pair again.
fn glob_chars(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ------------------------------------------------- the order of a rail ---
//
// The file above orders processes. What follows orders the tracks too: the
// rail as it stands, top to bottom, as text.
//
// ```text
// # a comment
// [MyGame*]                 a process, by name
// main                      a thread of it, by name
// Worker (*)                every thread the pattern matches, in name order
// main > frame time         a graph of a thread
// main > [async]            a thread's async block
// [Node Health]
// CPU
// ```
//
// A `[process]` line starts the tracks of the processes it matches, by the
// names the file above matches by; lines before the first one are tracks of
// the capture's target. Processes come in the order of their sections, and
// tracks in the order of their lines, ahead of everything the text does not
// name, which keeps the viewer's own order behind them. The first line that
// matches a track counts. Tracks one line matches are in name order, numbers
// counted as numbers (`Worker (2)` before `Worker (10)`); those of the same
// name keep the viewer's order among themselves. A thread's graphs and its
// async block follow it unless a line of their own places them.
//
// Names are what holds from one run of a program to the next, which is what
// a remembered order is for. A capture carries the order it was last seen
// in, and there the tracks are exactly the ones in the file, so that text
// names them by number: `[@<pid>]`, `@<tid>`, and `@<pid>:<tid>` for a track
// drawn under a process that is not its own.

use crate::tracks::{FastMap, TrackKey};

/// What a line picks a process or a thread by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// A name pattern (see [`glob_match`]).
    Name(String),
    /// `@<number>`: the process or thread with exactly that id.
    Id(u32),
    /// The capture's target: the lines before any `[process]`.
    Target,
}

/// Which track of a thread a line is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Thread,
    Async,
    /// One of its graphs, by name pattern.
    Graph(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackLine {
    /// The pid of `@<pid>:<tid>`: the track's own process, when it is
    /// drawn under another one's section.
    pub owner_pid: Option<u32>,
    pub thread: Pick,
    pub part: Part,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub process: Pick,
    pub tracks: Vec<TrackLine>,
}

/// A parsed rail order: the sections, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arrangement {
    pub sections: Vec<Section>,
}

/// How a thread's async block is written after the ` > `.
const ASYNC_PART: &str = "[async]";
/// What stands between a thread and the part of it a line is about.
const PART_SEPARATOR: &str = " > ";

/// `@12` and `@3:12` as `(None, 12)` and `(Some(3), 12)`.
fn exact_id(text: &str) -> Option<(Option<u32>, u32)> {
    let ids = text.strip_prefix('@')?;
    match ids.split_once(':') {
        Some((pid, tid)) => Some((Some(pid.parse().ok()?), tid.parse().ok()?)),
        None => Some((None, ids.parse().ok()?)),
    }
}

fn parse_track(line: &str) -> TrackLine {
    // A numbered thread has no ` > ` in it, so what follows the first one
    // is the graph's name whatever it holds; a thread's name can hold
    // one, and there the last one is the separator.
    let split = if line.starts_with('@') { line.split_once(PART_SEPARATOR) } else { line.rsplit_once(PART_SEPARATOR) };
    let (thread, part) = match split {
        Some((thread, part)) if !thread.trim().is_empty() && !part.trim().is_empty() => (thread.trim(), Some(part.trim())),
        _ => (line, None),
    };
    let (owner_pid, thread) = match exact_id(thread) {
        Some((owner, tid)) => (owner, Pick::Id(tid)),
        None => (None, Pick::Name(thread.to_string())),
    };
    let part = match part {
        None => Part::Thread,
        Some(ASYNC_PART) => Part::Async,
        Some(graph) => Part::Graph(graph.to_string()),
    };
    TrackLine { owner_pid, thread, part }
}

/// A process of the rail, as an arrangement is matched against it.
pub struct RailProcess {
    pub pid: u32,
    /// Whatever it is known by: its name on the rail, its executable's
    /// name, its path.
    pub names: Vec<String>,
    pub target: bool,
}

/// A track of the rail: its key, the name of its thread (empty when the
/// thread has none of its own) and, for a graph, the graph's.
#[derive(Clone, Debug)]
pub struct RailTrack {
    pub key: TrackKey,
    pub thread: String,
    pub graph: String,
    /// For a graph or an async block: whether it sits right under its
    /// thread, where it goes by itself. Only what is written reads this.
    pub follows: bool,
}

/// What an arrangement asks of a rail.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolved {
    /// The place of each process the text names: its section's.
    pub processes: FastMap<u32, usize>,
    /// The place of each track a line matches. Lower is higher up; two
    /// tracks with the same place are of one name on one line.
    pub tracks: FastMap<TrackKey, u64>,
    /// The process a track is drawn under when it is not its own.
    pub hosts: FastMap<TrackKey, u32>,
}

impl Arrangement {
    pub fn parse(text: &str) -> Arrangement {
        let mut sections: Vec<Section> = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(inner) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                let inner = inner.trim();
                let process = match exact_id(inner) {
                    Some((None, pid)) => Pick::Id(pid),
                    _ => Pick::Name(inner.to_string()),
                };
                sections.push(Section { process, tracks: Vec::new() });
                continue;
            }
            if sections.is_empty() {
                sections.push(Section { process: Pick::Target, tracks: Vec::new() });
            }
            if let Some(section) = sections.last_mut() {
                section.tracks.push(parse_track(line));
            }
        }
        Arrangement { sections }
    }

    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Places the processes and tracks of a rail.
    pub fn resolve(&self, processes: &[RailProcess], tracks: &[RailTrack]) -> Resolved {
        let mut out = Resolved::default();
        // A process belongs to the first section that picks it.
        let mut section_pids: Vec<Vec<u32>> = vec![Vec::new(); self.sections.len()];
        for process in processes {
            let names = || process.names.iter().map(String::as_str).filter(|n| !n.is_empty());
            let at = self.sections.iter().position(|section| match &section.process {
                Pick::Id(pid) => *pid == process.pid,
                Pick::Target => process.target,
                Pick::Name(pattern) => Rules { patterns: vec![pattern.clone()] }.rank(names()).is_some(),
            });
            if let Some(at) = at {
                out.processes.insert(process.pid, at);
                section_pids[at].push(process.pid);
            }
        }
        let names: Vec<(Vec<char>, Vec<char>)> = tracks.iter().map(|t| (lowered(&t.thread), lowered(&t.graph))).collect();
        let mut line_no = 0u64;
        for (section, pids) in self.sections.iter().zip(&section_pids) {
            let host = match section.process {
                Pick::Id(pid) => Some(pid),
                _ => None,
            };
            for line in &section.tracks {
                line_no += 1;
                let thread_pattern = match &line.thread {
                    Pick::Name(pattern) => lowered(pattern),
                    _ => Vec::new(),
                };
                let graph_pattern = match &line.part {
                    Part::Graph(pattern) => lowered(pattern),
                    _ => Vec::new(),
                };
                let mut matched: Vec<usize> = Vec::new();
                for (n, track) in tracks.iter().enumerate() {
                    let part = match (&line.part, track.key) {
                        (Part::Thread, TrackKey::Thread(_)) | (Part::Async, TrackKey::Async(_)) => true,
                        (Part::Graph(_), TrackKey::Value(_)) => glob_chars(&graph_pattern, &names[n].1),
                        _ => false,
                    };
                    if !part || out.tracks.contains_key(&track.key) {
                        continue;
                    }
                    let owner = track.key.owner();
                    let thread = match (&line.thread, line.owner_pid) {
                        (Pick::Id(tid), Some(pid)) => owner.pid == pid && owner.tid == *tid,
                        (Pick::Id(tid), None) => owner.tid == *tid && pids.contains(&owner.pid),
                        (Pick::Name(_), _) => {
                            pids.contains(&owner.pid) && !track.thread.is_empty() && glob_chars(&thread_pattern, &names[n].0)
                        }
                        (Pick::Target, _) => false,
                    };
                    if thread {
                        matched.push(n);
                    }
                }
                // Within the line: by name, numbers as numbers.
                let by_name = |a: &usize, b: &usize| {
                    natural_cmp(&tracks[*a].thread, &tracks[*b].thread).then_with(|| natural_cmp(&tracks[*a].graph, &tracks[*b].graph))
                };
                let mut distinct = matched.clone();
                distinct.sort_by(by_name);
                distinct.dedup_by(|a, b| by_name(a, b).is_eq());
                for n in matched {
                    let place = distinct.binary_search_by(|other| by_name(other, &n)).unwrap_or(0) as u64;
                    let key = tracks[n].key;
                    out.tracks.insert(key, (line_no << 32) | place);
                    if let (Some(host), Some(_)) = (host, line.owner_pid) {
                        if host != key.owner_pid() {
                            out.hosts.insert(key, host);
                        }
                    }
                }
            }
        }
        out
    }
}

/// Orders names the way a reader counts them: digits as numbers, so that
/// `Worker (2)` is before `Worker (10)`; case does not matter.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn number(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
        let mut digits = String::new();
        while let Some(c) = chars.peek().copied().filter(char::is_ascii_digit) {
            digits.push(c);
            chars.next();
        }
        let trimmed = digits.trim_start_matches('0');
        trimmed.to_string()
    }
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (n, m) = (number(&mut x), number(&mut y));
                match n.len().cmp(&m.len()).then_with(|| n.cmp(&m)) {
                    Ordering::Equal => {}
                    other => return other,
                }
            }
            (Some(c), Some(d)) => {
                let lower = |c: char| c.to_lowercase().next().unwrap_or(c);
                match lower(c).cmp(&lower(d)) {
                    Ordering::Equal => {
                        x.next();
                        y.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// One process of a rail as it stands, with its tracks top to bottom: what
/// an order is written from.
pub struct RailSection {
    pub pid: u32,
    /// The name the process is remembered by.
    pub name: String,
    pub tracks: Vec<RailTrack>,
}

/// A name as a line that matches it: the few characters that would make
/// the line something else are written as `?`, which matches any one.
fn literal(name: &str) -> String {
    let mut text = name.replace(PART_SEPARATOR, " ? ");
    let leads_astray = |c: char| c.is_whitespace() || matches!(c, '#' | '@' | '[');
    if text.chars().next().is_some_and(leads_astray) {
        let first = text.chars().next().map_or(0, char::len_utf8);
        text.replace_range(..first, "?");
    }
    if text.chars().last().is_some_and(char::is_whitespace) {
        let last = text.chars().last().map_or(0, char::len_utf8);
        text.replace_range(text.len() - last.., "?");
    }
    text
}

/// A name cut into runs of digits and runs of everything else.
fn digit_runs(name: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut digits = None;
    for (at, c) in name.char_indices() {
        let is_digit = c.is_ascii_digit();
        if digits.is_some_and(|d| d != is_digit) {
            runs.push(&name[start..at]);
            start = at;
        }
        digits = Some(is_digit);
    }
    if start < name.len() {
        runs.push(&name[start..]);
    }
    runs
}

/// The one run of digits in which `a` and `b` differ, when that is all
/// they differ in and `b`'s number is the greater.
fn numbered_after(a: &str, b: &str) -> Option<usize> {
    let (x, y) = (digit_runs(a), digit_runs(b));
    if x.len() != y.len() {
        return None;
    }
    let mut differing = (0..x.len()).filter(|i| x[*i] != y[*i]);
    let at = differing.next()?;
    let is_number = |run: &str| run.chars().all(|c| c.is_ascii_digit());
    if differing.next().is_some() || !is_number(x[at]) || !is_number(y[at]) || natural_cmp(x[at], y[at]).is_ge() {
        return None;
    }
    Some(at)
}

/// The lines of one section, by name. With `families`, threads that follow
/// one another and differ only in a number that goes up (`Worker (0)`,
/// `Worker (1)`, ...) are written as one line with `*` for the number: the
/// line then holds for a run of the program that has more of them, or
/// fewer.
fn section_lines(section: &RailSection, families: bool) -> Vec<String> {
    // The thread lines carry the name they were made from, to be joined.
    let mut lines: Vec<(String, Option<&str>)> = Vec::new();
    let mut written: std::collections::HashSet<String> = std::collections::HashSet::new();
    for track in &section.tracks {
        // A track of another process drawn here has no name to be found by
        // among this one's, and a thread without a name has none at all.
        if track.key.owner_pid() != section.pid || track.thread.is_empty() {
            continue;
        }
        let thread = literal(&track.thread);
        let (line, name) = match track.key {
            TrackKey::Thread(_) => (thread, Some(track.thread.as_str())),
            _ if track.follows => continue,
            TrackKey::Async(_) => (format!("{thread}{PART_SEPARATOR}{ASYNC_PART}"), None),
            TrackKey::Value(_) if track.graph.is_empty() => continue,
            TrackKey::Value(_) => (format!("{thread}{PART_SEPARATOR}{}", literal(&track.graph)), None),
        };
        if written.insert(line.clone()) {
            lines.push((line, name));
        }
    }
    if !families {
        return lines.into_iter().map(|(line, _)| line).collect();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut end = i + 1;
        let mut family = None;
        if let Some(first) = lines[i].1 {
            while let Some(next) = lines.get(end).and_then(|l| l.1) {
                let at = numbered_after(lines[end - 1].1.unwrap_or_default(), next);
                if at.is_none() || family.is_some_and(|f| Some(f) != at) {
                    break;
                }
                family = at;
                end += 1;
            }
            if let Some(at) = family {
                let mut runs = digit_runs(first);
                runs[at] = "*";
                let pattern = literal(&runs.concat());
                // A line of nothing but the wildcard would take every
                // thread there is.
                if pattern.chars().any(|c| c != '*') {
                    out.push(pattern);
                    i = end;
                    continue;
                }
            }
        }
        out.push(lines[i].0.clone());
        i += 1;
    }
    out
}

/// The order `lines` put the section's own tracks in when read back: the
/// tracks they place, top to bottom. Tracks of one name on one line cannot
/// be told apart and stay as they are among themselves.
fn order_read_back(section: &RailSection, lines: &[String]) -> Vec<usize> {
    let arrangement = Arrangement {
        sections: vec![Section { process: Pick::Id(section.pid), tracks: lines.iter().map(|l| parse_track(l)).collect() }],
    };
    let process = RailProcess { pid: section.pid, names: Vec::new(), target: false };
    let placed = arrangement.resolve(&[process], &section.tracks).tracks;
    let mut order: Vec<(u64, usize)> =
        section.tracks.iter().enumerate().filter_map(|(i, t)| Some((*placed.get(&t.key)?, i))).collect();
    order.sort();
    order.into_iter().map(|(_, i)| i).collect()
}

/// The rail as text that finds its tracks by name: what is remembered for
/// a process and applied the next time it is captured. `title` goes in the
/// comment at the top.
pub fn write_by_name(title: &str, sections: &[RailSection]) -> String {
    let mut out = format!(
        "# perfr track order for {title}\n\
         # Tracks are listed top to bottom under their process; what is not listed\n\
         # comes after, in the viewer's usual order. A line is a thread's name,\n\
         # `thread > graph` one of its graphs, `thread > [async]` its async block.\n\
         # `*` matches any run of characters and `?` one; case does not matter.\n"
    );
    for section in sections {
        if section.name.is_empty() {
            continue;
        }
        // A family's line must not change what the names alone say: its
        // `*` also matches a thread that was put somewhere else.
        let mut lines = section_lines(section, false);
        let families = section_lines(section, true);
        if families != lines && order_read_back(section, &families) == order_read_back(section, &lines) {
            lines = families;
        }
        out.push_str(&format!("[{}]\n", literal(&section.name)));
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The rail as text that names its tracks by number: exact, and only good
/// for the capture it was written from, which is where it is kept.
pub fn write_exact(sections: &[RailSection]) -> String {
    let mut out = String::new();
    for section in sections {
        out.push_str(&format!("[@{}]\n", section.pid));
        for track in &section.tracks {
            let owner = track.key.owner();
            let thread = if owner.pid == section.pid { format!("@{}", owner.tid) } else { format!("@{}:{}", owner.pid, owner.tid) };
            match track.key {
                TrackKey::Thread(_) => out.push_str(&thread),
                TrackKey::Async(_) => out.push_str(&format!("{thread}{PART_SEPARATOR}{ASYNC_PART}")),
                TrackKey::Value(_) if track.graph.trim().is_empty() => continue,
                TrackKey::Value(_) => out.push_str(&format!("{thread}{PART_SEPARATOR}{}", track.graph.trim())),
            }
            out.push('\n');
        }
    }
    out
}

/// The rail orders the viewer applies and keeps: the one the capture on
/// screen came with, and the one remembered for its target.
#[derive(Default)]
pub struct Arranging {
    /// The order the capture came with (a file's, or the one the service
    /// holds for its capture), kept up with what the hand changes since.
    /// It names every track by number, so it wins over a remembered one.
    pub capture: Option<Arrangement>,
    /// The capture's order as last exchanged with the service, received or
    /// sent: what the service holds, as far as this page knows.
    pub sent: String,
    pub sent_s: f64,
    /// The process name a remembered order was last asked for.
    pub process: String,
    /// The order the service keeps for that name: its text and what it
    /// parses to. `None` when it keeps none, which is "not remembered".
    pub remembered: Option<(String, Arrangement)>,
    /// Where the service keeps, or would keep, that file.
    pub path: String,
    /// The rail's count of changes by hand when the orders were last
    /// written from it.
    pub hand_gen: u64,
    /// When what the orders ask of the rail was last worked out, and how
    /// many processes and tracks the rail had then. Negative: again now.
    pub applied_s: f64,
    pub applied_to: (usize, usize),
}

impl Arranging {
    /// The order in force: the capture's own, else the remembered one.
    pub fn active(&self) -> Option<&Arrangement> {
        self.capture.as_ref().or(self.remembered.as_ref().map(|(_, a)| a)).filter(|a| !a.is_empty())
    }

    /// The capture on screen is another one: nothing of the last one's
    /// order holds, nor what was told the service about it.
    pub fn forget_capture(&mut self) {
        self.capture = None;
        self.sent.clear();
        self.applied_s = -1.0;
    }

    /// An order that came with the capture. One this page sent itself and
    /// is handed back is not news.
    pub fn adopt_capture(&mut self, text: &str) {
        if text == self.sent {
            return;
        }
        self.sent = text.to_string();
        self.capture = Some(Arrangement::parse(text));
        self.applied_s = -1.0;
    }

    /// The service's answer about the order it keeps for a process name.
    pub fn adopt_remembered(&mut self, process: &str, text: Option<String>, path: Option<String>) {
        if process != self.process {
            return;
        }
        self.path = path.unwrap_or_default();
        if self.remembered.as_ref().map(|(t, _)| t) != text.as_ref() {
            self.remembered = text.map(|t| {
                let parsed = Arrangement::parse(&t);
                (t, parsed)
            });
            self.applied_s = -1.0;
        }
    }
}

/// The viewer's side: the text as edited, its parsed rules, and a file
/// pick in flight.
#[derive(Default)]
pub struct State {
    pub text: String,
    pub rules: Rules,
    /// The text last sent to (or received from) the service, so a settings
    /// reply carrying what we already have does not clobber an edit.
    pub synced: String,
    pub message: String,
    pub import: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
}

impl State {
    /// Re-parses after an edit. Returns whether the rules changed.
    pub fn reparse(&mut self) -> bool {
        let rules = Rules::parse(&self.text);
        if rules != self.rules {
            self.rules = rules;
            true
        } else {
            false
        }
    }

    /// A settings reply from the service: adopt its text unless it is the
    /// one we last exchanged (an echo of our own PUT) or we are mid-edit.
    pub fn adopt_settings(&mut self, text: &str) {
        if text == self.synced {
            return;
        }
        self.synced = text.to_string();
        self.text = text.to_string();
        self.reparse();
    }

    pub fn receive(&mut self) {
        let result = self.import.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.import = None;
            match result {
                Ok(text) => {
                    if text.is_empty() {
                        return;
                    }
                    self.text = text;
                    self.reparse();
                    self.message.clear();
                }
                Err(error) => self.message = error,
            }
        }
    }
}

pub const FILENAME: &str = "track-order.txt";

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function perfrPickTrackOrder() {
  return new Promise((resolve, reject) => {
    const input = document.createElement('input');
    input.type = 'file'; input.accept = '.txt,text/plain'; input.multiple = false;
    input.style.display = 'none'; document.body.appendChild(input);
    input.oncancel = () => { input.remove(); resolve(''); };
    input.onchange = async () => {
      try {
        const file = (input.files || [])[0];
        if (!file) { resolve(''); return; }
        if (file.size > 1024 * 1024) throw new Error('Track order file exceeds 1 MB');
        resolve(await file.text());
      } catch (e) { reject(String(e)); } finally { input.remove(); }
    };
    input.click();
  });
}
export function perfrSaveTrackOrder(name, text) {
  const url = URL.createObjectURL(new Blob([text], {type: 'text/plain'}));
  const a = document.createElement('a'); a.href = url; a.download = name;
  document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    async fn perfrPickTrackOrder() -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
    #[wasm_bindgen(catch)]
    fn perfrSaveTrackOrder(name: &str, text: &str) -> Result<(), wasm_bindgen::JsValue>;
}

/// Picks a file; the text arrives on the receiver (empty when cancelled).
pub fn open(ctx: egui::Context) -> std::sync::mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(async move {
        let result = match perfrPickTrackOrder().await {
            Ok(value) => Ok(value.as_string().unwrap_or_default()),
            Err(e) => Err(format!("Could not open the track order file: {e:?}")),
        };
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    #[cfg(not(target_arch = "wasm32"))]
    {
        let result = match rfd::FileDialog::new().add_filter("Track order", &["txt"]).pick_file() {
            Some(path) => std::fs::read_to_string(path).map_err(|e| e.to_string()),
            None => Ok(String::new()),
        };
        let _ = tx.send(result);
        ctx.request_repaint();
    }
    rx
}

pub fn save(text: &str) -> Result<(), String> {
    #[cfg(target_arch = "wasm32")]
    perfrSaveTrackOrder(FILENAME, text).map_err(|e| format!("Could not save the track order file: {e:?}"))?;
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = rfd::FileDialog::new().set_file_name(FILENAME).save_file() {
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_runs_single_characters_and_ignore_case() {
        assert!(glob_match("TP_ThirdPerson*", "tp_thirdperson-linux-shipping"));
        assert!(glob_match("*shipping", "TP_ThirdPerson-Linux-Shipping"));
        assert!(glob_match("perfr-ho?k", "perfr-hook"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
        assert!(!glob_match("perfr", "perfr-hook"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn a_file_orders_matches_first_in_line_order_and_skips_comments() {
        let rules = Rules::parse("# the game first\nTP_ThirdPerson*\n\n  perfr  \n*/bin/python3*\n");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules.rank(["TP_ThirdPerson-", "TP_ThirdPerson-Linux-Shipping", "/x/TP_ThirdPerson-Linux-Shipping"]), Some(0));
        assert_eq!(rules.rank(["perfr", "perfr", "/usr/bin/perfr"]), Some(1));
        // A pattern with a slash is tried against the path only: the comm
        // "python3" alone does not match it.
        assert_eq!(rules.rank(["python3", "python3", "/usr/bin/python3.12"]), Some(2));
        assert_eq!(rules.rank(["python3"]), None);
        assert_eq!(rules.rank(["bash", "bash", "/bin/bash"]), None);
    }

    #[test]
    fn the_first_matching_line_wins() {
        let rules = Rules::parse("*\nperfr\n");
        assert_eq!(rules.rank(["perfr"]), Some(0));
    }

    fn thread(pid: u32, tid: u32, name: &str) -> RailTrack {
        let key = TrackKey::Thread(crate::tracks::ThreadId { pid, tid });
        RailTrack { key, thread: name.into(), graph: String::new(), follows: false }
    }

    fn graph(pid: u32, tid: u32, slot: u8, thread: &str, name: &str, follows: bool) -> RailTrack {
        let key = TrackKey::Value(perfr_live_event::LaneKey { pid, tid, kind: perfr_live_event::kind::VALUE, depth: slot, extra: 0 });
        RailTrack { key, thread: thread.into(), graph: name.into(), follows }
    }

    fn async_block(pid: u32, tid: u32, thread: &str, follows: bool) -> RailTrack {
        let key = TrackKey::Async(crate::tracks::ThreadId { pid, tid });
        RailTrack { key, thread: thread.into(), graph: String::new(), follows }
    }

    fn process(pid: u32, name: &str) -> RailProcess {
        RailProcess { pid, names: vec![name.into()], target: false }
    }

    /// The tracks in the order an arrangement puts them: placed ones by
    /// their place, the rest behind in the order given.
    fn ordered(resolved: &Resolved, tracks: &[RailTrack]) -> Vec<TrackKey> {
        let mut keys: Vec<(u64, usize, TrackKey)> =
            tracks.iter().enumerate().map(|(i, t)| (resolved.tracks.get(&t.key).copied().unwrap_or(u64::MAX), i, t.key)).collect();
        keys.sort_by_key(|(place, at, _)| (*place, *at));
        keys.into_iter().map(|(_, _, key)| key).collect()
    }

    fn tids(keys: &[TrackKey]) -> Vec<u32> {
        keys.iter().map(|k| k.owner().tid).collect()
    }

    #[test]
    fn a_rail_order_reads_sections_tracks_parts_and_numbers() {
        let text = "# mine\nloose\n[MyGame*]\nmain\nWorker (*)\nmain > frame time\nmain > [async]\n\n[@7]\n@70\n@9:90 > a > b\n@70 > [async]\n[ ]\n";
        let a = Arrangement::parse(text);
        let name = |n: &str| Pick::Name(n.into());
        let line = |owner_pid, thread, part| TrackLine { owner_pid, thread, part };
        assert_eq!(
            a.sections,
            vec![
                Section { process: Pick::Target, tracks: vec![line(None, name("loose"), Part::Thread)] },
                Section {
                    process: name("MyGame*"),
                    tracks: vec![
                        line(None, name("main"), Part::Thread),
                        line(None, name("Worker (*)"), Part::Thread),
                        line(None, name("main"), Part::Graph("frame time".into())),
                        line(None, name("main"), Part::Async),
                    ],
                },
                Section {
                    process: Pick::Id(7),
                    tracks: vec![
                        line(None, Pick::Id(70), Part::Thread),
                        // A numbered thread: the first ` > ` separates, so
                        // the graph's name can hold one.
                        line(Some(9), Pick::Id(90), Part::Graph("a > b".into())),
                        line(None, Pick::Id(70), Part::Async),
                    ],
                },
                Section { process: name(""), tracks: vec![] },
            ]
        );
        assert!(Arrangement::parse("# nothing\n\n").is_empty());
    }

    #[test]
    fn names_are_ordered_with_their_numbers_counted() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("Worker (2)", "Worker (10)"), Less);
        assert_eq!(natural_cmp("worker 10", "Worker 9"), Greater);
        assert_eq!(natural_cmp("a", "B"), Less);
        assert_eq!(natural_cmp("pool-7", "pool-07"), Equal);
        assert_eq!(natural_cmp("pool", "pool-1"), Less);
        assert_eq!(natural_cmp("x99999999999999999999999", "x100000000000000000000000"), Less);
        assert_eq!(natural_cmp("", ""), Equal);
    }

    #[test]
    fn named_tracks_lead_in_line_order_and_the_rest_keep_theirs() {
        let tracks = vec![
            thread(7, 1, "io"),
            thread(7, 2, "Worker (10)"),
            thread(7, 3, "render"),
            thread(7, 4, "Worker (2)"),
            thread(7, 5, "main"),
            thread(7, 6, ""),
            thread(9, 7, "main"),
        ];
        let processes = [process(7, "game"), process(9, "other")];
        let a = Arrangement::parse("[game]\nmain\nworker (*)\nRENDER\n");
        let r = a.resolve(&processes, &tracks);
        assert_eq!(r.processes.get(&7), Some(&0));
        assert_eq!(r.processes.get(&9), None, "a process the text does not name is not placed");
        // main, the workers by number, render; then what no line names, as
        // it was. The other process's `main` is not this section's.
        assert_eq!(tids(&ordered(&r, &tracks)), [5, 4, 2, 3, 1, 6, 7]);
        assert_eq!(r.tracks.len(), 4);
        assert!(r.hosts.is_empty());
    }

    #[test]
    fn the_first_line_that_matches_a_track_places_it() {
        let tracks = vec![thread(7, 1, "Worker (1)"), thread(7, 2, "Worker (2)"), thread(7, 3, "main")];
        let a = Arrangement::parse("[*]\nWorker (2)\nmain\nWorker*\n");
        let r = a.resolve(&[process(7, "game")], &tracks);
        assert_eq!(tids(&ordered(&r, &tracks)), [2, 3, 1]);
    }

    #[test]
    fn lines_before_any_section_are_the_targets() {
        let tracks = vec![thread(7, 1, "a"), thread(7, 2, "b"), thread(9, 3, "b")];
        let processes = [RailProcess { pid: 7, names: vec!["game".into()], target: true }, process(9, "other")];
        let r = Arrangement::parse("b\na\n").resolve(&processes, &tracks);
        assert_eq!(tids(&ordered(&r, &tracks)), [2, 1, 3]);
        assert_eq!(r.processes.get(&7), Some(&0));
        // With no target on the rail the lines are nobody's.
        let none = [process(7, "game"), process(9, "other")];
        assert!(Arrangement::parse("b\na\n").resolve(&none, &tracks).tracks.is_empty());
    }

    #[test]
    fn a_process_is_found_by_its_name_its_executable_or_its_path() {
        let game = RailProcess {
            pid: 7,
            names: vec!["TP_ThirdPerson-".into(), "TP_ThirdPerson-Linux-Shipping".into(), "/opt/game/TP_ThirdPerson-Linux-Shipping".into()],
            target: false,
        };
        let tracks = vec![thread(7, 1, "main")];
        for header in ["[tp_thirdperson*]", "[*shipping]", "[/opt/game/*]", "[@7]"] {
            let r = Arrangement::parse(&format!("{header}\nmain\n")).resolve(std::slice::from_ref(&game), &tracks);
            assert_eq!(r.tracks.len(), 1, "{header}");
        }
        for header in ["[shipping]", "[/opt/*/nothing]", "[@8]"] {
            let r = Arrangement::parse(&format!("{header}\nmain\n")).resolve(std::slice::from_ref(&game), &tracks);
            assert!(r.tracks.is_empty() && r.processes.is_empty(), "{header}");
        }
    }

    #[test]
    fn graphs_and_async_blocks_are_placed_by_lines_of_their_own() {
        let tracks = vec![
            thread(7, 1, "main"),
            async_block(7, 1, "main", true),
            graph(7, 1, 0, "main", "fps", true),
            graph(7, 1, 1, "main", "frame time", true),
            thread(7, 2, "render"),
        ];
        let a = Arrangement::parse("[game]\nmain > frame*\nrender\nmain > [async]\nmain\n");
        let r = a.resolve(&[process(7, "game")], &tracks);
        // `fps` has no line: it is not placed, and follows its thread.
        assert_eq!(r.tracks.len(), 4);
        assert!(!r.tracks.contains_key(&tracks[2].key));
        let placed: Vec<TrackKey> = ordered(&r, &tracks).into_iter().take(4).collect();
        assert_eq!(placed, [tracks[3].key, tracks[4].key, tracks[1].key, tracks[0].key]);
        // A thread's line is about the thread alone.
        let r = Arrangement::parse("[game]\nmain\n").resolve(&[process(7, "game")], &tracks);
        assert_eq!(r.tracks.keys().copied().collect::<Vec<_>>(), [tracks[0].key]);
    }

    fn section(pid: u32, name: &str, tracks: Vec<RailTrack>) -> RailSection {
        RailSection { pid, name: name.into(), tracks }
    }

    /// The lines of `text` that are not comments.
    fn lines(text: &str) -> Vec<&str> {
        text.lines().filter(|l| !l.starts_with('#')).collect()
    }

    #[test]
    fn an_order_written_by_name_reads_back_as_the_same_order() {
        let tracks = vec![
            thread(7, 5, "main"),
            thread(7, 2, "render"),
            thread(7, 9, "io 2"),
            thread(7, 8, "io 1"),
            thread(7, 3, ""),
            thread(7, 4, "audio"),
        ];
        let text = write_by_name("game", &[section(7, "game", tracks.clone())]);
        assert!(text.starts_with("# perfr track order for game\n"), "{text}");
        // The two `io` threads are not in their numbers' order: no family.
        assert_eq!(lines(&text), ["[game]", "main", "render", "io 2", "io 1", "audio"]);
        // Another run: other ids, another arrival order, a thread more.
        let again = vec![
            thread(70, 11, "audio"),
            thread(70, 12, "io 1"),
            thread(70, 13, "new"),
            thread(70, 14, "main"),
            thread(70, 15, "io 2"),
            thread(70, 16, "render"),
        ];
        let r = Arrangement::parse(&text).resolve(&[process(70, "game")], &again);
        assert_eq!(tids(&ordered(&r, &again)), [14, 16, 15, 12, 11, 13]);
    }

    #[test]
    fn threads_numbered_in_a_row_are_written_as_one_line_that_holds_for_more_of_them() {
        let tracks = vec![
            thread(7, 1, "main"),
            thread(7, 2, "Compute Task Pool (0)"),
            thread(7, 3, "Compute Task Pool (1)"),
            thread(7, 4, "Compute Task Pool (2)"),
            thread(7, 5, "Compute Task Pool (10)"),
            thread(7, 6, "IO Task Pool (0)"),
            thread(7, 7, "render"),
        ];
        let text = write_by_name("game", &[section(7, "game", tracks)]);
        // One `IO` thread is not a family.
        assert_eq!(lines(&text), ["[game]", "main", "Compute Task Pool (*)", "IO Task Pool (0)", "render"]);
        // A machine with more cores: its threads are in the family, in order.
        let again = vec![
            thread(7, 21, "render"),
            thread(7, 22, "Compute Task Pool (11)"),
            thread(7, 23, "Compute Task Pool (3)"),
            thread(7, 24, "main"),
            thread(7, 25, "Compute Task Pool (0)"),
            thread(7, 26, "IO Task Pool (0)"),
        ];
        let r = Arrangement::parse(&text).resolve(&[process(7, "game")], &again);
        assert_eq!(tids(&ordered(&r, &again)), [24, 25, 23, 22, 26, 21]);
    }

    #[test]
    fn a_family_is_not_written_when_its_line_would_take_a_thread_placed_elsewhere() {
        // `Worker 7` was put under `main`, away from the others.
        let tracks = vec![thread(7, 1, "Worker 1"), thread(7, 2, "Worker 2"), thread(7, 3, "main"), thread(7, 4, "Worker 7")];
        let text = write_by_name("game", &[section(7, "game", tracks.clone())]);
        assert_eq!(lines(&text), ["[game]", "Worker 1", "Worker 2", "main", "Worker 7"]);
        // Put above them it is named first, and the family's line is safe.
        let tracks = vec![thread(7, 4, "Worker 7"), thread(7, 3, "main"), thread(7, 1, "Worker 1"), thread(7, 2, "Worker 2")];
        let text = write_by_name("game", &[section(7, "game", tracks.clone())]);
        assert_eq!(lines(&text), ["[game]", "Worker 7", "main", "Worker *"]);
        let r = Arrangement::parse(&text).resolve(&[process(7, "game")], &tracks);
        assert_eq!(tids(&ordered(&r, &tracks)), [4, 3, 1, 2]);
        // Threads named by a number alone are not `*`, which is everything.
        let tracks = vec![thread(7, 1, "1"), thread(7, 2, "2"), thread(7, 3, "main")];
        let text = write_by_name("game", &[section(7, "game", tracks)]);
        assert_eq!(lines(&text), ["[game]", "1", "2", "main"]);
    }

    #[test]
    fn a_graph_is_written_only_when_it_was_moved_from_under_its_thread() {
        let at_home = vec![
            thread(7, 1, "main"),
            async_block(7, 1, "main", true),
            graph(7, 1, 0, "main", "fps", true),
            thread(7, 2, "render"),
        ];
        let text = write_by_name("game", &[section(7, "game", at_home)]);
        assert_eq!(lines(&text), ["[game]", "main", "render"]);
        let moved = vec![
            graph(7, 1, 0, "main", "fps", false),
            thread(7, 1, "main"),
            async_block(7, 1, "main", false),
            thread(7, 2, "render"),
        ];
        let text = write_by_name("game", &[section(7, "game", moved.clone())]);
        assert_eq!(lines(&text), ["[game]", "main > fps", "main", "main > [async]", "render"]);
        let r = Arrangement::parse(&text).resolve(&[process(7, "game")], &moved);
        assert_eq!(ordered(&r, &moved), moved.iter().map(|t| t.key).collect::<Vec<_>>());
    }

    #[test]
    fn names_that_would_read_as_something_else_are_written_so_they_match() {
        let tracks = vec![
            thread(7, 1, "#render"),
            thread(7, 2, "@12"),
            thread(7, 3, "[pool]"),
            thread(7, 4, "a > b"),
            thread(7, 5, " padded "),
        ];
        let text = write_by_name("game", &[section(7, "[odd] name", tracks.clone())]);
        assert_eq!(lines(&text), ["[?odd] name]", "?render", "?12", "?pool]", "a ? b", "?padded?"]);
        let r = Arrangement::parse(&text).resolve(&[process(7, "[odd] name")], &tracks);
        assert_eq!(tids(&ordered(&r, &tracks)), [1, 2, 3, 4, 5]);
        assert_eq!(r.tracks.len(), 5);
    }

    #[test]
    fn an_exact_order_names_every_track_by_number_and_reads_back_to_the_rail() {
        let guest = thread(9, 90, "theirs");
        let sections = vec![
            section(
                7,
                "game",
                vec![
                    graph(7, 71, 0, "worker", "queue > depth", false),
                    thread(7, 71, "worker"),
                    guest.clone(),
                    thread(7, 70, "worker"),
                    async_block(7, 70, "worker", true),
                    thread(7, 72, ""),
                ],
            ),
            section(9, "other", vec![thread(9, 91, "main")]),
        ];
        let text = write_exact(&sections);
        assert_eq!(text, "[@7]\n@71 > queue > depth\n@71\n@9:90\n@70\n@70 > [async]\n@72\n[@9]\n@91\n");
        let all: Vec<RailTrack> = sections.iter().flat_map(|s| s.tracks.iter().cloned()).collect();
        // Shuffled, as a file read back gives them.
        let mut shuffled = all.clone();
        shuffled.reverse();
        let r = Arrangement::parse(&text).resolve(&[process(9, "other"), process(7, "game")], &shuffled);
        assert_eq!(ordered(&r, &shuffled), all.iter().map(|t| t.key).collect::<Vec<_>>());
        assert_eq!(r.processes.get(&7), Some(&0));
        assert_eq!(r.processes.get(&9), Some(&1));
        // The track of the other process is drawn under this one.
        assert_eq!(r.hosts.len(), 1);
        assert_eq!(r.hosts.get(&guest.key), Some(&7));
    }

    #[test]
    fn the_captures_own_order_wins_and_goes_with_the_capture() {
        let mut state = Arranging::default();
        assert!(state.active().is_none());
        // An answer for a name that was not asked for is dropped.
        state.adopt_remembered("game", Some("[game]\nmain\n".into()), None);
        assert!(state.remembered.is_none());
        state.process = "game".into();
        state.adopt_remembered("game", Some("[game]\nmain\n".into()), Some("/cfg/track-order/game.txt".into()));
        assert_eq!(state.path, "/cfg/track-order/game.txt");
        assert_eq!(state.active(), Some(&Arrangement::parse("[game]\nmain\n")));
        // The capture's own order, when it has one, is the one in force.
        state.adopt_capture("[@7]\n@70\n");
        assert_eq!(state.active(), Some(&Arrangement::parse("[@7]\n@70\n")));
        // What this page sent and is handed back changes nothing.
        state.capture = None;
        state.adopt_capture("[@7]\n@70\n");
        assert!(state.capture.is_none());
        // Another capture: its order is asked for anew, the remembered one holds.
        state.adopt_capture("[@7]\n@71\n");
        state.forget_capture();
        assert!(state.sent.is_empty());
        assert_eq!(state.active(), Some(&Arrangement::parse("[game]\nmain\n")));
        // The service forgot it.
        state.adopt_remembered("game", None, None);
        assert!(state.active().is_none());
        // An order with nothing in it is no order.
        state.adopt_capture("# nothing\n");
        assert!(state.active().is_none());
    }

    #[test]
    fn a_settings_echo_does_not_clobber_an_edit() {
        let mut state = State::default();
        state.adopt_settings("a\n");
        assert_eq!(state.rules.len(), 1);
        state.text = "a\nb\n".into();
        assert!(state.reparse());
        // The service replies with what we last synced: the edit stays.
        state.adopt_settings("a\n");
        assert_eq!(state.text, "a\nb\n");
        // Something new from another browser is adopted.
        state.adopt_settings("c\n");
        assert_eq!(state.text, "c\n");
        assert_eq!(state.rules.len(), 1);
    }
}
