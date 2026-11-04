// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opens the viewer on a tiny built-in capture so the log window, the log
//! ticks, and a clicked recurrent scope can be seen without a service.

fn main() -> eframe::Result {
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "perfr logs",
        native,
        Box::new(|cc| {
            let mut app = perfr_live_viewer::PerfrLiveApp::new(cc);
            app.seed_log_preview();
            Ok(Box::new(app))
        }),
    )
}
