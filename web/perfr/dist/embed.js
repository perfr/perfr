// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
import { createByteRing, sabAvailable, writeBytes } from "./sab.js";
const SOURCE = "perfr-embed";
function withEmbed(viewerUrl) {
    const url = new URL(viewerUrl, globalThis.location?.href ?? "http://localhost/");
    if (!url.searchParams.has("capture"))
        url.searchParams.set("embed", "1");
    return url.toString();
}
function isTyping(target) {
    if (!(target instanceof HTMLElement))
        return false;
    const tag = target.tagName;
    return tag === "INPUT" || tag === "TEXTAREA" || target.isContentEditable;
}
/**
 * Mount the existing live viewer and stream this profiler's frames into it.
 * The viewer page (embed.js) accepts the stream on a MessageChannel. When the
 * page is cross-origin isolated and the viewer is same-origin, encoded bytes
 * go through a SharedArrayBuffer instead; the channel carries them when that
 * ring is full or SharedArrayBuffer is missing. Every write is announced on
 * the channel with its length, and the viewer reads the ring only as those
 * messages arrive, so the two paths cannot overtake each other.
 *
 * Share in the viewer reads the ring back through the same channel, then
 * posts it with the viewer's existing Share dialog.
 */
export function attachViewer(profiler, options, controls) {
    if (!options.viewerUrl) {
        throw new Error("perfr.attachViewer needs viewerUrl: the perfr viewer index.html");
    }
    const popup = options.popup === true;
    const overlay = !popup && !options.container && options.overlay !== false;
    const url = withEmbed(options.viewerUrl);
    // A cross-origin viewer is in another agent cluster. Posting it a
    // SharedArrayBuffer fails on its side, and the port in that message is lost.
    const canShareMemory = new URL(url).origin === globalThis.location?.origin;
    let frame = null;
    let shell = null;
    let win = null;
    let port = null;
    let sab = null;
    let usingSharedBuffer = false;
    let visible = true;
    const hotkey = options.hotkey === undefined ? "F8" : options.hotkey;
    if (popup) {
        win = window.open(url, "perfr", "width=1100,height=760");
        if (!win)
            throw new Error("perfr could not open a viewer window. Call attachViewer from a click.");
    }
    else {
        shell = document.createElement("div");
        shell.className = "perfr-embed";
        const header = document.createElement("div");
        header.className = "perfr-embed-bar";
        header.textContent = overlay && hotkey ? `perfr  ·  ${hotkey} hides this panel` : "perfr";
        const close = document.createElement("button");
        close.type = "button";
        close.textContent = "Hide";
        close.addEventListener("click", () => handle.hide());
        header.append(close);
        frame = document.createElement("iframe");
        frame.className = "perfr-embed-frame";
        frame.src = url;
        frame.title = "perfr";
        shell.append(header, frame);
        ensureStyles();
        if (overlay) {
            shell.classList.add("perfr-embed-overlay");
            if (options.width && options.width > 0)
                shell.style.width = `${options.width}px`;
            if (options.height && options.height > 0)
                shell.style.height = `${options.height}px`;
            document.body.append(shell);
        }
        else if (options.container) {
            options.container.append(shell);
            shell.classList.add("perfr-embed-fill");
        }
        else {
            document.body.append(shell);
        }
        win = frame.contentWindow;
    }
    const send = (bytes, stats) => {
        if (!port)
            return;
        if (sab && bytes.length && writeBytes(sab, bytes)) {
            port.postMessage({ source: SOURCE, kind: "stats", stats, sabBytes: bytes.length });
            return;
        }
        const copy = bytes.length ? bytes.slice() : new Uint8Array();
        const payload = { source: SOURCE, kind: "frames", stats };
        if (copy.byteLength) {
            payload.bytes = copy.buffer;
            port.postMessage(payload, [copy.buffer]);
        }
        else {
            port.postMessage(payload);
        }
    };
    const unlisten = profiler.onFlush((packet) => send(packet.bytes, packet.stats));
    const onWindowMessage = (ev) => {
        const data = ev.data;
        if (!data || data.source !== SOURCE || data.kind !== "ready")
            return;
        const from = ev.source;
        if (popup) {
            if (from !== win)
                return;
        }
        else if (frame && from !== frame.contentWindow) {
            return;
        }
        const channel = new MessageChannel();
        port = channel.port1;
        port.onmessage = (msg) => {
            const body = msg.data;
            if (!body)
                return;
            if (body.kind === "socket") {
                profiler.resync();
            }
            else if (body.kind === "export-request" && body.id != null) {
                void profiler.exportStream().then((bytes) => {
                    const copy = bytes.slice();
                    port?.postMessage({ source: SOURCE, kind: "export", id: body.id, bytes: copy.buffer }, [copy.buffer]);
                });
            }
            else if (body.kind === "control") {
                if (body.action === "stop")
                    controls ? controls.stop() : profiler.stop();
                else if (body.action === "start")
                    controls ? controls.start() : profiler.resume();
                else if (body.action === "clear")
                    controls ? controls.clear() : profiler.clear();
            }
        };
        sab = canShareMemory && sabAvailable() ? createByteRing(1 << 20) : null;
        usingSharedBuffer = sab !== null;
        from?.postMessage({ source: SOURCE, kind: "port", sab }, "*", [channel.port2]);
        profiler.resync();
    };
    window.addEventListener("message", onWindowMessage);
    const onKey = (ev) => {
        if (!hotkey || popup)
            return;
        if (ev.key !== hotkey || ev.metaKey || ev.ctrlKey || ev.altKey)
            return;
        if (isTyping(ev.target))
            return;
        ev.preventDefault();
        handle.toggle();
    };
    window.addEventListener("keydown", onKey);
    const handle = {
        element: shell,
        get usingSharedBuffer() {
            return usingSharedBuffer;
        },
        show() {
            visible = true;
            shell?.classList.remove("perfr-embed-hidden");
        },
        hide() {
            visible = false;
            shell?.classList.add("perfr-embed-hidden");
        },
        toggle() {
            if (visible)
                handle.hide();
            else
                handle.show();
        },
        close() {
            unlisten();
            window.removeEventListener("message", onWindowMessage);
            window.removeEventListener("keydown", onKey);
            port?.close();
            shell?.remove();
            if (popup)
                win?.close();
        },
    };
    return handle;
}
const STYLE_ID = "perfr-embed-style";
/** One stylesheet for every mode, added once per document. */
function ensureStyles() {
    if (document.getElementById(STYLE_ID))
        return;
    const style = document.createElement("style");
    style.id = STYLE_ID;
    style.textContent = embedCss();
    document.head.append(style);
}
function embedCss() {
    return `
.perfr-embed-overlay {
  position: fixed;
  z-index: 2147483000;
  right: 12px;
  bottom: 12px;
  width: min(960px, 100vw);
  height: 42vh;
  display: flex;
  flex-direction: column;
  background: #14181e;
  color: #e7e9ee;
  border: 1px solid #2c3340;
  border-radius: 8px;
  box-shadow: 0 12px 40px rgba(0,0,0,.45);
  overflow: hidden;
  font: 12px/1.3 ui-sans-serif, system-ui, sans-serif;
}
.perfr-embed-fill { display: flex; flex-direction: column; width: 100%; height: 100%; min-height: 240px; background: #14181e; }
.perfr-embed-hidden { display: none !important; }
.perfr-embed-bar {
  display: flex; align-items: center; justify-content: space-between;
  gap: 8px; padding: 4px 8px; background: #0e1116; color: #c5cad3;
}
.perfr-embed-bar button {
  background: transparent; color: inherit; border: 1px solid #3a4250;
  border-radius: 4px; padding: 2px 8px; cursor: pointer;
}
.perfr-embed-frame { flex: 1; width: 100%; border: 0; background: #0b0c0e; }
`;
}
