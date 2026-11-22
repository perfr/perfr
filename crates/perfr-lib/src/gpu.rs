// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the GPU ran, under the thread that asked for it.
//!
//! A CUDA program hands its work to the GPU and goes on: the kernel it
//! launched runs later, on a queue, and a profile of the CPU shows the
//! launch and then a wait somewhere else. NVIDIA's CUPTI, the library its
//! own profilers are built on, reports both halves from inside the process:
//!
//! - the callback API calls [`launched`] on the thread that enters
//!   `cuLaunchKernel` or one of the memory copies, with the correlation id
//!   the driver gave that call;
//! - the activity API later hands over a buffer of records, one per kernel,
//!   copy and fill that has finished, each with the same correlation id and
//!   the time it started and ended on the GPU.
//!
//! Each record becomes a span on the GPU lane of the thread that launched it
//! (`perfr_api::gpu_span`), named for the kernel: under the thread
//! running the forward pass, the kernels of the forward pass, when the GPU
//! got to them.
//!
//! CUPTI is not linked. It is found beside the CUDA runtime the program has
//! loaded and opened with `dlopen`, and the fields of its records are read
//! at their offsets: the leading fields used here have not moved since CUDA
//! 9, and every later version of a record only adds to its end.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

type Result32 = i32;
type Subscriber = *mut c_void;
type CallbackFn = unsafe extern "C" fn(*mut c_void, i32, u32, *const c_void);
type RequestedFn = unsafe extern "C" fn(*mut *mut u8, *mut usize, *mut usize);
type CompletedFn = unsafe extern "C" fn(*mut c_void, u32, *mut u8, usize, usize);

/// The functions of libcupti this uses.
struct Cupti {
    subscribe: unsafe extern "C" fn(*mut Subscriber, CallbackFn, *mut c_void) -> Result32,
    unsubscribe: unsafe extern "C" fn(Subscriber) -> Result32,
    enable_callback: unsafe extern "C" fn(u32, Subscriber, i32, u32) -> Result32,
    register_timestamp: unsafe extern "C" fn(extern "C" fn() -> u64) -> Result32,
    register_callbacks: unsafe extern "C" fn(RequestedFn, CompletedFn) -> Result32,
    enable: unsafe extern "C" fn(i32) -> Result32,
    disable: unsafe extern "C" fn(i32) -> Result32,
    flush_all: unsafe extern "C" fn(u32) -> Result32,
    flush_period: Option<unsafe extern "C" fn(u32) -> Result32>,
    next_record: unsafe extern "C" fn(*mut u8, usize, *mut *mut u8) -> Result32,
    version: Option<unsafe extern "C" fn(*mut u32) -> Result32>,
}

// SAFETY: plain function pointers into a library that is never unloaded.
unsafe impl Send for Cupti {}
unsafe impl Sync for Cupti {}

/// `CUpti_ActivityKind`.
mod activity {
    pub const MEMCPY: i32 = 1;
    pub const MEMSET: i32 = 2;
    pub const KERNEL: i32 = 3;
    pub const CONCURRENT_KERNEL: i32 = 10;
}
/// `CUPTI_CB_DOMAIN_DRIVER_API`.
const DOMAIN_DRIVER: i32 = 1;
/// `CUPTI_API_ENTER`.
const SITE_ENTER: u32 = 0;
/// `CUPTI_ACTIVITY_FLAG_FLUSH_FORCED`.
const FLUSH_FORCED: u32 = 1;

/// The driver calls that put work on the GPU, by their `cupti_driver_cbid.h`
/// numbers: the kernel launches, then the copies and fills PyTorch, CuPy and
/// the CUDA runtime make. An id never changes meaning; a call this misses
/// has its records dropped for want of a thread, and counted.
const LAUNCHES: &[u32] = &[
    // cuLaunch, cuLaunchGrid, cuLaunchGridAsync, cuLaunchKernel, cuGraphLaunch, cuLaunchKernelEx
    115, 116, 117, 307, 514, 652,
    // cuMemcpy{HtoD,DtoH,DtoD} and their Async forms, first and _v2 generations
    43, 45, 47, 60, 62, 64, 276, 277, 278, 279, 280, 281,
    // cuMemcpy, cuMemcpyAsync, cuMemcpy_v2, and the per-thread-stream forms
    248, 305, 306, 397, 398, 399, 408, 418, 421, 422, 423,
    // cuMemsetD{8,16,32}, Async, _v2, per-thread-stream
    71, 73, 75, 216, 218, 220, 249, 250, 251, 411, 412, 413, 428, 429, 430,
];

