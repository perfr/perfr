// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Whole comparisons of captures made up for the purpose: a program whose
//! figures are known, sampled and timed with noise, twice.

use perfr_diff::stats::Rng;
use perfr_diff::text::{headline, render, TextOptions};
use perfr_diff::{diff, CaptureInput, Diff, Frame, FunctionRow, Options, Per, Sample, ScopeRow, Span, SpanKind, Verdict};

const MS: u64 = 1_000_000;

/// A program: which stack is on the CPU for what share of the time, and
/// which spans it opens.
#[derive(Clone)]
struct Program {
    /// Outermost first, and the share of every millisecond it is running.
    stacks: Vec<(Vec<&'static str>, f64)>,
    spans: Vec<Timed>,
}

#[derive(Clone)]
struct Timed {
    kind: SpanKind,
    name: &'static str,
    every_ns: u64,
    mean_ns: f64,
    /// The standard deviation, as a share of the mean.
    spread: f64,
}

fn normal(rng: &mut Rng) -> f64 {
    (-2.0 * (1.0 - rng.unit()).ln()).sqrt() * (2.0 * std::f64::consts::PI * rng.unit()).cos()
}

/// `seconds` of the program, sampled every millisecond. Frame ids are
/// dealt differently for every seed: two captures agree on names only.
fn capture(label: &str, seed: u64, seconds: f64, program: &Program) -> CaptureInput {
    let mut rng = Rng::new(seed);
    let mut input = CaptureInput { label: label.to_string(), sample_period_ns: Some(MS), ..CaptureInput::default() };
    let mut names: Vec<&str> = program.stacks.iter().flat_map(|(stack, _)| stack.iter().copied()).collect();
    names.sort_unstable();
    names.dedup();
    let id_of = |name: &str| (names.iter().position(|known| *known == name).unwrap() as u32 * 7 + seed as u32 * 1000) % 100_003;
    for name in &names {
        let module = if name.starts_with("lib") { "/usr/lib/libc.so.6" } else { "/opt/game/bin/game" };
        input.frames.insert(id_of(name), Frame { name: name.to_string(), module: module.to_string() });
    }
    let end = (seconds * 1e9) as u64;
    let mut now = 0u64;
    while now < end {
        let mut at = rng.unit();
        for (stack, share) in &program.stacks {
            if at < *share {
                input.samples.push(Sample { timestamp_ns: now, frames: stack.iter().rev().map(|name| id_of(name)).collect() });
                break;
            }
            at -= share;
        }
        now += MS;
    }
    for timed in &program.spans {
        let name = input.name(timed.name);
        let mut start = 0u64;
        while start < end {
            let duration = (timed.mean_ns * (1.0 + timed.spread * normal(&mut rng))).max(1.0) as u64;
            input.spans.push(Span { kind: timed.kind, name, start_ns: start, duration_ns: duration });
            start += timed.every_ns;
        }
    }
    input
}

/// A game: 16 ms frames, a physics step that solves, a render that draws.
fn game() -> Program {
    Program {
        stacks: vec![
            (vec!["main", "update", "physics", "solve"], 0.25),
            (vec!["main", "update", "physics", "broadphase"], 0.10),
            (vec!["main", "update", "render", "draw"], 0.30),
            (vec!["main", "update", "render", "libc_memcpy"], 0.05),
        ],
        spans: vec![
            Timed { kind: SpanKind::Frame, name: "frame", every_ns: 16 * MS, mean_ns: 16.0 * MS as f64, spread: 0.03 },
            Timed { kind: SpanKind::Scope, name: "physics", every_ns: 16 * MS, mean_ns: 5.6 * MS as f64, spread: 0.10 },
            Timed { kind: SpanKind::Function, name: "draw", every_ns: 16 * MS, mean_ns: 4.8 * MS as f64, spread: 0.10 },
        ],
    }
}

fn function<'a>(diff: &'a Diff, name: &str) -> &'a FunctionRow {
    diff.functions.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("no function {name}"))
}

fn scope<'a>(diff: &'a Diff, name: &str) -> &'a ScopeRow {
    diff.scopes.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("no scope {name}"))
}

fn one(base: &CaptureInput, new: &CaptureInput, options: &Options) -> Diff {
    diff(std::slice::from_ref(base), std::slice::from_ref(new), options).unwrap()
}

