#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""What instrumentation costs the program it is in.

Measures, per call, a manual scope (`perfr_start` + `perfr_stop`) and a hooked
function, against the same call with neither, in each state a program can be
in: perfr not there, there and idle, capturing, capturing with the hook
switched off by auto-unhook, and after the capture.

    cargo build --release -p perfr -p perfr-lib -p perfr-hook -p perfr-api
    python3 tools/performance/overhead/overhead.py --out /tmp/overhead

Starts its own service on a free port; nothing else on the machine is
touched. Writes `overhead.md` (the tables) and `overhead.json` (every batch)
into --out, prints the tables, and exits 1 when a check fails. The checks are
about the measurement being one: that the scopes and calls counted were in
fact recorded, that a muted hook was muted, that a hook is gone after Stop.
`--max-scope-ns` and `--max-hook-ns` add a budget, for a machine that runs
this regularly.

Standard library only. See README.md beside this file for how to read the
numbers.
"""
import argparse
import json
import os
import platform
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent

# What the driver hooks, and what the scope cases are called: the names the
# service's function statistics list them under.
HOOKED = "overhead_hooked"
RECORDED_AS = {
    "hooked": HOOKED,
    "scope": "overhead_scope",
    "scope_long": "overhead_scope_long::a_name_as_long_as_a_cpp_signature<with, template, arguments>(and, parameters)",
    "decorated": "overhead_decorated",
    "start_stop": "overhead_start_stop",
}

# The limit auto-unhook ships with (`DEFAULT_MAX_HOOK_CALLS_PER_S`). Sent with
# the capture so the run does not depend on the settings of whoever runs it.
DEFAULT_CALL_LIMIT = 100_000


class Failure(Exception):
    pass


def performance_cpus():
    """One logical CPU per physical core, performance cores only where the
    processor has two kinds, and the core of CPU 0 last: it is the one the
    machine's interrupts land on."""
    base = Path("/sys/devices/system/cpu")
    hybrid = Path("/sys/devices/cpu_core/cpus")
    def parse(text):
        cpus = []
        for part in text.strip().split(","):
            if "-" in part:
                a, b = part.split("-")
                cpus.extend(range(int(a), int(b) + 1))
            elif part:
                cpus.append(int(part))
        return cpus
    try:
        cpus = parse(hybrid.read_text()) if hybrid.exists() else parse((base / "online").read_text())
    except OSError:
        return list(range(os.cpu_count() or 1))
    allowed = os.sched_getaffinity(0) if hasattr(os, "sched_getaffinity") else set(cpus)
    cores = {}
    for cpu in sorted(c for c in cpus if c in allowed):
        topology = base / f"cpu{cpu}" / "topology"
        try:
            key = ((topology / "physical_package_id").read_text().strip(), (topology / "core_id").read_text().strip())
        except OSError:
            key = ("?", str(cpu))
        cores.setdefault(key, cpu)
    picked = sorted(cores.values())
    if len(picked) > 1 and picked[0] == 0:
        picked = picked[1:] + [0]
    return picked or [0]


def machine(cpus):
    def read(path):
        try:
            return Path(path).read_text().strip()
        except OSError:
            return None
    model = None
    for line in (read("/proc/cpuinfo") or "").splitlines():
        if line.startswith("model name"):
            model = line.split(":", 1)[1].strip()
            break
    def run(argv):
        try:
            return subprocess.run(argv, capture_output=True, text=True, timeout=10).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return None
    return {
        "cpu": model or platform.processor(),
        "logical_cpus": os.cpu_count(),
        "cpus_measured_on": cpus,
        "governor": read(f"/sys/devices/system/cpu/cpu{cpus[0]}/cpufreq/scaling_governor"),
        "kernel": platform.release(),
        "system": platform.system(),
        "python": platform.python_version(),
        "perfr_commit": run(["git", "-C", str(REPO), "rev-parse", "--short", "HEAD"]),
    }


