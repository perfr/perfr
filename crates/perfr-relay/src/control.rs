// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Knobs for the synthetic source, and the limiter in front of them.
//!
//! `rate`, `noise`, and `herd` are read by the synthetic generator only.
//! Frames published from a real perfr capture do not consult them, so a
//! viewer cannot change the workload by calling this API.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::session::unix_ms;

/// Default ceiling for `POST /control` and WebSocket knob messages.
pub const DEFAULT_CONTROL_PER_SEC: u32 = 8;

#[derive(Clone, Debug, Serialize)]
pub struct KnobSnapshot {
    pub rate: u64,
    pub noise: f64,
    pub herd: u64,
}

/// Synthetic-source controls. Atomically published so the generator can read
/// them without taking a lock.
pub struct Knobs {
    rate: AtomicU64,
    noise_bits: AtomicU64,
    herd: AtomicU64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self::new()
    }
}

impl Knobs {
    pub fn new() -> Self {
        Self {
            rate: AtomicU64::new(0),
            noise_bits: AtomicU64::new(0.0f64.to_bits()),
            herd: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> KnobSnapshot {
        KnobSnapshot {
            rate: self.rate(),
            noise: self.noise(),
            herd: self.herd(),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn noise(&self) -> f64 {
        f64::from_bits(self.noise_bits.load(Ordering::Relaxed))
    }

    pub fn herd(&self) -> u64 {
        self.herd.load(Ordering::Relaxed)
    }

    /// One-shot burst size. The generator takes it; a second take is zero
    /// until a client asks again.
    pub fn take_herd(&self) -> u64 {
        self.herd.swap(0, Ordering::AcqRel)
    }

    pub fn apply_http(&self, update: &ControlUpdate) {
        if let Some(rate) = update.rate {
            self.rate.store(rate, Ordering::Relaxed);
        }
        if let Some(noise) = update.noise {
            self.store_noise(noise);
        }
        if let Some(herd) = update.herd {
            self.herd.store(herd, Ordering::Relaxed);
        }
    }

    /// WebSocket text. `{"op":"ping"}` and the bare word `ping` are activity
    /// with no knob change. Knob ops are applied only when `apply` is true
    /// (the caller already passed the limiter).
    pub fn apply_ws(&self, text: &str, apply: bool) -> WsControl {
        let text = text.trim();
        if text == "ping" || text == "{\"op\":\"ping\"}" {
            return WsControl::Ping;
        }
        let Ok(msg) = serde_json::from_str::<WsMessage>(text) else {
            return WsControl::Ignore;
        };
        match msg.op.as_str() {
            "ping" => WsControl::Ping,
            "rate" if apply => {
                if let Some(value) = msg.value_u64() {
                    self.rate.store(value, Ordering::Relaxed);
                }
                WsControl::Applied
            }
            "noise" if apply => {
                if let Some(value) = msg.value_f64() {
                    self.store_noise(value);
                }
                WsControl::Applied
            }
            "herd" if apply => {
                if let Some(value) = msg.value_u64() {
                    self.herd.store(value, Ordering::Relaxed);
                }
                WsControl::Applied
            }
            "rate" | "noise" | "herd" => WsControl::Limited,
            _ => WsControl::Ignore,
        }
    }

    fn store_noise(&self, noise: f64) {
        let noise = if noise.is_finite() {
            noise.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.noise_bits.store(noise.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsControl {
    /// Interaction ping. Resets the idle timer.
    Ping,
    /// A knob change that was stored.
    Applied,
    /// A knob change that was refused by the limiter. Still activity.
    Limited,
    /// Not a control message.
    Ignore,
}

impl WsControl {
    pub fn is_activity(self) -> bool {
        !matches!(self, WsControl::Ignore)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ControlUpdate {
    pub rate: Option<u64>,
    pub noise: Option<f64>,
    pub herd: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct WsMessage {
    op: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

impl WsMessage {
    fn value_u64(&self) -> Option<u64> {
        match self.value.as_ref()? {
            serde_json::Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    fn value_f64(&self) -> Option<f64> {
        match self.value.as_ref()? {
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        }
    }
}

/// Token bucket. `allow` is the only mutating call on the request path, and
/// it does not take a lock.
pub struct Limiter {
    per_sec: u32,
    tokens: AtomicU64,
    updated_ms: AtomicU64,
}

impl Limiter {
    pub fn new(per_sec: u32) -> Self {
        let per_sec = per_sec.max(1);
        Self {
            per_sec,
            tokens: AtomicU64::new(u64::from(per_sec)),
            updated_ms: AtomicU64::new(0),
        }
    }

    pub fn allow(&self) -> bool {
        self.allow_at(unix_ms())
    }

    pub fn allow_at(&self, now_ms: u64) -> bool {
        self.refill(now_ms);
        loop {
            let tokens = self.tokens.load(Ordering::Relaxed);
            if tokens == 0 {
                return false;
            }
            if self
                .tokens
                .compare_exchange(tokens, tokens - 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn refill(&self, now_ms: u64) {
        let prev = self.updated_ms.load(Ordering::Relaxed);
        if now_ms <= prev {
            return;
        }
        if self
            .updated_ms
            .compare_exchange(prev, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let elapsed = now_ms - prev;
        let add = elapsed.saturating_mul(u64::from(self.per_sec)) / 1000;
        if add == 0 {
            return;
        }
        let cur = self.tokens.load(Ordering::Relaxed);
        let cap = u64::from(self.per_sec);
        self.tokens
            .store(cur.saturating_add(add).min(cap), Ordering::Relaxed);
    }
}

#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;

    #[test]
    fn http_update_changes_only_the_fields_it_sets() {
        let knobs = Knobs::new();
        knobs.apply_http(&ControlUpdate {
            rate: Some(250),
            noise: Some(2.5),
            herd: None,
        });
        let snap = knobs.snapshot();
        assert_eq!(snap.rate, 250);
        assert_eq!(snap.noise, 1.0);
        assert_eq!(snap.herd, 0);
        assert_eq!(knobs.take_herd(), 0);
    }

    #[test]
    fn herd_is_a_one_shot() {
        let knobs = Knobs::new();
        knobs.apply_http(&ControlUpdate {
            rate: None,
            noise: None,
            herd: Some(40),
        });
        assert_eq!(knobs.take_herd(), 40);
        assert_eq!(knobs.take_herd(), 0);
    }

    #[test]
    fn ws_ping_does_not_touch_knobs() {
        let knobs = Knobs::new();
        knobs.apply_http(&ControlUpdate {
            rate: Some(9),
            noise: None,
            herd: None,
        });
        assert_eq!(knobs.apply_ws("ping", true), WsControl::Ping);
        assert_eq!(knobs.apply_ws("{\"op\":\"ping\"}", false), WsControl::Ping);
        assert_eq!(knobs.rate(), 9);
        assert_eq!(
            knobs.apply_ws("{\"op\":\"rate\",\"value\":3}", false),
            WsControl::Limited
        );
        assert_eq!(knobs.rate(), 9);
        assert_eq!(
            knobs.apply_ws("{\"op\":\"rate\",\"value\":3}", true),
            WsControl::Applied
        );
        assert_eq!(knobs.rate(), 3);
        assert!(!knobs.apply_ws("hello", true).is_activity());
    }

    #[test]
    fn limiter_refills_on_the_clock_it_is_given() {
        let limiter = Limiter::new(2);
        assert!(limiter.allow_at(1_000));
        assert!(limiter.allow_at(1_000));
        assert!(!limiter.allow_at(1_000));
        assert!(limiter.allow_at(2_000));
    }
}
