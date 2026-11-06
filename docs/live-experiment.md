# Live experiment relay

One machine runs the instrumented workload and a relay. The relay reads
perfr's live frames on a loopback socket and fans them out over WebSocket.
Browsers do not open any other connection to the relay. They cannot change
the capture: the control knobs below drive a synthetic source only.

## Pieces

`perfr` (or `perfr-live-service`) encodes each live frame once, the same
`OLIV` bytes a local viewer already understands. With
`PERFR_RELAY_INGEST=127.0.0.1:44781` it `try_send`s that buffer onto a
bounded queue. A side thread writes the queue to the relay. If the queue is
full the forward is dropped and counted, and the capture continues.

`perfr-relay --ingest 127.0.0.1:44781 --bind 0.0.0.0:44780` accepts that
socket only from loopback. Event batches are stored once in a broadcast
ring, as `Bytes`. Each browser keeps a cursor of a few words. Publishing
writes one slot and wakes waiters; it does not walk the client list and it
does not wait on a slow socket.

A slot is a seqlock. The writer stores an odd sequence, publishes the
payload, then stores the next even sequence. A reader copies the sequence,
copies the metadata, loads the payload, and copies the sequence again. If
the two snapshots differ, the copy is discarded. Slots are aligned and
padded to cache lines so a torn copy cannot spill into the next slot.

A client more than four batches behind, or whose slot has been overwritten,
jumps to the newest batch. The events it skipped are counted for that
client only and reported in the status frame's `dropped` field. Other
clients are unaffected.

## Sessions

Idle default is 60 seconds (`--idle-secs` changes it). The session is idle
only when both have been quiet for that long: no control message and no
interaction ping from the client, and no event batch delivered to that
socket. Either one keeps it alive.

The hard cap is 15 minutes from connect. Pings and events do not move it.
Hello carries the deadline as `session_expires_unix_ms` (unix epoch
milliseconds) after the 8-byte hello payload. The same field is repeated on
the periodic status tail. That is the countdown value. It is not the idle
deadline.

The viewer sends `{"op":"ping"}` on pointer, key, and wheel input, at most
once every 5 seconds. A bare text frame `ping` is the same thing. A
WebSocket protocol ping is answered and does not reset the idle timer.

## Client count

The number of open WebSocket tasks is an atomic. Publish does not touch it.

`GET /status` returns JSON:

```json
{"clients": 12, "events_per_sec": 19000, "uptime": 3.5}
```

`uptime` is seconds since the relay process started. `events_per_sec` is
how many events the ring accepted in the last second, not how many each
browser received.

About once a second, and again when that client drops batches, the socket
gets a normal status frame (type 6). The first 68 payload bytes are the
status the current viewer decodes. `dropped` there is how many events
**this** client skipped. Older decoders ignore the rest. The tail is four
little-endian `u64`s:

| offset from start of payload | field |
|---|---|
| 68 | `clients` |
| 76 | `events_per_sec` |
| 84 | `uptime_ms` |
| 92 | `session_expires_unix_ms` |

`perfr-live-protocol::read_relay_status` and `read_hello_expiry` read these.
The viewer's status panel shows **Viewers** and a **Session** `m:ss` from
them when they are present.

## What the page connects to

WebSocket `GET /ws`, binary messages, one `OLIV` frame per message. With no
query string the viewer uses `ws://<page host>/ws` or `wss://` on https.
`?ws=ws://relay-host:44780/ws` points it at the relay. The value must be
`ws:` or `wss:`.

A page that does not know about the tail still decodes Hello, names,
capture marks, and event batches. Raw batches are 32 bytes per event. That
is the form the relay's synthetic source sends: one buffer per batch, not
a serialization per browser.

## Control

`POST /control` with a JSON object, any subset of:

```json
{"rate": 20000, "noise": 0.1, "herd": 1000}
```

`rate` is synthetic events per second. `noise` is clamped to 0..=1 and is
the fraction of those events whose duration is jittered. `herd` is a
one-shot burst of that many events, then it clears. The limiter allows
`--control-per-sec` requests (default 8). Past that the response is HTTP
429 and the knobs do not change. `GET /control` reads them and is not
limited.

