// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { perfr } from "../perfr/dist/index.js";

const canvas = document.getElementById("view");
const ctx = canvas.getContext("2d");
const readout = document.getElementById("readout");

perfr.nameProcess("perfr demo");

const balls = Array.from({ length: 48 }, (_, i) => ({
  x: 40 + (i * 37) % 800,
  y: 40 + (i * 53) % 400,
  vx: ((i % 7) - 3) * 0.7,
  vy: ((i % 5) - 2) * 0.8,
  r: 6 + (i % 5),
  color: ["#7aa2f7", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7"][i % 5],
}));

const worker = new Worker(new URL("./sim-worker.js", import.meta.url), { type: "module" });
perfr.attachWorker(worker, { name: "sim" });
let workerN = 0;
worker.onmessage = (ev) => {
  if (ev.data && ev.data.source === "perfr-worker") return;
  workerN = ev.data && ev.data.n ? ev.data.n : workerN;
};

let stall = 0;
document.getElementById("stall").addEventListener("click", () => {
  stall = 80;
});
document.getElementById("download").addEventListener("click", () => {
  void perfr.download("demo.perfr.stream");
});

performance.mark("demo-start");

const viewerUrl = new URL("../../crates/perfr-live-viewer/viewer-dist/index.html", import.meta.url);
const viewer = perfr.attachViewer({
  viewerUrl: viewerUrl.href,
  overlay: true,
  hotkey: "F8",
  height: 340,
});
window.__perfrViewer = viewer;

let frame = 0;
let last = performance.now();

function resize() {
  const dpr = Math.min(2, window.devicePixelRatio || 1);
  canvas.width = Math.floor(canvas.clientWidth * dpr);
  canvas.height = Math.floor(canvas.clientHeight * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
resize();
window.addEventListener("resize", resize);

function tick(now) {
  frame++;
  const dt = Math.min(50, now - last);
  last = now;
  perfr.scope("frame", () => {
    perfr.scope("update", () => {
      const w = canvas.clientWidth;
      const h = canvas.clientHeight;
      for (const b of balls) {
        b.x += b.vx * dt * 0.06;
        b.y += b.vy * dt * 0.06;
        if (b.x < b.r || b.x > w - b.r) b.vx *= -1;
        if (b.y < b.r || b.y > h - b.r) b.vy *= -1;
      }
      if (frame % 10 === 0) worker.postMessage({ balls: balls.length, t: now });
    });
    perfr.scope("render", () => {
      ctx.clearRect(0, 0, canvas.clientWidth, canvas.clientHeight);
      for (const b of balls) {
        ctx.fillStyle = b.color;
        ctx.beginPath();
        ctx.arc(b.x, b.y, b.r, 0, Math.PI * 2);
        ctx.fill();
      }
    });
    if (stall > 0) {
      perfr.scope("stall", () => {
        const until = performance.now() + stall;
        while (performance.now() < until) {}
      });
      stall = 0;
    }
    perfr.value("balls", balls.length);
    perfr.frame();
  });
  if (frame % 30 === 0) {
    const stats = perfr.stats();
    readout.textContent = ` ${stats.produced | 0} events`;
    performance.measure("hud", "demo-start");
  }
  if (frame === 40) {
    fetch(new URL("./ping.txt", import.meta.url)).catch(() => {});
  }
  requestAnimationFrame(tick);
}

requestAnimationFrame(tick);
window.__perfr = perfr;
