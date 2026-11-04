# Logging on the scope ring

A log line is another record on the same per-thread rings as scopes,
instants and values. The slot stays 64 bytes (8-byte sequence stamp plus
the 56-byte `ScopeEvent`). There is no string arena.

## Queue record, before

Segment layout version **2**. `ScopeEvent` is 56 bytes:

| field | size | meaning |
|---|---|---|
| `timestamp_ns` | u64 | `CLOCK_MONOTONIC` |
| `scope_id` | u64 | handle, or the `f64` bits of a value |
| `tid` | u32 | thread |
| `kind` | u8 | 0 start, 1 stop, 2 instant, 3 value, 4 text continuation, 5 link |
| `depth` | u8 | 0, or the text-chain index on a continuation |
| `flags` | u8 | `MORE_TEXT`, `CUT`, `ASYNC`, `DYNAMIC` |
| `text_len` | u8 | bytes of `text` used |
| `text` | 32 | inline name |

Names longer than 32 bytes continue as `kind = TEXT` records. `depth` is
the chain index (1..=255), so a name stops at 8 KiB. A missing chunk is
`Completeness::Truncated`. A lap is a single `dropped` count on the drain.
The capture file is `perfr-capture/1`: the events table has no channel column.

## Queue record, after

Segment layout version stays **2**. Ring offsets do not move, so a segment
written by an older `libperfr_api` still opens. `api_version` in the header
is **2**, which only records the new meanings below. Older captures on disk
are `perfr-capture/1` and still load; new bundles are `perfr-capture/2` and
add an optional `bundle.channels` array. A file without that array has no
named channels (every scope is the default channel, id 0).

What changed inside the same 56 bytes:

* `kind = 6` (`LOG`) is one chunk of a log line.
* On `SCOPE_START` only, `depth` is the channel id captured at enter
  (0 is the default). The reader still computes nesting itself. Continuations
  still use `depth` as the chain index. Log chunks store 0 there.
* Log `scope_id`: bits 0..16 chunk index, bits 16..32 total chunks, bits
  32..64 message id. Flags bit 0 is cleared on the last chunk, bit 1 (`CUT`)
  is set when the line was longer than `MAX_LOG_BYTES` (~32 KiB), bits 4..6
  are the level. Chunk 0's first four text bytes are the original length,
  little-endian. The rest of the text is the message, split on bytes.
* Chunks of one line share `timestamp_ns` and `tid`. The consumer sorts by
  `(timestamp_ns, tid)` and the sort is stable, so one thread's chunks stay
  in the order that thread wrote them. Thread id is the tiebreak when two
  threads stamp the same nanosecond.
* A missing chunk inserts U+FFFD and the live event is flagged truncated.
  The line is never presented as complete.
* The control page, in bytes that used to be zero (after the reader leases,
  still inside the first 4 KiB), holds published read cursors, a per-thread
  per-kind drop table, and the channel-name directory (48 names, 28 bytes
  each). An older producer leaves them zero.

A log line does **not** store a channel id or a scope id. The viewer
attaches a line to a scope when the line's timestamp is inside that scope's
enter/exit interval on the same thread. The interval is closed at both ends.

## Channel

`register_channel(name)` runs once and writes the name into the control-page
directory. The hot path is a thread-local id. `ChannelGuard` pushes and
pops that id and emits nothing. Scope enter copies the id into `depth`.
That byte was already stored (it used to be zero). Logging does not read it.

## Drops

When the consumer has published its read cursor and a claim would overwrite
an unread slot, the producer reads the event still in that slot and adds one
to that event's `(tid, kind)` counter before replacing it. The counter is on
the control page, so the lap cannot erase the record of the loss. The
consumer still counts the lap (`events_lost`) and writes a `DROP` live event
per thread and kind, plus an unattributed marker for any lap it saw that the
table did not name. The CLI prints the breakdown. The log window shows the
total in red, and each thread grows a drops lane.

## Live event and capture file

`LiveEvent` stays 32 bytes.

| kind | value | payload |
|---|---|---|
| `LOG` (8) | instant | `extra` = level, `_pad` bit 6 = truncated, `name_id` = message |
| `DROP` (9) | instant | `duration_ns` = count (not a duration), `extra` = ring kind lost |

`API_SCOPE` / `API_TRACK` `extra` is the channel id. Older events have
`extra = 0`, which is the default channel.

The events Arrow schema is unchanged, so a `/1` file and a `/2` file use the
same columns. `/2` only adds `bundle.channels` in the manifest.
