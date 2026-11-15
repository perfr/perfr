// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The records of a `perf.data` as events in time order.
//!
//! `perf record` empties one ring per CPU after the other, so the file is
//! in time order within a CPU and not across them: a sample can come
//! before the `MMAP2` of the library it is in, or before the `FORK` of its
//! thread. `perf` marks the end of each pass over the rings with a
//! `FINISHED_ROUND` record, and whatever is older than the latest time of
//! the *previous* round can no longer be preceded by anything still to
//! come. Events wait in a queue until a round releases them; that is
//! `perf report`'s own rule.
//!
//! A recording made with `-z` holds its ring data as pieces of a zstd
//! stream; they are decoded here and read as the records they are.

use std::collections::{HashMap, VecDeque};

use crate::reader::{self, feature, Attr, Frame, Item, Reader, Record};
use crate::zstd::Decoder;

/// A record, with what it borrowed from the file copied out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Mmap(Mmap),
    Comm {
        pid: u32,
        tid: u32,
        name: String,
        exec: bool,
    },
    Fork {
        pid: u32,
        ppid: u32,
        tid: u32,
        ptid: u32,
    },
    Exit {
        pid: u32,
        tid: u32,
    },
    Sample(Sample),
    Switch {
        pid: u32,
        tid: u32,
        cpu: Option<u32>,
        out: bool,
        preempt: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mmap {
    pub pid: u32,
    pub start: u64,
    pub len: u64,
    pub pgoff: u64,
    pub path: String,
    pub build_id: Vec<u8>,
    pub kernel: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Index into [`Session::attrs`].
    pub attr: usize,
    pub pid: u32,
    pub tid: u32,
    pub cpu: Option<u32>,
    /// Innermost first; never empty for a sample that carries its pc.
    pub frames: Vec<Frame>,
    /// The registers and the copy of the stack the unwinder starts from,
    /// when the recording has them and the session was asked to keep them.
    pub user: Option<UserStack>,
}

/// What `perf record --call-graph dwarf` saves of a sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserStack {
    pub abi: u64,
    /// The registers of [`Attr::sample_regs_user`], in mask order.
    pub regs: Vec<u64>,
    pub stack: Vec<u8>,
}

/// What the file says about where it was recorded.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub hostname: String,
    pub os_release: String,
    pub arch: String,
    pub cmdline: Vec<String>,
    /// File name to build id, from the table `perf` appends. A newer
    /// `perf` puts the id in each `MMAP2` instead and writes no table.
    pub build_ids: HashMap<String, Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub records: u64,
    pub samples: u64,
    /// Samples that carry no time: the recording was made without
    /// timestamps, and there is nowhere on a timeline to put them.
    pub untimed_samples: u64,
    /// Records the kernel dropped because a ring was full.
    pub lost: u64,
}

struct Queued {
    time: u64,
    seq: u64,
    event: Event,
}

pub struct Session {
    reader: Reader,
    keep_stacks: bool,
    queue: Vec<Queued>,
    ready: VecDeque<(u64, Event)>,
    seq: u64,
    /// The latest time of the rounds before the current one.
    round_max: u64,
    max_seen: u64,
    zstd: Decoder,
    /// Decoded records, the last of them possibly not all there yet.
    inflated: Vec<u8>,
    done: bool,
    pub info: Info,
    pub stats: Stats,
}

impl Info {
    /// What a file held in memory says about where it was recorded,
    /// without making events of its records. In a file this is written
    /// after the data, so a reader that needs it before the first sample
    /// (which kernel, which architecture, which builds of the files) asks
    /// here first.
    pub fn read(bytes: &[u8]) -> Result<Info, String> {
        let mut reader = Reader::default();
        let mut session = Session::new(false);
        let mut at = 0;
        loop {
            let (used, item) = reader.step(&bytes[at..], true)?;
            at += used;
            match item {
                Item::Record(Record::BuildId(b)) => session.note_build_id(b),
                Item::Record(Record::Feature(bit, section)) => session.on_feature(bit, section),
                Item::Record(_) | Item::Nothing => {}
                Item::NeedBytes | Item::End => return Ok(session.info),
            }
        }
    }
}

impl Session {
    /// `keep_stacks`: copy the registers and the stack bytes of samples
    /// that have them (8 to 64 KiB each). Only a caller that will unwind
    /// them wants that.
    pub fn new(keep_stacks: bool) -> Session {
        Session {
            reader: Reader::default(),
            keep_stacks,
            queue: Vec::new(),
            ready: VecDeque::new(),
            seq: 0,
            round_max: 0,
            max_seen: 0,
            zstd: Decoder::default(),
            inflated: Vec::new(),
            done: false,
            info: Info::default(),
            stats: Stats::default(),
        }
    }

    pub fn attrs(&self) -> &[Attr] {
        self.reader.attrs()
    }

    /// The whole file has been read and its last events released.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Reads one step from `buf`, the bytes of the file not consumed yet
    /// (`ended`: none will follow them). Returns how many bytes it
    /// consumed, or `None` when it needs more than `buf` holds.
    pub fn step(&mut self, buf: &[u8], ended: bool) -> Result<Option<usize>, String> {
        if self.done {
            return Ok(Some(buf.len()));
        }
        let (used, item) = self.reader.step(buf, ended)?;
        match item {
            Item::NeedBytes => return Ok(None),
            Item::Nothing => {}
            Item::Record(record) => self.on_record(record)?,
            Item::End => {
                self.flush(u64::MAX);
                self.done = true;
            }
        }
        Ok(Some(used))
    }

