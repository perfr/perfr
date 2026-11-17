// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the cores of the captured machine are, and the mark each one's
//! scheduler lane carries.
//!
//! A capture says what it knows of each core (`CpuCores`, see
//! [`perfr_live_event::CoreInfo`]): its kind, its top frequency, its level 3
//! cache. A lane is marked with what tells its core from the others of the
//! machine, and with nothing when the cores are all alike: a `P` or an `E`
//! on a hybrid part, the cache's size where half the cores have a larger
//! one, the top frequency where it is not the same everywhere. All of what
//! is known of a core is in the words for its tooltip.

use perfr_live_event::{core_kind, CoreInfo};

/// The cores of the capture on screen, and which of their facts differ.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreMarks {
    /// In the order of their numbers.
    cores: Vec<CoreInfo>,
    kinds_differ: bool,
    caches_differ: bool,
    freqs_differ: bool,
    top_cache_kb: u32,
    top_freq_khz: u32,
}

/// What a core's lane is marked with. Either text may be empty, not both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreMark {
    /// The core's kind among the machine's: `P`, `E`, or the size of its
    /// cache (`96M`). Empty when the cores are of one kind and one cache.
    pub chip: String,
    /// The chip is the stronger of the machine's: a performance core, the
    /// larger cache.
    pub strong: bool,
    /// The core's top frequency in GHz (`5.8`). Empty when it is the same
    /// on every core.
    pub freq: String,
    /// No core of the machine has a higher top frequency.
    pub fastest: bool,
}

impl CoreMarks {
    /// Forgets the cores: the capture on screen is another one.
    pub fn clear(&mut self) {
        *self = CoreMarks::default();
    }

    /// Takes in what a capture says of some of its cores. A core said again
    /// is replaced, the others are kept: the machines of a capture each
    /// tell of their own.
    pub fn adopt(&mut self, cores: &[CoreInfo]) {
        for core in cores {
            match self.cores.binary_search_by_key(&core.cpu, |c| c.cpu) {
                Ok(at) => self.cores[at] = *core,
                Err(at) => self.cores.insert(at, *core),
            }
        }
        let differ = |values: &mut Vec<u32>| {
            values.sort_unstable();
            values.dedup();
            values.len() > 1
        };
        self.kinds_differ = differ(&mut self.cores.iter().map(|c| u32::from(c.kind)).collect());
        self.caches_differ = differ(&mut self.cores.iter().map(|c| c.l3_kb).filter(|&kb| kb > 0).collect());
        self.freqs_differ = differ(&mut self.cores.iter().map(|c| c.max_freq_khz).filter(|&khz| khz > 0).collect());
        self.top_cache_kb = self.cores.iter().map(|c| c.l3_kb).max().unwrap_or(0);
        self.top_freq_khz = self.cores.iter().map(|c| c.max_freq_khz).max().unwrap_or(0);
    }

    fn core(&self, cpu: u16) -> Option<&CoreInfo> {
        self.cores.binary_search_by_key(&cpu, |c| c.cpu).ok().map(|at| &self.cores[at])
    }

    /// The mark of core `cpu`'s lane; none when nothing is known of the
    /// core, or nothing tells it from the others.
    pub fn mark(&self, cpu: u16) -> Option<CoreMark> {
        let core = self.core(cpu)?;
        let mut mark = CoreMark::default();
        if self.kinds_differ && core.kind != core_kind::UNKNOWN {
            mark.chip = kind_letter(core.kind).to_string();
            mark.strong = core.kind == core_kind::PERFORMANCE;
        } else if self.caches_differ && core.l3_kb > 0 {
            mark.chip = cache_size(core.l3_kb, "M", "K");
            mark.strong = core.l3_kb == self.top_cache_kb;
        }
        if self.freqs_differ && core.max_freq_khz > 0 {
            mark.freq = format!("{:.1}", f64::from(core.max_freq_khz) / 1e6);
            mark.fastest = core.max_freq_khz == self.top_freq_khz;
        }
        (!mark.chip.is_empty() || !mark.freq.is_empty()).then_some(mark)
    }

