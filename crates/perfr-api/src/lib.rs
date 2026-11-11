// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! perfr manual instrumentation: the producer behind `include/perfr.h`.
//!
//! Thirteen functions, exported with a C ABI for every language that loads
//! the library, and offered natively for Rust. Every one is safe from any
//! thread at any time; before [`init`], and while nobody is capturing, each
//! is a load and a predictable branch.
//!
//! What a call costs is set by [`perfr_scope_ring`]: one `fetch_add`, one
//! compare-exchange, plain stores, one release store, about twenty
//! nanoseconds with the clock. What
//! this crate adds on top is the per-thread state -- the thread id read once
//! and cached, since `gettid` is a 60 ns syscall and would otherwise be four
//! times the cost of the scope it stamps -- and the handle arithmetic.
//!
//! Nesting depth is deliberately *not* tracked here. The reader works it out
//! from the order of starts and stops on a thread, so a scope that is never
//! stopped costs nothing but itself instead of skewing everything after it.

use perfr_scope_ring::event::{flags, kind};
use perfr_scope_ring::shm::now_monotonic_ns;
use perfr_scope_ring::text::continuations;
use perfr_scope_ring::{ring_for_thread, ScopeEvent, ScopeRingWriter};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

pub use perfr_scope_ring::event::level;

/// Runs in the child of a `fork`, on its one thread.
///
/// The child inherits the parent's segment, mapped and looking ready, and
/// must not write to it: its records would appear under the parent's pid
/// and the forking thread's cached id. So it starts as a process that has
/// never called [`init`], every call a no-op, and is given a segment of its
/// own when it does call it. An earlier version switched the child off for
/// good instead, and a server that initialised and then daemonised, a
/// prefork worker and a Python `multiprocessing` child all ran
/// uninstrumented with nothing to say why.
///
/// Only what a child of a threaded process may do here: stores, a `close`
/// and a `munmap`, and a thread-local that needs no initialising.
#[cfg(unix)]
unsafe extern "C" fn after_fork() {
    if let Some(writer) = SEGMENT.swap(std::ptr::null_mut(), Ordering::AcqRel).as_ref() {
        // The lease descriptor and the mapping came across the fork, and
        // either holds the parent's lock for as long as it stays here. The
        // pointer is gone first: nothing in this process can reach the
        // writer again.
        writer.abandon_in_fork_child();
    }
    UNLINKED.store(false, Ordering::Release);
    // A thread that held the lock in the parent does not exist here.
    INIT_BUSY.store(false, Ordering::Release);
    // The forking thread has a new id, and its counter belongs to handles
    // that were given out in the parent.
    let _ = THREAD.try_with(|cell| cell.set(None));
}

/// Process-local ABI for an injected caller. Published by init, so even
/// statically linked, stripped applications reuse their exact API instance.
#[repr(C)]
pub struct DynamicApiV1 {
    pub version: u64,
    pub start: unsafe extern "C" fn(*const libc::c_char, usize) -> u64,
    pub stop: extern "C" fn(u64),
}
static DYNAMIC_API: DynamicApiV1 = DynamicApiV1 {
    version: 1, start: perfr_start_dynamic, stop: perfr_stop,
};

/// A handle to an event this process recorded. See `perfr.h`.
pub type Handle = u64;

/// The process's segment, once [`init`] has created it. A raw pointer so the
/// hot path is one relaxed load and a null check, and so shutdown can be an
/// ordinary store.
static SEGMENT: AtomicPtr<ScopeRingWriter> = AtomicPtr::new(std::ptr::null_mut());

#[inline]
fn segment() -> Option<&'static ScopeRingWriter> {
    let p = SEGMENT.load(Ordering::Acquire);
    // SAFETY: only `init` stores a non-null pointer, to a leaked Box that is
    // never freed; `shutdown` unlinks the name but leaves the mapping alive
    // for any thread still mid-call.
    unsafe { p.as_ref() }
}

/// The segment, but only when a service is actually draining it.
///
/// This is the gate every call passes first. When nobody is capturing it is a
/// pointer load, a relaxed load of the capturing flag, and a return -- so an
/// instrumented program that ships with perfr linked in pays almost nothing
/// until the moment someone captures it.
#[inline]
fn active_segment() -> Option<&'static ScopeRingWriter> {
    let writer = segment()?;
    writer.is_capturing().then_some(writer)
}

/// Whether a service is draining this process right now.
///
/// Every call below already checks this; it is for a caller that would do
/// work only to feed one of them -- format a log line, walk a span's
/// fields -- and can skip it when nobody is capturing.
pub fn capturing() -> bool {
    active_segment().is_some()
}

/// Serialises `init`. A compare-exchange on the pointer is not enough:
/// creating a segment unlinks any existing one of the same name first, so two
/// threads initialising at once would have the loser destroy the winner's
/// segment on the way in and unlink it again on the way out. The lock is
/// taken only on the init path, never on the hot one.
///
/// A flag and not a `Mutex`, because a forked child has to be able to take
/// it: a mutex another thread held at the moment of the fork stays locked in
/// the child for ever, and nothing can be done to it from there. A flag the
/// child's fork handler simply clears.
static INIT_BUSY: AtomicBool = AtomicBool::new(false);

struct InitLock;

impl InitLock {
    fn take() -> InitLock {
        while INIT_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::thread::yield_now();
        }
        InitLock
    }
}

impl Drop for InitLock {
    fn drop(&mut self) {
        INIT_BUSY.store(false, Ordering::Release);
    }
}

/// Whether the fork and exit handlers are registered: once per process,
/// under the init lock, and inherited by a forked child along with the
/// registrations themselves.
#[cfg(unix)]
static HANDLERS: AtomicBool = AtomicBool::new(false);

