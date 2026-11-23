#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""The Python twin of overhead_target.c, for manual scopes from Python.

Speaks the same lines on standard input and output, so `overhead.py` drives
it the same way. One thread only: the interpreter runs one at a time anyway.

Cases:
  plain       work(rounds)
  scope       with perfr_api.scope("overhead_scope"): work(rounds)
  decorated   a function under @perfr_api.scope("overhead_decorated")
  start_stop  h = perfr_api.start(b"overhead_start_stop"); work(rounds); perfr_api.stop(h)

`rounds` is turns of a small loop in Python, not nanoseconds.
"""
import json
import os
import sys
import time

import perfr_api


def work(rounds):
    x = 0
    for i in range(rounds):
        x ^= i
    return x


@perfr_api.scope("overhead_decorated")
def decorated(rounds):
    return work(rounds)


def batch_plain(rounds, calls):
    for _ in range(calls):
        work(rounds)


def batch_scope(rounds, calls):
    scope = perfr_api.scope
    for _ in range(calls):
        with scope("overhead_scope"):
            work(rounds)


def batch_decorated(rounds, calls):
    for _ in range(calls):
        decorated(rounds)


def batch_start_stop(rounds, calls):
    start, stop = perfr_api.start, perfr_api.stop
    for _ in range(calls):
        handle = start(b"overhead_start_stop")
        work(rounds)
        stop(handle)


CASES = {"plain": batch_plain, "scope": batch_scope, "decorated": batch_decorated, "start_stop": batch_start_stop}


def run(words):
    name, rounds, calls, batches, cpus = words[0], int(words[1]), int(words[2]), int(words[3]), words[4]
    batch = CASES.get(name)
    if batch is None or calls <= 0 or batches <= 0:
        return {"error": "unknown case, or no calls or batches"}
    pinned = False
    if hasattr(os, "sched_setaffinity"):
        try:
            os.sched_setaffinity(0, {int(cpus.split(",")[0])})
            pinned = True
        except OSError:
            pass
    batch(rounds, min(calls, 64))
    clock = time.perf_counter_ns
    ns = []
    for _ in range(batches):
        t0 = clock()
        batch(rounds, calls)
        t1 = clock()
        ns.append((t1 - t0) / calls)
    return {"case": name, "rounds": rounds, "calls": calls, "batches": batches, "threads": 1, "pinned": pinned, "ns": ns}


def main():
    print(json.dumps({"pid": os.getpid()}), flush=True)
    for line in sys.stdin:
        words = line.split()
        if not words:
            continue
        if words[0] == "quit":
            break
        if words[0] == "init":
            rc = perfr_api.init()
            reply = {"init": rc, "available": int(perfr_api.available())}
        elif words[0] == "run" and len(words) == 6:
            reply = run(words[1:])
        else:
            reply = {"error": "unknown command"}
        print(json.dumps(reply), flush=True)
    perfr_api.shutdown()


if __name__ == "__main__":
    main()
