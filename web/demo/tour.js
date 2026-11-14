// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// The @perfr/web API, one button per call family, with the viewer mounted
// in a container. `window.__tour` is the same set of actions for a script.

import { perfr, TID } from "../perfr/dist/index.js";

const $ = (id) => document.getElementById(id);
const log = $("log");
const t0 = performance.now();

function say(text) {
  const line = document.createElement("div");
  const t = document.createElement("span");
  t.className = "t";
  t.textContent = `${((performance.now() - t0) / 1000).toFixed(2)}s  `;
  line.append(t, document.createTextNode(text));
  log.prepend(line);
  while (log.childElementCount > 200) log.lastChild.remove();
}

perfr.nameProcess("api tour");
perfr.nameThread("main");

// --- the viewer, in the page rather than floating over it ----------------
const viewerUrl = new URL("../../crates/perfr-client/viewer-dist/index.html", import.meta.url);
const viewer = perfr.attachViewer({
  viewerUrl: viewerUrl.href,
  container: $("viewer"),
  hotkey: null,
});

// --- a worker that calls installWorker -----------------------------------
const worker = new Worker(new URL("./sim-worker.js", import.meta.url), { type: "module" });
const workerTid = perfr.attachWorker(worker, { name: "sim" });
let workerReplies = 0;
worker.onmessage = (ev) => {
  if (ev.data && ev.data.source === "perfr-worker") return;
  workerReplies++;
};

// --- one function per API family -----------------------------------------
class Body {
  constructor() {
    this.x = 0;
  }
}
// `@perfr.decorate("physics")` in TypeScript. In plain JS the decorator is a
// function that takes the method and its context.
const physics = perfr.decorate("physics")(
  function physics(bodies) {
    for (const b of bodies) b.x += Math.sin(b.x + 1) * 0.01;
  },
  { kind: "method", name: "physics" },
);

const bodies = Array.from({ length: 64 }, () => new Body());

function busy(ms) {
  const until = performance.now() + ms;
  let n = 0;
  while (performance.now() < until) n += Math.sqrt(n + 1);
  return n;
}

const actions = {
  scopes() {
    for (let i = 0; i < 8; i++) {
      perfr.scope("frame", () => {
        perfr.scope("update", () => {
          physics(bodies);
          busy(0.3);
        });
        perfr.scope("render", () => busy(0.5));
        perfr.frame();
      });
    }
    say("8 frames: scope(frame) > scope(update) > decorated physics, then scope(render) and frame()");
  },

  async async() {
    const url = new URL("./ping.txt", import.meta.url);
    const one = perfr.beginAsync("level load");
    await Promise.all([
      perfr.scopeAsync("fetch ping 1", () => fetch(`${url}?1`).then((r) => r.text())),
      perfr.scopeAsync("fetch ping 2", () => fetch(`${url}?2`).then((r) => r.text())),
      perfr.scopeAsync("decode", () => new Promise((r) => setTimeout(r, 30))),
    ]);
    perfr.endAsync(one);
    say("async: beginAsync(level load) around three overlapping scopeAsync spans; each takes its own lane");
  },

  values() {
    return new Promise((resolve) => {
      const start = performance.now();
      let n = 0;
      const step = (now) => {
        const t = (now - start) / 1000;
        perfr.value("sine", Math.sin(t * Math.PI * 2) * 50 + 50);
        perfr.plot("bodies", bodies.length);
        if (n++ % 15 === 0) perfr.mark(`checkpoint ${n}`);
        perfr.frame();
        if (t < 1) requestAnimationFrame(step);
        else {
          say(`values: ${n} frames of value(sine), plot(bodies), a mark every 15 frames, frame()`);
          resolve();
        }
      };
      requestAnimationFrame(step);
    });
  },

  worker() {
    const before = workerReplies;
    for (let i = 0; i < 20; i++) worker.postMessage({ balls: 64, t: performance.now() + i });
    return new Promise((resolve) => {
      const wait = () => {
        if (workerReplies - before >= 20) {
          say(`worker: 20 messages, 20 replies. The worker's step scopes are thread ${workerTid} "sim"; send/recv marks are on Worker messages`);
          resolve();
        } else setTimeout(wait, 20);
      };
      wait();
    });
  },

  stall() {
    perfr.scope("stall 80ms", () => busy(80));
    say("long task: an 80 ms busy loop inside scope(stall 80ms). Chromium also reports it on Long tasks and the Frames track marks the jank");
  },

  shift() {
    const box = $("shift-box");
    box.style.height = box.style.height === "60px" ? "0px" : "60px";
    say("layout shift: a block above the controls changed height with no input, so Layout shifts gets an entry and CLS moves");
  },

  timing() {
    performance.mark("tour-start");
    busy(5);
    performance.mark("tour-end");
    performance.measure("tour measure", "tour-start", "tour-end");
    say("user timing: performance.mark ×2 and performance.measure land on the User Timing track");
  },

  async all() {
    actions.scopes();
    await actions.async();
    await actions.values();
    await actions.worker();
    actions.stall();
    actions.shift();
    actions.timing();
    say("ran everything");
  },

  stop() {
    perfr.stop();
    say("stop: open scopes closed, ring kept, further scopes dropped until Start");
  },

  async start() {
    await perfr.start();
    say("start: recording again into the same ring; the viewer receives the ring as a fresh stream");
  },

  clear() {
    perfr.clear();
    $("summary").hidden = true;
    say("clear: ring dropped");
  },

  async export() {
    const bytes = await perfr.exportStream();
    const summary = decode(bytes);
    $("summary").textContent = summary.text;
    $("summary").hidden = false;
    say(`export: ${bytes.length} bytes, ${summary.events} events, ${summary.threads.size} threads`);
    return summary;
  },

  download() {
    return perfr.download("tour.perfr.stream");
  },
};

