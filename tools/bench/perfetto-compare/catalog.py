#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Rebuild catalog.json from a google/perfetto checkout.

test/data stores a .sha256 sidecar per trace. The bytes live at
https://storage.googleapis.com/perfetto/test_data/<basename>-<sha256>
which is what tools/test_data downloads. This sends a HEAD for each and
records Content-Length. It does not download the traces.
"""

import json
import os
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor

BUCKET = "https://storage.googleapis.com/perfetto/test_data"


def sidecars(root):
    base = os.path.join(root, "test", "data")
    found = []
    for dirpath, _, names in os.walk(base):
        for name in names:
            if not name.endswith(".sha256"):
                continue
            full = os.path.join(dirpath, name)
            digest = open(full, encoding="utf-8").read().strip().split()[0]
            rel = os.path.relpath(full[: -len(".sha256")], root)
            leaf = os.path.basename(rel)
            found.append((rel, digest, f"{BUCKET}/{leaf}-{digest}"))
    return found


def head(item):
    rel, digest, url = item
    req = urllib.request.Request(url, method="HEAD")
    row = {"path": rel, "sha256": digest, "url": url}
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            row["bytes"] = int(resp.headers.get("Content-Length") or 0)
            row["status"] = resp.status
    except Exception as exc:
        row["bytes"] = None
        row["status"] = getattr(exc, "code", None)
        row["error"] = str(exc)[:200]
    return row


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else None
    if not root or not os.path.isdir(os.path.join(root, "test", "data")):
        sys.exit("usage: catalog.py /path/to/perfetto")
    commit = os.popen(f"git -C {root} rev-parse HEAD").read().strip()
    items = sidecars(root)
    with ThreadPoolExecutor(32) as pool:
        rows = list(pool.map(head, items))
    rows.sort(key=lambda r: r["path"])
    out = {
        "source": "google/perfetto test/data sha256 sidecars",
        "commit": commit,
        "bucket": BUCKET + "/<basename>-<sha256>",
        "count": len(rows),
        "bytes": sum(r["bytes"] or 0 for r in rows),
        "files": rows,
    }
    dest = os.path.join(os.path.dirname(__file__), "catalog.json")
    with open(dest, "w", encoding="utf-8") as fh:
        json.dump(out, fh, indent=1)
        fh.write("\n")
    print(f"{len(rows)} files, {out['bytes']} bytes -> {dest}")


if __name__ == "__main__":
    main()
