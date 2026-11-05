// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-connection session limits.
//!
//! A session is idle only when both of these are true: the client has sent
//! no control message and no interaction ping, and the relay has delivered
//! no new events. Either one keeps it alive. The idle window defaults to
//! 60 seconds and is configurable. The hard cap is 15 minutes from connect,
//! and it fires even when the client is active. `expires_unix_ms` is that
//! hard-cap deadline on the unix clock, so a viewer can count down without
//! trusting its own connect time.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default quiet period before an idle disconnect.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(60);

/// Hard cap. Not extended by pings or by events.
pub const HARD_CAP: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub struct Session {
    started: Instant,
    last_activity: Instant,
    last_delivery: Instant,
    idle: Duration,
    hard_cap: Duration,
    /// Unix epoch milliseconds when the hard cap expires.
    pub expires_unix_ms: u64,
}

impl Session {
    pub fn start(now: Instant, idle: Duration, hard_cap: Duration, now_unix_ms: u64) -> Self {
        let idle = if idle.is_zero() { DEFAULT_IDLE } else { idle };
        let hard_cap = if hard_cap.is_zero() {
            HARD_CAP
        } else {
            hard_cap
        };
        Self {
            started: now,
            last_activity: now,
            last_delivery: now,
            idle,
            hard_cap,
            expires_unix_ms: now_unix_ms.saturating_add(millis(hard_cap)),
        }
    }

    /// Idle window from [`DEFAULT_IDLE`], hard cap from [`HARD_CAP`].
    pub fn start_now(idle: Duration) -> Self {
        Self::start(Instant::now(), idle, HARD_CAP, unix_ms())
    }

    pub fn note_activity(&mut self, now: Instant) {
        if now > self.last_activity {
            self.last_activity = now;
        }
    }

    /// An event batch was written to this client's socket.
    pub fn note_delivery(&mut self, now: Instant) {
        if now > self.last_delivery {
            self.last_delivery = now;
        }
    }

    pub fn idle(&self) -> Duration {
        self.idle
    }

    /// Idle begins only once both the last ping and the last delivery are
    /// at least `idle` old, so the deadline is the later of the two plus
    /// the window.
    pub fn idle_deadline(&self) -> Instant {
        self.last_activity.max(self.last_delivery) + self.idle
    }

    pub fn hard_deadline(&self) -> Instant {
        self.started + self.hard_cap
    }

    pub fn idle_expired(&self, now: Instant) -> bool {
        now >= self.idle_deadline()
    }

    pub fn hard_expired(&self, now: Instant) -> bool {
        now >= self.hard_deadline()
    }

    pub fn expired(&self, now: Instant) -> Option<Expiry> {
        if self.hard_expired(now) {
            Some(Expiry::HardCap)
        } else if self.idle_expired(now) {
            Some(Expiry::Idle)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    Idle,
    HardCap,
}

impl Expiry {
    pub fn close_reason(self) -> &'static str {
        match self {
            Expiry::Idle => "idle",
            Expiry::HardCap => "session cap",
        }
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn millis(d: Duration) -> u64 {
    d.as_millis() as u64
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sixty_seconds_and_fifteen_minutes() {
        assert_eq!(DEFAULT_IDLE, Duration::from_secs(60));
        assert_eq!(HARD_CAP, Duration::from_secs(15 * 60));
        let now_ms = 1_700_000_000_000;
        let session = Session::start(Instant::now(), DEFAULT_IDLE, HARD_CAP, now_ms);
        assert_eq!(session.expires_unix_ms, now_ms + 15 * 60 * 1000);
        assert_eq!(session.idle(), DEFAULT_IDLE);
    }

    #[test]
    fn idle_requires_silence_from_both_the_client_and_the_stream() {
        let t0 = Instant::now();
        let idle = Duration::from_secs(60);
        let mut session = Session::start(t0, idle, HARD_CAP, 0);
        assert!(!session.idle_expired(t0 + Duration::from_secs(59)));
        assert!(session.idle_expired(t0 + idle));

        session.note_activity(t0 + Duration::from_secs(50));
        assert!(!session.idle_expired(t0 + Duration::from_secs(70)));
        assert!(session.idle_expired(t0 + Duration::from_secs(50) + idle));

        let mut session = Session::start(t0, idle, HARD_CAP, 0);
        session.note_delivery(t0 + Duration::from_secs(40));
        assert!(!session.idle_expired(t0 + Duration::from_secs(90)));
        assert!(session.idle_expired(t0 + Duration::from_secs(40) + idle));
    }

    #[test]
    fn hard_cap_fires_while_the_client_is_active() {
        let t0 = Instant::now();
        let mut session =
            Session::start(t0, Duration::from_secs(60), Duration::from_secs(90), 5_000);
        assert_eq!(session.expires_unix_ms, 5_000 + 90_000);
        let later = t0 + Duration::from_secs(80);
        session.note_activity(later);
        session.note_delivery(later);
        assert_eq!(session.expired(later), None);
        assert_eq!(
            session.expired(t0 + Duration::from_secs(90)),
            Some(Expiry::HardCap)
        );
    }

    #[test]
    fn zero_durations_fall_back_to_the_defaults() {
        let session = Session::start(Instant::now(), Duration::ZERO, Duration::ZERO, 0);
        assert_eq!(session.idle(), DEFAULT_IDLE);
        assert_eq!(session.hard_deadline() - session.started, HARD_CAP);
    }

    #[test]
    fn close_reasons() {
        assert_eq!(Expiry::Idle.close_reason(), "idle");
        assert_eq!(Expiry::HardCap.close_reason(), "session cap");
    }
}
