// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The bytes of a `perf.data`: its header, its event attributes, and the
//! records of its data section, one at a time.
//!
//! The reader holds no bytes of the file. The caller keeps what has not
//! been consumed and hands it back, so a file is read as it arrives and a
//! section nothing is made of is passed over without being held.

use std::collections::HashMap;

/// The first eight bytes of every `perf.data`, file or pipe.
pub const MAGIC: [u8; 8] = *b"PERFILE2";
/// The same, written by a machine of the other byte order.
const MAGIC_SWAPPED: [u8; 8] = *b"2ELIFREP";

const FILE_HEADER_LEN: usize = 104;
const PIPE_HEADER_LEN: usize = 16;
const RECORD_HEADER_LEN: usize = 8;
/// `perf_file_section`: an offset and a size.
const SECTION_LEN: usize = 16;
/// More than this between the header and the data, or after the data, is
/// not attributes or features this reader will hold in memory.
const SIDE_SECTION_LIMIT: u64 = 256 << 20;

/// Whether `bytes` begin a `perf.data`. False for fewer than eight bytes.
pub fn is_perf_data(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC) || bytes.starts_with(&MAGIC_SWAPPED)
}

/// `perf_event_attr.sample_type`: what a sample record carries, in the
/// order the fields are laid out.
pub mod sample_type {
    pub const IP: u64 = 1 << 0;
    pub const TID: u64 = 1 << 1;
    pub const TIME: u64 = 1 << 2;
    pub const ADDR: u64 = 1 << 3;
    pub const READ: u64 = 1 << 4;
    pub const CALLCHAIN: u64 = 1 << 5;
    pub const ID: u64 = 1 << 6;
    pub const CPU: u64 = 1 << 7;
    pub const PERIOD: u64 = 1 << 8;
    pub const STREAM_ID: u64 = 1 << 9;
    pub const RAW: u64 = 1 << 10;
    pub const BRANCH_STACK: u64 = 1 << 11;
    pub const REGS_USER: u64 = 1 << 12;
    pub const STACK_USER: u64 = 1 << 13;
    pub const IDENTIFIER: u64 = 1 << 16;
}

/// `perf_event_header.type`. Below 64 the kernel wrote the record; from 64
/// on `perf` did.
pub mod record {
    pub const MMAP: u32 = 1;
    pub const LOST: u32 = 2;
    pub const COMM: u32 = 3;
    pub const EXIT: u32 = 4;
    pub const FORK: u32 = 7;
    pub const SAMPLE: u32 = 9;
    pub const MMAP2: u32 = 10;
    pub const LOST_SAMPLES: u32 = 13;
    pub const SWITCH: u32 = 14;
    pub const SWITCH_CPU_WIDE: u32 = 15;
    pub const HEADER_ATTR: u32 = 64;
    pub const HEADER_TRACING_DATA: u32 = 66;
    pub const HEADER_BUILD_ID: u32 = 67;
    pub const FINISHED_ROUND: u32 = 68;
    pub const AUXTRACE: u32 = 71;
    pub const HEADER_FEATURE: u32 = 80;
    pub const COMPRESSED: u32 = 81;
    pub const COMPRESSED2: u32 = 83;
}

/// The feature sections after the data, by their bit in the header.
pub mod feature {
    pub const BUILD_ID: u32 = 2;
    pub const HOSTNAME: u32 = 3;
    pub const OSRELEASE: u32 = 4;
    pub const ARCH: u32 = 6;
    pub const CMDLINE: u32 = 11;
}

const MISC_CPUMODE_MASK: u16 = 7;
const MISC_KERNEL: u16 = 1;
/// On a `COMM`: the name is the one an `exec` gave. On a switch: out.
const MISC_BIT13: u16 = 1 << 13;
/// On an `MMAP2`: the build id is in the record. On a switch out: the
/// thread was preempted, it did not block.
const MISC_BIT14: u16 = 1 << 14;
/// On a build-id record: its size is in the record.
const MISC_BUILD_ID_SIZE: u16 = 1 << 15;

const ATTR_FREQ: u64 = 1 << 10;
const ATTR_SAMPLE_ID_ALL: u64 = 1 << 18;
const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;

const READ_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const READ_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
const READ_FORMAT_ID: u64 = 1 << 2;
const READ_FORMAT_GROUP: u64 = 1 << 3;
const READ_FORMAT_LOST: u64 = 1 << 4;

const BRANCH_HW_INDEX: u64 = 1 << 17;
const BRANCH_COUNTERS: u64 = 1 << 19;

