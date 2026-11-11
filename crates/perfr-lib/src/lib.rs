// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Native lifecycle guard around the target’s perfr start/stop API.
#[cfg(feature = "native")]
mod native;
use perfr_frida_transport::Transport;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

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
    let Ok(mut last_generation) = GENERATIONS.lock() else {
        return 0;
    };
    // Refuse two collectors rather than silently stealing a running session.
    if session().is_some_and(|s| s.mapping.active() && s.mapping.controller_alive()) {
        return 0;
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
                // Never replace a manual segment whose API could not be resolved.
                // Subsequent captures reuse our already initialized API instance.
                static OWN_API: OnceLock<bool> = OnceLock::new();
                let initialized = *OWN_API.get_or_init(|| {
                    if perfr_scope_ring::ScopeRingReader::open(std::process::id()).is_ok() {
                        return false;
                    }
                    perfr_api::init().is_ok()
                });
                if !initialized {
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