/// Set by [`shutdown`]: the segment's name is gone, so no capture that
/// starts from here on can find this process, though the segment is still
/// mapped and written. [`init`] has work to do again.
static UNLINKED: AtomicBool = AtomicBool::new(false);

/// Whether this process has a segment a service can find by its name.
fn findable() -> bool {
    !SEGMENT.load(Ordering::Acquire).is_null() && !UNLINKED.load(Ordering::Acquire)
}

/// Creates this process's segment. Idempotent; returns `Err(errno)` if the
/// segment could not be made. Safe to call from several threads at once.
///
/// After [`shutdown`] it makes a new one. The old segment's name cannot be
/// put back, so the old segment stays mapped for any thread still inside a
/// call, marked as superseded for a capture still reading it, and is
/// written to no more.
pub fn init() -> Result<(), i32> {
    if findable() {
        return Ok(());
    }
    let _guard = InitLock::take();
    if findable() {
        return Ok(());
    }
    // Windows has no fork, so there is nothing to disarm in a child there,
    // and deletes the segment's file itself when its last handle closes.
    #[cfg(unix)]
    if !HANDLERS.load(Ordering::Relaxed) {
        // SAFETY: registering handlers that only touch statics.
        let error = unsafe { libc::pthread_atfork(None, None, Some(after_fork)) };
        if error != 0 { return Err(error); }
        // The writer is leaked, so its `Drop`, the one place the name was
        // removed without being asked, never runs. A refusal (the table is
        // full) leaves the segment to the service's sweep.
        // SAFETY: as above.
        unsafe { libc::atexit(unlink_at_exit) };
        HANDLERS.store(true, Ordering::Relaxed);
    }
    // A second SDK initialized after injection must not unlink the active
    // instance. Initialize the application's SDK before attaching Frida.
    if perfr_scope_ring::ScopeRingReader::open(std::process::id())
        .is_ok_and(|r| r.api_descriptor() != 0) { return Err(libc::EALREADY); }
    let writer = ScopeRingWriter::create_default().map_err(|e| e.raw_os_error().unwrap_or(-1))?;
    let writer = Box::leak(Box::new(writer));
    let replaced = SEGMENT.swap(writer, Ordering::AcqRel);
    writer.publish_api_descriptor(&DYNAMIC_API as *const DynamicApiV1 as u64);
    UNLINKED.store(false, Ordering::Release);
    // SAFETY: a non-null pointer here is a writer an earlier init leaked.
    if let Some(replaced) = unsafe { replaced.as_ref() } {
        // Last, so a reader that sees the mark finds the new segment there.
        replaced.mark_superseded();
        // Its descriptor is closed so that one is not left open per
        // re-initialisation; the mapping stays.
        // SAFETY: the writer is leaked, and this is the one time it is
        // replaced. Windows keeps the handle: closing it there is what
        // deletes the file, under any thread still inside a call.
        #[cfg(unix)]
        unsafe {
            replaced.release_lease();
        }
    }
    Ok(())
}

/// Removes the segment's name when the process exits by returning from
/// `main` or calling `exit`. A process that is killed, or leaves through
/// `_exit`, still leaves its segment to the sweep a service runs.
///
/// A forked child that exits runs this too. It has no writer unless it made
/// one of its own; the pid is compared all the same, so that a child made
/// some other way than `fork` cannot remove the name its parent is found by.
#[cfg(unix)]
extern "C" fn unlink_at_exit() {
    if let Some(writer) = segment() {
        if writer.pid() == std::process::id() {
            writer.unlink();
        }
    }
}

/// Removes the segment's name. The mapping stays until the process exits, so
/// a thread mid-call never dereferences a freed writer.
pub fn shutdown() {
    // Under the lock, so it cannot remove the name of a segment an `init`
    // on another thread is in the middle of making.
    let _guard = InitLock::take();
    if let Some(writer) = segment() {
        writer.unlink();
        UNLINKED.store(true, Ordering::Release);
    }
}

/// Per-thread state, built on the thread's first call.
#[derive(Clone, Copy)]
struct ThreadState {
    tid: u32,
    ring: usize,
    /// Which of the process's recording threads this is, counting from
    /// one: the top half of its handles.
    instance: u32,
    counter: u32,
}

/// How many threads have recorded so far.
static THREAD_INSTANCES: AtomicU32 = AtomicU32::new(0);

impl ThreadState {
    /// The state of a thread whose id is `raw_tid`, as it records for the
    /// first time.
    fn new(raw_tid: u64, ring_count: usize) -> ThreadState {
        ThreadState {
            tid: raw_tid as u32,
            ring: ring_for_thread(raw_tid, ring_count),
            instance: THREAD_INSTANCES.fetch_add(1, Ordering::Relaxed).wrapping_add(1),
            counter: 0,
        }
    }

    /// The thread's next handle.
    ///
    /// The top half says which thread, and is not the thread id: the
    /// operating system gives an id out again once its thread has exited,
    /// and the new thread's counter starts again at one, so the two halves
    /// together came round to handles the old thread had issued and might
    /// still have open. Nothing reads a thread out of a handle; a record
    /// carries its thread id in a field of its own.
    #[inline]
    fn bump(&mut self) -> Handle {
        // Zero is "no event", so the counter starts at one.
        self.counter = self.counter.wrapping_add(1).max(1);
        (u64::from(self.instance) << 32) | u64::from(self.counter)
    }
}

thread_local! {
    static THREAD: Cell<Option<ThreadState>> = const { Cell::new(None) };
}

#[inline]
fn with_thread<R>(ring_count: usize, f: impl FnOnce(&ThreadState) -> R) -> R {
    THREAD.with(|cell| {
        let state = cell.get().unwrap_or_else(|| {
            // The one OS call, paid once per thread rather than per scope.
            let state = ThreadState::new(perfr_scope_ring::platform::thread_id(), ring_count);
            cell.set(Some(state));
            state
        });
        f(&state)
    })
}

