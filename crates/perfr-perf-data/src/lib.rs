// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Reads `perf.data`, the file `perf record` writes.
//!
//! Three layers, each usable without the one above it:
//!
//! - [`Reader`]: the header, the event attributes and the records, one at
//!   a time, from bytes pushed as they arrive. A file (`perf record`) and a
//!   pipe (`perf record -o -`) are both read; so is a file whose recording
//!   was killed before it wrote its sizes.
//! - [`Session`]: the records as owned [`Event`]s in time order, which the
//!   file is not in, with a `-z` recording's zstd stream decoded.
//! - [`Processes`]: the address spaces and thread names those events
//!   describe, to look a sampled address up in; and [`Switches`], when
//!   each thread was on a CPU.
//!
//! What a `perf.data` does not hold is a symbol: it has addresses, and the
//! paths and build ids of the files they are in. A caller with those files
//! names the frames; one without shows `module+0xoffset`
//! ([`Mapping::label`]).
//!
//! The kernel's `include/uapi/linux/perf_event.h` is the specification of
//! the records, and `tools/perf/Documentation/perf.data-file-format.txt`
//! of the file around them. Little-endian files only.

mod processes;
mod reader;
mod sched;
mod session;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg_attr(not(feature = "zstd"), path = "no_zstd.rs")]
mod zstd;

pub use processes::{unmapped_label, Mapping, Processes};
pub use reader::{
    build_ids, callchain, feature, feature_string, feature_strings, is_perf_data, record,
    sample_type, Attr, BuildId, Comm, Frame, Item, Mmap as MmapRecord, Reader, Record,
    Sample as SampleRecord, SampleId, Switch, Task, MAGIC,
};
pub use sched::{Span, Switches};
pub use session::{Event, Info, Mmap, Sample, Session, Stats, UserStack};

/// Reads a whole file held in memory: every event in time order, and the
/// session for its attributes, its info and its counts.
pub fn read_all(bytes: &[u8], keep_stacks: bool) -> Result<(Session, Vec<(u64, Event)>), String> {
    let mut session = Session::new(keep_stacks);
    let mut events = Vec::new();
    let mut at = 0;
    while !session.is_done() {
        match session.step(&bytes[at..], true)? {
            Some(used) => at += used,
            // With the whole file in hand the reader never asks for more.
            None => return Err("perf.data: the file ends in the middle of a table".into()),
        }
        while let Some(event) = session.next_event() {
            events.push(event);
        }
    }
    Ok((session, events))
}

#[cfg(test)]
mod tests;
