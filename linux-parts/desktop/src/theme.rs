//! Zen colours (apple/qh/Theme.swift) and Geist fonts.
use std::path::Path;

pub const ORANGE: u32 = 0xFF4D1C;
pub const ORANGE_DARK: u32 = 0xFF5A2A;
pub const INK_LIGHT: u32 = 0x0E0E0D; // ink on light surfaces
pub const INK_DARK: u32 = 0xECEBE6; // ink on dark surfaces

/// Register the Geist .ttf files in `dir` (e.g. apple/qh/Fonts or an installed
/// share dir) with GTK's default Pango font map. Call after gtk::init().
/// Returns how many files were added.
pub fn register_fonts(dir: &Path) -> usize {
    use gtk::prelude::*;
    let map = gtk::Label::new(None).pango_context().font_map();
    let Some(map) = map else { return 0 };
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("ttf") && map.add_font_file(&p).is_ok() {
                n += 1;
            }
        }
    }
    n
}
