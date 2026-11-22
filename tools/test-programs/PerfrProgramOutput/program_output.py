#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""A program that prints, and does nothing else for a profiler.

It knows nothing of perfr: no API, no scopes, no log calls. What it prints
is what a training loop prints, so a capture with Output on has its lines
as log lines of the thread that printed them.

    program_output.py [--seconds N]

The first line of standard output is `pid=<n>`, as the e2e suite's targets
print. Then, ten times a second, a line of standard output; every tenth
step a second line, and a coloured warning on standard error.
"""
import argparse
import os
import sys
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=float, default=0.0, help="0: until killed")
    args = parser.parse_args()
    print(f"pid={os.getpid()}", flush=True)
    start = time.monotonic()
    step = 0
    while args.seconds <= 0 or time.monotonic() - start < args.seconds:
        loss = 4.0 / (1.0 + step / 20.0)
        print(f"iter {step}: loss {loss:.4f}, time {15 + step % 7:.2f}ms", flush=True)
        if step % 10 == 0:
            print(f"step {step}: train loss {loss * 0.9:.4f}, val loss {loss * 1.1:.4f}", flush=True)
            print("\x1b[33mUserWarning: this line is yellow on a terminal\x1b[0m", file=sys.stderr, flush=True)
        step += 1
        time.sleep(0.1)


if __name__ == "__main__":
    main()