/// One event `perf record` opened: what its samples carry and how often
/// they were taken.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub kind: u32,
    pub config: u64,
    /// `sample_period`, or `sample_freq` when [`Attr::is_freq`].
    pub sample_period: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    /// The ids its records are stamped with: one per CPU or thread.
    pub ids: Vec<u64>,
}

impl Attr {
    /// A `perf_event_attr` of any version: fields past its size are zero.
    fn parse(bytes: &[u8]) -> Option<Attr> {
        if bytes.len() < 48 {
            return None;
        }
        let u64_at = |at: usize| bytes.get(at..at + 8).map_or(0, le_u64);
        Some(Attr {
            kind: le_u32(&bytes[0..4]),
            config: u64_at(8),
            sample_period: u64_at(16),
            sample_type: u64_at(24),
            read_format: u64_at(32),
            flags: u64_at(40),
            branch_sample_type: u64_at(72),
            sample_regs_user: u64_at(80),
            ids: Vec::new(),
        })
    }

    /// `sample_period` is a frequency in hertz, not a count of events.
    pub fn is_freq(&self) -> bool {
        self.flags & ATTR_FREQ != 0
    }

    fn sample_id_all(&self) -> bool {
        self.flags & ATTR_SAMPLE_ID_ALL != 0
    }

    /// The time between two samples, where the attribute says it: a
    /// frequency, or the period of one of the two clock events (which
    /// count nanoseconds). An event sampled every N cycles or cache misses
    /// has no period in time.
    pub fn period_ns(&self) -> Option<u64> {
        if self.sample_period == 0 {
            return None;
        }
        if self.is_freq() {
            return Some((1_000_000_000 / self.sample_period).max(1));
        }
        let clock = self.kind == PERF_TYPE_SOFTWARE
            && (self.config == PERF_COUNT_SW_CPU_CLOCK || self.config == PERF_COUNT_SW_TASK_CLOCK);
        clock.then_some(self.sample_period)
    }

    /// Bytes of the `sample_id` block that ends every record that is not a
    /// sample, when the attribute asks for one.
    fn trailer_len(&self) -> usize {
        if !self.sample_id_all() {
            return 0;
        }
        use sample_type::*;
        [TID, TIME, ID, STREAM_ID, CPU, IDENTIFIER]
            .iter()
            .filter(|bit| self.sample_type & **bit != 0)
            .count()
            * 8
    }
}

/// Who and when, as far as a record says: from a sample's own fields, or
/// from the `sample_id` block at the end of any other record. A field the
/// record does not carry is zero (`cpu`: `None`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleId {
    pub pid: u32,
    pub tid: u32,
    pub time: u64,
    pub cpu: Option<u32>,
    pub id: u64,
}

/// A file mapped into a process (`MMAP`, `MMAP2`), or a piece of the
/// kernel: its text, a module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mmap<'a> {
    pub pid: u32,
    pub tid: u32,
    pub start: u64,
    pub len: u64,
    pub pgoff: u64,
    pub filename: &'a [u8],
    /// From an `MMAP2` written by a `perf` that stamps them.
    pub build_id: &'a [u8],
    /// The mapping is the kernel's, not a process's.
    pub kernel: bool,
    pub at: SampleId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comm<'a> {
    pub pid: u32,
    pub tid: u32,
    pub comm: &'a [u8],
    /// The name an `exec` gave: the process is a new program from here.
    pub exec: bool,
    pub at: SampleId,
}

/// `FORK` and `EXIT`: a thread and the one that made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Task {
    pub pid: u32,
    pub ppid: u32,
    pub tid: u32,
    pub ptid: u32,
    pub time: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample<'a> {
    /// Index into [`Reader::attrs`] of the event that took it.
    pub attr: usize,
    pub ip: Option<u64>,
    pub at: SampleId,
    pub has_time: bool,
    pub period: u64,
    /// The sampled pc is in the kernel.
    pub kernel: bool,
    /// `nr` little-endian addresses, innermost first, with the kernel's
    /// context markers between them; see [`callchain`].
    pub callchain: &'a [u8],
    pub raw: &'a [u8],
    /// The ABI the registers were sampled under (0: none were), and the
    /// registers of `sample_regs_user` in mask order.
    pub regs_abi: u64,
    pub regs: &'a [u8],
    /// The bytes of the user stack that were copied, from the stack
    /// pointer up.
    pub stack: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switch {
    /// The thread left the CPU; false when it got on.
    pub out: bool,
    /// Leaving, it was still runnable: something took the CPU from it.
    pub preempt: bool,
    pub at: SampleId,
}