#[test]
fn the_same_program_captured_twice_is_no_change() {
    // Twenty pairs, some thirty comparisons each: with false findings held
    // to 5% and a 5% threshold on top, a verdict is rare.
    let mut verdicts = 0;
    for seed in 0..20 {
        let found = one(&capture("a", seed * 2 + 1, 8.0, &game()), &capture("b", seed * 2 + 2, 8.0, &game()), &Options::default());
        assert!(found.outcome.tested >= 8, "{:?}", found.outcome);
        verdicts += found.outcome.regressed + found.outcome.improved;
        if found.outcome.regressed + found.outcome.improved == 0 {
            assert!(headline(&found).starts_with("NO CHANGE"), "{}", headline(&found));
            assert!(!found.regressed());
        }
    }
    assert!(verdicts <= 1, "{verdicts} verdicts between captures of the same program");
}

#[test]
fn a_function_that_takes_more_is_found_with_its_callers_and_its_place_in_the_tree() {
    let mut slower = game();
    slower.stacks[0].1 = 0.33;
    // Twenty seconds: long enough that chance alone is well under the 5%
    // a verdict needs, so what did not change is not called changed.
    let found = one(&capture("base", 1, 20.0, &game()), &capture("new", 2, 20.0, &slower), &Options::default());
    assert!(found.regressed());

    let solve = function(&found, "solve");
    assert_eq!(solve.module, "game", "a module is its file name");
    assert_eq!(solve.self_time.verdict, Verdict::Regressed);
    // Milliseconds of CPU in every second: 250 became 330.
    assert!((solve.self_time.base - 250.0).abs() < 15.0 && (solve.self_time.new - 330.0).abs() < 15.0, "{solve:?}");
    let (low, high) = solve.self_time.delta_ci.unwrap();
    assert!(low > 55.0 && high < 105.0 && low < solve.self_time.delta && solve.self_time.delta < high, "{low} {high}");
    assert!(solve.self_time.q.unwrap() < 0.01);
    assert!(solve.self_samples.1 > solve.self_samples.0);
    assert_eq!((found.method.per.as_str(), found.method.function_unit.as_str()), ("second", "ms"));

    // Its caller has no time of its own, and more with what it calls; so
    // has everything above, up to all sampled time.
    let physics = function(&found, "physics");
    assert_eq!(physics.self_samples, (0, 0));
    assert_eq!(physics.self_time.verdict, Verdict::Untested);
    assert_eq!(physics.inclusive.verdict, Verdict::Regressed);
    assert_eq!(function(&found, "main").inclusive.verdict, Verdict::Regressed);
    assert_eq!(found.cpu.unwrap().verdict, Verdict::Regressed);
    // What did the same work every second did not change.
    assert_eq!(function(&found, "draw").self_time.verdict, Verdict::Unchanged);
    assert_eq!(function(&found, "render").inclusive.verdict, Verdict::Unchanged);

    // The tree leads to it, largest change first at every level.
    let tree = found.tree.as_ref().unwrap();
    assert_eq!(tree.name, "all samples");
    let path = ["main", "update", "physics", "solve"];
    let mut node = tree;
    for name in path {
        assert_eq!(node.children[0].name, name, "under {}", node.name);
        node = &node.children[0];
    }
    assert_eq!(node.inclusive.verdict, Verdict::Regressed);
    assert!(node.children.is_empty());
    assert_eq!(node.samples, solve.self_samples);

    // And the report says so, to a terminal and to a pull request.
    let text = render(&found, &TextOptions::default());
    assert!(text.starts_with("REGRESSED: "), "{text}");
    assert!(text.contains("Functions, their own time (CPU time per s)"), "{text}");
    let row = text.lines().find(|line| line.contains(" solve ")).unwrap();
    assert!(row.contains('▲') && row.contains(" ms") && row.contains("game"), "{row}");
    assert!(text.contains("Where in the call tree"), "{text}");
    let markdown = render(&found, &TextOptions { markdown: true, ..TextOptions::default() });
    assert!(markdown.starts_with("**REGRESSED: "), "{markdown}");
    assert!(markdown.contains("| ▲ |"), "{markdown}");
}

