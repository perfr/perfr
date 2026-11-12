// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The scheduling tracepoint payloads.
//!
//! A tracepoint sample's `RAW` payload is the tracepoint's own fields, laid
//! out exactly as its `format` file in tracefs describes. The offsets are
//! read from that file when a capture starts (`*Layout::from_format`); the
//! compiled-in `DEFAULT` layouts are the fallback for a format file that
//! cannot be read, and match the kernels measured so far.
//!
//! Reading is by explicit offset rather than by casting a packed struct, so
//! nothing here needs `unsafe` and a short payload returns `None` instead of
//! reading past the end.

/// Every tracepoint payload starts with these 8 bytes.
pub const COMMON_LEN: usize = 8;

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn i64_at(bytes: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

/// A fixed-width `char[16]` comm field, trimmed at the first NUL. Lent from
/// the payload: these are parsed for every context switch on the machine,
/// and nothing downstream has yet wanted a name badly enough to own one.
fn comm_at(bytes: &[u8], offset: usize) -> Option<&[u8]> {
    let raw = bytes.get(offset..offset + 16)?;
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    Some(&raw[..end])
}

/// The `offset` and `size` of each named field in a tracepoint's tracefs
/// `format` file. The offsets are what move between kernels (v5.14 dropped
/// `sched_wakeup`'s `success` field and shifted everything after it), so a
/// service that means to run cross-kernel parses this at capture start
/// rather than trusting a compiled-in number.
///
/// Lines look like `\tfield:char prev_comm[16];\toffset:8;\tsize:16;\tsigned:1;`.
/// The field name is the last identifier of the declaration, with any `[N]`
/// stripped.
pub fn format_field_offsets(text: &str) -> std::collections::HashMap<String, (usize, usize)> {
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("field:") else { continue };
        let mut decl = None;
        let mut offset = None;
        let mut size = None;
        for part in rest.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("offset:") {
                offset = v.trim().parse::<usize>().ok();
            } else if let Some(v) = part.strip_prefix("size:") {
                size = v.trim().parse::<usize>().ok();
            } else if !part.is_empty() && decl.is_none() {
                decl = Some(part);
            }
        }
        let (Some(decl), Some(offset), Some(size)) = (decl, offset, size) else { continue };
        // The declaration is `<type...> <name>` with an optional `[N]`; the
        // name is the last whitespace-separated token, cut at `[`.
        let name = decl.rsplit(|c: char| c.is_whitespace() || c == '*').next().unwrap_or("");
        let name = name.split('[').next().unwrap_or(name).trim();
        if !name.is_empty() {
            out.insert(name.to_string(), (offset, size));
        }
    }
    out
}

/// `sched:sched_switch`. The comms are the bytes the kernel wrote, which
/// need not be UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedSwitch<'a> {
    pub prev_comm: &'a [u8],
    pub prev_tid: i32,
    /// The task state bits; see [`thread_state_from_bits`].
    pub prev_state: i64,
    pub next_comm: &'a [u8],
    pub next_tid: i32,
}

/// Where `sched_switch`'s fields sit, from its `format` file or the
/// compiled-in default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedSwitchLayout {
    pub prev_comm: usize,
    pub prev_pid: usize,
    pub prev_state: usize,
    pub next_comm: usize,
    pub next_pid: usize,
}

impl SchedSwitchLayout {
    /// The layout the C++ hard-codes, matching every kernel measured so far.
    pub const DEFAULT: SchedSwitchLayout = SchedSwitchLayout {
        prev_comm: 8,
        prev_pid: 24,
        prev_state: 32,
        next_comm: 40,
        next_pid: 56,
    };

    /// From the tracefs `format` text; `None` if a field it needs is absent.
    pub fn from_format(text: &str) -> Option<SchedSwitchLayout> {
        let f = format_field_offsets(text);
        Some(SchedSwitchLayout {
            prev_comm: f.get("prev_comm")?.0,
            prev_pid: f.get("prev_pid")?.0,
            prev_state: f.get("prev_state")?.0,
            next_comm: f.get("next_comm")?.0,
            next_pid: f.get("next_pid")?.0,
        })
    }
}

