// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Joining a Python stack to the native stack of the same thread.
//!
//! The interpreter runs Python in one C function, its eval function. Every
//! time C code calls into Python (the start of a thread, a callback handed to
//! `sorted`, `__init__` reached through a type's C slot) that function is
//! entered again, and since CPython 3.11 a Python function calling another
//! stays inside the same entry. So a native stack has one eval frame per
//! *run* of Python frames, and the Python stack marks where each run ends
//! ([`Frame::entry`](crate::Frame)). Before 3.11 every Python frame is a run
//! of its own.
//!
//! [`merge`] walks the native stack from the innermost frame and, at each
//! eval frame, puts the next run of Python frames in its place.
//!
//! What stays of the native stack is what a Python-only profiler does not
//! have: everything under the innermost Python function (the interpreter
//! allocating, collecting garbage, sorting a list; a C extension and the
//! libraries it calls), and every frame that is not the interpreter's
//! wherever it is. What goes is the interpreter's own frames *above* a
//! Python function: `_PyEval_Vector`, `method_vectorcall`, `thread_run`,
//! `Py_RunMain` are how Python calls Python, three or four of them at every
//! boundary, nameless in a stripped interpreter, and they say nothing the
//! two Python frames around them do not. py-spy leaves out more: every
//! interpreter frame it does not know by a short list of prefixes.

use crate::readings::Frame;

/// One frame of a merged stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merged {
    /// The native frame at this index of the native stack.
    Native(usize),
    /// A Python function, by its symbol id in the [`Readings`](crate::Readings).
    Python(u32),
}

/// What a native frame is, as far as a merge cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Native {
    /// The interpreter's eval function, see [`is_eval_frame`].
    Eval,
    /// Any other frame inside the interpreter's image.
    Interpreter,
    /// A frame of anything else: a C extension, libc, the program Python is
    /// embedded in.
    Other,
}

/// Whether a native symbol is the interpreter's eval function: the frame a
/// run of Python frames stands for. `_PyEval_EvalFrameDefault` since 3.6,
/// `PyEval_EvalFrameEx` before, and the pieces a compiler splits off either
/// (`.cold`, `.part.0`). The same test py-spy makes.
pub fn is_eval_frame(name: &str) -> bool {
    let whole = name.split('.').next().unwrap_or(name);
    matches!(
        whole.trim_start_matches('_'),
        "PyEval_EvalFrameDefault" | "PyEval_EvalFrameEx"
    )
}

/// How many runs a Python stack has: its marked frames, and the outermost
/// run when the last frame does not carry a mark.
fn runs(python: &[Frame]) -> usize {
    python.iter().filter(|frame| frame.entry).count()
        + usize::from(python.last().is_some_and(|frame| !frame.entry))
}