class Service:
    """A perfr service of this run's own, on a free loopback port, with its
    settings in a directory of this run's own."""

    def __init__(self, args, out):
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self.base = f"http://127.0.0.1:{self.port}"
        self.env = dict(os.environ, PERFR_LIB=args.agent, PERFR_HOOK=args.helper, XDG_CONFIG_HOME=str(out / "config"))
        (out / "config").mkdir(parents=True, exist_ok=True)
        self.binary = args.service
        argv = [args.service, "--host", "127.0.0.1", "--serve", str(self.port)]
        if args.sudo:
            # The wrapper tools/sudo/install.sh installs, as the e2e suite
            # uses it: uprobes need capabilities an ordinary user has not got.
            # sudo does not pass this run's environment on, so the service
            # then finds libperfr and perfr-hook beside itself and keeps its
            # settings where the user's are.
            wrapper = args.sudo_wrapper or "/usr/local/bin/perfr-sudo"
            argv = ["sudo", "-n", "--"] + ([wrapper] if os.path.exists(wrapper) else []) + argv
        self.log = open(out / "service.log", "w")
        self.process = subprocess.Popen(argv, env=self.env, stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 30
        while True:
            try:
                self.request("/api/status", timeout=1)
                break
            except OSError:
                if self.process.poll() is not None or time.monotonic() > deadline:
                    raise Failure(f"the service did not come up; see {out / 'service.log'}")
                time.sleep(0.1)

    def request(self, path, body=None, timeout=120):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=timeout) as response:
            payload = response.read()
        return json.loads(payload) if payload[:1] in (b"{", b"[") else payload

    def version(self):
        try:
            return subprocess.run([self.binary, "--version"], capture_output=True, text=True, timeout=10).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return None

    def start(self, pid, **body):
        body["pid"] = pid
        try:
            self.request("/api/capture/start", body)
        except urllib.error.HTTPError as error:
            raise Failure(f"capture start refused: {error.read().decode(errors='replace')[:400]}")

    def stop(self):
        self.request("/api/capture/stop", {})

    def status(self):
        return self.request("/api/status")

    def recorded(self, pid, name):
        """How many completed scopes or calls named `name` the capture holds."""
        query = urllib.parse.urlencode({"q": name[:40], "pid": pid, "limit": 100})
        rows = self.request(f"/api/functions/stats?{query}").get("rows", [])
        # A symbol may be listed with its argument list; the bare name is
        # what is compared.
        return sum(row["count"] for row in rows if row["name"] == name or row["name"].split("(")[0] == name)

    def function_id(self, pid, name):
        self.request("/api/symbols/load", {"pid": pid})
        deadline = time.monotonic() + 120
        while True:
            state = self.request(f"/api/symbols/status?pid={pid}")
            if state.get("status") == "ready":
                break
            if state.get("status") == "error" or time.monotonic() > deadline:
                raise Failure(f"symbols of {pid} did not load: {state}")
            time.sleep(0.1)
        found = self.request(f"/api/functions/search?pid={pid}&q={urllib.parse.quote(name)}&limit=40")["functions"]
        exact = [f for f in found if f["name"].lstrip("_").split("(")[0] == name]
        if len(exact) != 1:
            raise Failure(f"expected one function named {name}, found {[f['name'] for f in found]}")
        return exact[0]["function_id"]

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(15)
            except subprocess.TimeoutExpired:
                self.process.kill()
        self.log.close()


