// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The write path does not allocate, whatever the length of the name.
//!
//! This code runs inside somebody else's program, on the path being
//! measured. An allocation there is the allocator's lock and its cost inside
//! the scope, and makes the API unusable from a signal handler or from
//! inside an allocator hook. A test binary of its own, because the only way
//! to know is to be the allocator.

use perfr_scope_ring::ScopeRingReader;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every request is passed to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(|n| n.get())
}

#[test]
fn recording_allocates_nothing_however_long_the_name() {
    perfr_api::init().expect("init");
    let service = ScopeRingReader::open(std::process::id()).unwrap();
    service.set_capturing(true);

    // What nearly every real name is: a __PRETTY_FUNCTION__, a Python
    // qualified name, a log line. Anything past 32 bytes is a chain.
    let long = "void Renderer::drawFrame(const Scene&, const Camera&, FrameGraph&) [with T = float]";
    let line = "x".repeat(300);
    let huge = "y".repeat(20_000);
    assert!(long.len() > 32);

    // The first call on a thread reads its id; not what is measured here.
    perfr_api::stop(perfr_api::start("warm"));

    let before = allocations();
    let handle = perfr_api::start(long);
    assert_ne!(handle, 0, "nothing was recorded");
    perfr_api::stop(handle);
    perfr_api::stop(perfr_api::start_async(long));
    perfr_api::instant(long);
    perfr_api::value(long, 1.0);
    perfr_api::log_info(&line);
    perfr_api::log_error(&huge);
    perfr_api::span(long, 1, 2);
    assert_eq!(allocations() - before, 0, "the write path allocated");

    // Time it too: `cargo test -p perfr-api --release --test no_alloc -- --nocapture`.
    const CALLS: u32 = 200_000;
    let best = (0..9)
        .map(|_| {
            let started = std::time::Instant::now();
            for _ in 0..CALLS {
                perfr_api::stop(perfr_api::start(std::hint::black_box(long)));
            }
            started.elapsed().as_nanos() as f64 / f64::from(CALLS)
        })
        .fold(f64::MAX, f64::min);
    println!("  start + stop with an {}-byte name: {best:6.1} ns", long.len());
}
