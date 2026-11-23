# Timeline

Zoom from the cursor, pan, and light up one scope name. An empty timeline prints the same reminders: ruler wheel zoom, Ctrl+wheel zoom, WASD, Ctrl+drag to select scopes, Home or a double-click on the ruler to fit, space to follow. Navigation is on the [home page](../index.html#timeline). Scope search is on the [features page](../features.html#scope-search).

@clip 02-timeline-navigation | Live timeline | Cursor-anchored zoom, pan, and tooltips on nested hooked scopes

## Pan, zoom, follow, fit

| Key | What it does |
|---|---|
| **A** / **D**, or **Left** / **Right** | Pan in time. The arrows pan only when nothing is selected. With a selection, Left and Right nudge that selection instead |
| **W** / **S** | Zoom, locked to the cursor. Panning or zooming turns Follow off |
| **Up** / **Down** | Scroll the track list, when nothing is selected |
| **Page Up** / **Page Down** | Scroll the track list by a page |
| **Space** | Toggle **Follow latest** |
| **Home** | Fit the capture |

**Follow latest** keeps the window on the live edge. More → “Follow latest   Space” is the same switch. More → “Follow latest” off, or any pan or zoom, freezes the window where it is. When a live capture stops, Follow stops too and the window stays where it was. It does not jump out to the whole capture.

**Home**, or a double-click on the ruler, fits the content cluster: the time that actually has events, not the empty stretch before the first or after the last. The fitted window is never narrower than 1 µs, so a tiny capture cannot freeze the ruler. More → “Fit capture   Home”.

Wheel over the ruler always zooms. Over the tracks, the wheel scrolls (with inertia). **Ctrl+wheel** or **⌘+wheel**, or a pinch, zooms the tracks around the cursor. A left-drag that did not start on a sample bar pans.

These keys do nothing while a text field has focus, except **Escape**, which clears the scope search.

## Mouse on the tracks

| Gesture | Result |
|---|---|
| Hover a scope | Tooltip |
| Click a scope | Select it. Click empty canvas to clear |
| Double-click a scope | Zoom to it, at 1.1× its duration. A value lane does not zoom |
| Click a thread header | Focus that thread. Click it again to release. Other threads’ scheduler slices go grey, and a chip above the timeline names the thread |
| Click a thread’s triangle | Fold the thread, or unfold it. Folded, it keeps its state bar, its sample bar and its first row of scopes where they were; the rows under them go |
| Ctrl+click or ⌘+click a thread’s triangle | Fold or unfold every thread of that process, the way the clicked one goes |
| Click a sample tick | Copy that sample’s call stack to the clipboard, leaf first |
| Left-drag on a thread’s sample bar | Select that thread’s samples. The report recomputes for them |
| Right-drag anywhere | Select every thread’s samples in that range (ruler, frame strip, bar, or empty space). The span’s length is written between two arrows under the ruler; when the span is narrower than that text, the arrows stand outside it pointing in and the text goes beside them |
| Shift+drag | Add to the sample selection |
| Ctrl+drag or ⌘+drag | Draw a rectangle over scopes. On release the scopes inside are summarised and copied to the clipboard. The per-function summary covers all of them; the individual list stops at 500 and says how many more there were. The sampling report is then over the rectangle too: its time span, on the threads whose tracks it crosses |
| Right-click a scope | Menu: **Sampling report for this scope**, **Highlight every instance** |

Ctrl+drag does not zoom. The old note that said it did was wrong.

**Escape** clears the scope search, a Live-row highlight, the selection, and a rectangle with the samples it selected, all at once.

## Frames

A program that marks its frames (`perfr_frame_mark`, see [Instrument your code](index.html#instrument-your-code)) gets a strip under the ruler: one bar per frame, as tall as the frame was long. The strip always shows the whole capture. It does not zoom or pan with the timeline. The time the timeline is showing is a window on the strip, lit while the rest steps back, with a handle on each edge.

| Gesture | Result |
|---|---|
| Hover a bar | Its frame time, the rate that comes to, and its ratio to the median frame |
| Click a bar | The timeline jumps to that frame, centred, filling 90% of the width |
| Drag the window | Pan the timeline. The window keeps its length and stops at the capture's ends |
| Drag a handle | Move that edge of the timeline's window; the other edge stays. Out to grow it, in to shrink it |
| Drag from outside the window | The window comes to the pointer, by its middle, and follows it |
| Wheel, or pinch | Grow or shrink the timeline's window, a tenth a notch: wheel down to grow it, up to shrink it. The time under the pointer stays put while the pointer is on the window; off it, the window's nearer edge does |
| Click the label beside the strip | The next series of frames, when the capture has more than one |
| Right-drag | Measure, as on the ruler and the tracks, in the timeline's scale and not the strip's. The selection is one column from the ruler down, across the strip |

The window is never drawn narrower than ten pixels, so that zoomed in to one frame of a long capture it can still be seen and taken hold of; a drag moves the timeline by what the pointer travelled all the same.

A bar is drawn against the median frame of its series: the accent colour up to 1.5 medians, amber up to 2.5, red past that. The strip is three medians tall, so one long hitch does not flatten every other bar. Where several frames share a pixel the bar is the longest of them, and a click there goes to that one. The label beside the strip names the series and its median frame.

Behind the tracks of the process that marked the frames, every other frame is a little darker, so where a frame ends can be read off any of its threads. The scheduler and every other process keep their plain ground: the frames are not theirs. So does the row that names the process, so a folded process shows no bands. When several processes in a capture mark frames, each has its own bands behind its own tracks, and the strip shows the series of the process the capture is of. The bands fade out once frames are only a few pixels wide.

## Scope search

The box hint is `scope`. Typing a name leaves matching scopes lit and greys out the rest. The hover text is “Grey scopes that do not match”. Escape, or clearing the box, brings the rest back.

@clip 11-scope-search | Scope search | Scope search keeping matching scopes lit and greying out the rest

## Tracks filter

The box hint is `tracks`. It hides rows whose labels do not match, and a counter reads how many of the total are still shown. The × on that box clears it, and so does Escape. `?tracks=` on the page URL fills the box when the viewer loads, with `+` or `%20` as spaces.

`?collapse=scheduler` starts with the scheduler track folded, which matters on a machine with many cores.

## Track order

By default the target leads the rail and the busiest thread leads each process. Drag a thread, a graph or a process header to put it somewhere else.

### Remember it for a process

Right-click the name on the target's header and tick **Remember track order for** *name*. The same checkbox is in the settings window. The service then keeps the order as a text file, one per process name, and rewrites it each time you move a track or a header:

```
~/.config/perfr/track-order/<process name>.txt
```

The next capture of a process with that name starts in that order. Tracks the file does not name come after the ones it does, in the usual order. Untick the box and the file is deleted.

The file is plain text and can be edited or copied to another machine:

```
# a comment
[MyGame]
main
Compute Task Pool (*)
render
main > frame time
[Node Health]
CPU
```

| Line | Meaning |
|---|---|
| `[name]` | The tracks that follow belong to processes with this name. Lines before the first one belong to the capture's target |
| `name` | A thread |
| `thread > graph` | One graph of a thread |
| `thread > [async]` | A thread's async block |

`*` matches any run of characters and `?` one. Case does not matter. A process is matched by its name, its executable's name or its path; a pattern with a `/` in it is tried against the path only.

Processes come in the order of their sections and tracks in the order of their lines. The first line that matches a track places it. Tracks that one line matches are in name order, with numbers counted as numbers, so `Worker (2)` is before `Worker (10)`. A graph or an async block with no line of its own stays under its thread.

When the viewer writes the file, threads that follow one another and differ only in a rising number become one line, `Compute Task Pool (*)`, so the line still holds on a machine with more cores.

Two limits follow from matching by name. Threads with the same name cannot be told apart, so they stay together. A thread with no name is not in the file. With the option on, a track you drag lands where the file can keep it, which is what the next capture will show.

### In a saved or shared capture

A capture carries the order its tracks were in when it was saved or shared. **Save**, **Share** and `.perfr.stream` exports all include it, so the file opens the way it looked, with nothing to set up on the other side. That copy names tracks by process and thread id, so same-named threads keep their places too. An older viewer ignores it and shows its default order.

The full binding list is the [keyboard and mouse reference](keys.html).