/// One entry of the build-id table: the id `perf` read from a file it saw
/// mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildId<'a> {
    pub filename: &'a [u8],
    pub id: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record<'a> {
    Mmap(Mmap<'a>),
    Comm(Comm<'a>),
    Fork(Task),
    Exit(Task),
    Sample(Sample<'a>),
    Switch(Switch),
    /// Records the kernel could not write: a ring was full.
    Lost(u64),
    BuildId(BuildId<'a>),
    /// Everything written before this is in the file: records up to the
    /// previous round's latest time can be put in order.
    FinishedRound,
    /// A piece of the zstd stream the records were written through.
    Compressed(&'a [u8]),
    /// A feature section, in a file that was piped: see [`feature`].
    Feature(u32, &'a [u8]),
    /// A record of a kind nothing here reads.
    Other(u32),
}

/// What one [`Reader::step`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item<'a> {
    /// Nothing can be made of the bytes held until more of the file is.
    NeedBytes,
    /// Bytes were consumed and there is nothing to say about them.
    Nothing,
    Record(Record<'a>),
    /// The file has been read to the end of what it holds.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Header,
    /// Between the header and the data: the ids and the attributes.
    Attrs,
    Data,
    /// After the data: the feature sections, read when the file has ended.
    Features,
    Done,
}

/// Reads a `perf.data` from its first byte, as the bytes arrive.
pub struct Reader {
    phase: Phase,
    /// Offset in the file of the first byte not consumed yet.
    offset: u64,
    attrs: Vec<Attr>,
    by_id: HashMap<u64, usize>,
    /// Where the attribute table is, and the size of one entry.
    attrs_at: (u64, u64),
    attr_size: u64,
    data_at: u64,
    /// `None`: the header gives no size (the recording was killed) or the
    /// file is a pipe's, and the records run to the end of the file.
    data_end: Option<u64>,
    features: [u64; 4],
    /// The feature sections not handed out yet: (bit, start, end) in the
    /// bytes held.
    pending_features: Option<Vec<(u32, usize, usize)>>,
    /// Bytes to pass over before the next record: a payload that follows
    /// its record rather than being part of it.
    skip: u64,
    /// A record has been read: the data section is what it claims to be.
    read_any: bool,
}

impl Default for Reader {
    fn default() -> Self {
        Reader {
            phase: Phase::Header,
            offset: 0,
            attrs: Vec::new(),
            by_id: HashMap::new(),
            attrs_at: (0, 0),
            attr_size: 0,
            data_at: 0,
            data_end: None,
            features: [0; 4],
            pending_features: None,
            skip: 0,
            read_any: false,
        }
    }
}

impl Reader {
    /// The events of the recording. Complete once the first record has
    /// been read from a file; a pipe's arrive as records, before the
    /// samples they describe.
    pub fn attrs(&self) -> &[Attr] {
        &self.attrs
    }

    /// Reads from `buf`, the bytes of the file that have not been consumed
    /// yet, and returns how many it consumed with what they were. `ended`:
    /// no more bytes will follow those of `buf`.
    pub fn step<'a>(&mut self, buf: &'a [u8], ended: bool) -> Result<(usize, Item<'a>), String> {
        let (used, item) = self.step_inner(buf, ended)?;
        self.offset += used as u64;
        Ok((used, item))
    }

    fn step_inner<'a>(&mut self, buf: &'a [u8], ended: bool) -> Result<(usize, Item<'a>), String> {
        match self.phase {
            Phase::Header => self.read_header(buf, ended),
            Phase::Attrs => self.read_attrs(buf, ended),
            Phase::Data => self.read_data(buf, ended),
            Phase::Features => self.read_features(buf, ended),
            Phase::Done => Ok((buf.len(), Item::End)),
        }
    }

    fn read_header<'a>(&mut self, buf: &'a [u8], ended: bool) -> Result<(usize, Item<'a>), String> {
        if buf.len() < PIPE_HEADER_LEN {
            return if ended {
                Err("perf.data: the file ends inside its header".into())
            } else {
                Ok((0, Item::NeedBytes))
            };
        }
        if buf.starts_with(&MAGIC_SWAPPED) {
            return Err("perf.data: recorded on a big-endian machine, which is not read".into());
        }
        if !buf.starts_with(&MAGIC) {
            return Err("perf.data: not a perf.data file (no PERFILE2 at its start)".into());
        }
        let size = le_u64(&buf[8..16]);
        if size == PIPE_HEADER_LEN as u64 {
            // `perf record -o -`: no tables, the attributes are records.
            self.phase = Phase::Data;
            return Ok((PIPE_HEADER_LEN, Item::Nothing));
        }
        if size < FILE_HEADER_LEN as u64 {
            return Err(format!("perf.data: a header of {size} bytes"));
        }
        if buf.len() < FILE_HEADER_LEN {
            return if ended {
                Err("perf.data: the file ends inside its header".into())
            } else {
                Ok((0, Item::NeedBytes))
            };
        }
        let field = |at: usize| le_u64(&buf[at..at + 8]);
        self.attr_size = field(16);
        self.attrs_at = (field(24), field(32));
        self.data_at = field(40);
        let data_size = field(48);
        for (word, at) in self.features.iter_mut().zip([72, 80, 88, 96]) {
            *word = field(at);
        }
        if self.attr_size < 48 + SECTION_LEN as u64 || self.attr_size > 4096 {
            return Err(format!(
                "perf.data: attributes of {} bytes each",
                self.attr_size
            ));
        }
        let (attrs_at, attrs_size) = self.attrs_at;
        let attrs_end = attrs_at.checked_add(attrs_size);
        if self.data_at < size || attrs_at < size || attrs_end.is_none_or(|end| end > self.data_at)
        {
            // Every writer puts the tables first. One that did not would
            // need the whole file held to find them.
            return Err("perf.data: the attributes are not between the header and the data".into());
        }
        if self.data_at - size > SIDE_SECTION_LIMIT {
            return Err("perf.data: the tables before the data are too large".into());
        }
        // A recording that was killed never went back to write the size.
        self.data_end = (data_size != 0).then(|| self.data_at.saturating_add(data_size));
        self.phase = Phase::Attrs;
        Ok((FILE_HEADER_LEN, Item::Nothing))
    }

    /// The bytes from the header to the data: the ids, then one entry per
    /// event with the attribute and where its ids are.
    fn read_attrs<'a>(&mut self, buf: &'a [u8], ended: bool) -> Result<(usize, Item<'a>), String> {
        let len = (self.data_at - self.offset) as usize;
        if buf.len() < len {
            return if ended {
                Err("perf.data: the file ends before its data".into())
            } else {
                Ok((0, Item::NeedBytes))
            };
        }
        // Offsets in the file, as offsets in `buf`.
        let base = self.offset;
        let slice = |at: u64, size: u64| -> Option<&[u8]> {
            let start = usize::try_from(at.checked_sub(base)?).ok()?;
            let end = start.checked_add(usize::try_from(size).ok()?)?;
            buf.get(start..end.min(len))
        };
        let (at, size) = self.attrs_at;
        let table = slice(at, size).ok_or("perf.data: the attribute table is outside the file")?;
        for entry in table.chunks_exact(self.attr_size as usize) {
            let (attr, ids) = entry.split_at(entry.len() - SECTION_LEN);
            let Some(mut attr) = Attr::parse(attr) else {
                continue;
            };
            if let Some(ids) = slice(le_u64(&ids[0..8]), le_u64(&ids[8..16])) {
                attr.ids = ids.chunks_exact(8).map(le_u64).collect();
            }
            self.add_attr(attr);
        }
        if self.attrs.is_empty() {
            return Err("perf.data: no event attributes".into());
        }
        self.phase = Phase::Data;
        Ok((len, Item::Nothing))
    }

    fn add_attr(&mut self, attr: Attr) {
        let index = self.attrs.len();
        for id in &attr.ids {
            self.by_id.insert(*id, index);
        }
        self.attrs.push(attr);
    }

    fn read_data<'a>(&mut self, buf: &'a [u8], ended: bool) -> Result<(usize, Item<'a>), String> {
        if self.skip > 0 {
            let n = self.skip.min(buf.len() as u64) as usize;
            self.skip -= n as u64;
            return Ok(match (n, ended) {
                (0, true) => self.finish(buf),
                (0, false) => (0, Item::NeedBytes),
                _ => (n, Item::Nothing),
            });
        }
        let left = self.data_end.map(|end| end.saturating_sub(self.offset));
        if left == Some(0) {
            self.phase = Phase::Features;
            return Ok((0, Item::Nothing));
        }
        if buf.len() < RECORD_HEADER_LEN {
            // A file cut short is read as far as it goes.
            return Ok(if ended {
                self.finish(buf)
            } else {
                (0, Item::NeedBytes)
            });
        }
        let kind = le_u32(&buf[0..4]);
        let size = le_u16(&buf[6..8]) as usize;
        if size < RECORD_HEADER_LEN {
            if self.data_end.is_none() && self.read_any {
                // No size says where the records stop, and these bytes are
                // not one: whatever follows the data of a recording that
                // never wrote its sizes.
                return Ok(self.finish(buf));
            }
            return Err(format!(
                "perf.data: a record of {size} bytes at offset {}",
                self.offset
            ));
        }
        if left.is_some_and(|left| (size as u64) > left) {
            // The record runs past the end of the data: the header's size
            // is wrong, or these are not records.
            return Err(format!(
                "perf.data: a record past the end of the data at offset {}",
                self.offset
            ));
        }
        if buf.len() < size {
            return Ok(if ended {
                self.finish(buf)
            } else {
                (0, Item::NeedBytes)
            });
        }
        let bytes = &buf[..size];
        // Two records are followed by a payload their size leaves out.
        match kind {
            record::AUXTRACE if size >= 16 => self.skip = le_u64(&bytes[8..16]),
            record::HEADER_TRACING_DATA if size >= 12 => {
                self.skip = u64::from(le_u32(&bytes[8..12])).next_multiple_of(8)
            }
            _ => {}
        }
        self.read_any = true;
        Ok((size, Item::Record(self.record(bytes))))
    }

    fn finish<'a>(&mut self, buf: &'a [u8]) -> (usize, Item<'a>) {
        self.phase = Phase::Done;
        (buf.len(), Item::End)
    }

    /// After the data: a table of sections, one per feature bit set in the
    /// header, then what they point at. Small, and wanted whole.
    fn read_features<'a>(
        &mut self,
        buf: &'a [u8],
        ended: bool,
    ) -> Result<(usize, Item<'a>), String> {
        if !ended {
            if buf.len() as u64 > SIDE_SECTION_LIMIT {
                return Ok(self.finish(buf));
            }
            return Ok((0, Item::NeedBytes));
        }
        let (base, features) = (self.offset, self.features);
        let pending = self.pending_features.get_or_insert_with(|| {
            let bits =
                (0..256u32).filter(|bit| features[(bit / 64) as usize] >> (bit % 64) & 1 != 0);
            let mut sections: Vec<(u32, usize, usize)> = bits
                .zip(buf.chunks_exact(SECTION_LEN))
                .filter_map(|(bit, section)| {
                    let start = usize::try_from(le_u64(&section[0..8]).checked_sub(base)?).ok()?;
                    let end = start.checked_add(usize::try_from(le_u64(&section[8..16])).ok()?)?;
                    (end <= buf.len()).then_some((bit, start, end))
                })
                .collect();
            sections.reverse();
            sections
        });
        match pending.pop() {
            Some((bit, start, end)) => {
                Ok((0, Item::Record(Record::Feature(bit, &buf[start..end]))))
            }
            None => Ok(self.finish(buf)),
        }
    }

    /// One record, header included. Also what a decompressed stream of
    /// records is read with.
    pub fn record<'a>(&mut self, bytes: &'a [u8]) -> Record<'a> {
        let kind = le_u32(&bytes[0..4]);
        let misc = le_u16(&bytes[4..6]);
        let body = &bytes[RECORD_HEADER_LEN..];
        let parsed = match kind {
            record::SAMPLE => self.sample(body, misc).map(Record::Sample),
            record::MMAP | record::MMAP2 => self
                .mmap(body, misc, kind == record::MMAP2)
                .map(Record::Mmap),
            record::COMM => self.comm(body, misc).map(Record::Comm),
            record::FORK | record::EXIT => task(body).map(if kind == record::FORK {
                Record::Fork
            } else {
                Record::Exit
            }),
            record::SWITCH | record::SWITCH_CPU_WIDE => Some(Record::Switch(Switch {
                out: misc & MISC_BIT13 != 0,
                preempt: misc & MISC_BIT14 != 0,
                at: self.trailer(body),
            })),
            record::LOST => body.get(8..16).map(|n| Record::Lost(le_u64(n))),
            record::LOST_SAMPLES => body.get(0..8).map(|n| Record::Lost(le_u64(n))),
            record::HEADER_ATTR => {
                self.attr_record(body);
                None
            }
            record::HEADER_BUILD_ID => build_id(bytes).map(Record::BuildId),
            record::FINISHED_ROUND => Some(Record::FinishedRound),
            record::COMPRESSED => Some(Record::Compressed(body)),
            record::COMPRESSED2 => body
                .get(0..8)
                .and_then(|n| body.get(8..8usize.checked_add(usize::try_from(le_u64(n)).ok()?)?))
                .map(Record::Compressed),
            record::HEADER_FEATURE => body
                .get(0..8)
                .map(|id| Record::Feature(le_u64(id) as u32, &body[8..])),
            _ => None,
        };
        parsed.unwrap_or(Record::Other(kind))
    }

    /// A pipe's attribute: the `perf_event_attr`, then its ids.
    fn attr_record(&mut self, body: &[u8]) {
        let Some(size) = body.get(4..8).map(|s| le_u32(s) as usize) else {
            return;
        };
        let (Some(attr), Some(ids)) = (body.get(..size), body.get(size..)) else {
            return;
        };
        if let Some(mut attr) = Attr::parse(attr) {
            attr.ids = ids.chunks_exact(8).map(le_u64).collect();
            self.add_attr(attr);
        }
    }

    /// The attribute a record that is not a sample was written under: the
    /// one its trailing id names, when every event stamps one.
    fn trailer_attr(&self, body: &[u8]) -> Option<&Attr> {
        let first = self.attrs.first()?;
        if self.attrs.len() == 1
            || !self
                .attrs
                .iter()
                .all(|a| a.sample_id_all() && a.sample_type & sample_type::IDENTIFIER != 0)
        {
            return Some(first);
        }
        let id = body.len().checked_sub(8).map(|at| le_u64(&body[at..]))?;
        Some(
            self.by_id
                .get(&id)
                .map_or(first, |index| &self.attrs[*index]),
        )
    }

    /// The `sample_id` block at the end of `body`.
    fn trailer(&self, body: &[u8]) -> SampleId {
        let mut at = SampleId::default();
        let Some(attr) = self.trailer_attr(body) else {
            return at;
        };
        let Some(start) = body.len().checked_sub(attr.trailer_len()) else {
            return at;
        };
        if attr.trailer_len() == 0 {
            return at;
        }
        let mut cur = Cursor {
            bytes: &body[start..],
        };
        use sample_type::*;
        let has = |bit: u64| attr.sample_type & bit != 0;
        if has(TID) {
            at.pid = cur.u32().unwrap_or(0);
            at.tid = cur.u32().unwrap_or(0);
        }
        if has(TIME) {
            at.time = cur.u64().unwrap_or(0);
        }
        if has(ID) {
            at.id = cur.u64().unwrap_or(0);
        }
        if has(STREAM_ID) {
            cur.u64();
        }
        if has(CPU) {
            at.cpu = cur.u32();
            cur.u32();
        }
        if has(IDENTIFIER) {
            at.id = cur.u64().unwrap_or(0);
        }
        at
    }

    /// Bytes of `body` before its `sample_id` block.
    fn before_trailer<'a>(&self, body: &'a [u8]) -> &'a [u8] {
        let trailer = self.trailer_attr(body).map_or(0, Attr::trailer_len);
        &body[..body.len().saturating_sub(trailer)]
    }

    fn mmap<'a>(&self, body: &'a [u8], misc: u16, v2: bool) -> Option<Mmap<'a>> {
        let mut cur = Cursor {
            bytes: self.before_trailer(body),
        };
        let (pid, tid) = (cur.u32()?, cur.u32()?);
        let (start, len, pgoff) = (cur.u64()?, cur.u64()?, cur.u64()?);
        let mut build_id: &[u8] = &[];
        if v2 {
            // Device and inode, or (a newer perf) the build id in their
            // place; then protection and flags.
            let ident = cur.take(24)?;
            if misc & MISC_BIT14 != 0 {
                build_id = ident.get(4..4 + usize::from(ident[0]).min(20))?;
            }
            cur.take(8)?;
        }
        Some(Mmap {
            pid,
            tid,
            start,
            len,
            pgoff,
            filename: c_string(cur.bytes),
            build_id,
            kernel: misc & MISC_CPUMODE_MASK == MISC_KERNEL || pid == u32::MAX,
            at: self.trailer(body),
        })
    }

    fn comm<'a>(&self, body: &'a [u8], misc: u16) -> Option<Comm<'a>> {
        let mut cur = Cursor {
            bytes: self.before_trailer(body),
        };
        let (pid, tid) = (cur.u32()?, cur.u32()?);
        Some(Comm {
            pid,
            tid,
            comm: c_string(cur.bytes),
            exec: misc & MISC_BIT13 != 0,
            at: self.trailer(body),
        })
    }

    /// The attribute a sample was taken under. One event, or several that
    /// lay their samples out alike, need no lookup; otherwise the sample's
    /// id says, and `perf` only records together events whose id is at one
    /// place.
    fn sample_attr(&self, body: &[u8]) -> Option<usize> {
        let first = self.attrs.first()?;
        if self.attrs.len() == 1 {
            return Some(0);
        }
        use sample_type::*;
        let id_at = if first.sample_type & IDENTIFIER != 0 {
            0
        } else if first.sample_type & ID != 0 {
            [IP, TID, TIME, ADDR]
                .iter()
                .filter(|bit| first.sample_type & **bit != 0)
                .count()
                * 8
        } else {
            return Some(0);
        };
        let id = le_u64(body.get(id_at..id_at + 8)?);
        Some(self.by_id.get(&id).copied().unwrap_or(0))
    }

    fn sample<'a>(&self, body: &'a [u8], misc: u16) -> Option<Sample<'a>> {
        use sample_type::*;
        let index = self.sample_attr(body)?;
        let attr = &self.attrs[index];
        let has = |bit: u64| attr.sample_type & bit != 0;
        let mut cur = Cursor { bytes: body };
        let mut sample = Sample {
            attr: index,
            ip: None,
            at: SampleId::default(),
            has_time: has(TIME),
            period: 0,
            kernel: misc & MISC_CPUMODE_MASK == MISC_KERNEL,
            callchain: &[],
            raw: &[],
            regs_abi: 0,
            regs: &[],
            stack: &[],
        };
        if has(IDENTIFIER) {
            sample.at.id = cur.u64()?;
        }
        if has(IP) {
            sample.ip = Some(cur.u64()?);
        }
        if has(TID) {
            sample.at.pid = cur.u32()?;
            sample.at.tid = cur.u32()?;
        }
        if has(TIME) {
            sample.at.time = cur.u64()?;
        }
        if has(ADDR) {
            cur.u64()?;
        }
        if has(ID) {
            sample.at.id = cur.u64()?;
        }
        if has(STREAM_ID) {
            cur.u64()?;
        }
        if has(CPU) {
            sample.at.cpu = Some(cur.u32()?);
            cur.u32()?;
        }
        if has(PERIOD) {
            sample.period = cur.u64()?;
        }
        if has(READ) {
            let format = attr.read_format;
            let optional = |bit: u64| usize::from(format & bit != 0);
            let times =
                optional(READ_FORMAT_TOTAL_TIME_ENABLED) + optional(READ_FORMAT_TOTAL_TIME_RUNNING);
            let per_value = 1 + optional(READ_FORMAT_ID) + optional(READ_FORMAT_LOST);
            let words = if format & READ_FORMAT_GROUP != 0 {
                let nr = usize::try_from(cur.u64()?).ok()?;
                times.checked_add(nr.checked_mul(per_value)?)?
            } else {
                times + per_value
            };
            cur.take(words.checked_mul(8)?)?;
        }
        if has(CALLCHAIN) {
            let nr = usize::try_from(cur.u64()?).ok()?;
            sample.callchain = cur.take(nr.checked_mul(8)?)?;
        }
        if has(RAW) {
            let size = cur.u32()? as usize;
            sample.raw = cur.take(size)?;
        }
        if has(BRANCH_STACK) {
            let nr = usize::try_from(cur.u64()?).ok()?;
            if attr.branch_sample_type & BRANCH_HW_INDEX != 0 {
                cur.u64()?;
            }
            // from, to, flags; and a word of counters per branch when asked.
            let per_branch = if attr.branch_sample_type & BRANCH_COUNTERS != 0 {
                32
            } else {
                24
            };
            cur.take(nr.checked_mul(per_branch)?)?;
        }
        if has(REGS_USER) {
            sample.regs_abi = cur.u64()?;
            if sample.regs_abi != 0 {
                sample.regs = cur.take(attr.sample_regs_user.count_ones() as usize * 8)?;
            }
        }
        if has(STACK_USER) {
            let size = usize::try_from(cur.u64()?).ok()?;
            if size != 0 {
                let data = cur.take(size)?;
                let copied = usize::try_from(cur.u64()?).ok()?;
                sample.stack = &data[..copied.min(size)];
            }
        }
        Some(sample)
    }
}

