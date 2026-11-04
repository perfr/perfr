---
name: perfr-setup
description: Install perfr and fix sampling or attach permissions before the first capture. Use when perfr is missing, a capture warns that sampling or hooks are degraded, perf_event_paranoid or ptrace or ETW or task_for_pid is blocking a profile, or the user asks to set perfr up.
---

# perfr setup

Get perfr installed, then make sampling and dynamic instrumentation actually work, then take one capture. Do not edit the target's source.

## Install

If `perfr` is not on `PATH`:

```
curl -fsSL https://perfr.dev/install.sh | sh
```

On Windows, in PowerShell:

```
irm https://perfr.dev/install.ps1 | iex
```

If `perfr update --check` says a package manager owns the binary, use the command it prints. Do not replace that binary by hand.

`perfr skill install` writes this skill and the perfr skill. It also registers the MCP server where the agent keeps that config.

## See what is missing

```
perfr doctor
```

`perfr setup --check` prints the same table. Read every degraded row before changing the machine.

The viewer and manual scopes work without these privileges. An empty sampling report is still a failed profile when the user asked for call stacks. Say which row is degraded and the fix from the table.

## Apply a fix only with the user's agreement

Do not run `sudo`, `sysctl`, or `setcap` until the user agrees.

Prefer the interactive command so each change is confirmed:

```
perfr setup
```

Non-interactive, still only after a yes:

```
perfr setup --yes
```

That applies the runtime sysctls and, when uprobes were requested by the plan, the setcap. It does not write a file under `/etc` unless `--persist` is also set:

```
perfr setup --yes --persist
```

`--persist` writes `/etc/sysctl.d/99-perfr-perf.conf`.

What the rows mean:

- `kernel.perf_event_paranoid` above 2 blocks sampling. Above 0 blocks the scheduler. `sudo sysctl -w kernel.perf_event_paranoid=0` allows both. Root, `CAP_PERFMON`, or `CAP_SYS_ADMIN` bypass the sysctl.
- `kernel.kptr_restrict` above 0 hides kernel addresses from an unprivileged process. Userspace samples still have their own symbols.
- tracefs (`/sys/kernel/tracing` or `/sys/kernel/debug/tracing`) has to be readable for scheduler tracepoints. `CAP_PERFMON` or root is the usual grant.
- Uprobes need `CAP_SYS_ADMIN`. Frida is the default hook engine and does not. A kernel built without `CONFIG_UPROBE_EVENTS` has no uprobe PMU; switch hooks to Frida. There is no sysctl that adds the PMU.
- `kernel.yama.ptrace_scope` above 0 blocks Frida from attaching to a process that is not a child. `sudo sysctl -w kernel.yama.ptrace_scope=0`, or `CAP_SYS_PTRACE`.
- On Windows, sampling and the scheduler come from the kernel logger and need a token from **Run as administrator**. Manual scopes and Frida hooks do not. A normal prompt of an administrator account is not elevated.
- On macOS this build records manual scopes and Frida hooks. Attach needs a task port. `sudo perfr` is the practical grant. SIP and the hardened runtime can still refuse system processes. This build does not sample through dtrace.

`CAP_SYS_ADMIN` is root in all but name. Say that before a setcap that includes it.

## First capture

Start the service on loopback:

```
perfr
```

Open `http://127.0.0.1:44766/` or drive it with MCP. Call `perfr_capabilities` first. Then list processes, attach, search functions, arm at most 16 hooks, start, stop, and read `perfr_function_stats`. Read `perfr_sampling_report` only when capabilities says sampling is supported.

A one-shot file, without the viewer:

```
perfr capture --pid <pid> --duration-ms 5000 --out capture.pod
```

On macOS and Windows the path ends in `.perfr.zip`.

## Leave these alone

Do not offer to publish a capture or to draw on it. Do not describe recording several machines at once. Do not disable the preflight warning with `--no-preflight` or `PERFR_NO_PREFLIGHT` unless the user asked to hide it.
