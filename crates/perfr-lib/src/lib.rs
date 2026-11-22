// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Native lifecycle guard around the target’s perfr start/stop API.
#[cfg(feature = "native")]
mod native;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod returns;
use perfr_frida_transport::Transport;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::OnceLock;

// An injected mapping belongs to one process. A fork inherits both it and
// possibly-held Rust locks; ignore child callbacks before touching those locks.
static FORK_CHILD: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static ATFORK: OnceLock<i32> = OnceLock::new();
#[cfg(unix)]
unsafe extern "C" fn after_fork() {
    FORK_CHILD.store(true, Ordering::Relaxed);
}

/// Registers the fork guard once; nothing to guard against on Windows.
#[cfg(unix)]
unsafe fn arm_fork_guard() -> bool {
    *ATFORK.get_or_init(|| libc::pthread_atfork(None, None, Some(after_fork))) == 0
}
#[cfg(windows)]
unsafe fn arm_fork_guard() -> bool {
    true
}

type Start = unsafe extern "C" fn(*const libc::c_char, usize) -> u64;
type Stop = extern "C" fn(u64);
type Init = extern "C" fn() -> libc::c_int;

/// One capture's way from a hook to the scope API.
struct Session {
    generation: u32,
    mapping: Transport,
    start: Start,
    stop: Stop,
    /// Where calls through this capture's hooks are journaled for the
    /// sampler, when the capture samples.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    returns: Option<perfr_frida_transport::returns::Journal>,
}

/// The capture hooks report to now, or null between captures.
///
/// A pointer the hooks load, not a lock they take. Every hooked call on
/// every thread came through a process-wide `RwLock` twice, on entry and
/// on return: four atomic updates of one cache line per call, and a stop
/// waiting to close the session held every caller up behind it. With eight
/// threads in a hooked function that was 700 ns a call against 24 on one.
///
/// Each session is leaked, its 32-byte lease mapping with it, so a call
/// that loaded the pointer just before a close still reads memory that is
/// there. That is a page of address space for each capture of the process.
///
/// The return journal of a capture that samples is a megabyte of the lease
/// file, and is reached the same way, with no lock. It is not unmapped
/// either: when the next capture opens, `Journal::discard` swaps the
/// file's pages for untouched zero pages at the same addresses (see
/// `returns::RETIRED`), so a late call writes to memory that is there and
/// the file's pages are not kept for the life of the process.
static SESSION: AtomicPtr<Session> = AtomicPtr::new(std::ptr::null_mut());

/// Serialises open and close, and remembers the last generation given out.
static GENERATIONS: Mutex<u32> = Mutex::new(0);

#[inline]
fn session() -> Option<&'static Session> {
    // SAFETY: only `perfr_frida_open` stores a non-null pointer, to a
    // leaked box that is never freed.
    unsafe { SESSION.load(Ordering::Acquire).as_ref() }
}