/// A fresh handle for the calling thread, whose state is `state`.
///
/// The counter is read from the thread's cell and put back at once, before
/// anything is pushed. Kept in a copy until the call had finished, it gave
/// a signal handler that recorded a scope while its thread was inside a
/// call the handle that call was about to use.
#[inline]
fn next_handle(state: &ThreadState) -> Handle {
    THREAD.with(|cell| {
        let mut current = cell.get().unwrap_or(*state);
        let handle = current.bump();
        cell.set(Some(current));
        handle
    })
}

fn push_named(writer: &ScopeRingWriter, state: &ThreadState, mut head: ScopeEvent, name: &[u8]) {
    let rest = continuations(&mut head, name);
    let rings = writer.rings();
    rings.push(state.ring, head);
    for chunk in rest {
        rings.push(state.ring, chunk);
    }
}

/// The clock, for a record that is going to be written.
#[inline]
fn clock_ns() -> u64 {
    #[cfg(test)]
    CLOCK_READS.with(|reads| reads.set(reads.get() + 1));
    now_monotonic_ns()
}

#[cfg(test)]
thread_local! {
    /// How often this thread has read the clock through [`clock_ns`].
    static CLOCK_READS: Cell<u64> = const { Cell::new(0) };
}

/// `timestamp_ns` is the caller's, or `None` for now. The clock is read
/// here, behind the gate, and not by the caller on its way in: it is most of
/// what a scope costs, and read first it was paid on every start and stop of
/// a program nobody was capturing.
fn start_at(flag: u8, name: &[u8], timestamp_ns: Option<u64>) -> Handle {
    let Some(writer) = active_segment() else { return 0 };
    let timestamp_ns = timestamp_ns.unwrap_or_else(clock_ns);
    with_thread(writer.rings().ring_count(), |state| {
        let handle = next_handle(state);
        let head = ScopeEvent {
            timestamp_ns,
            scope_id: handle,
            tid: state.tid,
            kind: kind::SCOPE_START,
            flags: flag,
            ..ScopeEvent::default()
        };
        push_named(writer, state, head, name);
        handle
    })
}

fn stop_at(handle: Handle, timestamp_ns: Option<u64>) {
    if handle == 0 {
        return;
    }
    let Some(writer) = active_segment() else { return };
    let timestamp_ns = timestamp_ns.unwrap_or_else(clock_ns);
    with_thread(writer.rings().ring_count(), |state| {
        writer.rings().push(
            state.ring,
            ScopeEvent {
                timestamp_ns,
                scope_id: handle,
                tid: state.tid,
                kind: kind::SCOPE_STOP,
                ..ScopeEvent::default()
            },
        );
    });
}

/// Begins a scope on the calling thread. Takes `&str` or `&[u8]`.
pub fn start(name: impl AsRef<[u8]>) -> Handle {
    start_at(0, name.as_ref(), None)
}

/// Begins a scope attributed to dynamic instrumentation, using the same
/// handles, segment and nesting stream as manual scopes.
pub fn start_dynamic(name: impl AsRef<[u8]>) -> Handle {
    start_at(flags::DYNAMIC, name.as_ref(), None)
}

/// Begins a scope that may be stopped from any thread.
pub fn start_async(name: impl AsRef<[u8]>) -> Handle {
    start_at(flags::ASYNC, name.as_ref(), None)
}

/// Records a complete scope whose timestamps the caller supplies, rather than
/// reading the clock now.
///
/// For events that already happened at a time you captured elsewhere: GPU work
/// whose timestamps you read back after the fact, a trace being replayed, or
/// events you buffered yourself and flush in a batch. Timestamps must be
/// [`now_ns`]'s clock -- `CLOCK_MONOTONIC` nanoseconds -- so they line up with
/// everything else on the timeline.
///
/// Nesting depth still comes from the order scopes are emitted, so for nested
/// imported data, emit parents around their children (outer span, then the
/// inner spans within it). Flat data needs no such care.
pub fn span(name: impl AsRef<[u8]>, start_ns: u64, end_ns: u64) {
    let handle = start_at(0, name.as_ref(), Some(start_ns));
    stop_at(handle, Some(end_ns));
}

/// A complete async span at supplied timestamps, drawn on its own track.
/// The right one for GPU spans: independent of any CPU thread's nesting.
pub fn span_async(name: impl AsRef<[u8]>, start_ns: u64, end_ns: u64) {
    let handle = start_at(flags::ASYNC, name.as_ref(), Some(start_ns));
    stop_at(handle, Some(end_ns));
}

/// Ends a scope, from any thread. `0` is a no-op.
pub fn stop(handle: Handle) {
    stop_at(handle, None);
}

/// `CLOCK_MONOTONIC` in nanoseconds, the clock every timestamp in the segment
/// uses. Grab it at the real site of an event so a later [`span`] lines up
/// with scheduling, samples and other scopes.
pub fn now_ns() -> u64 {
    now_monotonic_ns()
}

/// A point in time with a name and no duration.
pub fn instant(name: impl AsRef<[u8]>) -> Handle {
    let name = name.as_ref();
    let Some(writer) = active_segment() else { return 0 };
    with_thread(writer.rings().ring_count(), |state| {
        let handle = next_handle(state);
        let head = ScopeEvent {
            timestamp_ns: clock_ns(),
            scope_id: handle,
            tid: state.tid,
            kind: kind::INSTANT,
            ..ScopeEvent::default()
        };
        push_named(writer, state, head, name);
        handle
    })
}

