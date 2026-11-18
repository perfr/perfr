# Systems

Where the thread ran, and what the profiler itself is doing. The scheduler is on the [home page](../index.html#systems). The service’s own lanes are on the [features page](../features.html#service).

## Scheduler and thread state

**CSW** draws the scheduler: one lane per core the capture saw, under a track you can fold. `?collapse=scheduler` starts it folded. Hover a slice for which thread was on that core.

Each lane shows the core's top frequency when the capture recorded one, including when every core of the machine has the same one. Where a base frequency was recorded and it is not the same number, the lane shows both (`3.2–4.6`). A short bar after that is the current frequency read once at the start of the capture (`scaling_cur_freq` on Linux). The capture does not record a frequency trace. A chip is added only where the cores are not alike: `P` or `E` on a machine with performance and efficiency cores (Intel's hybrid parts, big.LITTLE), or the size of the level 3 cache where some cores have a larger one (the X3D Ryzens, `96M` beside `32M`). The stronger kind is in the accent colour, and where the top frequencies differ the highest is in the text colour and the others are muted.

Where the capture recorded SMT pairing, the two logical threads of one physical core share a faint rail and are labelled `P0 · T0` and `P0 · T1`. A core with no sibling on that machine is `P2`. Where it recorded more than one NUMA node, the lanes are tinted by node, with a line and a small `N0` where the node changes. The tint is the capture's node map.

Hover a lane for the rest of what that capture recorded: the kind, the physical core and which thread of it, the socket, the NUMA node, the die, each cache with its size, associativity, line and which CPUs share it, the min, base and max frequency, the microcode, the ISA flags the capture kept, the cache line, nodes per socket, and the power mode. The level 3 size is in that text. It is on the lane only when it is the chip that tells the cores apart.

The scheduler row, over the timeline, names the power mode and nodes per socket when they were recorded (`powersave · boost on · balanced · NPS2`). Click the row to open a diagram of the sockets, nodes, dies or shared level 3 caches, and the threads of each physical core. Hover the row for the same diagram while the pointer is there. A line under the diagram that came from a model table, rather than from the machine, says so, and so do memory-channel marks when the table supplied a count. Right-click the row for Copy CPU info, Search CPU model, and Product page when the model string and the core and thread counts matched a table entry that has a page. A model that did not match has no product link.

On Linux the service reads this from sysfs and procfs at the start of the capture: topology, cache/index*, cpufreq, the hybrid PMUs or cpu_capacity, NUMA nodes and distances, platform_profile, and `powerprofilesctl get` when that program is installed and prints `power-saver`, `balanced` or `performance`. Nodes per socket is the node count divided by the socket count when that divides evenly, and the summary says it was derived. The hostname is included, as the capture's system record already includes it.

On Windows the topology is what `GetLogicalProcessorInformationEx` reports: kind, caches, NUMA, packages, dies and SMT. There is no per-core frequency in that buffer. The power plan is the active scheme when powrprof answers, the overlay when `PowerGetActualOverlayScheme` is exported, and the processor boost mode is the AC index of that setting. Per-node memory is not read. That path has not been run on a Windows machine. On macOS the brand, counts, cache sizes and perf levels are sysctl, and low power, high power and AC or battery are `pmset` when it runs. macOS has no scheduler track. That path has not been run on a Mac.

The cores and the topology travel with the capture. A capture recorded by perfr 0.1.6 has the lane marks and no topology frame, so it has no NUMA tint, no SMT labels, no diagram and no power line; where its cores differ it still shows the chip and the top frequency, and where they do not it now shows the top frequency if one was recorded. A capture from perfr 0.1.5 or older, a Chrome or Perfetto trace, and a machine that tells nothing have no marks. The flags kept are a short list (AVX2, AVX-512 and the like), not every flag the kernel printed.

**States** draws a strip of thread states on each thread. The names the tooltip uses are Running (on a CPU), Runnable (ready, waiting for a CPU), Sleeping (interruptible), Uninterruptible sleep, Stopped, Traced, Dead, Zombie, Parked and Idle.

With no thread focused, the scheduler shows the slices of the capture's target in colour and the rest of the machine's in grey. A capture with no target has them all in colour. A saved or shared capture names its target, so it opens with the same slices in colour as when it was captured; a `.perfr.stream` saved by perfr 0.1.5 or older names none, and opens with them all in colour.

Click a thread header to focus it. Click again to release. The scheduler keeps that thread’s slices in colour and greys the others, and a chip above the timeline names it. Clicking one of that thread’s scopes does the same. A click on a scheduler slice, a state bar, or a sample tick does not move the focus. Escape, a click on empty canvas, or the chip’s × clears it.

@clip 04-scheduler | Scheduler and thread states | Scheduler track with one lane per core, thread focus, and Running and Sleeping states

System-wide scheduling needs `perf_event_paranoid` at 0 or below, or root / `CAP_PERFMON` / `CAP_SYS_ADMIN`. If the lanes stay empty, [Troubleshooting](troubleshooting.html) has the check.

## The service, profiling itself

`perfr` instruments its own capture loop. The value lanes it emits include `service cpu %`, `service rss MiB` and `events per pass`, plus ring fill (`perf switch rings fill %`, `scope rings fill %`, `viewer ring fill %`) and `scope records lost`. A vertical cursor reads them across the capture. The service’s own scopes (`capture pass`, `read samples`, `unwind`, and the rest) sit on its tracks with everything else.

@clip 09-service-health | Service self-profile | perfr profiling itself: CPU, RSS, events per pass and ring lanes

## The viewer’s Self pane

More → “Self   F2”, or the **F2** key, opens a pane with the viewer’s own frame times. **F3** toggles Benchmark, which asks the service for fake scopes at a rate you set. Both are about the tools, not the target. R, I, F2 and F3 do nothing while that Self pane is the one taking keys.
