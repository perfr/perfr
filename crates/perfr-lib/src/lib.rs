// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Native lifecycle guard around the target’s perfr start/stop API.
#[cfg(feature = "native")]
mod native;
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
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    returns: std::sync::RwLock<Option<perfr_frida_transport::returns::Journal>>,
}

/// The capture hooks report to now, or null between captures.
///
/// The duration path loads a pointer, not a lock. Previously every hooked call on
/// every thread came through a process-wide `RwLock` twice, on entry and
/// on return: four atomic updates of one cache line per call, and a stop
/// waiting to close the session held every caller up behind it. With eight
/// threads in a hooked function that was 700 ns a call against 24 on one.
///
/// Sampled-return metadata takes a short separate read lock while publishing;
/// close takes its write lock to release the larger journal mapping safely.
/// Neither lock is held for the duration of the instrumented target call.
///
/// Each session is leaked, its 32-byte lease mapping with it, so a call
/// that loaded the pointer just before a close still reads memory that is
/// there. That is a page of address space for each capture of the process.
static SESSION: AtomicPtr<Session> = AtomicPtr::new(std::ptr::null_mut());

/// Serialises open and close, and remembers the last generation given out.
static GENERATIONS: Mutex<u32> = Mutex::new(0);

// An old call may return after Stop and after the next capture starts.
// Remember its trampoline so missing old history truncates instead of guessing.
// Accessed during capture transitions and first discovery of each hook.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
static KNOWN_RETURNS: Mutex<(Vec<u64>, bool)> = Mutex::new((Vec::new(), false));
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn retire_return_journal(session: &Session) {
    if let Ok(mut journal) = session.returns.write() {
        if let Some(journal) = journal.take() {
            if let Ok(mut known) = KNOWN_RETURNS.lock() {
                known.1 |= journal.unknown_return();
                for address in journal.trampolines() {
                    if !known.0.contains(&address) {
                        if known.0.len() == 1024 { known.1 = true; break; }
                        known.0.push(address);
                    }
                }
            }
        }
    }
}

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
        match perfr_frida_transport::returns::Journal::open(&file) { Ok(j) => Some(j), Err(_) => return 0 }
    } else { None };
    let Ok(mut last_generation) = GENERATIONS.lock() else {
        return 0;
    };
    // Refuse two collectors rather than silently stealing a running session.
    if session().is_some_and(|s| s.mapping.active() && s.mapping.controller_alive()) {
        return 0;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if let Some(old) = session() { retire_return_journal(old); }
        if let Some(journal) = returns.as_ref() {
            let Ok(known) = KNOWN_RETURNS.lock() else { return 0; };
            for &address in &known.0 { journal.register(address); }
            if known.1 { journal.invalidate(); }
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
    let session = Box::leak(Box::new(Session {
        generation: *last_generation,
        mapping,
        start: api.0,
        stop: api.1,
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        returns: std::sync::RwLock::new(returns),
    }));
    SESSION.store(session, Ordering::Release);
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
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            if let Some(current) = session() {
                if let Ok(journal) = current.returns.read() {
                    if let Some(journal) = journal.as_ref() { journal.close(); }
                }
            }
            let old = SESSION.swap(std::ptr::null_mut(), Ordering::AcqRel);
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            if let Some(old) = unsafe { old.as_ref() } {
                // Wait only for metadata publication, never for the target call.
                retire_return_journal(old);
            }
            #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
            let _ = old;
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

/// First discovery of a hook must survive a racing capture transition.
/// This cold path is serialized with open/close: seeding a new journal and
/// publishing SESSION cannot miss a late callback from the previous capture.
#[no_mangle]
pub extern "C" fn perfr_frida_register_return(trampoline: u64) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        if FORK_CHILD.load(Ordering::Relaxed) { return; }
        let Ok(_control) = GENERATIONS.lock() else { return; };
        let Ok(mut known) = KNOWN_RETURNS.lock() else { return; };
        if trampoline == 0 {
            known.1 = true;
        } else if !known.0.contains(&trampoline) {
            if known.0.len() < 1024 { known.0.push(trampoline); }
            else { known.1 = true; }
        }
        if let Some(current) = session() {
            if let Ok(journal) = current.returns.read() {
                if let Some(journal) = journal.as_ref() {
                    if known.1 { journal.invalidate(); }
                    else { journal.register(trampoline); }
                }
            }
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let _ = trampoline;
}

// Gum calls these before it replaces a return slot, and on return. A token
// belongs to one capture; a late return cannot close a newer invocation.
#[no_mangle]
pub extern "C" fn perfr_frida_return_enter(generation: u32, tid: u32, sp: u64, original: u64, trampoline: u64) -> u64 {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if !FORK_CHILD.load(Ordering::Relaxed) {
        if let Some(s) = session().filter(|s| s.generation == generation && s.mapping.active()) {
            if let Ok(journal) = s.returns.read() {
                if let Some(j) = journal.as_ref() { return j.enter(tid, sp, original, trampoline); }
            }
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let _ = (generation, tid, sp, original, trampoline);
    0
}
#[no_mangle]
pub extern "C" fn perfr_frida_return_leave(generation: u32, invocation: u64) {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if invocation != 0 && !FORK_CHILD.load(Ordering::Relaxed) {
        if let Some(s) = session().filter(|s| s.generation == generation && s.mapping.active()) {
            if let Ok(journal) = s.returns.read() { if let Some(j) = journal.as_ref() { j.leave(invocation); } }
        }
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let _ = (generation, invocation);
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

    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn first_return_discovery_survives_capture_boundaries() {
        let _one = ONE_SESSION.lock().unwrap_or_else(|p| p.into_inner());
        // A callback may discover its address with no active session at all.
        perfr_frida_register_return(0x123400);
        let mut previous = 0;
        for address in [0x123500, 0x123600] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let lease = Transport::create(file.as_file(), std::process::id()).unwrap();
            lease.enable_return_repair();
            let journal = perfr_frida_transport::returns::Journal::open(file.as_file()).unwrap();
            let path = std::ffi::CString::new(file.path().to_str().unwrap()).unwrap();
            let generation = unsafe { perfr_frida_open(path.as_ptr(), Some(init), Some(quiet_start), Some(quiet_stop)) };
            assert_ne!(generation, 0);
            assert!(journal.trampolines().contains(&0x123400));
            if previous != 0 { assert!(journal.trampolines().contains(&previous)); }
            // An old callback discovered after the new session was published.
            perfr_frida_register_return(address);
            assert!(journal.trampolines().contains(&address));
            assert_eq!(perfr_frida_return_enter(generation.wrapping_sub(1), 1, 8, 16, address), 0);
            perfr_frida_close(generation);
            assert!(journal.closed());
            previous = address;
        }
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