/// An arrow from one event to another. Either handle being `0` is a no-op.
pub fn link(from: Handle, to: Handle) {
    if from == 0 || to == 0 {
        return;
    }
    let Some(writer) = active_segment() else { return };
    with_thread(writer.rings().ring_count(), |state| {
        let mut event = ScopeEvent {
            timestamp_ns: clock_ns(),
            scope_id: from,
            tid: state.tid,
            kind: kind::LINK,
            text_len: 8,
            ..ScopeEvent::default()
        };
        event.text[..8].copy_from_slice(&to.to_le_bytes());
        writer.rings().push(state.ring, event);
    });
}

/// A value to graph over time on a track named `name`.
pub fn value(name: impl AsRef<[u8]>, value: f64) {
    let name = name.as_ref();
    let Some(writer) = active_segment() else { return };
    with_thread(writer.rings().ring_count(), |state| {
        let head = ScopeEvent {
            timestamp_ns: clock_ns(),
            scope_id: value.to_bits(),
            tid: state.tid,
            kind: kind::VALUE,
            ..ScopeEvent::default()
        };
        push_named(writer, state, head, name);
    });
}

/// A scope that stops when dropped. `let _s = perfr_api::scope("update");`
pub struct Scope(Handle);

impl Scope {
    pub fn handle(&self) -> Handle {
        self.0
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        stop(self.0);
    }
}

pub fn scope(name: impl AsRef<[u8]>) -> Scope {
    Scope(start(name))
}

pub fn scope_async(name: impl AsRef<[u8]>) -> Scope {
    Scope(start_async(name))
}

// ------------------------------------------------------------------ logs --

/// Records a log line on the calling thread.
///
/// A no-op, one branch, when nobody is capturing. The line goes into the
/// same ring as scopes and is written exactly like a scope's name: the
/// first 32 bytes in the record, the rest as continuations, cut at 8 KiB
/// with the cut marked. It carries the level, the thread and the timestamp,
/// and nothing that names a scope: the viewer files it under the scopes
/// open on this thread at that time, so a scope is the way to group lines.
pub fn log(level: u8, message: impl AsRef<[u8]>) {
    let Some(writer) = active_segment() else { return };
    with_thread(writer.rings().ring_count(), |state| {
        let head = ScopeEvent {
            timestamp_ns: clock_ns(),
            scope_id: next_handle(state),
            tid: state.tid,
            kind: kind::LOG,
            flags: level::to_flags(level),
            ..ScopeEvent::default()
        };
        push_named(writer, state, head, message.as_ref());
    });
}

pub fn log_trace(message: impl AsRef<[u8]>) { log(level::TRACE, message); }
pub fn log_debug(message: impl AsRef<[u8]>) { log(level::DEBUG, message); }
pub fn log_info(message: impl AsRef<[u8]>) { log(level::INFO, message); }
pub fn log_warn(message: impl AsRef<[u8]>) { log(level::WARN, message); }
pub fn log_error(message: impl AsRef<[u8]>) { log(level::ERROR, message); }
pub fn log_fatal(message: impl AsRef<[u8]>) { log(level::FATAL, message); }

// ---------------------------------------------------------------- C ABI --

/// # Safety
/// `name` must point at `name_len` readable bytes, or `name_len` must be 0.
unsafe fn bytes<'a>(name: *const libc::c_char, name_len: usize) -> &'a [u8] {
    if name.is_null() || name_len == 0 {
        return &[];
    }
    std::slice::from_raw_parts(name.cast::<u8>(), name_len)
}

#[no_mangle]
pub extern "C" fn perfr_init() -> libc::c_int {
    match init() {
        Ok(()) => 0,
        Err(errno) => -errno.abs(),
    }
}

