// Headless comparison of https://ui.perfetto.dev and https://perfr.dev/viewer/
// on the same local files. See README.md for what is and is not measured.
//
//   node bench.mjs --runs 5
//   node bench.mjs --runs 1 --only chrome_scroll_without_vsync.pftrace,async-trace-1.json

import puppeteer from 'puppeteer-core';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const CHROME = process.env.CHROME || '/usr/bin/google-chrome';
const PERFR_URL = 'https://perfr.dev/viewer/';
const PERFETTO_URL = 'https://ui.perfetto.dev/';

const args = process.argv.slice(2);
function flag(name, fallback) {
  const i = args.indexOf(name);
  if (i === -1) return fallback;
  return args[i + 1];
}
const RUNS = Number(flag('--runs', '5'));
const ONLY = (flag('--only', '') || '').split(',').map((s) => s.trim()).filter(Boolean);
const TRACES_DIR = path.resolve(flag('--traces', '/tmp/traces-bench'));
const OUT = path.resolve(flag('--out', path.join(HERE, 'results', 'latest.json')));
const SHOTS = path.resolve(flag('--shots', path.join(HERE, '../../../docs/images')));
const POLL_MS = 50;

const spec = JSON.parse(fs.readFileSync(path.join(HERE, 'traces.json'), 'utf8'));
const traces = spec.timed.filter((t) => ONLY.length === 0 || ONLY.includes(t.file) || ONLY.includes(t.id));

function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

function rssTree(rootPid) {
  const out = execFileSync('ps', ['-eo', 'pid=,ppid=,rss='], { encoding: 'utf8' });
  const rows = [];
  for (const line of out.trim().split('\n')) {
    const [pid, ppid, rss] = line.trim().split(/\s+/).map(Number);
    if (pid) rows.push({ pid, ppid, rss });
  }
  const kids = new Map();
  const rssOf = new Map();
  for (const row of rows) {
    rssOf.set(row.pid, row.rss);
    if (!kids.has(row.ppid)) kids.set(row.ppid, []);
    kids.get(row.ppid).push(row.pid);
  }
  let sum = 0;
  let max = 0;
  let n = 0;
  const stack = [rootPid];
  const seen = new Set();
  while (stack.length) {
    const pid = stack.pop();
    if (seen.has(pid)) continue;
    seen.add(pid);
    const rss = rssOf.get(pid) || 0;
    sum += rss;
    if (rss > max) max = rss;
    n += 1;
    for (const kid of kids.get(pid) || []) stack.push(kid);
  }
  return { sumKb: sum, maxKb: max, procs: n };
}

function percentile(values, p) {
  if (!values.length) return null;
  const s = [...values].sort((a, b) => a - b);
  const i = (s.length - 1) * p;
  const lo = Math.floor(i);
  const hi = Math.ceil(i);
  if (lo === hi) return s[lo];
  return s[lo] * (hi - i) + s[hi] * (i - lo);
}

function frameStats(stamps) {
  const d = [];
  for (let i = 1; i < stamps.length; i++) d.push(stamps[i] - stamps[i - 1]);
  const span = stamps.length > 1 ? stamps[stamps.length - 1] - stamps[0] : 0;
  return {
    frames: stamps.length,
    spanMs: span,
    fps: span > 0 ? ((stamps.length - 1) * 1000) / span : null,
    p50: percentile(d, 0.5),
    p95: percentile(d, 0.95),
    p99: percentile(d, 0.99),
    max: d.length ? Math.max(...d) : null,
  };
}

