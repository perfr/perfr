# What instrumentation costs

`overhead.py` answers the question asked first about any profiler: what does
it cost the program? It measures, per call, what a manual scope and a hooked
function add to a function that has neither.

```sh
cargo build --release -p perfr -p perfr-lib -p perfr-hook -p perfr-api
python3 tools/performance/overhead/overhead.py --out /tmp/overhead
```

It starts a service of its own on a free port, with its settings in `--out`,
so a service you have running is left alone. A run takes about a minute
and writes `overhead.md` (the tables below) and `overhead.json` (every batch).
`--quick` takes half a minute and is for seeing that it runs, not for figures
to quote.

## What is measured

`overhead_target.c` calls one small function in a loop, three ways:

- `plain`: `overhead_plain(x, rounds)`, never instrumented.
- `hooked`: `overhead_hooked(x, rounds)`, the same body, which the driver
  hooks.
- `scope`: `perfr_start("overhead_scope")`, the plain call, `perfr_stop`.
  `scope_long` is the same under a 96-byte name.

`rounds` is how long the function is: each round is one turn of a xorshift
that waits on the turn before, a little over a nanosecond on a current
desktop processor. The default sizes are 0, 100, 1000 and 10000 rounds: an
empty function, then about 0.1, 1 and 10 microseconds. The size matters
because nobody hooks an empty function, and the cost of a record is not the
same when the program has had ten microseconds to push it out of the cache.

Each figure comes from batches taken in turns, one of the plain loop and one
of the instrumented one, fifteen times; what is reported as added is the
median of the fifteen differences. In turns, because a capture's sampler
interrupts both loops and the processor's clock speed moves under both, and
only the difference is wanted.

The program is built twice: finding `libperfr` at run time, which is what
`perfr.h` does unless told otherwise, and with the API linked in
(`PERFR_STATIC`), which is also what a Rust program calling `perfr_api` has.
`overhead_target.py` is the same thing for the Python package.

## The states

A manual scope is measured:

- before `perfr_init`: the API is in the program and nothing else is;
- after it, with no capture: the cost of shipping instrumentation;
- in a capture, at each function size, and on 2, 4 and 8 threads;
- after the capture.

A hooked function is measured:

- before it is hooked;
- hooked, with auto-unhook off, at each size and on 2, 4 and 8 threads;
- hooked with auto-unhook at its default of 100,000 calls a second, called
  until the status line says the hook is off: what a hook costs once it has
  been switched off;
- after the capture, when the function's first bytes are its own again.

Every capture row starts a capture of its own, so `recorded` is that row's:
the share of the calls made that the capture holds when it stops. Below 100%
the program wrote records faster than the service read them. The row is
still the cost of writing them, but it is not a rate anyone can record at.

## What fails the run

Not a slow number, unless a budget is given. The checks are that the
measurement is one:

- a capture row recorded at least some of its calls;
- the auto-unhook rows say on the status line that the hook was switched off;
- after the capture a scope costs what it did before, and the hooked
  function what the plain one does;
- every measured thread was pinned to its CPU.

`--max-scope-ns` and `--max-hook-ns` add a ceiling on what a recorded scope
and a Frida hook add to the one-microsecond function, for a machine that
runs this on a schedule. No workflow runs it: hosted runners share their
cores, and a figure taken there says more about the neighbours.

## Reading it honestly

- The threads are pinned, one to a core, to performance cores where the
  processor has two kinds (`--cpus` to choose). The service is not pinned.
- Sampling and the scheduler are recorded during every capture, as they are
  when someone presses Record. Their cost lands on the plain loop too, so it
  is not in `added`; compare a row's `plain call` with the rows outside a
  capture to see it.
- A tight loop around an empty function is the worst case, and the number
  it gives is a throughput: the processor overlaps one call's work with the
  next. The rows with a body are what a call adds to a function.
- `--uprobes` adds the kernel engine: a uprobe at the function's entry and a
  uretprobe at its return, so its rows are the pair. It needs the service
  privileged: `--sudo` runs it through `sudo -n`, and through `perfr-sudo`
  when `tools/sudo/install.sh` has installed it (`--sudo-wrapper` names
  another). With it a run takes two minutes.
- A capture switches recording on in every program that has the API, not
  only the one it is of. If another perfr service on the machine starts a
  capture during a row that has none, the row's batches disagree; it is
  taken again, and the run fails if it never settles.
- Linux only as written: the target pins threads with `sched_setaffinity`
  and the driver reads `/sys` for the cores.