fn task(body: &[u8]) -> Option<Task> {
    let mut cur = Cursor { bytes: body };
    Some(Task {
        pid: cur.u32()?,
        ppid: cur.u32()?,
        tid: cur.u32()?,
        ptid: cur.u32()?,
        time: cur.u64()?,
    })
}

/// One `perf_record_header_build_id`, header included: the pid, the id in
/// a 24-byte field (20 bytes of it unless the record says how many), the
/// file name.
fn build_id(bytes: &[u8]) -> Option<BuildId<'_>> {
    let misc = le_u16(bytes.get(4..6)?);
    let field = bytes.get(12..36)?;
    let len = if misc & MISC_BUILD_ID_SIZE != 0 {
        usize::from(field[20]).min(20)
    } else {
        20
    };
    Some(BuildId {
        filename: c_string(bytes.get(36..)?),
        id: &field[..len],
    })
}

/// The entries of the build-id feature section ([`feature::BUILD_ID`]):
/// build-id records one after the other.
pub fn build_ids(mut section: &[u8]) -> Vec<BuildId<'_>> {
    let mut out = Vec::new();
    while section.len() >= RECORD_HEADER_LEN {
        let size = le_u16(&section[6..8]) as usize;
        if size < RECORD_HEADER_LEN || size > section.len() {
            break;
        }
        out.extend(build_id(&section[..size]));
        section = &section[size..];
    }
    out
}