class Target:
    """One of the programs being measured, driven a line at a time."""

    def __init__(self, label, argv, env, cpus):
        self.label = label
        self.cpus = cpus
        self.process = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.pid = self.reply()["pid"]
        # Calls made per case since `forget`, to hold against what a
        # capture recorded.
        self.made = {}

    def reply(self):
        line = self.process.stdout.readline()
        if not line:
            raise Failure(f"{self.label} exited ({self.process.poll()})")
        reply = json.loads(line)
        if "error" in reply:
            raise Failure(f"{self.label}: {reply['error']}")
        return reply

    def command(self, line):
        self.process.stdin.write(line + "\n")
        self.process.stdin.flush()
        return self.reply()

    def init(self):
        reply = self.command("init")
        if reply["init"] != 0:
            raise Failure(f"{self.label}: perfr_init returned {reply['init']}")

    def forget(self):
        self.made = {}

    def run(self, case, rounds, calls, batches, threads):
        cpus = ",".join(str(c) for c in self.cpus[:threads])
        reply = self.command(f"run {case} {rounds} {calls} {batches} {cpus}")
        self.made[case] = self.made.get(case, 0) + reply["threads"] * (min(calls, 64) + calls * batches)
        return reply

    def calibrate(self, case, rounds, threads, opts):
        """How many calls of `case` make a batch of `opts.batch_ms`."""
        calls = 64
        while True:
            probe = self.run(case, rounds, calls, 1, threads)
            per_call = max(statistics.median(probe["ns"]), 0.01)
            if per_call * calls >= 2e6 or calls >= 1 << 30:
                break
            calls *= 8
        return max(64, int(opts.batch_ms * 1e6 / per_call))

    def measure(self, case, rounds, threads, opts):
        """Per-call nanoseconds of `case` and of the plain call, a batch of
        one and then a batch of the other, `opts.batches` times over.

        Taken in turns because what is wanted is the difference, and the
        two drift together: a capture's sampler interrupts both, the
        processor's clock speed moves under both. A batch's figure is the
        median over its threads; what the case adds is the median of the
        differences between each batch and the plain batch before it."""
        plain_calls = self.calibrate("plain", rounds, threads, opts)
        case_calls = self.calibrate(case, rounds, threads, opts)
        plain, measured, pinned = [], [], True
        for _ in range(opts.batches):
            reply = self.run("plain", rounds, plain_calls, 1, threads)
            plain.append(statistics.median(reply["ns"]))
            reply = self.run(case, rounds, case_calls, 1, threads)
            measured.append(statistics.median(reply["ns"]))
            pinned = pinned and reply["pinned"]
        added = sorted(m - p for m, p in zip(measured, plain))
        quartiles = statistics.quantiles(added, n=4) if len(added) >= 2 else [added[0]] * 3
        return {
            "plain_ns": statistics.median(plain),
            "ns": statistics.median(measured),
            "added_ns": statistics.median(added),
            "added_q1_ns": quartiles[0],
            "added_q3_ns": quartiles[2],
            "calls_per_batch": case_calls,
            "pinned": pinned,
            "plain_batches_ns": plain,
            "batches_ns": measured,
        }

    def close(self):
        try:
            self.process.stdin.write("quit\n")
            self.process.stdin.close()
            self.process.wait(10)
        except (OSError, subprocess.TimeoutExpired):
            self.process.kill()


