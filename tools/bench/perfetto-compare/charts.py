#!/usr/bin/env python3
"""Bar charts from results/run.json. Writes SVG next to the blog images.

The numbers are the medians already stored on each trace. Frame-rate bars
use that median and do not hide a run that sat at the idle rate: the
caption is the post's job.
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results", "run.json")
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(HERE, "..", "..", "..", "docs", "images")

ACCENT = "#1F7B95"
INK = "#3c4741"
MUTED = "#62716a"


def bars(path, title, ylabel, rows, width=760):
    """rows: list of (label, perfr, perfetto). None is a gap."""
    label_w = 168
    height = 36 * len(rows) + 56
    pad_r = 16
    max_v = max((v for _, a, b in rows for v in (a, b) if v is not None), default=1) or 1
    plot_w = width - label_w - pad_r - 8
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" role="img">',
        f'<title>{title}</title>',
        f'<rect width="100%" height="100%" fill="#fafbf9"/>',
        f'<text x="16" y="22" fill="{INK}" font-family="Archivo, Helvetica, sans-serif" font-size="14" font-weight="600">{title}</text>',
        f'<text x="{width - 16}" y="22" text-anchor="end" fill="{MUTED}" font-family="IBM Plex Mono, monospace" font-size="11">{ylabel} · teal perfr, grey Perfetto UI</text>',
    ]
    for i, (label, a, b) in enumerate(rows):
        y = 40 + i * 36
        parts.append(
            f'<text x="12" y="{y + 16}" fill="{INK}" font-family="IBM Plex Mono, monospace" font-size="11">{label}</text>'
        )
        for j, (val, color) in enumerate(((a, ACCENT), (b, "#b7c3ba"))):
            if val is None:
                continue
            w = max(1, plot_w * val / max_v)
            yy = y + j * 14
            parts.append(f'<rect x="{label_w}" y="{yy}" width="{w:.1f}" height="12" fill="{color}"/>')
            parts.append(
                f'<text x="{label_w + w + 4:.1f}" y="{yy + 10}" fill="{MUTED}" font-family="IBM Plex Mono, monospace" font-size="10">{val:g}</text>'
            )
    parts.append("</svg>")
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, path), "w", encoding="utf-8") as fh:
        fh.write("\n".join(parts) + "\n")
    print(path)


def main():
    data = json.load(open(SRC, encoding="utf-8"))
    rows_r, rows_f, rows_m = [], [], []
    for t in data["traces"]:
        label = t["id"]
        p = t["summary"]["perfr"]
        q = t["summary"]["perfetto"]
        rows_r.append((label, None if p["renderMs"] is None else round(p["renderMs"] / 1000, 2),
                       None if q["renderMs"] is None else round(q["renderMs"] / 1000, 2)))
        rows_f.append((label, None if p["fps"] is None else round(p["fps"], 1),
                       None if q["fps"] is None else round(q["fps"], 1)))
        rows_m.append((label, None if p["peakRssMb"] is None else round(p["peakRssMb"]),
                       None if q["peakRssMb"] is None else round(q["peakRssMb"])))
    bars("perfetto-compare-render.svg", "Time to a loaded timeline, median of five", "seconds", rows_r)
    bars("perfetto-compare-fps.svg", "Frame rate during the scripted zoom and pan, median", "frames/s", rows_f)
    bars("perfetto-compare-rss.svg", "Peak Chrome process-tree RSS, median", "MB", rows_m)


if __name__ == "__main__":
    main()
