#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fail if the slim viewer reaches the capture pipeline or the full page.

Two checks, over the viewer core (perfr-viewer) and the slim embed built on
it (perfr-viewer-embed):

1. The workspace crates either of them links, by any chain of dependencies,
   are the event format, the stream framing, the rasterizer and the core.
   Read from `cargo metadata`, so it is the graph cargo builds: a dependency
   inherited from the workspace, renamed, or declared for one target counts
   like any other. Dev-dependencies are left out; they are not in the pack.

2. Neither crate's sources `include_str!` or `include_bytes!` a file outside
   its own directory. That is a dependency cargo does not know about: a test
   in the core once read the full page's index.html this way.

    python3 .github/scripts/check-viewer-boundary.py
"""
import json
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
STARTS = ["perfr-viewer", "perfr-viewer-embed"]
ALLOWED = {
    "perfr-viewer",
    "perfr-viewer-embed",
    "perfr-live-event",
    "perfr-live-protocol",
    "perfr-live-render",
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
                    if os.path.commonpath([crate, resolved]) != crate:
                        found.append((os.path.relpath(path, ROOT), target))
    return found


def main():
    run = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, capture_output=True, text=True,
    )
    if run.returncode != 0:
        print(run.stderr, file=sys.stderr)
        return 1
    chains = linked_workspace_crates(json.loads(run.stdout))

    failed = False
    for name in sorted(set(chains) - ALLOWED):
        failed = True
        print(
            f"the slim viewer must not link {name} "
            f"(reached through {' -> '.join(chains[name])})",
            file=sys.stderr,
        )
    for path, target in escaping_includes():
        failed = True
        print(f"{path} includes {target}, a file outside its crate", file=sys.stderr)
    if failed:
        return 1
    print("slim viewer workspace crates: " + ", ".join(sorted(chains)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