These knobs are read only by the synthetic task (`--synthetic` / `--rate`).
Frames that arrived on the ingest socket are published as-is.

## Scale

The target people have used for this demo is thousands of browsers on one
stream, with headroom before a serialization-per-client design would fall
over. Two different limits get mixed together if they are stated as one
number.

**Bandwidth.** A 1 Gbit/s link is 125,000,000 bytes/s.

| per viewer | viewers to fill 1 Gbit/s |
|---|---|
| 10 KB/s (10,000 bytes/s) | 12,500 |
| 50 KB/s | 2,500 |
| 640 KB/s (20,000 raw 32-byte events/s) | about 195 |

A planning note for this work said a 1 Gbit/s link at 10–50 KB/s fills
around 200–1000 viewers. That range is what the division gives for roughly
125–625 KB/s per viewer, which is the neighborhood of a full raw event
stream, not of 10–50 KB/s. This page uses the table above. None of those
rows is a measurement of a NIC.

**This machine.** The numbers below are from one run on a 4 vCPU Intel Xeon
cloud VM with 15 GiB RAM. Binaries were `cargo build -p perfr-relay --bins
--profile bench`: opt-level 3, LTO off, debug info left on. The relay and
the load generator shared the VM. Clients were `perfr-relay-load` on
loopback, not browsers, and not a gigabit path. The synthetic source was
asked for 20,000 events/s in raw batches of 64. Each step is 3 seconds
after a 400 ms warmup. Latency is the batch's unix timestamp to the load
client finishing the read of that WebSocket frame.

The Orbit note "Where the Frame Went" is the reason this path is batched
binary frames rather than a per-event encode. That note's viewer-side
figure, about 5 ms per frame and later 0.14 ms, was not re-measured here.
Nothing in the table is a viewer ingest time.

| clients | received events/s | MiB/s | p50 µs | p95 µs | p99 µs | relay CPU, % of one core | RSS MiB | producer events/s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 22940 | 0.70 | 170 | 316 | 42613 | 0 | 6.7 | 19136 |
| 8 | 182157 | 5.58 | 199 | 40785 | 43708 | 1 | 9.4 | 19200 |
| 32 | 732744 | 22.46 | 19329 | 43423 | 47125 | 4 | 12.4 | 18816 |
| 64 | 1458360 | 44.71 | 20243 | 43601 | 47270 | 7 | 15.2 | 18816 |
| 128 | 2923626 | 89.63 | 20518 | 43813 | 47631 | 13 | 16.7 | 19200 |
| 256 | 5495863 | 168.63 | 19954 | 43867 | 47676 | 22 | 17.8 | 18965 |
| 512 | 10977225 | 337.64 | 18820 | 43986 | 46565 | 41 | 16.9 | 18816 |
| 1024 | 14688160 | 460.50 | 5071 | 44719 | 47483 | 77 | 25.2 | 19200 |

Producer rate stays near the requested 20,000 through the whole sweep. RSS
stays under 30 MiB. Relay CPU at 1024 clients is about three quarters of
one core, with the other cores running the load clients.

Received events track `clients × producer rate` through 512 clients. At
1024 clients, 1024 × 19,200 is about 19.7 million events/s if nobody
dropped; the clients received 14.7 million. That is the first step in this
run where clients fell behind the coalesce window. It is not a NIC limit
and it is not a claim about a browser. Median latency is under 200 µs
through 8 clients and about one 20 ms producer tick from 32 clients on.
The 99th percentile sits near 43–48 ms for every step, including the single
client, so that tail is not something that appears only when the fan-out
is large.

A 1 Gbit/s path carrying this same 20,000-event raw stream (about 0.7
MiB/s per client in the one-client row) would be full around 200 clients,
which is before the coalesce loss seen here at 1024 loopback clients.
Loopback at 460 MiB/s is not that path.

## Run

```text
perfr-relay --bind 0.0.0.0:44780 --ingest 127.0.0.1:44781 --idle-secs 60
PERFR_RELAY_INGEST=127.0.0.1:44781 perfr

# synthetic stream instead of a capture, and a local fan-out check
perfr-relay-load --spawn --clients 1,8,32,64 --duration-secs 3 --rate 20000
```
