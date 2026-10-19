//! Linux desktop parts for qh: a StatusNotifierItem tray, actionable ask
//! notifications over org.freedesktop.Notifications, and a command palette.
//!
//! Standalone: no dependency on the app. Tray runs on ksni's own thread
//! (blocking API, async-io executor — no tokio). Notifications and the
//! palette run on the GLib main loop of the caller's GTK application.

pub mod icon;
pub mod notify;
pub mod palette;
pub mod theme;
pub mod tray;

pub use notify::Notifier;
pub use palette::{Item, Palette};
pub use tray::{Tray, TrayHandle};
