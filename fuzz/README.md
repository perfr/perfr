# Fuzz targets

perfr opens files it did not write. A `.perfr.zip` or a `.perfr.stream` can
come from a colleague, a bug report or a share link; a Chrome or Perfetto
trace comes from another tool; a remote machine's stream comes over a
socket. The code that reads them should return an error for bytes it does
not understand, and never panic, read out of bounds or ask for all of
memory. These targets feed that code arbitrary bytes under
[libFuzzer](https://llvm.org/docs/LibFuzzer.html) and AddressSanitizer.

| Target | What it reads | Entry point |
|---|---|---|
| `live_stream` | a `.perfr.stream`, or a peer's WebSocket frames | `perfr_live_protocol::decode_all`, then `encode_frame` |
| `bundle` | a `.perfr.zip`: the zip, the manifest, the Parquet tables | `CaptureBundle::from_zip`, then `to_stream` and `slice` |
| `chrome_trace` | Chrome Trace Event JSON or a Perfetto proto | `perfr_live_chrome::ingest_collect` |
| `cpu_topology` | the machine a capture says it ran on (a JSON frame) | `CpuTopology::from_json`, then everything the viewer and a hub do with it |
| `proc_maps` | `/proc/<pid>/maps` | `perfr_maps::parse_maps` |

`seeds/<target>/` holds one or two small valid inputs for each, so a run
starts from something the parser accepts.

## Running one

The targets need a nightly toolchain and
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz); the `fuzz` workflow
names the versions it uses.

```sh
cargo install cargo-fuzz
cp Cargo.lock fuzz/Cargo.lock          # fuzz the versions the service ships with
cargo +nightly fuzz list
cargo +nightly fuzz run bundle fuzz/corpus/bundle fuzz/seeds/bundle -- -max_total_time=300
```

`fuzz/` is a workspace of its own, outside the one at the repository root:
`cargo build --workspace` there does not build libFuzzer. `corpus/`,
`artifacts/` and `Cargo.lock` here are not committed.

## When a target fails

libFuzzer writes the input to `fuzz/artifacts/<target>/`, and the workflow
uploads that directory. Reproduce it with
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`, fix the
parser, and add the input (minimized with `cargo fuzz tmin`) to
`seeds/<target>/` with a test beside the parser that names what it was.

If the input shows a flaw someone could use against a perfr user, report it
as [SECURITY.md](../SECURITY.md) says and not in a public issue.

## Adding a target

A file in `fuzz_targets/`, a `[[bin]]` for it in `Cargo.toml`, a seed in
`seeds/<name>/`, and a row in the table above. The workflow runs whatever
`cargo fuzz list` prints.

## What is not here yet

`perfr-object` (ELF, PE and PDB) and `perfr-unwind` (DWARF unwind tables)
read the binaries of the process being profiled, and the service's HTTP
handlers read requests. They are the next targets worth writing.
