---
name: perfr
description: Profile a running process with perfr. Use when the user asks to profile, find a hotspot, see why a function is slow, sample call stacks, hook a function, or inspect timing of a live program. Drives the perfr CLI and the perfr MCP server. Does not edit source.
---

# perfr

perfr profiles a process that is already running. The viewer is a web page. An agent uses the MCP server. This skill does not edit source, rebuild, kill the process, or delete files.

Hooking a function patches it in memory for the duration of a capture and restores the bytes when the capture stops.

## Start the service

The MCP process is a client. The service has to be running first, on loopback:

```
perfr
```

That listens on `http://127.0.0.1:44766/`. The HTTP API has no authentication.

```
perfr serve --host 127.0.0.1
```

`perfr --host 0.0.0.0 --serve` exposes attach and capture on the network. Leave the service on `127.0.0.1` unless the user asked for that.

If startup prints a reduced-capture warning, follow the perfr-setup skill. Do not ignore it and then treat an empty sample list as a profile.

## MCP

```
perfr mcp --url http://127.0.0.1:44766
```

Call `perfr_capabilities` first. It reports the platform and which features this machine can record.

Typical loop:

1. `perfr_list_processes`
2. `perfr_attach` with a pid
3. `perfr_search_functions`
4. `perfr_arm_hooks` (at most 16 functions; pass `module` when a name is ambiguous)
5. `perfr_start_capture`
6. `perfr_stop_capture`
7. `perfr_function_stats` for hooks and manual scopes, and for frame times when the program marks its frames (`query` `frame`; rows of kind `frame`)
8. `perfr_sampling_report` and `perfr_callstacks` when sampling is supported
9. `perfr_logs` for what the program logged; pass `scope` to read the lines inside one scope

A capture that is already running keeps the hooks it started with. `perfr_unhook` edits the set for the next start.

Sampling and the scheduler are recorded on Linux, and on Windows when the service was started from an elevated prompt. On macOS those tools return `supported: false`. Frida hooks and manual scopes still work there. Do not invent an empty profile when the tool says the feature is unsupported.

`perfr_capabilities` saying sampling is supported, then a report with `"samples": 0`, means the feature is allowed but recorded nothing. On Linux that is often `perf_event_paranoid`. Say so. The perfr-setup skill has the checks.

### A Python target

When `perfr_capabilities` lists `python_stacks` as supported (a Linux service), the report and the call stacks of a Python program name its Python functions: `crunch (app.py:10)` is the function `crunch` defined at line 10 of `app.py`, and its `module` is that file. Native frames are in the same stacks: what the interpreter or a C extension does under a Python function is listed under it. Nothing has to be asked for; the service does it when the target has a Python interpreter mapped.

- A Python function has `function_id` 0 and cannot be hooked. `perfr_search_functions` lists native functions only. To time one Python function exactly, the program has to mark it itself with the `perfr-api` package (`perfr.scope`).
- The Python frames of a sample are read up to half a millisecond from the sample. Shares over a report are sound. Do not read one stack as proof that a Python function called the native frame under it.
- A report of a Python program that names `_PyEval_EvalFrameDefault` and no Python function means the stacks were not read. The usual cause is that the service may not read the target's memory, which is the permission attaching needs. The perfr-setup skill has the checks.

## One-shot capture

```
perfr capture --pid <pid> --duration-ms 5000 --out capture.pod
```

On macOS and Windows the output path ends in `.perfr.zip`.

## A saved capture

```
perfr slice <in.perfr.zip> <out.perfr.zip> <t0_ns> <t1_ns>
```

## What this skill does not do

It does not edit the target's source or apply a patch. It does not share or annotate captures.