/// Which thread made the call with a correlation id: `(id << 32) | tid` at
/// `id % SLOTS`. Written on the launching thread inside the driver call,
/// read later on CUPTI's thread, with nothing to wait for and nothing to
/// allocate. Ids count up, so a slot is reused only after `SLOTS` further
/// launches: seconds, where a kernel runs within a fraction of one.
const SLOTS: usize = 1 << 16;
static LAUNCHED_BY: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

fn note_launch(correlation: u32, tid: u32) {
    LAUNCHED_BY[correlation as usize % SLOTS].store((u64::from(correlation) << 32) | u64::from(tid), Ordering::Relaxed);
}

fn launcher_of(correlation: u32) -> Option<u32> {
    let slot = LAUNCHED_BY[correlation as usize % SLOTS].load(Ordering::Relaxed);
    ((slot >> 32) as u32 == correlation && slot as u32 != 0).then_some(slot as u32)
}

/// The capture the records go to, or 0.
static GENERATION: AtomicU32 = AtomicU32::new(0);
/// Records that became spans, and records whose launch was not seen: work
/// already queued when the capture started, or a driver call not in
/// [`LAUNCHES`].
static SPANS: AtomicU64 = AtomicU64::new(0);
static UNATTRIBUTED: AtomicU64 = AtomicU64::new(0);
/// Bytes of activity buffers CUPTI holds right now.
static BUFFERED: AtomicUsize = AtomicUsize::new(0);

/// The library's functions, once it has been loaded: never unloaded, so
/// never cleared. CUPTI's callbacks read it without a lock.
static FUNCTIONS: AtomicPtr<Cupti> = AtomicPtr::new(std::ptr::null_mut());

fn functions() -> Option<&'static Cupti> {
    // SAFETY: only `load` stores here, a leaked box.
    unsafe { FUNCTIONS.load(Ordering::Acquire).as_ref() }
}

/// The subscription of the capture that is recording, for `stop`.
struct Recording(Subscriber);
// SAFETY: the handle is only passed back to CUPTI.
unsafe impl Send for Recording {}

static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);
/// Kernel names as CUPTI gives them, to how each is sent.
static NAMES: Mutex<Option<HashMap<Vec<u8>, SentName>>> = Mutex::new(None);

/// A kernel's name on the wire. The text is a hundred bytes, four records
/// of a ring that a flush fills with a few thousand kernels at once, so it
/// is sent now and then and a number stands for it in between (the service
/// reads both: `scopes.rs`, `name_definition`).
struct SentName {
    /// `\x04`, the number in hex, the name: says what the number means.
    definition: Vec<u8>,
    /// `\x03` and the number: the name again, in one record.
    token: [u8; 9],
    uses: u64,
}

/// Spans of one name between two sendings of its text. A text lost to a lap
/// of the ring leaves that many spans without their name at most.
const TEXT_EVERY: u64 = 2048;

impl SentName {
    fn new(shown: &[u8]) -> Self {
        // FNV-1a: the number only has to differ between the few hundred
        // kernels of one program.
        let number = shown.iter().fold(0x811c_9dc5u32, |hash, byte| (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193));
        let hex = format!("{number:08x}");
        let mut token = [3u8; 9];
        token[1..].copy_from_slice(hex.as_bytes());
        let mut definition = Vec::with_capacity(9 + shown.len());
        definition.push(4);
        definition.extend_from_slice(hex.as_bytes());
        definition.extend_from_slice(shown);
        Self { definition, token, uses: 0 }
    }