impl<'a> SchedSwitch<'a> {
    /// Layout: common(8), prev_comm[16], prev_pid(4), prev_prio(4),
    /// prev_state(8), next_comm[16], next_pid(4), next_prio(4), pad(4) = 68.
    pub const LEN: usize = 68;

    /// Parse with the compiled-in default layout.
    pub fn parse(payload: &'a [u8]) -> Option<SchedSwitch<'a>> {
        SchedSwitch::parse_with(payload, &SchedSwitchLayout::DEFAULT)
    }

    pub fn parse_with(payload: &'a [u8], layout: &SchedSwitchLayout) -> Option<SchedSwitch<'a>> {
        Some(SchedSwitch {
            prev_comm: comm_at(payload, layout.prev_comm)?,
            prev_tid: i32_at(payload, layout.prev_pid)?,
            prev_state: i64_at(payload, layout.prev_state)?,
            next_comm: comm_at(payload, layout.next_comm)?,
            next_tid: i32_at(payload, layout.next_pid)?,
        })
    }
}

/// `sched:sched_wakeup`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedWakeup<'a> {
    pub comm: &'a [u8],
    pub tid: i32,
}

/// Where `sched_wakeup`'s fields sit. `comm` then `pid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedWakeupLayout {
    pub comm: usize,
    pub pid: usize,
}

impl SchedWakeupLayout {
    pub const DEFAULT: SchedWakeupLayout = SchedWakeupLayout { comm: 8, pid: 24 };

    pub fn from_format(text: &str) -> Option<SchedWakeupLayout> {
        let f = format_field_offsets(text);
        Some(SchedWakeupLayout { comm: f.get("comm")?.0, pid: f.get("pid")?.0 })
    }
}

impl<'a> SchedWakeup<'a> {
    /// Only the fixed head is read. Kernel v5.14 removed the `success` field
    /// (torvalds/linux 58b9987de86c) and the padding with it, so everything
    /// after `prio` is version-dependent and none of it is needed here.
    pub const FIXED_LEN: usize = 32;

    pub fn parse(payload: &'a [u8]) -> Option<SchedWakeup<'a>> {
        SchedWakeup::parse_with(payload, &SchedWakeupLayout::DEFAULT)
    }

    pub fn parse_with(payload: &'a [u8], layout: &SchedWakeupLayout) -> Option<SchedWakeup<'a>> {
        Some(SchedWakeup { comm: comm_at(payload, layout.comm)?, tid: i32_at(payload, layout.pid)? })
    }
}

/// `task:task_newtask`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskNewtask<'a> {
    pub tid: i32,
    pub comm: &'a [u8],
    /// The `clone(2)` flags; `CLONE_THREAD` set means a thread of the parent's
    /// process, clear means a new process. Zero when the record is too short
    /// to carry them.
    pub clone_flags: u64,
}

/// `CLONE_THREAD` from `<linux/sched.h>`.
pub const CLONE_THREAD: u64 = 0x0001_0000;

/// Where `task_newtask`'s fields sit: `pid`, `comm`, `clone_flags`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskNewtaskLayout {
    pub pid: usize,
    pub comm: usize,
    pub clone_flags: usize,
}

impl TaskNewtaskLayout {
    /// `clone_flags` is an 8-byte field and the kernel's struct is not
    /// packed, so it sits at 32, not directly after `comm` at 28. Read at
    /// 28, `CLONE_THREAD` is tested against padding and a new thread is
    /// taken for a new process whenever the format file cannot be parsed.
    pub const DEFAULT: TaskNewtaskLayout = TaskNewtaskLayout { pid: 8, comm: 12, clone_flags: 32 };

    pub fn from_format(text: &str) -> Option<TaskNewtaskLayout> {
        let f = format_field_offsets(text);
        Some(TaskNewtaskLayout {
            pid: f.get("pid")?.0,
            comm: f.get("comm")?.0,
            clone_flags: f.get("clone_flags")?.0,
        })
    }
}

