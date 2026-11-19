// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Python call stacks of a running process, and how they join a native one.
//!
//! A stack sample of a Python program, unwound natively, is the interpreter:
//! `_PyEval_EvalFrameDefault` over and over, and not one function the program's
//! author wrote. The names are in the interpreter's own frame objects, on its
//! heap. [py-spy](https://github.com/benfred/py-spy) knows how to find and
//! read those from outside the process for every CPython since 2.3, and this
//! crate is how perfr uses it:
//!
//! - [`Reader`] is a thread that asks py-spy for every thread's Python stack,
//!   about as often as the capture samples, **without pausing the target**. A
//!   paused target would show in the capture as its own scheduling, and only
//!   one tracer may hold a process, which would be perfr's hooks or this.
//! - [`Readings`] keeps the last seconds of what it read, by thread and time,
//!   with who held the GIL and what Python calls each thread.
//! - [`merge`] puts a Python stack where a native stack has the interpreter's
//!   eval function, the rule py-spy's own `--native` mode uses, and leaves
//!   out the interpreter's frames between one Python function and the next.
//!
//! # What a merged stack can and cannot claim
//!
//! The native sample is taken by the kernel at one instant. The Python stack
//! is read by another thread at another instant, up to half a reading period
//! away, and from memory the target is writing. So:
//!
//! - A reading that fails is dropped and counted; nothing is made up for it.
//!   So is one thread's stack that stops short of the thread's outermost
//!   frame: it was read while a frame was being linked to its caller.
//! - A reading is only joined to a sample of the same thread taken close to
//!   it ([`Readings::near`]): there is one on either side, and the nearer is
//!   used unless the other is closer in shape to the native stack.
//! - Where the two stacks differ in shape, one has a call from C into Python
//!   that the other does not: a callback entered or returned between the
//!   sample and the reading. [`merge`] pairs them from the root and leaves
//!   the difference at the inner end, where a stack changes: an eval frame
//!   with no Python function stays the interpreter's, and a Python function
//!   with no eval frame is left out.
//! - What cannot be checked is the inside of a run: Python calling Python
//!   does not move the native stack. In code that calls many small functions
//!   the innermost Python frames of a merged stack are those of a moment up
//!   to half a period earlier or later. Over a report that evens out; for one
//!   sample it is the limit of reading from outside.
//!
//! Everything but the reader is plain data and runs on any platform; the
//! reader is Linux, where perfr samples.

mod merge;
mod readings;

#[cfg(target_os = "linux")]
mod reader;

pub use merge::{is_eval_frame, merge, Merged, Native};
pub use readings::{Frame, GilSpan, Near, RawFrame, Readings, State, Stats, Symbol, ThreadReading};

#[cfg(target_os = "linux")]
pub use reader::Reader;

/// Whether a mapped file is the Python interpreter or its shared library, by
/// its name: `python`, `python3`, `python3.14`, `python3.14t`,
/// `libpython3.12.so.1.0`. A program that links the interpreter statically
/// under its own name is not recognised by this; it can still be read when
/// asked for outright.
pub fn is_python_image(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let rest = match name.strip_prefix("libpython") {
        Some(rest) => rest,
        None => match name.strip_prefix("python") {
            Some(rest) => rest,
            None => return false,
        },
    };
    // What may follow: a version, a build suffix (`t`, `d`, `m`, `dm`), and
    // for the library `.so` and its version. `python-config` and
    // `pythonista` are not interpreters.
    let bytes = rest.as_bytes();
    let mut at = 0;
    while at < bytes.len()
        && (bytes[at].is_ascii_digit()
            || (bytes[at] == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)))
    {
        at += 1;
    }
    let tail = rest[at..].trim_start_matches(['t', 'd', 'm', 'u']);
    tail.is_empty() || tail.starts_with(".so") || tail == ".exe" || tail == ".dll"
}

/// Whether a process has a Python interpreter mapped, from the text of its
/// `/proc/<pid>/maps`.
pub fn maps_show_python(maps: &str) -> bool {
    maps.lines().any(|line| {
        // address perms offset dev inode path
        let mut fields = line.split_ascii_whitespace();
        let perms = fields.nth(1).unwrap_or("");
        let path = fields.nth(3).unwrap_or("");
        perms.contains('x')
            && path.starts_with('/')
            && is_python_image(path.trim_end_matches(" (deleted)"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interpreter_and_its_library_are_recognised_by_name() {
        for yes in [
            "/usr/bin/python3.14",
            "/usr/bin/python3",
            "/usr/bin/python",
            "/opt/py/bin/python3.13t",
            "/usr/bin/python2.7",
            "/usr/lib/x86_64-linux-gnu/libpython3.12.so.1.0",
            "/usr/lib/libpython3.11d.so",
            "/home/a/.pyenv/versions/3.10.4/bin/python3.10m",
            "C:/Python312/python.exe",
            "python312.dll",
        ] {
            assert!(is_python_image(yes), "{yes}");
        }
        for no in [
            "/usr/bin/python3-config",
            "/usr/bin/pythonista",
            "/usr/lib/libpythonic.so",
            "/usr/bin/bash",
            "",
        ] {
            assert!(!is_python_image(no), "{no}");
        }
    }

    #[test]
    fn a_process_is_python_when_the_interpreter_is_mapped_executable() {
        let python = "\
00400000-00420000 r--p 00000000 08:02 1234 /usr/bin/python3.14
00420000-00700000 r-xp 00020000 08:02 1234 /usr/bin/python3.14
7f00-7f10 r-xp 00000000 08:02 99 /usr/lib/x86_64-linux-gnu/libc.so.6
";
        assert!(maps_show_python(python));
        let embedded = "7f00-7f10 r-xp 00000000 08:02 99 /usr/lib/libpython3.12.so.1.0 (deleted)\n";
        assert!(maps_show_python(embedded));
        // A data mapping of a file that only looks like it, and no file at all.
        let not = "\
00400000-00420000 r--p 00000000 08:02 1234 /usr/bin/python3.14
7f00-7f10 r-xp 00000000 08:02 99 /usr/lib/x86_64-linux-gnu/libc.so.6
7f20-7f30 r-xp 00000000 00:00 0
7f20-7f30 r-xp 00000000 00:00 0 [vdso]
";
        assert!(!maps_show_python(not));
    }
}
