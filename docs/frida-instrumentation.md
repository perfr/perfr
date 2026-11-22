# Frida instrumentation

Frida is the default dynamic instrumentation engine. The viewer selects it by
default; HTTP requests with no method, `frida`, or the older `user_space` value
select Frida. Linux's existing uprobes remain available through the **Uprobes**
selector or `dynamic_instrumentation_method: "kernel_uprobes"`. Failures are
reported at Start, without silently switching engines.

## Build and run

On Linux, install Rust 1.88.0, a C compiler, curl and tar/xz, then:

```sh
./tools/frida/build.sh
./dist/frida/perfr --host 127.0.0.1 --serve 3000
```

The package contains a native `perfr-hook` linked to Frida Core and
`libperfr.so` linked to Gum. No Python environment or JavaScript
agent is needed. `tools/frida/devkits.sh` downloads Frida 17.17.0 native devkits,
verifies their pinned SHA-256 checksums, and caches them under
`target/frida-devkits`. Set `PERFR_FRIDA_DEVKIT_ROOT` to use another cache.
Builds thereafter can use the cached devkits without downloading them again.

A debug build of `perfr-hook` on a checkout that has no devkits (what
`cargo test --workspace` does on a fresh clone) links a stub instead and says
so in a build warning: that helper starts, and fails every injection with a
message naming `tools/frida/devkits.sh`. Release builds, and any build with
`PERFR_FRIDA_DEVKIT_ROOT` set, still stop when the devkit is missing.

On macOS, `tools/build/build-service-macos.sh` packages the helper and agent alongside the
service and manual SDK. `--universal` builds both architectures for all native
components. The agent must contain the target process's architecture.

For a development build:

```sh
./tools/frida/devkits.sh
cargo +1.88.0 build --locked -p perfr-hook
cargo +1.88.0 build --locked -p perfr-lib --features native
cargo +1.88.0 build --locked -p perfr
PERFR_HOOK="$PWD/target/debug/perfr-hook" \
PERFR_LIB="$PWD/target/debug/libperfr.so" \
  target/debug/perfr
```

Use `.dylib` on macOS. The service otherwise finds both components beside its
executable. A Cargo-only service build does not build the optional injector.
The agent's default Cargo feature set keeps lifecycle unit tests independent
of the devkits; a deployable agent requires `--features native`. Ordinary manual
captures need neither native Frida component. The musl service remains static;
its Frida helper and agent use the host's native libc.

Select a running process, load its functions, select functions, and record.
Linux uses the existing ELF/detached-debug-file index. macOS uses Frida to
inspect loaded Mach-O symbols and translates their addresses to segment file
offsets. Shared-cache images with unavailable symbols may not appear. Loading
Mac symbols itself requires permission to attach. Live Mach-O disassembly is
still separate future work.

## Recording and lifecycle

The Rust helper calls Frida Core's native injector to load the agent library
and invoke `perfr_frida_main`. On Darwin, Core first injects a tiny C bootstrap
embedded in the helper; it calls `dlopen` on the real agent so dyld owns its
dependencies and native thread-local storage on every target thread. The
bootstrap can unload after control ends, while the agent stays loaded.
The helper does not create a Frida script/session or load
a GumJS agent. Small C ABI adapters call Core and Gum directly; control and
capture state remain in Rust. Gum listeners call `perfr_start_dynamic` and
`perfr_stop` through the **same perfr API instance** used by manual scopes.
Per-invocation Gum storage holds the scope handle and capture generation.

The helper and agent exchange startup/stop messages over an AF_UNIX socket;
the helper authenticates the target's kernel-reported peer PID. Scope events
never use this socket. EOF on helper death detaches the listeners. Agent writes
suppress SIGPIPE locally, preserving the target's process-wide signal policy.
Gum keeps outstanding invocation data alive after detach, and the agent remains
resident so late returns cannot jump into unloaded code.

