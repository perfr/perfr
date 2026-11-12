// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Eased camera moves between two capture views.
//!
//! Time zoom interpolates the window's duration in log space and its center
//! linearly, so a zoom does not rush at one end. Vertical scroll is linear
//! in rail points. Both use smoothstep, which is flat at the ends.

/// Default move between steps, in milliseconds.
pub const TRANSITION_MS: f32 = 700.0;

/// One camera pose in capture coordinates plus rail scroll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub t0_ns: f64,
    pub t1_ns: f64,
    pub scroll_y: f32,
}

impl Camera {
    pub fn span(self) -> (u64, u64) {
        let a = self.t0_ns.min(self.t1_ns).max(0.0) as u64;
        let b = self.t1_ns.max(self.t0_ns).max(self.t0_ns + 1.0) as u64;
        (a, b.max(a + 1))
    }
}

/// Smoothstep. 0 and 1 stay put; the slope is 0 at both ends.
pub fn ease_smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Pose at `t` in 0..=1. `t` outside that range is clamped.
pub fn interpolate_camera(from: Camera, to: Camera, t: f32) -> Camera {
    let e = ease_smoothstep(t) as f64;
    let d0 = (from.t1_ns - from.t0_ns).abs().max(1.0);
    let d1 = (to.t1_ns - to.t0_ns).abs().max(1.0);
    let dur = (d0.ln() + (d1.ln() - d0.ln()) * e).exp();
    let c0 = (from.t0_ns + from.t1_ns) * 0.5;
    let c1 = (to.t0_ns + to.t1_ns) * 0.5;
    let center = c0 + (c1 - c0) * e;
    let scroll = from.scroll_y + (to.scroll_y - from.scroll_y) * e as f32;
    Camera {
        t0_ns: center - dur * 0.5,
        t1_ns: center + dur * 0.5,
        scroll_y: scroll,
    }
}

/// How far overlays have drawn in, from the same 0..=1 as the camera move.
/// They wait a little, then catch up as the camera settles.
pub fn overlay_reveal(transition_t: f32) -> f32 {
    let t = ((transition_t - 0.15) / 0.85).clamp(0.0, 1.0);
    ease_smoothstep(t)
}

#[derive(Clone, Copy, Debug)]
struct Transition {
    from: Camera,
    to: Camera,
    duration_ms: f32,
    elapsed_ms: f32,
}

/// Play head. The viewer owns the story; this only advances time.
#[derive(Clone, Debug)]
pub struct Player {
    pub step: usize,
    pub elapsed_ms: f32,
    pub playing: bool,
    transition: Option<Transition>,
}

impl Default for Player {
    fn default() -> Self {
        Self {
            step: 0,
            elapsed_ms: 0.0,
            playing: false,
            transition: None,
        }
    }
}

/// What one `tick` decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    /// The pose to show while a move is running. `None` means stay.
    pub camera: Option<Camera>,
    /// A step whose camera the viewer still has to move toward.
    pub entered_step: Option<usize>,
    /// 0..=1 of the move in progress. 1 when the camera is settled.
    pub transition_t: f32,
}

impl Player {
    pub fn pause(&mut self) {
        self.playing = false;
    }

    pub fn toggle_play(&mut self) {
        self.playing = !self.playing;
    }

    pub fn transitioning(&self) -> bool {
        self.transition.is_some()
    }

    pub fn clear_transition(&mut self) {
        self.transition = None;
    }

    pub fn begin_transition(&mut self, from: Camera, to: Camera, duration_ms: f32) {
        self.transition = Some(Transition {
            from,
            to,
            duration_ms: duration_ms.max(1.0),
            elapsed_ms: 0.0,
        });
    }

    /// Jump to `index` and report it so the viewer can start a move.
    /// The same step with no move in flight is left alone.
    pub fn go(&mut self, index: usize, len: usize) -> Option<usize> {
        if len == 0 {
            return None;
        }
        let index = index.min(len - 1);
        if index == self.step && self.transition.is_none() {
            self.elapsed_ms = 0.0;
            return None;
        }
        self.step = index;
        self.elapsed_ms = 0.0;
        self.transition = None;
        Some(index)
    }

    pub fn next(&mut self, len: usize) -> Option<usize> {
        self.go(self.step.saturating_add(1), len)
    }

    pub fn prev(&mut self) -> Option<usize> {
        if self.step == 0 && self.transition.is_none() {
            self.elapsed_ms = 0.0;
            return None;
        }
        self.go(self.step.saturating_sub(1), self.step.saturating_add(1))
    }