    /// What to name the next span by.
    fn next(&mut self) -> &[u8] {
        let text = self.uses % TEXT_EVERY == 0;
        self.uses += 1;
        if text {
            &self.definition
        } else {
            &self.token
        }
    }
}

extern "C" fn timestamp() -> u64 {
    perfr_api::now_ns()
}

/// CUPTI's callback on a driver call: the thread that is launching.
unsafe extern "C" fn launched(_user: *mut c_void, _domain: i32, _cbid: u32, data: *const c_void) {
    if data.is_null() || GENERATION.load(Ordering::Relaxed) == 0 {
        return;
    }
    // CUpti_CallbackData: callbackSite at 0, correlationId at 64.
    let site = data.cast::<u32>().read_unaligned();
    if site == SITE_ENTER {
        let correlation = data.cast::<u8>().add(64).cast::<u32>().read_unaligned();
        note_launch(correlation, perfr_scope_ring::platform::thread_id() as u32);
    }
}

/// One megabyte a buffer: about five thousand kernel records.
const BUFFER_BYTES: usize = 1 << 20;

unsafe extern "C" fn requested(buffer: *mut *mut u8, size: *mut usize, max_records: *mut usize) {
    let bytes = libc::malloc(BUFFER_BYTES).cast::<u8>();
    *buffer = bytes;
    *size = if bytes.is_null() { 0 } else { BUFFER_BYTES };
    *max_records = 0;
    if !bytes.is_null() {
        BUFFERED.fetch_add(BUFFER_BYTES, Ordering::Relaxed);
    }
}

unsafe extern "C" fn completed(_context: *mut c_void, _stream: u32, buffer: *mut u8, _size: usize, valid: usize) {
    if buffer.is_null() {
        return;
    }
    let generation = GENERATION.load(Ordering::Acquire);
    if let (Some(cupti), true) = (functions(), generation != 0 && super::session_is(generation) && valid > 0) {
        let mut record: *mut u8 = std::ptr::null_mut();
        while (cupti.next_record)(buffer, valid, &mut record) == 0 && !record.is_null() {
            let left = valid.saturating_sub(record as usize - buffer as usize);
            if let Some(work) = Work::read(std::slice::from_raw_parts(record, left)) {
                emit(&work);
            }
        }
    }
    libc::free(buffer.cast());
    BUFFERED.fetch_sub(BUFFER_BYTES, Ordering::Relaxed);
}

/// What one activity record says the GPU did.
#[derive(Debug, PartialEq)]
struct Work {
    start_ns: u64,
    end_ns: u64,
    correlation: u32,
    what: What,
}

#[derive(Debug, PartialEq)]
enum What {
    /// The kernel's symbol, as the pointer CUPTI keeps.
    Kernel(*const c_char),
    /// `CUpti_ActivityMemcpyKind`.
    Copy(u8),
    Fill,
}

impl Work {
    /// Reads the record that starts `bytes`: its kind, then the fields of
    /// that kind at the offsets every version of the record has them at.
    /// `None` for a kind that is not GPU work, a record cut short, and one
    /// with no end time (CUPTI writes 0 for a kernel it lost track of).
    fn read(bytes: &[u8]) -> Option<Work> {
        let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let u64_at = |at: usize| bytes.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        let kind = u32_at(0)? as i32;
        let (start_ns, end_ns) = (u64_at(16)?, u64_at(24)?);
        let (correlation, what) = match kind {
            activity::KERNEL | activity::CONCURRENT_KERNEL => {
                (u32_at(92)?, What::Kernel(u64_at(104)? as usize as *const c_char))
            }
            activity::MEMCPY => (u32_at(44)?, What::Copy(*bytes.get(4)?)),
            activity::MEMSET => (u32_at(44)?, What::Fill),
            _ => return None,
        };
        (end_ns > start_ns && start_ns != 0).then_some(Work { start_ns, end_ns, correlation, what })
    }
}

