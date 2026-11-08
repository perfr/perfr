// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Opens the viewer on the box3d demo story.
//!
//! The lanes are a small in-process preview, not `box3d.perfr.stream`.
//! `play` (the default) auto-plays. `edit` opens the editor.
//! `about [png]` opens About and, when a path is given, writes that
//! screenshot and exits.

fn main() -> eframe::Result {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "play".into());
    let shot = std::env::args().nth(2);
    let native = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title(if mode == "about" {
                "perfr"
            } else {
                "perfr story"
            }),
        ..Default::default()
    };
    eframe::run_native(
        "perfr story",
        native,
        Box::new(move |cc| {
            let mut app = perfr_live_viewer::PerfrLiveApp::new(cc);
            app.seed_story_preview();
            if mode == "about" {
                app.open_about();
                return Ok(Box::new(AboutShot {
                    inner: app,
                    frames: 0,
                    path: shot,
                    saved: false,
                }));
            }
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

/// Grabs one frame after About has laid out, for `docs/images/`.
struct AboutShot {
    inner: perfr_live_viewer::PerfrLiveApp,
    frames: u32,
    path: Option<String>,
    saved: bool,
}

impl eframe::App for AboutShot {
    fn update(&mut self, ctx: &eframe::egui::Context, frame: &mut eframe::Frame) {
        self.inner.update(ctx, frame);
        let Some(path) = &self.path else {
            return;
        };
        self.frames += 1;
        if self.frames == 8 && !self.saved {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Screenshot(
                eframe::egui::UserData::default(),
            ));
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                eframe::egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            if !self.saved {
                write_png(std::path::Path::new(path), &image).expect("write about screenshot");
                self.saved = true;
                ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            }
        }
        ctx.request_repaint();
    }
}

fn write_png(path: &std::path::Path, image: &eframe::egui::ColorImage) -> std::io::Result<()> {
    use std::io::Write;
    let width = image.size[0] as u32;
    let height = image.size[1] as u32;
    let mut raw = Vec::with_capacity((width as usize + 1) * height as usize * 4);
    for y in 0..height as usize {
        raw.push(0);
        let row = y * width as usize;
        for x in 0..width as usize {
            raw.extend_from_slice(&image.pixels[row + x].to_srgba_unmultiplied());
        }
    }
    let compressed = compress_store(&raw);
    let mut png = Vec::new();
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &compressed);
    chunk(&mut png, b"IEND", &[]);
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(&png)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_bytes = Vec::with_capacity(4 + data.len());
    crc_bytes.extend_from_slice(kind);
    crc_bytes.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_bytes).to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// zlib wrapper around stored (uncompressed) deflate blocks.
fn compress_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0;
    while i < data.len() || data.is_empty() && i == 0 {
        let end = (i + 65535).min(data.len());
        let last = end == data.len();
        let block = &data[i..end];
        let len = block.len() as u16;
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
        if data.is_empty() {
            break;
        }
        i = end;
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}
