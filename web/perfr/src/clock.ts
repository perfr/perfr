// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/** Nanoseconds since this context's `performance.timeOrigin`. */
export function nowNs(): number {
  const perf = globalThis.performance;
  if (!perf) return 0;
  return perf.now() * 1e6;
}
