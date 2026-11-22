// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the program prints, as log lines of the capture.
//!
//! A probe on libc's `write` hands every write to descriptor 1 or 2 to
//! [`perfr_output_write`], on the thread that made it and before the kernel
//! sees it. The bytes are put together into lines here, the colour codes a
//! terminal would have eaten are taken out, and each line becomes one log
//! record of that thread, at the time of the write that completed it.
//!
//! A line is recorded as it was written, at the info level. Nothing is read
//! into its words: what is an error, a warning or a value to graph is the
//! program's to say, through the API.
//!
//! The probe is on the entry of `write` alone. Nothing is done to its
//! return, so a stack sample taken inside `write` unwinds as it always did.
//!
//! What this does not see: a program that writes with a system call of its
//! own (Go), with `writev` or `sendfile`, or to a duplicate of the
//! descriptor under another number.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use perfr_scope_ring::event::level;

/// The longest line kept. A longer one is cut here and marked.
const LINE_MAX: usize = 2048;
/// Lines a second past which the rest of that second is counted and not
/// recorded: a program dumping megabytes to a file through descriptor 1
/// would lap the scope rings and take the scopes with it.
const LINES_PER_SECOND: u32 = 2000;
/// The most of one `write` that is looked at.
const WRITE_MAX: usize = 256 * 1024;

/// The capture the probe reports to: the generation `perfr_frida_open` gave
/// out, or 0 when no capture wants the output.
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Writes that found the tap busy on another thread, or in a signal handler
/// that interrupted it, and were let go.
static CONTENDED: AtomicU64 = AtomicU64::new(0);

static TAP: Mutex<Tap> = Mutex::new(Tap::new());

/// Starts handing lines to the capture of `generation`.
pub(crate) fn start(generation: u32) {
    if let Ok(mut tap) = TAP.lock() {
        *tap = Tap::new();
    }
    GENERATION.store(generation, Ordering::Release);
}

/// Stops. A line still waiting for its newline is dropped: it would be
/// logged by the thread that stops the capture, which did not write it.
pub(crate) fn stop(generation: u32) {
    let _ = GENERATION.compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Relaxed);
}

/// A terminal escape sequence being skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    /// ESC was the last byte.
    Started,
    /// Inside `ESC [ ... final`, the colours and cursor moves.
    Control,
    /// Inside `ESC ] ... BEL` or `ESC ] ... ESC \`, the window titles.
    System,
}

/// The line of one descriptor that is being put together.
struct Line {
    bytes: [u8; LINE_MAX],
    len: usize,
    cut: bool,
    escape: Escape,
}

impl Line {
    const fn new() -> Self {
        Self { bytes: [0; LINE_MAX], len: 0, cut: false, escape: Escape::None }
    }

    /// Takes `bytes` in and calls `emit` with every line they complete. A
    /// line ends at a newline or at a carriage return, so a progress bar
    /// that redraws itself is a line per redraw; an empty line is nothing.
    fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&[u8], bool)) {
        for &byte in bytes {
            match self.escape {
                Escape::Started => {
                    self.escape = match byte {
                        b'[' => Escape::Control,
                        b']' => Escape::System,
                        _ => Escape::None,
                    };
                    continue;
                }
                Escape::Control => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.escape = Escape::None;
                    }
                    continue;
                }
                Escape::System => {
                    if byte == 0x07 {
                        self.escape = Escape::None;
                    } else if byte == 0x1b {
                        self.escape = Escape::Started;
                    }
                    continue;
                }
                Escape::None => {}
            }
            match byte {
                0x1b => self.escape = Escape::Started,
                b'\n' | b'\r' => {
                    let mut end = self.len;
                    while end > 0 && self.bytes[end - 1] == b' ' {
                        end -= 1;
                    }
                    if end > 0 {
                        emit(&self.bytes[..end], self.cut);
                    }
                    self.len = 0;
                    self.cut = false;
                }
                b'\t' => self.push(b' '),
                byte if byte < 0x20 || byte == 0x7f => {}
                byte => self.push(byte),
            }
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len < LINE_MAX {
            self.bytes[self.len] = byte;
            self.len += 1;
        } else {
            self.cut = true;
        }
    }
}

struct Tap {
    /// Descriptor 1 and descriptor 2.
    lines: [Line; 2],
    /// The second the count below is of, and lines recorded in it.
    second: u64,
    recorded: u32,
    /// Lines of that second that were over the limit.
    skipped: u32,
}

impl Tap {
    const fn new() -> Self {
        Self { lines: [Line::new(), Line::new()], second: 0, recorded: 0, skipped: 0 }
    }

    /// Whether a line at `now_ns` is within the limit; `Some(n)` first when
    /// a second has just ended in which `n` lines were not.
    fn admit(&mut self, now_ns: u64) -> (bool, Option<u32>) {
        let second = now_ns / 1_000_000_000;
        let mut report = None;
        if second != self.second {
            if self.skipped > 0 {
                report = Some(self.skipped);
            }
            self.second = second;
            self.recorded = 0;
            self.skipped = 0;
        }
        if self.recorded < LINES_PER_SECOND {
            self.recorded += 1;
            (true, report)
        } else {
            self.skipped += 1;
            (false, report)
        }
    }
}