/// A feature section that is one string ([`feature::HOSTNAME`],
/// [`feature::OSRELEASE`], [`feature::ARCH`]): a length, then the text
/// padded with zeros.
pub fn feature_string(section: &[u8]) -> Option<String> {
    let len = le_u32(section.get(0..4)?) as usize;
    let text = c_string(section.get(4..4usize.checked_add(len)?)?);
    Some(String::from_utf8_lossy(text).into_owned())
}

/// [`feature::CMDLINE`]: a count, then that many strings.
pub fn feature_strings(section: &[u8]) -> Vec<String> {
    let Some(count) = section.get(0..4).map(le_u32) else {
        return Vec::new();
    };
    let mut rest = &section[4..];
    let mut out = Vec::new();
    for _ in 0..count {
        let Some(len) = rest.get(0..4).map(|n| le_u32(n) as usize) else {
            break;
        };
        let Some(text) = feature_string(rest) else {
            break;
        };
        out.push(text);
        rest = &rest[4 + len..];
    }
    out
}

/// A frame of a sampled call chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The address to name the frame by: see [`callchain`].
    pub address: u64,
    pub kernel: bool,
}

/// The frames of a sample, innermost first: its call chain without the
/// context markers, or its sampled pc alone when it has no chain.
///
/// A chain is `nr` addresses, the innermost first, and between them the
/// kernel puts markers (`PERF_CONTEXT_KERNEL`, `PERF_CONTEXT_USER`, ...)
/// that say which side the addresses after them are on: very large
/// numbers, in a range no code is mapped in.
///
/// The first address on each side is a pc: where the sample landed, or
/// where the program was when it entered the kernel. The ones after it
/// are return addresses, and a return address is the instruction *after*
/// a call, which can be the next line or (after a call that does not
/// return) the next function. Those are given minus one, the address
/// inside the call, which is what a symbol table is to be asked.
pub fn callchain(sample: &Sample<'_>) -> Vec<Frame> {
    const CONTEXT_MAX: u64 = -4095i64 as u64;
    const CONTEXT_KERNEL: u64 = -128i64 as u64;
    const CONTEXT_USER: u64 = -512i64 as u64;
    let mut frames = Vec::with_capacity(sample.callchain.len() / 8);
    let mut kernel = sample.kernel;
    let mut pc = true;
    for address in sample.callchain.chunks_exact(8).map(le_u64) {
        if address >= CONTEXT_MAX {
            kernel = match address {
                CONTEXT_KERNEL => true,
                CONTEXT_USER => false,
                // A hypervisor's or a guest's: nothing of this machine.
                _ => kernel,
            };
            pc = true;
            continue;
        }
        frames.push(Frame {
            address: if pc {
                address
            } else {
                address.saturating_sub(1)
            },
            kernel,
        });
        pc = false;
    }
    if frames.is_empty() {
        frames.extend(sample.ip.map(|address| Frame {
            address,
            kernel: sample.kernel,
        }));
    }
    frames
}

/// The bytes up to the first zero: a name in a fixed or padded field.
fn c_string(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())]
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

pub(crate) fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Reads fields off the front of a record; `None` past its end.
struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.bytes.split_at_checked(n)?;
        self.bytes = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(le_u32)
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(le_u64)
    }
}
