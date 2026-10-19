//! Render tray icon states to a PNG strip: rows = dark bar (light rings),
//! light bar (dark rings); each row at 48 and 22 px. Usage: icons OUT.png
use qh_desktop::icon::{draw, Variant};

fn main() {
    let out = std::env::args().nth(1).unwrap_or("tray-icons.png".into());
    let states = [(0, 0), (1, 0), (2, 0), (3, 0), (1, 1), (2, 1), (3, 2)];
    let cell = 64u32;
    let (w, h) = (cell * states.len() as u32, cell * 4);
    let mut img = vec![0u8; (w * h * 4) as usize];
    for (row, (var, bg, sz)) in [(Variant::Dark, 0x141413u32, 48u32), (Variant::Dark, 0x141413, 22), (Variant::Light, 0xECEBE6, 48), (Variant::Light, 0xECEBE6, 22)].iter().enumerate() {
        for (col, &(r, wt)) in states.iter().enumerate() {
            let px = draw(*sz, r, wt, *var);
            for y in 0..cell {
                for x in 0..cell {
                    let (ox, oy) = ((cell - sz) / 2, (cell - sz) / 2);
                    let mut c = *bg;
                    if x >= ox && y >= oy && x < ox + sz && y < oy + sz {
                        let p = px[((y - oy) * sz + (x - ox)) as usize];
                        let a = (p >> 24) as f64 / 255.0;
                        let mix = |s: u32| ((((p >> s) & 0xFF) as f64 * a + ((bg >> s) & 0xFF) as f64 * (1.0 - a)) as u32) << s;
                        c = mix(16) | mix(8) | mix(0);
                    }
                    let i = (((row as u32 * cell + y) * w + col as u32 * cell + x) * 4) as usize;
                    img[i..i + 4].copy_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8, 255]);
                }
            }
        }
    }
    let f = std::fs::File::create(&out).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&img).unwrap();
    println!("wrote {out}");
}
