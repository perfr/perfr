// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import { installWorker } from "../perfr/dist/worker.js";

const perfr = installWorker({ name: "sim" });

self.onmessage = (ev) => {
  const data = ev.data;
  if (!data || data.source === "perfr-worker") return;
  let n = 0;
  perfr.scope("step", () => {
    perfr.scope("integrate", () => {
      for (let i = 0; i < 2000; i++) n += Math.sin(i + data.t);
    });
    perfr.value("energy", n);
  });
  self.postMessage({ n });
};