#[no_mangle]
pub extern "C" fn perfr_shutdown() {
    shutdown();
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_start(name: *const libc::c_char, name_len: usize) -> u64 {
    start(bytes(name, name_len))
}

/// # Safety
/// See [`bytes`]. Kept as an explicit entry point for injected instrumentation.
#[no_mangle]
pub unsafe extern "C" fn perfr_start_dynamic(name: *const libc::c_char, name_len: usize) -> u64 {
    start_dynamic(bytes(name, name_len))
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_start_async(name: *const libc::c_char, name_len: usize) -> u64 {
    start_async(bytes(name, name_len))
}

#[no_mangle]
pub extern "C" fn perfr_stop(handle: u64) {
    stop(handle);
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_instant(name: *const libc::c_char, name_len: usize) -> u64 {
    instant(bytes(name, name_len))
}

#[no_mangle]
pub extern "C" fn perfr_link(from: u64, to: u64) {
    link(from, to);
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_value(name: *const libc::c_char, name_len: usize, v: f64) {
    value(bytes(name, name_len), v);
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_span(
    name: *const libc::c_char,
    name_len: usize,
    start_ns: u64,
    end_ns: u64,
) {
    span(bytes(name, name_len), start_ns, end_ns);
}

/// # Safety
/// See [`bytes`].
#[no_mangle]
pub unsafe extern "C" fn perfr_span_async(
    name: *const libc::c_char,
    name_len: usize,
    start_ns: u64,
    end_ns: u64,
) {
    span_async(bytes(name, name_len), start_ns, end_ns);
}

#[no_mangle]
pub extern "C" fn perfr_now_ns() -> u64 {
    now_ns()
}

/// # Safety
/// `message` must point at `message_len` readable bytes, or `message_len` must be 0.
#[no_mangle]
pub unsafe extern "C" fn perfr_log(level: u8, message: *const libc::c_char, message_len: usize) {
    log(level, bytes(message, message_len));
}

// ------------------------------------------------------- loadable table --

/// The ABI the single-header `perfr.h` speaks. Bumped only when a field is
/// added, removed or changes meaning; a header built against another version
/// gets a null table and stays no-op rather than calling through a wrong
/// layout.
pub const API_ABI_VERSION: u32 = 1;

/// Every entry point, as a table a header-only caller fills once.
///
/// `perfr.h` in its default (dynamic) mode never links this library: it
/// `dlopen`s it, asks [`perfr_api_table_v1`] for this struct, and dispatches
/// through it. That keeps the ring protocol out of every compiled
/// application -- the library that ships beside `perfr` is the one
/// implementation, and it always matches the service it came with.
#[repr(C)]
pub struct ApiTableV1 {
    /// `API_ABI_VERSION` of the library that built the table.
    pub abi_version: u32,
    /// `size_of::<ApiTableV1>()`, so a newer header can see an older table.
    pub size: u32,
    pub init: extern "C" fn() -> libc::c_int,
    pub shutdown: extern "C" fn(),
    pub start: unsafe extern "C" fn(*const libc::c_char, usize) -> u64,
    pub start_dynamic: unsafe extern "C" fn(*const libc::c_char, usize) -> u64,
    pub start_async: unsafe extern "C" fn(*const libc::c_char, usize) -> u64,
    pub stop: extern "C" fn(u64),
    pub instant: unsafe extern "C" fn(*const libc::c_char, usize) -> u64,
    pub link: extern "C" fn(u64, u64),
    pub value: unsafe extern "C" fn(*const libc::c_char, usize, f64),
    pub now_ns: extern "C" fn() -> u64,
    pub span: unsafe extern "C" fn(*const libc::c_char, usize, u64, u64),
    pub span_async: unsafe extern "C" fn(*const libc::c_char, usize, u64, u64),
    /// Appended after the first release of this table. A header reads it
    /// only when `size` covers it, so every field above this line is the
    /// part an older library is guaranteed to have.
    pub log: unsafe extern "C" fn(u8, *const libc::c_char, usize),
}

static API_TABLE: ApiTableV1 = ApiTableV1 {
    abi_version: API_ABI_VERSION,
    size: std::mem::size_of::<ApiTableV1>() as u32,
    init: perfr_init,
    shutdown: perfr_shutdown,
    start: perfr_start,
    start_dynamic: perfr_start_dynamic,
    start_async: perfr_start_async,
    stop: perfr_stop,
    instant: perfr_instant,
    link: perfr_link,
    value: perfr_value,
    now_ns: perfr_now_ns,
    span: perfr_span,
    span_async: perfr_span_async,
    log: perfr_log,
};

/// The table for `abi_version`, or null when this library does not speak
/// it. The one symbol `perfr.h` looks up after loading the library; the
/// table is static and lives as long as the mapping.
#[no_mangle]
pub extern "C" fn perfr_api_table_v1(abi_version: u32) -> *const ApiTableV1 {
    if abi_version == API_ABI_VERSION { &API_TABLE } else { std::ptr::null() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perfr_scope_ring::merge::{drain, Cursors};
    use perfr_scope_ring::text::{Completeness, TextAssembler};
    use perfr_scope_ring::ScopeRingReader;

    #[test]
    fn the_table_is_handed_out_only_for_its_own_abi() {
        let table = perfr_api_table_v1(API_ABI_VERSION);
        assert!(!table.is_null());
        let table = unsafe { &*table };
        assert_eq!(table.abi_version, API_ABI_VERSION);
        assert_eq!(table.size as usize, std::mem::size_of::<ApiTableV1>());
        // The table as first released ends where `log` begins. perfr.h loads
        // any library at least that long (PERFR_API_TABLE_BASE_SIZE_), so
        // nothing may ever be inserted above `log`, only appended below it.
        assert_eq!(
            std::mem::offset_of!(ApiTableV1, log),
            8 + 12 * std::mem::size_of::<usize>()
        );
        assert_eq!((table.now_ns)() > 0, true);
        assert!(perfr_api_table_v1(API_ABI_VERSION + 1).is_null());
        assert!(perfr_api_table_v1(0).is_null());
    }

    /// `perfr.h` declares the table by hand, and a C program reads the
    /// library's through that declaration. The one assertion above pins the
    /// offset of one field as arithmetic; this asks a C compiler what the
    /// header really says, for every field.
    #[cfg(unix)]
    #[test]
    fn the_c_header_lays_the_table_out_as_the_library_does() {
        use std::mem::{offset_of, size_of};
        let dir = std::env::temp_dir().join(format!("perfr-api-table-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fields = [
            "abi_version", "size", "init", "shutdown", "start", "start_dynamic", "start_async", "stop",
            "instant", "link", "value", "now_ns", "span", "span_async", "log",
        ];
        let mut program = String::from("#include <stdio.h>\n#include \"perfr.h\"\nint main(void) {\n");
        for field in fields {
            program += &format!(
                "  printf(\"{field} %zu\\n\", offsetof(struct perfr_api_table_v1, {field}));\n"
            );
        }
        program += "  printf(\"sizeof %zu\\n\", sizeof(struct perfr_api_table_v1));\n";
        program += "  printf(\"base %zu\\n\", (size_t)PERFR_API_TABLE_BASE_SIZE_);\n";
        program += "  printf(\"abi %u\\n\", PERFR_API_ABI_VERSION);\n  return 0;\n}\n";
        let source = dir.join("table.c");
        let binary = dir.join("table");
        std::fs::write(&source, program).unwrap();
        let include = concat!(env!("CARGO_MANIFEST_DIR"), "/include");
        let compiled = std::process::Command::new("cc")
            .args(["-std=c11", "-Wall", "-Werror", "-I", include, "-o"])
            .arg(&binary)
            .arg(&source)
            .output();
        let compiled = match compiled {
            Ok(output) => output,
            Err(error) => {
                // A machine with no C compiler cannot ask the question.
                eprintln!("skipped: cc did not run ({error})");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        assert!(compiled.status.success(), "perfr.h did not compile: {}", String::from_utf8_lossy(&compiled.stderr));
        let printed = std::process::Command::new(&binary).output().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let in_c: std::collections::HashMap<String, usize> = String::from_utf8(printed.stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (name, number) = line.split_once(' ').unwrap();
                (name.to_string(), number.parse().unwrap())
            })
            .collect();
        let in_rust = [
            offset_of!(ApiTableV1, abi_version), offset_of!(ApiTableV1, size), offset_of!(ApiTableV1, init),
            offset_of!(ApiTableV1, shutdown), offset_of!(ApiTableV1, start), offset_of!(ApiTableV1, start_dynamic),
            offset_of!(ApiTableV1, start_async), offset_of!(ApiTableV1, stop), offset_of!(ApiTableV1, instant),
            offset_of!(ApiTableV1, link), offset_of!(ApiTableV1, value), offset_of!(ApiTableV1, now_ns),
            offset_of!(ApiTableV1, span), offset_of!(ApiTableV1, span_async), offset_of!(ApiTableV1, log),
        ];
        for (field, offset) in fields.iter().zip(in_rust) {
            assert_eq!(in_c[*field], offset, "perfr.h puts `{field}` somewhere else");
        }
        assert_eq!(in_c["sizeof"], size_of::<ApiTableV1>());
        assert_eq!(in_c["base"], offset_of!(ApiTableV1, log), "the size every library is required to have");
        assert_eq!(in_c["abi"], API_ABI_VERSION as usize);
    }

    fn all_events() -> Vec<ScopeEvent> {
        let reader = ScopeRingReader::open(std::process::id()).expect("segment exists");
        let rings = reader.rings();
        let mut cursors = Cursors::for_rings(rings.ring_count());
        let pass = drain(rings, &mut cursors, now_monotonic_ns());
        pass.slices.into_iter().flat_map(|s| s.events).collect()
    }

    /// A second copy of this test binary running one test, for what only a
    /// process of its own can show: what is left behind when it exits.
    /// `role` is how that test knows it is the child and not an ordinary run.
    #[cfg(unix)]
    fn run_as_child(test: &str, role: &str) -> (u32, std::process::ExitStatus) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--test-threads=1", "--nocapture"])
            .env(CHILD_ROLE, role)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        (pid, child.wait().unwrap())
    }

    #[cfg(unix)]
    const CHILD_ROLE: &str = "PERFR_API_TEST_CHILD";

    #[cfg(unix)]
    fn child_role_is(role: &str) -> bool {
        std::env::var(CHILD_ROLE).is_ok_and(|r| r == role)
    }

    fn segment_file(pid: u32) -> std::path::PathBuf {
        perfr_scope_ring::shm::segment_directory().join(perfr_scope_ring::shm::shm_file_name(pid))
    }

    #[cfg(unix)]
    #[test]
    fn child_that_inits_and_returns_from_main() {
        if !child_role_is("exit") {
            return;
        }
        init().expect("init");
        assert!(segment_file(std::process::id()).exists(), "the child has a segment while it runs");
    }

    #[cfg(unix)]
    #[test]
    fn a_process_that_exits_normally_takes_its_segment_with_it() {
        // Nothing removed a segment unless the program called
        // `perfr_shutdown()`, and most do not: every run left its file in
        // /dev/shm for the next capture on the machine to sweep.
        let (pid, status) = run_as_child("tests::child_that_inits_and_returns_from_main", "exit");
        assert!(status.success(), "the child failed: {status}");
        assert!(!segment_file(pid).exists(), "{} was left behind", segment_file(pid).display());
    }

    /// The process `a_forked_child_starts_inert_and_is_instrumented_again_by_init`
    /// runs. It forks, and its exit status is its child's verdict.
    #[cfg(unix)]
    #[test]
    fn child_that_forks_a_child_of_its_own() {
        if !child_role_is("fork") {
            return;
        }
        init().expect("init");
        let parent = std::process::id();
        let service = ScopeRingReader::open(parent).unwrap();
        service.set_capturing(true);
        let in_parent = start(b"in the parent");
        assert_ne!(in_parent, 0);
        let my_ring = ring_for_thread(perfr_scope_ring::platform::thread_id(), service.rings().ring_count());
        let written = service.rings().write_cursor(my_ring);

        // SAFETY: fork in a process whose other thread is the idle test
        // harness. The child only runs the checks below and exits.
        let forked = unsafe { libc::fork() };
        assert!(forked >= 0, "fork failed");
        if forked == 0 {
            let verdict = (|| -> Result<(), &'static str> {
                let me = std::process::id();
                // Its parent's rings are mapped here too, with a reader
                // capturing them: nothing may go into them from this side.
                if start(b"in the child, before init") != 0 || capturing() {
                    return Err("the child wrote into its parent's segment");
                }
                // A lock on a file lasts as long as anything refers to the
                // open file, and a mapping does. While the child maps its
                // parent's segment the parent's lease is held, by a process
                // that may outlive it by days: a daemon's.
                // (The writer's mapping is the writable one of the whole
                // segment. The reader this test opened in the parent came
                // across too, read-only but for one page, on an open file
                // of its own that carries no lock.)
                #[cfg(target_os = "linux")]
                {
                    let name = perfr_scope_ring::shm::shm_file_name(parent);
                    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|_| "no /proc/self/maps")?;
                    let writable_bytes = |line: &str| -> Option<u64> {
                        let mut fields = line.split_whitespace();
                        let (start, end) = fields.next()?.split_once('-')?;
                        let writable = fields.next()?.starts_with("rw");
                        let bytes = u64::from_str_radix(end, 16).ok()? - u64::from_str_radix(start, 16).ok()?;
                        writable.then_some(bytes)
                    };
                    if maps.lines().filter(|line| line.contains(&name)).filter_map(writable_bytes).any(|n| n > 65536) {
                        return Err("the child still maps its parent's segment for writing");
                    }
                }
                init().map_err(|_| "init failed in the child")?;
                if !segment_file(me).exists() {
                    return Err("the child has no segment of its own");
                }
                let service = ScopeRingReader::open(me).map_err(|_| "the child's segment does not open")?;
                service.set_capturing(true);
                let handle = start(b"in the child");
                stop(handle);
                let events: Vec<ScopeEvent> = {
                    let mut cursors = Cursors::for_rings(service.rings().ring_count());
                    drain(service.rings(), &mut cursors, 0).slices.into_iter().flat_map(|s| s.events).collect()
                };
                // The forking thread is the child's only thread, and its
                // id is the child's pid, not what the parent had cached.
                if events.iter().filter(|e| e.scope_id == handle && e.tid == me).count() != 2 {
                    return Err("the child's scope is not in its segment under its own thread id");
                }
                Ok(())
            })();
            if let Err(what) = verdict {
                eprintln!("forked child: {what}");
            }
            // `exit`, not `_exit`: the handler that removes a segment at
            // exit runs, and must take the child's and leave the parent's.
            // SAFETY: ends this process.
            unsafe { libc::exit(i32::from(verdict.is_err())) }
        }
        let mut status = 0;
        // SAFETY: waiting for the child forked above.
        assert_eq!(unsafe { libc::waitpid(forked, &mut status, 0) }, forked);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "the forked child failed");
        assert!(!segment_file(forked as u32).exists(), "the child's segment went with it");
        assert!(segment_file(parent).exists(), "the child's exit removed its parent's segment");
        assert_eq!(
            service.rings().write_cursor(my_ring),
            written,
            "the child wrote into its parent's rings"
        );
        stop(in_parent);
    }

    #[cfg(unix)]
    #[test]
    fn a_forked_child_starts_inert_and_is_instrumented_again_by_init() {
        // A server that initialises and then daemonises, a prefork worker,
        // Python multiprocessing with the fork start method: each ran with
        // instrumentation off for good, init() answering ENOTSUP.
        let (_, status) = run_as_child("tests::child_that_forks_a_child_of_its_own", "fork");
        assert!(status.success(), "{status}");
    }

    /// One segment per process, and these tests share a process, so the
    /// ones that write are serialised.
    static WRITES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_zero_handle_is_inert_everywhere() {
        // Zero is what every call returns while profiling is off; every
        // function must accept it and do nothing, initialised or not.
        stop(0);
        link(0, 0);
        link(0, 7);
        link(7, 0);
        if segment().is_none() {
            assert_eq!(start(b"x"), 0);
            assert_eq!(instant(b"x"), 0);
            value(b"x", 1.0);
        }
    }

    #[test]
    fn init_from_many_threads_at_once_leaves_one_working_segment() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| init().expect("init"));
            }
        });
        // The segment must still be openable: a lost race must not have
        // unlinked the winner's file.
        let reader = ScopeRingReader::open(std::process::id()).expect("segment survives racing inits");
        assert_eq!(reader.pid(), std::process::id());
    }

    #[test]
    fn every_api_writes_the_record_it_promises() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        // No service here, so nothing has set the capturing flag; the calls
        // would all be no-ops. Stand in for the service and set it, holding
        // the reader: dropping it would return its count.
        let service = ScopeRingReader::open(std::process::id()).unwrap();
        service.set_capturing(true);
        let s = start(b"outer");
        assert_ne!(s, 0);
        let i = instant(b"marker");
        let a = start_async(b"job");
        link(i, a);
        value(b"hp", 42.5);
        stop(a);
        stop(s);
        let long = "L".repeat(100);
        let l = start(long.as_bytes());
        stop(l);

        let mine = [s, i, a, l];
        let events: Vec<ScopeEvent> = all_events()
            .into_iter()
            .filter(|e| mine.contains(&e.scope_id) || e.kind == kind::VALUE || e.kind == kind::TEXT)
            .collect();
        let count = |k: u8| events.iter().filter(|e| e.kind == k).count();
        assert_eq!(count(kind::SCOPE_START), 3);
        assert_eq!(count(kind::SCOPE_STOP), 3);
        assert_eq!(count(kind::INSTANT), 1);
        assert_eq!(count(kind::LINK), 1);
        assert!(count(kind::VALUE) >= 1);
        assert!(count(kind::TEXT) >= 3, "the 100-byte name spilled");

        let async_start = events.iter().find(|e| e.scope_id == a && e.kind == kind::SCOPE_START).unwrap();
        assert!(async_start.flags & flags::ASYNC != 0, "async is flagged at start");
        let linkrec = events.iter().find(|e| e.kind == kind::LINK).unwrap();
        assert_eq!(linkrec.scope_id, i);
        assert_eq!(u64::from_le_bytes(linkrec.text[..8].try_into().unwrap()), a);
        let v = events.iter().find(|e| e.kind == kind::VALUE).unwrap();
        assert_eq!(v.value(), Some(42.5));

        // The long name comes back whole.
        let mut asm = TextAssembler::new();
        let mut names = Vec::new();
        for e in &events {
            if let Some(done) = asm.accept(e) {
                names.push(done);
            }
        }
        assert!(names.contains(&(long.clone(), Completeness::Complete)));

        // A pre-timestamped span carries the times given, not the clock now.
        span(b"replayed", 111, 222);
        let events2 = all_events();
        let starts: Vec<&ScopeEvent> = events2
            .iter()
            .filter(|e| e.kind == kind::SCOPE_START)
            .collect();
        let replay_start = starts
            .iter()
            .find(|e| e.timestamp_ns == 111)
            .expect("the span used the start timestamp given");
        let replay_stop = events2
            .iter()
            .find(|e| e.kind == kind::SCOPE_STOP && e.scope_id == replay_start.scope_id)
            .expect("and its stop");
        assert_eq!(replay_stop.timestamp_ns, 222, "and the end timestamp given");
        // No handle bytes from the link leaking through as a "name". (Other
        // tests in this process write names of their own to the same
        // segment, so which names there are is not checked here.)
        for (name, _) in &names {
            assert!(
                name.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
                "binary leaked into a name: {name:?}"
            );
        }
    }

    #[test]
    fn init_after_shutdown_makes_the_process_findable_again() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        let attached = ScopeRingReader::open(std::process::id()).expect("a capture already reading");
        shutdown();
        assert!(!segment_file(std::process::id()).exists(), "shutdown removes the name");

        // A plugin host that reloads, a harness that brackets each test: it
        // was told 0, and no capture started from here on could find it.
        init().expect("init again");
        let service = ScopeRingReader::open(std::process::id()).expect("a later capture finds the process");
        assert!(attached.superseded(), "the capture reading the old segment is told to move");
        assert!(!service.superseded());
        service.set_capturing(true);
        let h = start(b"after the second init");
        assert_ne!(h, 0);
        stop(h);
        assert!(
            all_events().iter().any(|e| e.scope_id == h && e.kind == kind::SCOPE_START),
            "and what the process records goes where that capture reads"
        );
        init().expect("and a third is the no-op a second one used to be");
        assert!(!service.superseded());
    }

    #[test]
    fn a_thread_given_the_id_of_one_that_exited_does_not_reissue_its_handles() {
        // The operating system hands a thread id out again once its thread
        // is gone, Windows readily and Linux when the ids wrap. The new
        // thread's counter starts again at one. With the id as the top half
        // of a handle, its first scope had the handle of the old thread's
        // first, and if that one was still open (an async scope stopped
        // later from elsewhere), the old stop closed the new scope.
        let ring_count = perfr_scope_ring::DEFAULT_RING_COUNT;
        let mut exited = ThreadState::new(4242, ring_count);
        let mut reusing_its_id = ThreadState::new(4242, ring_count);
        assert_eq!(exited.tid, reusing_its_id.tid);
        assert_eq!(exited.ring, reusing_its_id.ring, "the ring is still the thread id's");
        let old: Vec<Handle> = (0..3).map(|_| exited.bump()).collect();
        let new: Vec<Handle> = (0..3).map(|_| reusing_its_id.bump()).collect();
        assert!(old.iter().all(|handle| !new.contains(handle)), "{old:x?} and {new:x?} share a handle");
        assert!(old.iter().chain(&new).all(|handle| *handle != 0), "zero is reserved for no event");
    }

    #[test]
    fn a_record_carries_its_thread_whatever_its_handle_says() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        let service = ScopeRingReader::open(std::process::id()).unwrap();
        service.set_capturing(true);
        let h = start(b"h");
        assert_ne!(h, 0);
        stop(h);
        let tid = perfr_scope_ring::platform::thread_id() as u32;
        let mine: Vec<ScopeEvent> = all_events().into_iter().filter(|e| e.scope_id == h).collect();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|e| e.tid == tid));
    }

    #[test]
    fn nothing_is_written_until_a_service_is_capturing() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        let reader = ScopeRingReader::open(std::process::id()).unwrap();
        reader.set_capturing(false);
        // Not capturing: every call is inert and returns the zero handle.
        assert_eq!(start(b"ignored"), 0);
        assert_eq!(instant(b"ignored"), 0);
        value(b"ignored", 1.0);

        // Capturing: the same calls now record.
        reader.set_capturing(true);
        let h = start(b"recorded");
        assert_ne!(h, 0);
        stop(h);
        reader.set_capturing(false);
    }

    #[test]
    fn nothing_reads_the_clock_while_nobody_is_capturing() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        let reader = ScopeRingReader::open(std::process::id()).unwrap();
        if capturing() {
            // A perfr running on this machine has found the test process.
            eprintln!("skipped: something is capturing this process");
            return;
        }
        // The clock is most of what a recorded scope costs. Read before the
        // gate, it was most of what an unrecorded one cost too: two reads a
        // scope, 20 ns against 2, in a program nobody was looking at.
        let reads = || CLOCK_READS.with(|n| n.get());
        let before = reads();
        let handle = start(b"idle");
        stop(handle);
        stop(7);
        drop(scope(b"idle"));
        assert_eq!(start_async(b"idle"), 0);
        assert_eq!(start_dynamic(b"idle"), 0);
        assert_eq!(instant(b"idle"), 0);
        link(7, 8);
        value(b"idle", 1.0);
        log_info(b"idle");
        assert_eq!(reads() - before, 0, "the clock was read with nobody capturing");

        // And once per record when somebody is.
        reader.set_capturing(true);
        let before = reads();
        stop(start(b"recorded"));
        assert_eq!(reads() - before, 2);
        span(b"replayed", 1, 2);
        assert_eq!(reads() - before, 2, "a span brings its own timestamps");
        reader.set_capturing(false);
    }

    #[test]
    fn a_log_is_a_no_op_until_a_service_is_capturing() {
        let _serial = WRITES.lock().unwrap_or_else(|p| p.into_inner());
        init().expect("init");
        let reader = ScopeRingReader::open(std::process::id()).unwrap();
        reader.set_capturing(false);
        log(level::INFO, b"silent");
        reader.set_capturing(true);
        log(level::ERROR, b"visible");
        let events = all_events();
        let has = |needle: &[u8]| {
            events.iter().any(|e| e.kind == kind::LOG && e.text_bytes().windows(needle.len()).any(|w| w == needle))
        };
        assert!(has(b"visible"));
        assert!(!has(b"silent"), "a log while capture is off writes nothing");
        reader.set_capturing(false);
    }
}