function median(values) {
  const s = values.filter((v) => v != null && !Number.isNaN(v)).sort((a, b) => a - b);
  if (!s.length) return null;
  const mid = Math.floor(s.length / 2);
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

const CHROME_ARGS = [
  '--no-sandbox',
  '--disable-dev-shm-usage',
  '--use-gl=angle',
  '--use-angle=swiftshader',
  '--enable-unsafe-swiftshader',
  '--ignore-gpu-blocklist',
  '--enable-precise-memory-info',
  '--disable-extensions',
  '--window-size=1600,1000',
  '--no-first-run',
  '--disable-background-networking',
];

async function launch(userDataDir) {
  return puppeteer.launch({
    executablePath: CHROME,
    headless: 'new',
    userDataDir,
    args: CHROME_ARGS,
    defaultViewport: { width: 1600, height: 1000 },
  });
}

async function heap(page) {
  try {
    return await page.evaluate(() => (performance.memory ? performance.memory.usedJSHeapSize : null));
  } catch {
    return null;
  }
}

function startSampler(pid, page) {
  const samples = [];
  const timer = setInterval(() => {
    try {
      const rss = rssTree(pid);
      samples.push(rss.sumKb);
    } catch { /* process already gone */ }
  }, 100);
  const heapTimer = setInterval(async () => {
    const h = await heap(page);
    if (h != null) samples.heap = Math.max(samples.heap || 0, h);
  }, 150);
  return {
    stop() {
      clearInterval(timer);
      clearInterval(heapTimer);
      return {
        peakRssKb: samples.length ? Math.max(...samples) : null,
        peakJsHeap: samples.heap || null,
      };
    },
  };
}

// Puppeteer's keyboard.press is delivered, and the page hears the keydown,
// but egui/winit in the perfr build ignores it. A CDP rawKeyDown moves the
// view. The same event is used for both viewers so the gesture matches.
async function cdp(page) {
  return page.__cdp || (page.__cdp = await page.createCDPSession());
}

function keyFields(code) {
  const key = code.replace('Key', '').toLowerCase();
  const vk = key.toUpperCase().charCodeAt(0);
  return { key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
}

async function keyDown(page, code) {
  const client = await cdp(page);
  await client.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', ...keyFields(code) });
}

async function keyUp(page, code) {
  const client = await cdp(page);
  await client.send('Input.dispatchKeyEvent', { type: 'keyUp', ...keyFields(code) });
}

async function tapKey(page, code) {
  // Held across more than one frame. On a heavy trace a frame can take
  // well over 100ms, and a shorter hold is up before it is sampled.
  await keyDown(page, code);
  await sleep(220);
  await keyUp(page, code);
}

async function focusTimeline(page, viewer) {
  const x = viewer === 'perfr' ? 800 : 780;
  const y = viewer === 'perfr' ? 360 : 220;
  await page.bringToFront();
  await page.mouse.click(x, y);
  await page.evaluate(() => {
    for (const el of document.querySelectorAll('input, textarea')) el.blur();
    const canvas = document.querySelector('canvas');
    if (canvas) {
      canvas.tabIndex = 0;
      canvas.focus();
    }
  });
  await page.mouse.click(x, y);
  return { x, y };
}

async function gesture(page, viewer) {
  await page.evaluate(() => {
    window.__raf = [];
    const loop = (t) => { window.__raf.push(t); requestAnimationFrame(loop); };
    requestAnimationFrame(loop);
  });
  await sleep(100);
  const mark = await page.evaluate(() => window.__raf.length);
  const pos = await focusTimeline(page, viewer);
  const { x, y } = pos;
  const tap = async (key, n) => {
    for (let i = 0; i < n; i++) {
      await tapKey(page, key);
      await sleep(40);
    }
  };
  await tap('KeyW', 10);
  await tap('KeyS', 10);
  await tap('KeyA', 12);
  await tap('KeyD', 12);

  await page.mouse.move(x, y);
  await page.keyboard.down('Control');
  for (let i = 0; i < 8; i++) {
    await page.mouse.wheel({ deltaY: -120 });
    await sleep(40);
  }
  for (let i = 0; i < 8; i++) {
    await page.mouse.wheel({ deltaY: 120 });
    await sleep(40);
  }
  await page.keyboard.up('Control');

  // Pan drag. perfr pans on a left drag. Perfetto's left drag is an area
  // selection; its pan drag is Shift+left. Same distance, the gesture each
  // viewer documents as pan.
  if (viewer === 'perfetto') await page.keyboard.down('Shift');
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + 240, y, { steps: 12 });
  await page.mouse.up();
  if (viewer === 'perfetto') await page.keyboard.up('Shift');
  await sleep(150);

  const stamps = await page.evaluate((from) => window.__raf.slice(from), mark);
  return frameStats(stamps);
}

async function clipHash(page) {
  const buf = await page.screenshot({
    clip: { x: 280, y: 90, width: 640, height: 180 },
    type: 'png',
  });
  return createHash('sha1').update(buf).digest('hex');
}

async function viewChanged(page, viewer, before) {
  if (viewer === 'perfr') {
    const after = await page.evaluate(() => {
      try { return JSON.parse(window.__perfr_sel).view; } catch { return null; }
    });
    if (!after || !before) return false;
    return after[0] !== before[0] || after[1] !== before[1];
  }
  const after = await clipHash(page);
  return after !== before;
}

async function snapshotView(page, viewer) {
  if (viewer === 'perfr') {
    return page.evaluate(() => {
      try { return JSON.parse(window.__perfr_sel).view; } catch { return null; }
    });
  }
  // The visible window is drawn into a canvas, not exposed as text.
  // A clip hash is the signal that a zoom changed pixels. Taking it adds
  // one screenshot to the interaction measurement.
  return clipHash(page);
}

async function openPerfr(page, file) {
  // Ctrl+O does not open the chooser unless the canvas already has focus.
  // The Open pill's rectangle is published for this harness.
  const chooserP = page.waitForFileChooser({ timeout: 8000 });
  const open = await page.evaluate(() => {
    const rows = JSON.parse(window.__perfr_ui || '[]');
    return rows.find((r) => r[0] === 'Open') || null;
  });
  if (!open) throw new Error('Open button is not in window.__perfr_ui');
  await page.mouse.click(open[1] + open[3] / 2, open[2] + open[4] / 2);
  const chooser = await chooserP;
  const t0 = Date.now();
  await chooser.accept([file]);
  return t0;
}

async function waitPerfr(page, t0, timeoutMs) {
  let prev = -1;
  let stable = 0;
  while (Date.now() - t0 < timeoutMs) {
    const s = await page.evaluate(() => {
      let sel = null;
      try { sel = JSON.parse(window.__perfr_sel); } catch { /* not yet */ }
      let status = '';
      try {
        const rows = JSON.parse(window.__perfr_ui || '[]');
        const row = rows.find((r) => typeof r[0] === 'string' && r[0].startsWith('symbols-status:'));
        if (row) status = row[0].slice('symbols-status:'.length);
      } catch { /* ui not published this frame */ }
      if (!sel) return null;
      return {
        events: sel.events,
        content: sel.content,
        prims: sel.prims,
        view: sel.view,
        build: sel.build,
        renderer: sel.renderer,
        status,
      };
    });
    // `symbols-status:` is the loading line ("Loading name  18 MB / 58 MB")
    // until the parser finishes, then the file name. Event count can sit
    // still for a frame on a large file, so stability alone is not "done".
    // Counter tracks draw without adding to `prims`.
    const loading = s && typeof s.status === 'string' && s.status.startsWith('Loading');
    if (s && !loading && s.events > 0 && s.content) {
      if (s.events === prev) stable += 1;
      else stable = 0;
      prev = s.events;
      if (stable >= 2) {
        return {
          ok: true,
          renderMs: Date.now() - t0,
          events: s.events,
          prims: s.prims,
          content: s.content,
          view: s.view,
          build: s.build,
          renderer: s.renderer,
          status: s.status,
        };
      }
    } else if (s) {
      prev = s.events;
      stable = 0;
    }
    await sleep(POLL_MS);
  }
  const s = await page.evaluate(() => {
    try { return JSON.parse(window.__perfr_sel); } catch { return null; }
  }).catch(() => null);
  return { ok: false, renderMs: Date.now() - t0, events: s && s.events, prims: s && s.prims, build: s && s.build, renderer: s && s.renderer };
}

async function openPerfetto(page, file) {
  await page.waitForSelector('input.trace_file', { timeout: 60000 });
  await page.evaluate(() => {
    const ok = [...document.querySelectorAll('button')].find((b) => (b.textContent || '').trim() === 'OK');
    if (ok) ok.click();
  });
  const input = await page.$('input.trace_file');
  const t0 = Date.now();
  await input.uploadFile(file);
  return t0;
}

async function waitPerfetto(page, t0, timeoutMs) {
  // "Duration:" is painted into a canvas, so it is not in innerText.
  // The shell switches from QUICK START to CURRENT TRACE once the timeline
  // is up, and the overview canvases exist by then.
  let stable = 0;
  while (Date.now() - t0 < timeoutMs) {
    const snap = await page.evaluate(() => {
      const text = document.body.innerText;
      const canvases = [...document.querySelectorAll('canvas')].map((c) => c.width);
      const err = /Could not fetch|could not be parsed|Trace error|Failed to parse/i.test(text);
      const loading = /Loading trace|Indexing|Parsing/.test(text);
      const ready = text.includes('CURRENT TRACE') && !text.includes('QUICK START') && !loading
        && canvases.some((w) => w > 200);
      const version = (text.match(/v\d+\.\d+-[0-9a-f]+/) || [])[0] || null;
      return { ready, err, version, head: text.slice(0, 400) };
    });
    if (snap.err && !snap.ready) return { ok: false, renderMs: Date.now() - t0, error: snap.head };
    if (snap.ready) {
      stable += 1;
      if (stable >= 2) return { ok: true, renderMs: Date.now() - t0, version: snap.version };
    } else stable = 0;
    await sleep(POLL_MS);
  }
  const head = await page.evaluate(() => document.body.innerText.slice(0, 400)).catch(() => '');
  return { ok: false, renderMs: Date.now() - t0, error: 'timeout', head };
}

async function runOnce(viewer, trace, userDataDir, shotPath) {
  const browser = await launch(userDataDir);
  const pid = browser.process().pid;
  const page = await browser.newPage();
  page.setDefaultTimeout(60000);
  const record = {
    viewer,
    id: trace.id,
    file: trace.file,
    ok: false,
  };
  try {
    if (viewer === 'perfr') {
      await page.goto(PERFR_URL, { waitUntil: 'domcontentloaded', timeout: 120000 });
      await page.waitForFunction(() => window.__perfr_sel && window.__perfr_sel.length > 10, { timeout: 90000 });
    } else {
      await page.goto(PERFETTO_URL, { waitUntil: 'domcontentloaded', timeout: 120000 });
      await page.waitForFunction(() => document.body.innerText.includes('Open trace file'), { timeout: 90000 });
    }
    const baseline = rssTree(pid);
    record.baselineRssKb = baseline.sumKb;
    record.baselineJsHeap = await heap(page);
    const sampler = startSampler(pid, page);
    try {
      const file = path.join(TRACES_DIR, trace.file);
      const timeoutMs = trace.bytes > 8_000_000 ? 180000 : trace.bytes < 100000 ? 8000 : 60000;
      const opened = viewer === 'perfr'
        ? await waitPerfr(page, await openPerfr(page, file), timeoutMs)
        : await waitPerfetto(page, await openPerfetto(page, file), timeoutMs);
      Object.assign(record, opened);
      if (opened.ok) {
        if (shotPath && !fs.existsSync(shotPath)) {
          fs.mkdirSync(path.dirname(shotPath), { recursive: true });
          await page.screenshot({ path: shotPath });
          record.shot = path.relative(path.join(HERE, '../../..'), shotPath);
        }
        const before = await snapshotView(page, viewer);
        await focusTimeline(page, viewer);
        const keyAt = Date.now();
        await keyDown(page, 'KeyW');
        let changed = false;
        for (let i = 0; i < 30; i++) {
          await sleep(viewer === 'perfetto' ? 80 : 50);
          if (await viewChanged(page, viewer, before)) { changed = true; break; }
        }
        await keyUp(page, 'KeyW');
        record.viewChanged = changed;
        // Render time, plus the wait from the first zoom key until the
        // view actually moves. The setup screenshot is not included.
        record.interactMs = changed ? opened.renderMs + (Date.now() - keyAt) : null;
        record.frames = await gesture(page, viewer);
      } else if (shotPath) {
        const fail = shotPath.replace(/\.png$/, '-failed.png');
        fs.mkdirSync(path.dirname(fail), { recursive: true });
        await page.screenshot({ path: fail }).catch(() => {});
      }
      record.ok = !!opened.ok;
    } finally {
      const mem = sampler.stop();
      record.peakRssKb = mem.peakRssKb;
      record.peakJsHeap = mem.peakJsHeap;
    }
  } catch (err) {
    record.ok = false;
    record.error = String(err && err.stack || err).slice(0, 800);
  } finally {
    await browser.close().catch(() => {});
  }
  return record;
}

function summarise(runs) {
  const ok = runs.filter((r) => r.ok);
  const pick = (fn) => median(ok.map(fn));
  return {
    runs: runs.length,
    ok: ok.length,
    renderMs: pick((r) => r.renderMs),
    interactMs: pick((r) => r.interactMs),
    peakRssMb: pick((r) => (r.peakRssKb != null ? r.peakRssKb / 1024 : null)),
    baselineRssMb: pick((r) => (r.baselineRssKb != null ? r.baselineRssKb / 1024 : null)),
    peakJsHeapMb: pick((r) => (r.peakJsHeap != null ? r.peakJsHeap / 1048576 : null)),
    fps: pick((r) => r.frames && r.frames.fps),
    frameP50: pick((r) => r.frames && r.frames.p50),
    frameP95: pick((r) => r.frames && r.frames.p95),
    frameP99: pick((r) => r.frames && r.frames.p99),
    events: pick((r) => r.events),
    prims: pick((r) => r.prims),
  };
}

fs.mkdirSync(TRACES_DIR, { recursive: true });
for (const trace of traces) {
  const dest = path.join(TRACES_DIR, trace.file);
  if (!fs.existsSync(dest) || fs.statSync(dest).size !== trace.bytes) {
    process.stdout.write(`download ${trace.file}\n`);
    execFileSync('curl', ['-fL', '--retry', '3', '-o', dest, trace.url], { stdio: 'inherit' });
  }
  const size = fs.statSync(dest).size;
  if (size !== trace.bytes) {
    throw new Error(`${trace.file} is ${size} bytes, traces.json says ${trace.bytes}`);
  }
}

const chromeVersion = execFileSync(CHROME, ['--version'], { encoding: 'utf8' }).trim();
const userDataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'perfetto-compare-'));

