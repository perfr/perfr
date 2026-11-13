#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fail if the viewer core depends on the capture pipeline.

Walks `path = "../…"` dependencies starting at perfr-viewer. The only
crates on that walk are the event format, the stream framing, and the
rasterizer. Capture, instrumentation, the live server, and the full
viewer page are not allowed, and a cycle is a failure too.

    python3 crates/perfr-viewer/check_boundary.py
"""
import os
import re
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
START = "perfr-viewer"
ALLOWED = {
    "perfr-viewer",
    "perfr-live-event",
    "perfr-live-protocol",
    "perfr-live-render",
}


def path_deps(name):
    manifest = os.path.join(ROOT, "crates", name, "Cargo.toml")
    with open(manifest, encoding="utf-8") as handle:
        text = handle.read()
    return re.findall(r'path\s*=\s*"\.\./([^"/]+)"', text)


def main():
    seen = []
    todo = [START]
    bad = []
    while todo:
        name = todo.pop()
        if name in seen:
            continue
        seen.append(name)
        if name not in ALLOWED:
            bad.append(name)
            continue
        for dep in path_deps(name):
            todo.append(dep)
    if bad:
        print(
            "perfr-viewer must not depend on the capture pipeline "
            f"(found {', '.join(bad)})",
            file=sys.stderr,
        )
        return 1
    print("viewer core path deps: " + ", ".join(sorted(seen)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
