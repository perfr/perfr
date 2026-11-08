---
name: perfr-story
description: Write a capture story for a perfr blog post or an iframe. Use when the user wants a narrated walkthrough of a capture, annotations on a timeline, a before/after, or an embeddable viewer. Drives the perfr MCP story tools. Does not edit the program's source.
---

# perfr story

A story is a JSON document (`format` `perfr.story`, `version` 1) of ordered steps. The viewer plays it over a capture: each step is a camera (time range, which tracks are open, where the rail is scrolled) and overlays anchored to the capture (a timestamp, a thread or track id, a scope id). Anchors are not screen pixels.

The story is not part of the capture. Export it and keep it beside the `.perfr.stream`. The viewer opens both with `?capture=<stream url>&story=<story url>`. Add `chrome=compact` for an iframe. perfr-web can store the JSON as `captures/<id>.perfr.story` next to `captures/<id>.perfr.stream`; a short link can later resolve to those two URLs. This skill does not upload either file.

## Investigate first

Profile with the perfr skill, then read the capture with the story query tools. Do not write a step until those tools have answered the question the step will point at.

1. `perfr_story_create` with a title, a one-line summary, and the capture URL.
2. `perfr_story_cpu` for the window you are about to show.
3. `perfr_story_hotspots` for that same window.
4. `perfr_story_find_scopes` and `perfr_story_find_threads` for the names you will anchor to. Use the `pid`, `tid`, and `scope_id` they return.

If a tool says the feature is unsupported, or the sample count is 0, say that. Do not fill the gap with a plausible function name or a core count.

## What a claim may say

A number in a title, a body, or a callout must be a number a tool returned for this capture, and the sentence must say it is from this capture. "The scheduler shows two busy cores in this window" is allowed after `perfr_story_cpu` reported `cores_busy: 2`. "The game only uses two cores" is not: the tool counted scheduling slices, and it did not describe the program's design.

A code overlay is a pointer: `file`, a 1-based `line`, and a short snippet. Use it only after you have opened that file and the line is the one you mean. If you have not opened it, do not add the overlay. A guess about where a constant is set belongs in the body as a hypothesis, not in a code overlay and not as a root cause.

Do not invent a frame time, a percent, or a function that the hotspots list did not return.

## The arc

Six steps is enough. Skip a step when you have nothing measured for it.

| kind | What it shows |
|---|---|
| `context` | The whole capture, or the process the post is about. No conclusion. |
| `problem` | The symptom as the capture shows it: a time range, the sample bar, the scopes in that range. |
| `evidence` | One measurement. The scheduler lanes, or the hotspot list, not both crammed into one sentence. |
| `root_cause` | The function the samples landed in, and where. Only if the hotspots and a source line you opened agree. |
| `proposed_fix` | What you would change. A code overlay here is the line you would edit, and only if you opened it. |
| `applied_fix` or `before_after` | Two windows, or two captures. The numbers are the two tool readings. Do not subtract them in your head and call the difference a speedup unless you show both readings. |

`duration_ms` is how long Play holds the step after the camera arrives. 4–6 seconds for a sentence. 7–8 if the reader has to count lanes. Under 3 seconds the camera is still moving when the step ends.

## Annotation style

- One idea per step. One or two overlays. A third overlay is a sign the step should be split.
- Callout text is a sentence, not a paragraph. `**bold**` and `` `code` `` are enough markdown.
- An arrow goes from the thing you name to the thing you mean. Both ends are capture anchors.
- A rect or a time highlight is the window the sentence is about. Use `t0_ns`/`t1_ns` from the query, or fractions of the capture when you mean "the middle".
- A scope highlight uses `scope_id` from `perfr_story_find_scopes`. A name match is a fallback when you do not have an id.
- Prefer `kind: thread` with pid and tid over `kind: name` once you have the ids. `kind: name` is for a first draft.
- `fit: content` for the opening. `fit: span` once you are inside a window.
- Collapse the scheduler when the step is about functions. Expand it when the step is about cores, and focus it.

## Embed

The viewer URL, with the stream and the story each encoded:

```
viewer/?capture=<encoded stream url>&story=<encoded story url>&chrome=compact
```

`chrome=compact` hides the capture controls and keeps Play, the step list, and Share. `?capture=` still selects the capture; the story's `capture` field is used only when the page did not pass one.

In a post, the iframe is that URL. Under it, repeat the numbers in text so the post still says something if the frame does not load. Those numbers are the same tool readings, not a tighter claim.

## Worked outline: a Godot capture

This is an outline, not a finished story. The capture is not in this repository. Do not fill the brackets with invented numbers. Where a line says hypothesis, leave it as a hypothesis until you have opened the file.

1. **context.** Fit the whole capture. Title: the scene and the machine the capture was taken on, from the capture's own process name. Body: what was running. No frame-time budget.

2. **problem.** A window around a heavy frame, chosen from the timeline, not from a target frame time. Callout: "Samples in this window." Do not write a millisecond figure unless `perfr_story_hotspots` or the scope stats returned one you are quoting.

3. **evidence, cores.** Expand and focus the scheduler. `perfr_story_cpu` on this window. Callout quotes `cores_busy` of `core_count`, and says they are the cores with scheduling slices in this window. If the reading is 4 of N, the sentence is "In this window, 4 of N cores have scheduling slices." It is not "Godot only uses 4 cores."

4. **evidence, time.** Collapse the scheduler. `perfr_story_hotspots` on the same window. The callout names the top function and its sample share, as the tool returned them. A scope highlight uses that function's id if `perfr_story_find_scopes` found it.

5. **root cause, as a hypothesis.** The question is where Godot decides how many worker threads to start, or where it pins them. Search the Godot tree (worker thread pool, processor count). The file you land on is a hypothesis until you have read it and the samples in step 4 are in that code or in something it calls. Until then the body says "Hypothesis, not checked against this capture: the thread count is chosen in <file you opened>." Do not add a code overlay for a file you have not opened. Do not state the line as the cause.

6. **before / after.** Only if you have a second capture taken after a change. Two steps, or one step whose body quotes both `perfr_story_cpu` or hotspot readings. The title does not say "fixed" unless both readings are in the post and the change is the one you made.

Export with `perfr_story_export`. The viewer line for the post is `?capture=<the Godot stream>&story=<the exported document>&chrome=compact`.