/// Called by the probe on `write(fd, buf, count)` for descriptor 1 or 2.
///
/// # Safety
/// `buf` points at `count` readable bytes for the length of the call: they
/// are the arguments the program is passing to `write`.
#[no_mangle]
pub unsafe extern "C" fn perfr_output_write(fd: i32, buf: *const u8, count: usize) {
    if super::FORK_CHILD.load(Ordering::Relaxed) || buf.is_null() || !(1..=2).contains(&fd) {
        return;
    }
    let generation = GENERATION.load(Ordering::Acquire);
    if generation == 0 || !super::session_is(generation) || !perfr_api::capturing() {
        return;
    }
    // Never wait: this is inside the program's `write`, which a signal
    // handler may call while the thread it interrupted is in here.
    let Ok(mut tap) = TAP.try_lock() else {
        CONTENDED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let bytes = std::slice::from_raw_parts(buf, count.min(WRITE_MAX));
    let now_ns = perfr_api::now_ns();
    let mut pending = PendingLines::new();
    tap.lines[(fd - 1) as usize].feed(bytes, |line, cut| pending.push(line, cut));
    for (line, cut) in pending.iter() {
        let (admitted, skipped) = tap.admit(now_ns);
        if let Some(skipped) = skipped {
            let mut text = [0u8; 64];
            let n = write_skipped(&mut text, skipped);
            perfr_api::log(level::WARN, &text[..n]);
        }
        if admitted {
            if cut {
                let mut marked = [0u8; LINE_MAX + 4];
                marked[..line.len()].copy_from_slice(line);
                marked[line.len()..line.len() + 4].copy_from_slice(b" ...");
                perfr_api::log(level::INFO, &marked[..line.len() + 4]);
            } else {
                perfr_api::log(level::INFO, line);
            }
        }
    }
}

/// The lines one `write` completed, copied out of the line buffer so that
/// the limiter, which lives beside it, can be asked about each. On the
/// stack: there is no allocating inside the program's `write`.
struct PendingLines {
    bytes: [u8; PendingLines::BYTES],
    used: usize,
    ends: [(u16, bool); PendingLines::LINES],
    count: usize,
}

impl PendingLines {
    const BYTES: usize = 16 * 1024;
    const LINES: usize = 128;

    fn new() -> Self {
        Self { bytes: [0; Self::BYTES], used: 0, ends: [(0, false); Self::LINES], count: 0 }
    }

    /// A line that does not fit is dropped: one `write` of more than 128
    /// lines or 16 KiB is a dump, not a log.
    fn push(&mut self, line: &[u8], cut: bool) {
        if self.count == Self::LINES || self.used + line.len() > Self::BYTES {
            return;
        }
        self.bytes[self.used..self.used + line.len()].copy_from_slice(line);
        self.used += line.len();
        self.ends[self.count] = (self.used as u16, cut);
        self.count += 1;
    }

    fn iter(&self) -> impl Iterator<Item = (&[u8], bool)> {
        let mut start = 0usize;
        self.ends[..self.count].iter().map(move |&(end, cut)| {
            let line = &self.bytes[start..end as usize];
            start = end as usize;
            (line, cut)
        })
    }
}

/// "N more line(s) of output in that second were not recorded", without
/// allocating. Returns the length.
fn write_skipped(out: &mut [u8; 64], skipped: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut n = skipped;
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let tail = b" more line(s) of output in that second were not recorded";
    let mut len = 0;
    for &byte in digits[at..].iter().chain(tail.iter()) {
        out[len] = byte;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(chunks: &[&[u8]]) -> Vec<(String, bool)> {
        let mut line = Line::new();
        let mut out = Vec::new();
        for chunk in chunks {
            line.feed(chunk, |bytes, cut| out.push((String::from_utf8_lossy(bytes).into_owned(), cut)));
        }
        out
    }

    #[test]
    fn a_line_is_what_ends_in_a_newline_however_the_writes_were_cut() {
        assert_eq!(
            lines_of(&[b"iter 0: loss 4.2", b"654, time 14.9ms\niter 10: lo", b"ss 3.1\n\npartial"]),
            [("iter 0: loss 4.2654, time 14.9ms".to_string(), false), ("iter 10: loss 3.1".to_string(), false)]
        );
    }

    #[test]
    fn colours_and_cursor_moves_are_taken_out_and_a_redraw_is_a_line() {
        assert_eq!(
            lines_of(&[b"\x1b[32mok\x1b[0m  \x1b]0;title\x07done\r\n", b"10%|#   |\r20%|##  |\r"]),
            [("ok  done".to_string(), false), ("10%|#   |".to_string(), false), ("20%|##  |".to_string(), false)]
        );
    }

    #[test]
    fn a_line_longer_than_is_kept_is_cut_and_marked() {
        let long = vec![b'x'; LINE_MAX + 100];
        let got = lines_of(&[&long, b"\nnext\n"]);
        assert_eq!(got[0].0.len(), LINE_MAX);
        assert!(got[0].1, "the cut is marked");
        assert_eq!(got[1], ("next".to_string(), false));
    }

    #[test]
    fn past_the_limit_a_seconds_lines_are_counted_and_reported_once() {
        let mut tap = Tap::new();
        let second = 5_000_000_000u64;
        for _ in 0..LINES_PER_SECOND {
            assert_eq!(tap.admit(second), (true, None));
        }
        assert_eq!(tap.admit(second + 1), (false, None));
        assert_eq!(tap.admit(second + 2), (false, None));
        // The next second's first line is recorded, after the report.
        assert_eq!(tap.admit(second + 1_000_000_000), (true, Some(2)));
        assert_eq!(tap.admit(second + 1_000_000_001), (true, None));
        let mut text = [0u8; 64];
        let n = write_skipped(&mut text, 1234);
        assert_eq!(&text[..n], b"1234 more line(s) of output in that second were not recorded");
    }

    #[test]
    fn the_lines_of_one_write_are_handed_on_in_order() {
        let mut pending = PendingLines::new();
        pending.push(b"one", false);
        pending.push(b"two two", true);
        let got: Vec<(Vec<u8>, bool)> = pending.iter().map(|(l, c)| (l.to_vec(), c)).collect();
        assert_eq!(got, [(b"one".to_vec(), false), (b"two two".to_vec(), true)]);
    }
}
