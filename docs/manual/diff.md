<!--
Copyright (c) 2026 The perfr Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# Comparing two captures

`perfr diff` says what changed between two saved captures, and how sure that is. The same comparison is the **Compare** tab of the viewer, `POST /api/diff` on the service and the `perfr_diff` tool of the MCP server.

```
perfr diff before.perfr.zip after.perfr.zip
```

The first line is the verdict: `REGRESSED`, `IMPROVED`, `NO CHANGE` or `INCONCLUSIVE`. Under it:

- **All sampled time**: cores busy, or CPU time per unit of work.
- **Functions**, by their own time and with what they call.
- **Scopes**: manual scopes, hooked functions, frame series, async tracks and GPU work, by mean time per call and by what they cost in all. The median and the 95th percentile are listed with their intervals.
- **Where in the call tree** the change is. Both captures are folded into one tree. Only the paths that lead to a change are shown, and a run of frames that each only call the next is shown as its first frame and its last.

Every row has the value in the base and in the new capture, the change, the interval of the change and `q`. `▲` is slower, `▼` is faster, `=` is tested and not changed; the viewer draws the same marks in red and green.

Scheduling, thread states, values and log lines are not compared. A capture of a process is compared for that process, the processes sampled with it, and the scopes an agent marked over HTTP; the scopes of other programs that happened to be instrumented while it was taken, the perfr service among them, are left out.

## What a verdict means

A row is **slower** or **faster** when both of these hold:

- The difference is beyond chance: `q` is at or under 0.05 (`--confidence 0.95`). `q` is the p-value adjusted for every comparison the report makes (Benjamini-Hochberg), so of the rows a report calls changed, about one in twenty is expected to be a false finding, however many rows it tested.
- The change is at least 5% (`--threshold 5`).

Rows that are under 0.5% of both captures are not tested (`--min-share 0.5`), and neither is a function with fewer than 10 samples or a scope with fewer than 5 calls or batches on a side. They are listed with their difference and no verdict.

Verdicts are those of means. A median says nothing of a slow path that one call in a hundred takes, and time is what a program is charged for.

## How chance is measured

Two samples taken a millisecond apart are of the same function more often than not, and a call is slow when the call before it was. Counting each sample or call as an independent observation makes the intervals far too narrow: in the simulation in `crates/perfr-diff/src/stats.rs`, a test that does so calls a difference between two captures of the same thing more than half the time.

So the unit of every comparison is a stretch of a capture, never one sample or one call:

- A capture's time is cut into 32 equal **batches**. A function's figure is its samples summed over the batches, divided by what they are counted against summed over the batches, and its error comes from how much the batches disagree.
- A scope's calls are cut into 32 batches in the order they were made, once it has 64. With fewer, each call is a unit.
- Two sides are compared with Welch's t-test. Quantiles are compared with a bootstrap that redraws whole batches.

With one capture a side, the intervals are those of these two captures. They say whether the captures differ, not whether two builds do: the same build run twice differs by a few percent, from the machine's state, the layout of memory, the frequency of the cores.

With **three or more captures a side**, each capture is one unit, and the intervals include what differs from run to run:

```
perfr diff --base main-1.perfr.zip main-2.perfr.zip main-3.perfr.zip \
           --new pr-1.perfr.zip pr-2.perfr.zip pr-3.perfr.zip
```

That is the form for a regression test.

## What time is counted against

`--per` chooses it. The right choice depends on what the two captures have in common.

| `--per` | A function's figure | Right when |
|---|---|---|
| `second` (default) | CPU time per second of capture | The program does the same work every second in both: a game at its frame rate, a server under the same load |
| `capture` | CPU time in the whole capture | Both captures are of the same amount of work, start to end: a benchmark, a test, a batch job |
| a scope or frame series, such as `frame` | CPU time per call of it | The program names its unit of work |
| `sample` | Share of all samples | Reading the two sampling reports side by side |

A program that runs flat out does less work in a second when one function gets slower, so per second every other function looks faster. Shares have the same fault always: they add up to 100%. Counted per capture or per frame, only what got slower is slower. The report says which it used, and what to mind.

A scope's total is counted the same way. Counted per frame, a scope that is called twice as often per frame has the same time per call and twice the total, and is reported slower.

## In the viewer

Open the report panel and its **Compare** tab, then **Baseline…** and pick a `.perfr.zip`. The capture on screen is judged against it. **This capture is the base** turns that round.

**per second**, **per capture**, **per frame** and **shares** choose what time is counted against. **Changed only** leaves out the rows without a verdict; switch it off to see every row. The panel's filter narrows the rows by name. Hover a row for the figures that do not fit on it. Click a function to open this capture's Flat report on it.

The tab needs the service: it reads both captures. A capture that is still running is compared as far as it has got; **Compare again** asks once more.

## As a regression test

```
perfr diff --base main-*.perfr.zip --new pr-*.perfr.zip --per capture --fail-on-regression
```

The exit status is 0 when nothing got slower, 1 when something did, and 2 when the comparison could not be made. Getting faster is not a failure. `--markdown` prints the report for a pull request comment, and `--json` prints all of it: every tested row, the largest 200 of the others, and the tree.

Capture the same work on both sides, on the same machine, and take at least three captures a side. `--threshold` is the smallest change worth failing for; chance is already accounted for, so it does not need to be padded for noise.

## Functions that moved

A Python function is named with the line it starts at: `step (train.py:137)`. An edit above it changes the line. When a name is gone from the base and new in the new capture at another line of the same file, and the file has as many of that name left over on both sides, it is taken for the same function. The report says how many were matched this way.

A native function without symbols is named `module+offset`, which is the same across runs of one binary and not across two builds of it. Compare builds with their symbols.

## From the service and from an agent

`POST /api/diff` takes a `.perfr.zip` as its body and compares it with the capture the service holds. The query takes `name` (what the body is called in the answer), `per`, `confidence`, `threshold` and `min_share` (both in percent), and `current=base` to make the service's capture the base. The answer is the JSON `perfr diff --json` prints. The capture the service holds is not changed.

`perfr_diff` is the same for an agent: `base` and `new` are paths, or lists of paths; without `new`, `base` is compared with the capture the service holds. See [MCP](mcp.md).
