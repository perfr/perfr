// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! What the service and the viewer agree on about a Python program: the row
//! that shows who holds the GIL.
//!
//! The service reads, about a thousand times a second, which thread of a
//! Python target holds the interpreter's lock. Once the lock has changed
//! hands, each stretch one thread was seen holding it is a
//! [`crate::kind::API_SCOPE`] named after that thread, on one row of the
//! target's process. The row is filed as a thread of the process so that it
//! sits with the threads it is about, and [`GIL_TID`] is the thread id no
//! thread has.

/// The thread id of the GIL row. Above Linux's `pid_max` ceiling of 2^22, so
/// no thread has it, and below 2^24, so a hub can file it under a linked
/// machine like any other id.
pub const GIL_TID: u32 = 0x00FE_611A;
const _: () = assert!(GIL_TID >= 1 << 22 && GIL_TID < 1 << crate::dev::MACHINE_SHIFT);

/// What the row is called on the rail.
pub const GIL_ROW_NAME: &str = "GIL";

/// What the rail says under the name, where a thread's row says its id: the
/// scopes of the row are named after the thread that held the lock.
pub const GIL_ROW_CAPTION: &str = "held by";

/// Whether a thread id, as the viewer holds it (a linked machine's has the
/// machine in its top byte), is the GIL row's.
pub fn is_gil_tid(tid: u32) -> bool {
    tid & ((1 << crate::dev::MACHINE_SHIFT) - 1) == GIL_TID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gil_row_is_known_on_this_machine_and_on_a_linked_one() {
        assert!(is_gil_tid(GIL_TID));
        assert!(is_gil_tid((3 << crate::dev::MACHINE_SHIFT) | GIL_TID));
        assert!(!is_gil_tid(4242));
        assert!(!is_gil_tid(0));
    }
}