class Run:
    def __init__(self, service, opts):
        self.service = service
        self.opts = opts
        self.rows = []
        self.failures = []

    def check(self, ok, message):
        if not ok:
            self.failures.append(message)
            print(f"  FAILED: {message}", flush=True)

    def row(self, target, state, case, rounds, threads, capture=None, expect_recorded=True, before=None):
        """Measures `case` beside the plain call, in the state the target is
        in now, or inside a capture started for it when `capture` is given
        (the body of the start request). `before` runs inside the capture,
        ahead of the measurement."""
        service, opts = self.service, self.opts
        status = ""
        recorded = None
        if capture is not None:
            service.start(target.pid, **capture)
            # The capture's own threads start, and a hook's agent settles.
            time.sleep(opts.settle)
            target.forget()
            if before is not None:
                before()
        # Outside a capture the batches of a row agree to a fraction of a
        # nanosecond, unless another perfr service on the machine starts a
        # capture meanwhile: recording is switched on in every program that
        # has the API, whichever one the capture is of. Such a row is taken
        # again.
        for attempt in range(4):
            measured = target.measure(case, rounds, threads, opts)
            spread = measured["added_q3_ns"] - measured["added_q1_ns"]
            steady = spread <= max(5.0, 0.25 * abs(measured["added_ns"]))
            if capture is not None or steady:
                break
            print(f"  {state} / {case}: the batches disagree ({measured['added_q1_ns']:.1f} to {measured['added_q3_ns']:.1f} ns); taking the row again", flush=True)
            time.sleep(2)
        if capture is None:
            self.check(steady, f"{target.label} / {state} / {case}: the batches disagree with no capture of this run's going on; is another perfr service capturing?")
        if capture is not None:
            # What the reader has not yet drained when Stop arrives is not
            # in the capture; give it the moment it needs.
            time.sleep(0.3)
            status = service.status().get("instrumentation", "")
            service.stop()
            made = target.made.get(case, 0)
            count = service.recorded(target.pid, RECORDED_AS[case])
            recorded = {"made": made, "recorded": count, "fraction": min(1.0, count / made) if made else 0.0}
            if expect_recorded:
                self.check(count > 0, f"{target.label} / {state} / {case}: the capture recorded none of {made} calls")
        self.check(measured["pinned"], f"{target.label} / {state} / {case}: the threads could not be pinned to their CPUs")
        row = {
            "target": target.label,
            "state": state,
            "case": case,
            "rounds": rounds,
            "threads": threads,
            "plain_ns": measured["plain_ns"],
            "ns": measured["ns"],
            "overhead_ns": measured["added_ns"],
            "overhead_q1_ns": measured["added_q1_ns"],
            "overhead_q3_ns": measured["added_q3_ns"],
            "recorded": recorded,
            "status": status,
            "calls_per_batch": measured["calls_per_batch"],
            "plain_batches_ns": measured["plain_batches_ns"],
            "batches_ns": measured["batches_ns"],
        }
        self.rows.append(row)
        share = "" if recorded is None else f"  recorded {recorded['fraction'] * 100:5.1f}%"
        print(
            f"  {target.label:8} {state:48} {case:10} x{threads}  plain {row['plain_ns']:10.2f} ns  "
            f"with {row['ns']:10.2f} ns  added {row['overhead_ns']:8.2f} ns "
            f"({row['overhead_q1_ns']:.2f} to {row['overhead_q3_ns']:.2f}){share}",
            flush=True,
        )
        return row


def build(opts, out):
    """The C program, twice: finding libperfr at run time, as perfr.h does
    unless told otherwise, and with the API linked in."""
    cc = shutil.which(os.environ.get("CC", "cc"))
    if cc is None:
        raise Failure("no C compiler (cc) on PATH")
    source = str(HERE / "overhead_target.c")
    common = [cc, "-O2", "-g", "-fno-omit-frame-pointer", "-I", str(REPO / "crates/perfr-api/include"), source]
    built = {}
    dynamic = out / "overhead_target"
    subprocess.run(common + ["-lpthread", "-ldl", "-o", str(dynamic)], check=True)
    built["dynamic"] = str(dynamic)
    if opts.api_lib and Path(opts.api_lib).exists():
        static = out / "overhead_target_static"
        subprocess.run(common + ["-DPERFR_STATIC", opts.api_lib, "-lpthread", "-ldl", "-lm", "-o", str(static)], check=True)
        built["static"] = str(static)
    else:
        print(f"no {opts.api_lib}: skipping the statically linked target (cargo build --release -p perfr-api)")
    return built