/// Returns zero on failure, otherwise a generation identifying this capture.
/// The path is supplied by our native controller and must be C-terminated.
///
/// # Safety
///
/// `path` is null or points at a NUL-terminated string that stays valid for
/// the call.
#[no_mangle]
pub unsafe extern "C" fn perfr_frida_open(
    path: *const libc::c_char,
    init: Option<Init>,
    start: Option<Start>,
    stop: Option<Stop>,
) -> u32 {
    if FORK_CHILD.load(Ordering::Relaxed) || !arm_fork_guard() {
        return 0;
    }
    if path.is_null() {
        return 0;
    }
    let Ok(path) = std::ffi::CStr::from_ptr(path).to_str() else {
        return 0;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return 0;
    };
    let Ok(mapping) = Transport::open(&file, std::process::id()) else {
        return 0;
    };
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let returns = if mapping.return_repair_enabled() {
        match perfr_frida_transport::returns::Journal::open(&file) {
            Ok(journal) => Some(journal),
            Err(_) => return 0,
        }
    } else {
        None
    };
    let Ok(mut last_generation) = GENERATIONS.lock() else {
        return 0;
    };
    // Refuse two collectors rather than silently stealing a running session.
    if session().is_some_and(|s| s.mapping.active() && s.mapping.controller_alive()) {
        return 0;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        // A capture that ended has had until now for its late calls to
        // finish; one that is only now being replaced waits a capture more.
        returns::discard_retired();
        if let Some(journal) = session().and_then(|old| old.returns.as_ref()) {
            returns::retire(journal);
        }
        if returns.as_ref().is_some_and(|journal| !returns::seed_known(journal)) {
            return 0;
        }
    }
    let reader = perfr_scope_ring::ScopeRingReader::open(std::process::id()).ok();
    let descriptor = reader.as_ref().map_or(0, |r| r.api_descriptor());
    let api = if descriptor != 0 {
        // This executes inside the producer; perfr-api publishes a static
        // descriptor after initialization. No cross-process dereference.
        let api = &*(descriptor as *const perfr_api::DynamicApiV1);
        if api.version != 1 {
            return 0;
        }
        (api.start, api.stop)
    } else {
        match (init, start, stop) {
            (Some(init), Some(start), Some(stop)) => {
                if init() != 0 {
                    return 0;
                }
                (start, stop)
            }
            (None, None, None) => {
                // A segment with no descriptor is one of two things. Held
                // by a live writer, it belongs to an SDK in this process
                // from before the descriptor existed, whose API cannot be
                // resolved: never replace that. Held by nobody, it is what
                // a dead process with this pid left behind, and the sweep
                // will not remove it while the pid is alive, which it now
                // is. The two used to be taken for the first, and the
                // answer kept: every arm failed for the life of the
                // process, as it did after one init that failed for a
                // reason that had passed. So nothing is remembered here;
                // init is a pointer load once it has succeeded.
                if reader.as_ref().is_some_and(|r| r.writer_alive()) {
                    return 0;
                }
                if perfr_api::init().is_err() {
                    return 0;
                }
                (
                    perfr_api::perfr_start_dynamic as Start,
                    perfr_api::perfr_stop as Stop,
                )
            }
            _ => return 0,
        }
    };
    *last_generation = last_generation.wrapping_add(1).max(1);
    let session: &'static Session = Box::leak(Box::new(Session {
        generation: *last_generation,
        mapping,
        start: api.0,
        stop: api.1,
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        returns,
    }));
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    returns::seed_tainted_and_publish(session.returns.as_ref(), || {
        SESSION.store(std::ptr::from_ref(session).cast_mut(), Ordering::Release)
    });
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    SESSION.store(std::ptr::from_ref(session).cast_mut(), Ordering::Release);
    *last_generation
}

#[no_mangle]
pub extern "C" fn perfr_frida_close(generation: u32) {
    if FORK_CHILD.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(_control) = GENERATIONS.lock() {
        if session().is_some_and(|s| s.generation == generation) {
            // Publish the end of metadata before callbacks stop recording,
            // including a detach initiated by loss of the helper connection.
            // The journal stays mapped: see `returns::RETIRED`.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            if let Some(journal) = session().and_then(|current| current.returns.as_ref()) {
                journal.close();
                returns::retire(journal);
            }
            SESSION.store(std::ptr::null_mut(), Ordering::Release);
        }
    }
}