`perfr_init` publishes a versioned, process-local API descriptor in reserved
scope-header bytes. The injected agent uses that descriptor even when the SDK
is statically linked and its symbols are stripped. A producer-held file lease
prevents reuse of descriptor addresses after exit or exec. It does not replace the
application's segment, TLS state or handle allocator. If the application has no
perfr API, the agent initializes its bundled API. Initialize an application's
own SDK before attachment; loading/initializing another SDK instance afterward
is not supported. Older manually instrumented binaries need the updated SDK
for shared API discovery; attachment fails instead of replacing their segment.

Both sources write ordinary start/stop records to the same scope ring. The
service computes one nesting hierarchy per thread; async scopes remain outside
that hierarchy. Retained uprobes carry the same live provenance flag, but keep
their existing independent nesting implementation. Dynamic starts set `event::flags::DYNAMIC` (bit 3) in the
existing flags byte. Completed live events carry provenance in bit 7 of the
metadata byte (`LiveEvent::_pad`), leaving low bits for color mode. Capture
exports preserve that byte in an optional `flags` column; older files default
to zero. Batches carrying metadata use the existing raw live format because the
original packed format omits that byte. Event sizes and existing frame formats
are unchanged.

The capture-private file now holds only an attachment lease and a completed-call
counter, not a second stream of events. A read lock keeps callbacks from racing
capture teardown; a generation prevents late returns from entering a subsequent
capture. Ring overflow is reported through the ordinary scope-source loss
counter. Stop closes the capture generation and detaches Gum before the shared source's final drain. Scopes still
open at Stop are clipped to capture end, like manual scopes. Exceptions/longjmp
that bypass return can leave scopes open; the API does not infer unwinding.

The agent dylib stays loaded for reuse. Concurrent perfr collectors for one
target and self-hooking the service are refused. Fork-child callbacks and manual
API calls are disabled before touching inherited state; the child must exec
before instrumentation can resume. The existing 16-function selection limit and
32-bit wire thread IDs remain. Shared nesting still has the manual reader's
8-bit depth limit; very deep recursion is not represented faithfully.

## Platform requirements and current limits

Linux attachment must be permitted by ptrace/Yama and the target's dumpability
and namespace settings. The test target explicitly allows sibling attachment;
the profiler does not change machine policy. Existing uprobes retain their own
kernel capability requirements. Targets must be able to read the agent library
and map the capture file. For a root Linux service the capture file is owned by
the target's user. Start with a service and target running as the same user.

On Mac, task-port access, code signing, hardened runtime, library validation and
executable-memory policies constrain attachment and patching independently.
This integration does not bypass SIP or promise attachment to protected system
processes. Development targets are the initial supported use case. Build the
agent for the target architecture, including Rosetta when applicable.

Native end-to-end tests pass on Linux x86-64, Apple Silicon and Intel Macs.
The hosted Mac tests run both collector and development target as root to avoid
interactive task-port authorization prompts; this does not validate unprivileged
attachment. ARM Linux and Rosetta runtime validation remain pending.

Linux x86-64 timing hooks repair intercepted return addresses in copied perf
stacks before DWARF unwinding. The agent journals each invocation's thread,
stack slot, original return address, Gum trampoline, and entry and exit time,
in a part of the capture's lease file that only the service reads. The service
keeps that history until older samples have been consumed, Python enrichment
included, and patches only slots that still hold the expected trampoline. Real
target stacks are unchanged; entry and exit timing remains enabled.

A hook finds its return trampoline on its first call, by checking the pinned
Gum entry redirect against Gum's public invocation-stack translation API, and
registers it before Gum replaces that call's return slot. A registered
trampoline stays registered for the life of the process, because a call that
began under it may not have returned; every capture's unwinder stops at one it
has no history for. 4096 can be registered.

What a sample gets when the history is short of something, from least lost to
most:

- **A call with no entry** (the journal had no room, the hook is muted and
  gave way, or the call began under an earlier capture): the walk stops at
  that call. The frames inside it are right; its callers are missing.
- **A thread that lost an exit record**: its open entries are closed at the
  last instant the journal is known to have been complete, and are calls with
  no entry from then on. Its later calls are journaled as usual, and no other
  thread is affected.
- **A hook whose trampoline could not be identified** (a redirect the agent
  does not understand, or 4096 already registered): a thread is leaf-only
  while it is inside a call to that hook, in this capture and in any other
  that opens before the call returns.
