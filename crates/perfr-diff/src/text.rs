// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A [`Diff`] written out: for a terminal, or as Markdown for a pull
//! request. The verdict is the first line, the rows that changed follow,
//! and what the figures mean is said once at the end.

use std::fmt::Write as _;

use crate::input::SpanKind;
use crate::model::{Diff, FunctionRow, Metric, Quantile, ScopeRow, Side, TreeNode, Verdict};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextOptions {
    /// Rows a table shows.
    pub top: usize,
    pub markdown: bool,
}

impl Default for TextOptions {
    fn default() -> TextOptions {
        TextOptions { top: 15, markdown: false }
    }
}

/// A duration in the unit it reads best in.
pub fn duration(ns: f64) -> String {
    let size = ns.abs();
    if size < 1e3 {
        format!("{ns:.0} ns")
    } else if size < 1e6 {
        format!("{:.2} µs", ns / 1e3)
    } else if size < 1e9 {
        format!("{:.2} ms", ns / 1e6)
    } else {
        format!("{:.2} s", ns / 1e9)
    }
}

/// A relative change as a signed percentage.
pub fn percent(change: f64) -> String {
    format!("{:+.1}%", change * 100.0)
}

fn interval(ci: Option<(f64, f64)>, show: impl Fn(f64) -> String) -> String {
    ci.map_or(String::new(), |(low, high)| format!("[{}, {}]", show(low), show(high)))
}

/// A q-value the way a table can hold it.
pub fn q_value(q: Option<f64>) -> String {
    match q {
        None => String::new(),
        Some(q) if q < 0.001 => "<0.001".to_string(),
        Some(q) => format!("{q:.3}"),
    }
}

/// The mark of a verdict: up is slower.
pub fn mark(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Regressed => "▲",
        Verdict::Improved => "▼",
        Verdict::Unchanged => "=",
        Verdict::Untested => " ",
    }
}

/// How the report's figures are written, which depends on what they are
/// counted against.
pub struct Units<'a> {
    diff: &'a Diff,
}

