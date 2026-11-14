#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fail a pull request whose committed viewer pack does not match its sources.

perfr.dev runs this comparison in build_site.py: it recomputes
crates/perfr-client/source_id.py over the checkout it deploys and
refuses a pack whose id differs. A mismatch there fails the deploy. This
script is that comparison on the pull request, so the mismatch is caught
before it reaches main.

It also fails when a file that counts toward the id changed since the base
and viewer-dist/perfr_client_bg.wasm did not. Writing a new source id
into VERSION by hand is not a rebuild; build_wasm.sh rewrites the wasm.

    python3 .github/scripts/check-viewer-pack.py [--base origin/main]
"""
import argparse
import os
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "crates", "perfr-client"))

import source_id  # noqa: E402

PACK = "crates/perfr-client/viewer-dist"
VERSION = os.path.join(PACK, "VERSION")
WASM = os.path.join(PACK, "perfr_client_bg.wasm")


def git(args):
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True)


def version_fields():
    path = os.path.join(ROOT, VERSION)
    if not os.path.isfile(path):
        return None
    fields = {}
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line or "=" not in line:
                continue
            key, value = line.split("=", 1)
            fields[key] = value
    return fields


def is_counted(rel):
    """True when source_id.py would hash this path.

    A deletion is included: the file is gone from HEAD, but it counted in
    the base and dropping it changes the id.
    """
    prefixes = tuple(f"crates/{name}/" for name in source_id.crates(ROOT))
    return rel.startswith(prefixes) and source_id.counts(rel)


def changed_since(base):
    """Paths that differ between the merge base with `base` and HEAD."""
    merged = git(["merge-base", base, "HEAD"])
    if merged.returncode != 0:
        raise SystemExit(
            f"cannot find a merge base of {base} and HEAD:\n{merged.stderr.decode()}"
        )
    base_sha = merged.stdout.decode().strip()
    diff = git(["diff", "-z", "--name-only", base_sha, "HEAD"])
    if diff.returncode != 0:
        raise SystemExit(diff.stderr.decode())
    return [p.decode() for p in diff.stdout.split(b"\0") if p]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base",
        default=None,
        help="ref whose merge-base with HEAD is the comparison point "
             "(default: origin/$GITHUB_BASE_REF, or origin/main)",
    )
    args = parser.parse_args()
    base = args.base or f"origin/{os.environ.get('GITHUB_BASE_REF') or 'main'}"

    fields = version_fields()
    if not fields or "source" not in fields:
        print(
            f"{VERSION} has no source id. Run crates/perfr-client/build_wasm.sh "
            "and commit viewer-dist.",
            file=sys.stderr,
        )
        return 1

    checkout = source_id.source_id(ROOT)
    pack = fields["source"]
    commit = fields.get("hash", "unknown")
    failed = False
    if pack != checkout:
        failed = True
        print(
            f"the viewer pack in {PACK} was built from other viewer sources than "
            f"this checkout has (pack {pack} at commit {commit}, checkout {checkout}). "
            "Rebuild it with crates/perfr-client/build_wasm.sh and commit "
            "viewer-dist. perfr.dev runs this same check and will refuse to deploy.",
            file=sys.stderr,
        )

    try:
        changed = changed_since(base)
    except SystemExit as exc:
        # A missing base would hide a source change that did not rebuild
        # the wasm. Fail closed.
        print(exc, file=sys.stderr)
        return 1

    counted = [rel for rel in changed if is_counted(rel)]
    wasm_changed = WASM in changed
    if counted and not wasm_changed:
        failed = True
        shown = "\n".join(f"  {rel}" for rel in counted[:30])
        extra = ""
        if len(counted) > 30:
            extra = f"\n  … and {len(counted) - 30} more"
        print(
            "viewer sources changed but "
            f"{WASM} did not. Updating {VERSION} by hand is not a rebuild.\n"
            f"{shown}{extra}\n"
            "Run crates/perfr-client/build_wasm.sh and commit viewer-dist.",
            file=sys.stderr,
        )

    if not failed:
        print(f"viewer pack {pack} matches this checkout")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
