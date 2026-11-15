#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Summarize live_client.py runs, excluding startup; retain raw input files."""
import argparse
import json
import statistics
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runs", type=Path, nargs="+")
parser.add_argument("--warmup", type=float, default=15)
args = parser.parse_args()

def quantile(values, fraction):
    values = sorted(values)
    return values[round((len(values) - 1) * fraction)]

results = []
for path in args.runs:
    frames = json.loads((path / "frames.json").read_text())
    process = json.loads((path / "process.json").read_text())
    # Process polling starts just before the window; discard an extra second
    # so both windows are past warmup. Do not treat update time as full-frame
    # CPU time: egui tessellation and GPU submission occur outside update().
    readings = [r for r in process if r["seconds"] >= process[0]["seconds"] + args.warmup + 1]
    samples = [r for r in frames["frames"] if r[0] >= args.warmup]
    counters = [r["state"] for r in frames["counters"] if r["seconds"] >= args.warmup]
    first, last = readings[0], readings[-1]
    results.append(dict(
        run=str(path), warmup_seconds=args.warmup, frames=len(samples),
        cpu_cores=(last["cpu_s"] - first["cpu_s"]) / (last["seconds"] - first["seconds"]),
        update_p50_ms=quantile([r[2] for r in samples], .5),
        update_p95_ms=quantile([r[2] for r in samples], .95),
        period_p95_ms=quantile([r[1] for r in samples], .95),
        gpu_prepare_mean_us=statistics.mean(r["gpu_prepare_us"] for r in counters),
        queue_max_bytes=max(r["queued_bytes"] for r in counters),
        retained_events_last=counters[-1]["events"],
        rss_mean_mb=statistics.mean(r["rss_bytes"] for r in readings) / 1e6,
        achieved_events_per_second=statistics.mean(r["bench"]["achieved"] for r in readings),
        errors=sorted(set(r["error"] for r in counters) - {""}),
    ))
print(json.dumps(results, indent=2))
