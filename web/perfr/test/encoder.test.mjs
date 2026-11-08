// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { writeFileSync as writeFile } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import init, { WebProfiler, page_pid } from "../pkg/perfr_web.js";

const ready = readFile(new URL("../pkg/perfr_web_bg.wasm", import.meta.url)).then((bytes) =>
  init({ module_or_path: bytes }),
);

const HELLO = 1;
const EVENT_BATCH = 2;
const STRING = 3;
const THREAD = 7;
const PROCESS = 8;
const FINISHED = 5;

function decode(bytes) {
  const frames = [];
  let o = 0;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  while (o + 5 <= bytes.length) {
    const len = view.getUint32(o, true);
    const typ = bytes[o + 4];
    const payload = bytes.subarray(o + 5, o + 5 + len);
    frames.push({ typ, payload });
    o += 5 + len;
  }
  assert.equal(o, bytes.length, "stream ends on a frame boundary");
  return frames;
}

function eventsOf(frames) {
  const out = [];
  for (const frame of frames) {
    if (frame.typ !== EVENT_BATCH) continue;
    const view = new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength);
    const n = view.getUint32(0, true);
    for (let i = 0; i < n; i++) {
      const b = 4 + i * 32;
      out.push({
        start: view.getBigUint64(b, true),
        dur: view.getBigUint64(b + 8, true),
        tid: view.getUint32(b + 16, true),
        pid: view.getUint32(b + 20, true),
        kind: frame.payload[b + 24],
        depth: frame.payload[b + 25],
        name: view.getUint32(b + 28, true),
      });
    }
  }
  return out;
}

function stringsOf(frames) {
  const map = new Map();
  for (const frame of frames) {
    if (frame.typ !== STRING) continue;
    const view = new DataView(frame.payload.buffer, frame.payload.byteOffset, frame.payload.byteLength);
    const id = view.getUint32(0, true);
    const len = view.getUint32(4, true);
    const text = new TextDecoder().decode(frame.payload.subarray(8, 8 + len));
    map.set(id, text);
  }
  return map;
}

test("wasm encoder round-trips nested scopes, async spans and values", async () => {
  await ready;
  assert.equal(page_pid(), 0xb10b0001);
  const profiler = new WebProfiler(64);
  profiler.name_process("demo");
  profiler.name_thread(1, "main");
  const outer = profiler.begin("frame", 1, 1_000);
  const inner = profiler.begin("update", 1, 1_200);
  profiler.end(inner, 1_500);
  profiler.end(outer, 2_000);
  const load = profiler.begin_async("load", 1, 1_100);
  const other = profiler.begin_async("decode", 1, 1_150);
  profiler.end_async(load, 1_800);
  profiler.end_async(other, 1_900);
  profiler.value("fps", 2, 59.5, 2_000);
  const bytes = profiler.snapshot(2_000);
  assert.equal(String.fromCharCode(...bytes.subarray(5, 9)), "OLIV");
  const frames = decode(bytes);
  assert.ok(frames.some((f) => f.typ === HELLO));
  assert.ok(frames.some((f) => f.typ === FINISHED));
  assert.ok(frames.some((f) => f.typ === PROCESS));
  assert.ok(frames.some((f) => f.typ === THREAD));
  const names = stringsOf(frames);
  const events = eventsOf(frames);
  const byName = (text) => events.filter((e) => names.get(e.name) === text);
  const frame = byName("frame")[0];
  const update = byName("update")[0];
  assert.equal(frame.kind, 1);
  assert.equal(frame.depth, 0);
  assert.equal(frame.dur, 1000n);
  assert.equal(update.depth, 1);
  assert.equal(update.dur, 300n);
  const asyncEvents = events.filter((e) => e.kind === 5);
  assert.equal(asyncEvents.length, 2);
  assert.deepEqual(asyncEvents.map((e) => e.depth).sort(), [0, 1]);
  const fps = byName("fps")[0];
  assert.equal(fps.kind, 6);
  const bits = Number(fps.dur & 0xffffffffn);
  const value = new DataView(Uint32Array.of(bits).buffer).getFloat32(0, true);
  assert.ok(Math.abs(value - 59.5) < 0.01);
  assert.equal(fps.pid, 0xb10b0001);

  const fixture = fileURLToPath(new URL("../../demo/sample.perfr.stream", import.meta.url));
  writeFile(fixture, bytes);
});

test("ring drops the oldest event", async () => {
  await ready;
  const profiler = new WebProfiler(2);
  profiler.span("a", 1, 1, 2);
  profiler.span("b", 1, 3, 4);
  profiler.span("c", 1, 5, 6);
  assert.equal(profiler.dropped(), 1);
  assert.equal(profiler.live(), 2);
  const names = stringsOf(decode(profiler.snapshot(6)));
  const events = eventsOf(decode(profiler.snapshot(6)));
  assert.equal(events.length, 2);
  const texts = events.map((e) => names.get(e.name));
  assert.deepEqual(texts.sort(), ["b", "c"]);
});

test("flush is empty until something new is recorded", async () => {
  await ready;
  const profiler = new WebProfiler(8);
  profiler.span("a", 1, 1, 2);
  const first = profiler.flush();
  assert.ok(first.length > 0);
  assert.equal(profiler.flush().length, 0);
  profiler.mark("hit", 1, 3);
  const second = profiler.flush();
  assert.ok(second.length > 0);
  const events = eventsOf(decode(second));
  assert.equal(events.length, 1);
  assert.equal(events[0].dur, 0n);
});
