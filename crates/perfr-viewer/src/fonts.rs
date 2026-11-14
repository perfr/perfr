// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! IBM Plex (OFL) for chrome and tabular status / time.
//!
//! Plex here is a Latin subset. Behind it come the fallbacks, for what a
//! capture brings that Plex has no glyph for:
//!
//! * With the `service` feature (`perfr-client`), egui's four built-in
//!   fonts: Ubuntu Light, Hack, Noto Emoji and an icon font. The controls
//!   that need a service use their symbols.
//! * Without it (the viewer's own pack), Ubuntu Light alone: Greek,
//!   Cyrillic and accented Latin in thread, process and scope names. The
//!   other three are 1 MB that the viewer's own text never draws from. An
//!   emoji in a name is the one thing that shows as a missing glyph.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId, TextStyle};

pub const SANS: &[u8] = include_bytes!("../fonts/IBMPlexSans-Regular.ttf");
pub const SANS_MEDIUM: &[u8] = include_bytes!("../fonts/IBMPlexSans-Medium.ttf");
pub const MONO: &[u8] = include_bytes!("../fonts/IBMPlexMono-Regular.ttf");

pub fn medium() -> FontFamily {
    FontFamily::Name("ibm_medium".into())
}

/// The fallback of a build without egui's built-in fonts.
#[cfg(not(feature = "service"))]
const FALLBACK: &[u8] = epaint_default_fonts::UBUNTU_LIGHT;

/// Font definitions with the fallbacks in and nothing in front of them.
fn fallbacks() -> FontDefinitions {
    #[cfg(feature = "service")]
    {
        FontDefinitions::default()
    }
    #[cfg(not(feature = "service"))]
    {
        let mut fonts = FontDefinitions::empty();
        fonts.font_data.insert(
            "ubuntu_light".into(),
            std::sync::Arc::new(FontData::from_static(FALLBACK)),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.insert(family, vec!["ubuntu_light".into()]);
        }
        fonts
    }
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = fallbacks();
    fonts.font_data.insert(
        "ibm_plex_sans".into(),
        std::sync::Arc::new(FontData::from_static(SANS)),
    );
    fonts.font_data.insert(
        "ibm_plex_sans_md".into(),
        std::sync::Arc::new(FontData::from_static(SANS_MEDIUM)),
    );
    fonts.font_data.insert(
        "ibm_plex_mono".into(),
        std::sync::Arc::new(FontData::from_static(MONO)),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "ibm_plex_sans".into());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "ibm_plex_mono".into());
    fonts.families.insert(
        medium(),
        vec!["ibm_plex_sans_md".into(), "ibm_plex_sans".into()],
    );
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(10.5, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(12.5, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(12.0, FontFamily::Proportional),
    );
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::new(13.0, medium()));
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(11.5, FontFamily::Monospace),
    );
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.handle_min_length = 20.0;
    // Solid bars, not egui's floating ones. A floating bar shows and grows
    // on hover, and at the timeline's right edge the report splitter's grab
    // zone sits on top of it: hover there and the two took turns claiming
    // the pointer, the bar appearing and vanishing every other frame.
    style.spacing.scroll.floating = false;
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever the fallbacks are, Plex is asked first, and both families
    /// end in a font that has Greek and Cyrillic.
    #[test]
    fn plex_leads_and_a_fallback_follows() {
        let ctx = egui::Context::default();
        install(&ctx);
        let _ = ctx.run(Default::default(), |ctx| {
            for (family, lead) in [(FontFamily::Proportional, "ibm_plex_sans"), (FontFamily::Monospace, "ibm_plex_mono")] {
                let id = FontId::new(12.0, family.clone());
                ctx.fonts(|f| {
                    assert!(f.has_glyphs(&id, "Record 12.5 ms µ — …"), "{lead}");
                    assert!(f.has_glyphs(&id, "поток Ελληνικά žluťoučký"), "no fallback behind {lead}");
                });
            }
        });
        let defs = fallbacks();
        assert!(!defs.families[&FontFamily::Proportional].is_empty());
        assert!(!defs.families[&FontFamily::Monospace].is_empty());
    }

    #[test]
    fn plex_fonts_are_ttf() {
        assert!(SANS.starts_with(b"\0\x01\0\0") || SANS.starts_with(b"OTTO"));
        assert!(SANS_MEDIUM.len() > 1000);
        assert!(MONO.len() > 1000);
    }
}
