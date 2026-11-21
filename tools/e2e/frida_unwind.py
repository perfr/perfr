#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Linux x86-64 live regression: real DWARF samples across recursive Gum hooks.
Requires pyarrow. Builds no code; pass matching service/agent/helper and fixture.
Never changes perf/ptrace policy. Missing sampling is a failure, not a pass.
"""

import argparse, io, json, os, socket, subprocess, tempfile, time, urllib.request, zipfile
from pathlib import Path
import pyarrow.parquet as pq
from frida_smoke import read_line_within


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for x in ("service", "agent", "helper", "target", "out"):
        p.add_argument("--" + x, required=True)
    a = p.parse_args()
    out = Path(a.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    base = f"http://127.0.0.1:{port}"

    def req(path, body=None):
        r = urllib.request.Request(
            base + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(r, timeout=60) as f:
            data = f.read()
            return json.loads(data) if data.startswith((b"{", b"[")) else data

    with (out / "service.log").open("w") as log:
        service = subprocess.Popen(
            [
                str(Path(a.service).resolve()),
                "--host",
                "127.0.0.1",
                "--serve",
                str(port),
            ],
            env=dict(
                os.environ,
                PERFR_LIB=str(Path(a.agent).resolve()),
                PERFR_HOOK=str(Path(a.helper).resolve()),
            ),
            stdout=log,
            stderr=log,
        )
        target = subprocess.Popen(
            [str(Path(a.target).resolve())],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=log,
            text=True,
        )
        try:
            pid = int(read_line_within(target, 10))
            for _ in range(100):
                try:
                    req("/api/status")
                    break
                except OSError:
                    time.sleep(0.1)
            req("/api/symbols/load", {"pid": pid})
            deadline = time.monotonic() + 60
            while req(f"/api/symbols/status?pid={pid}")["status"] != "ready":
                assert time.monotonic() < deadline
                time.sleep(0.1)
            functions = req(
                f"/api/functions/search?pid={pid}&q=perfr_unwind_&limit=30"
            )["functions"]
            hooks = [
                f
                for f in functions
                if f["name"].split("(")[0]
                in ("perfr_unwind_recursive", "perfr_unwind_leaf")
            ]
            assert len(hooks) == 2, functions
            results = []
            for iteration, mode in enumerate(("none", "frida", "frida")):
                name = f"{iteration}-{mode}"
                req(
                    "/api/capture/start",
                    {
                        "pid": pid,
                        "sampling": True,
                        "samples_per_second": 1000,
                        "context_switches": False,
                        "thread_states": False,
                        "machine_health": False,
                        "dynamic_instrumentation_method": (
                            "frida" if mode == "none" else mode
                        ),
                        "instrumented_functions": (
                            []
                            if mode == "none"
                            else [{"function_id": f["function_id"]} for f in hooks]
                        ),
                    },
                )
                target.stdin.write("g")
                target.stdin.flush()
                assert read_line_within(target, 30) == "done"
                time.sleep(0.15)
                req("/api/capture/stop", {})
                stacks = req("/api/sampling/callstacks?limit=32")
                while len(stacks["stacks"]) < stacks["total_stacks"]:
                    page = req(
                        f"/api/sampling/callstacks?limit=32&offset={len(stacks['stacks'])}"
                    )
                    assert page["stacks"]
                    stacks["stacks"].extend(page["stacks"])
                (out / (name + "-stacks.json")).write_text(json.dumps(stacks, indent=2))
                (out / (name + "-status.json")).write_text(
                    json.dumps(req("/api/status"), indent=2)
                )
                # Only samples actually executing the leaf; ignore legitimate setup/teardown samples.
                selected = [
                    s
                    for s in stacks["stacks"]
                    if "perfr_unwind_leaf" in s["frames"][-1]
                ]
                total = sum(s["count"] for s in selected)
                complete = sum(
                    s["count"]
                    for s in selected
                    if any("perfr_unwind_worker" in f for f in s["frames"])
                    and sum("perfr_unwind_recursive" in f for f in s["frames"]) == 4
                    and any(
                        "perfr_unwind_alpha" in f or "perfr_unwind_beta" in f
                        for f in s["frames"]
                    )
                )
                # The same hooked function has different callers on each
                # thread. A single trampoline-to-caller map must not pass.
                callers = {}
                for stack in selected:
                    for caller in ("perfr_unwind_alpha", "perfr_unwind_beta"):
                        if caller in stack["frames"]:
                            callers.setdefault(stack["tid"], set()).add(caller)
                assert len(callers) == 2, (name, callers)
                assert all(len(names) == 1 for names in callers.values()), (name, callers)
                assert set().union(*callers.values()) == {"perfr_unwind_alpha", "perfr_unwind_beta"}, (name, callers)
                assert total >= 30, (
                    name,
                    "sampling unavailable or insufficient",
                    stacks,
                )
                bundle = req("/api/capture/export?format=bundle")
                (out / (name + ".perfr.zip")).write_bytes(bundle)
                with zipfile.ZipFile(io.BytesIO(bundle)) as z:
                    manifest = json.loads(z.read("manifest.json"))
                    rows = pq.read_table(
                        io.BytesIO(z.read(manifest["files"]["events"]))
                    ).to_pylist()
                durations = [
                    r
                    for r in rows
                    if r["pid"] == pid
                    and r["kind"] == 1
                    and r["name"].startswith("perfr_unwind_")
                ]
                if mode == "frida":
                    assert len(durations) == 480, (name, len(durations))
                    assert all(r["duration_ns"] > 0 for r in durations)
                    assert sum("recursive" in r["name"] for r in durations) == 384
                    leaves = [r for r in durations if "leaf" in r["name"]]
                    assert len(leaves) == 96
                    # Each timed leaf is enclosed by all four recursive calls on its thread.
                    for leaf in leaves:
                        parents = [
                            r
                            for r in durations
                            if r["tid"] == leaf["tid"]
                            and "recursive" in r["name"]
                            and r["start_ns"] <= leaf["start_ns"]
                            and r["start_ns"] + r["duration_ns"]
                            >= leaf["start_ns"] + leaf["duration_ns"]
                        ]
                        assert len(parents) == 4, (leaf, parents)
                        assert sorted(r["depth"] for r in parents) == [0, 1, 2, 3]
                    assert (
                        "return history unavailable"
                        not in req("/api/status")["instrumentation"]
                    )
                result = {
                    "timed_calls": len(durations),
                    "mode": mode,
                    "leaf_samples": total,
                    "complete_caller_samples": complete,
                    "complete_percent": round(100 * complete / total, 2),
                }
                if mode in ("none", "frida"):
                    assert complete / total > 0.95, result
                results.append(result)
                print(json.dumps(result), flush=True)
            # A return intercepted in the previous capture has no invocation record
            # in this one. It must stay partial rather than invent an outer caller.
            config = {
                "pid": pid,
                "samples_per_second": 1000,
                "context_switches": False,
                "thread_states": False,
                "machine_health": False,
                "dynamic_instrumentation_method": "frida",
                "instrumented_functions": [
                    {"function_id": f["function_id"]} for f in hooks
                ],
            }
            req("/api/capture/start", config)
            target.stdin.write("h")
            target.stdin.flush()
            assert read_line_within(target, 30) == "held"
            time.sleep(0.08)
            req("/api/capture/stop", {})
            req("/api/capture/start", config)
            time.sleep(0.12)
            target.stdin.write("r")
            target.stdin.flush()
            assert read_line_within(target, 30) == "done"
            time.sleep(0.1)
            req("/api/capture/stop", {})
            stacks = req("/api/sampling/callstacks?limit=32")
            while len(stacks["stacks"]) < stacks["total_stacks"]:
                page = req(
                    f"/api/sampling/callstacks?limit=32&offset={len(stacks['stacks'])}"
                )
                assert page["stacks"]
                stacks["stacks"].extend(page["stacks"])
            selected = [
                s for s in stacks["stacks"] if "perfr_unwind_leaf" in s["frames"][-1]
            ]
            complete = [
                s
                for s in selected
                if any("perfr_unwind_worker" in f for f in s["frames"])
                and sum("perfr_unwind_recursive" in f for f in s["frames"]) == 4
            ]
            partial = [s for s in selected if len(s["frames"]) == 1]
            assert sum(s["count"] for s in complete) >= 30, stacks
            assert sum(s["count"] for s in partial) > 0, stacks
            assert len(complete) + len(partial) == len(selected), selected
            status = req("/api/status")
            (out / "cross-capture-stacks.json").write_text(json.dumps(stacks, indent=2))
            (out / "cross-capture-status.json").write_text(json.dumps(status, indent=2))
            print(
                json.dumps(
                    {
                        "cross_capture": "pass",
                        "complete_samples": sum(s["count"] for s in complete),
                        "explicitly_partial_samples": sum(s["count"] for s in partial),
                    }
                ),
                flush=True,
            )
            (out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        finally:
            for child in (target, service):
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()


if __name__ == "__main__":
    main()