#[test]
fn counted_per_frame_only_what_got_slower_is_slower() {
    // The solver takes 2 ms more of every frame, and frames are 18 ms for
    // it. Nothing else does more work.
    let base = game();
    let mut new = game();
    for (index, per_frame_ms) in [6.0, 1.6, 4.8, 0.8].into_iter().enumerate() {
        new.stacks[index].1 = per_frame_ms / 18.0;
    }
    new.spans[0].every_ns = 18 * MS;
    new.spans[0].mean_ns = 18.0 * MS as f64;
    let (base, new) = (capture("base", 3, 10.0, &base), capture("new", 4, 10.0, &new));

    let per_frame = one(&base, &new, &Options { per: Per::Scope("frame".to_string()), ..Options::default() });
    assert_eq!(per_frame.method.function_unit, "ms");
    let solve = function(&per_frame, "solve");
    assert_eq!(solve.self_time.verdict, Verdict::Regressed);
    assert!((solve.self_time.base - 4.0).abs() < 0.3 && (solve.self_time.new - 6.0).abs() < 0.3, "{solve:?}");
    for name in ["draw", "broadphase"] {
        assert_eq!(function(&per_frame, name).self_time.verdict, Verdict::Unchanged, "{name}");
    }
    // All sampled time per frame: 11.2 ms became 13.2.
    let cpu = per_frame.cpu.unwrap();
    assert_eq!(cpu.verdict, Verdict::Regressed);
    assert!((cpu.base - 11.2).abs() < 0.4 && (cpu.new - 13.2).abs() < 0.4, "{cpu:?}");
    assert_eq!(per_frame.method.cpu_unit, "ms");

    // As shares, the same captures say the rest got faster: they are a
    // smaller part of a larger whole. The report says to mind that.
    let shares = one(&base, &new, &Options { per: Per::Sample, ..Options::default() });
    assert_eq!(shares.method.function_unit, "share");
    assert_eq!(function(&shares, "solve").self_time.verdict, Verdict::Regressed);
    assert_eq!(function(&shares, "draw").self_time.verdict, Verdict::Improved);
    assert!((function(&shares, "draw").self_time.base - 4.8 / 11.2).abs() < 0.02);
    assert!(shares.method.notes.iter().any(|note| note.contains("Shares add up to 100%")));
    // So does time per second, here: the program runs one frame after
    // another, and a longer frame leaves fewer of them in a second.
    let per_second = one(&base, &new, &Options::default());
    assert_eq!(function(&per_second, "draw").self_time.verdict, Verdict::Improved);
    assert!(per_second.method.notes.iter().any(|note| note.contains("count per capture")));
    assert!(!per_frame.method.notes.iter().any(|note| note.contains("Shares add up") || note.contains("count per capture")));
    let text = render(&shares, &TextOptions::default());
    assert!(text.contains("Functions, their own time (share of samples)") && text.contains(" pp "), "{text}");
}

#[test]
fn a_scope_that_takes_longer_per_call_is_found_and_its_interval_holds_the_truth() {
    let mut slower = game();
    slower.spans[1].mean_ns *= 1.08;
    let found = one(&capture("base", 5, 8.0, &game()), &capture("new", 6, 8.0, &slower), &Options::default());
    let physics = scope(&found, "physics");
    assert_eq!(physics.kind, SpanKind::Scope);
    assert_eq!(physics.calls, (500, 500));
    assert_eq!(physics.mean.verdict, Verdict::Regressed);
    let (low, high) = physics.mean.change_ci.unwrap();
    assert!(low < 0.08 && 0.08 < high && low > 0.05 && high < 0.11, "{low} {high}");
    // Nanoseconds per call.
    assert!((physics.mean.base - 5.6e6).abs() < 0.1e6, "{}", physics.mean.base);
    // The quantiles moved with it.
    let (low, high) = physics.median.change_ci.unwrap();
    assert!(low < 0.08 && 0.08 < high && low > 0.0, "{low} {high}");
    assert!(physics.p95.new > physics.p95.base && physics.p95.base > physics.median.base);
    // And what it costs a second: 5.6 ms of every 16.
    assert_eq!(physics.total.verdict, Verdict::Regressed);
    assert!((physics.total.base - 350.0).abs() < 10.0, "{}", physics.total.base);

    // The others are tested and are not verdicts.
    assert_eq!(scope(&found, "draw").mean.verdict, Verdict::Unchanged);
    assert_eq!(scope(&found, "draw").kind, SpanKind::Function);
    assert_eq!(scope(&found, "frame").mean.verdict, Verdict::Unchanged);
    assert_eq!((found.outcome.regressed, found.outcome.improved), (1, 0));

    let text = render(&found, &TextOptions::default());
    let row = text.lines().find(|line| line.trim_start().starts_with("physics")).unwrap();
    assert!(row.contains("▲") && row.contains("ms"), "{row}");
    // Frames are listed whatever they did.
    assert!(text.lines().any(|line| line.trim_start().starts_with("frame [frame]")), "{text}");
}

