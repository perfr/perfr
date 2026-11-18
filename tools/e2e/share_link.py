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

The dialog has a name field, opened on a suggestion (the capture's process
and the time). A name the website would refuse takes Upload away, one the
account already uses comes back from the server as a 409 and stays in the
field, and the name typed last goes with the upload as X-Perfr-Name and is
the one in the link.

Usage:
  python3 tools/e2e/share_link.py [/path/to/viewer-dist]
"""
import json
import os
from pathlib import Path
import re
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
VIEWER = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "crates/perfr-client/viewer-dist"


def frame(kind, payload):
    return struct.pack("<I", len(payload)) + bytes([kind]) + payload


def demo_stream():
    """A one-scope capture the viewer can open with no service."""
    name = b"work"
    event = struct.pack("<QQIIBBBBI", 1_000_000, 500_000, 8, 7, 1, 0, 0, 0, 1)
    return b"".join([
        frame(1, b"OLIV" + struct.pack("<HH", 1, 32)),
        # The capture's start comes before its names: a viewer drops the
        # names it holds when a capture starts.
        frame(4, struct.pack("<IQ", 7, 1_000_000)),
        frame(8, struct.pack("<II", 7, len(b"demo")) + b"demo"),
        frame(7, struct.pack("<III", 7, 8, len(b"main")) + b"main"),
        frame(3, struct.pack("<II", 1, len(name)) + name),
        frame(2, struct.pack("<I", 1) + event),
        frame(5, b""),
    ])


def without_track_order(data):
    """The stream less its track order frames (type 11).

    Share writes the order the tracks are in on screen into what it posts,
    so the link opens the same way; the rest is the capture as loaded.
    """
    out, at = [], 0
    while at + 5 <= len(data):
        end = at + 5 + struct.unpack_from("<I", data, at)[0]
        if data[at + 4] != 11:
            out.append(data[at:end])
        at = end
    return b"".join(out)


def servable(root):
    """Every file under `root`, by the URL path it is served at.

    The server answers from this table and from nothing else: a request
    names a key, never a path on disk, so no spelling of `..` or of a link
    reaches a file outside the pack.
    """
    root = root.resolve()
    return {file.relative_to(root).as_posix(): file for file in root.rglob("*") if file.is_file()}


def local_redirect(target):
    """Where /login sends the browser back to: `target` when it is a path on
    this server or a URL on this machine, and the front page otherwise.

    The target comes from the query string and goes into a header, so its
    line breaks are taken out: a header has none.
    """
    target = target.replace("\r", "").replace("\n", "")
    parts = urllib.parse.urlsplit(target)
    if parts.scheme not in ("", "http") or parts.hostname not in (None, "127.0.0.1", "localhost"):
        return "/"
    # `//host/path` has no scheme and names another host all the same.
    if not parts.scheme and not target.startswith("/") or target.startswith("//"):
        return "/"
    return target


def main():
    stream = demo_stream()
    store = {"demo": stream}
    posted = {}
    files = servable(VIEWER)

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
                        "user": {"id": "u1", "name": "Ada Lovelace", "handle": "ada", "avatar": ""},
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
                nxt = local_redirect((query.get("next") or ["/"])[0])
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
            file = files.get("index.html" if path in ("/", "") else path.lstrip("/"))
            if file is None:
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
            name = self.headers.get("X-Perfr-Name")
            posted.setdefault("names", []).append(name)
            if name == "taken":
                # The account already has a capture there, as perfr-web says it.
                self.json_response(409, {"error": "name_taken", "name": name, "handle": "ada"})
                return
            posted["body"] = body
            posted["type"] = self.headers.get("Content-Type")
            origin = f"http://127.0.0.1:{httpd.server_address[1]}"
            url = f"{origin}/ada/{name}" if name else f"{origin}/c/abc"
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
        # The name field opens on the capture's process and the time, and
        # the line under it is the link that name gives.
        origin = f"http://127.0.0.1:{host_port}"
        try:
            suggested = wait_until(lambda: rect("share-name-text:demo-"), timeout=5)[0][len("share-name-text:"):]
        except AssertionError:
            rows = chrome.eval("JSON.parse(window.__perfr_ui || '[]')") or []
            print("UI", [row[0] for row in rows if str(row[0]).startswith("share-")])
            raise
        assert re.fullmatch(r"demo-\d{4}-\d\d-\d\d-\d{6}", suggested), suggested
        note = wait_until(lambda: rect("share-name-note:"), timeout=5)
        assert note[0] == f"share-name-note:The link will be {origin}/ada/{suggested}", note[0]
        assert chrome.screenshot(str(signed_in_shot)) > 20000, "signed-in dialog screenshot looks blank"
        confirm_shot = Path(os.environ.get("PERFR_SHARE_CONFIRM_SHOT", str(signed_in_shot)))

        def name_it(text):
            """Replace what is in the name field with `text`."""
            field = wait_until(lambda: rect("share-name"), timeout=5)
            chrome.click(field[1] + field[3] / 2, field[2] + field[4] / 2)
            for kind in ("keyDown", "keyUp"):
                chrome.call("Input.dispatchKeyEvent", type=kind, key="a", code="KeyA", modifiers=2,
                            windowsVirtualKeyCode=65, nativeVirtualKeyCode=65)
            chrome.call("Input.insertText", text=text)
            wait_until(lambda: rect(f"share-name-text:{text}"), timeout=5)

        def click_upload():
            upload = wait_until(lambda: rect("share-upload"), timeout=5)
            chrome.click(upload[1] + upload[3] / 2, upload[2] + upload[4] / 2)

        # A name the website would refuse: the reason, and no Upload.
        name_it("first try")
        refused = wait_until(lambda: rect("share-error:Use letters, digits"), timeout=5)
        assert rect("share-upload") is None, f"Upload was offered under a refused name: {refused[0]}"
        assert rect("share-name-note:") is None, "a refused name was given a link"
        # One the account already uses: the server says so, and the dialog
        # keeps the name for the reader to change.
        name_it("taken")
        click_upload()
        taken = wait_until(lambda: rect("share-error:You already have a capture named taken"), timeout=15)
        assert rect("share-name-text:taken"), taken
        assert "body" not in posted, "a refused upload was stored"
        named_shot = Path(os.environ.get("PERFR_SHARE_NAMED_SHOT", "/tmp/perfr-share-named.png"))
        name_it("runs/first-try")
        wait_until(lambda: rect(f"share-name-note:The link will be {origin}/ada/runs/first-try"), timeout=5)
        wait_until(lambda: rect("share-error:") is None, timeout=5)
        assert chrome.screenshot(str(named_shot)) > 20000, "named dialog screenshot looks blank"
        click_upload()
        try:
            url_row = wait_until(lambda: rect("share-url:"), timeout=15)
        except AssertionError:
            rows = chrome.eval("JSON.parse(window.__perfr_ui || '[]')") or []
            print("UI", [row[0] for row in rows])
            print("posted", posted)
            chrome.screenshot("/tmp/perfr-share-fail.png")
            raise
        assert url_row[0] == f"share-url:{origin}/ada/runs/first-try", url_row[0]
        assert posted["names"] == ["taken", "runs/first-try"], posted["names"]
        assert without_track_order(posted.get("body", b"")) == stream, "Share did not post the loaded capture"
        assert posted.get("type") == "application/octet-stream"
        # The link's capture path is what a recipient's viewer fetches.
        got = store["abc"]
        assert without_track_order(got) == stream
        copy = wait_until(lambda: rect("Copy"), timeout=10)
        chrome.click(copy[1] + copy[3] / 2, copy[2] + copy[4] / 2)
        wait_until(lambda: rect("Copied"), timeout=10)
        assert chrome.screenshot(str(shot)) > 20000, "share dialog screenshot looks blank"
        print(f"PASS: signed out, then shared as Ada Lovelace, showed {human[0][len('share-size:'):]} ({len(stream)} bytes) before upload, suggested {suggested}, refused a bad name and a taken one, posted the loaded capture as runs/first-try, link {url_row[0][len('share-url:'):]}")
        print(f"screenshot {shot}")
        print(f"named {named_shot}")
        print(f"signed-out {signed_out_shot}")
        print(f"signed-in {signed_in_shot}")
        print(f"confirm {confirm_shot}")
    finally:
        if chrome:
            chrome.close()
        httpd.shutdown()


if __name__ == "__main__":
    main()