fn emit(work: &Work) {
    let Some(tid) = launcher_of(work.correlation) else {
        UNATTRIBUTED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    match work.what {
        What::Kernel(symbol) => {
            if symbol.is_null() {
                perfr_api::gpu_span(tid, "kernel", work.start_ns, work.end_ns);
            } else {
                // SAFETY: CUPTI's string for the kernel, valid for the callback.
                let raw = unsafe { CStr::from_ptr(symbol) }.to_bytes();
                let Ok(mut names) = NAMES.lock() else { return };
                let names = names.get_or_insert_with(HashMap::new);
                if !names.contains_key(raw) {
                    names.insert(raw.to_vec(), SentName::new(&kernel_name(raw)));
                }
                let name = names.get_mut(raw).expect("just put there").next();
                perfr_api::gpu_span(tid, name, work.start_ns, work.end_ns);
            }
        }
        What::Copy(kind) => perfr_api::gpu_span(tid, copy_name(kind), work.start_ns, work.end_ns),
        What::Fill => perfr_api::gpu_span(tid, "memset", work.start_ns, work.end_ns),
    }
    SPANS.fetch_add(1, Ordering::Relaxed);
}

/// The longest kernel name shown. A span's name travels with every span,
/// and a cutlass or flash-attention kernel's is a paragraph of template
/// arguments.
const NAME_MAX: usize = 120;

/// The name a kernel is shown by: its C++ symbol demangled, without the
/// return type and the parameters, cut to [`NAME_MAX`]. A name that is not
/// a C++ symbol (`triton_poi_fused_gelu_7`, `ampere_sgemm_128x64_tn`) is
/// shown as it is.
fn kernel_name(symbol: &[u8]) -> Vec<u8> {
    let mut name = if symbol.starts_with(b"_Z") {
        let options = cpp_demangle::DemangleOptions::new().no_params().no_return_type();
        cpp_demangle::Symbol::new(symbol)
            .ok()
            .and_then(|parsed| parsed.demangle_with_options(&options).ok())
            .map(String::into_bytes)
            .unwrap_or_else(|| symbol.to_vec())
    } else {
        symbol.to_vec()
    };
    if name.len() > NAME_MAX {
        let mut cut = NAME_MAX - 3;
        while cut > 0 && (name[cut] & 0xc0) == 0x80 {
            cut -= 1;
        }
        name.truncate(cut);
        name.extend_from_slice(b"...");
    }
    name
}

/// `CUpti_ActivityMemcpyKind`, in words.
fn copy_name(kind: u8) -> &'static str {
    match kind {
        1 => "memcpy: host to device",
        2 => "memcpy: device to host",
        8 => "memcpy: device to device",
        9 => "memcpy: host to host",
        10 => "memcpy: device to another device",
        _ => "memcpy",
    }
}

/// The path of a library the process has mapped whose file name starts with
/// `prefix`, from `maps` (the text of `/proc/self/maps`).
fn mapped_library(maps: &str, prefix: &str) -> Option<String> {
    maps.lines().find_map(|line| {
        let path = line.split_whitespace().nth(5)?;
        let file = path.rsplit('/').next()?;
        (path.starts_with('/') && file.starts_with(prefix)).then(|| path.to_string())
    })
}

/// Where CUPTI is for this process: already mapped (PyTorch links it),
/// else beside the CUDA runtime it has mapped, else where a CUDA toolkit
/// keeps it relative to that runtime, else by name through the loader.
fn cupti_candidates(maps: &str) -> Result<Vec<String>, String> {
    if let Some(path) = mapped_library(maps, "libcupti.so") {
        return Ok(vec![path]);
    }
    let runtime = mapped_library(maps, "libcudart.so");
    if runtime.is_none() && mapped_library(maps, "libcuda.so").is_none() {
        return Err("the program has not loaded CUDA (start the capture once it has)".into());
    }
    let mut candidates = Vec::new();
    if let Some(dir) = runtime.as_deref().and_then(|path| path.rsplit_once('/')).map(|(dir, _)| dir) {
        for dir in [dir.to_string(), format!("{dir}/../extras/CUPTI/lib64")] {
            let mut found: Vec<String> = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| name.starts_with("libcupti.so"))
                .map(|name| format!("{dir}/{name}"))
                .collect();
            found.sort();
            candidates.extend(found);
        }
    }
    candidates.extend(["libcupti.so.13", "libcupti.so.12", "libcupti.so"].map(String::from));
    Ok(candidates)
}

