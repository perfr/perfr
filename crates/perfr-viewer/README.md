# perfr viewer

The page that shows a capture: the timeline, the track rail, the reports,
search, the log window, stories, and the import of Chrome and Perfetto
traces. It has no capture service behind it. It is given a capture and
shows it.

```
./crates/perfr-viewer/build_wasm.sh     # writes crates/perfr-viewer/viewer-dist/
```

Host that folder anywhere and open it on a capture:

| Address | What opens |
| --- | --- |
| `index.html?capture=<url>` | a `.perfr.stream` |
| `...&story=<url>` | a story played over it |
| `...&chrome=compact` | the same without the toolbar, for an iframe |
| `index.html` | nothing: Open, or drop a `.perfr.stream`, a Chrome `.json` or a Perfetto trace on the page |

There is no Record, no process picker, no hooks and no Share: those need
the capture service, and belong to `perfr-client`, the layer on top of this
crate and the page the service itself serves. They are not switched off in
this pack; they are not in it. The code for them is the `service` feature,
which only `perfr-client` turns on, and the pack is built for size besides:
about 5.4 MB of wasm (2 MB gzipped) against the client's 9.9 MB. A `.perfr.zip` is opened by
the service too; the viewer says so when it is given one.

## How the two fit

This crate holds the whole interface. What is behind the page is a
`net::Backend`, and what the page may offer is a `Host`:

- The viewer's own entry point (`start_viewer`, the `page` feature) runs the
  interface over `Host::viewer()`: a backend that reads capture files and
  stories, no service, no sharing.
- `perfr-client` runs the same interface over a host that has a service,
  with the `service` feature on. Its backend (`perfr-client/src/service.rs`)
  is the event socket and the requests under `/api`, and the interface then
  shows recording, the process picker, hooks, the service's settings, the
  code views and Share. The source of those controls is in this crate,
  behind that feature; a build without it has none of them.

`perfr-client` depends on this crate; this crate depends on neither it nor
the capture service (`.github/scripts/check-viewer-boundary.py`). The
interface itself, its controls and how it is built and measured are
described in [the client's README](../perfr-client/README.md).
