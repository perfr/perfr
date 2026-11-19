#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Python program with nothing of perfr in it, to be profiled from outside.

It has what a capture of a Python program should show: threads that take
turns on the GIL, a thread pool with Python's names for its workers, a
callback from C (`sorted` with a key), a C accelerator (`json`), recursion, a
method of a class, and a thread that mostly sleeps.

    python3 python_stacks.py [--seconds N]      # 0, the default, runs until killed

The first line it prints is `pid=<n>`.
"""

import argparse
import json
import math
import os
import threading
import time
from concurrent.futures import ThreadPoolExecutor


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
    rows = [{"name": f"row {i}", "score": (i * 7919) % 1000} for i in range(4000)]
    report = Report(rows)
    report.ranked()
    return len(report.render())


def worker_loop():
    while True:
        crunch(40000)
        build_report()


def recurse_loop():
    while True:
        fib(20)


def sleeper():
    while True:
        time.sleep(0.02)
        crunch(2000)


def pool_job(n):
    return crunch(n)


def pool_loop():
    with ThreadPoolExecutor(max_workers=2, thread_name_prefix="pool") as pool:
        while True:
            list(pool.map(pool_job, [20000] * 4))
            time.sleep(0.01)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--seconds", type=float, default=0.0, help="how long to run; 0 is until killed")
    args = parser.parse_args()
    print(f"pid={os.getpid()}", flush=True)
    for target, name in (
        (worker_loop, "report-builder"),
        (recurse_loop, "fibonacci"),
        (sleeper, "sleeper"),
        (pool_loop, "pool-feeder"),
    ):
        threading.Thread(target=target, name=name, daemon=True).start()
    if args.seconds > 0:
        # The main thread works too, then lets the daemon threads die with it.
        deadline = time.monotonic() + args.seconds
        while time.monotonic() < deadline:
            crunch(40000)
            build_report()
    else:
        worker_loop()


if __name__ == "__main__":
    main()
