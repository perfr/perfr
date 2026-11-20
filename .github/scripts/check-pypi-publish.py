#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fail a pull request that would break, or widen, publishing to PyPI.

The Trusted Publishers of `perfr` and `perfr-api` on PyPI accept an upload
from one place: this repository, the workflow file python-wheels.yml, the
GitHub environment `pypi`. None of that is in the repository, so nothing
here notices when a workflow stops matching it. The next release then fails
at its last step with `invalid-publisher`, on a tag that cannot be moved,
as 0.1.3 to 0.1.6 did.

So every job that uploads to PyPI must:

- be in python-wheels.yml, the file the publishers name;
- run in the environment `pypi`;
- run only for the push of a `perfr-v*` tag, so that a manual run started
  from a tag builds wheels and publishes nothing.

    python3 .github/scripts/check-pypi-publish.py [workflow.yml ...]

The workflows are read as text, by indentation, so that the check needs
nothing the runner might not have.
"""
import glob
import os
import re
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
WORKFLOWS = os.path.join(ROOT, ".github", "workflows")

PUBLISHER_FILE = "python-wheels.yml"
ENVIRONMENT = "pypi"
UPLOADS = "pypa/gh-action-pypi-publish"
ON_PUSH = "github.event_name == 'push'"
ON_TAG = "startsWith(github.ref, 'refs/tags/perfr-v')"


def code(line):
    """A line without its trailing comment."""
    return re.sub(r"\s+#.*$", "", line.rstrip())


def jobs(text):
    """The jobs of a workflow: name and the lines of its block."""
    found = []
    in_jobs = False
    for line in text.splitlines():
        if re.match(r"^\S", line):
            in_jobs = code(line) == "jobs:"
            continue
        if not in_jobs:
            continue
        head = re.match(r"^  ([A-Za-z0-9_-]+):\s*(#.*)?$", line)
        if head:
            found.append((head.group(1), []))
        elif found:
            found[-1][1].append(line)
    return found


def key(block, name):
    """The value of a job's own key, one level under the job's name."""
    for line in block:
        match = re.match(rf"^    {re.escape(name)}:\s*(.*)$", code(line))
        if match:
            return match.group(1).strip()
    return None


def environment(block):
    """The job's environment, written as a name or as a mapping with one."""
    value = key(block, "environment")
    if value:
        return value.strip("'\"")
    if value is None:
        return None
    for line in block:
        match = re.match(r"^      name:\s*(.*)$", code(line))
        if match:
            return match.group(1).strip().strip("'\"")
    return None


def problems(path, text):
    found = []
    name = os.path.basename(path)
    for job, block in jobs(text):
        if not any(UPLOADS in code(line) for line in block):
            continue
        where = f"{name}: job `{job}`"
        if name != PUBLISHER_FILE:
            found.append(
                f"{where} uploads to PyPI, and the Trusted Publishers on PyPI "
                f"name {PUBLISHER_FILE}: an upload from another file is refused."
            )
        env = environment(block)
        if env != ENVIRONMENT:
            found.append(
                f"{where} uploads to PyPI from "
                f"{'no environment' if env is None else f'the environment `{env}`'}; "
                f"the Trusted Publishers on PyPI accept `{ENVIRONMENT}` only."
            )
        condition = key(block, "if") or ""
        if ON_PUSH not in condition or ON_TAG not in condition:
            found.append(
                f"{where} must run only for the push of a release tag: its `if` "
                f"needs `{ON_PUSH} && {ON_TAG}`, and is `{condition or 'absent'}`."
            )
    return found


def main():
    paths = sys.argv[1:] or sorted(
        glob.glob(os.path.join(WORKFLOWS, "*.yml")) + glob.glob(os.path.join(WORKFLOWS, "*.yaml"))
    )
    found = []
    uploads = 0
    for path in paths:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        uploads += sum(UPLOADS in code(line) for line in text.splitlines())
        found += problems(path, text)
    if not sys.argv[1:] and uploads == 0:
        found.append(
            f"no workflow uploads to PyPI ({UPLOADS}): if the upload moved, "
            "this check and the Trusted Publishers on PyPI have to move with it."
        )
    for line in found:
        print(line)
    if found:
        print("docs/releasing.md, \"Once, before the first release\", says what PyPI is set to.")
        return 1
    print(f"the upload to PyPI is from {PUBLISHER_FILE}, environment `{ENVIRONMENT}`, on a pushed perfr-v* tag")
    return 0


if __name__ == "__main__":
    sys.exit(main())
