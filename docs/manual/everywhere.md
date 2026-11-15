# Everywhere

The same capture, in another colour scheme, on a phone, or loaded from a trace file or a Linux `perf` recording. The recordings are on the [features page](../features.html#everywhere).

## Colour schemes

More → **Color scheme**. The hover text is “Recolour the whole viewer — chrome, scopes and thread states”. The list, in order, is perfr, Dracula, Nord, Gruvbox, Solarized, Solarized Light and Gruvbox Light. A fresh page, and a fresh install of the native viewer, opens on **Nord**. The choice is stored in the browser as `perfr_theme` and restored on the next load. A saved choice wins over the default, including a saved `perfr`.

`?theme=` pins one when the page opens. The keys are `perfr`, `dracula`, `nord`, `gruvbox`, `solarized`, `solarized-light` and `gruvbox-light`.

@clip 12-themes | Colour schemes | Switching the viewer colour scheme through Dracula, Solarized Light and Nord

The page is WebGL2. Add `?webgpu` to try WebGPU where the driver supports it. More shows which backend is in use (“renderer …”).

## Phone

Under 840 CSS pixels the layout tightens on its own, unless you have already set **Compact tracks** yourself. Tracks draw at 0.72× their usual height, the settings window closes if you did not open it, and the top bar uses the narrow form: the record control reads **Rec** / **Stop**. More → **Compact tracks** (hover: “Track density”) is the same switch, and choosing it yourself keeps your choice when the window gets wide again.

@clip 13-mobile-compact | Phone layout | The live viewer at phone width, with the report and flame graph open

Zoom, pan and the report are the same gestures as on a wide window. A pinch zooms, which is the track-pad stand-in for Ctrl+wheel.

## Chrome and Perfetto traces

**Open**, a drop, or **Ctrl+O** / **⌘O** loads a trace with no service behind the page.

Accepted files:

- Chrome Trace Event JSON, either a top-level array or an object with `traceEvents`
- The same JSON gzipped (`.json.gz`), inflated as it arrives
- A single-file zip of that JSON
- Perfetto protobuf: `.pftrace`, `.perfetto-trace` or `.pb`

Processes, threads, nested slices, counters and flows land on the same timeline. Home, or a double-click on the ruler, fits the content once events are in.

Opening one of these traces during a live capture stops the capture. `begin_trace_load` calls stop before it clears the timeline. A saved `.perfr.zip` is the other path: the service will not open it while a capture is running, and answers `busy: stop the capture before opening one`.

The WebAssembly heap is capped at 4 GiB (`--max-memory=4294967296` in `wasm_pack.sh`), or 2 GiB in a browser that will not reserve 4. A trace that does not fit in that heap cannot load in the browser viewer.

@clip 14-chrome-trace-import | Chrome trace | Opening a Chrome Trace Event JSON file onto the perfr timeline

The drop overlay reads “Drop a Chrome (.json / .json.gz) or Perfetto (.pftrace) trace, or a perf.data”. A file that is none of those and not a capture shows “Not a Chrome or Perfetto trace, or a perf.data” plus the name.

## Linux perf recordings

**Open**, a drop, or **Ctrl+O** / **⌘O** also takes the file `perf record` writes: `perf.data`, `perf.data.old`, or one renamed to end in `.data`. What `perf record -o -` pipes into a file is read too, and so is a recording made with `-z`.

```
perf record -g -- ./my-program        # call chains from frame pointers
perf record --call-graph dwarf -- ./my-program
perf record -g -a -- sleep 5          # the whole machine
```

Each sample is a tick on its thread's sample bar, as for a capture perfr sampled itself: click one to copy its call stack, drag across the bar to select, and the flame graph and the sampling trees are computed from them. Threads and processes carry the names the recording has. A recording made with `--switch-events` also fills the thread-state bars (running, and runnable when a thread was pushed off its CPU), and the scheduler lanes when it covers the whole machine (`-a`).

### Where the names come from

A `perf.data` holds addresses, and for each one the file it was in. It holds no function names: `perf report` reads those from the binaries on the machine it runs on. So what you see depends on who reads the file.

- **The page perfr serves, on Linux.** The file goes to the service, which names the frames from the binaries at the recorded paths (and their detached debug files), kernel frames from `/proc/kallsyms`, and unwinds the stack copies of a `--call-graph dwarf` recording. Open the recording on the machine that made it and the names are the ones `perf report` shows. A binary is used only if it is the build that was recorded (the build ids are compared), and the kernel's symbols only if it is the same kernel: a rebuilt program, or a recording from another machine, gets no wrong names.
- **A page with no service** (the hosted viewer, a page opened on a file), or a service on macOS or Windows. The page reads the file itself and shows each frame as `module+0xoffset`, for example `libc.so.6+0x8f2a4`. The offset is the one in the file, which is what `addr2line -e` takes. Threads, the reports and the selection all work; only the names are missing. A `--call-graph dwarf` recording shows the sampled function alone, since unwinding a stack copy takes the binary.

A frame the service cannot name is shown the same way, `module+0xoffset`. The service log has one line per recording opened: how many frames it named, and how many files it read, did not find, or found in another build.

### What to know

- The service opens a recording as it opens a `.perfr.zip`: it replaces the current capture, and answers `busy: stop the capture before opening one` while one is running. Read by the page, a recording stops a live capture first, as a trace does.
- Samples need their time (`perf record` stores it by default; with `--no-inherit` on one process, add `-T`). A recording without it is refused with that advice.
- Every event of the recording lands on the same sample bar. `perf record -e cycles -e cache-misses` is two profiles in one file, and they are not told apart.
- Intel PT and other AUX data are skipped. Files written on a big-endian machine are not read.
- A service takes a recording of up to 4 GiB. The page is bounded by its WebAssembly heap, as for a trace.
