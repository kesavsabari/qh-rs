//! qh for Linux, L1: a GTK4 + libadwaita client of `qh serve`, mirroring the Mac and Windows apps
//! (apple/qh, windows/): the Zen layout, a sidebar on a warm ground and the chat floating on it as a card.

mod client;
mod model;
mod ui;

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

pub const APP_ID: &str = "dev.qh.App";

/// Geist ships inside the binary: written once to the cache dir, then given to fontconfig as app
/// fonts before GTK makes its font map.
fn register_fonts() {
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
    let dir = glib::user_cache_dir().join("qh-app").join("fonts");
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

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let peek = args.iter().any(|a| a == "--peek");
    let folder = args.iter().skip(1).find(|a| !a.starts_with("--"))
        .map(|a| std::fs::canonicalize(a).map(|p| p.to_string_lossy().to_string()).unwrap_or(a.clone()));

    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::empty()).build();
    let (tx, rx) = async_channel::unbounded();
    let state = Rc::new(RefCell::new(model::App::new(tx)));
    let start = Rc::new(RefCell::new((peek, folder.clone())));

    {
        let state = state.clone();
        app.connect_startup(move |app| {
            register_fonts();
            ui::install(app, &state, rx.clone());
        });
    }
    {
        let (state, start) = (state.clone(), start.clone());
        app.connect_activate(move |app| {
            let (peek, folder) = std::mem::take(&mut *start.borrow_mut());
            ui::activate(app, &state, folder, peek);
        });
    }
    {
        let state = state.clone();
        app.connect_shutdown(move |_| state.borrow().shutdown());
    }

    if app.register(None::<&gio::Cancellable>).is_err() { return glib::ExitCode::FAILURE }
    if app.is_remote() {
        // a qh-app is running: hand it the folder / the peek request and leave
        if let Some(f) = &folder { app.activate_action("open", Some(&f.to_variant())) }
        if peek { app.activate_action("peek", None) } else if folder.is_none() { app.activate() }
        if let Some(c) = app.dbus_connection() { let _ = c.flush_sync(None::<&gio::Cancellable>); }
        return glib::ExitCode::SUCCESS;
    }
    app.run_with_args(&args[..1])
}