impl<'a> Units<'a> {
    pub fn new(diff: &'a Diff) -> Units<'a> {
        Units { diff }
    }

    /// What follows "per": a second, the capture, a named scope.
    pub fn per(&self) -> &str {
        match self.diff.method.per.as_str() {
            "sample" | "second" => "s",
            other => other,
        }
    }

    /// The heading of the function columns.
    pub fn function_heading(&self) -> String {
        match self.diff.method.function_unit.as_str() {
            "share" => "share of samples".to_string(),
            "ms" => format!("CPU time per {}", self.per()),
            _ => format!("samples per {}", self.per()),
        }
    }

    pub fn function_value(&self, value: f64) -> String {
        match self.diff.method.function_unit.as_str() {
            "share" => format!("{:.2}%", value * 100.0),
            "ms" => duration(value * 1e6),
            _ => format!("{value:.1}"),
        }
    }

    pub fn function_delta(&self, delta: f64) -> String {
        match self.diff.method.function_unit.as_str() {
            "share" => format!("{:+.2} pp", delta * 100.0),
            "ms" => format!("{}{}", if delta >= 0.0 { "+" } else { "" }, duration(delta * 1e6)),
            _ => format!("{delta:+.1}"),
        }
    }

    pub fn cpu_value(&self, value: f64) -> String {
        match self.diff.method.cpu_unit.as_str() {
            "cores" => format!("{value:.2} cores"),
            "samples/s" => format!("{value:.0} samples/s"),
            "ms" => format!("{} per {}", duration(value * 1e6), self.per()),
            _ => format!("{value:.0} samples per {}", self.per()),
        }
    }

    pub fn cpu_delta(&self, delta: f64) -> String {
        match self.diff.method.cpu_unit.as_str() {
            "cores" => format!("{delta:+.2}"),
            "ms" => format!("{}{}", if delta >= 0.0 { "+" } else { "" }, duration(delta * 1e6)),
            _ => format!("{delta:+.0}"),
        }
    }

    /// A scope's total, which is milliseconds per unit.
    pub fn total_value(&self, value: f64) -> String {
        format!("{}/{}", duration(value * 1e6), self.per())
    }
}

/// Rows of cells, written aligned or as a Markdown table.
struct Table {
    header: Vec<&'static str>,
    /// Columns whose cells sit against the right edge.
    right: Vec<bool>,
    rows: Vec<Vec<String>>,
}

impl Table {
    fn new(columns: &[(&'static str, bool)]) -> Table {
        Table {
            header: columns.iter().map(|(name, _)| *name).collect(),
            right: columns.iter().map(|(_, right)| *right).collect(),
            rows: Vec::new(),
        }
    }

    fn write(&self, out: &mut String, markdown: bool) {
        if markdown {
            let _ = writeln!(out, "| {} |", self.header.join(" | "));
            let rule: Vec<&str> = self.right.iter().map(|right| if *right { "---:" } else { "---" }).collect();
            let _ = writeln!(out, "| {} |", rule.join(" | "));
            for row in &self.rows {
                let cells: Vec<String> = row.iter().map(|cell| cell.replace('|', "\\|")).collect();
                let _ = writeln!(out, "| {} |", cells.join(" | "));
            }
            return;
        }
        let width = |column: usize| {
            std::iter::once(self.header[column].chars().count())
                .chain(self.rows.iter().map(|row| row[column].chars().count()))
                .max()
                .unwrap_or(0)
        };
        let widths: Vec<usize> = (0..self.header.len()).map(width).collect();
        let line = |cells: Vec<&str>, out: &mut String| {
            let mut text = String::from(" ");
            for (column, cell) in cells.iter().enumerate() {
                let pad = " ".repeat(widths[column] - cell.chars().count());
                if self.right[column] {
                    let _ = write!(text, " {pad}{cell} ");
                } else {
                    let _ = write!(text, " {cell}{pad} ");
                }
            }
            let _ = writeln!(out, "{}", text.trim_end());
        };
        line(self.header.clone(), out);
        for row in &self.rows {
            line(row.iter().map(String::as_str).collect(), out);
        }
    }
}

fn side_line(name: &str, side: &Side) -> String {
    let captures = match side.captures.as_slice() {
        [one] => one.clone(),
        many => format!("{} captures ({})", many.len(), many.join(", ")),
    };
    let period = side.sample_period_ns.map_or(String::new(), |ns| format!(" every {}", duration(ns as f64)));
    format!("{name}  {captures}: {:.2} s, {} samples{period}, {} spans", side.seconds, side.samples, side.spans)
}

/// The first line: what a reader in a hurry takes away.
pub fn headline(diff: &Diff) -> String {
    let outcome = diff.outcome;
    let rows = |n: u32| if n == 1 { "1 row".to_string() } else { format!("{n} rows") };
    let cpu = diff.cpu.map_or(Verdict::Untested, |cpu| cpu.verdict);
    if diff.regressed() {
        let mut text = format!("REGRESSED: {} slower", rows(outcome.regressed));
        if outcome.improved > 0 {
            let _ = write!(text, ", {} faster", outcome.improved);
        }
        let _ = write!(text, ", of {} tested.", outcome.tested);
        text
    } else if outcome.improved > 0 || cpu == Verdict::Improved {
        format!("IMPROVED: {} faster, none slower, of {} tested.", rows(outcome.improved), outcome.tested)
    } else if outcome.tested == 0 && cpu == Verdict::Untested {
        "INCONCLUSIVE: nothing had enough data to test.".to_string()
    } else {
        format!("NO CHANGE: nothing beyond chance and the threshold, of {} tested.", rows(outcome.tested))
    }
}

fn metric_cells(metric: &Metric, value: impl Fn(f64) -> String, delta: impl Fn(f64) -> String) -> Vec<String> {
    vec![
        mark(metric.verdict).to_string(),
        value(metric.base),
        value(metric.new),
        delta(metric.delta),
        metric.change.map_or_else(|| if metric.new > 0.0 { "new".to_string() } else { String::new() }, percent),
        interval(metric.change_ci, percent),
        q_value(metric.q),
    ]
}

const METRIC_COLUMNS: [(&str, bool); 7] =
    [("", false), ("base", true), ("new", true), ("change", true), ("%", true), ("interval", false), ("q", true)];

fn function_table(units: &Units<'_>, rows: &[(&FunctionRow, &Metric)]) -> Table {
    let mut columns = METRIC_COLUMNS.to_vec();
    columns.extend([("function", false), ("module", false)]);
    let mut table = Table::new(&columns);
    for (row, metric) in rows {
        let mut cells = metric_cells(metric, |v| units.function_value(v), |d| units.function_delta(d));
        cells.extend([row.name.clone(), row.module.clone()]);
        table.rows.push(cells);
    }
    table
}

/// A duration per call, or a dash for a side that never made the call.
fn per_call(ns: f64, calls: u64) -> String {
    if calls == 0 {
        "-".to_string()
    } else {
        duration(ns)
    }
}

fn quantile_cell(quantile: &Quantile, calls: (u64, u64)) -> String {
    if calls.0 == 0 || calls.1 == 0 {
        return format!("{} -> {}", per_call(quantile.base, calls.0), per_call(quantile.new, calls.1));
    }
    let change = quantile.change.map_or(String::new(), |change| format!(" {}", percent(change)));
    let ci = interval(quantile.change_ci, percent);
    format!("{} -> {}{change}{}{ci}", duration(quantile.base), duration(quantile.new), if ci.is_empty() { "" } else { " " })
}

fn scope_name(row: &ScopeRow) -> String {
    match row.kind {
        SpanKind::Scope => row.name.clone(),
        kind => format!("{} [{}]", row.name, kind.label()),
    }
}

fn scope_table(units: &Units<'_>, rows: &[&ScopeRow]) -> Table {
    let mut columns = vec![("scope", false), ("calls", true)];
    columns.extend(METRIC_COLUMNS);
    columns.extend([("", false), ("in all", false)]);
    let mut table = Table::new(&columns);
    for row in rows {
        let calls = if row.calls.0 == row.calls.1 {
            row.calls.0.to_string()
        } else {
            format!("{} -> {}", row.calls.0, row.calls.1)
        };
        let mut cells = vec![scope_name(row), calls];
        let mut mean = metric_cells(&row.mean, duration, |d| format!("{}{}", if d >= 0.0 { "+" } else { "" }, duration(d)));
        if row.calls.0 == 0 || row.calls.1 == 0 {
            // Time per call of something never called is not a figure.
            (mean[1], mean[2]) = (per_call(row.mean.base, row.calls.0), per_call(row.mean.new, row.calls.1));
            mean[3] = String::new();
            mean[4] = if row.calls.0 == 0 { "new" } else { "gone" }.to_string();
        }
        cells.extend(mean);
        let total = &row.total;
        cells.extend([
            mark(total.verdict).to_string(),
            format!(
                "{} -> {}{}",
                units.total_value(total.base),
                units.total_value(total.new),
                total.change.map_or(String::new(), |change| format!(" {} {}", percent(change), interval(total.change_ci, percent))),
            ),
        ]);
        table.rows.push(cells);
    }
    table
}

fn quantile_table(rows: &[&ScopeRow]) -> Table {
    let mut table = Table::new(&[("scope", false), ("median", false), ("95th percentile", false)]);
    for row in rows {
        table.rows.push(vec![scope_name(row), quantile_cell(&row.median, row.calls), quantile_cell(&row.p95, row.calls)]);
    }
    table
}

fn has_change(node: &TreeNode) -> bool {
    node.inclusive.changed() || node.children.iter().any(has_change)
}

/// One line of the call tree as a report shows it.
#[derive(Clone, Copy, Debug)]
pub struct TreeLine<'a> {
    pub depth: usize,
    pub node: &'a TreeNode,
    /// Frames left out right under this one: a run that each did nothing
    /// but call the next.
    pub folded: usize,
}

/// The tree cut down to where something changed: a node is shown when it
/// or something under it has a verdict, at most `budget` of them. A run of
/// frames that each only call the next is its first frame and then where
/// it ends. The second value says whether the budget cut it short.
pub fn tree_lines(tree: &TreeNode, budget: usize) -> (Vec<TreeLine<'_>>, bool) {
    fn shown(node: &TreeNode) -> Vec<&TreeNode> {
        node.children.iter().filter(|child| has_change(child)).collect()
    }
    fn only_child(node: &TreeNode) -> Option<&TreeNode> {
        match shown(node).as_slice() {
            [only] if node.samples == only.samples && node.inclusive.verdict == only.inclusive.verdict => Some(*only),
            _ => None,
        }
    }
    fn walk<'a>(node: &'a TreeNode, depth: usize, budget: &mut usize, out: &mut Vec<TreeLine<'a>>) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let mut last = node;
        let mut folded = 0;
        while let Some(next) = only_child(last).filter(|next| only_child(next).is_some()) {
            last = next;
            folded += 1;
        }
        out.push(TreeLine { depth, node, folded });
        for child in shown(last) {
            walk(child, depth + 1, budget, out);
        }
    }
    let mut lines = Vec::new();
    if !has_change(tree) {
        return (lines, false);
    }
    let mut left = budget;
    walk(tree, 0, &mut left, &mut lines);
    (lines, left == 0)
}

fn tree_text(units: &Units<'_>, tree: &TreeNode, budget: usize) -> Vec<String> {
    let (lines, cut) = tree_lines(tree, budget);
    let mut out = Vec::new();
    for line in lines {
        let metric = &line.node.inclusive;
        let module = if line.node.module.is_empty() { String::new() } else { format!("  [{}]", line.node.module) };
        out.push(format!(
            "{:>12} {} {}{}{module}",
            units.function_delta(metric.delta),
            mark(metric.verdict),
            "  ".repeat(line.depth),
            line.node.name,
        ));
        if line.folded > 0 {
            out.push(format!(
                "{:>12}   {}... {} more frame{} with nothing of their own",
                "",
                "  ".repeat(line.depth + 1),
                line.folded,
                if line.folded == 1 { "" } else { "s" }
            ));
        }
    }
    if cut {
        out.push("             ...".to_string());
    }
    out
}

/// The whole report.
pub fn render(diff: &Diff, options: &TextOptions) -> String {
    let markdown = options.markdown;
    let units = Units::new(diff);
    let mut out = String::new();
    let heading = |out: &mut String, text: &str| {
        let _ = if markdown { writeln!(out, "\n### {text}\n") } else { writeln!(out, "\n{text}") };
    };
    let block = |out: &mut String, lines: &[String]| {
        if markdown {
            let _ = writeln!(out, "```\n{}\n```", lines.join("\n"));
        } else {
            let _ = writeln!(out, "{}", lines.join("\n"));
        }
    };
    let _ = if markdown { writeln!(out, "**{}**\n", headline(diff)) } else { writeln!(out, "{}\n", headline(diff)) };
    block(&mut out, &[side_line("base", &diff.base), side_line("new ", &diff.new)]);

    if let Some(cpu) = &diff.cpu {
        heading(&mut out, "All sampled time");
        let mut table = Table::new(&METRIC_COLUMNS);
        table.rows.push(metric_cells(cpu, |v| units.cpu_value(v), |d| units.cpu_delta(d)));
        table.write(&mut out, markdown);
    }

    let changed_self: Vec<(&FunctionRow, &Metric)> =
        diff.functions.iter().filter(|row| row.self_time.changed()).map(|row| (row, &row.self_time)).collect();
    // A caller is listed for what it calls only when nothing changed in
    // the function itself: its own row says more.
    let changed_inclusive: Vec<(&FunctionRow, &Metric)> = diff
        .functions
        .iter()
        .filter(|row| row.inclusive.changed() && !row.self_time.changed())
        .map(|row| (row, &row.inclusive))
        .collect();
    let more = |out: &mut String, shown: usize, all: usize| {
        if all > shown {
            let _ = writeln!(out, "{}and {} more", if markdown { "\n" } else { "  " }, all - shown);
        }
    };
    if !diff.functions.is_empty() {
        heading(&mut out, &format!("Functions, their own time ({})", units.function_heading()));
        if changed_self.is_empty() {
            let _ = writeln!(out, "{}No function's own time changed.", if markdown { "" } else { "  " });
        } else {
            let shown = &changed_self[..changed_self.len().min(options.top)];
            function_table(&units, shown).write(&mut out, markdown);
            more(&mut out, shown.len(), changed_self.len());
        }
        if !changed_inclusive.is_empty() {
            heading(&mut out, &format!("Functions, with what they call ({})", units.function_heading()));
            let shown = &changed_inclusive[..changed_inclusive.len().min(options.top)];
            function_table(&units, shown).write(&mut out, markdown);
            more(&mut out, shown.len(), changed_inclusive.len());
        }
    }

    // Frames are shown whatever they did: they are what the rest is read
    // against.
    let scope_rows: Vec<&ScopeRow> = diff
        .scopes
        .iter()
        .filter(|row| row.mean.changed() || row.total.changed() || (row.kind == SpanKind::Frame && row.mean.p.is_some()))
        .collect();
    if !diff.scopes.is_empty() {
        heading(&mut out, "Scopes (mean time per call, and in all)");
        if scope_rows.is_empty() {
            let _ = writeln!(out, "{}No scope changed.", if markdown { "" } else { "  " });
        } else {
            let shown = &scope_rows[..scope_rows.len().min(options.top)];
            scope_table(&units, shown).write(&mut out, markdown);
            more(&mut out, shown.len(), scope_rows.len());
            heading(&mut out, "Scopes, the middle and the slow end (time per call)");
            quantile_table(shown).write(&mut out, markdown);
        }
    }

    if let Some(tree) = diff.tree.as_ref().filter(|tree| has_change(tree)) {
        heading(&mut out, &format!("Where in the call tree ({}, with callees)", units.function_heading()));
        block(&mut out, &tree_text(&units, tree, options.top * 3));
    }

    heading(&mut out, "How to read this");
    let method = &diff.method;
    let confidence = method.confidence * 100.0;
    let mut notes = vec![
        format!(
            "▲ slower, ▼ faster, = tested and not changed. Intervals are at {confidence:.0}% confidence; \
             q is the p-value adjusted for every comparison made here, and a verdict needs q at or under \
             {:.2} and a change of at least {:.0}%.",
            1.0 - method.confidence,
            method.threshold * 100.0
        ),
        format!(
            "Rows under {:.1}% of both sides are not tested. Verdicts are those of means; medians and p95 are \
             shown with their intervals.",
            method.min_share * 100.0
        ),
    ];
    notes.extend(method.notes.iter().cloned());
    for note in notes {
        let _ = writeln!(out, "{}{note}", if markdown { "- " } else { "  " });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figures_read_in_the_unit_that_suits_them() {
        assert_eq!(duration(950.0), "950 ns");
        assert_eq!(duration(1_500.0), "1.50 µs");
        assert_eq!(duration(16_600_000.0), "16.60 ms");
        assert_eq!(duration(2.5e9), "2.50 s");
        assert_eq!(duration(-1_500_000.0), "-1.50 ms");
        assert_eq!(percent(0.1234), "+12.3%");
        assert_eq!(percent(-0.5), "-50.0%");
        assert_eq!(q_value(Some(0.0004)), "<0.001");
        assert_eq!(q_value(Some(0.0312)), "0.031");
        assert_eq!(q_value(None), "");
    }

    fn node(name: &str, samples: (u64, u64), verdict: Verdict, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            name: name.to_string(),
            module: String::new(),
            samples,
            inclusive: Metric { delta: 0.01, verdict, ..Metric::default() },
            children,
        }
    }

    #[test]
    fn a_run_of_frames_that_only_call_the_next_is_folded_and_what_did_not_change_is_left_out() {
        let slow = Verdict::Regressed;
        let leaf = node("solve", (10, 30), slow, vec![]);
        let chain = node("a", (20, 40), slow, vec![node("b", (20, 40), slow, vec![node("c", (20, 40), slow, vec![node("d", (20, 40), slow, vec![leaf])])])]);
        let quiet = node("idle", (50, 50), Verdict::Unchanged, vec![]);
        let root = node("all samples", (70, 90), Verdict::Untested, vec![chain, quiet]);
        let diff = Diff { method: crate::model::Method { function_unit: "share".into(), ..Default::default() }, ..Default::default() };
        let lines = tree_text(&Units::new(&diff), &root, 50);
        let names: Vec<&str> = lines.iter().map(|line| line.trim_start().trim_start_matches("+1.00 pp").trim()).collect();
        assert_eq!(names, ["all samples", "▲   a", "... 2 more frames with nothing of their own", "▲     d", "▲       solve"], "{lines:#?}");
        let (structured, cut) = tree_lines(&root, 50);
        assert_eq!(structured.iter().map(|line| (line.depth, line.node.name.as_str(), line.folded)).collect::<Vec<_>>(), [(0, "all samples", 0), (1, "a", 2), (2, "d", 0), (3, "solve", 0)]);
        assert!(!cut);
        // The budget cuts it short, and says so.
        let (short, cut) = tree_lines(&root, 2);
        assert_eq!((short.len(), cut), (2, true));
        // Nothing changed: nothing to show.
        assert!(tree_lines(&quiet_only(), 50).0.is_empty());
    }

    fn quiet_only() -> TreeNode {
        node("all samples", (5, 5), Verdict::Untested, vec![node("idle", (5, 5), Verdict::Unchanged, vec![])])
    }

    #[test]
    fn a_scope_that_one_side_never_called_has_a_dash_for_its_time() {
        assert_eq!(per_call(0.0, 0), "-");
        assert_eq!(per_call(1_500.0, 3), "1.50 µs");
        let quantile = Quantile { base: 0.0, new: 2e6, change: None, change_ci: None };
        assert_eq!(quantile_cell(&quantile, (0, 12)), "- -> 2.00 ms");
        let both = Quantile { base: 1e6, new: 2e6, change: Some(1.0), change_ci: Some((0.8, 1.2)) };
        assert_eq!(quantile_cell(&both, (5, 5)), "1.00 ms -> 2.00 ms +100.0% [+80.0%, +120.0%]");
    }

    #[test]
    fn a_table_is_aligned_for_a_terminal_and_piped_for_markdown() {
        let mut table = Table::new(&[("name", false), ("value", true)]);
        table.rows.push(vec!["a|b".to_string(), "1".to_string()]);
        table.rows.push(vec!["longer".to_string(), "100".to_string()]);
        let mut text = String::new();
        table.write(&mut text, false);
        assert_eq!(text, "  name    value\n  a|b         1\n  longer    100\n");
        let mut markdown = String::new();
        table.write(&mut markdown, true);
        assert_eq!(markdown, "| name | value |\n| --- | ---: |\n| a\\|b | 1 |\n| longer | 100 |\n");
    }
}