#[test]
fn a_scope_called_more_often_costs_the_same_per_call_and_more_per_frame() {
    let mut twice = game();
    twice.spans[2].every_ns = 8 * MS;
    let options = Options { per: Per::Scope("frame".to_string()), ..Options::default() };
    let found = one(&capture("base", 7, 8.0, &game()), &capture("new", 8, 8.0, &twice), &options);
    let draw = scope(&found, "draw");
    assert_eq!(draw.calls, (500, 1000));
    assert_eq!(draw.mean.verdict, Verdict::Unchanged);
    assert_eq!(draw.total.verdict, Verdict::Regressed);
    // Milliseconds per frame: 4.8, then twice that.
    assert!((draw.total.base - 4.8).abs() < 0.2 && (draw.total.new - 9.6).abs() < 0.3, "{draw:?}");
    assert!(found.regressed());
}

#[test]
fn what_is_only_on_one_side_is_new_or_gone() {
    let mut with_more = game();
    with_more.stacks.push((vec!["main", "update", "audio_mix"], 0.08));
    with_more.spans.push(Timed { kind: SpanKind::Scope, name: "audio", every_ns: 16 * MS, mean_ns: 1.3 * MS as f64, spread: 0.1 });
    let (plain, more) = (capture("plain", 9, 8.0, &game()), capture("more", 10, 8.0, &with_more));

    let added = one(&plain, &more, &Options::default());
    let mix = function(&added, "audio_mix");
    assert_eq!(mix.self_samples.0, 0);
    assert_eq!((mix.self_time.base, mix.self_time.change), (0.0, None));
    assert_eq!(mix.self_time.verdict, Verdict::Regressed);
    let audio = scope(&added, "audio");
    assert_eq!(audio.calls, (0, 500));
    // Time per call of something that was never called is not a figure.
    assert_eq!(audio.mean.verdict, Verdict::Untested);
    assert_eq!(audio.total.verdict, Verdict::Regressed);
    assert!(render(&added, &TextOptions::default()).lines().any(|line| line.contains("audio_mix") && line.contains("new")));

    let removed = one(&more, &plain, &Options::default());
    let mix = function(&removed, "audio_mix");
    assert_eq!(mix.self_time.verdict, Verdict::Improved);
    assert_eq!(mix.self_time.change, Some(-1.0));
    assert_eq!(scope(&removed, "audio").total.verdict, Verdict::Improved);
}

#[test]
fn three_captures_a_side_measure_what_differs_from_run_to_run() {
    // Every run of this program is a few percent faster or slower than
    // the last, all of it together: the machine, not the build.
    let run = |label: &str, seed: u64, factor: f64| {
        let mut program = game();
        for timed in &mut program.spans {
            timed.mean_ns *= factor;
        }
        capture(label, seed, 6.0, &program)
    };
    let factors = [0.93, 1.06, 1.0, 1.07, 0.95, 1.01];
    let runs: Vec<CaptureInput> = factors.iter().enumerate().map(|(i, f)| run(&format!("run{i}"), 20 + i as u64, *f)).collect();

    // One against one, the runs do differ, and the comparison says so.
    let pair = one(&runs[0], &runs[1], &Options::default());
    assert_eq!(scope(&pair, "physics").mean.verdict, Verdict::Regressed);
    assert_eq!(pair.method.units, "batches");
    assert!(pair.method.notes.iter().any(|note| note.contains("three or more captures")));

    // Three against three of the same build, it does not.
    let same = diff(&runs[..3], &runs[3..], &Options::default()).unwrap();
    assert_eq!(same.method.units, "captures");
    assert_eq!((same.outcome.regressed, same.outcome.improved), (0, 0), "{}", render(&same, &TextOptions::default()));
    assert_eq!(scope(&same, "physics").mean.verdict, Verdict::Unchanged);
    assert_eq!(same.base.captures, ["run0", "run1", "run2"]);
    assert!((same.base.seconds - 18.0).abs() < 0.1);

    // A build that is 25% slower stands out of that noise.
    let slow: Vec<CaptureInput> = factors[3..].iter().enumerate().map(|(i, f)| run(&format!("slow{i}"), 40 + i as u64, f * 1.25)).collect();
    let slower = diff(&runs[..3], &slow, &Options::default()).unwrap();
    let physics = scope(&slower, "physics");
    assert_eq!(physics.mean.verdict, Verdict::Regressed);
    let (low, high) = physics.mean.change_ci.unwrap();
    assert!(low < 0.25 && 0.25 < high * 1.3, "{low} {high}");
}

