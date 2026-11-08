// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opens the viewer on the box3d demo story.
//!
//! The lanes are a small in-process preview, not `box3d.perfr.stream`.
//! `play` (the default) auto-plays. `edit` opens the editor.

fn main() -> eframe::Result {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "play".into());
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("perfr story"),
        ..Default::default()
    };
    eframe::run_native(
        "perfr story",
        native,
        Box::new(move |cc| {
            let mut app = perfr_live_viewer::PerfrLiveApp::new(cc);
            app.seed_story_preview();
            app.load_story(&perfr_story::demo_box3d_json())
                .expect("demo story");
            if mode == "edit" {
                app.open_story_editor();
            } else {
                app.play_story();
            }
            Ok(Box::new(app))
        }),
    )
}