def manual_scopes(run, target, rounds_list, thread_counts, first_cases, python=False):
    """A manual scope, from before the API is initialised to after a capture."""
    case = first_cases[0]
    run.row(target, "API not initialised", case, 0, 1)
    target.init()
    run.row(target, "initialised, no capture", case, 0, 1)
    for rounds in rounds_list:
        run.row(target, "capturing", case, rounds, 1, capture={})
    for extra in first_cases[1:]:
        run.row(target, "capturing", extra, rounds_list[-2] if len(rounds_list) > 1 else rounds_list[0], 1, capture={})
    if not python:
        for threads in thread_counts:
            if threads > 1:
                run.row(target, "capturing", case, 1000, threads, capture={})
    after = run.row(target, "after the capture", case, 0, 1)
    idle = next(r for r in run.rows if r["target"] == target.label and r["state"] == "initialised, no capture")
    # Generous: this is "the gate closed again", not a benchmark of the gate.
    run.check(
        after["overhead_ns"] <= max(3 * idle["overhead_ns"], idle["overhead_ns"] + 20),
        f"{target.label}: a scope costs {after['overhead_ns']:.1f} ns after the capture, {idle['overhead_ns']:.1f} ns before it",
    )


def hooks(run, target, engine, label, rounds_list, thread_counts):
    """A hooked function, from before it is hooked to after the capture."""
    service = run.service
    function_id = service.function_id(target.pid, HOOKED)
    body = {"instrumented_functions": [{"function_id": function_id}], "dynamic_instrumentation_method": engine}
    run.row(target, f"{label}: not hooked", "hooked", 0, 1)
    armed = dict(body, max_hook_calls_per_s=0)
    for rounds in rounds_list:
        run.row(target, f"{label}: hooked", "hooked", rounds, 1, capture=armed)
    for threads in thread_counts:
        if threads > 1:
            run.row(target, f"{label}: hooked", "hooked", 1000, threads, capture=armed)
    # A loop this tight is past the limit, so the engine switches the hook
    # off: what is measured is what the function costs from then on. Frida's
    # agent judges a tenth of a second at a time and the kernel engine a
    # whole one, so the function is called until the status says it is off.
    limited = dict(body, max_hook_calls_per_s=DEFAULT_CALL_LIMIT)

    def trip(rounds):
        deadline = time.monotonic() + 15
        while "auto-unhooked" not in service.status().get("instrumentation", "") and time.monotonic() < deadline:
            target.run("hooked", rounds, 50_000, 1, 1)

    for rounds in (0, 1000):
        muted = run.row(
            target, f"{label}: switched off by auto-unhook", "hooked", rounds, 1,
            capture=limited, expect_recorded=False, before=lambda rounds=rounds: trip(rounds),
        )
        run.check("auto-unhooked" in muted["status"], f"{label}: the status does not say the hook was switched off: {muted['status'][:160]!r}")
    after = run.row(target, f"{label}: after the capture", "hooked", 0, 1)
    run.check(
        abs(after["overhead_ns"]) <= max(2.0, 0.5 * after["plain_ns"]),
        f"{label}: the function costs {after['ns']:.2f} ns a call after the capture against {after['plain_ns']:.2f} ns plain: is the hook still there?",
    )


def fmt_ns(value):
    if abs(value) < 0.005:
        return "0.00"
    if abs(value) >= 1000:
        return f"{value:,.0f}"
    if abs(value) >= 100:
        return f"{value:.0f}"
    if abs(value) >= 10:
        return f"{value:.1f}"
    return f"{value:.2f}"


