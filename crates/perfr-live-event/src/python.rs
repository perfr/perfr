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

/// A span of the GIL row is drawn in the colour of the thread that held the
/// lock, the colour of that thread's chip on the rail. A thread's colour is
/// its id's place in the scheme's scope palette
/// ([`crate::thread_scope_color`]), and an event has one spare byte, `extra`,
/// not room for an id. Every scheme's palette has a length that divides this
/// number, so the id modulo it picks the colour the id itself picks.
pub const HOLDER_MODULUS: u32 = 168;
const _: () = assert!(HOLDER_MODULUS < u8::MAX as u32);

/// What a GIL span carries in `extra` for the thread that held the lock: one
/// more than its id modulo [`HOLDER_MODULUS`]. 0 is a span that does not say,
/// which is drawn in the colour of its name.
pub fn holder_extra(holder_tid: u32) -> u8 {
    (holder_tid % HOLDER_MODULUS) as u8 + 1
}

/// An id that takes the colour the holder's does, from the GIL row's thread
/// id as the viewer holds it and the span's `extra`; `None` for a span that
/// does not say who held the lock. A linked machine's threads have the
/// machine in the top byte of their ids, and so does its GIL row: the
/// holder's colour is that of its id with the machine in it.
pub fn holder_color_id(row_tid: u32, extra: u8) -> Option<u32> {
    let machine = row_tid & !((1 << crate::dev::MACHINE_SHIFT) - 1);
    (extra != 0).then(|| machine + (extra as u32 - 1))
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

    #[test]
    fn every_scheme_gives_a_gil_span_the_colour_of_the_thread_that_held_it() {
        let _scheme = crate::theme::switch_lock();
        for theme in crate::theme::THEMES.iter().copied() {
            assert_eq!(
                HOLDER_MODULUS as usize % theme.scope.len(),
                0,
                "{}: its scope palette has a length HOLDER_MODULUS is not a multiple of",
                theme.name
            );
            crate::theme::set_active(theme);
            for machine in [0u32, 1, 3, 255] {
                for holder in [1u32, 7, 167, 168, 169, 2_890_053, 4_194_303] {
                    let packed = (machine << crate::dev::MACHINE_SHIFT) | holder;
                    let row = (machine << crate::dev::MACHINE_SHIFT) | GIL_TID;
                    let id = holder_color_id(row, holder_extra(holder)).unwrap();
                    assert_eq!(
                        crate::thread_scope_color(id, 1),
                        crate::thread_scope_color(packed, 1),
                        "{}: thread {holder} of machine {machine}",
                        theme.name
                    );
                    // And an event of the row is drawn in it.
                    let span = crate::LiveEvent {
                        tid: row,
                        kind: crate::kind::API_SCOPE,
                        extra: holder_extra(holder),
                        name_id: 9,
                        ..Default::default()
                    };
                    assert_eq!(span.color_rgba(), crate::thread_scope_color(packed, 1));
                }
            }
        }
        crate::theme::restore_default();
        // A span that does not say who held the lock keeps its name's colour,
        // and a scope of any other row is not read this way.
        let named = |tid: u32, extra: u8| {
            crate::LiveEvent {
                tid,
                kind: crate::kind::API_SCOPE,
                extra,
                name_id: 9,
                ..Default::default()
            }
            .color_rgba()
        };
        assert_eq!(
            named(GIL_TID, 0),
            crate::named_scope_color(&9u32.to_le_bytes(), 0)
        );
        assert_eq!(
            named(4242, 5),
            crate::named_scope_color(&9u32.to_le_bytes(), 0)
        );
        assert_eq!(holder_color_id(GIL_TID, 0), None);
    }
}