- **A journal that can no longer be read** (a thread stopped for 250 ms in
  the middle of writing a record, the hooks were detached unexpectedly, or
  more than 262,144 calls were in flight at once): nothing is patched and
  walks stop at the registered trampolines. Samples become leaf-only only if
  a hook with an unidentified trampoline is in play as well.

None of these invents a caller, and each is named in the instrumentation
status. Calls spanning captures therefore remain partial.

The journal is eight rings of 4096 records, and a thread writes to the one
its id picks: a thread that calls a hook faster than the service reads fills
its own ring, and the few threads that share it, not everyone's. A hook muted
for passing the call-rate limit still has its returns intercepted by Gum, so
it still journals, but only into the first half of its ring. The journal's
two megabytes are allocated in `/tmp` when the hooks are armed, and arming
fails if there is no room: a sparse file would instead fault in the target on
its first write.

Journaling adds 100 to 160 ns to a call on one thread and about 170 ns with
eight threads calling at once (`cargo test -p perfr-lib --release
what_a_sampled_call -- --nocapture`, on a 13900KF). With one ring for every
thread the eight-thread figure was 1.1 µs. Its cost inside a real hooked
workload has not been measured.

A target in another PID namespace names its threads differently from the
sampler. The service translates through `NSpid` in `/proc/<pid>/task/*/status`;
this path has unit tests and has not been run against a containerised target.

`tools/e2e/frida_unwind.py` checks full sampled caller chains and all 480 timed
calls with recursive nesting in the same capture, across two threads and
repeated captures, with frame pointers omitted; that a call held across two
captures stays partial; and that a function hooked while two threads call it
flat out costs the other threads nothing: full chains on a thread with no hook
on its stack, and on one running the hooked recursion. Unit tests cover delayed
samples, reused slots, tail-call chains, concurrent publication, a full ring,
a lost exit, an unidentified trampoline and a stalled writer.

This repairs return substitution, not missing unwind tables for relocated
instructions, generated code, or hook callbacks. It does not certify C++ exception
handling through hooks or repair another tool's interception. Runtime validation
of this repair is Linux x86-64 only; CPU sampling on macOS remains future work.

The pinned upstream Core and Gum license texts are shipped under `licenses/`
with the native package. perfr's adapter, helper and transport use this
repository's BSD license. Devkit archives remain build inputs, not runtime files.

## Self-profiling

The service records `Frida:` phases in its own timeline: arming hooks, launching
and waiting for the helper, native-agent injection, Gum initialization, connecting
the scope API, executable-address resolution, installing each named trampoline,
and normal detach. Recording begins before attachment.

The installation span surrounds `gum_interceptor_attach`: it includes Gum's
relocation, trampoline construction and entry-patch work, rather than separate
measurements of those internal steps. Remote phases use host-wide
`CLOCK_MONOTONIC` timestamps and arrive as complete async spans on the service's
control-reader thread. They are attributed to the service, not application work;
concurrent phases may overlap. No second SDK is initialized in the target, and
per-invocation callbacks carry no additional self-profiling events. If the
controller dies, its final detach timing cannot be delivered.

## Validation

`tools/e2e/frida_smoke.py` checks attachment to an already-running C target,
exact function counts (270 per capture), three worker threads, seven alternating
manual/dynamic nesting levels, async isolation, source flags, parent containment,
fork isolation, target survival and repeated capture. `--inflight` stops with a blocked call
and releases its old return in a new capture; `--controller-death` additionally
kills the helper to exercise EOF cleanup and SIGPIPE handling. Separate runs cover targets
without a linked SDK and API discovery with its symbols stripped. It can
also exercise the retained backend with `--engine kernel_uprobes`.

Unit tests check lease validation/concurrency, rejection of stale capture
generations, and metadata round trips through live transport and capture files. `.github/workflows/frida-rust.yml` runs the native matrix.

Upstream references: [Frida modes](https://frida.re/docs/modes/),
[Gum](https://github.com/frida/frida-gum),
[Native C API](https://frida.re/docs/c-api/),
[Apple hardened runtime](https://developer.apple.com/documentation/xcode/configuring-the-hardened-runtime/).