/// # Safety
/// `name` points to `len` readable bytes for this call.
#[no_mangle]
pub unsafe extern "C" fn perfr_frida_start(
    generation: u32,
    name: *const libc::c_char,
    len: usize,
) -> u64 {
    if FORK_CHILD.load(Ordering::Relaxed) {
        return 0;
    }
    if let Some(session) = session() {
        if session.generation == generation && session.mapping.active() {
            return (session.start)(name, len);
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn perfr_frida_stop(generation: u32, handle: u64) {
    if handle == 0 || FORK_CHILD.load(Ordering::Relaxed) {
        return;
    }
    if let Some(session) = session() {
        if session.generation == generation && session.mapping.active() {
            (session.stop)(handle);
            session.mapping.record_call();
        }
    }
}

/// A hook has found its return trampoline, on the first call through it
/// and before Gum replaces that call's return slot. Nonzero when samples
/// can now be stopped at it; zero when it is not an address or no more can
/// be registered, and the hook's calls are then tainted ones.
///
/// This cold path is serialised with open and close, so seeding a new
/// journal and publishing its session cannot miss a late callback from the
/// previous capture.
#[no_mangle]
pub extern "C" fn perfr_frida_register_return(trampoline: u64) -> i32 {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if FORK_CHILD.load(Ordering::Relaxed) {
            return 0;
        }
        let Ok(_control) = GENERATIONS.lock() else {
            return 0;
        };
        let current = session().and_then(|s| s.returns.as_ref());
        i32::from(returns::register(trampoline, current))
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = trampoline;
        0
    }
}

/// The journal a call through a hook of `generation` writes to.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[inline]
fn journal_of(generation: u32) -> Option<&'static perfr_frida_transport::returns::Journal> {
    if FORK_CHILD.load(Ordering::Relaxed) {
        return None;
    }
    session()
        .filter(|s| s.generation == generation && s.mapping.active())
        .and_then(|s| s.returns.as_ref())
}

// Gum calls these before it replaces a return slot, and on return. A token
// belongs to one capture; a late return cannot close a newer invocation.
// `droppable` is a hook muted for calling too often: its entries give way
// before the journal fills (see `Journal::enter`).
#[no_mangle]
pub extern "C" fn perfr_frida_return_enter(
    generation: u32,
    tid: u32,
    sp: u64,
    original: u64,
    trampoline: u64,
    droppable: i32,
) -> u64 {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Some(journal) = journal_of(generation) {
        return journal.enter(tid, sp, original, trampoline, droppable != 0);
    }
    let _ = (generation, tid, sp, original, trampoline, droppable);
    0
}
#[no_mangle]
pub extern "C" fn perfr_frida_return_leave(generation: u32, tid: u32, invocation: u64) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if invocation != 0 {
        if let Some(journal) = journal_of(generation) {
            journal.leave(tid, invocation);
        }
    }
    let _ = (generation, tid, invocation);
}

