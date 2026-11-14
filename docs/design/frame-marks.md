<!--
Copyright (c) 2026 The perfr Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.
-->

# Frame marks

A program with a loop says where each turn of it ends with one call,
`perfr_frame_mark`. Everything else follows from the marks: the frames, their
times, and where they fall against every other track.

## The call

```c
void perfr_frame_mark(const char* name, size_t name_len);
```

"The frame that was running ends here and the next one begins." There is no
begin/end pair: a loop has one boundary per turn, and one call cannot be left
unbalanced. `name` says which series the mark belongs to, for a program with
more than one loop (a render thread, a fixed-step simulation); null or empty
is the main series. Rust has `frame_mark()` and `frame_mark_named(name)`, C
the macros `PERFR_FRAME_MARK()` and `PERFR_FRAME_MARK_NAMED("physics")`,
Python `perfr_api.frame_mark(name="")`.

It was appended to `perfr_api_table_v1` after `log` and is read the same
way: `perfr.h` calls it only when the library's `size` covers the field, so a
program built with this header still runs against an older library and the
call is a no-op there.

## On the ring

`kind = 7` (`FRAME`), written exactly the way a scope's name is: the series
name inline, continued in `TEXT` records when it is longer than 32 bytes,
keyed by a handle from the thread's counter. An empty name is one record. The
segment layout and `API_VERSION` do not change: no existing call changed
meaning, and a service from before this kind skips the record.

A lost mark cannot be named by the producer's drop table. The table has a
slot for kinds 0..=6 and one for "could not be read", it is shared with
producers built before this kind, and so a lost `FRAME` record is counted in
that last slot. The count of what was lost is exact as before.

## In the service

Two consecutive marks of one series, in one process, are one frame. The
service keeps the frame in progress per `(process, series)` and pushes it
when the next mark arrives, as a live event:

| field | value |
|---|---|
| `kind` | `FRAME` (10) |
| `start_ns`, `duration_ns` | from the mark that began the frame to the mark that ended it |
| `pid`, `tid` | the process, and the thread that made the first of the two marks |
| `name_id` | the series; `frame` for the main one |
| `extra` | the low byte of the frame's number in its series |

- The first mark of a series only begins a frame. The frame in progress when
  the capture ends, or when the process goes, is never pushed. Neither the
  time before the first mark nor the time after the last was measured by
  anyone.
- When a ring laps, a mark may be among what was lost and the frame in
  progress would run on into the next. Every frame in progress in that
  process is dropped, as open scopes are; the next mark starts afresh.
- A process may have 256 series in progress. Past that a mark for a new one
  is not followed, and the service log says so once.

`LiveEvent` stays 32 bytes and the capture file is still `perfr-capture/1`:
a frame is a row of the events table like any other, of a kind an older
reader does not know.

## In the viewer

A frame is not a row of the timeline. Its lane (one per series, keyed like a
value's by the series name) stays in the index and is left out of the track
catalogue and of the level-of-detail decision. It is drawn twice
(`perfr-live-viewer/src/frame_marks.rs`):

- **The strip** under the ruler: one bar per frame, as tall as the frame was
  long, across the whole capture whatever the timeline is zoomed to. Where
  several frames share a pixel column the bar is the longest of them. Height
  and colour are relative to the median frame of the series (three medians is
  full height; past 1.5 amber, past 2.5 red), so the strip needs no target
  frame rate and one long hitch does not flatten it. A click shows the frame
  in the timeline at nine tenths of its width, centred. The timeline's
  window is drawn on the strip, never narrower than ten points, with a
  handle on each edge. A drag of the window pans and a drag of a handle
  moves its edge; either is decided by where the button went down, and
  carries the pointer's distance from what it moves, so nothing jumps when a
  handle is taken a little off its edge or the window is drawn wider than it
  is. A drag that begins off the window takes the window by its middle, as
  the capture slider does. The wheel over the strip, or a pinch, grows and
  shrinks the window by the same step as over the ruler, anchored the way
  the capture slider anchors it: on the time under the pointer while the
  pointer is on the window, on the window's nearer edge otherwise.
- **The bands** behind the tracks of the process that marked the frames:
  every other frame a little darker. They are drawn behind that process's
  rows and no others (its header, threads, async blocks and graphs,
  wherever a track of it has been dragged to), so the scheduler and every
  other process keep their plain ground: a frame of one program says
  nothing about another's threads. When several processes mark frames each
  has its own bands, from one series of its own, and the strip offers the
  series of the capture's process first. Whose
  turn it is comes from the frame's number, not its place in the lane, so the
  bands do not swap when old frames leave the ring. They fade out between
  eight and two points per frame.

The strip's bars are recomputed when the lane, the capture's extent or the
strip's width change, and there is at most one per pixel column. The bands
look only at the frames in view.

## Stats

`GET /api/functions/stats` and the `perfr_function_stats` MCP tool fold
frames into one row per series, of kind `frame`: count, total, average,
minimum, maximum and deviation of the frame times.
