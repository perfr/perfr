// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Invocation history, not a map from a trampoline to its latest caller.
use perfr_frida_transport::returns::{Journal, Record};
use std::{cell::Cell, collections::HashMap};
struct Frame {
    record: Record,
    end: u64,
}
pub struct Returns {
    pub journal: Journal,
    frames: HashMap<u64, Frame>,
    threads: HashMap<u32, Vec<u64>>,
    pub trampolines: Vec<u64>,
    pub horizon: u64,
    pub invalid: bool,
    pub patched_slots: Cell<u64>,
    pub incomplete_samples: Cell<u64>,
}
impl Returns {
    pub fn new(journal: Journal) -> Self {
        Self {
            journal,
            frames: HashMap::new(),
            threads: HashMap::new(),
            trampolines: Vec::new(),
            horizon: crate::now_monotonic_ns(),
            invalid: false,
            patched_slots: Cell::new(0),
            incomplete_samples: Cell::new(0),
        }
    }
    pub fn poll(&mut self) {
        let mut records = Vec::new();
        let horizon = self.journal.drain(&mut records);
        self.trampolines = self.journal.trampolines();
        self.invalid |= self.journal.lost() != 0;
        // A stopped publisher must not retain sampled stacks indefinitely.
        self.invalid |= horizon.is_none()
            && crate::now_monotonic_ns().saturating_sub(self.horizon) > 250_000_000;
        if !self.invalid {
            for record in records {
                if record.entry {
                    // Bound history even if calls never return or a stalled
                    // publisher prevents samples advancing their watermark.
                    if self.frames.len() >= 262144 {
                        self.invalid = true;
                        break;
                    }
                    self.threads
                        .entry(record.tid)
                        .or_default()
                        .push(record.invocation);
                    self.frames.insert(
                        record.invocation,
                        Frame {
                            record,
                            end: u64::MAX,
                        },
                    );
                } else if let Some(frame) = self.frames.get_mut(&record.invocation) {
                    frame.end = record.time;
                } else {
                    self.invalid = true;
                    break;
                }
            }
        }
        if self.invalid {
            self.frames.clear();
            self.threads.clear();
            // With invalid history samples are emitted leaf-only, not held.
            self.horizon = u64::MAX;
        } else if let Some(horizon) = horizon {
            self.horizon = horizon;
        }
    }
    pub fn patch(&self, tid: u32, sp: u64, time: u64, stack: &mut [u8]) -> bool {
        if self.invalid {
            return false;
        }
        let Some(frames) = self.threads.get(&tid) else {
            return true;
        };
        // Reverse invocation order also peels a tail-call chain: the newer
        // hook's saved original may itself be the older hook's trampoline.
        for id in frames.iter().rev() {
            let Some(frame) = self.frames.get(id) else {
                continue;
            };
            let r = &frame.record;
            if time < r.time || time >= frame.end {
                continue;
            }
            let Some(offset) = r.sp.checked_sub(sp).and_then(|v| usize::try_from(v).ok()) else {
                continue;
            };
            let Some(end) = offset.checked_add(8) else {
                continue;
            };
            let Some(slot) = stack.get_mut(offset..end) else {
                continue;
            };
            // A nonlocal exit can skip on_leave. Never rewrite a reused
            // slot merely because an old invocation is still in history.
            if u64::from_le_bytes(slot.try_into().unwrap()) == r.trampoline {
                slot.copy_from_slice(&r.original.to_le_bytes());
                self.patched_slots.set(self.patched_slots.get() + 1);
            }
        }
        true
    }
    pub fn retire(&mut self, horizon: u64) {
        self.frames.retain(|_, f| f.end > horizon);
        let frames = &self.frames;
        self.threads.retain(|_, ids| {
            ids.retain(|id| frames.contains_key(id));
            !ids.is_empty()
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn history() -> (tempfile::NamedTempFile, Returns) {
        let f = tempfile::NamedTempFile::new().unwrap();
        f.as_file()
            .set_len(perfr_frida_transport::returns::FILE_SIZE as u64)
            .unwrap();
        let j = Journal::open(f.as_file()).unwrap();
        (f, Returns::new(j))
    }
    fn entry(
        h: &mut Returns,
        id: u64,
        tid: u32,
        sp: u64,
        original: u64,
        trampoline: u64,
        start: u64,
        end: u64,
    ) {
        h.threads.entry(tid).or_default().push(id);
        h.frames.insert(
            id,
            Frame {
                record: Record {
                    time: start,
                    tid,
                    sp,
                    original,
                    trampoline,
                    invocation: id,
                    entry: true,
                },
                end,
            },
        );
    }
    #[test]
    fn recursive_calls_and_delayed_samples_keep_their_own_callers() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1010, 0xaaa, 0xccc, 10, 60);
        entry(&mut h, 2, 7, 0x1000, 0xbbb, 0xccc, 20, 40);
        let mut stack = [0; 24];
        stack[..8].copy_from_slice(&0xcccu64.to_le_bytes());
        stack[16..].copy_from_slice(&0xcccu64.to_le_bytes());
        h.patch(7, 0x1000, 30, &mut stack);
        assert_eq!(&stack[..8], &0xbbbu64.to_le_bytes());
        assert_eq!(&stack[16..], &0xaaau64.to_le_bytes());
        let mut later = 0xcccu64.to_le_bytes();
        h.patch(7, 0x1000, 50, &mut later);
        assert_eq!(later, 0xcccu64.to_le_bytes());
        // Identical slot address and trampoline on another thread must
        // restore that thread's own caller, not the latest global caller.
        entry(&mut h, 3, 8, 0x1000, 0xddd, 0xccc, 20, 40);
        let mut other = 0xcccu64.to_le_bytes();
        h.patch(8, 0x1000, 30, &mut other);
        assert_eq!(other, 0xdddu64.to_le_bytes());
        let mut unknown = 0xcccu64.to_le_bytes();
        h.patch(9, 0x1000, 30, &mut unknown);
        assert_eq!(unknown, 0xcccu64.to_le_bytes());
    }
    #[test]
    fn tail_calls_peel_in_reverse_but_reused_slots_are_not_overwritten() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1000, 0xaaa, 0xccc, 10, u64::MAX);
        entry(&mut h, 2, 7, 0x1000, 0xccc, 0xddd, 20, 40);
        let mut slot = 0xdddu64.to_le_bytes();
        h.patch(7, 0x1000, 30, &mut slot);
        assert_eq!(slot, 0xaaau64.to_le_bytes());
        // The old invocation survived a longjmp; the slot now holds normal code.
        let mut reused = 0xeeeeu64.to_le_bytes();
        h.patch(7, 0x1000, 50, &mut reused);
        assert_eq!(reused, 0xeeeeu64.to_le_bytes());
        entry(&mut h, 3, 7, 0x1000, 0xbbb, 0xccc, 60, 90);
        let mut newer = 0xcccu64.to_le_bytes();
        h.patch(7, 0x1000, 70, &mut newer);
        assert_eq!(newer, 0xbbbu64.to_le_bytes());
    }
    #[test]
    fn history_is_kept_until_the_sample_consumers_have_advanced() {
        let (_f, mut h) = history();
        entry(&mut h, 1, 7, 0x1000, 0xaaa, 0xccc, 10, 40);
        // Native return metadata has advanced, but Python enrichment has not.
        h.horizon = 100;
        h.retire(20);
        let mut delayed = 0xcccu64.to_le_bytes();
        assert!(h.patch(7, 0x1000, 30, &mut delayed));
        assert_eq!(delayed, 0xaaau64.to_le_bytes());
        h.retire(100);
        assert!(h.frames.is_empty());
        assert!(h.threads.is_empty());
    }

    #[test]
    fn missing_metadata_never_results_in_a_guessed_patch() {
        let (_f, mut h) = history();
        h.journal.invalidate();
        h.poll();
        let mut slot = 0xcccu64.to_le_bytes();
        assert!(!h.patch(7, 0x1000, 30, &mut slot));
        assert_eq!(slot, 0xcccu64.to_le_bytes());
    }
}
