//! qh for Linux: a GTK4 + libadwaita client of `qh serve`, mirroring the Mac and Windows apps
//! (apple/qh, windows/): the Zen layout, a sidebar on a warm ground and the chat floating on it as a card.

mod client;
mod composer;
mod hooks;
mod model;
mod pages;
mod theme;
mod threads;
mod ui;

use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

pub const APP_ID: &str = "dev.qh.App";

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let peek = args.iter().any(|a| a == "--peek");
    let folder = args.iter().skip(1).find(|a| !a.starts_with("--"))
        .map(|a| std::fs::canonicalize(a).map(|p| p.to_string_lossy().to_string()).unwrap_or(a.clone()));

    let app = adw::Application::builder().application_id(APP_ID).flags(if std::env::var_os("QH_APP_SNAPSHOT").is_some() { gio::ApplicationFlags::NON_UNIQUE } else { gio::ApplicationFlags::empty() }).build();
    let (tx, rx) = async_channel::unbounded();
    let state = Rc::new(RefCell::new(model::App::new(tx)));
    let start = Rc::new(RefCell::new((peek, folder.clone())));

    {
        let state = state.clone();
        app.connect_startup(move |app| {
            theme::register_fonts();
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

