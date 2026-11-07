#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Open the viewer against a local share server and exercise Share.

The server speaks the website contract: POST /api/share of a .perfr.stream
returns {"id", "url"}, and GET /api/share/<id> returns those bytes. The page
is opened as /?capture=/api/share/<id>&share_base=<this server>, which is how
https://perfr.dev/c/<id> loads the viewer.

Usage:
  python3 tools/e2e/share_link.py [/path/to/viewer-dist]
"""
import json
import os
from pathlib import Path
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(Path(__file__).resolve().parent))
from cdp import Chrome  # noqa: E402
from share_capture import port, wait_until  # noqa: E402


ROOT = Path(__file__).resolve().parents[2]
VIEWER = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "crates/perfr-live-viewer/viewer-dist"


def frame(kind, payload):
    return struct.pack("<I", len(payload)) + bytes([kind]) + payload


def demo_stream():
    """A one-scope capture the viewer can open with no service."""
    name = b"work"
    event = struct.pack("<QQIIBBBBI", 1_000_000, 500_000, 8, 7, 1, 0, 0, 0, 1)
    return b"".join([
        frame(1, b"OLIV" + struct.pack("<HH", 1, 32)),
        frame(8, struct.pack("<II", 7, len(b"demo")) + b"demo"),
        frame(7, struct.pack("<III", 7, 8, len(b"main")) + b"main"),
        frame(3, struct.pack("<II", 1, len(name)) + name),
        frame(4, struct.pack("<IQ", 7, 1_000_000)),
        frame(2, struct.pack("<I", 1) + event),
        frame(5, b""),
    ])


def main():
    stream = demo_stream()
    store = {"demo": stream}
    posted = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def end_headers(self):
            self.send_header("Cross-Origin-Opener-Policy", "same-origin")
            self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
            self.send_header("Cross-Origin-Resource-Policy", "same-origin")
            self.send_header("Cache-Control", "no-store")
            super().end_headers()

        def do_GET(self):
            path = self.path.split("?", 1)[0]
            if path.startswith("/api/share/"):
                body = store.get(path.rsplit("/", 1)[-1])
                if body is None:
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return
            rel = "index.html" if path in ("/", "") else path.lstrip("/")
            file = (VIEWER / rel).resolve()
            if not str(file).startswith(str(VIEWER.resolve())) or not file.is_file():
                self.send_error(404)
                return
            data = file.read_bytes()
            kind = "application/javascript" if file.suffix == ".js" else "application/wasm" if file.suffix == ".wasm" else "application/octet-stream"
            if file.suffix == ".html":
                kind = "text/html"
            elif file.suffix == ".png":
                kind = "image/png"
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_POST(self):
            path = self.path.split("?", 1)[0]
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length)
            if path != "/api/share":
                self.send_error(404)
                return
            if self.headers.get("Content-Type") != "application/octet-stream":
                self.send_error(415)
                return
            posted["body"] = body
            posted["type"] = self.headers.get("Content-Type")
            url = f"http://127.0.0.1:{httpd.server_address[1]}/c/abc"
            payload = json.dumps({"id": "abc", "url": url}).encode()
            store["abc"] = body
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    httpd = ThreadingHTTPServer(("127.0.0.1", port()), Handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    host_port = httpd.server_address[1]
    page = f"http://127.0.0.1:{host_port}/?capture=/api/share/demo&share_base=http://127.0.0.1:{host_port}"
    shot = Path(os.environ.get("PERFR_SHARE_SHOT", "/tmp/perfr-share-dialog.png"))
    chrome = None
    try:
        chrome = Chrome(port=port(), width=1280, height=800)
        chrome.goto(page, settle=2)

        def state():
            raw = chrome.eval("window.__perfr_sel || ''")
            if not raw:
                return None
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                return None
            return parsed if parsed.get("events") else None

        wait_until(state, timeout=60)

        def rect(prefix):
            rows = chrome.eval("JSON.parse(window.__perfr_ui || '[]')") or []
            for row in rows:
                if isinstance(row[0], str) and (row[0] == prefix or row[0].startswith(prefix)):
                    return row
            return None

        label, x, y, w, h = wait_until(lambda: rect("Share"), timeout=30)
        chrome.click(x + w / 2, y + h / 2)
        try:
            url_row = wait_until(lambda: rect("share-url:"), timeout=15)
        except AssertionError:
            rows = chrome.eval("JSON.parse(window.__perfr_ui || '[]')") or []
            print("UI", [row[0] for row in rows])
            print("posted", posted)
            chrome.screenshot("/tmp/perfr-share-fail.png")
            raise
        assert url_row[0] == f"share-url:http://127.0.0.1:{host_port}/c/abc", url_row[0]
        assert posted.get("body") == stream, "Share did not post the loaded capture"
        assert posted.get("type") == "application/octet-stream"
        # The link's capture path is what a recipient's viewer fetches.
        got = store["abc"]
        assert got == stream
        copy = wait_until(lambda: rect("Copy"), timeout=10)
        chrome.click(copy[1] + copy[3] / 2, copy[2] + copy[4] / 2)
        wait_until(lambda: rect("Copied"), timeout=10)
        assert chrome.screenshot(str(shot)) > 20000, "share dialog screenshot looks blank"
        print(f"PASS: share dialog, posted {len(stream)} stream bytes, link {url_row[0][len('share-url:'):]}")
        print(f"screenshot {shot}")
    finally:
        if chrome:
            chrome.close()
        httpd.shutdown()


if __name__ == "__main__":
    main()