/// The thread is entering a hook whose return trampoline is not known. Its
/// samples are leaf-only until `perfr_frida_return_untaint`: in this
/// capture, and in any other that opens while the call is still running.
///
/// The token is nonzero only for a call through a hook of the capture that
/// is open. A late call through an older capture's hook taints the open
/// journal all the same, and that taint stays to the capture's end.
#[no_mangle]
pub extern "C" fn perfr_frida_return_taint(generation: u32, tid: u32) -> u64 {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !FORK_CHILD.load(Ordering::Relaxed) {
        let mut own = false;
        let token = returns::taint(tid, || {
            let current = session()?;
            own = current.generation == generation;
            current.returns.as_ref()
        });
        return if own { token } else { 0 };
    }
    let _ = (generation, tid);
    0
}
#[no_mangle]
pub extern "C" fn perfr_frida_return_untaint(generation: u32, tid: u32, token: u64) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !FORK_CHILD.load(Ordering::Relaxed) {
        returns::untaint(tid);
        if let Some(journal) = journal_of(generation) {
            journal.untaint(tid, token);
        }
    }
    let _ = (generation, tid, token);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static STOPS: AtomicU64 = AtomicU64::new(0);
    extern "C" fn init() -> i32 {
        0
    }
    unsafe extern "C" fn start(_: *const libc::c_char, _: usize) -> u64 {
        42
    }
    extern "C" fn stop(h: u64) {
        assert_eq!(h, 42);
        STOPS.fetch_add(1, Ordering::Relaxed);
    }
    /// The session is one per process, so the tests that open one take turns.
    static ONE_SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

    unsafe extern "C" fn quiet_start(_: *const libc::c_char, _: usize) -> u64 {
        7
    }
    extern "C" fn quiet_stop(_: u64) {}

    /// A lease as the service makes it for a capture that samples, and the
    /// service's own view of the journal in it.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn sampled_lease() -> (tempfile::NamedTempFile, Transport, perfr_frida_transport::returns::Journal) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let lease = Transport::create(file.as_file(), std::process::id()).unwrap();
        let journal = perfr_frida_transport::returns::Journal::create(file.as_file()).unwrap();
        lease.enable_return_repair();
        (file, lease, journal)
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn open_quiet(file: &tempfile::NamedTempFile) -> u32 {
        let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
        let generation = unsafe { perfr_frida_open(path.as_ptr(), Some(init), Some(quiet_start), Some(quiet_stop)) };
        assert_ne!(generation, 0);
        generation
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn drained(journal: &perfr_frida_transport::returns::Journal) -> Vec<(perfr_frida_transport::returns::Kind, u32)> {
        let mut records = Vec::new();
        journal.drain(&mut records);
        records.iter().map(|r| (r.kind, r.tid)).collect()
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn first_return_discovery_survives_capture_boundaries() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        returns::forget_everything();
        assert_eq!(perfr_frida_register_return(0), 0, "not an address");
        // A callback may discover its address with no active session at all.
        assert_eq!(perfr_frida_register_return(0x123400), 1);
        let mut previous = 0;
        for address in [0x123500, 0x123600] {
            let (file, _lease, journal) = sampled_lease();
            let generation = open_quiet(&file);
            assert!(journal.trampolines().contains(&0x123400));
            if previous != 0 {
                assert!(journal.trampolines().contains(&previous));
            }
            // An old callback discovered after the new session was published.
            assert_eq!(perfr_frida_register_return(address), 1);
            assert!(journal.trampolines().contains(&address));
            // A hook of another capture writes nothing here; one of this does.
            assert_eq!(perfr_frida_return_enter(generation.wrapping_sub(1), 1, 8, 16, address, 0), 0);
            let token = perfr_frida_return_enter(generation, 1, 8, 16, address, 0);
            assert_ne!(token, 0);
            perfr_frida_return_leave(generation, 1, token);
            use perfr_frida_transport::returns::Kind;
            assert_eq!(drained(&journal), [(Kind::Enter, 1), (Kind::Leave, 1)]);
            perfr_frida_close(generation);
            assert!(journal.closed());
            previous = address;
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn a_full_registry_refuses_the_next_hook_and_condemns_no_capture() {
        use perfr_frida_transport::returns::{Kind, TRAMPOLINES};
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        returns::forget_everything();
        for hook in 1..=TRAMPOLINES as u64 {
            assert_eq!(perfr_frida_register_return(hook * 16), 1);
        }
        assert_eq!(perfr_frida_register_return(0xdead0), 0);
        let (file, _lease, journal) = sampled_lease();
        let generation = open_quiet(&file);
        assert_eq!(journal.trampolines().len(), TRAMPOLINES);
        assert!(!journal.unknown_return(), "the capture's other samples are still unwound");
        // What the refused hook's calls do instead: taint their own thread.
        let token = perfr_frida_return_taint(generation, 41);
        assert_ne!(token, 0);
        perfr_frida_return_untaint(generation, 41, token);
        assert_eq!(drained(&journal), [(Kind::Taint, 41), (Kind::Untaint, 41)]);
        perfr_frida_close(generation);
        returns::forget_everything();
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn a_call_through_an_unknown_trampoline_taints_its_thread_in_the_next_capture_too() {
        use perfr_frida_transport::returns::Kind;
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        returns::forget_everything();
        let (first_file, _first_lease, first_journal) = sampled_lease();
        let first = open_quiet(&first_file);
        let token = perfr_frida_return_taint(first, 77);
        assert_ne!(token, 0);
        assert_eq!(drained(&first_journal), [(Kind::Taint, 77)]);
        perfr_frida_close(first);

        // The call is still running when the next capture opens: this
        // journal has no entry of its own for it, and is told.
        let (second_file, _second_lease, second_journal) = sampled_lease();
        let second = open_quiet(&second_file);
        assert_eq!(drained(&second_journal), [(Kind::Taint, 77)]);
        // A late call through the first capture's hook, on another thread.
        assert_eq!(perfr_frida_return_taint(first, 78), 0, "no token: it is not this capture's to end");
        assert_eq!(drained(&second_journal), [(Kind::Taint, 78)]);
        // Both return. Neither taint is lifted in a journal that cannot
        // tell which of a thread's calls it was.
        perfr_frida_return_untaint(first, 77, token);
        perfr_frida_return_untaint(first, 78, 0);
        assert_eq!(drained(&second_journal), []);
        perfr_frida_close(second);

        // And the capture after that starts with nobody tainted.
        let (third_file, _third_lease, third_journal) = sampled_lease();
        let third = open_quiet(&third_file);
        assert_eq!(drained(&third_journal), []);
        perfr_frida_close(third);
    }

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn a_call_that_outlives_its_capture_writes_to_memory_that_is_still_there() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        returns::forget_everything();
        let (first_file, _first_lease, first_journal) = sampled_lease();
        let first = open_quiet(&first_file);
        // What a hook that loaded the session just before the close holds.
        let late = session().unwrap().returns.as_ref().unwrap();
        perfr_frida_close(first);
        assert_eq!(perfr_frida_return_enter(first, 1, 8, 16, 24, 0), 0, "a new call finds no session");
        assert_ne!(late.enter(1, 8, 16, 24, false), 0);
        assert_eq!(drained(&first_journal).len(), 1, "closed, and still the file");

        let (second_file, _second_lease, _second_journal) = sampled_lease();
        let second = open_quiet(&second_file);
        // The next capture has opened: the first journal's pages went back
        // to the file, and the addresses are still memory.
        assert_ne!(late.enter(1, 8, 16, 24, false), 0);
        assert_ne!(late.enter(1, 8, 16, 24, false), 0);
        assert_eq!(drained(&first_journal).len(), 0, "no longer the file");
        perfr_frida_close(second);
    }

    /// What journaling a call for the sampler adds to the hook path, with
    /// the service reading the journal as fast as it is written.
    ///
    /// Twice over. Back to back, which is more than one reader can take
    /// from eight threads, so part of what it times is a full ring turning
    /// a record away (the share is printed); and with a few hundred
    /// nanoseconds of the thread's own work around each record, which is
    /// what a call through a hook is: Gum's own entry and exit cost more
    /// than that.
    ///
    /// With one ring for every thread the second figure was 1100 ns a call
    /// on eight threads, all of it waiting for the ring's one reservation
    /// word. With a ring picked by the thread's id it is 175.
    ///
    /// `cargo test -p perfr-lib --release what_a_sampled_call -- --nocapture`
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn what_a_sampled_call_pays_to_journal_its_return() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        returns::forget_everything();
        const CALLS: u64 = 200_000;
        fn work(rounds: u32) {
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            for _ in 0..rounds {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
            }
            std::hint::black_box(x);
        }
        let (file, _lease, reader) = sampled_lease();
        let generation = open_quiet(&file);
        assert_eq!(perfr_frida_register_return(0x7000), 1);
        let reading = AtomicBool::new(true);
        std::thread::scope(|scope| {
            let read = scope.spawn(|| {
                let (mut records, mut read) = (Vec::new(), 0u64);
                while reading.load(Ordering::Relaxed) {
                    reader.drain(&mut records);
                    read += records.len() as u64;
                    records.clear();
                }
                reader.drain(&mut records);
                read + records.len() as u64
            });
            let mut written = 0;
            for rounds in [0u32, 150] {
                for threads in [1u32, 2, 8] {
                    let mut best = [f64::MAX; 2];
                    let (lost_before, written_before) = (reader.lost(), written);
                    for journaled in [false, true, false, true, false, true, false, true] {
                        let started = std::time::Instant::now();
                        std::thread::scope(|scope| {
                            for tid in 1..=threads {
                                scope.spawn(move || {
                                    for _ in 0..CALLS {
                                        work(rounds);
                                        let token = if journaled {
                                            perfr_frida_return_enter(generation, tid, 8, 16, 0x7000, 0)
                                        } else {
                                            0
                                        };
                                        work(rounds);
                                        if journaled {
                                            perfr_frida_return_leave(generation, tid, token);
                                        }
                                    }
                                });
                            }
                        });
                        if journaled {
                            written += u64::from(threads) * CALLS * 2;
                        }
                        let each = started.elapsed().as_nanos() as f64 / CALLS as f64;
                        best[usize::from(journaled)] = best[usize::from(journaled)].min(each);
                    }
                    println!(
                        "  enter + leave, {threads} thread(s), {:3.0} ns of other work a call: {:7.1} ns ({:.1}% of the records found no room)",
                        best[0],
                        best[1] - best[0],
                        100.0 * (reader.lost() - lost_before) as f64 / (written - written_before) as f64
                    );
                    assert!(best[1].is_finite());
                }
            }
            reading.store(false, Ordering::Relaxed);
            let read = read.join().unwrap();
            let lost = reader.lost();
            println!("  {read} records read, {lost} not written of {written}");
            assert!(read + lost <= written && read > 0);
        });
        perfr_frida_close(generation);
    }

    /// What the hook path adds to a hooked call, beyond the record it
    /// writes: the stubs above do nothing, so this is the session check and
    /// the call counter alone, on one thread and on eight.
    ///
    /// `cargo test -p perfr-lib --release what_a_hooked_call -- --nocapture`
    #[test]
    fn what_a_hooked_call_pays_to_reach_the_api() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        const CALLS: u64 = 400_000;
        let file = tempfile::NamedTempFile::new().unwrap();
        let reader = Transport::create(file.as_file(), std::process::id()).unwrap();
        let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
        let generation =
            unsafe { perfr_frida_open(path.as_ptr(), Some(init), Some(quiet_start), Some(quiet_stop)) };
        assert_ne!(generation, 0);
        let mut total = 0;
        for threads in [1u64, 8] {
            let best = (0..7)
                .map(|_| {
                    let started = std::time::Instant::now();
                    std::thread::scope(|scope| {
                        for _ in 0..threads {
                            scope.spawn(|| {
                                for _ in 0..CALLS {
                                    let h = unsafe { perfr_frida_start(generation, std::ptr::null(), 0) };
                                    perfr_frida_stop(generation, h);
                                }
                            });
                        }
                    });
                    total += threads * CALLS;
                    started.elapsed().as_nanos() as f64 / CALLS as f64
                })
                .fold(f64::MAX, f64::min);
            println!("  start + stop through the session, {threads} thread(s): {best:7.1} ns a call");
            assert!(best > 0.0 && best.is_finite());
        }
        perfr_frida_close(generation);
        assert_eq!(reader.calls(), total, "every completed call was counted");
    }

    #[cfg(unix)]
    #[test]
    fn a_segment_left_under_this_pid_by_a_dead_process_does_not_stop_hooks_arming() {
        use perfr_scope_ring::{ScopeRingReader, ScopeRingWriter};
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        let pid = std::process::id();
        let segment = perfr_scope_ring::shm::segment_directory().join(perfr_scope_ring::shm::shm_file_name(pid));
        // What a process that had this pid before left in /dev/shm when it
        // was killed: a segment that validates, and that nobody holds.
        {
            let writer = ScopeRingWriter::create(1, 8).unwrap();
            let bytes = std::fs::read(&segment).unwrap();
            drop(writer);
            std::fs::write(&segment, bytes).unwrap();
        }
        let leftover = ScopeRingReader::open(pid).expect("the leftover is a valid segment");
        assert!(!leftover.writer_alive());
        // Arming gives this process a real segment. The other tests here
        // stand in for the API with their own functions, which a segment
        // that publishes the real one would take precedence over.
        struct NoSegmentAfterwards;
        impl Drop for NoSegmentAfterwards {
            fn drop(&mut self) {
                perfr_api::shutdown();
            }
        }
        let _restore = NoSegmentAfterwards;

        let file = tempfile::NamedTempFile::new().unwrap();
        let _controller = Transport::create(file.as_file(), pid).unwrap();
        let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
        // The sweep cannot remove it, since the pid is alive: it is ours.
        // Taken for a segment some other SDK in the process owned, it made
        // every arm fail, this one and each one after.
        let generation = unsafe { perfr_frida_open(path.as_ptr(), None, None, None) };
        assert_ne!(generation, 0, "the hooks did not arm");
        let ours = ScopeRingReader::open(pid).unwrap();
        assert!(ours.writer_alive(), "the leftover was replaced by a segment this process holds");
        assert_ne!(ours.api_descriptor(), 0);
        perfr_frida_close(generation);
        let again = unsafe { perfr_frida_open(path.as_ptr(), None, None, None) };
        assert_ne!(again, 0, "and the next capture arms too");
        perfr_frida_close(again);
    }

    #[test]
    fn late_callbacks_cannot_write_to_the_next_capture() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        let file = tempfile::NamedTempFile::new().unwrap();
        let reader = Transport::create(file.as_file(), std::process::id()).unwrap();
        let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
        let open =
            || unsafe { perfr_frida_open(path.as_ptr(), Some(init), Some(start), Some(stop)) };
        let first = open();
        assert_ne!(first, 0);
        assert_eq!(open(), 0, "do not steal active capture");
        perfr_frida_close(first);
        let second = open();
        assert_ne!(second, first);
        perfr_frida_stop(first, 42);
        perfr_frida_close(first);
        let h = unsafe { perfr_frida_start(second, std::ptr::null(), 0) };
        assert_eq!(h, 42);
        perfr_frida_stop(second, h);
        perfr_frida_close(second);
        assert_eq!(STOPS.load(Ordering::Relaxed), 1);
        assert_eq!(reader.calls(), 1);
    }
}
