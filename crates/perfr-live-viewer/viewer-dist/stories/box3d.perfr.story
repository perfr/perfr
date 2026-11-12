{
  "format": "perfr.story",
  "version": 1,
  "title": "box3d capture",
  "summary": "A short tour of captures/box3d.perfr.stream. The steps point at the scheduler and the sample bar. They do not state a frame time or a core count; those are whatever this capture shows.",
  "capture": "captures/box3d.perfr.stream",
  "transition_ms": 800,
  "steps": [
    {
      "id": "context",
      "kind": "context",
      "title": "The whole capture",
      "body": "This is the box3d demo capture, fitted to its own span. The rail below is the machine, the process, and its threads.",
      "duration_ms": 4500,
      "view": {},
      "overlays": []
    },
    {
      "id": "problem",
      "kind": "problem",
      "title": "A stretch of the timeline",
      "body": "The highlighted band is the middle of the capture. Read the sample bar and the scopes inside it for where the time went. This text does not name a function.",
      "duration_ms": 5000,
      "view": {
        "t0_frac": 0.35,
        "t1_frac": 0.65,
        "fit": "span",
        "focused": [
          {
            "kind": "name",
            "name": "box3d"
          }
        ]
      },
      "overlays": [
        {
          "kind": "highlight",
          "id": "mid",
          "target": {
            "kind": "time",
            "t0_frac": 0.42,
            "t1_frac": 0.58
          }
        }
      ]
    },
    {
      "id": "cores",
      "kind": "evidence",
      "title": "The scheduler lanes",
      "body": "Each scheduler lane is one core. A lane with no slice in this window was idle here. Count the busy lanes on the capture; this story does not state how many there are.",
      "duration_ms": 5500,
      "view": {
        "t0_frac": 0.35,
        "t1_frac": 0.65,
        "fit": "span",
        "expanded": [
          {
            "kind": "scheduler"
          }
        ],
        "focused": [
          {
            "kind": "scheduler"
          }
        ]
      },
      "overlays": [
        {
          "kind": "rect",
          "id": "sched",
          "t0_frac": 0.35,
          "t1_frac": 0.65,
          "tracks": [
            {
              "kind": "scheduler"
            }
          ],
          "label": "scheduler"
        },
        {
          "kind": "callout",
          "id": "cores",
          "anchor": {
            "time_frac": 0.5,
            "track": {
              "kind": "scheduler"
            },
            "y": 0.15
          },
          "text": "One lane per core. Idle lanes stay empty."
        }
      ]
    },
    {
      "id": "samples",
      "kind": "evidence",
      "title": "Where the samples landed",
      "body": "The arrow points from the scheduler at the threads. The sampling report for this window is the list of functions the samples landed in. Open it on the capture before naming a hotspot.",
      "duration_ms": 5500,
      "view": {
        "t0_frac": 0.4,
        "t1_frac": 0.6,
        "fit": "span",
        "collapsed": [
          {
            "kind": "scheduler"
          }
        ],
        "focused": [
          {
            "kind": "name",
            "name": "box3d"
          }
        ]
      },
      "overlays": [
        {
          "kind": "arrow",
          "id": "to-threads",
          "from": {
            "time_frac": 0.48,
            "track": {
              "kind": "scheduler"
            },
            "y": 0.8
          },
          "to": {
            "time_frac": 0.52,
            "track": {
              "kind": "name",
              "name": "box3d"
            },
            "y": 0.25
          }
        }
      ]
    },
    {
      "id": "code",
      "kind": "proposed_fix",
      "title": "A code reference is a pointer",
      "body": "A code overlay is a file and a line the author wants the reader to open. It is not a measurement. This one is only an example of the overlay, and it is not a claim about box3d.",
      "duration_ms": 5000,
      "view": {
        "fit": "content"
      },
      "overlays": [
        {
          "kind": "code",
          "id": "example-code",
          "anchor": {
            "time_frac": 0.5,
            "track": {
              "kind": "name",
              "name": "box3d"
            },
            "y": 0.2
          },
          "file": "example/not-from-this-capture.rs",
          "line": 1,
          "snippet": "Replace this with a file and a line the capture's symbols support."
        }
      ]
    }
  ]
}