// Each symbol becomes the field it is assigned to: the type is written once,
// in `Cupti`, and not a second time at every transmute.
#[allow(clippy::missing_transmute_annotations)]
unsafe fn load(candidates: &[String]) -> Result<(&'static Cupti, String), String> {
    if let Some(cupti) = functions() {
        return Ok((cupti, candidates.first().cloned().unwrap_or_default()));
    }
    for path in candidates {
        let Ok(c_path) = CString::new(path.as_str()) else { continue };
        let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            continue;
        }
        macro_rules! symbol {
            ($name:literal) => {{
                let pointer = libc::dlsym(handle, concat!($name, "\0").as_ptr().cast());
                if pointer.is_null() {
                    return Err(format!("{path} has no {}", $name));
                }
                std::mem::transmute::<*mut c_void, _>(pointer)
            }};
        }
        macro_rules! optional {
            ($name:literal) => {{
                let pointer = libc::dlsym(handle, concat!($name, "\0").as_ptr().cast());
                (!pointer.is_null()).then(|| std::mem::transmute::<*mut c_void, _>(pointer))
            }};
        }
        let cupti = Cupti {
            subscribe: symbol!("cuptiSubscribe"),
            unsubscribe: symbol!("cuptiUnsubscribe"),
            enable_callback: symbol!("cuptiEnableCallback"),
            register_timestamp: symbol!("cuptiActivityRegisterTimestampCallback"),
            register_callbacks: symbol!("cuptiActivityRegisterCallbacks"),
            enable: symbol!("cuptiActivityEnable"),
            disable: symbol!("cuptiActivityDisable"),
            flush_all: symbol!("cuptiActivityFlushAll"),
            flush_period: optional!("cuptiActivityFlushPeriod"),
            next_record: symbol!("cuptiActivityGetNextRecord"),
            version: optional!("cuptiGetVersion"),
        };
        // Leaked, with the library: CUPTI keeps calling back into this
        // agent, which stays loaded, for as long as the process lives.
        let cupti: &'static Cupti = Box::leak(Box::new(cupti));
        FUNCTIONS.store(cupti as *const Cupti as *mut Cupti, Ordering::Release);
        return Ok((cupti, path.clone()));
    }
    Err("CUPTI (libcupti.so) was not found beside the program's CUDA runtime".into())
}

