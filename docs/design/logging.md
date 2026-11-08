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
The capture file is `perfr-capture/1`.

## Queue record, after

Segment layout version stays **2**. Ring offsets do not move, so a segment
written by an older library still opens. `api_version` in the header
is **2**, which only records the new meanings below. The capture file is
still `perfr-capture/1`: no table changed shape.

What changed inside the same 56 bytes:

* `kind = 6` (`LOG`) is one chunk of a log line.
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
  still inside the first 4 KiB), holds published read cursors and a
  per-thread per-kind drop table. An older producer leaves them zero.

## Scopes group log lines

A log line does **not** store a scope id, and there is no category on it.
A line belongs to a scope when its timestamp is inside that scope's
enter/exit interval on the same thread. The interval is closed at both ends.
Nested scopes each contain the line.

That makes a scope the grouping: to read what a subsystem logged, wrap it in
a scope and ask for the lines inside that scope (click it in the viewer, or
pass `scope` to `/api/logs` and `perfr_logs`). A separate channel system
would be a second way to say the same thing, with its own registry, ids and
names to carry through the ring, the wire and the file.

The lookup is not the product of lines and scopes. A scope's instances on a
thread are merged into disjoint sorted intervals once, and each line is one
binary search on its own thread (`perfr_live_event::logs::SpanSet`).

## The loadable table

`perfr_log` was appended to `perfr_api_table_v1`. `perfr.h` loads any
library whose table is at least as long as the table first released
(`PERFR_API_TABLE_BASE_SIZE_`, the offset of `log`) and checks `size`
again where it calls `log`, so a program built with the new header still
runs against a library from before logging: every other call works and
`perfr_log` is a no-op. Nothing may be inserted above `log`; new entries
go below it and are checked the same way.

## Drops

There are two numbers, and only one of them is a count.

**How many** records a reader lost is its own lap count: its cursor fell
more than a ring behind the write cursor, and the difference is gone. This
is exact, per reader, and is what `events_lost` and every `DROP` marker add
up to.

**What** was lost comes from the producer. When a consumer has published
its read cursor and a claim would overwrite a slot at or past it, the
producer reads the event still in that slot and adds one to that event's
`(tid, kind)` counter before replacing it. The counter is on the control
page, so the lap cannot erase it.

The table can say more than the reader lost, so it is never reported on its
own:

* The cursor is published once per drain, right after the copy and before
  anything is done with the events. A slot the reader has copied but not yet
  published is counted when it is overwritten.
* There is one cursor per ring however many readers drain the segment
  (every `perfr` on a machine reads every segment). The last to publish
  wins, and the table then counts against that reader.
* The counters are totals since the process started. A reader records where
  they stand when it adopts a segment and only looks at what is added.

Each pass the consumer spends the table's new counts against its own loss.
If the table names less than was lost, the rest is one unattributed marker.
If it names more, each `(tid, kind)` gets its proportional share and the
surplus waits one pass, because a producer counts an overwrite the moment it
happens and the reader sees that lap on its next drain. A count still
unmatched after that was never this reader's loss and is dropped. A pass
that lost nothing writes no marker.

The CLI prints the breakdown. The log window shows the total in red, and
each thread grows a drops lane.

## Log text

A live event has no room for text, so a line's text is interned like a scope
name, once per distinct line, for the life of the capture, in the service
and in every viewer. The events are in a bounded ring and are evicted; the
strings are not. A capture therefore keeps at most 64 MiB of distinct log
text (`LOG_TEXT_BUDGET_BYTES`). Past that, a new line keeps its time, thread
and level, is flagged truncated, and reads "(log text not kept: ...)".
A line already kept is never affected, however often it repeats.

## Live event and capture file

`LiveEvent` stays 32 bytes.

| kind | value | payload |
|---|---|---|
| `LOG` (8) | instant | `extra` = level, `_pad` bit 6 = truncated, `name_id` = message |
| `DROP` (9) | instant | `duration_ns` = count (not a duration), `extra` = ring kind lost |

The events Arrow schema is unchanged and so is the manifest, so the file
format is still `perfr-capture/1`. A reader from before logging sees two
event kinds it does not know.
