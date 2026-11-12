// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/**
 * Byte ring in a SharedArrayBuffer. One writer, one reader. The reader is
 * the viewer page (`viewer-dist/embed.js`): it takes each write off the ring
 * when the message announcing that write arrives.
 */

const HEADER = 16;

export function sabAvailable(): boolean {
  return (
    typeof SharedArrayBuffer === "function" &&
    (globalThis as { crossOriginIsolated?: boolean }).crossOriginIsolated === true
  );
}

export function createByteRing(capacity: number): SharedArrayBuffer {
  const sab = new SharedArrayBuffer(HEADER + capacity);
  new Uint32Array(sab, 0, 4)[2] = capacity;
  return sab;
}

/** Returns false when the ring cannot fit `bytes` (caller should fall back). */
export function writeBytes(sab: SharedArrayBuffer, bytes: Uint8Array): boolean {
  const header = new Uint32Array(sab, 0, 4);
  const cap = Atomics.load(header, 2);
  if (cap <= 1 || bytes.length >= cap) return false;
  const data = new Uint8Array(sab, HEADER, cap);
  const read = Atomics.load(header, 1);
  const write = Atomics.load(header, 0);
  const used = (write - read + cap) % cap;
  const free = cap - 1 - used;
  if (bytes.length > free) return false;
  for (let i = 0; i < bytes.length; i++) data[(write + i) % cap] = bytes[i];
  Atomics.store(header, 0, (write + bytes.length) % cap);
  return true;
}
