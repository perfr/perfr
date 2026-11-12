// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Headless smoke test: the demo records events and the embedded viewer boots
// in embed mode. A second page loads ?capture= and must not enter embed mode.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import assert from "node:assert/strict";
import puppeteer from "puppeteer-core";

const repo = fileURLToPath(new URL("../../..", import.meta.url));
const chrome =
  process.env.CHROME ||
  (process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    : "/usr/bin/google-chrome");

const types = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".css": "text/css",
  ".png": "image/png",
  ".txt": "text/plain",
  ".json": "application/json",
};

function startServer() {
  const server = createServer(async (req, res) => {
    try {
      const url = new URL(req.url || "/", "http://127.0.0.1");
      let path = decodeURIComponent(url.pathname);
      if (path.endsWith("/")) path += "index.html";
      const file = normalize(join(repo, path));
      if (!file.startsWith(repo)) {
        res.writeHead(403);
        res.end();
        return;
      }
      const body = await readFile(file);
      res.writeHead(200, {
        "content-type": types[extname(file)] || "application/octet-stream",
        "cross-origin-opener-policy": "same-origin",
        "cross-origin-embedder-policy": "require-corp",
        "cross-origin-resource-policy": "same-origin",
      });
      res.end(body);
    } catch {
      res.writeHead(404);
      res.end("not found");
    }
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

test("demo records and the embedded viewer starts", async () => {
  const server = await startServer();
  const { port } = server.address();
  const browser = await puppeteer.launch({
    executablePath: chrome,
    headless: true,
    args: [
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--use-angle=swiftshader",
      "--enable-webgl",
      "--ignore-gpu-blocklist",
    ],
  });
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(60_000);
    const errors = [];
    page.on("pageerror", (err) => errors.push(String(err)));
    await page.setViewport({ width: 1280, height: 800 });
    await page.goto(`http://127.0.0.1:${port}/web/demo/index.html`, { waitUntil: "networkidle0" });
    await page.waitForFunction(() => {
      const stats = window.__perfr && window.__perfr.stats && window.__perfr.stats();
      return stats && stats.produced > 15;
    });
    const produced = await page.evaluate(() => window.__perfr.stats().produced);
    assert.ok(produced > 15, `expected recorded events, got ${produced}`);
    const support = await page.evaluate(() => window.__perfr.tracks.support());
    assert.equal(support.frames.available, true);
    assert.equal(typeof support.longTasks.note, "string");
    const embed = await page.evaluate(() => {
      const frame = document.querySelector("iframe.perfr-embed-frame");
      return {
        isolated: crossOriginIsolated,
        shared: window.__perfrViewer ? window.__perfrViewer.usingSharedBuffer : false,
        hasFrame: Boolean(frame),
        active: frame && frame.contentWindow ? frame.contentWindow.__perfrEmbedActive === true : false,
      };
    });
    assert.equal(embed.hasFrame, true);
    assert.equal(embed.active, true, "viewer iframe should be in embed mode");
    assert.equal(embed.isolated, true);
    assert.equal(embed.shared, true, "cross-origin isolated demo should use the shared buffer");
    await page.screenshot({ path: "/tmp/perfr-web-demo.png" });
    const fatal = errors.filter((e) => !/WebGL|wgpu|gpu/i.test(e));
    assert.deepEqual(fatal, [], fatal.join("\n"));

    const capture = await browser.newPage();
    const captureErrors = [];
    capture.on("pageerror", (err) => captureErrors.push(String(err)));
    await capture.goto(
      `http://127.0.0.1:${port}/crates/perfr-live-viewer/viewer-dist/index.html?capture=${encodeURIComponent("/web/demo/sample.perfr.stream")}`,
      { waitUntil: "domcontentloaded" },
    );
    await capture.waitForFunction(() => document.querySelector("canvas"));
    const embedFlag = await capture.evaluate(() => window.__perfrEmbedActive === true);
    assert.equal(embedFlag, false, "?capture= must not install embed mode");
    assert.ok(
      captureErrors.every((e) => !/perfr-embed/.test(e)),
      captureErrors.join("\n"),
    );
  } finally {
    await browser.close();
    await new Promise((resolve) => server.close(resolve));
  }
});