const results = {
  started: new Date().toISOString(),
  chrome: chromeVersion,
  host: {
    uname: execFileSync('uname', ['-sr'], { encoding: 'utf8' }).trim(),
    cores: os.cpus().length,
    model: os.cpus()[0] && os.cpus()[0].model,
    memGb: Math.round(os.totalmem() / 1024 / 1024 / 1024),
  },
  flags: CHROME_ARGS,
  viewers: {
    perfr: PERFR_URL,
    perfetto: PERFETTO_URL,
  },
  method: {
    pollMs: POLL_MS,
    runs: RUNS,
    note: 'Clock starts when the file is handed to the page (file-chooser accept, or input.uploadFile) after the shell is already up. Page load and wasm download are not included. Assets stay in one Chrome profile so the second run is not a cold download.',
  },
  traces: [],
};

console.log(chromeVersion, results.host);

for (const trace of traces) {
  console.log(`\n== ${trace.id} ==`);
  const perViewer = { perfr: [], perfetto: [] };
  for (const viewer of ['perfr', 'perfetto']) {
    for (let i = 1; i <= RUNS; i++) {
      const shot = path.join(SHOTS, `${viewer}-${trace.id}.png`);
      const run = await runOnce(viewer, trace, userDataDir, i === 1 ? shot : null);
      perViewer[viewer].push(run);
      const fps = run.frames && run.frames.fps != null ? run.frames.fps.toFixed(1) : '-';
      console.log(`  ${viewer} ${i}/${RUNS} ok=${run.ok} render=${run.renderMs} interact=${run.interactMs} rss=${run.peakRssKb} fps=${fps} ${run.error ? run.error.split('\n')[0] : ''}`);
    }
  }
  results.traces.push({
    ...trace,
    summary: {
      perfr: summarise(perViewer.perfr),
      perfetto: summarise(perViewer.perfetto),
    },
    runs: perViewer,
  });
  fs.mkdirSync(path.dirname(OUT), { recursive: true });
  fs.writeFileSync(OUT, JSON.stringify(results, null, 2));
}

results.finished = new Date().toISOString();
fs.writeFileSync(OUT, JSON.stringify(results, null, 2));
console.log('\nwrote', OUT);
fs.rmSync(userDataDir, { recursive: true, force: true });
