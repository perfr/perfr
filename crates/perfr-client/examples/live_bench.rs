// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// A real native window receiving a real service stream. No synthetic update
// calls and no Self pane: the same UI/renderer as the shipped client.
use eframe::egui;
use std::time::Instant;

struct Measured {
    app: perfr_viewer::PerfrLiveApp,
    start: Instant,
    last: Instant,
    until: f64,
    output: String,
    frames: Vec<[f64; 3]>,
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
        self.frames.push([seconds, period_ms, update_ms]);
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
            "frame_columns":["seconds","period_ms","update_ms"],
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
    eframe::run_native(
        "perfr live benchmark",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Wgpu,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 800.0])
                .with_resizable(false),
            ..Default::default()
        },
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