#[test]
fn too_little_is_not_tested_and_nothing_at_all_is_an_error() {
    // Three calls a side, and a function with a handful of samples.
    let mut program = game();
    program.spans.push(Timed { kind: SpanKind::Scope, name: "load", every_ns: 3000 * MS, mean_ns: 900.0 * MS as f64, spread: 0.2 });
    program.stacks.push((vec!["main", "rare"], 0.0006));
    let found = one(&capture("a", 11, 8.0, &program), &capture("b", 12, 8.0, &program), &Options::default());
    let load = scope(&found, "load");
    assert_eq!(load.calls, (3, 3));
    assert_eq!((load.mean.verdict, load.mean.p), (Verdict::Untested, None));
    assert!(load.mean.change.is_some(), "the difference is still shown");
    let rare = function(&found, "rare");
    assert_eq!((rare.self_time.verdict, rare.self_time.q), (Verdict::Untested, None));

    let empty = CaptureInput { label: "empty.perfr.zip".to_string(), ..CaptureInput::default() };
    let error = diff(&[empty], &[capture("b", 1, 1.0, &game())], &Options::default()).unwrap_err();
    assert!(error.contains("empty.perfr.zip") && error.contains("nothing to compare"), "{error}");
    assert!(diff(&[], &[capture("b", 1, 1.0, &game())], &Options::default()).is_err());
    let per = Options { per: Per::Scope("tick".to_string()), ..Options::default() };
    let error = diff(&[capture("a", 1, 1.0, &game())], &[capture("b", 2, 1.0, &game())], &per).unwrap_err();
    assert!(error.contains("no scope or frame named \"tick\""), "{error}");
    let odd = Options { confidence: 95.0, ..Options::default() };
    assert!(diff(&[capture("a", 1, 1.0, &game())], &[capture("b", 2, 1.0, &game())], &odd).is_err());
}

#[test]
fn scopes_alone_and_samples_alone_are_compared_for_what_they_have() {
    let mut scopes_only = capture("a", 13, 4.0, &game());
    scopes_only.samples.clear();
    let found = one(&scopes_only, &scopes_only.clone(), &Options::default());
    assert!(found.functions.is_empty() && found.cpu.is_none() && found.tree.is_none());
    assert!(found.method.notes.iter().any(|note| note.contains("No samples on either side")));
    assert_eq!(scope(&found, "physics").mean.verdict, Verdict::Unchanged);
    assert_eq!(scope(&found, "physics").mean.p, Some(1.0));

    let mut samples_only = capture("b", 14, 4.0, &game());
    samples_only.spans.clear();
    let found = one(&samples_only, &samples_only.clone(), &Options::default());
    assert!(found.scopes.is_empty());
    assert_eq!(function(&found, "solve").self_time.verdict, Verdict::Unchanged);
    // Cores busy: 70% of one.
    let cpu = found.cpu.unwrap();
    assert!((cpu.base - 0.70).abs() < 0.03, "{cpu:?}");
    assert_eq!(found.method.cpu_unit, "cores");

    // Without a sampling period there is no time to give: samples it is.
    samples_only.sample_period_ns = None;
    let found = one(&samples_only, &samples_only.clone(), &Options { per: Per::Second, ..Options::default() });
    assert_eq!((found.method.function_unit.as_str(), found.method.cpu_unit.as_str()), ("samples", "samples/s"));
    assert!((found.cpu.unwrap().base - 700.0).abs() < 30.0);
}

