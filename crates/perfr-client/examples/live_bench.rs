// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// A real native window receiving a real service stream. No synthetic update
// calls and no Self pane. The window and renderer are the shipped client's
// own (`perfr_viewer::native_options`), but for a size that cannot change,
// so that every trial draws the same number of pixels.
use perfr_viewer::eframe::{self, egui};
use std::time::Instant;

struct Measured {
    app: perfr_viewer::PerfrLiveApp,
    start: Instant,
    last: Instant,
    until: f64,
    output: String,
    frames: Vec<[f64; 4]>,
    counters: Vec<serde_json::Value>,
    next_counter: f64,
}

impl eframe::App for Measured {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let now = Instant::now();
        let seconds = now.duration_since(self.start).as_secs_f64();
        let period_ms = now.duration_since(self.last).as_secs_f64() * 1000.0;
        self.last = now;
        self.app.update(ctx, frame);
        let update_ms = now.elapsed().as_secs_f64() * 1000.0;
        // The GPU callback runs after update(), so this is what the frame
        // before this one took there. Read on every frame: a reading taken
        // once a second sees one frame in sixty.
        let (gpu_prepare_us, _) = self.app.gpu_times_us();
        self.frames.push([seconds, period_ms, update_ms, f64::from(gpu_prepare_us)]);
        if seconds >= self.next_counter {
            self.counters
                .push(serde_json::json!({"seconds":seconds,"state":self.app.profile_readout()}));
            self.next_counter = seconds + 1.0;
        }
        if seconds >= self.until {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl Drop for Measured {
    fn drop(&mut self) {
        let data = serde_json::json!({
            "frame_columns":["seconds","period_ms","update_ms","gpu_prepare_us"],
            "frames":self.frames,"counters":self.counters,
        });
        std::fs::write(&self.output, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: live_bench SERVICE_ORIGIN SECONDS OUTPUT.json".into());
    }
    let host = perfr_client::native_host(&args[1])?;
    let until: f64 = args[2].parse()?;
    let output = args[3].clone();
    let mut options = perfr_viewer::native_options();
    options.viewport = options.viewport.with_resizable(false);
    eframe::run_native(
        "perfr live benchmark",
        options,
        Box::new(move |cc| {
            let now = Instant::now();
            Ok(Box::new(Measured {
                app: perfr_viewer::PerfrLiveApp::new(cc, host),
                start: now,
                last: now,
                until,
                output,
                frames: Vec::with_capacity(10000),
                counters: Vec::new(),
                next_counter: 0.0,
            }))
        }),
    )
    .map_err(|e| e.to_string().into())
}
