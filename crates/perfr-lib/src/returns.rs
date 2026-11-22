// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the agent remembers about intercepted returns from one capture to
//! the next. A call that began under one capture can still be on a stack
//! in a later one, with that capture's trampoline in its return slot and
//! no entry in the later journal.
//!
//! Nothing here is on the path of a call through a hook whose trampoline
//! is known: that path is `Journal::enter` and `Journal::leave` alone.
use perfr_frida_transport::returns::{Journal, TRAMPOLINES};
use std::sync::Mutex;

/// The return trampoline of every hook this process has had. Each new
/// journal starts with all of them, so a sample that meets one it has no
/// history for stops there and does not guess a caller.
///
/// Never pruned: nothing says when the last call through an old one has
/// returned. Once there are `TRAMPOLINES` of them a new hook's is refused,
/// and that hook's calls are tainted ones.
static KNOWN: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Threads now inside a hook whose trampoline is not known, and how deep.
/// Kept across captures: the next journal is told about each of them.
static TAINTED: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// Journals of captures that have ended, each still mapped. A call that
/// loaded its session just before the end may yet write to one, so a
/// journal is only given back when the next capture opens, and then by
/// `Journal::discard`, which leaves memory where it was.
static RETIRED: Mutex<Vec<&'static Journal>> = Mutex::new(Vec::new());

/// A capture has ended, or its session is being replaced.
pub fn retire(journal: &'static Journal) {
    if let Ok(mut retired) = RETIRED.lock() {
        if !retired.iter().any(|j| std::ptr::eq(*j, journal)) {
            retired.push(journal);
        }
    }
}

/// Gives back the journals of the captures before the last one.
pub fn discard_retired() {
    if let Ok(mut retired) = RETIRED.lock() {
        for journal in retired.drain(..) {
            let _ = journal.discard();
        }
    }
}

/// Starts a new journal with every trampoline known so far. False when the
/// registry cannot be read: the capture must not start half-told.
pub fn seed_known(journal: &Journal) -> bool {
    let Ok(known) = KNOWN.lock() else {
        return false;
    };
    known.iter().all(|&address| journal.register(address))
}

/// Tells a new journal which threads are inside a hook nobody can name,
/// then runs `publish`, which makes the journal the current one, before
/// any other thread can become one of them. A thread that taints itself
/// is therefore either in this list or sees the new journal.
pub fn seed_tainted_and_publish(journal: Option<&Journal>, publish: impl FnOnce()) {
    let tainted = TAINTED.lock();
    if let (Ok(tainted), Some(journal)) = (tainted.as_ref(), journal) {
        for &(tid, _) in tainted.iter() {
            journal.taint_carried(tid);
        }
    }
    publish();
}

/// A hook has found its return trampoline. True when samples can now be
/// stopped at it: in the registry, and in `current` when a capture is
/// open. The caller holds the lock that serialises open and close.
pub fn register(trampoline: u64, current: Option<&Journal>) -> bool {
    if trampoline == 0 {
        return false;
    }
    let Ok(mut known) = KNOWN.lock() else {
        return false;
    };
    if !known.contains(&trampoline) {
        if known.len() >= TRAMPOLINES {
            return false;
        }
        known.push(trampoline);
    }
    current.is_none_or(|journal| journal.register(trampoline))
}

/// A thread is entering a hook whose trampoline is not known. `current`
/// is looked up under the lock, by the caller's closure, for the reason
/// `seed_tainted_and_publish` gives.
pub fn taint<'a>(tid: u32, current: impl FnOnce() -> Option<&'a Journal>) -> u64 {
    let Ok(mut tainted) = TAINTED.lock() else {
        return 0;
    };
    match tainted.iter_mut().find(|(t, _)| *t == tid) {
        Some((_, depth)) => *depth += 1,
        None => tainted.push((tid, 1)),
    }
    current().map_or(0, |journal| journal.taint(tid))
}

/// The thread has returned through that hook.
pub fn untaint(tid: u32) {
    if let Ok(mut tainted) = TAINTED.lock() {
        if let Some(at) = tainted.iter().position(|(t, _)| *t == tid) {
            tainted[at].1 -= 1;
            if tainted[at].1 == 0 {
                tainted.swap_remove(at);
            }
        }
    }
}

#[cfg(test)]
pub fn forget_everything() {
    KNOWN.lock().unwrap().clear();
    TAINTED.lock().unwrap().clear();
    RETIRED.lock().unwrap().clear();
}
