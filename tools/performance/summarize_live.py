#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Summarize live_client.py runs, excluding startup; retain raw input files."""
import argparse
import json
import statistics
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runs", type=Path, nargs="+")
parser.add_argument("--warmup", type=float, default=15)
args = parser.parse_args()


class Unusable(Exception):
    """A run with too little in it to summarize."""


def quantile(values, fraction):
    values = sorted(values)
    return values[round((len(values) - 1) * fraction)]


def summarize(path, warmup):
    try:
        frames = json.loads((path / "frames.json").read_text())
        process = json.loads((path / "process.json").read_text())
    except (OSError, ValueError) as error:
        raise Unusable(f"cannot be read: {error}") from error
    # Process polling starts just before the window; discard an extra second
    # so both windows are past warmup. Do not treat update time as full-frame
    # CPU time: egui tessellation and GPU submission occur outside update().
    readings = [r for r in process if r["seconds"] >= process[0]["seconds"] + warmup + 1]
    samples = [r for r in frames["frames"] if r[0] >= warmup]
    counters = [r["state"] for r in frames["counters"] if r["seconds"] >= warmup]
    if len(readings) < 2 or readings[-1]["seconds"] <= readings[0]["seconds"]:
        raise Unusable(f"{len(readings)} process readings after a {warmup:g} s warmup, and CPU use needs two")
    if not samples or not counters:
        raise Unusable(f"{len(samples)} frames and {len(counters)} counter readings after a {warmup:g} s warmup")
    first, last = readings[0], readings[-1]
    # Every frame's reading where the run has them. A run from before the
    # harness kept them has the once-a-second counter readings only:
    # gpu_prepare_readings says how many values the figures stand on.
    if "gpu_prepare_us" in frames["frame_columns"]:
        column = frames["frame_columns"].index("gpu_prepare_us")
        prepare = [r[column] for r in samples]
    else:
        prepare = [r["gpu_prepare_us"] for r in counters]
    achieved = [r["bench"]["achieved"] for r in readings if "bench" in r]
    return dict(
        run=str(path), warmup_seconds=warmup, frames=len(samples),
        cpu_cores=(last["cpu_s"] - first["cpu_s"]) / (last["seconds"] - first["seconds"]),
        update_p50_ms=quantile([r[2] for r in samples], .5),
        update_p95_ms=quantile([r[2] for r in samples], .95),
        period_p95_ms=quantile([r[1] for r in samples], .95),
        gpu_prepare_mean_us=statistics.mean(prepare),
        gpu_prepare_p50_us=quantile(prepare, .5),
        gpu_prepare_p95_us=quantile(prepare, .95),
        gpu_prepare_readings=len(prepare),
        queue_max_bytes=max(r["queued_bytes"] for r in counters),
        retained_events_last=counters[-1]["events"],
        rss_mean_mb=statistics.mean(r["rss_bytes"] for r in readings) / 1e6,
        achieved_events_per_second=statistics.mean(achieved) if achieved else None,
        readings_without_service=len(readings) - len(achieved),
        errors=sorted(set(r["error"] for r in counters) - {""}),
    )


results = []
unusable = 0
for path in args.runs:
    try:
        results.append(summarize(path, args.warmup))
    except Unusable as error:
        unusable += 1
        print(f"{path}: {error}", file=sys.stderr)
print(json.dumps(results, indent=2))
raise SystemExit(1 if unusable else 0)