// --- a reader for the stream, enough to count what came out ---------------
const FRAME = { 1: "hello", 2: "events", 3: "string", 4: "status", 5: "finished", 6: "started", 7: "thread", 8: "process" };

function decode(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const frames = {};
  const names = new Map();
  const threads = new Map();
  const perThread = new Map();
  const byName = new Map();
  let events = 0;
  let o = 0;
  let process = "";
  const dec = new TextDecoder();
  while (o + 5 <= bytes.length) {
    const len = view.getUint32(o, true);
    const typ = bytes[o + 4];
    const p = o + 5;
    frames[FRAME[typ] || `type ${typ}`] = (frames[FRAME[typ] || `type ${typ}`] || 0) + 1;
    if (typ === 3) {
      const id = view.getUint32(p, true);
      const n = view.getUint32(p + 4, true);
      names.set(id, dec.decode(bytes.subarray(p + 8, p + 8 + n)));
    } else if (typ === 7) {
      const tid = view.getUint32(p + 4, true);
      const n = view.getUint32(p + 8, true);
      threads.set(tid, dec.decode(bytes.subarray(p + 12, p + 12 + n)));
    } else if (typ === 8) {
      const n = view.getUint32(p + 4, true);
      process = dec.decode(bytes.subarray(p + 8, p + 8 + n));
    } else if (typ === 2) {
      const n = view.getUint32(p, true);
      for (let i = 0; i < n; i++) {
        const b = p + 4 + i * 32;
        const tid = view.getUint32(b + 16, true);
        const name = view.getUint32(b + 28, true);
        perThread.set(tid, (perThread.get(tid) || 0) + 1);
        byName.set(name, (byName.get(name) || 0) + 1);
        events++;
      }
    }
    o += 5 + len;
  }
  const lines = [];
  lines.push(`${bytes.length} bytes, ${events} events, process "${process}", pid 0x${perfr.pagePid.toString(16)}`);
  lines.push("frames: " + Object.entries(frames).map(([k, v]) => `${k} ${v}`).join(", "));
  lines.push("events per thread:");
  for (const [tid, n] of [...perThread.entries()].sort((a, b) => b[1] - a[1])) {
    lines.push(`  ${String(n).padStart(6)}  ${threads.get(tid) || `tid ${tid}`}`);
  }
  lines.push("top names:");
  for (const [id, n] of [...byName.entries()].sort((a, b) => b[1] - a[1]).slice(0, 10)) {
    lines.push(`  ${String(n).padStart(6)}  ${names.get(id) || `name ${id}`}`);
  }
  return { text: lines.join("\n"), events, threads, perThread, byName, names, frames, bytes: bytes.length };
}

// --- wiring ---------------------------------------------------------------
for (const [id, fn] of Object.entries({
  "b-scopes": actions.scopes,
  "b-async": actions.async,
  "b-values": actions.values,
  "b-worker": actions.worker,
  "b-stall": actions.stall,
  "b-shift": actions.shift,
  "b-timing": actions.timing,
  "b-all": actions.all,
  "b-stop": actions.stop,
  "b-start": actions.start,
  "b-clear": actions.clear,
  "b-export": actions.export,
  "b-download": actions.download,
})) {
  $(id).addEventListener("click", () => void fn());
}

const support = perfr.tracks.support();
const tbody = $("tracks").querySelector("tbody");
for (const [id, s] of Object.entries(support)) {
  const tr = document.createElement("tr");
  const toggle = document.createElement("input");
  toggle.type = "checkbox";
  toggle.checked = s.available;
  toggle.disabled = !s.available;
  toggle.id = `track-${id}`;
  toggle.addEventListener("change", () => {
    perfr.tracks.set({ [id]: toggle.checked });
    say(`${id}: ${toggle.checked ? "on" : "off"}`);
  });
  const c0 = document.createElement("td");
  c0.append(toggle);
  const c1 = document.createElement("td");
  c1.textContent = id;
  const c2 = document.createElement("td");
  c2.className = s.available ? "ok" : "no";
  c2.textContent = s.available ? "yes" : "no";
  c2.title = s.note;
  tr.append(c0, c1, c2);
  const note = document.createElement("tr");
  const nc = document.createElement("td");
  nc.colSpan = 3;
  nc.className = "note";
  nc.textContent = s.note;
  note.append(nc);
  tbody.append(tr, note);
}

function refresh() {
  const s = perfr.stats();
  $("s-produced").textContent = String(s.produced);
  $("s-live").textContent = `${s.live} / ${s.capacity}`;
  $("s-dropped").textContent = String(s.dropped);
  $("s-capturing").textContent = s.capturing ? "yes" : "no (stopped)";
  const frame = document.querySelector("iframe.perfr-embed-frame");
  const active = frame && frame.contentWindow && frame.contentWindow.__perfrEmbedActive === true;
  $("s-viewer").textContent = active ? "embed mode" : "loading";
  $("s-sab").textContent = viewer.usingSharedBuffer ? "yes (cross-origin isolated)" : crossOriginIsolated ? "no" : "no (page is not cross-origin isolated; MessageChannel)";
}
setInterval(refresh, 500);
refresh();
say(`page pid 0x${perfr.pagePid.toString(16)}, main tid ${TID.main}, worker tid ${workerTid}`);

window.__tour = { actions, decode, perfr, viewer };
window.__perfr = perfr;
window.__perfrViewer = viewer;
