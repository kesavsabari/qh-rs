//! qh's chat thread as GTK4/libadwaita widgets, in the Zen design of the Mac and Windows apps:
//! paper and ink, one signal orange, Geist. No dependency on the app.

pub mod cards;
pub mod markdown;
pub mod thread;

pub use cards::{Answer, Ask, AskCard, PlanCard, Step};
pub use markdown::{render, render_with, Links};
pub use thread::{ReplyHandle, Thread, ToolHandle};

use adw::prelude::*;

/// Every class the widgets use. Light by default, dark under prefers-color-scheme (which
/// libadwaita's style manager drives).
pub const CSS: &str = include_str!("qh-thread.css");

/// The dark palette alone, for when libadwaita's StyleManager is dark but the CSS media query
/// is not (forced schemes, ADW_DEBUG_COLOR_SCHEME). [`install_css`] swaps it in and out.
pub const CSS_DARK: &str = ":root { --qh-card: #1D1D1B; --qh-tile: #262624; --qh-ink: #ECEBE6; --qh-soft: #9C9B94; \
  --qh-faint: #85847D; --qh-rule: #34342F; --qh-line: #31312E; --qh-hover: #2E2E2B; \
  --qh-accent: #FF5A2A; --qh-red: #FF6B5E; --qh-green: #8FD18A; }";

/// Load [`CSS`] for the default display, plus [`CSS_DARK`] whenever adw::StyleManager is dark.
/// Call once, after the application has started up.
pub fn install_css() {
    let Some(d) = gtk::gdk::Display::default() else { return };
    let p = gtk::CssProvider::new();
    p.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(&d, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let dark = gtk::CssProvider::new();
    dark.load_from_string(CSS_DARK);
    let sm = adw::StyleManager::default();
    let apply = move |sm: &adw::StyleManager| {
        if sm.is_dark() {
            gtk::style_context_add_provider_for_display(&d, &dark, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
        } else {
            gtk::style_context_remove_provider_for_display(&d, &dark);
        }
    };
    apply(&sm);
    sm.connect_dark_notify(apply);
}

/// Register the Geist .ttf files in `dir` with GTK's font map (process only). Call after
/// gtk::init and before building widgets. Returns how many files were added.
pub fn register_fonts(dir: &std::path::Path) -> usize {
    let probe = gtk::Label::new(None);
    let Some(map) = probe.pango_context().font_map() else { return 0 };
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "ttf" || x == "otf") && map.add_font_file(&p).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Fade and slide a freshly added widget in (180 ms). libadwaita skips the animation when
/// gtk-enable-animations is off.
pub(crate) fn appear(w: &impl IsA<gtk::Widget>) {
    let w = w.as_ref().clone();
    let base = w.margin_top();
    w.set_opacity(0.0);
    let ww = w.clone();
    let target = adw::CallbackAnimationTarget::new(move |v| {
        ww.set_opacity(v);
        ww.set_margin_top(base + ((1.0 - v) * 8.0) as i32);
    });
    let a = adw::TimedAnimation::new(&w, 0.0, 1.0, 180, target);
    a.set_easing(adw::Easing::EaseOutCubic);
    a.play();
}

pub(crate) fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    for c in classes {
        l.add_css_class(c);
    }
    l
}
