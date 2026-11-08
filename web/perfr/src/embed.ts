// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import type { Profiler, Stats } from "./profiler.js";
import { createByteRing, sabAvailable, writeBytes } from "./sab.js";

export interface AttachViewerOptions {
  /** Element the viewer fills. Omit for an overlay panel or a popup. */
  container?: HTMLElement;
  /** Floating panel. Default when `container` and `popup` are omitted. */
  overlay?: boolean;
  /** Open the viewer in its own window. Popups must be opened from a click. */
  popup?: boolean;
  /**
   * Toggle the overlay. Default "F8". `null` disables the hotkey.
   * Ignored for a popup.
   */
  hotkey?: string | null;
  /**
   * URL of the perfr viewer's `index.html` (the files in
   * `crates/perfr-live-viewer/viewer-dist`). The page is opened with
   * `embed=1`. `?capture=` on that URL is left alone and embed mode stays off.
   */
  viewerUrl: string;
  /** Panel width in CSS pixels. Overlay only. */
  width?: number;
  /** Panel height in CSS pixels. Overlay only. */
  height?: number;
}

export interface ViewerHandle {
  close(): void;
  show(): void;
  hide(): void;
  toggle(): void;
  readonly element: HTMLElement | null;
  readonly usingSharedBuffer: boolean;
}

const SOURCE = "perfr-embed";

function withEmbed(viewerUrl: string): string {
  const url = new URL(viewerUrl, globalThis.location?.href ?? "http://localhost/");
  if (!url.searchParams.has("capture")) url.searchParams.set("embed", "1");
  return url.toString();
}

function isTyping(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || target.isContentEditable;
}

/**
 * Mount the existing live viewer and stream this profiler's frames into it.
 * The viewer page (embed.js) accepts the stream on a MessageChannel. When the
 * page is cross-origin isolated, encoded bytes also go through a
 * SharedArrayBuffer; the channel is the fallback when that ring is full or
 * SharedArrayBuffer is missing.
 *
 * Share in the viewer reads the ring back through the same channel, then
 * posts it with the viewer's existing Share dialog.
 */
export function attachViewer(profiler: Profiler, options: AttachViewerOptions): ViewerHandle {
  if (!options.viewerUrl) {
    throw new Error("perfr.attachViewer needs viewerUrl: the perfr viewer index.html");
  }
  const popup = options.popup === true;
  const overlay = !popup && !options.container && options.overlay !== false;
  const url = withEmbed(options.viewerUrl);

  let frame: HTMLIFrameElement | null = null;
  let shell: HTMLElement | null = null;
  let win: Window | null = null;
  let port: MessagePort | null = null;
  let sab: SharedArrayBuffer | null = null;
  let usingSharedBuffer = false;
  let visible = true;
  const hotkey = options.hotkey === undefined ? "F8" : options.hotkey;

  if (popup) {
    win = window.open(url, "perfr", "width=1100,height=760");
    if (!win) throw new Error("perfr could not open a viewer window. Call attachViewer from a click.");
  } else {
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
    if (overlay) {
      const style = document.createElement("style");
      style.textContent = overlayCss(options.width ?? 0, options.height ?? 320);
      document.head.append(style);
      shell.classList.add("perfr-embed-overlay");
      document.body.append(shell);
    } else if (options.container) {
      options.container.append(shell);
      shell.classList.add("perfr-embed-fill");
    } else {
      document.body.append(shell);
    }
    win = frame.contentWindow;
  }

  const send = (bytes: Uint8Array, stats: Stats) => {
    if (!port) return;
    if (sab && bytes.length && writeBytes(sab, bytes)) {
      port.postMessage({ source: SOURCE, kind: "stats", stats });
      return;
    }
    const copy = bytes.length ? bytes.slice() : new Uint8Array();
    const payload: Record<string, unknown> = { source: SOURCE, kind: "frames", stats };
    if (copy.byteLength) {
      payload.bytes = copy.buffer;
      port.postMessage(payload, [copy.buffer]);
    } else {
      port.postMessage(payload);
    }
  };

  const unlisten = profiler.onFlush((packet) => send(packet.bytes, packet.stats));

  const onWindowMessage = (ev: MessageEvent) => {
    const data = ev.data as { source?: string; kind?: string } | null;
    if (!data || data.source !== SOURCE || data.kind !== "ready") return;
    const from = ev.source as Window | null;
    if (popup) {
      if (from !== win) return;
    } else if (frame && from !== frame.contentWindow) {
      return;
    }
    const channel = new MessageChannel();
    port = channel.port1;
    port.onmessage = (msg: MessageEvent) => {
      const body = msg.data as { kind?: string; id?: number; action?: string };
      if (!body) return;
      if (body.kind === "socket") {
        profiler.resync();
      } else if (body.kind === "export-request" && body.id != null) {
        void profiler.exportStream().then((bytes) => {
          const copy = bytes.slice();
          port?.postMessage(
            { source: SOURCE, kind: "export", id: body.id, bytes: copy.buffer },
            [copy.buffer],
          );
        });
      } else if (body.kind === "control") {
        if (body.action === "stop") profiler.stop();
        else if (body.action === "start") void profiler.start();
        else if (body.action === "clear") profiler.clear();
      }
    };
    if (sabAvailable()) {
      sab = createByteRing(1 << 20);
      usingSharedBuffer = true;
    }
    from?.postMessage({ source: SOURCE, kind: "port", sab }, "*", [channel.port2]);
    profiler.resync();
  };
  window.addEventListener("message", onWindowMessage);

  const onKey = (ev: KeyboardEvent) => {
    if (!hotkey || popup) return;
    if (ev.key !== hotkey || ev.metaKey || ev.ctrlKey || ev.altKey) return;
    if (isTyping(ev.target)) return;
    ev.preventDefault();
    handle.toggle();
  };
  window.addEventListener("keydown", onKey);

  const handle: ViewerHandle = {
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
      if (visible) handle.hide();
      else handle.show();
    },
    close() {
      unlisten();
      window.removeEventListener("message", onWindowMessage);
      window.removeEventListener("keydown", onKey);
      port?.close();
      shell?.remove();
      if (popup) win?.close();
    },
  };
  return handle;
}

function overlayCss(width: number, height: number): string {
  const h = height > 0 ? `${height}px` : "42vh";
  const w = width > 0 ? `${width}px` : "min(960px, 100vw)";
  return `
.perfr-embed-overlay {
  position: fixed;
  z-index: 2147483000;
  right: 12px;
  bottom: 12px;
  width: ${w};
  height: ${h};
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
