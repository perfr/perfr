# Capture

Pick a process, choose what to record, and press Record. The timeline starts streaming and the capture timer ticks. Attach and record is on the [home page](../index.html#capture). Auto-profiling is on the [features page](../features.html#auto-profiling).

@clip 01-attach-record | Attach and record | The live timeline just after Record, with scheduler lanes and the capture timer

## Record, stop, open, save

The red button starts and stops a capture. **X** does the same, unless the page is showing a file you opened (a static capture has no Record button). On a window narrower than 840 CSS pixels the word on the button is **Rec**, then **Stop**.

**Open** loads a saved perfr capture (`.perfr.zip`), a Chrome trace (`.json` / `.json.gz`), a Perfetto trace (`.pftrace` / `.pb`) or a Linux `perf.data`. You can also drop the file on the page, or press **Ctrl+O** (⌘O). Opening a Chrome or Perfetto trace stops a live capture first. Opening a `.perfr.zip` does not: the service answers `busy: stop the capture before opening one` until you stop. [Traces](everywhere.html#chrome-and-perfetto-traces) has the formats, and [Linux perf recordings](everywhere.html#linux-perf-recordings) what becomes of a `perf.data`.

**Save** downloads **Capture (.perfr.zip)**, or **Selected slice (.perfr.zip)** when a time range is selected. **Clear** empties the capture. Clear is refused while a capture is still running.

## What to collect

Settings, **COLLECT**:

| Switch | Hover | What it records |
|---|---|---|
| **CSW** | Context switches / Scheduler track | One scheduler lane per core |
| **States** | Thread state slices | Running, Sleeping, and the other states |
| **API** | Manual perfr.h API scopes | Scopes from `perfr.h`, Rust and Python |
| **Sample** | Callstack sampling | A stack sample per period |
| **Health** | Machine health | The machine's CPU, memory, pressure, disks, network and GPU, twice a second |

The field beside Sample is a period in milliseconds (`ms`). It starts at `1.0`. The viewer sends it as `samples_per_second`. The service on main does not read that field: a Linux or Windows capture samples at a fixed 1 kHz.

> The COLLECT switches are sent with the capture start. On this service a Linux capture always samples, always opens the scheduler and thread-state rings when the kernel allows it, and always reads manual scopes; a Windows capture does the same through the kernel logger when the service is elevated. Turning a switch off does not drop that stream yet.

## Machine health

A stall in the target is not always the target's doing: the machine may have been reclaiming memory, a disk may have been saturated, another process may have had every core. With **Health** on (the default), a Linux service samples the machine every 500 ms and draws the result as a **machine health** track right under the target, so the machine's state is under the same cursor as the stall. The switch is honoured: off, nothing is read. The body field is `machine_health`.

| Group | Graph | What it is |
|---|---|---|
| cpu | `CPU %` | Time not idle, every core together (`/proc/stat`) |
| | `CPU pressure %` | Share of the interval in which at least one task waited for a core |
| | `CPU steal %` | Time a hypervisor gave to another guest. Appears with the first stolen tick |
| | `CPU temperature C` | The processor's package sensor (`coretemp`, `k10temp`, `zenpower`, `cpu_thermal`) |
| memory | `memory used %` | Total minus `MemAvailable`: what could not be handed out without reclaim or swap |
| | `memory pressure %` | Share of the interval in which at least one task was stalled reclaiming or swapping |
| | `swap used MiB` | Only on a machine with swap |
| | `major faults /s` | Page faults that had to read from disk |
| | `OOM kills` | Kills by the kernel's OOM killer since the capture started. Appears with the first |
| io | `IO pressure %` | Share of the interval in which at least one task was stalled on IO |
| | `disk read MiB/s`, `disk write MiB/s` | Whole physical disks, summed (`/proc/diskstats`) |
| | `disk busy %` | Time the busiest disk had a request in flight |
| | `network receive MiB/s`, `network send MiB/s` | Physical interfaces, summed (`/proc/net/dev`) |
| gpu | `GPU %` | The busiest GPU |
| | `GPU memory %` | Memory in use, every GPU together |

Each group has a header on the rail. The triangle on it folds the group: its graphs go and the groups below move up. The label of each fold triangle in `window.__perfr_ui` is `fold:thread:<pid>:<tid>`.

The three pressure graphs are the kernel's PSI `some` totals (`/proc/pressure`), differenced over each interval rather than read as the kernel's ten-second average, so a stall of 100 ms shows as a 20 percent step and not as a ripple. They need a kernel built with `CONFIG_PSI`. The GPU graphs come from sysfs for AMD cards (`gpu_busy_percent`) and from the NVML helper (`perfr-gpu`, beside `perfr`) for NVIDIA; an Intel GPU has neither. A source the machine lacks leaves its graphs out.

Every graph of the track starts at zero. The percentages that are a share of a whole end at 100, the pressures at 10, the rates at 1; a larger value in view raises the top. A sample is about ten `pread` calls on files the service keeps open, some 70 µs on a 32-core machine. On macOS and Windows the switch does nothing yet.

**UNWIND** shows **DWARF** or **FP**. The viewer sends `unwinding` as `"dwarf"` or `"frame_pointers"`. The service unwinds with DWARF (framehop, `.eh_frame` / `.debug_frame`) and does not read that field, so FP is not a second unwinder on this build. On Windows the kernel walks the stack itself (`StackWalk` events) and the field is unused.

## Hooks

**HOOKS** is **Frida** or **Uprobes**. Frida is the default, and the only engine on macOS and Windows. Its note reads “requires permission to attach to the target”. Uprobes reads “requires Linux uprobe permissions”. That permission is `CAP_SYS_ADMIN`. Lowering `perf_event_paranoid`, or granting only `CAP_PERFMON`, does not arm a uprobe. See [Troubleshooting](troubleshooting.html).

**Dedupe** appears only while Uprobes is selected. Hover: drop the duplicate entry the kernel reports when a thread migrates inside a uprobe (same stack and instruction pointer, another CPU), and an entry above the last one’s stack. Off, those ghost scopes stay on the timeline. The status line counts them either way.

The **HOOKED** row:

- **Presets…** opens instrumentation presets.
- A count: “no functions hooked”, “1 function hooked”, or “N functions hooked”.
- **Functions** opens the Functions view: every function of the selected process, with a hooked column.
- **Unhook all** clears the list. It shows once something is hooked.

The service instruments at most 16 functions (`MAX_HOOKS`). A longer selection is truncated to the first 16, and the log says how many were selected.

After Record the same row says what happened. A clean uprobe arm reads “instrumenting N of M functions”. If nothing armed, the line is amber and names the fix. A function that auto-unhook switched off is marked with a warning on that line.

Right-click a function in the Flat report or a call tree and choose **Hook function for dynamic instrumentation** (or **Unhook function**). On a flame-graph bar the same items read **Hook function** and **Unhook function**. A function with no file offset says “Not hookable: no file offset for this function”. Record is what arms the list.

## Auto-unhook

**Auto-unhook** is on by default. Past the rate — 100 k calls/s until you change it — the service switches that function off mid-capture, on Frida and on uprobes. The checkbox sits on the HOOKS row, and while it is on a drag field reads “k calls/s”. The hover text: a function that hot is not something to time with a hook, the samples already show it, and hooked it costs the target about a core.

When it fires, the instrumentation line turns amber and an instant marks the moment on the thread. The checkbox and the rate are stored by the service in `~/.config/perfr/settings.json` (`auto_unhook`, `max_hook_calls_per_s`). A change is written at once and applies to the next Record. `0` on a capture request means never unhook for that capture.

## Auto-profiling

On Linux, **Auto** lets the service choose what to hook. It needs root or `CAP_SYS_ADMIN`: it uses the kernel hook engine, the one that can arm and disarm during a capture. It is not available on macOS or Windows.

@clip 03-auto-profile | Auto-profiling | The Functions view listing functions auto-profile hooked during a capture

Every two seconds it reads the sampling report of the last five and hooks the next functions it points at, highest inclusive share first, up to three at a time, skipping any the hook-safety check calls unsafe. It keeps a quarter of the budget free, so it stops adding once the set is at 75% of the budget. It unhooks a function that is past half the budget on its own, one that completed no call while it was armed (retried later, waiting longer each time, and left alone after three silent tries), one that was entered and never returned, and the hottest of the rest while the set is over the budget.

The budget starts at 1000 scopes/s (`auto_profile_scopes_per_s` in `~/.config/perfr/settings.json`). It hooks with the kernel engine, the one that can arm mid-capture, inside the 16-hook cap and under Auto-unhook. The status line reads “auto-profiling: N function(s) hooked, X of Y scopes/s”. Each change is an instant `auto-profile: hooked …` or `auto-profile: unhooked …` on the target’s main thread. Turning the switch off leaves the hooks that are already armed.

The capture-start body takes `"auto_profile": true`. `POST /api/auto_profile` with `{"on": true}` or `{"on": false}` toggles it, and `/api/status` reports `auto_profile` and `auto_profile_status`.

## Save a slice

Select a time range on the timeline, then use **Save → Selected slice (.perfr.zip)**.
The file keeps overlapping scopes whole, samples within the selected range,
their callstack frames, and thread/process names. The selected window is stored
in the archive alongside the retained events' full bounds.
