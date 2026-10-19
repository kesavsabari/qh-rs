//! Palette demo. Env:
//!   QH_SNAPSHOT=/path.png  render window + palette to PNG after ~3 s, exit
//!   QH_TRAY_DEMO=1         start the tray, cycle states for 10 s, exit
//!   QH_NOTIFY_DEMO=1       send one ask notification, print the answer, exit
//!   QH_FONTS=dir           register Geist .ttf files from dir
use gtk::prelude::*;
use gtk::{gio, glib};
use qh_desktop::{Item, Notifier, Palette, Tray};
use std::time::Duration;

fn main() -> glib::ExitCode {
    let app = gtk::Application::new(Some("dev.qh.DesktopDemo"), gio::ApplicationFlags::NON_UNIQUE);
    app.connect_activate(activate);
    app.run_with_args::<&str>(&[])
}

fn activate(app: &gtk::Application) {
    if let Ok(d) = std::env::var("QH_FONTS") {
        eprintln!("fonts registered: {}", qh_desktop::theme::register_fonts(d.as_ref()));
    }
    if std::env::var("QH_TRAY_DEMO").is_ok() {
        return tray_demo(app);
    }
    if std::env::var("QH_NOTIFY_DEMO").is_ok() {
        return notify_demo(app);
    }
    let win = gtk::ApplicationWindow::builder().application(app).title("qh palette demo").default_width(900).default_height(560).build();
    let bg = gtk::Box::new(gtk::Orientation::Vertical, 0);
    win.set_child(Some(&bg));
    let pal = Palette::new();
    let mut items = vec![];
    for (t, h) in [
        ("src/main.rs", "acme-demo"), ("src/tray.rs", "acme-demo"), ("linux/src/window.rs", "acme-demo"),
        ("README.md", "acme-demo"), ("New tab", "⌘T"), ("Open project…", "⌘O"), ("Ask all agents", "command"),
        ("Toggle sidebar", "⌘\\"), ("tests/palette_score.rs", "acme-demo"), ("Settings", "⌘,"),
    ] {
        let t2 = t.to_string();
        items.push(Item::new(t, h, move || println!("picked {t2}")));
    }
    pal.set_items(items);
    win.present();
    let snap = std::env::var("QH_SNAPSHOT").ok();
    if snap.is_some() {
        // Popovers are separate surfaces; for the snapshot, host the card inline.
        pal.popover().set_child(None::<&gtk::Widget>);
        let card = pal.card();
        card.set_halign(gtk::Align::Center);
        card.set_margin_top(56);
        bg.append(card);
        pal.entry().set_text(&std::env::var("QH_QUERY").unwrap_or_default());
        pal.entry().grab_focus_without_selecting();
    } else {
        let (p, b) = (pal.clone(), bg.clone());
        glib::timeout_add_local_once(Duration::from_millis(400), move || p.show(&b));
    }
    if let Some(path) = snap {
        let paint = gtk::WidgetPaintable::new(Some(&bg));
        let (p, w) = (pal.clone(), win.clone());
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            snapshot(&w, &p, &paint, &path);
            w.application().unwrap().quit();
        });
    }
}

fn snapshot(win: &gtk::ApplicationWindow, pal: &Palette, paint: &gtk::WidgetPaintable, path: &str) {
    let (w, h) = (win.width() as f64, win.height() as f64);
    let card = pal.card();
    let s = gtk::Snapshot::new();
    let dark = card.has_css_class("qh-dark");
    let ground = if dark { gtk::gdk::RGBA::new(0.078, 0.078, 0.075, 1.0) } else { gtk::gdk::RGBA::new(0.925, 0.922, 0.902, 1.0) };
    s.append_color(&ground, &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
    let (cw, ch) = (card.width() as f64, card.height() as f64);
    eprintln!("paintable {}x{}", paint.intrinsic_width(), paint.intrinsic_height());
    paint.snapshot(&s, w, h);
        let node = s.to_node().expect("node");
    let tex = win.native().unwrap().renderer().unwrap().render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    tex.save_to_png(path).expect("save png");
    println!("wrote {path} ({w}x{h}, card {cw}x{ch})");
}

fn tray_demo(app: &gtk::Application) {
    let hold = app.hold();
    let tray = Tray::start(|| println!("tray: open"), || println!("tray: quit")).expect("tray");
    let states = [(0, 0), (1, 0), (2, 0), (3, 0), (1, 1), (3, 2), (0, 0)];
    let mut i = 0;
    glib::timeout_add_local(Duration::from_millis(1500), move || {
        let (r, w) = states[i % states.len()];
        println!("tray: {}", qh_desktop::tray::tooltip(r, w));
        tray.update(r, w);
        i += 1;
        glib::ControlFlow::Continue
    });
    let app = app.clone();
    glib::timeout_add_local_once(Duration::from_secs(10), move || {
        drop(hold);
        app.quit();
    });
}

fn notify_demo(app: &gtk::Application) {
    let hold = std::rc::Rc::new(std::cell::RefCell::new(Some(app.hold())));
    let (a, h) = (app.clone(), hold.clone());
    let n = Notifier::new(move |key, ans| {
        println!("answer: {key} {ans}");
        h.borrow_mut().take();
        a.quit();
    })
    .expect("dbus");
    n.notify_ask("qh test", "Bash", "cargo test --workspace", "demo-ask-1");
    println!("sent; click the notification (Open) or wait");
    let a = app.clone();
    let wait = std::env::var("QH_NOTIFY_WAIT").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    glib::timeout_add_local_once(Duration::from_secs(wait), move || {
        println!("answer: none (timeout)");
        n.clear("demo-ask-1");
        hold.borrow_mut().take();
        a.quit();
    });
}
