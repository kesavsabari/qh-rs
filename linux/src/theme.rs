//! One place for the Zen colours and every stylesheet (apple/qh/Theme.swift).
//!
//! The strategy: the three widget crates' stylesheets go in one base provider; the app's own rules
//! and the colour tokens go in a provider one step above it, so they win whatever the crates say
//! (their own light/dark blocks included). Dark is a third provider with only the tokens, added
//! while libadwaita's StyleManager is dark and removed when it isn't — so System/Light/Dark in
//! Settings (StyleManager's colour scheme) is the single switch every class follows.

use gtk::gdk;

/// (token, light, dark). Every colour in the app and the crates is `var(--qh-<token>)`.
const TOKENS: &[(&str, &str, &str)] = &[
    ("ground", "#ECEBE6", "#141413"),
    ("card", "#FBFBF9", "#1D1D1B"),
    ("paper", "#FBFBF9", "#1D1D1B"),
    ("tile", "#F0EFEA", "#262624"),
    ("side", "#F0EFEA", "#262624"),
    ("ink", "#0E0E0D", "#ECEBE6"),
    ("soft", "#6B6B66", "#9C9B94"),
    ("faint", "#9A9A94", "#85847D"),
    ("rule", "#DCDCD5", "#34342F"),
    ("line", "#E3E2DC", "#31312E"),
    ("hover", "#E6E6E0", "#2E2E2B"),
    ("accent", "#FF4D1C", "#FF5A2A"),
    ("red", "#C4342B", "#FF6B5E"),
    ("green", "#2F7D3A", "#8FD18A"),
    ("add", "#E6F4E4", "#1E3320"),
    ("del", "#FBE7E3", "#3A1E1B"),
    ("pick", "rgba(255,77,28,0.13)", "rgba(255,90,42,0.20)"),
    ("k", "#B3360F", "#FF8A5C"),
    ("s", "#2F7D3A", "#8FD18A"),
    ("c", "#9A9A94", "#6F6E68"),
    ("n", "#373FF5", "#9AA0FF"),
];

/// Ground colour for snapshots (a toplevel paints nothing through a snapshot of its child).
pub fn ground(dark: bool) -> &'static str {
    if dark { TOKENS[0].2 } else { TOKENS[0].1 }
}

fn tokens(dark: bool) -> String {
    // the crates define their tokens on these too; same selectors here, higher priority
    let mut s = String::from(":root, .qh-code, .qh-switch, .qh-card, .qh-palette {\n");
    for (k, l, d) in TOKENS {
        s.push_str(&format!("  --qh-{k}: {};\n", if dark { d } else { l }));
    }
    s.push_str("}\n");
    s
}

fn provider(css: &str) -> gtk::CssProvider {
    let p = gtk::CssProvider::new();
    p.connect_parsing_error(|_, sec, e| eprintln!("qh-app: css: {e} at {}", sec.to_str()));
    p.load_from_string(css);
    p
}

/// Install every stylesheet once, after startup, and follow StyleManager's dark flag.
pub fn install() {
    let Some(d) = gdk::Display::default() else { return };
    let prio = gtk::STYLE_PROVIDER_PRIORITY_APPLICATION;
    // the thread crate also calls markdown code blocks .qh-code: scope those under .qh-md so they
    // never style the code view (which is .qh-code in the code crate)
    let thread_css = qh_thread::CSS.replace(".qh-code", ".qh-md .qh-code");
    let base = format!("{}\n{}", qh_code::CSS, thread_css);
    gtk::style_context_add_provider_for_display(&d, &provider(&base), prio);
    let app = format!("{}\n{}", tokens(false), include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&d, &provider(&app), prio + 1);
    let dark = provider(&tokens(true));
    let sm = adw::StyleManager::default();
    let apply = move |sm: &adw::StyleManager| {
        if sm.is_dark() {
            gtk::style_context_add_provider_for_display(&d, &dark, prio + 2);
        } else {
            gtk::style_context_remove_provider_for_display(&d, &dark);
        }
    };
    apply(&sm);
    sm.connect_dark_notify(apply);
}

/// "system" | "light" | "dark" → StyleManager's scheme (everything above follows it).
pub fn set_appearance(v: &str) {
    adw::StyleManager::default().set_color_scheme(match v {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
}

/// Geist ships inside the binary: written once to the cache dir, then given to fontconfig as app
/// fonts before GTK makes its font map. The only font registration in the app.
pub fn register_fonts() {
    #[link(name = "fontconfig")]
    extern "C" {
        fn FcConfigAppFontAddFile(config: *mut std::ffi::c_void, file: *const std::ffi::c_char) -> i32;
    }
    const FONTS: [(&str, &[u8]); 4] = [
        ("Geist-Regular.ttf", include_bytes!("../../apple/qh/Fonts/Geist-Regular.ttf")),
        ("Geist-Medium.ttf", include_bytes!("../../apple/qh/Fonts/Geist-Medium.ttf")),
        ("GeistMono-Regular.ttf", include_bytes!("../../apple/qh/Fonts/GeistMono-Regular.ttf")),
        ("GeistMono-Medium.ttf", include_bytes!("../../apple/qh/Fonts/GeistMono-Medium.ttf")),
    ];
    let dir = gtk::glib::user_cache_dir().join("qh-app").join("fonts");
    let _ = std::fs::create_dir_all(&dir);
    for (name, bytes) in FONTS {
        let p = dir.join(name);
        if std::fs::metadata(&p).map(|m| m.len() as usize).ok() != Some(bytes.len()) {
            let _ = std::fs::write(&p, bytes);
        }
        if let Ok(c) = std::ffi::CString::new(p.to_string_lossy().as_bytes()) {
            unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), c.as_ptr()) };
        }
    }
}
