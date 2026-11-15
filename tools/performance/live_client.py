#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Run live_bench against an isolated service; preserve raw frame/process readings."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request

p = argparse.ArgumentParser()
p.add_argument("binary", type=Path)
p.add_argument("output", type=Path)
p.add_argument("--service", default="http://127.0.0.1:44863")
p.add_argument("--seconds", type=int, default=40)
p.add_argument("--rate", type=int, default=1_000_000)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=True)
def api(path, body=None):
    req = urllib.request.Request(a.service + path, data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=5) as r:
        raw = r.read()
        return json.loads(raw) if raw else None
api("/api/bench/stop", {})
api("/api/capture/clear", {})
api("/api/bench/start", dict(threads=16, depth_min=8, depth_max=16, rate=a.rate))
log = open(a.output / "client.log", "w")
child = subprocess.Popen([str(a.binary.resolve()), a.service, str(a.seconds),
                          str((a.output / "frames.json").resolve())], stdout=log, stderr=log)
(a.output / "pid").write_text(str(child.pid))
print(f"pid={child.pid} output={a.output}", flush=True)
affinity = sorted(os.sched_getaffinity(child.pid))
hz = os.sysconf("SC_CLK_TCK")
readings = []
try:
    while child.poll() is None:
        try:
            fields = Path(f"/proc/{child.pid}/stat").read_text().rsplit(")", 1)[1].split()
            readings.append(dict(seconds=time.monotonic(), cpu_s=(int(fields[11])+int(fields[12]))/hz,
                                 rss_bytes=int(fields[21])*os.sysconf("SC_PAGE_SIZE"),
                                 bench=api("/api/bench"), service=api("/api/status")))
        except FileNotFoundError:
            break
        time.sleep(1)
finally:
    api("/api/bench/stop", {})
    if child.poll() is None:
        child.terminate()
    child.wait(timeout=10)
    (a.output / "process.json").write_text(json.dumps(readings, indent=2))
    (a.output / "command.json").write_text(json.dumps(dict(binary=str(a.binary.resolve()),
        service=a.service, seconds=a.seconds, rate=a.rate, affinity=affinity, returncode=child.returncode), indent=2))
print(f"exit={child.returncode}", flush=True)
raise SystemExit(child.returncode)
