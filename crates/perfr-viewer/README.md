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
crate and the page the service itself serves. A `.perfr.zip` is opened by
the service too; the viewer says so when it is given one.

## How the two fit

This crate holds the whole interface. What is behind the page is a
`net::Backend`, and what the page may offer is a `Host`:

- The viewer's own entry point (`start_viewer`, the `page` feature) runs the
  interface over `Host::viewer()`: a backend that reads capture files and
  stories, no service, no sharing.
- `perfr-client` runs the same interface over a host that has a service.
  Its backend (`perfr-client/src/service.rs`) is the event socket and the
  requests under `/api`, and the interface then shows recording, the process
  picker, hooks, the service's settings and Share.

`perfr-client` depends on this crate; this crate depends on neither it nor
the capture service (`.github/scripts/check-viewer-boundary.py`). The
interface itself, its controls and how it is built and measured are
described in [the client's README](../perfr-client/README.md).