def table(rows):
    lines = [
        "| state | case | threads | plain call (ns) | with it (ns) | added (ns) | middle half of batches (ns) | added (%) | recorded |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for r in rows:
        percent = 100.0 * r["overhead_ns"] / r["plain_ns"] if r["plain_ns"] > 0 else 0.0
        # A percentage of a call that takes a third of a nanosecond says
        # nothing; it is given once the call is long enough to be a function.
        percent = f"{percent:.1f}" if r["plain_ns"] >= 20 else ""
        recorded = "" if r["recorded"] is None else f"{r['recorded']['fraction'] * 100:.1f}%"
        lines.append(
            f"| {r['state']} | {r['case']} | {r['threads']} | {fmt_ns(r['plain_ns'])} | {fmt_ns(r['ns'])} | "
            f"{fmt_ns(r['overhead_ns'])} | {fmt_ns(r['overhead_q1_ns'])} to {fmt_ns(r['overhead_q3_ns'])} | {percent} | {recorded} |"
        )
    return "\n".join(lines)


def report(run, info, opts, argv):
    out = ["# Instrumentation overhead", ""]
    out.append(f"`{' '.join(argv)}`")
    out.append("")
    for key, value in info.items():
        out.append(f"- {key}: {value}")
    out.append(f"- batches: {opts.batches} of {opts.batch_ms} ms each, per row and side; every figure is per call")
    out.append("")
    targets = []
    for row in run.rows:
        if row["target"] not in targets:
            targets.append(row["target"])
    titles = {
        "dynamic": "C, libperfr found at run time (perfr.h as it comes)",
        "static": "C, the API linked in (PERFR_STATIC; what a Rust program has)",
        "python": "Python (perfr_api)",
    }
    for target in targets:
        mine = [r for r in run.rows if r["target"] == target]
        manual = [r for r in mine if r["case"] != "hooked"]
        hooked = [r for r in mine if r["case"] == "hooked"]
        if manual:
            out += [f"## Manual scopes: {titles.get(target, target)}", "", table(manual), ""]
        if hooked:
            out += [f"## Hooked function: {titles.get(target, target)}", "", table(hooked), ""]
    out += [
        "`plain call` is the same loop with no scope and no hook, in the same state, a batch of one and then a batch",
        "of the other. `added` is the median of the differences between each batch and the plain batch before it;",
        "`middle half of batches` is the first and third quartile of those differences.",
        "`recorded` is how many of the calls made during the capture the capture holds: below 100%, the program",
        "wrote faster than the service read, and the figure beside it is the cost of writing records that were lost.",
        "",
    ]
    if run.failures:
        out += ["## Failed checks", ""] + [f"- {f}" for f in run.failures] + [""]
    return "\n".join(out)


def main():
    release = REPO / "target" / "release"
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", help="directory for overhead.md, overhead.json and the built targets (default: a temporary one)")
    parser.add_argument("--service", default=str(release / "perfr"))
    parser.add_argument("--agent", default=str(release / "libperfr.so"), help="libperfr, the hook agent and the API library")
    parser.add_argument("--helper", default=str(release / "perfr-hook"))
    parser.add_argument("--api-lib", default=str(release / "libperfr_api.a"), help="for the statically linked target")
    parser.add_argument("--cpus", help="CPUs to pin the measured threads to, e.g. 2,4,6,8 (default: one per performance core)")
    parser.add_argument("--threads", default="1,2,4,8", help="thread counts for the scaling rows")
    parser.add_argument("--rounds", default="0,100,1000,10000", help="function sizes, in turns of a ~1 ns loop")
    parser.add_argument("--batches", type=int, default=15)
    parser.add_argument("--batch-ms", type=float, default=30.0)
    parser.add_argument("--settle", type=float, default=0.5, help="seconds between starting a capture and measuring in it")
    parser.add_argument("--quick", action="store_true", help="5 batches of 10 ms: a smoke run, not figures to quote")
    parser.add_argument("--only", default="manual,hooks,python", help="which parts to run")
    parser.add_argument("--uprobes", action="store_true", help="also hook with the kernel engine, a uprobe at the entry and a uretprobe at the return (needs --sudo or a privileged service)")
    parser.add_argument("--sudo", action="store_true", help="run the service through sudo -n (and perfr-sudo when installed)")
    parser.add_argument("--sudo-wrapper", help="the wrapper sudo runs the service through (default: /usr/local/bin/perfr-sudo when installed)")
    parser.add_argument("--max-scope-ns", type=float, help="fail when a recorded scope adds more than this to a ~1 us function")
    parser.add_argument("--max-hook-ns", type=float, help="fail when a Frida hook adds more than this to a ~1 us function")
    opts = parser.parse_args()
    if opts.quick:
        opts.batches, opts.batch_ms = 5, 10.0
    parts = set(opts.only.split(","))
    rounds_list = [int(r) for r in opts.rounds.split(",")]
    for name in ("service", "agent", "helper"):
        # Made absolute and no more: the wrapper sudo runs the service
        # through goes by the name it is given, link or not.
        path = Path(os.path.abspath(getattr(opts, name)))
        if not path.exists():
            parser.error(f"--{name}: {path} does not exist; build it first (see --help)")
        setattr(opts, name, str(path))
    cpus = [int(c) for c in opts.cpus.split(",")] if opts.cpus else performance_cpus()
    thread_counts = [t for t in (int(t) for t in opts.threads.split(",")) if t <= len(cpus)]
    out = Path(opts.out).resolve() if opts.out else Path(tempfile.mkdtemp(prefix="perfr-overhead-"))
    out.mkdir(parents=True, exist_ok=True)

    info = machine(cpus)
    service = None
    targets = []
    run = None
    try:
        built = build(opts, out)
        service = Service(opts, out)
        info["perfr"] = service.version()
        run = Run(service, opts)
        print(f"service on {service.base}; measuring on CPUs {cpus[:max(thread_counts or [1])]}", flush=True)

        def spawn(label, argv, env=None):
            target = Target(label, argv, env or service.env, cpus)
            targets.append(target)
            return target

        if "manual" in parts or "hooks" in parts:
            dynamic = spawn("dynamic", [built["dynamic"]])
            if "manual" in parts:
                manual_scopes(run, dynamic, rounds_list, thread_counts, ["scope", "scope_long"])
            else:
                dynamic.init()
            if "hooks" in parts:
                hooks(run, dynamic, "frida", "Frida", rounds_list, thread_counts)
                if opts.uprobes:
                    hooks(run, dynamic, "kernel_uprobes", "uprobe + uretprobe", rounds_list, thread_counts)
        if "manual" in parts and "static" in built:
            manual_scopes(run, spawn("static", [built["static"]]), rounds_list, thread_counts, ["scope"])
        if "python" in parts:
            env = dict(service.env, PYTHONPATH=str(REPO / "crates/perfr-api/python"))
            python = spawn("python", [sys.executable, str(HERE / "overhead_target.py")], env)
            manual_scopes(run, python, [0, 10, 100], [1], ["scope", "decorated", "start_stop"], python=True)

        def budget(limit, case, state_has, what):
            if limit is None:
                return
            for r in run.rows:
                if r["case"] == case and state_has in r["state"] and r["rounds"] == 1000 and r["threads"] == 1 and r["target"] != "python":
                    run.check(r["overhead_ns"] <= limit, f"{r['target']}: {what} adds {r['overhead_ns']:.1f} ns, over the budget of {limit:.1f} ns")

        budget(opts.max_scope_ns, "scope", "capturing", "a recorded scope")
        budget(opts.max_hook_ns, "hooked", "Frida: hooked", "a Frida hook")
    except Failure as failure:
        print(f"FAILED: {failure}", file=sys.stderr)
        if run is not None:
            run.failures.append(str(failure))
        else:
            return 1
    finally:
        for target in targets:
            target.close()
        if service is not None:
            service.close()

    text = report(run, info, opts, ["overhead.py"] + sys.argv[1:])
    (out / "overhead.md").write_text(text)
    (out / "overhead.json").write_text(json.dumps({"machine": info, "rows": run.rows, "failures": run.failures}, indent=1))
    print()
    print(text)
    print(f"written to {out / 'overhead.md'} and {out / 'overhead.json'}")
    return 1 if run.failures else 0


if __name__ == "__main__":
    sys.exit(main())