/// Starts recording GPU work for the capture of `generation`. Returns what
/// was loaded, for the status line.
pub(crate) fn start(generation: u32) -> Result<String, String> {
    if !perfr_api::initialized() {
        return Err("the program has a perfr API of its own, which this agent cannot record through".into());
    }
    let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| format!("reading the program's maps: {e}"))?;
    let candidates = cupti_candidates(&maps)?;
    let mut recording = RECORDING.lock().map_err(|_| "GPU state poisoned".to_string())?;
    if recording.is_some() {
        return Err("another capture is recording the GPU".into());
    }
    let (cupti, path) = unsafe { load(&candidates) }?;
    let check = |what: &str, result: Result32| {
        if result == 0 {
            Ok(())
        } else {
            // 39 is CUPTI_ERROR_MULTIPLE_SUBSCRIBERS_NOT_SUPPORTED.
            let why = if result == 39 { " (another profiler in the program is using CUPTI)" } else { "" };
            Err(format!("CUPTI {what} failed with {result}{why}"))
        }
    };
    let mut subscriber: Subscriber = std::ptr::null_mut();
    unsafe {
        check("subscribe", (cupti.subscribe)(&mut subscriber, launched, std::ptr::null_mut()))?;
        let armed = (|| {
            for &cbid in LAUNCHES {
                check("callback", (cupti.enable_callback)(1, subscriber, DOMAIN_DRIVER, cbid))?;
            }
            check("timestamps", (cupti.register_timestamp)(timestamp))?;
            check("buffers", (cupti.register_callbacks)(requested, completed))?;
            for kind in [activity::CONCURRENT_KERNEL, activity::MEMCPY, activity::MEMSET] {
                check("activity", (cupti.enable)(kind))?;
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = armed {
            (cupti.unsubscribe)(subscriber);
            return Err(error);
        }
        // Hand finished work over ten times a second, so that a live view
        // has it while it is being looked at. Without this a buffer is
        // handed over when it is full.
        if let Some(flush_period) = cupti.flush_period {
            flush_period(100);
        }
    }
    SPANS.store(0, Ordering::Relaxed);
    UNATTRIBUTED.store(0, Ordering::Relaxed);
    *recording = Some(Recording(subscriber));
    GENERATION.store(generation, Ordering::Release);
    let mut version = 0u32;
    let versioned = cupti.version.is_some_and(|get| unsafe { get(&mut version) } == 0);
    let file = path.rsplit('/').next().unwrap_or(&path);
    Ok(if versioned { format!("CUPTI {version} ({file})") } else { format!("CUPTI ({file})") })
}

/// Hands over what the GPU has finished, then stops. Returns the spans
/// recorded and the records left out for want of a launching thread.
pub(crate) fn stop(generation: u32) -> Option<(u64, u64)> {
    if GENERATION.load(Ordering::Acquire) != generation || generation == 0 {
        return None;
    }
    let recording = RECORDING.lock().ok()?.take()?;
    let cupti = functions()?;
    unsafe {
        (cupti.unsubscribe)(recording.0);
        // The capture is still on while this runs: what the flush hands
        // over is recorded.
        (cupti.flush_all)(FLUSH_FORCED);
        for kind in [activity::CONCURRENT_KERNEL, activity::MEMCPY, activity::MEMSET] {
            (cupti.disable)(kind);
        }
        if let Some(flush_period) = cupti.flush_period {
            flush_period(0);
        }
    }
    GENERATION.store(0, Ordering::Release);
    Some((SPANS.load(Ordering::Relaxed), UNATTRIBUTED.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as CUPTI lays it out, with the fields this reads.
    fn record(kind: i32, start: u64, end: u64, fields: &[(usize, &[u8])]) -> Vec<u8> {
        let mut bytes = vec![0u8; 224];
        bytes[0..4].copy_from_slice(&kind.to_le_bytes());
        bytes[16..24].copy_from_slice(&start.to_le_bytes());
        bytes[24..32].copy_from_slice(&end.to_le_bytes());
        for (at, value) in fields {
            bytes[*at..*at + value.len()].copy_from_slice(value);
        }
        bytes
    }

    #[test]
    fn a_kernel_record_is_read_at_the_offsets_every_version_keeps() {
        let name = c"triton_poi_fused_gelu_7";
        let pointer = (name.as_ptr() as usize as u64).to_le_bytes();
        let bytes = record(activity::CONCURRENT_KERNEL, 1_000, 1_750, &[(92, &77u32.to_le_bytes()), (104, &pointer)]);
        let work = Work::read(&bytes).expect("a kernel");
        assert_eq!((work.start_ns, work.end_ns, work.correlation), (1_000, 1_750, 77));
        assert_eq!(work.what, What::Kernel(name.as_ptr()));
        // The older serialized-kernel kind is the same record.
        assert!(Work::read(&record(activity::KERNEL, 1, 2, &[(92, &1u32.to_le_bytes())])).is_some());
    }

    #[test]
    fn copies_and_fills_are_read_and_everything_else_is_not_gpu_work() {
        let copy = record(activity::MEMCPY, 10, 30, &[(4, &[2]), (44, &9u32.to_le_bytes())]);
        assert_eq!(Work::read(&copy), Some(Work { start_ns: 10, end_ns: 30, correlation: 9, what: What::Copy(2) }));
        let fill = record(activity::MEMSET, 10, 30, &[(44, &8u32.to_le_bytes())]);
        assert_eq!(Work::read(&fill).map(|w| w.what), Some(What::Fill));
        // A driver API record, an overhead record: not work.
        assert_eq!(Work::read(&record(4, 10, 30, &[])), None);
        assert_eq!(Work::read(&record(17, 10, 30, &[])), None);
        // A kernel CUPTI lost the end of, and a record cut short.
        assert_eq!(Work::read(&record(activity::CONCURRENT_KERNEL, 10, 0, &[])), None);
        assert_eq!(Work::read(&copy[..40]), None);
    }

    #[test]
    fn a_launch_is_remembered_until_its_slot_is_used_again() {
        note_launch(5, 4242);
        note_launch(6, 4343);
        assert_eq!(launcher_of(5), Some(4242));
        assert_eq!(launcher_of(6), Some(4343));
        assert_eq!(launcher_of(7), None);
        // The id that shares the slot of 5 replaces it, and 5 is then unknown
        // rather than given to the wrong thread.
        note_launch(5 + SLOTS as u32, 99);
        assert_eq!(launcher_of(5), None);
        assert_eq!(launcher_of(5 + SLOTS as u32), Some(99));
    }

    #[test]
    fn a_kernel_is_named_without_its_signature_and_within_bounds() {
        assert_eq!(kernel_name(b"triton_poi_fused_gelu_7"), b"triton_poi_fused_gelu_7");
        assert_eq!(
            String::from_utf8(kernel_name(b"_ZN2at6native12_GLOBAL__N_19my_kernelIfEEvPT_i")).unwrap(),
            "at::native::(anonymous namespace)::my_kernel<float>"
        );
        let long = kernel_name(&[b'k'; 400]);
        assert_eq!(long.len(), NAME_MAX);
        assert!(long.ends_with(b"..."));
        assert_eq!(copy_name(1), "memcpy: host to device");
        assert_eq!(copy_name(200), "memcpy");
    }

    #[test]
    fn a_name_is_sent_as_text_now_and_then_and_as_its_number_in_between() {
        let mut name = SentName::new(b"triton_poi_fused_gelu_7");
        let first = name.next().to_vec();
        assert_eq!(first[0], 4);
        assert!(first.ends_with(b"triton_poi_fused_gelu_7"));
        let token = name.next().to_vec();
        assert_eq!((token.len(), token[0]), (9, 3));
        // The number is the same in both, and another name has another.
        assert_eq!(first[1..9], token[1..9]);
        assert_ne!(SentName::new(b"triton_poi_fused_gelu_8").token, name.token);
        for _ in 2..TEXT_EVERY {
            assert_eq!(name.next().len(), 9);
        }
        assert_eq!(name.next(), first, "the text again");
    }

    #[test]
    fn cupti_is_looked_for_where_the_programs_cuda_is() {
        let torch = "7f00-7f01 r-xp 00000000 08:02 1 /venv/nvidia/cu13/lib/libcudart.so.13\n\
                     7f02-7f03 r-xp 00000000 08:02 2 /venv/nvidia/cu13/lib/libcupti.so.13\n";
        assert_eq!(cupti_candidates(torch).unwrap(), ["/venv/nvidia/cu13/lib/libcupti.so.13"]);
        // Not mapped: beside the runtime, then by name.
        let runtime_only = "7f00-7f01 r-xp 00000000 08:02 1 /nowhere/lib64/libcudart.so.12\n";
        let candidates = cupti_candidates(runtime_only).unwrap();
        assert_eq!(candidates, ["libcupti.so.13", "libcupti.so.12", "libcupti.so"]);
        // No CUDA at all.
        let plain = "7f00-7f01 r-xp 00000000 08:02 1 /usr/lib/x86_64-linux-gnu/libc.so.6\n7f02-7f03 rw-p 00000000 00:00 0\n";
        assert!(cupti_candidates(plain).unwrap_err().contains("has not loaded CUDA"));
    }
}
