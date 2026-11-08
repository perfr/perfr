// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/** Nanoseconds since this context's `performance.timeOrigin`. */
export function nowNs(): number {
  const perf = globalThis.performance;
  if (!perf) return 0;
  return perf.now() * 1e6;
}

/**
 * Nanoseconds on the page's clock, from inside a worker.
 * A worker's `timeOrigin` is later than the page's, so `performance.now()`
 * alone is not comparable. `pageOrigin` is `performance.timeOrigin` copied
 * from the page when the worker is attached.
 */
export function nowNsAligned(pageOrigin: number): number {
  const perf = globalThis.performance;
  if (!perf) return 0;
  const origin = typeof perf.timeOrigin === "number" ? perf.timeOrigin : pageOrigin;
  return (origin - pageOrigin + perf.now()) * 1e6;
}