    /// Advance by `dt_ms`. `durations[i]` is how long step `i` holds once
    /// the camera has arrived.
    pub fn tick(&mut self, dt_ms: f32, durations: &[u32]) -> Tick {
        if durations.is_empty() {
            return Tick {
                camera: None,
                entered_step: None,
                transition_t: 1.0,
            };
        }
        if self.step >= durations.len() {
            self.step = durations.len() - 1;
        }
        let dt_ms = dt_ms.max(0.0);
        if let Some(tr) = self.transition.as_mut() {
            tr.elapsed_ms = (tr.elapsed_ms + dt_ms).min(tr.duration_ms);
            let t = tr.elapsed_ms / tr.duration_ms;
            let camera = interpolate_camera(tr.from, tr.to, t);
            if tr.elapsed_ms >= tr.duration_ms {
                self.transition = None;
                return Tick {
                    camera: Some(camera),
                    entered_step: None,
                    transition_t: 1.0,
                };
            }
            return Tick {
                camera: Some(camera),
                entered_step: None,
                transition_t: t,
            };
        }
        if self.playing {
            self.elapsed_ms += dt_ms;
            let hold = durations[self.step].max(1) as f32;
            if self.elapsed_ms >= hold && self.step + 1 < durations.len() {
                self.step += 1;
                self.elapsed_ms = 0.0;
                return Tick {
                    camera: None,
                    entered_step: Some(self.step),
                    transition_t: 0.0,
                };
            }
            if self.elapsed_ms >= hold {
                self.playing = false;
                self.elapsed_ms = hold;
            }
        }
        Tick {
            camera: None,
            entered_step: None,
            transition_t: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(t0: f64, t1: f64, y: f32) -> Camera {
        Camera {
            t0_ns: t0,
            t1_ns: t1,
            scroll_y: y,
        }
    }

    #[test]
    fn smoothstep_is_flat_at_the_ends_and_even_in_the_middle() {
        assert_eq!(ease_smoothstep(0.0), 0.0);
        assert_eq!(ease_smoothstep(1.0), 1.0);
        assert!((ease_smoothstep(0.5) - 0.5).abs() < 1e-6);
        assert!(ease_smoothstep(-1.0) == 0.0 && ease_smoothstep(2.0) == 1.0);
        let early = ease_smoothstep(0.1) - ease_smoothstep(0.0);
        let mid = ease_smoothstep(0.55) - ease_smoothstep(0.45);
        assert!(
            mid > early,
            "the middle of the ease moves faster than the start"
        );
        let mut prev = 0.0;
        for i in 1..=20 {
            let v = ease_smoothstep(i as f32 / 20.0);
            assert!(v + 1e-6 >= prev);
            prev = v;
        }
    }

    #[test]
    fn interpolation_holds_the_endpoints_and_zooms_in_log_space() {
        let from = cam(0.0, 1_000.0, 0.0);
        let to = cam(10_000.0, 10_100.0, 80.0);
        let start = interpolate_camera(from, to, 0.0);
        assert!((start.t0_ns - from.t0_ns).abs() < 1e-6);
        assert!((start.t1_ns - from.t1_ns).abs() < 1e-6);
        assert_eq!(start.scroll_y, 0.0);
        let end = interpolate_camera(from, to, 1.0);
        assert!((end.t0_ns - to.t0_ns).abs() < 1e-4, "{}", end.t0_ns);
        assert!((end.t1_ns - to.t1_ns).abs() < 1e-4, "{}", end.t1_ns);
        assert!((end.scroll_y - 80.0).abs() < 1e-4);
        // At the smoothstep midpoint the duration is the geometric mean.
        let mid = interpolate_camera(from, to, 0.5);
        let dur = mid.t1_ns - mid.t0_ns;
        let geometric = (1_000.0_f64 * 100.0).sqrt();
        assert!((dur - geometric).abs() < 1e-3, "{dur} vs {geometric}");
        let center = (mid.t0_ns + mid.t1_ns) * 0.5;
        let from_c = 500.0;
        let to_c = 10_050.0;
        assert!((center - (from_c + to_c) * 0.5).abs() < 1e-3, "{center}");
        assert!((mid.scroll_y - 40.0).abs() < 1e-3);
    }

    #[test]
    fn overlay_reveal_lags_the_camera_then_finishes_with_it() {
        assert_eq!(overlay_reveal(0.0), 0.0);
        assert_eq!(overlay_reveal(0.15), 0.0);
        assert_eq!(overlay_reveal(1.0), 1.0);
        assert!(overlay_reveal(0.5) > 0.2 && overlay_reveal(0.5) < 0.8);
    }

    #[test]
    fn play_advances_by_each_steps_duration_and_stops_at_the_end() {
        let mut player = Player { playing: true, ..Default::default() };
        player.begin_transition(cam(0.0, 100.0, 0.0), cam(0.0, 50.0, 10.0), 100.0);
        let mid = player.tick(50.0, &[1_000, 1_000]);
        assert!(mid.camera.is_some());
        assert!(mid.transition_t > 0.0 && mid.transition_t < 1.0);
        let done = player.tick(50.0, &[1_000, 1_000]);
        assert_eq!(done.transition_t, 1.0);
        assert!(player.transition.is_none());
        let hold = player.tick(1_000.0, &[1_000, 1_000]);
        assert_eq!(hold.entered_step, Some(1));
        assert_eq!(player.step, 1);
        player.begin_transition(cam(0.0, 50.0, 10.0), cam(20.0, 40.0, 0.0), 10.0);
        let _ = player.tick(10.0, &[1_000, 500]);
        let end = player.tick(500.0, &[1_000, 500]);
        assert!(end.entered_step.is_none());
        assert!(!player.playing);
        assert_eq!(player.step, 1);
    }

    #[test]
    fn next_and_prev_clamp_and_do_not_refire_on_the_same_step() {
        let mut player = Player::default();
        assert_eq!(player.next(3), Some(1));
        assert_eq!(player.step, 1);
        assert_eq!(player.go(1, 3), None);
        assert_eq!(player.prev(), Some(0));
        assert_eq!(player.prev(), None);
        assert_eq!(player.go(9, 3), Some(2));
        assert_eq!(player.step, 2);
        assert_eq!(player.next(0), None);
    }
}
