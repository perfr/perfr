#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Python program to be profiled from outside, with its own scopes beside.

It has what a capture of a Python program should show: threads that take
turns on the GIL, a thread pool with Python's names for its workers, a
callback from C (`sorted` with a key), a C accelerator (`json`), recursion, a
method of a class, and a thread that mostly sleeps.

It is also instrumented by hand with perfr's Python API (`perfr_api`), so
that one capture has both what the program says of itself and what the
samples find: a scope around each piece of work, the turns of the main thread
as frames, a value to graph, an arrow from the thread that feeds the pool to
each job, and a log line now and then. The API does nothing until `init()`
finds perfr's library, and `--plain` does not call it: the program then runs
as one with nothing of perfr in it, which is what the samples need.

    python3 python_stacks.py [--seconds N] [--plain]   # 0 seconds, the default, runs until killed

The first line it prints is `pid=<n>`.
"""

import argparse
import json
import math
import os
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

# The shippable package lives beside the header it mirrors; pip installs it
# as perfr-api. In the tree, import it from there.
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                "../../../crates/perfr-api/python"))
import perfr_api as perfr  # noqa: E402


def crunch(n):
    total = 0.0
    for i in range(n):
        total += math.sqrt(i)
    return total


def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)


class Report:
    def __init__(self, rows):
        self.rows = rows

    def render(self):
        return json.dumps(self.rows)

    def ranked(self):
        return sorted(self.rows, key=lambda row: -row["score"])


def build_report():
    with perfr.scope("make rows"):
        rows = [{"name": f"row {i}", "score": (i * 7919) % 1000} for i in range(4000)]
    report = Report(rows)
    with perfr.scope("rank"):
        report.ranked()
    with perfr.scope("render"):
        return len(report.render())


def worker_loop(frames=False, until=None):
    """Turns of the same work, until `until` on `time.monotonic()`'s clock or
    for good. With `frames`, each turn is a frame of the capture."""
    while until is None or time.monotonic() < until:
        if frames:
            perfr.frame_mark()
        began = time.perf_counter()
        with perfr.scope("turn"):
            with perfr.scope("crunch"):
                crunch(40000)
            with perfr.scope("build report"):
                build_report()
        # How long a turn takes is how long this thread waited for the GIL.
        perfr.value("turn ms", (time.perf_counter() - began) * 1e3)


def recurse_loop():
    while True:
        with perfr.scope("fib(20)"):
            fib(20)


def sleeper():
    while True:
        time.sleep(0.02)
        with perfr.scope("wake"):
            crunch(2000)


def pool_job(job):
    n, submitted = job
    with perfr.scope("pool job") as running:
        # An arrow from where the feeder submitted the job to where it runs.
        perfr.link(submitted, running.handle)
        return crunch(n)


def pool_loop():
    with ThreadPoolExecutor(max_workers=2, thread_name_prefix="pool") as pool:
        batches = 0
        while True:
            with perfr.scope("feed the pool"):
                jobs = [(20000, perfr.instant("submit")) for _ in range(4)]
                list(pool.map(pool_job, jobs))
            batches += 1
            if batches % 10 == 0:
                perfr.log(perfr.INFO, f"{batches * 4} pool jobs done")
            time.sleep(0.01)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seconds", type=float, default=0.0, help="how long to run; 0 is until killed")
    parser.add_argument("--plain", action="store_true",
                        help="leave perfr's API unloaded: every instrumentation call does nothing")
    args = parser.parse_args()
    print(f"pid={os.getpid()}", flush=True)
    if not args.plain:
        rc = perfr.init()
        if rc != 0:
            print(f"python_stacks: perfr.init() failed ({rc}); running uninstrumented", file=sys.stderr)
    for target, name in (
        (worker_loop, "report-builder"),
        (recurse_loop, "fibonacci"),
        (sleeper, "sleeper"),
        (pool_loop, "pool-feeder"),
    ):
        threading.Thread(target=target, name=name, daemon=True).start()
    # The main thread works too; with a time to stop at, it then lets the
    # daemon threads die with it.
    worker_loop(frames=True, until=time.monotonic() + args.seconds if args.seconds > 0 else None)


if __name__ == "__main__":
    main()
