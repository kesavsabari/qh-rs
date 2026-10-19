//! StatusNotifierItem tray via ksni (blocking API; runs on its own thread).
//! Callbacks fire on the tray thread — hop to the GTK main thread yourself,
//! e.g. with `glib::MainContext::default().invoke(..)`.
use crate::icon::{self, Variant};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use std::sync::Arc;

type Cb = Arc<dyn Fn() + Send + Sync>;

pub struct Tray {
    running: u32,
    waiting: u32,
    variant: Variant,
    on_open: Cb,
    on_quit: Cb,
}

pub struct TrayHandle(ksni::blocking::Handle<Tray>);

impl Tray {
    /// Start the tray with light rings (for dark bars such as Omarchy's).
    pub fn start(on_open: impl Fn() + Send + Sync + 'static, on_quit: impl Fn() + Send + Sync + 'static) -> Result<TrayHandle, ksni::Error> {
        Self::start_with(Variant::Dark, on_open, on_quit)
    }

    pub fn start_with(
        variant: Variant,
        on_open: impl Fn() + Send + Sync + 'static,
        on_quit: impl Fn() + Send + Sync + 'static,
    ) -> Result<TrayHandle, ksni::Error> {
        let t = Tray { running: 0, waiting: 0, variant, on_open: Arc::new(on_open), on_quit: Arc::new(on_quit) };
        // Unique bus name per process (ksni default); fine for one app instance.
        t.spawn().map(TrayHandle)
    }
}

pub fn tooltip(running: u32, waiting: u32) -> String {
    format!("qh · {running} working · {waiting} waiting")
}

impl TrayHandle {
    /// Redraws the icon and tooltip; no-op if nothing changed.
    pub fn update(&self, running: u32, waiting: u32) {
        self.0.update(|t| {
            t.running = running;
            t.waiting = waiting;
        });
    }
    pub fn set_variant(&self, v: Variant) {
        self.0.update(|t| t.variant = v);
    }
    pub fn shutdown(&self) {
        let _ = self.0.shutdown();
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "qh".into()
    }
    fn title(&self) -> String {
        "qh".into()
    }
    fn status(&self) -> ksni::Status {
        if self.waiting > 0 { ksni::Status::NeedsAttention } else { ksni::Status::Active }
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icon::pixmaps(self.running, self.waiting, self.variant)
    }
    fn attention_icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon_pixmap()
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: tooltip(self.running, self.waiting), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_open)()
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem { label: "Open qh".into(), activate: Box::new(|t: &mut Self| (t.on_open)()), ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem { label: "Quit qh".into(), activate: Box::new(|t: &mut Self| (t.on_quit)()), ..Default::default() }.into(),
        ]
    }
}
