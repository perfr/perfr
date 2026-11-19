# Capture

Pick a process, choose what to record, and press Record. The timeline starts streaming and the capture timer ticks. Attach and record is on the [home page](../index.html#capture). Auto-profiling is on the [features page](../features.html#auto-profiling).

@clip 01-attach-record | Attach and record | The live timeline just after Record, with scheduler lanes and the capture timer

## Record, stop, open, save

The red button starts and stops a capture. **X** does the same, unless the page is showing a file you opened (a static capture has no Record button). On a window narrower than 840 CSS pixels the word on the button is **Rec**, then **Stop**.

**Open** loads a saved perfr capture (`.perfr.zip`), a Chrome trace (`.json` / `.json.gz`) or a Perfetto trace (`.pftrace` / `.pb`). You can also drop the file on the page, or press **Ctrl+O** (⌘O). Opening a Chrome or Perfetto trace stops a live capture first. Opening a `.perfr.zip` does not: the service answers `busy: stop the capture before opening one` until you stop. [Traces](everywhere.html#chrome-and-perfetto-traces) has the formats.

**Save** downloads **Capture (.perfr.zip)**, or **Selected slice (.perfr.zip)** when a time range is selected. **Clear** empties the capture. Clear is refused while a capture is still running.

## What to collect

Settings, **COLLECT**:

| Switch | Hover | What it records |
|---|---|---|
| **CSW** | Context switches / Scheduler track | One scheduler lane per core |
| **States** | Thread state slices | Running, Sleeping, and the other states |
| **API** | Manual perfr.h API scopes | Scopes from `perfr.h`, Rust and Python |
| **Sample** | Callstack sampling | A stack sample per period |
| **Health** | Node Health | The machine's CPU, memory, pressure, disks, network and GPU, twice a second |
| **Python** | When the target is a Python program | Its Python functions in the stack samples, Python's names for its threads, and who holds the GIL |

The field beside Sample is a period in milliseconds (`ms`). It starts at `1.0`. The viewer sends it as `samples_per_second`. The service on main does not read that field: a Linux or Windows capture samples at a fixed 1 kHz.

> The COLLECT switches are sent with the capture start. On this service a Linux capture always samples, always opens the scheduler and thread-state rings when the kernel allows it, and always reads manual scopes; a Windows capture does the same through the kernel logger when the service is elevated. Turning a switch off does not drop that stream yet.

## Node Health

A stall in the target is not always the target's doing: the machine may have been reclaiming memory, a disk may have been saturated, another process may have had every core. With **Health** on (the default), a Linux service samples the machine every 500 ms and draws the result as a **Node Health** track right under the target, so the machine's state is under the same cursor as the stall. A capture opened from a file names no target: there the track is under the process that leads the rail. The switch is honoured: off, nothing is read. The body field is `machine_health`.

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

## Python programs

A stack sample of a Python program, unwound like any other, is the interpreter: `_PyEval_EvalFrameDefault` in every stack and not one function of the program. With **Python** on (the default) a Linux or Windows service that is asked to capture a process with a Python interpreter mapped also reads the program's Python stacks, and the capture has three more things in it. On Linux that mapping is `python3.x` or `libpython3.x.so`. On Windows it is a loaded module: `python.exe`, `pythonw.exe`, `python3x.dll`, `python3.dll` or `libpython3.x.dll`. Nothing is installed in the target, imported by it or passed to it, and it is not restarted.

**Python functions in the samples.** Each is named `function (file.py:line)`: the qualified name, the source file relative to the `sys.path` entry it was imported from, and the line of its `def`. Its module is that file. It is a frame like any other: a row of the Flat report, a node of the call trees and the flame graph, a line of a sample's callstack, and a name in the saved capture. It has no hook box, because there is no native function to hook.

The native frames are still there, where they say something:

| Native frames | In a merged stack |
|---|---|
| Under the innermost Python function: the interpreter allocating, collecting garbage, sorting a list; a C extension and what it calls | Kept |
| Of anything that is not the interpreter (a C extension calling back into Python, libc, the program Python is embedded in), anywhere | Kept |
| The interpreter's own, above a Python function (`_PyEval_Vector`, `method_vectorcall`, `Py_RunMain`): how Python calls Python | Left out |

So a thread's stack reads `start_thread`, `Thread.run (threading.py:1013)`, `worker_loop (app.py:39)`, `crunch (app.py:10)`, `_Py_Dealloc`.

**Thread names.** A thread the program named with `threading.Thread(name=...)`, or that a pool named, has that name on the rail. Before Python 3.14 the kernel is never told those names: on Linux every thread reads `python3`, and on Windows a thread with no description would read `thread <id>`. From 3.14 Linux is told the first fifteen bytes and Windows is told the name in full (`SetThreadDescription`). Python's name is used when the kernel's adds nothing to it, and when the kernel has no name at all. A thread named for the kernel as something else keeps that name.

**The GIL.** Once the lock has changed hands, the process has a row called **GIL**, captioned `held by`. Each scope on it is a stretch one thread was seen holding the lock on every reading; it is named after that thread and drawn in that thread's colour, the colour of the chip beside the thread's name on the rail. The row shows the hand-overs; the Live tab's statistics for those names are how often and for how long each thread held it. A program whose GIL is only ever held by one thread gets no row: that thread's state bar already says when it ran.

### How it is read, and what that costs

The service reads the interpreter's own frame objects out of the target's memory with [py-spy](https://github.com/benfred/py-spy) 0.4.3, as a library, about a thousand times a second. The target is never paused and never suspended, and never traced: a paused target would show in the capture as its own scheduling, and only one tracer may hold a process, which would rule out hooks. On Windows py-spy opens the process with `PROCESS_SUSPEND_RESUME` among the other rights, and this reader never calls the suspend. Each reading has every Python thread's stack, which thread holds the GIL, and the thread names. The cost below was measured on Linux; it has not been measured on Windows.

Measured on the machine this was written on (a 32-thread x86-64 desktop, Python 3.14.4):

- One reading of a program of five threads takes about 0.09 ms of the service's time. The reader slows down by itself when a reading costs more than a fifth of the time between two, so it never takes more than a fifth of a core.
- A CPU-bound Python loop did 1.6% less work in six seconds under a capture with **Python** off, and 3.4% less with it on, than with no capture at all (medians of three runs each).

### What a merged stack can claim

The kernel takes the native sample at one instant. On Windows that sample is a `StackWalk`, already unwound, and it is held about 100 ms for the user half of a split walk to arrive, which is longer than the wait for the Python reading after it. The Python stack is read by another thread at another instant, up to half a millisecond away on Linux and within that settle on Windows. The two are joined like this:

- A sample takes the reading of its thread nearest to it in time, from the one before and the one after. A reading more than three reading periods away (5 ms at least) is not used.
- The native stack says how many times the interpreter was entered from C, and the Python stack says the same of itself. Where they agree, each run of Python frames takes the place of its eval frame. Where they differ, a callback from C into Python began or ended between the sample and the reading: the stacks are paired from the root, an eval frame with no Python function stays as `_PyEval_EvalFrameDefault`, and a Python function with no eval frame is left out. If the other reading is closer in shape, it is used instead.
- A sample whose native stack is in a call from C into Python that neither reading beside it caught looks a little further: a reading within the same reach that is the nearest one with more Python called under it, and fits the native stack exactly, supplies that call. A generator drained by `sum`, or a key function called by `sorted`, is entered and left thousands of times a second, so some readings have it and some do not; under the same callers, within milliseconds, it is the same call.
- A reading that fails, because the target changed what was being read, is dropped. So is one thread's stack when it stops short of that thread's outermost frame: read in the instant a frame was being linked to its caller, it is one or two frames that are not the thread's stack. A thread's outermost Python frame does not change while it lives, which is how these are told. Nothing is made up for either.

What cannot be checked is the inside of a run. Python calling Python does not move the native stack, so in code that calls many small functions the innermost Python frames of one sample are those of a moment up to half a millisecond earlier or later. Over a report this evens out: each function gets its share. For one sample it is the limit of reading from outside, and it is why a native leaf can now and then sit under a Python function that did not call it.

When the capture stops the service log says how it went:

```
Python stacks: 3913 readings of 7 thread(s) at 983 per second; 1 failed and 4 stack(s) were read torn, and were dropped
3929 sample(s) carry Python frames: 249 between two readings of the same stack, 6 with their innermost call into Python taken from a reading a little further away, 163 with a Python stack a call deeper or shallower than the native one. 2 kept the interpreter's frames: 2 had no reading close enough, 0 no Python frame to place
```

Code that hands generators and callbacks to C all the time is the hard case. A loop of `sum(i * i for i in range(n))` and `sorted(..., key=...)` had 16% of its stacks read torn, and 6% of its samples kept one `_PyEval_EvalFrameDefault` for a call into Python that no reading nearby explained; the rest named the generator or the key function.

### Limits

- Linux and Windows services. The macOS service does not read Python stacks.
- On Windows the kernel's walk is treated as the whole user stack. A walk the kernel cut at its frame cap is not marked; an unmatched outer Python run is left out, which is the rule for a whole stack. Nothing is appended to fill a gap.
- CPython, the versions py-spy 0.4.3 knows: 2.3 to 2.7 and 3.3 to 3.15. PyPy and other interpreters are not read. The capture described above was taken on Linux with 3.14.4. The Windows reader was run in CI (`windows-rust`, `windows-2022`) by `crates/perfr-pystack/tests/reader.rs` against `tools/test-programs/PerfrPythonStacks/python_stacks.py` on CPython 3.12.10: the program's functions, Python's thread names, the GIL spans, and that the process's CPU time moved while it was read. A capture on Windows with those readings joined to the kernel's stack samples, and the GIL row on the timeline, was not run. A free-threaded build has not been tried on either platform.
- The service must be allowed to read the target's memory. On Linux that is the permission attaching needs (`perfr doctor`). On Windows the open asks for `PROCESS_VM_READ`, `PROCESS_QUERY_INFORMATION` and `PROCESS_SUSPEND_RESUME`. The process's owner can open it. Another user's process, an elevated target, or a protected one needs an elevated prompt (Run as administrator). A capture that records samples is already elevated, because the kernel logger needs it. When the open is refused the log says so and the samples keep the interpreter's own frames.
- The interpreter's `_PyEval_EvalFrameDefault` must be found by name in the native stack for the Python frames to be placed. On Linux it is in the dynamic symbol table of Ubuntu's build, the one the capture above was taken with. On Windows the symbolizer takes the name from a PDB beside the DLL, else from that DLL's export table. The frames live in the versioned `python3x.dll`, not in `python3.dll` (the stable-ABI stub, which does not export this function). A build that hides the name and ships no PDB leaves the interpreter's frames, and the log's last line reports no merged samples. That Windows export was not checked against a DLL in this change.
- Functions, not lines: a row is a function, named with the line it is defined at.
- Only the target process. The workers of `multiprocessing` are other processes and are not read.
- A program that links the interpreter statically under its own name is not recognised. `"python_stacks": true` in the capture request tries anyway.

The body field is `python_stacks`: absent means when the target is a Python program, `false` is never, `true` is to try regardless. The viewer sends `false` when the switch is off and nothing when it is on.

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

## Share from the terminal

`perfr share` puts a capture on perfr.dev and prints its link, as the **Share** button does from the viewer. Anyone who has the link opens the capture in a browser.

```
perfr share capture.perfr.zip                      # a saved capture
perfr share capture.perfr.zip --name runs/spike    # under another name, in a folder
perfr share                                        # the capture the service on this machine holds
```

The file is a `.perfr.zip` or a `.perfr.stream`. The link is `https://perfr.dev/<your handle>/<name>`: the name is the file's unless `--name` gives one, and a name you already use is refused unless `--replace` is passed. `--unlisted` leaves the capture out of your public folder. `--json` prints the website's whole answer (`id`, `url`, `path`, `handle`, `listed`) for a script. `--service <url>` shares the capture of a service on another port.

The website knows you by an API token. Make one on [perfr.dev/account](https://perfr.dev/account), then run `perfr login` once and paste it: perfr asks the website whose it is and keeps it in a file only you can read (`credentials.json` in `~/.config/perfr`, or in `%APPDATA%\perfr`). `PERFR_TOKEN` is used first when it is set. `perfr login --status` says who is signed in, and `perfr logout` forgets the token; revoke it on the account page to end it. The upload is done by `curl`.

A `.perfr.zip` is read by a second service that perfr starts on a spare port of this machine and stops when the file is read. The one you are recording with is left alone.