impl<'a> TaskNewtask<'a> {
    /// Layout: common(8), pid(4), comm[16], pad(4), clone_flags(8),
    /// oom_score_adj(2), pad(6) = 48.
    pub const LEN: usize = 48;

    pub fn parse(payload: &'a [u8]) -> Option<TaskNewtask<'a>> {
        TaskNewtask::parse_with(payload, &TaskNewtaskLayout::DEFAULT)
    }

    pub fn parse_with(payload: &'a [u8], layout: &TaskNewtaskLayout) -> Option<TaskNewtask<'a>> {
        Some(TaskNewtask {
            tid: i32_at(payload, layout.pid)?,
            comm: comm_at(payload, layout.comm)?,
            clone_flags: payload
                .get(layout.clone_flags..layout.clone_flags + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(0),
        })
    }
}

/// `ThreadStateSlice::ThreadState`, matching the proto's numeric values and
/// `perfr_thread_states::state`.
pub mod thread_state {
    pub const RUNNING: i32 = 0;
    pub const RUNNABLE: i32 = 1;
    pub const INTERRUPTIBLE_SLEEP: i32 = 2;
    pub const UNINTERRUPTIBLE_SLEEP: i32 = 3;
    pub const STOPPED: i32 = 4;
    pub const TRACED: i32 = 5;
    pub const DEAD: i32 = 6;
    pub const ZOMBIE: i32 = 7;
    pub const PARKED: i32 = 8;
    pub const IDLE: i32 = 9;
}

/// The state a `prev_state` mask means, twin of
/// `SwitchesStatesNamesVisitor::GetThreadStateFromBits`.
///
/// The order is the C++'s and the kernel's: the mask can in principle have
/// more than one bit set, and the first match wins. Zero means the thread was
/// preempted while still runnable, which is the common case and the reason
/// this is not simply a table lookup.
pub fn thread_state_from_bits(bits: i64) -> i32 {
    let bits = bits as u64;
    if bits & 0x01 != 0 {
        return thread_state::INTERRUPTIBLE_SLEEP;
    }
    if bits & 0x02 != 0 {
        return thread_state::UNINTERRUPTIBLE_SLEEP;
    }
    if bits & 0x04 != 0 {
        return thread_state::STOPPED;
    }
    if bits & 0x08 != 0 {
        return thread_state::TRACED;
    }
    if bits & 0x10 != 0 {
        return thread_state::DEAD;
    }
    if bits & 0x20 != 0 {
        return thread_state::ZOMBIE;
    }
    if bits & 0x40 != 0 {
        return thread_state::PARKED;
    }
    if bits & 0x80 != 0 {
        return thread_state::IDLE;
    }
    thread_state::RUNNABLE
}

/// True when the mask names more than one state, which the C++ logs as an
/// error before reporting only the first.
pub fn is_combined_state(bits: i64) -> bool {
    (bits as u64 & 0xFF).count_ones() > 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched_switch_payload(prev_tid: i32, prev_state: i64, next_tid: i32) -> Vec<u8> {
        let mut payload = vec![0u8; SchedSwitch::LEN];
        payload[8..12].copy_from_slice(b"prev");
        payload[24..28].copy_from_slice(&prev_tid.to_le_bytes());
        payload[32..40].copy_from_slice(&prev_state.to_le_bytes());
        payload[40..44].copy_from_slice(b"next");
        payload[56..60].copy_from_slice(&next_tid.to_le_bytes());
        payload
    }

    /// A `sched_switch` sample as the tracepoint ring carries it: header,
    /// the id fields, then the payload behind a u32 size.
    fn sched_switch_record(prev_tid: i32, next_tid: i32) -> Vec<u8> {
        let payload = sched_switch_payload(prev_tid, 0x01, next_tid);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&crate::record_type::SAMPLE.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&100u32.to_le_bytes()); // pid
        bytes.extend_from_slice(&101u32.to_le_bytes()); // tid
        bytes.extend_from_slice(&7_000u64.to_le_bytes()); // time
        bytes.extend_from_slice(&9u64.to_le_bytes()); // stream id
        bytes.extend_from_slice(&3u32.to_le_bytes()); // cpu
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        let size = bytes.len() as u16;
        bytes[6..8].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    /// `cargo test --release -p perfr-perf-records -- --ignored --nocapture parsing_cost`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn parsing_cost_of_a_sched_switch_record() {
        use crate::reader::{parse_record_sample, parse_tracepoint_sample, sample_bits, SampleFlags};
        let record = sched_switch_record(1234, 5678);
        let flags = SampleFlags { sample_type: sample_bits::TID_TIME_STREAMID_CPU | sample_bits::RAW, regs_user_count: 0 };
        const ROUNDS: u32 = 5_000_000;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let mut sum = 0i64;
            for _ in 0..ROUNDS {
                let sample = parse_record_sample(std::hint::black_box(&record), flags, true).unwrap();
                let switch = SchedSwitch::parse_with(sample.raw_data.as_deref().unwrap(), &SchedSwitchLayout::DEFAULT).unwrap();
                sum += i64::from(switch.prev_tid) + i64::from(switch.next_tid) + sample.time as i64;
            }
            eprintln!(
                "owned sample + sched_switch:    {:.1} ns/record ({sum})",
                start.elapsed().as_nanos() as f64 / f64::from(ROUNDS)
            );
            let start = std::time::Instant::now();
            let mut sum = 0i64;
            for _ in 0..ROUNDS {
                let sample = parse_tracepoint_sample(std::hint::black_box(&record)).unwrap();
                let switch = SchedSwitch::parse_with(sample.raw, &SchedSwitchLayout::DEFAULT).unwrap();
                sum += i64::from(switch.prev_tid) + i64::from(switch.next_tid) + sample.time as i64;
            }
            eprintln!(
                "borrowed sample + sched_switch: {:.1} ns/record ({sum})",
                start.elapsed().as_nanos() as f64 / f64::from(ROUNDS)
            );
        }
    }

    #[test]
    fn a_tracepoint_sample_read_in_place_matches_the_owned_one() {
        use crate::reader::{parse_record_sample, parse_tracepoint_sample, sample_bits, SampleFlags};
        let record = sched_switch_record(1234, 5678);
        let flags = SampleFlags { sample_type: sample_bits::TID_TIME_STREAMID_CPU | sample_bits::RAW, regs_user_count: 0 };
        let owned = parse_record_sample(&record, flags, true).unwrap();
        let lent = parse_tracepoint_sample(&record).unwrap();
        assert_eq!((lent.pid, lent.tid, lent.time, lent.cpu), (owned.pid, owned.tid, owned.time, owned.cpu));
        assert_eq!((lent.pid, lent.tid, lent.time, lent.cpu), (100, 101, 7_000, 3));
        assert_eq!(Some(lent.raw), owned.raw_data.as_deref());
        assert_eq!(SchedSwitch::parse(lent.raw).unwrap().next_tid, 5678);
        // Cut anywhere, both refuse it; neither reads past the end.
        for cut in 0..record.len() {
            assert_eq!(parse_tracepoint_sample(&record[..cut]), None, "cut at {cut}");
            assert!(parse_record_sample(&record[..cut], flags, true).is_none(), "cut at {cut}");
        }
        // A payload size that runs past the record.
        let mut long = record.clone();
        long[40..44].copy_from_slice(&(SchedSwitch::LEN as u32 + 1).to_le_bytes());
        assert_eq!(parse_tracepoint_sample(&long), None);
    }

    #[test]
    fn a_sched_switch_payload_yields_both_threads_and_the_state() {
        let payload = sched_switch_payload(1234, 0x01, 5678);
        let parsed = SchedSwitch::parse(&payload).unwrap();
        assert_eq!(parsed.prev_tid, 1234);
        assert_eq!(parsed.next_tid, 5678);
        assert_eq!(parsed.prev_state, 0x01);
        assert_eq!(parsed.prev_comm, b"prev");
        assert_eq!(parsed.next_comm, b"next");
    }

    #[test]
    fn a_payload_missing_its_documented_tail_still_parses() {
        // The last 4 bytes are undocumented padding; a kernel that omits them
        // must not cost us the record.
        let mut payload = sched_switch_payload(7, 0, 9);
        payload.truncate(60);
        assert_eq!(SchedSwitch::parse(&payload).unwrap().next_tid, 9);
    }

    #[test]
    fn a_truncated_payload_is_none_not_a_panic() {
        assert!(SchedSwitch::parse(&[0u8; 8]).is_none());
        assert!(SchedWakeup::parse(&[]).is_none());
        assert!(TaskNewtask::parse(&[0u8; 4]).is_none());
    }

    #[test]
    fn a_comm_is_trimmed_at_its_nul() {
        let mut payload = vec![0u8; SchedSwitch::LEN];
        payload[8..16].copy_from_slice(b"physics\0");
        assert_eq!(SchedSwitch::parse(&payload).unwrap().prev_comm, b"physics");
    }

    #[test]
    fn a_comm_filling_all_sixteen_bytes_is_not_truncated() {
        let mut payload = vec![0u8; SchedSwitch::LEN];
        payload[8..24].copy_from_slice(b"0123456789abcdef");
        assert_eq!(SchedSwitch::parse(&payload).unwrap().prev_comm, b"0123456789abcdef");
    }

    #[test]
    fn state_bits_map_the_way_the_kernel_prints_them() {
        assert_eq!(thread_state_from_bits(0x00), thread_state::RUNNABLE);
        assert_eq!(thread_state_from_bits(0x01), thread_state::INTERRUPTIBLE_SLEEP);
        assert_eq!(thread_state_from_bits(0x02), thread_state::UNINTERRUPTIBLE_SLEEP);
        assert_eq!(thread_state_from_bits(0x04), thread_state::STOPPED);
        assert_eq!(thread_state_from_bits(0x08), thread_state::TRACED);
        assert_eq!(thread_state_from_bits(0x10), thread_state::DEAD);
        assert_eq!(thread_state_from_bits(0x20), thread_state::ZOMBIE);
        assert_eq!(thread_state_from_bits(0x40), thread_state::PARKED);
        assert_eq!(thread_state_from_bits(0x80), thread_state::IDLE);
    }

    #[test]
    fn a_combined_mask_reports_the_first_state_and_is_flagged() {
        // The C++ logs an error and reports only the first; same order here.
        let bits = 0x01 | 0x02;
        assert!(is_combined_state(bits));
        assert_eq!(thread_state_from_bits(bits), thread_state::INTERRUPTIBLE_SLEEP);
        assert!(!is_combined_state(0x01));
        assert!(!is_combined_state(0x00));
    }

    #[test]
    fn high_bits_above_the_state_mask_are_ignored() {
        // Kernels set bits above 0xFF for things like TASK_NEW; they must not
        // be mistaken for a state.
        assert_eq!(thread_state_from_bits(0x1000), thread_state::RUNNABLE);
        assert!(!is_combined_state(0x1001));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    // A trimmed-down but real-shaped sched_switch format file.
    const SWITCH_FMT: &str = "name: sched_switch\nID: 314\nformat:\n\tfield:unsigned short common_type;\toffset:0;\tsize:2;\tsigned:0;\n\tfield:unsigned char common_flags;\toffset:2;\tsize:1;\tsigned:0;\n\tfield:unsigned char common_preempt_count;\toffset:3;\tsize:1;\tsigned:0;\n\tfield:int common_pid;\toffset:4;\tsize:4;\tsigned:1;\n\n\tfield:char prev_comm[16];\toffset:8;\tsize:16;\tsigned:0;\n\tfield:pid_t prev_pid;\toffset:24;\tsize:4;\tsigned:1;\n\tfield:int prev_prio;\toffset:28;\tsize:4;\tsigned:1;\n\tfield:long prev_state;\toffset:32;\tsize:8;\tsigned:1;\n\tfield:char next_comm[16];\toffset:40;\tsize:16;\tsigned:0;\n\tfield:pid_t next_pid;\toffset:56;\tsize:4;\tsigned:1;\n\tfield:int next_prio;\toffset:60;\tsize:4;\tsigned:1;\n";

    #[test]
    fn the_format_parser_reads_offsets_and_sizes_and_strips_array_bounds() {
        let f = format_field_offsets(SWITCH_FMT);
        assert_eq!(f.get("prev_comm"), Some(&(8, 16)));
        assert_eq!(f.get("prev_pid"), Some(&(24, 4)));
        assert_eq!(f.get("prev_state"), Some(&(32, 8)));
        assert_eq!(f.get("next_pid"), Some(&(56, 4)));
        assert_eq!(f.get("common_pid"), Some(&(4, 4)));
    }

    #[test]
    fn a_sched_switch_layout_from_format_matches_the_compiled_default() {
        assert_eq!(SchedSwitchLayout::from_format(SWITCH_FMT), Some(SchedSwitchLayout::DEFAULT));
    }

    #[test]
    fn a_moved_field_parses_by_its_own_layout_where_the_default_would_read_garbage() {
        // A kernel that inserted 8 bytes before next_comm/next_pid.
        let moved = SWITCH_FMT
            .replace("next_comm[16];\toffset:40", "next_comm[16];\toffset:48")
            .replace("next_pid;\toffset:56", "next_pid;\toffset:64");
        let layout = SchedSwitchLayout::from_format(&moved).unwrap();
        assert_eq!(layout.next_pid, 64);
        // Build a payload with next_pid=4242 at the moved offset.
        let mut payload = vec![0u8; 72];
        payload[8..10].copy_from_slice(b"ab"); // prev_comm
        payload[24..28].copy_from_slice(&7i32.to_le_bytes());
        payload[48..50].copy_from_slice(b"cd"); // next_comm at 48
        payload[64..68].copy_from_slice(&4242i32.to_le_bytes());
        let parsed = SchedSwitch::parse_with(&payload, &layout).unwrap();
        assert_eq!(parsed.next_tid, 4242);
        assert_eq!(parsed.prev_tid, 7);
        // The default layout would read next_pid from offset 56 -- zero here.
        let by_default = SchedSwitch::parse_with(&payload, &SchedSwitchLayout::DEFAULT).unwrap();
        assert_eq!(by_default.next_tid, 0);
    }

    #[test]
    fn from_format_is_none_when_a_needed_field_is_missing() {
        let stripped = SWITCH_FMT.replace("prev_state", "prev_STATE_renamed");
        assert_eq!(SchedSwitchLayout::from_format(&stripped), None);
    }

    #[test]
    fn wakeup_and_newtask_layouts_parse_from_format() {
        let wake = "\tfield:char comm[16];\toffset:8;\tsize:16;\tsigned:0;\n\tfield:pid_t pid;\toffset:24;\tsize:4;\tsigned:1;\n";
        assert_eq!(SchedWakeupLayout::from_format(wake), Some(SchedWakeupLayout::DEFAULT));
        let newtask = "\tfield:pid_t pid;\toffset:8;\tsize:4;\n\tfield:char comm[16];\toffset:12;\tsize:16;\n\tfield:unsigned long clone_flags;\toffset:32;\tsize:8;\n";
        assert_eq!(TaskNewtaskLayout::from_format(newtask), Some(TaskNewtaskLayout::DEFAULT));
    }

    /// The record as the kernel lays it out: `clone_flags` is an 8-byte
    /// field, so it is aligned to 32 and four bytes of padding follow `comm`.
    #[test]
    fn the_default_newtask_layout_reads_clone_flags_past_the_alignment_padding() {
        let mut payload = vec![0u8; TaskNewtask::LEN];
        payload[8..12].copy_from_slice(&4242i32.to_le_bytes());
        payload[12..18].copy_from_slice(b"worker");
        payload[32..40].copy_from_slice(&(CLONE_THREAD | 0x100).to_le_bytes());
        payload[40..42].copy_from_slice(&(-17i16).to_le_bytes()); // oom_score_adj
        let parsed = TaskNewtask::parse(&payload).unwrap();
        assert_eq!(parsed.tid, 4242);
        assert_eq!(parsed.comm, b"worker");
        assert_eq!(parsed.clone_flags, CLONE_THREAD | 0x100);
        assert_eq!(payload.len(), 48, "the size of trace_event_raw_task_newtask");
    }
}
