// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Embed mode for a page that profiles itself. Active only for ?embed=1
// without ?capture=. A capture URL keeps the normal loader: this file
// returns immediately and does not replace WebSocket or fetch.
(function () {
  var params = new URLSearchParams(location.search);
  if (params.get("embed") !== "1" || params.get("capture")) return;

  var SOURCE = "perfr-embed";
  var PAGE_PID = 0xB10B0001;
  var port = null;
  var sab = null;
  var socket = null;
  var stats = {
    capturing: true,
    demo: false,
    hooks: false,
    events_live: 0,
    events_capacity: 200000,
    dropped: 0,
    spilled: 0,
    produced: 0,
    oldest_start_ns: 0,
    newest_end_ns: 0,
    ring_bytes: 0,
    machine: "browser",
    target_pid: PAGE_PID,
    service_pid: 0,
    wire: "raw",
    instrumentation: "in-page",
    hook_crash: "",
    auto_profile: false,
  };
  var exportWait = new Map();
  var exportSeq = 1;

  window.__perfrEmbedActive = true;

  function hostWindow() {
    if (window.parent && window.parent !== window) return window.parent;
    return window.opener || null;
  }

  function deliver(bytes) {
    if (!socket || socket.readyState !== 1 || !bytes || !bytes.byteLength) return;
    var ev = new MessageEvent("message", { data: bytes.buffer ? bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) : bytes });
    if (typeof socket.onmessage === "function") socket.onmessage(ev);
  }

  // Take one write off the shared ring. The page announces every write on
  // the port with its length, and the ring is read only here, as that message
  // arrives. Port messages are ordered, so frames in the ring and frames that
  // fell back to the port reach the socket in the order they were encoded.
  // Reading ahead (on a timer, or everything at once) would let a later ring
  // write overtake an earlier port message: events ahead of the Hello that
  // then drops them. `want` is absent from a page that does not send lengths;
  // then everything written so far is taken.
  function takeSab(want) {
    if (!sab) return;
    var header = new Uint32Array(sab, 0, 4);
    var cap = Atomics.load(header, 2);
    if (!cap) return;
    var read = Atomics.load(header, 1);
    var write = Atomics.load(header, 0);
    var n = (write - read + cap) % cap;
    if (typeof want === "number" && want < n) n = want;
    if (n <= 0) return;
    var data = new Uint8Array(sab, 16, cap);
    var out = new Uint8Array(n);
    for (var i = 0; i < n; i++) out[i] = data[(read + i) % cap];
    Atomics.store(header, 1, (read + n) % cap);
    deliver(out);
  }

  function applyStats(next) {
    if (!next) return;
    if (typeof next.live === "number") stats.events_live = next.live;
    if (typeof next.capacity === "number") stats.events_capacity = next.capacity;
    if (typeof next.dropped === "number") stats.dropped = next.dropped;
    if (typeof next.produced === "number") stats.produced = next.produced;
    if (typeof next.capturing === "boolean") stats.capturing = next.capturing;
    stats.ring_bytes = stats.events_live * 32;
  }

  function onPortMessage(ev) {
    var data = ev.data || {};
    if (data.kind === "frames") {
      applyStats(data.stats);
      if (data.bytes) deliver(new Uint8Array(data.bytes));
    } else if (data.kind === "stats") {
      applyStats(data.stats);
      takeSab(data.sabBytes);
    } else if (data.kind === "export") {
      var wait = exportWait.get(data.id);
      if (wait) {
        exportWait.delete(data.id);
        wait(data.bytes ? new Uint8Array(data.bytes) : new Uint8Array());
      }
    }
  }

  window.addEventListener("message", function (ev) {
    var data = ev.data || {};
    if (data.source !== SOURCE || data.kind !== "port" || !ev.ports || !ev.ports[0]) return;
    // Only the page that mounted this viewer may hand it a port: whatever
    // holds the port feeds the timeline and answers Share's export. Another
    // frame of that page, or a window that merely has a reference to this
    // one, is not it.
    if (ev.source !== hostWindow()) return;
    port = ev.ports[0];
    sab = data.sab || null;
    port.onmessage = onPortMessage;
    port.start();
    if (socket && socket.readyState === 1) {
      port.postMessage({ source: SOURCE, kind: "socket" });
    }
  });

  function EmbedSocket(url) {
    if (!String(url).endsWith("/ws")) return new RealWS(url);
    if (!(this instanceof EmbedSocket)) return new EmbedSocket(url);
    this.url = String(url);
    this.readyState = 0;
    this.binaryType = "blob";
    this.onopen = null;
    this.onmessage = null;
    this.onerror = null;
    this.onclose = null;
    var self = this;
    socket = this;
    queueMicrotask(function () {
      self.readyState = 1;
      if (typeof self.onopen === "function") self.onopen(new Event("open"));
      if (port) port.postMessage({ source: SOURCE, kind: "socket" });
    });
  }
  var RealWS = window.WebSocket;
  EmbedSocket.CONNECTING = 0;
  EmbedSocket.OPEN = 1;
  EmbedSocket.CLOSING = 2;
  EmbedSocket.CLOSED = 3;
  EmbedSocket.prototype.send = function () {};
  EmbedSocket.prototype.close = function () {
    this.readyState = 3;
    if (typeof this.onclose === "function") this.onclose(new CloseEvent("close"));
  };
  EmbedSocket.prototype.addEventListener = function (type, fn) {
    if (type === "open") this.onopen = fn;
    else if (type === "message") this.onmessage = fn;
    else if (type === "close") this.onclose = fn;
    else if (type === "error") this.onerror = fn;
  };
  EmbedSocket.prototype.removeEventListener = function () {};
  window.WebSocket = EmbedSocket;

  function jsonResponse(body, status) {
    return new Response(JSON.stringify(body), {
      status: status || 200,
      headers: { "content-type": "application/json" },
    });
  }

  // The page's ring as one stream, or null when the page did not answer:
  // no port yet, or no reply in three seconds (a page busy enough to need
  // profiling can be). Null is not an empty capture, and must not be
  // uploaded or saved as one.
  function requestExport() {
    if (!port) return Promise.resolve(null);
    var id = exportSeq++;
    var p = new Promise(function (resolve) {
      exportWait.set(id, resolve);
      setTimeout(function () {
        if (exportWait.has(id)) {
          exportWait.delete(id);
          resolve(null);
        }
      }, 3000);
    });
    port.postMessage({ source: SOURCE, kind: "export-request", id: id });
    return p;
  }

  function control(action) {
    if (port) port.postMessage({ source: SOURCE, kind: "control", action: action });
  }

  var realBeacon = navigator.sendBeacon ? navigator.sendBeacon.bind(navigator) : null;
  if (realBeacon) {
    navigator.sendBeacon = function (url, data) {
      var path = localApiPath(url);
      if (path !== null && path.indexOf("/api/log") !== -1) return true;
      return realBeacon(url, data);
    };
  }

  // The service API the viewer would talk to is on this page's origin.
  // perfr.dev (Share, /api/me) and anything else cross-origin passes through.
  function localApiPath(url) {
    var text = typeof url === "string" ? url : url && url.url ? url.url : String(url || "");
    var parsed;
    try {
      parsed = new URL(text, location.href);
    } catch (e) {
      return null;
    }
    if (parsed.origin !== location.origin) return null;
    var path = parsed.pathname + parsed.search;
    return path.indexOf("/api/") === -1 ? null : path;
  }

  var realFetch = window.fetch.bind(window);
  window.fetch = function (input, init) {
    var url = localApiPath(input);
    var method = (init && init.method ? init.method : "GET").toUpperCase();
    if (url === null) return realFetch(input, init);
    if (url.indexOf("/api/status") !== -1) return Promise.resolve(jsonResponse(stats));
    if (url.indexOf("/api/processes") !== -1) {
      return Promise.resolve(jsonResponse([{ pid: PAGE_PID, name: "this page", cpu: 0, path: location.href }]));
    }
    if (url.indexOf("/api/settings") !== -1) return Promise.resolve(jsonResponse({}));
    if (url.indexOf("/api/timeline") !== -1) {
      return Promise.resolve(jsonResponse({ lod: "instanced", width: 16, height: 0, instances: [] }));
    }
    if (url.indexOf("/api/capture/export") !== -1 && url.indexOf("format=stream") !== -1) {
      return requestExport().then(function (bytes) {
        if (!bytes) {
          return new Response("the page did not answer the export request", { status: 504, headers: { "content-type": "text/plain" } });
        }
        return new Response(bytes, { status: 200, headers: { "content-type": "application/octet-stream" } });
      });
    }
    if (method === "POST" && url.indexOf("/api/capture/stop") !== -1) {
      stats.capturing = false;
      control("stop");
      return Promise.resolve(jsonResponse({}));
    }
    if (method === "POST" && url.indexOf("/api/demo/stop") !== -1) {
      stats.capturing = false;
      control("stop");
      return Promise.resolve(jsonResponse({}));
    }
    if (method === "POST" && (url.indexOf("/api/demo/start") !== -1 || url.indexOf("/api/capture/start") !== -1)) {
      stats.capturing = true;
      control("start");
      return Promise.resolve(jsonResponse({}));
    }
    if (method === "POST" && url.indexOf("/api/capture/clear") !== -1) {
      control("clear");
      stats.capturing = true;
      return Promise.resolve(jsonResponse({}));
    }
    if (url.indexOf("/api/log") !== -1) return Promise.resolve(jsonResponse({}));
    return Promise.resolve(jsonResponse({}));
  };

  var realOpen = window.open;
  window.open = function (url) {
    var text = localApiPath(url) || "";
    if (text.indexOf("/api/capture/export") !== -1 && text.indexOf("format=stream") !== -1) {
      requestExport().then(function (bytes) {
        if (!bytes || !bytes.byteLength) return;
        var blob = new Blob([bytes], { type: "application/octet-stream" });
        var link = document.createElement("a");
        link.href = URL.createObjectURL(blob);
        link.download = "capture.perfr.stream";
        link.click();
        setTimeout(function () { URL.revokeObjectURL(link.href); }, 1000);
      });
      return null;
    }
    return realOpen.apply(window, arguments);
  };

  var host = hostWindow();
  if (host) host.postMessage({ source: SOURCE, kind: "ready" }, "*");
})();
