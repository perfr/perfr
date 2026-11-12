# @perfr/web

In-page profiling for a JS, TypeScript or WASM app. Scopes, async spans,
values and the timings the browser already reports are written into a
`.perfr.stream` by the `perfr-web` crate (compiled to WASM) and can be shown
in the perfr live viewer mounted on the page.

This does not sample CPU stacks or context switches. The browser does not
let a page do that. The manual page is
[docs/manual/web.md](../../docs/manual/web.md).

```ts
import { perfr } from "@perfr/web";

perfr.scope("frame", () => {
  perfr.scope("update", () => step());
  perfr.value("entities", count);
});

perfr.attachViewer({
  viewerUrl: "/viewer/index.html", // crates/perfr-live-viewer/viewer-dist
  overlay: true,
  hotkey: "F8",
});
```

`perfr.tracks.support()` is the honest list of which built-in tracks this
browser can fill. `perfr.exportStream()` is the ring as a `.perfr.stream`.
Share in the embedded viewer posts that snapshot.