#[test]
fn the_document_is_the_same_every_time_and_survives_json() {
    let mut slower = game();
    slower.stacks[0].1 = 0.33;
    slower.spans[1].mean_ns *= 1.2;
    let (base, new) = (capture("base", 15, 6.0, &game()), capture("new", 16, 6.0, &slower));
    let first = one(&base, &new, &Options::default());
    assert_eq!(first, one(&base, &new, &Options::default()));
    let json = serde_json::to_string(&first).unwrap();
    // A float may come back from JSON one last digit away, so the two are
    // compared as a reader sees them.
    let back: Diff = serde_json::from_str(&json).unwrap();
    assert_eq!(render(&back, &TextOptions::default()), render(&first, &TextOptions::default()));
    assert_eq!((back.outcome, back.functions.len(), back.scopes.len()), (first.outcome, first.functions.len(), first.scopes.len()));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["functions"][0]["self"]["verdict"], "regressed");
    assert_eq!(value["scopes"][0]["kind"], "scope");
    assert_eq!(value["method"]["per"], "second");
    // The tree is nested no deeper than a JSON reader will follow.
    assert!(value["tree"]["children"][0]["children"][0]["name"].is_string());
}

#[test]
fn a_table_keeps_what_was_tested_and_the_largest_of_the_rest() {
    let mut program = game();
    let rare: Vec<String> = (0..40).map(|i| format!("rare_{i:02}")).collect();
    let rare: &'static [String] = Vec::leak(rare);
    for name in rare {
        program.stacks.push((vec!["main", name.as_str()], 0.001));
    }
    let options = Options { max_rows: 5, ..Options::default() };
    let found = one(&capture("a", 17, 8.0, &program), &capture("b", 18, 8.0, &program), &options);
    let tested = found.functions.iter().filter(|row| row.self_time.p.is_some() || row.inclusive.p.is_some()).count();
    assert_eq!(found.functions.len(), tested + 5);
    assert_eq!(found.functions_omitted, 48 - found.functions.len() as u64);
    assert!(found.functions[..tested].iter().all(|row| row.self_time.p.is_some() || row.inclusive.p.is_some()));
}

#[test]
fn a_function_whose_line_moved_is_still_the_same_function() {
    // An interpreter's frames carry the line their function starts at. A
    // few lines added at the top of two files, and one function added.
    let before = Program {
        stacks: vec![
            (vec!["main", "tick (game.py:10)", "forward (model.py:52)"], 0.30),
            (vec!["main", "tick (game.py:10)", "forward (model.py:95)"], 0.10),
            (vec!["main", "tick (game.py:10)", "operator() (native)"], 0.10),
        ],
        spans: Vec::new(),
    };
    let after = Program {
        stacks: vec![
            (vec!["main", "tick (game.py:14)", "forward (model.py:60)"], 0.30),
            (vec!["main", "tick (game.py:14)", "forward (model.py:103)"], 0.10),
            (vec!["main", "tick (game.py:14)", "operator() (native)"], 0.10),
            (vec!["main", "tick (game.py:14)", "extra (game.py:3)"], 0.10),
        ],
        spans: Vec::new(),
    };
    let found = one(&capture("before", 51, 8.0, &before), &capture("after", 52, 8.0, &after), &Options::default());
    // The base side goes by the new side's names, and the two `forward`s
    // are told apart by their order in the file.
    let names: Vec<&str> = found.functions.iter().map(|row| row.name.as_str()).collect();
    assert!(!names.iter().any(|name| name.ends_with(":10)") || name.ends_with(":52)") || name.ends_with(":95)")), "{names:?}");
    let first = function(&found, "forward (model.py:60)");
    let second = function(&found, "forward (model.py:103)");
    assert!(first.self_samples.0 > 2 * second.self_samples.0 && second.self_samples.0 > 500, "{first:?} {second:?}");
    assert_eq!((first.self_time.verdict, second.self_time.verdict), (Verdict::Unchanged, Verdict::Unchanged));
    assert!(function(&found, "tick (game.py:14)").inclusive_samples.0 > 3000);
    // What is new is new, and is all that got slower: itself, and the two
    // frames it is under.
    let extra = function(&found, "extra (game.py:3)");
    assert_eq!((extra.self_samples.0, extra.self_time.verdict), (0, Verdict::Regressed));
    assert_eq!(found.outcome.regressed, 3, "{}", render(&found, &TextOptions::default()));
    assert!(found.method.notes.iter().any(|note| note.starts_with("3 functions are matched")), "{:?}", found.method.notes);
    // The tree is one tree: the path through `tick` is not there twice.
    let main = &found.tree.as_ref().unwrap().children[0];
    assert_eq!(main.children.len(), 1);
    assert_eq!(main.children[0].name, "tick (game.py:14)");
}
