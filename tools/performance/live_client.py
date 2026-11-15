#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Run live_bench against an isolated service; preserve raw frame/process readings."""
import argparse
import http.client
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import urllib.request

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("binary", type=Path)
p.add_argument("output", type=Path)
p.add_argument("--service", default="http://127.0.0.1:44863")
p.add_argument("--seconds", type=int, default=40)
p.add_argument("--rate", type=int, default=1_000_000)
a = p.parse_args()
binary = a.binary.resolve()
if not binary.is_file():
    p.error(f"{binary} is not a file")
a.output.mkdir(parents=True, exist_ok=True)

# What a request to the service can fail with: no answer, a late one, a
# broken one, or one that is not JSON.
UNANSWERED = (OSError, http.client.HTTPException, ValueError)


def api(path, body=None, timeout=5):
    req = urllib.request.Request(a.service + path, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read()
        return json.loads(raw) if raw else None


def process_reading(pid):
    """CPU seconds and resident bytes of `pid`, or None once it has gone."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    except (FileNotFoundError, ProcessLookupError):
        return None
    return dict(seconds=time.monotonic(), cpu_s=(int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
                rss_bytes=int(fields[21]) * os.sysconf("SC_PAGE_SIZE"))


def stop_producer():
    """Stops the producer and waits for the capture it was making to end.

    The service answers the stop before the producer has left its loop, and a
    producer that leaves after the next one has started ends that one's
    capture under it. The capture ending is what says it has left.
    """
    api("/api/bench/stop", {})
    deadline = time.monotonic() + 10
    while api("/api/status").get("capturing"):
        if time.monotonic() > deadline:
            raise SystemExit(f"a capture is still running on {a.service}: this needs a service of its own")
        time.sleep(0.05)


stop_producer()
api("/api/capture/clear", {})
child = None
affinity = []
readings = []
missed = 0
try:
    # From here on the producer is running, or may be: everything that can
    # fail is inside, so that it is stopped whatever happens.
    api("/api/bench/start", dict(threads=16, depth_min=8, depth_max=16, rate=a.rate))
    with open(a.output / "client.log", "w") as log:
        child = subprocess.Popen([str(binary), a.service, str(a.seconds),
                                  str((a.output / "frames.json").resolve())], stdout=log, stderr=log)
    (a.output / "pid").write_text(str(child.pid))
    print(f"pid={child.pid} output={a.output}", flush=True)
    affinity = sorted(os.sched_getaffinity(child.pid))
    while child.poll() is None:
        reading = process_reading(child.pid)
        if reading is None:
            break
        # A poll the service is too busy to answer costs that reading its
        # service half, not the trial: the client writes its frames only when
        # it is left to finish.
        try:
            reading.update(bench=api("/api/bench", timeout=2), service=api("/api/status", timeout=2))
        except UNANSWERED as error:
            missed += 1
            print(f"no answer from the service, reading kept without it: {error}", file=sys.stderr, flush=True)
        readings.append(reading)
        time.sleep(1)
finally:
    try:
        api("/api/bench/stop", {})
    except UNANSWERED as error:
        print(f"the producer may still be running on {a.service}: {error}", file=sys.stderr, flush=True)
    if child is not None:
        if child.poll() is None:
            child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
    (a.output / "process.json").write_text(json.dumps(readings, indent=2))
    (a.output / "command.json").write_text(json.dumps(dict(
        binary=str(binary), service=a.service, seconds=a.seconds, rate=a.rate, affinity=affinity,
        missed_polls=missed, returncode=None if child is None else child.returncode), indent=2))
print(f"exit={child.returncode} missed_polls={missed}", flush=True)
raise SystemExit(child.returncode)
