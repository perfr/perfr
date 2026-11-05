// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fan-out relay for perfr's public live experiment.
//!
//! One process on the capture machine reads events locally and publishes
//! each batch into [`BroadcastRing`] once. Browsers connect with a
//! WebSocket and keep a cursor. A slow browser drops or coalesces on its
//! own cursor; the publisher does not wait for it and does not serialize
//! the batch again.
//!
//! [`Relay`] serves three endpoints:
//!
//! * `GET /ws` — the viewer stream. Hello, then the name preamble, then
//!   batches. About once a second, a Status frame whose tail carries
//!   `clients`.
//! * `GET /status` — `{"clients","events_per_sec","uptime"}`. The client
//!   count is an atomic, not a lock.
//! * `POST /control` — rate-limited synthetic knobs (`rate`, `noise`,
//!   `herd`). They never touch frames that were published from a capture.

mod control;
mod ring;
mod seqlock;
mod server;
mod session;
mod sync;
mod wire;

pub use control::{ControlUpdate, KnobSnapshot, DEFAULT_CONTROL_PER_SEC};
pub use ring::{BroadcastRing, Cursor, Poll, SharedFrame};
pub use server::{serve, serve_ephemeral, Relay, RelayConfig};
pub use session::{Expiry, Session, DEFAULT_IDLE, HARD_CAP};