/// Merges `python` into a native stack, both innermost first.
///
/// `native` says what each native frame is. `whole` is whether the native
/// unwind reached the root of the stack.
///
/// Returns `None`, leaving `out` unspecified, when nothing of the Python
/// stack can be placed; otherwise how far the two stacks were from agreeing:
/// the number of eval frames or runs left without a partner. Zero is two
/// stacks of the same shape. The caller has two readings to choose from and
/// takes the one that is closer.
///
/// The two stacks are not of the same instant, so they may differ, and a
/// stack grows and shrinks at its inner end. That is where a difference is
/// put:
///
/// - **A whole native stack** is paired from the root: its outermost eval
///   frame is the outermost run, and so on inwards. Runs the native stack has
///   no eval frame for were called after the sample, or had returned before
///   it, and are left out. Eval frames with no run stay as they are, the
///   interpreter at work under the innermost Python function that is known.
///   (py-spy pauses the process and still meets the second case: the
///   interpreter pops a Python frame a moment before its eval function
///   returns.)
/// - **A native stack cut short** has lost its root, so there is only the
///   inner end to pair from. The runs it has no eval frame for are its
///   missing outer part and go on the outer end. More eval frames than runs
///   cannot be placed, beyond the one py-spy lets through.
///
/// The interpreter's frames above a Python frame are left out, see the
/// module documentation.
pub fn merge(
    native: &[Native],
    whole: bool,
    python: &[Frame],
    out: &mut Vec<Merged>,
) -> Option<usize> {
    out.clear();
    let runs = runs(python);
    let evals = native.iter().filter(|kind| **kind == Native::Eval).count();
    if runs == 0 || (whole && evals == 0) {
        return None;
    }
    // Inner eval frames and inner runs that have no partner.
    let (mut spare_evals, spare_runs) = if whole {
        (evals.saturating_sub(runs), runs.saturating_sub(evals))
    } else {
        (0, 0)
    };
    let apart = spare_evals + spare_runs;
    // The first frame after the spare inner runs.
    let mut next = 0;
    for _ in 0..spare_runs {
        while next < python.len() {
            next += 1;
            if python[next - 1].entry {
                break;
            }
        }
    }
    let first = next;
    let mut unmatched_evals = 0;
    for (index, kind) in native.iter().copied().enumerate() {
        // Once a Python frame is placed, everything further out is above it.
        let above_python = next > first;
        let is_eval = match kind {
            Native::Other => {
                out.push(Merged::Native(index));
                continue;
            }
            Native::Interpreter => false,
            Native::Eval => true,
        };
        if is_eval && spare_evals > 0 {
            // An eval frame whose Python function is not known: the
            // interpreter, under whatever is.
            spare_evals -= 1;
        } else if is_eval && next < python.len() {
            // One run: up to and including the frame that entered the eval
            // function.
            while next < python.len() {
                let frame = python[next];
                out.push(Merged::Python(frame.symbol));
                next += 1;
                if frame.entry {
                    break;
                }
            }
            continue;
        } else if is_eval {
            unmatched_evals += 1;
        }
        if !above_python {
            out.push(Merged::Native(index));
        }
    }
    if unmatched_evals > 1 {
        return None;
    }
    if next < python.len() {
        // Only a native stack cut short gets here: a whole one has been
        // paired run for run.
        out.extend(
            python[next..]
                .iter()
                .map(|frame| Merged::Python(frame.symbol)),
        );
    }
    Some(apart + unmatched_evals)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Python stack from a compact description: one string per frame,
    /// innermost first, a trailing `|` marking the end of a run.
    fn python(frames: &[&str]) -> Vec<Frame> {
        frames
            .iter()
            .map(|text| {
                let entry = text.ends_with('|');
                Frame {
                    symbol: text.trim_end_matches('|').parse().unwrap(),
                    entry,
                }
            })
            .collect()
    }

    /// A native stack: `E` is an eval frame, `i` another frame of the
    /// interpreter, anything else a frame of something else.
    fn native(frames: &str) -> Vec<Native> {
        frames
            .chars()
            .map(|c| match c {
                'E' => Native::Eval,
                'i' => Native::Interpreter,
                _ => Native::Other,
            })
            .collect()
    }

    /// The merged stack and how far apart the two were.
    fn merged_apart(
        native_frames: &str,
        whole: bool,
        python_frames: &[&str],
    ) -> Option<(String, usize)> {
        let mut out = Vec::new();
        merge(
            &native(native_frames),
            whole,
            &python(python_frames),
            &mut out,
        )
        .map(|apart| {
            let text = out
                .iter()
                .map(|frame| match frame {
                    Merged::Native(index) => format!("n{index}"),
                    Merged::Python(symbol) => format!("p{symbol}"),
                })
                .collect::<Vec<_>>()
                .join(" ");
            (text, apart)
        })
    }

    /// The merged stack of two stacks that agree.
    fn merged(native_frames: &str, whole: bool, python_frames: &[&str]) -> Option<String> {
        merged_apart(native_frames, whole, python_frames).map(|(text, apart)| {
            assert_eq!(
                apart, 0,
                "{native_frames} and {python_frames:?} were expected to agree"
            );
            text
        })
    }

    #[test]
    fn the_eval_function_is_known_by_every_name_it_has() {
        for yes in [
            "_PyEval_EvalFrameDefault",
            "_PyEval_EvalFrameDefault.cold",
            "_PyEval_EvalFrameDefault.part.0",
            "PyEval_EvalFrameEx",
            "_PyEval_EvalFrameDefault.cold.2962",
        ] {
            assert!(is_eval_frame(yes), "{yes}");
        }
        for no in [
            "_PyEval_Vector",
            "PyEval_EvalCode",
            "_PyEval_EvalFrame",
            "PyEval_RestoreThread",
            "python3.14+0x62a268",
            "",
        ] {
            assert!(!is_eval_frame(no), "{no}");
        }
    }

    #[test]
    fn a_run_of_python_frames_takes_the_place_of_its_eval_frame() {
        // PyLong_FromLong <- eval <- _PyEval_Vector <- thread_run <- start_thread <- clone,
        // and the eval ran crunch, called by worker, called by Thread.run.
        assert_eq!(
            merged("iEiinn", true, &["1", "2", "3|"]).as_deref(),
            Some("n0 p1 p2 p3 n4 n5")
        );
    }

    #[test]
    fn what_the_interpreter_does_under_the_innermost_python_frame_stays() {
        // free <- _Py_Dealloc <- list_dealloc <- eval <- glue <- main.
        assert_eq!(
            merged("niiEin", true, &["1|"]).as_deref(),
            Some("n0 n1 n2 p1 n5")
        );
        // A C extension under Python, reached through the interpreter's call
        // machinery: all of it is under the innermost Python frame.
        assert_eq!(
            merged("nniEn", true, &["1|"]).as_deref(),
            Some("n0 n1 n2 p1 n4")
        );
    }

    #[test]
    fn each_eval_frame_takes_one_run_and_the_interpreter_between_them_goes() {
        // A key function called by sorted(): eval <- list_sort <- builtin_sorted <- eval <- main.
        assert_eq!(
            merged("EiiEn", true, &["7|", "8", "9|"]).as_deref(),
            Some("p7 p8 p9 n4")
        );
    }

    #[test]
    fn a_frame_of_another_module_between_two_runs_stays() {
        // Python called back from a C extension: eval <- qsort (libc) <- extension <- glue <- eval.
        assert_eq!(
            merged("EnniEn", true, &["7|", "8|"]).as_deref(),
            Some("p7 n1 n2 p8 n5")
        );
    }

    #[test]
    fn before_3_11_every_python_frame_is_a_run_of_its_own() {
        assert_eq!(
            merged("iEiEiEn", true, &["1|", "2|", "3|"]).as_deref(),
            Some("n0 p1 p2 p3 n6")
        );
    }

    #[test]
    fn the_outermost_run_needs_no_mark() {
        // The last frame of the Python stack ends a run whether marked or not.
        assert_eq!(
            merged("iEn", true, &["1", "2"]).as_deref(),
            Some("n0 p1 p2 n2")
        );
    }

    #[test]
    fn a_native_stack_cut_short_gets_the_rest_of_the_python_stack_as_its_outer_part() {
        // The unwind failed two frames up: no eval frame was reached at all.
        assert_eq!(
            merged("ni", false, &["1", "2|", "3|"]).as_deref(),
            Some("n0 n1 p1 p2 p3")
        );
        // It reached one of two.
        assert_eq!(
            merged("nEi", false, &["1|", "2", "3|"]).as_deref(),
            Some("n0 p1 p2 p3")
        );
    }

    #[test]
    fn a_whole_native_stack_is_paired_from_the_root_and_inner_runs_it_has_no_eval_for_are_left_out()
    {
        // The reading caught a key function that the sample, a moment
        // later, was no longer in: the outer run is the eval frame's.
        assert_eq!(
            merged_apart("iEn", true, &["7|", "8", "9|"]),
            Some(("n0 p8 p9 n2".to_string(), 1))
        );
        assert_eq!(
            merged_apart("iEn", true, &["5|", "6|", "8", "9|"]),
            Some(("n0 p8 p9 n2".to_string(), 2))
        );
    }

    #[test]
    fn an_inner_eval_frame_with_no_run_is_the_interpreter_under_the_python_that_is_known() {
        // The sample is in a key function the reading did not see: the inner
        // eval frame stays, with what it called and what called it.
        assert_eq!(
            merged_apart("iEiiEin", true, &["8", "9|"]),
            Some(("n0 n1 n2 n3 p8 p9 n6".to_string(), 1))
        );
        assert_eq!(
            merged_apart("EiEiEn", true, &["1|"]),
            Some(("n0 n1 n2 n3 p1 n5".to_string(), 2))
        );
    }

    #[test]
    fn a_whole_native_stack_that_is_not_in_the_interpreter_takes_no_python() {
        assert_eq!(merged_apart("nin", true, &["1|"]), None);
    }

    #[test]
    fn a_native_stack_cut_short_lets_one_eval_frame_beyond_the_python_stack_through_and_not_two() {
        assert_eq!(
            merged_apart("EiEn", false, &["1|"]),
            Some(("p1 n3".to_string(), 1))
        );
        assert_eq!(merged_apart("EiEiE", false, &["1|"]), None);
    }

    #[test]
    fn an_empty_python_stack_merges_nothing() {
        assert_eq!(merged_apart("nEn", true, &[]), None);
        assert_eq!(merged_apart("nn", false, &[]), None);
    }
}