    /// Everything known of core `cpu`, in words: `Performance core, up to
    /// 5.80 GHz, 36 MB L3`. None when nothing is.
    pub fn describe(&self, cpu: u16) -> Option<String> {
        let core = self.core(cpu)?;
        let mut parts: Vec<String> = Vec::new();
        match core.kind {
            core_kind::PERFORMANCE => parts.push("Performance core".into()),
            core_kind::EFFICIENCY => parts.push("Efficiency core".into()),
            _ => {}
        }
        if core.max_freq_khz > 0 {
            parts.push(format!("up to {:.2} GHz", f64::from(core.max_freq_khz) / 1e6));
        }
        if core.l3_kb > 0 {
            parts.push(format!("{} L3", cache_size(core.l3_kb, " MB", " KB")));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

fn kind_letter(kind: u8) -> &'static str {
    match kind {
        core_kind::PERFORMANCE => "P",
        core_kind::EFFICIENCY => "E",
        _ => "",
    }
}

/// A cache of `kb` KiB in the unit that reads shortest: whole megabytes
/// when it is one or more, kilobytes below.
fn cache_size(kb: u32, mega: &str, kilo: &str) -> String {
    if kb >= 1024 {
        format!("{}{mega}", (f64::from(kb) / 1024.0).round())
    } else {
        format!("{kb}{kilo}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core(cpu: u16, kind: u8, max_freq_khz: u32, l3_kb: u32) -> CoreInfo {
        CoreInfo { cpu, kind, max_freq_khz, l3_kb }
    }

    fn marks(cores: &[CoreInfo]) -> CoreMarks {
        let mut marks = CoreMarks::default();
        marks.adopt(cores);
        marks
    }

    /// `chip` and `freq` of a core's mark, with `!` after the strong chip
    /// and the fastest frequency.
    fn shown(marks: &CoreMarks, cpu: u16) -> String {
        match marks.mark(cpu) {
            Some(m) => format!(
                "{}{} {}{}",
                m.chip,
                if m.strong { "!" } else { "" },
                m.freq,
                if m.fastest { "!" } else { "" }
            )
            .trim()
            .to_string(),
            None => "-".into(),
        }
    }

    #[test]
    fn a_hybrid_machine_marks_kind_and_top_frequency() {
        // A 13th generation Core: two favoured performance cores, the
        // others a step below, the efficiency cores well below.
        let m = marks(&[
            core(0, core_kind::PERFORMANCE, 5_500_000, 36_864),
            core(4, core_kind::PERFORMANCE, 5_800_000, 36_864),
            core(16, core_kind::EFFICIENCY, 4_300_000, 36_864),
        ]);
        assert_eq!(shown(&m, 0), "P! 5.5");
        assert_eq!(shown(&m, 4), "P! 5.8!");
        assert_eq!(shown(&m, 16), "E 4.3");
        assert_eq!(shown(&m, 9), "-", "a core the capture said nothing of");
        assert_eq!(m.describe(4).as_deref(), Some("Performance core, up to 5.80 GHz, 36 MB L3"));
        assert_eq!(m.describe(16).as_deref(), Some("Efficiency core, up to 4.30 GHz, 36 MB L3"));
        assert_eq!(m.describe(9), None);
    }

    #[test]
    fn a_machine_with_a_larger_cache_on_some_cores_marks_the_cache() {
        // An X3D Ryzen: no kind named, the stacked cache on half the cores
        // and the higher frequency on the other half.
        let m = marks(&[core(0, core_kind::UNKNOWN, 5_050_000, 98_304), core(8, core_kind::UNKNOWN, 5_759_000, 32_768)]);
        assert_eq!(shown(&m, 0), "96M! 5.0");
        assert_eq!(shown(&m, 8), "32M 5.8!");
        assert_eq!(m.describe(0).as_deref(), Some("up to 5.05 GHz, 96 MB L3"));
    }

    #[test]
    fn cores_that_are_all_alike_carry_no_mark_and_still_say_what_they_are() {
        let m = marks(&[core(0, core_kind::UNKNOWN, 4_900_000, 16_384), core(1, core_kind::UNKNOWN, 4_900_000, 16_384)]);
        assert_eq!(m.mark(0), None);
        assert_eq!(m.mark(1), None);
        assert_eq!(m.describe(1).as_deref(), Some("up to 4.90 GHz, 16 MB L3"));
        // A frequency not known on one core is not a difference.
        let m = marks(&[core(0, core_kind::UNKNOWN, 4_900_000, 0), core(1, core_kind::UNKNOWN, 0, 0)]);
        assert_eq!(m.mark(0), None);
        assert_eq!(m.describe(1), None);
        // A cache under a megabyte is said in kilobytes.
        let m = marks(&[core(0, core_kind::UNKNOWN, 0, 512), core(1, core_kind::UNKNOWN, 0, 2_048)]);
        assert_eq!(shown(&m, 0), "512K");
        assert_eq!(shown(&m, 1), "2M!");
        assert_eq!(m.describe(0).as_deref(), Some("512 KB L3"));
    }

    #[test]
    fn cores_said_later_join_the_others_and_a_new_capture_starts_with_none() {
        let mut m = marks(&[core(0, core_kind::UNKNOWN, 4_900_000, 0), core(1, core_kind::UNKNOWN, 4_900_000, 0)]);
        assert_eq!(m.mark(0), None);
        // Another machine of the capture, on the lanes after these: its
        // cores top out lower, and now the frequency tells them apart.
        m.adopt(&[core(2, core_kind::UNKNOWN, 3_600_000, 0)]);
        assert_eq!(shown(&m, 0), "4.9!");
        assert_eq!(shown(&m, 2), "3.6");
        // A core said again is replaced.
        m.adopt(&[core(2, core_kind::UNKNOWN, 4_900_000, 0)]);
        assert_eq!(m.mark(2), None);
        m.clear();
        assert_eq!(m, CoreMarks::default());
        assert_eq!(m.describe(0), None);
    }
}