    /// The next event in time order, with its time: 0 for what `perf`
    /// wrote down before the recording started (the mappings and names of
    /// what was already running).
    pub fn next_event(&mut self) -> Option<(u64, Event)> {
        self.ready.pop_front()
    }

    fn on_record(&mut self, record: Record<'_>) -> Result<(), String> {
        self.stats.records += 1;
        match record {
            Record::Mmap(m) => {
                let event = Event::Mmap(Mmap {
                    pid: m.pid,
                    start: m.start,
                    len: m.len,
                    pgoff: m.pgoff,
                    path: String::from_utf8_lossy(m.filename).into_owned(),
                    build_id: m.build_id.to_vec(),
                    kernel: m.kernel,
                });
                self.queue(m.at.time, event);
            }
            Record::Comm(c) => {
                let name = String::from_utf8_lossy(c.comm).into_owned();
                self.queue(
                    c.at.time,
                    Event::Comm {
                        pid: c.pid,
                        tid: c.tid,
                        name,
                        exec: c.exec,
                    },
                );
            }
            Record::Fork(t) => self.queue(
                t.time,
                Event::Fork {
                    pid: t.pid,
                    ppid: t.ppid,
                    tid: t.tid,
                    ptid: t.ptid,
                },
            ),
            Record::Exit(t) => self.queue(
                t.time,
                Event::Exit {
                    pid: t.pid,
                    tid: t.tid,
                },
            ),
            Record::Sample(s) => {
                self.stats.samples += 1;
                if !s.has_time {
                    self.stats.untimed_samples += 1;
                }
                let user =
                    (self.keep_stacks && s.regs_abi != 0 && !s.stack.is_empty()).then(|| {
                        UserStack {
                            abi: s.regs_abi,
                            regs: s.regs.chunks_exact(8).map(reader::le_u64).collect(),
                            stack: s.stack.to_vec(),
                        }
                    });
                let sample = Sample {
                    attr: s.attr,
                    pid: s.at.pid,
                    tid: s.at.tid,
                    cpu: s.at.cpu,
                    frames: reader::callchain(&s),
                    user,
                };
                self.queue(s.at.time, Event::Sample(sample));
            }
            Record::Switch(s) => {
                self.queue(
                    s.at.time,
                    Event::Switch {
                        pid: s.at.pid,
                        tid: s.at.tid,
                        cpu: s.at.cpu,
                        out: s.out,
                        preempt: s.preempt,
                    },
                );
            }
            Record::Lost(n) => self.stats.lost += n,
            Record::BuildId(b) => self.note_build_id(b),
            Record::FinishedRound => {
                self.flush(self.round_max);
                self.round_max = self.max_seen;
            }
            Record::Compressed(piece) => self.on_compressed(piece)?,
            Record::Feature(bit, section) => self.on_feature(bit, section),
            Record::Other(_) => {}
        }
        Ok(())
    }

    fn queue(&mut self, time: u64, event: Event) {
        self.max_seen = self.max_seen.max(time);
        self.seq += 1;
        self.queue.push(Queued {
            time,
            seq: self.seq,
            event,
        });
    }

    /// Releases, in time order, the events no later than `limit`. Among
    /// equal times the order of the file holds.
    fn flush(&mut self, limit: u64) {
        self.queue.sort_unstable_by_key(|q| (q.time, q.seq));
        let n = self.queue.partition_point(|q| q.time <= limit);
        self.ready
            .extend(self.queue.drain(..n).map(|q| (q.time, q.event)));
    }

    fn on_compressed(&mut self, piece: &[u8]) -> Result<(), String> {
        let mut inflated = std::mem::take(&mut self.inflated);
        self.zstd.push(piece, &mut inflated)?;
        let mut at = 0;
        while let Some(header) = inflated.get(at..at + 8) {
            let size = usize::from(u16::from_le_bytes([header[6], header[7]]));
            if size < 8 {
                return Err(
                    "perf.data: a record of fewer than 8 bytes in the compressed data".into(),
                );
            }
            let Some(bytes) = inflated.get(at..at + size) else {
                break;
            };
            let record = self.reader.record(bytes);
            // A compressed stream holds what the kernel wrote, which is
            // never another piece of itself.
            if !matches!(record, Record::Compressed(_)) {
                self.on_record(record)?;
            }
            at += size;
        }
        inflated.drain(..at);
        self.inflated = inflated;
        Ok(())
    }

    fn note_build_id(&mut self, b: reader::BuildId<'_>) {
        self.info.build_ids.insert(
            String::from_utf8_lossy(b.filename).into_owned(),
            b.id.to_vec(),
        );
    }

    fn on_feature(&mut self, bit: u32, section: &[u8]) {
        let string = || reader::feature_string(section).unwrap_or_default();
        match bit {
            feature::BUILD_ID => {
                for b in reader::build_ids(section) {
                    self.note_build_id(b);
                }
            }
            feature::HOSTNAME => self.info.hostname = string(),
            feature::OSRELEASE => self.info.os_release = string(),
            feature::ARCH => self.info.arch = string(),
            feature::CMDLINE => self.info.cmdline = reader::feature_strings(section),
            _ => {}
        }
    }
}
