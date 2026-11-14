#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fail if the viewer reaches the capture pipeline or the client.

Three checks:

1. The workspace crates perfr-viewer links, by any chain of dependencies,
   are the event format, the stream framing, the rasterizer, the trace
   importers, the story document and the build stamp. Read from
   `cargo metadata`, so it is the graph cargo builds: a dependency inherited
   from the workspace, renamed, or declared for one target counts like any
   other. Dev-dependencies are left out; they are not in the pack.

2. Its sources do not `include_str!` or `include_bytes!` a file outside the
   crate's directory, apart from the two example files the code view shows
   (EXAMPLES). That is a dependency cargo does not know about: a test here
   once read the client's index.html this way.

3. perfr-client builds on perfr-viewer with the `page` feature off. `page`
   is the viewer's own entry point, and an entry point compiled into a
   crate is exported from every pack that links it.

    python3 .github/scripts/check-viewer-boundary.py
"""
import json
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
STARTS = ["perfr-viewer"]
CLIENT = "perfr-client"
PAGE_FEATURE = "page"
ALLOWED = {
    "perfr-viewer",
    "perfr-live-event",
    "perfr-live-protocol",
    "perfr-live-render",
    "perfr-live-chrome",
    "perfr-story",
    "perfr-build-info",
}
# Text the code view shows as examples when nothing is captured: two files
# of this repository embedded as strings, not code the viewer runs.
EXAMPLES = {
    "crates/perfr/src/uprobes.rs",
    "tools/test-programs/PerfrTestCpp/PerfrTestCpp.cpp",
}
INCLUDE = re.compile(r'include_(?:str|bytes)!\s*\(\s*"([^"]+)"')


def linked_workspace_crates(metadata):
    """Workspace crates reachable from STARTS, as {name: the chain to it}."""
    names = {p["id"]: p["name"] for p in metadata["packages"]}
    members = set(metadata["workspace_members"])
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    ids = {names[i]: i for i in members}
    missing = [name for name in STARTS if name not in ids]
    if missing:
        raise SystemExit(f"not a workspace crate: {', '.join(missing)}")

    chains = {}
    todo = [(ids[name], [name]) for name in STARTS]
    seen = set()
    while todo:
        pkg, chain = todo.pop()
        if pkg in seen:
            continue
        seen.add(pkg)
        if pkg in members:
            chains.setdefault(names[pkg], chain)
        for dep in nodes[pkg]["deps"]:
            # kind is null for a normal dependency, "build" or "dev" otherwise.
            if all(k["kind"] == "dev" for k in dep["dep_kinds"]):
                continue
            todo.append((dep["pkg"], chain + [names[dep["pkg"]]]))
    return chains


def escaping_includes():
    """(file, included path) for every include that leaves its crate."""
    found = []
    for name in STARTS:
        crate = os.path.join(ROOT, "crates", name)
        for folder, _, files in os.walk(os.path.join(crate, "src")):
            for file in files:
                if not file.endswith(".rs"):
                    continue
                path = os.path.join(folder, file)
                with open(path, encoding="utf-8") as handle:
                    text = handle.read()
                for target in INCLUDE.findall(text):
                    resolved = os.path.normpath(os.path.join(folder, target))
                    if os.path.commonpath([crate, resolved]) == crate:
                        continue
                    if os.path.relpath(resolved, ROOT).replace(os.sep, "/") in EXAMPLES:
                        continue
                    found.append((os.path.relpath(path, ROOT), target))
    return found


def client_page_feature(metadata):
    """Why the client's pack would carry the viewer's page, or an empty list."""
    client = next((p for p in metadata["packages"] if p["name"] == CLIENT), None)
    if client is None:
        return [f"not a workspace crate: {CLIENT}"]
    wanted = f"{STARTS[0]}/{PAGE_FEATURE}"
    reasons = []
    for dep in client["dependencies"]:
        if dep["name"] != STARTS[0] or dep["kind"] == "dev":
            continue
        if dep["uses_default_features"]:
            reasons.append(f"{CLIENT} depends on {STARTS[0]} with its default features")
        if PAGE_FEATURE in dep["features"]:
            reasons.append(f"{CLIENT} asks for {wanted}")
    for feature, enables in client["features"].items():
        if any(e.replace("?", "") == wanted for e in enables):
            reasons.append(f"the {feature} feature of {CLIENT} turns on {wanted}")
    return reasons


def main():
    run = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if run.returncode != 0:
        print(run.stderr, file=sys.stderr)
        return 1
    metadata = json.loads(run.stdout)
    chains = linked_workspace_crates(metadata)

    failed = False
    for name in sorted(set(chains) - ALLOWED):
        failed = True
        print(
            f"the viewer must not link {name} "
            f"(reached through {' -> '.join(chains[name])})",
            file=sys.stderr,
        )
    for path, target in escaping_includes():
        failed = True
        print(f"{path} includes {target}, a file outside its crate", file=sys.stderr)
    for reason in client_page_feature(metadata):
        failed = True
        print(f"{reason}: the client's pack must not export the viewer's entry point", file=sys.stderr)
    if failed:
        return 1
    print("viewer workspace crates: " + ", ".join(sorted(chains)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
