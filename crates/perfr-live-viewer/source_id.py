#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Prints an id of the sources the viewer pack is built from.

The id is a SHA-256 over the tracked files of perfr-live-viewer and of every
workspace crate it depends on (followed through `path = "../x"` in each
Cargo.toml), taken from the working copy, excluding viewer-dist/ itself, docs,
examples, tests and benches. build_wasm.sh writes it to viewer-dist/VERSION;
perfr-web's build_site.py recomputes it over the checkout it deploys and
refuses a pack whose id differs. Content, not commits: a squash merge keeps
the id, and a pack built on a branch that missed a viewer change merged
meanwhile does not.

Cargo.lock is left out on purpose: a lock change for a server-only crate must
not demand a viewer rebuild. A dependency bump that matters shows up in the
viewer's Cargo.toml.

    python3 crates/perfr-live-viewer/source_id.py [--files] [perfr checkout]

With --files the id is followed by the files it was computed over, one per
line, relative to the checkout: perfr-live-server's build script asks for
them so that cargo runs it again, and it can say the embedded pack is stale,
when one of them changes.
"""
import hashlib
import os
import re
import subprocess
import sys

VIEWER = "perfr-live-viewer"
SKIP_DIRS = {"viewer-dist", "examples", "tests", "benches"}


def counts(rel):
    """Sources only: not the pack itself, docs, examples, tests or benches."""
    parts = rel.split("/")
    return not rel.endswith(".md") and SKIP_DIRS.isdisjoint(parts[2:-1])


def crates(root):
    """The viewer and its workspace dependencies, by crate directory name."""
    seen = []
    todo = [VIEWER]
    while todo:
        name = todo.pop()
        if name in seen:
            continue
        seen.append(name)
        with open(os.path.join(root, "crates", name, "Cargo.toml"), encoding="utf-8") as handle:
            manifest = handle.read()
        todo.extend(re.findall(r'path\s*=\s*"\.\./([^"/]+)"', manifest))
    return sorted(seen)


def source_files(root):
    """The files the id covers, sorted, relative to `root`."""
    paths = [f"crates/{name}" for name in crates(root)]
    listing = subprocess.run(
        ["git", "ls-files", "-z", "--", *paths],
        cwd=root, capture_output=True, check=True,
    ).stdout
    files = []
    for rel in sorted(p.decode() for p in listing.split(b"\0") if p):
        if not counts(rel) or not os.path.isfile(os.path.join(root, rel)):
            continue  # excluded, or tracked but deleted in the working copy
        files.append(rel)
    return files


def source_id(root, files=None):
    digest = hashlib.sha256()
    for rel in source_files(root) if files is None else files:
        digest.update(rel.encode() + b"\0")
        with open(os.path.join(root, rel), "rb") as handle:
            digest.update(handle.read())
        digest.update(b"\0")
    return digest.hexdigest()[:16]


def main():
    args = sys.argv[1:]
    list_files = "--files" in args
    args = [a for a in args if a != "--files"]
    root = args[0] if args else os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
    root = os.path.abspath(root)
    files = source_files(root)
    print(source_id(root, files))
    if list_files:
        for rel in files:
            print(rel)


if __name__ == "__main__":
    main()
