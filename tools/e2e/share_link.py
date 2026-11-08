#!/usr/bin/env python3
# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Open the viewer against a local share server and exercise Share.

The server speaks the website contract: GET /api/me is 401 until /login
sets a session cookie, then 200 with a user and quota. POST /api/share of
a .perfr.stream returns {"id", "url"}, and GET /api/share/<id> returns
those bytes. The page is opened as
/?capture=/api/share/<id>&share_base=<this server>, which is how
https://perfr.dev/c/<id> loads the viewer.

Share opens a confirm step first. Signed out, it says "Sign in to share".
After GitHub sign-in it names the account and the quota. Nothing is posted
until Upload, and the dialog shows the byte size of the loaded capture.
With no time range selected, the hint explains how to share one.

Usage:
  python3 tools/e2e/share_link.py [/path/to/viewer-dist]
"""
import json
import os
from pathlib import Path
import struct
import sys
import threading
import time
import urllib.parse
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

        def signed_in(self):
            return "perfr_session=ok" in self.headers.get("Cookie", "")

        def json_response(self, status, payload):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            parsed = urllib.parse.urlsplit(self.path)
            path = parsed.path
            if path == "/api/me":
                if self.signed_in():
                    self.json_response(200, {
                        "user": {"id": "u1", "name": "Ada Lovelace", "avatar": ""},
                        "quota": {
                            "used_bytes": 12_000_000,
                            "limit_bytes": 1_000_000_000,
                            "max_capture_bytes": 50_000_000,
                        },
                    })
                else:
                    self.json_response(401, {"error": "login_required", "login_url": "/login?next=/"})
                return
            if path == "/login":
                query = urllib.parse.parse_qs(parsed.query)
                nxt = (query.get("next") or ["/"])[0]
                host = urllib.parse.urlsplit(nxt).hostname
                if host not in (None, "127.0.0.1", "localhost"):
                    nxt = "/"
                self.send_response(302)
                self.send_header("Set-Cookie", "perfr_session=ok; Path=/; SameSite=Lax")
                self.send_header("Location", nxt)
                self.end_headers()
                return
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
            if "perfr_session=ok" not in self.headers.get("Cookie", ""):
                body = json.dumps({"error": "login_required", "login_url": "/login?next=/"}).encode()
                self.send_response(401)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
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
    signed_out_shot = Path(os.environ.get("PERFR_SHARE_SIGNED_OUT_SHOT", "/tmp/perfr-share-signed-out.png"))
    signed_in_shot = Path(os.environ.get("PERFR_SHARE_SIGNED_IN_SHOT", "/tmp/perfr-share-signed-in.png"))
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

        def wait_rect(prefix, timeout=30):
            # Navigation (same-tab sign-in) makes eval fail until the page is back.
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                try:
                    row = rect(prefix)
                    if row:
                        return row
                except Exception as exc:  # noqa: BLE001 - the page is mid-navigation
                    last = exc
                time.sleep(0.2)
            raise AssertionError(f"timed out waiting for {prefix}: {last}")

        label, x, y, w, h = wait_until(lambda: rect("Share"), timeout=30)
        chrome.click(x + w / 2, y + h / 2)
        signin = wait_until(lambda: rect("share-signin"), timeout=20)
        assert signin[0] == "share-signin", signin
        github = wait_until(lambda: rect("share-provider:github"), timeout=5)
        google = wait_until(lambda: rect("share-provider:google"), timeout=5)
        assert github[0] == "share-provider:github"
        assert google[0] == "share-provider:google"
        assert rect("share-upload") is None, "Upload was offered before sign-in"
        assert "body" not in posted, "Share posted before sign-in"
        assert chrome.screenshot(str(signed_out_shot)) > 20000, "signed-out dialog screenshot looks blank"
        chrome.click(github[1] + github[3] / 2, github[2] + github[4] / 2)
        try:
            account = wait_rect("share-account:Ada Lovelace", timeout=40)
        except AssertionError:
            rows = chrome.eval("JSON.parse(window.__perfr_ui || '[]')") or []
            print("UI after sign-in", [row[0] for row in rows])
            chrome.screenshot("/tmp/perfr-share-signin-fail.png")
            raise
        assert account[0] == "share-account:Ada Lovelace"
        quota = wait_until(lambda: rect("share-quota:"), timeout=5)
        assert "12.0 MB of 1.0 GB used" in quota[0], quota[0]
        size_row = wait_until(lambda: rect(f"share-bytes:{len(stream)}"), timeout=15)
        assert size_row[0] == f"share-bytes:{len(stream)}"
        human = wait_until(lambda: rect("share-size:"), timeout=5)
        assert human[0].startswith("share-size:")
        assert human[0] != "share-size:"
        hint = wait_until(lambda: rect("share-hint:"), timeout=5)
        assert "right-drag" in hint[0] and "sample bar" in hint[0], hint[0]
        assert rect("share-range-bytes:") is None, "a range was offered with nothing selected"
        assert "body" not in posted, "Share posted before Upload"
        assert chrome.screenshot(str(signed_in_shot)) > 20000, "signed-in dialog screenshot looks blank"
        confirm_shot = Path(os.environ.get("PERFR_SHARE_CONFIRM_SHOT", str(signed_in_shot)))
        upload = wait_until(lambda: rect("share-upload"), timeout=5)
        chrome.click(upload[1] + upload[3] / 2, upload[2] + upload[4] / 2)
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
        print(f"PASS: signed out, then shared as Ada Lovelace, showed {human[0][len('share-size:'):]} ({len(stream)} bytes) before upload, posted the loaded capture, link {url_row[0][len('share-url:'):]}")
        print(f"screenshot {shot}")
        print(f"signed-out {signed_out_shot}")
        print(f"signed-in {signed_in_shot}")
        print(f"confirm {confirm_shot}")
    finally:
        if chrome:
            chrome.close()
        httpd.shutdown()


if __name__ == "__main__":
    main()
