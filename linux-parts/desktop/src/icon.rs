//! Tray icon drawn in code — same geometry as windows/Tray.cs Draw(), scaled
//! from its 32px design to any size: a dot when idle, up to three rings for
//! running agents, the first ring orange when one is waiting.
use crate::theme::{INK_DARK, INK_LIGHT, ORANGE};

/// Which bar the icon sits on. `Dark` = light rings (Omarchy's bar is dark).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Dark,
    Light,
}

/// Straight (non-premultiplied) ARGB pixels, row-major, `size*size` u32s.
pub fn draw(size: u32, running: u32, waiting: u32, variant: Variant) -> Vec<u32> {
    let s = size as usize;
    let k = size as f64 / 32.0;
    let ink = if variant == Variant::Dark { INK_DARK } else { INK_LIGHT };
    let mut px = vec![0u32; s * s];
    let n = running.max(if waiting > 0 { 1 } else { 0 }).min(3);
    let mut ring = |cx: f64, cy: f64, r: f64, w: f64, c: u32, fill: bool| {
        for y in 0..s {
            for x in 0..s {
                let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
                let d = (dx * dx + dy * dy).sqrt();
                let a = if fill { (r - d + 0.5).clamp(0.0, 1.0) } else { (w / 2.0 - (d - r).abs() + 0.5).clamp(0.0, 1.0) };
                if a > 0.0 {
                    px[y * s + x] = (((a * 255.0) as u32) << 24) | (c & 0xFF_FFFF);
                }
            }
        }
    };
    let c = 16.0 * k;
    if n == 0 {
        ring(c, c, 4.5 * k, 0.0, ink, true);
    } else {
        // Windows uses r=4.6/gap 2.5 for three rings, which clips the outer
        // strokes at 32px; 4.0/2.0 keeps all three inside the square.
        let (r, g) = match n { 1 => (9.0, 2.5), 2 => (6.5, 2.5), _ => (4.0, 2.0) };
        let (r, gap) = (r * k, (2.0 * r + g) * k);
        let x0 = c - (n as f64 - 1.0) * gap / 2.0;
        for i in 0..n {
            let col = if i == 0 && waiting > 0 { ORANGE } else { ink };
            ring(x0 + i as f64 * gap, c, r, 2.6 * k, col, false);
        }
    }
    px
}

/// SNI pixmaps (ARGB32, network byte order) at 22, 32 and 48 px.
pub fn pixmaps(running: u32, waiting: u32, variant: Variant) -> Vec<ksni::Icon> {
    [22u32, 32, 48]
        .iter()
        .map(|&sz| ksni::Icon {
            width: sz as i32,
            height: sz as i32,
            data: draw(sz, running, waiting, variant).iter().flat_map(|p| p.to_be_bytes()).collect(),
        })
        .collect()
}
