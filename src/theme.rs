//! The TUI's colours: three levels of ink on the terminal's own background, a hairline grey, and
//! one orange that only marks live things (the eye, what needs you, the working session).
//! Terminals without 24-bit colour (Windows conhost, plain xterm) get the nearest of the 16.

use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

fn truecolor() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| {
        if let Ok(v) = std::env::var("QH_COLOR") { return v != "16" }
        let ct = std::env::var("COLORTERM").unwrap_or_default();
        ct.contains("truecolor") || ct.contains("24bit")
            || std::env::var("WT_SESSION").is_ok() // Windows Terminal
            || matches!(std::env::var("TERM_PROGRAM").as_deref(), Ok("iTerm.app" | "WezTerm" | "vscode" | "ghostty"))
    })
}

fn pick(rgb: (u8, u8, u8), basic: Color) -> Color {
    if truecolor() { Color::Rgb(rgb.0, rgb.1, rgb.2) } else { basic }
}

/// Names, the user's words, replies.
pub fn ink() -> Color { pick((239, 238, 233), Color::White) }
/// Secondary text: tool titles, notes.
pub fn ink2() -> Color { pick((196, 195, 189), Color::Gray) }
/// Hints, times, paths.
pub fn dim() -> Color { pick((141, 140, 134), Color::DarkGray) }
/// Hairlines between areas.
pub fn line() -> Color { pick((52, 52, 48), Color::DarkGray) }
/// The one accent: live, needs you, working.
pub fn orange() -> Color { pick((255, 77, 28), Color::LightRed) }
/// A dimmed orange for the thinking pulse.
pub fn ember() -> Color { pick((110, 40, 20), Color::Red) }
/// Diffs keep their meaning colours.
pub fn added() -> Color { pick((120, 190, 120), Color::Green) }
pub fn removed() -> Color { pick((220, 110, 100), Color::Red) }

pub fn fg(c: Color) -> Style { Style::new().fg(c) }
pub fn bold(c: Color) -> Style { Style::new().fg(c).add_modifier(Modifier::BOLD) }
/// A selected row or the team chip: dark text on light ink.
pub fn inverted() -> Style { Style::new().fg(Color::Black).bg(ink()).add_modifier(Modifier::BOLD) }
/// What needs you, selected: dark text on orange.
pub fn hot() -> Style { Style::new().fg(Color::Black).bg(orange()).add_modifier(Modifier::BOLD) }
