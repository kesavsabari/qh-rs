//! The window (apple/qh/ContentView.swift, ChatView.swift; windows/MainWindow.cs): sidebar on the
//! ground, the chat as a card, no title bar. State lives in `model::App`; every change schedules one
//! coalesced redraw of the parts that are rebuilt from it (the composer and setup inputs persist).

use crate::client::Msg;
use crate::model::{self, App, Item};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Shared = Rc<RefCell<App>>;

#[allow(dead_code)]
struct Ui {
    window: adw::ApplicationWindow,
    tiles: gtk::Box,
    side: gtk::Box,
    tabs: gtk::Box,
    layout: gtk::Box,
    stack: gtk::Stack,
    thread: gtk::Box,
    scroll: gtk::ScrolledWindow,
    empty: gtk::Box,
    setup: gtk::Box,
    setup_models: gtk::Box,
    setup_filter: gtk::SearchEntry,
    status: gtk::Label,
    composer: gtk::Widget,
    text: gtk::TextView,
    placeholder: gtk::Label,
    meta_model: gtk::Label,
    meta_mode: gtk::Label,
    meta_tokens: gtk::Label,
    stop: gtk::Button,
    send: gtk::Button,
    dots: gtk::Box,
    peek: RefCell<Option<(gtk::Window, gtk::Entry, gtk::Label)>>,
}

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
    static PENDING: Cell<bool> = const { Cell::new(false) };
    static SETUP_DIRTY: Cell<bool> = const { Cell::new(true) };
    static THREAD_SIG: RefCell<String> = const { RefCell::new(String::new()) };
}

fn ui() -> Option<Rc<Ui>> { UI.with(|u| u.borrow().clone()) }

const LIGHT: [(&str, &str); 11] = [("ground", "#ECEBE6"), ("card", "#FBFBF9"), ("tile", "#F0EFEA"), ("side", "#F0EFEA"),
    ("ink", "#0E0E0D"), ("soft", "#6B6B66"), ("faint", "#9A9A94"), ("rule", "#DCDCD5"), ("hover", "#E6E6E0"),
    ("accent", "#FF4D1C"), ("line", "#E3E2DC")];
const DARK: [(&str, &str); 11] = [("ground", "#141413"), ("card", "#1D1D1B"), ("tile", "#262624"), ("side", "#262624"),
    ("ink", "#ECEBE6"), ("soft", "#9C9B94"), ("faint", "#85847D"), ("rule", "#34342F"), ("hover", "#2E2E2B"),
    ("accent", "#FF5A2A"), ("line", "#31312E")];

fn css(dark: bool) -> String {
    let mut s: String = (if dark { DARK } else { LIGHT }).iter().map(|(k, v)| format!("@define-color {k} {v};\n")).collect();
    s.push_str(include_str!("style.css"));
    s
}

/// Once, at startup: styles (following the system's light/dark), actions and shortcuts, the event pump.
pub fn install(app: &adw::Application, st: &Shared, rx: async_channel::Receiver<Msg>) {
    let sm = adw::StyleManager::default();
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css(sm.is_dark()));
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    sm.connect_dark_notify(move |sm| provider.load_from_string(&css(sm.is_dark())));

    let action = |name: &str, f: Box<dyn Fn()>| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        app.add_action(&a);
    };
    let s = st.clone();
    action("new-tab", Box::new(move || if let Some(p) = s.borrow().project() { p.send(json!({"type": "new_tab"})) }));
    let s = st.clone();
    action("open-folder", Box::new(move || pick_folder(&s)));
    let s = st.clone();
    action("peek", Box::new(move || show_peek(&s)));
    let s = st.clone();
    action("setup", Box::new(move || {
        if let Some(p) = s.borrow_mut().project_mut() { p.show_setup = !p.show_setup; if p.show_setup { p.load_setup() } }
        SETUP_DIRTY.with(|d| d.set(true));
        schedule(&s);
    }));
    let open = gio::SimpleAction::new("open", Some(glib::VariantTy::STRING));
    let s = st.clone();
    open.connect_activate(move |_, v| {
        if let Some(p) = v.and_then(|v| v.get::<String>()) { s.borrow_mut().open(&p); schedule(&s) }
        if let Some(u) = ui() { u.window.present() }
    });
    app.add_action(&open);
    app.set_accels_for_action("app.new-tab", &["<Control>t"]);
    app.set_accels_for_action("app.open-folder", &["<Control>o"]);

    let s = st.clone();
    glib::spawn_future_local(async move {
        while let Ok(m) = rx.recv().await {
            let setup = {
                let mut a = s.borrow_mut();
                match m {
                    Msg::Event(path, v) => a.by_path(&path).is_some_and(|p| p.handle(&v)),
                    Msg::Closed(path, why) => { if let Some(p) = a.by_path(&path) { p.state = format!("closed: {why}") } true }
                }
            };
            if setup { SETUP_DIRTY.with(|d| d.set(true)) }
            schedule(&s);
        }
    });
}

pub fn activate(app: &adw::Application, st: &Shared, folder: Option<String>, peek: bool) {
    if ui().is_none() {
        build(app, st);
        let first = st.borrow().cfg.recent.iter().find(|p| std::path::Path::new(p).is_dir()).cloned();
        if folder.is_none() { if let Some(f) = first { st.borrow_mut().open(&f) } }
    }
    if let Some(f) = folder { st.borrow_mut().open(&f) }
    schedule(st);
    let u = ui().unwrap();
    let peek_snap = std::env::var("QH_APP_SNAPSHOT_PEEK").is_ok();
    if !peek || u.window.is_visible() { u.window.present() }
    if peek || peek_snap { show_peek(st) }
    // test hooks: QH_APP_PROMPT sends a prompt 3 s in; QH_APP_SNAPSHOT_DELAY (seconds, default 6)
    if let Ok(text) = std::env::var("QH_APP_PROMPT").map(|t| t.strip_prefix('@').and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or(t)) {
        let s = st.clone();
        glib::timeout_add_seconds_local_once(3, move || { if let Some(p) = s.borrow_mut().project_mut() { p.prompt(&text) } schedule(&s) });
    }
    if let Ok(path) = std::env::var("QH_APP_SNAPSHOT") {
        let delay = std::env::var("QH_APP_SNAPSHOT_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(6);
        glib::timeout_add_seconds_local_once(delay, move || {
            let Some(u) = ui() else { return };
            let w: gtk::Widget = if peek_snap { u.peek.borrow().as_ref().map(|p| p.0.clone().upcast()).unwrap_or(u.window.clone().upcast()) }
                else { u.window.clone().upcast() };
            match snapshot_png(&w, &path) { Ok(()) => eprintln!("qh-app: snapshot {path}"), Err(e) => eprintln!("qh-app: snapshot failed: {e}") }
        });
    }
}

/// Renders a window to a PNG without the screen (debug hook for tests).
fn snapshot_png(w: &gtk::Widget, path: &str) -> Result<(), String> {
    let (wd, ht) = (w.width(), w.height());
    if wd == 0 || ht == 0 { return Err("window not allocated".into()) }
    // a toplevel paints nothing through WidgetPaintable: paint its child over the ground colour
    let child = w.first_child().ok_or("empty window")?;
    let snap = gtk::Snapshot::new();
    let dark = adw::StyleManager::default().is_dark();
    let peek = w.has_css_class("peek");
    let bg = gdk::RGBA::parse(if peek { "rgba(0,0,0,0)" } else if dark { "#141413" } else { "#ECEBE6" }).unwrap();
    snap.append_color(&bg, &gtk::graphene::Rect::new(0.0, 0.0, wd as f32, ht as f32));
    // a window on a hidden workspace gets no frame callbacks, so it may never have been laid out
    child.measure(gtk::Orientation::Horizontal, -1);
    child.allocate(wd, ht, -1, None);
    w.snapshot_child(&child, &snap);
    let node = snap.to_node().ok_or("nothing drawn")?;
    let r = w.native().and_then(|n| n.renderer()).ok_or("no renderer")?;
    let tex = r.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, wd as f32, ht as f32)));
    tex.save_to_png(path).map_err(|e| e.to_string())
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes { l.add_css_class(c) }
    l
}

fn icon_button(icon: &str, tip: &str, f: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("plain");
    b.set_tooltip_text(Some(tip));
    b.set_cursor_from_name(Some("pointer"));
    b.connect_clicked(move |_| f());
    b
}

fn clear(b: &gtk::Box) { while let Some(c) = b.first_child() { b.remove(&c) } }

fn build(app: &adw::Application, st: &Shared) {
    let (w, h) = { let a = st.borrow(); (a.cfg.width, a.cfg.height) };
    let window = adw::ApplicationWindow::builder().application(app).title("qh").default_width(w).default_height(h).build();
    window.add_css_class("qh");

    // sidebar
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    sidebar.set_size_request(260, -1);
    sidebar.set_hexpand(false);
    let tiles = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tiles.set_margin_start(12); tiles.set_margin_end(12); tiles.set_margin_top(14);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 2);
    side.set_margin_start(8); side.set_margin_end(8);
    let side_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&side).build();
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    bottom.set_margin_start(14); bottom.set_margin_end(14); bottom.set_margin_top(10); bottom.set_margin_bottom(10);
    let dots = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    bottom.append(&dots);
    bottom.append(&gtk::Box::builder().hexpand(true).build());
    let s = st.clone();
    bottom.append(&icon_button("folder-open-symbolic", "Open folder (Ctrl+O)", move || pick_folder(&s)));
    bottom.append(&icon_button("emblem-system-symbolic", "Settings (coming soon)", || {}));
    let top = gtk::WindowHandle::builder().child(&tiles).build();
    sidebar.append(&top);
    sidebar.append(&side_scroll);
    sidebar.append(&bottom);

    // card: tab bar, main area, composer
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("card");
    card.set_hexpand(true);
    card.set_overflow(gtk::Overflow::Hidden);
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    bar.add_css_class("tabbar");
    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabs.set_valign(gtk::Align::End);
    bar.append(&tabs);
    let plus = gtk::Button::with_label("+");
    plus.add_css_class("plain"); plus.add_css_class("faint");
    plus.set_size_request(30, 34); plus.set_valign(gtk::Align::End);
    plus.set_tooltip_text(Some("New tab (Ctrl+T)"));
    plus.set_action_name(Some("app.new-tab"));
    bar.append(&plus);
    bar.append(&gtk::Box::builder().hexpand(true).build());
    let layout = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    layout.add_css_class("seg-group");
    layout.set_valign(gtk::Align::Center);
    bar.append(&layout);
    let menu = gio::Menu::new();
    menu.append(Some("Connect models"), Some("app.setup"));
    menu.append(Some("Review changes (soon)"), Some("app.none"));
    menu.append(Some("Project memory (soon)"), Some("app.none"));
    let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").menu_model(&menu).build();
    more.add_css_class("plain"); more.set_valign(gtk::Align::Center); more.set_margin_start(6);
    more.set_tooltip_text(Some("More"));
    bar.append(&more);
    let handle = gtk::WindowHandle::builder().child(&bar).build();
    card.append(&handle);

    let thread = gtk::Box::new(gtk::Orientation::Vertical, 14);
    thread.set_margin_top(18); thread.set_margin_bottom(18); thread.set_margin_start(24); thread.set_margin_end(24);
    let clamp = adw::Clamp::builder().maximum_size(720).child(&thread).build();
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build();
    scroll.add_css_class("thread");
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 22);
    empty.set_valign(gtk::Align::Center);
    empty.set_margin_start(24); empty.set_margin_end(24); empty.set_margin_bottom(40);
    let empty_clamp = adw::Clamp::builder().maximum_size(560).child(&empty).vexpand(true).build();
    let status = label("", &["mono", "soft", "small"]);
    status.set_halign(gtk::Align::Center); status.set_valign(gtk::Align::Center); status.set_wrap(true);
    let (setup_page, setup, setup_models, setup_filter) = setup_page(st);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&scroll, Some("thread"));
    stack.add_named(&empty_clamp, Some("empty"));
    stack.add_named(&status, Some("status"));
    stack.add_named(&setup_page, Some("setup"));
    card.append(&stack);

    let (composer, text, placeholder, meta_model, meta_mode, meta_tokens, stop, send) = composer(st);
    card.append(&composer);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&sidebar);
    body.append(&card);

    // window controls only near the top-right corner; Hyprland draws no decorations of its own
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.set_margin_top(14); controls.set_margin_end(18);
    let wc = |icon: &str, tip: &str, f: Box<dyn Fn(&adw::ApplicationWindow)>| {
        let b = gtk::Button::from_icon_name(icon);
        b.add_css_class("wctl"); b.set_tooltip_text(Some(tip));
        let win = window.clone();
        b.connect_clicked(move |_| f(&win));
        controls.append(&b);
    };
    wc("window-minimize-symbolic", "Minimize", Box::new(|w| w.minimize()));
    wc("window-maximize-symbolic", "Maximize", Box::new(|w| if w.is_maximized() { w.unmaximize() } else { w.maximize() }));
    wc("window-close-symbolic", "Close", Box::new(|w| w.close()));
    let reveal = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::Crossfade).transition_duration(120)
        .child(&controls).halign(gtk::Align::End).valign(gtk::Align::Start).build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&body));
    overlay.add_overlay(&reveal);
    let motion = gtk::EventControllerMotion::new();
    let (win, rv) = (window.clone(), reveal.clone());
    motion.connect_motion(move |_, x, y| rv.set_reveal_child(y < 56.0 && x > win.width() as f64 - 170.0));
    let rv = reveal.clone();
    motion.connect_leave(move |_| rv.set_reveal_child(false));
    overlay.add_controller(motion);
    window.set_content(Some(&overlay));

    let s = st.clone();
    window.connect_close_request(move |w| {
        let (wd, ht) = w.default_size();
        let mut a = s.borrow_mut();
        if !w.is_maximized() && wd > 0 && ht > 0 { a.cfg.width = wd; a.cfg.height = ht; a.cfg.save() }
        glib::Propagation::Proceed
    });

    let u = Ui { window, tiles, side, tabs, layout, stack, thread, scroll, empty, setup, setup_models, setup_filter, status,
        composer: composer.upcast(), text, placeholder, meta_model, meta_mode, meta_tokens, stop, send, dots, peek: RefCell::new(None) };
    UI.with(|x| *x.borrow_mut() = Some(Rc::new(u)));
}

/// One redraw for any number of changes in the same ~50 ms.
pub fn schedule(st: &Shared) {
    if PENDING.with(|p| p.replace(true)) { return }
    let s = st.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
        PENDING.with(|p| p.set(false));
        render(&s);
    });
}

fn render(st: &Shared) {
    let Some(u) = ui() else { return };
    let a = st.borrow();
    render_sidebar(st, &a, &u);
    render_tabs(st, &a, &u);
    let p = a.project();
    let page = match p {
        None => { u.status.set_text("Open a folder to start.  Ctrl+O"); "status" }
        Some(p) if p.show_setup => "setup",
        Some(p) if p.state != "live" => {
            u.status.set_text(&if p.state == "starting" { format!("starting qh in {}…", p.name()) } else { p.state.clone() });
            "status"
        }
        Some(p) => if p.current().is_none_or(|t| t.items.is_empty()) { "empty" } else { "thread" },
    };
    u.composer.set_visible(matches!(page, "thread" | "empty"));
    if page == "setup" && SETUP_DIRTY.with(|d| d.replace(false)) { render_setup(st, &a, &u) }
    if page == "empty" { render_empty(st, &a, &u) }
    if page == "thread" { render_thread(&a, &u) }
    u.stack.set_visible_child_name(page);
    if let Some(t) = p.and_then(|p| p.current()) {
        u.meta_model.set_text(&model::short_model(&t.model, &t.provider));
        u.meta_mode.set_text(&p.map(|p| p.mode.clone()).unwrap_or_default());
        u.meta_tokens.set_text(&if t.tokens >= 1000 { format!("{}k tokens", t.tokens / 1000) } else { String::new() });
        u.stop.set_visible(t.busy());
        u.placeholder.set_text(if t.busy() { "qh is working — Esc to stop, or type to queue" } else { "Ask anything · / commands · @ files" });
    }
    if let Some((_, e, hint)) = u.peek.borrow().as_ref() {
        let (proj, tab) = p.map(|p| (p.name(), p.current().map(|t| t.label().to_string()).unwrap_or_default())).unwrap_or_default();
        e.set_placeholder_text(Some(&if proj.is_empty() { "Open a project in qh first".to_string() } else { format!("Ask qh in {proj}…") }));
        hint.set_text(&if proj.is_empty() { String::new() } else { format!("⏎ sends to {proj} · {tab}    Esc hides") });
    };
}

fn render_sidebar(st: &Shared, a: &App, u: &Ui) {
    clear(&u.tiles);
    let mine: Vec<String> = if a.team { vec![] } else { a.cfg.recent.clone() };
    if mine.is_empty() {
        let l = label(if a.team { "No team projects yet." } else { "Open a folder to start.  Ctrl+O" }, &["soft", "small"]);
        l.set_margin_start(8);
        u.tiles.append(&l);
    }
    for chunk in mine.chunks(3) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_homogeneous(true);
        for path in chunk {
            let name = model::name_of(path);
            let b = gtk::Button::new();
            let l = gtk::Label::new(Some(&name));
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            b.set_child(Some(&l));
            b.add_css_class("tile");
            if a.current.as_ref() == Some(path) { b.add_css_class("on") }
            b.set_tooltip_text(Some(path));
            b.set_cursor_from_name(Some("pointer"));
            let (s, p) = (st.clone(), path.clone());
            b.connect_clicked(move |_| { s.borrow_mut().open(&p); schedule(&s) });
            row.append(&b);
        }
        for _ in chunk.len()..3 { row.append(&gtk::Box::new(gtk::Orientation::Horizontal, 0)) }
        u.tiles.append(&row);
    }

    clear(&u.side);
    if let (Some(p), false) = (a.project(), a.team) {
        let nc = row_button(&[("list-add-symbolic", ""), ("", "New chat"), ("", "~Ctrl+T")], false);
        nc.set_action_name(Some("app.new-tab"));
        nc.set_margin_top(12);
        u.side.append(&nc);
        u.side.append(&label(&p.name().to_uppercase(), &["header"]));
        for t in &p.tabs {
            let b = row_button(&[("", t.label()), ("", if t.busy() { "~working" } else { "" })], t.name == p.active);
            let (s, n) = (st.clone(), t.name.clone());
            b.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.active = n.clone() } schedule(&s) });
            u.side.append(&b);
        }
        if !p.sessions.is_empty() {
            u.side.append(&label("SESSIONS", &["header"]));
            for x in p.sessions.iter().take(30) {
                let title = if x.title.is_empty() { "untitled" } else { &x.title };
                let open_in = p.open.get(&x.id).cloned();
                let b = row_button(&[("", title), ("", &format!("~{}", model::ago(x.when)))], false);
                let (s, id) = (st.clone(), x.id.clone());
                b.connect_clicked(move |_| {
                    if let Some(p) = s.borrow_mut().project_mut() {
                        match &open_in { Some(t) => p.active = t.clone(), None => p.send(json!({"type": "resume", "id": id})) }
                    }
                    schedule(&s)
                });
                u.side.append(&b);
            }
        }
    }

    clear(&u.dots);
    for (label_, team) in [("Personal", false), ("Team", true)] {
        let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        d.add_css_class("dot");
        if a.team == team { d.add_css_class("on") }
        d.set_halign(gtk::Align::Center); d.set_valign(gtk::Align::Center);
        let b = gtk::Button::new();
        b.set_child(Some(&d));
        b.add_css_class("plain");
        b.set_size_request(26, 26);
        b.set_tooltip_text(Some(&format!("{label_} projects")));
        let s = st.clone();
        b.connect_clicked(move |_| { s.borrow_mut().team = team; schedule(&s) });
        u.dots.append(&b);
    }
}

/// A sidebar row: an icon and/or labels; a "~" label is the faint trailing one.
fn row_button(parts: &[(&str, &str)], on: bool) -> gtk::Button {
    let b = gtk::Button::new();
    b.add_css_class("row");
    if on { b.add_css_class("on") }
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    for (icon, text) in parts {
        if !icon.is_empty() {
            let i = gtk::Image::from_icon_name(icon);
            i.add_css_class("soft");
            h.append(&i);
        } else if let Some(t) = text.strip_prefix('~') {
            let l = label(t, &["faint", "small"]);
            h.append(&l);
        } else if !text.is_empty() {
            let l = label(text, if on { &["ink"] } else { &["ink"] });
            l.set_hexpand(true);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            h.append(&l);
        }
    }
    b.set_child(Some(&h));
    b.set_cursor_from_name(Some("pointer"));
    b
}

fn render_tabs(st: &Shared, a: &App, u: &Ui) {
    clear(&u.tabs);
    clear(&u.layout);
    let Some(p) = a.project() else { return };
    for t in &p.tabs {
        let b = gtk::Button::with_label(t.label());
        b.add_css_class("tab");
        if t.name == p.active { b.add_css_class("on") }
        b.set_tooltip_text(Some("Middle-click to close"));
        let (s, n) = (st.clone(), t.name.clone());
        b.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.active = n.clone() } schedule(&s) });
        let mid = gtk::GestureClick::builder().button(2).build();
        let (s, n) = (st.clone(), t.name.clone());
        mid.connect_released(move |_, _, _, _| if let Some(p) = s.borrow().project() { p.send(json!({"type": "close_tab", "tab": n})) });
        b.add_controller(mid);
        u.tabs.append(&b);
    }
    for (label_, key) in [("Chat", "chat"), ("Split", "split"), ("Code", "code")] {
        let b = gtk::Button::with_label(label_);
        b.add_css_class("seg");
        if p.layout == key { b.add_css_class("on") }
        if key != "chat" { b.set_tooltip_text(Some("Code view comes in the next build")) }
        let s = st.clone();
        b.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.layout = key.into() } schedule(&s) });
        u.layout.append(&b);
    }
}

fn render_empty(st: &Shared, a: &App, u: &Ui) {
    clear(&u.empty);
    let Some(p) = a.project() else { return };
    u.empty.append(&label(&format!("What should we work on in {}?", p.name()), &["greeting"]));
    let grid = gtk::Grid::builder().column_spacing(8).row_spacing(8).column_homogeneous(true).build();
    for (i, s) in ["Explain this project", "Find bugs in recent changes", "Write tests for ", "What changed since yesterday?"].iter().enumerate() {
        let shown = if s.ends_with(' ') { format!("{s}…") } else { s.to_string() };
        let b = gtk::Button::new();
        let l = label(&shown, &[]);
        b.set_child(Some(&l));
        b.add_css_class("starter");
        b.set_hexpand(true);
        b.set_cursor_from_name(Some("pointer"));
        let (text, fill) = (u.text.clone(), s.to_string());
        b.connect_clicked(move |_| { text.buffer().set_text(&fill); text.grab_focus(); let buf = text.buffer(); buf.place_cursor(&buf.end_iter()) });
        grid.attach(&b, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }
    u.empty.append(&grid);
    let _ = st;
    u.empty.append(&label("Ctrl+T new chat  ·  / commands  ·  @ files  ·  Esc stops", &["mono", "faint", "small"]));
}

fn render_thread(a: &App, u: &Ui) {
    let Some(t) = a.project().and_then(|p| p.current()) else { return };
    // rebuild only when the thread changed (tab, item count, last item's length)
    let last = t.items.last().map(|i| match i { Item::User(s) | Item::Reply(s) | Item::Notice(s) | Item::Error(s) | Item::Tool(s, _) => s.len() }).unwrap_or(0);
    let sig = format!("{}|{}|{}|{}", a.current.clone().unwrap_or_default(), t.name, t.items.len(), last);
    if THREAD_SIG.with(|s| s.replace(sig.clone())) == sig { return }
    clear(&u.thread);
    for it in &t.items {
        let w: gtk::Widget = match it {
            Item::User(s) => {
                let l = label(s, &["user"]);
                l.set_wrap(true); l.set_wrap_mode(gtk::pango::WrapMode::WordChar); l.set_selectable(true);
                l.set_halign(gtk::Align::End); l.set_max_width_chars(70);
                l.upcast()
            }
            Item::Reply(s) => {
                let l = label(s.trim(), &["reply"]);
                l.set_wrap(true); l.set_wrap_mode(gtk::pango::WrapMode::WordChar); l.set_selectable(true);
                l.upcast()
            }
            Item::Tool(n, args) => { let l = label(&format!("› {n}  {args}"), &["toolline"]); l.set_ellipsize(gtk::pango::EllipsizeMode::End); l.upcast() }
            Item::Notice(s) => { let l = label(s, &["toolline", "faint"]); l.set_wrap(true); l.upcast() }
            Item::Error(s) => { let l = label(s, &["toolline", "err"]); l.set_wrap(true); l.upcast() }
        };
        u.thread.append(&w);
    }
    let adj = u.scroll.vadjustment();
    glib::idle_add_local_once(move || adj.set_value(adj.upper()));
}

#[allow(clippy::type_complexity)]
fn composer(st: &Shared) -> (gtk::Box, gtk::TextView, gtk::Label, gtk::Label, gtk::Label, gtk::Label, gtk::Button, gtk::Button) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_start(24); outer.set_margin_end(24); outer.set_margin_bottom(18); outer.set_margin_top(4);
    let bx = gtk::Box::new(gtk::Orientation::Vertical, 8);
    bx.add_css_class("composer");
    let text = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).accepts_tab(false).build();
    let tscroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true)
        .max_content_height(240).min_content_height(24).child(&text).build();
    let placeholder = label("Ask anything · / commands · @ files", &["placeholder"]);
    placeholder.set_can_target(false); placeholder.set_valign(gtk::Align::Start);
    let ov = gtk::Overlay::new();
    ov.set_child(Some(&tscroll));
    ov.add_overlay(&placeholder);
    bx.append(&ov);
    let ph = placeholder.clone();
    text.buffer().connect_changed(move |b| ph.set_visible(b.char_count() == 0));

    let meta = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let meta_model = label("", &["meta"]);
    meta_model.set_tooltip_text(Some("Model"));
    let meta_mode = label("", &["meta"]);
    meta_mode.set_tooltip_text(Some("Mode"));
    let meta_tokens = label("", &["meta"]);
    meta.append(&meta_model); meta.append(&meta_mode); meta.append(&meta_tokens);
    meta.append(&gtk::Box::builder().hexpand(true).build());
    let stop = gtk::Button::with_label("stop");
    stop.add_css_class("plain"); stop.add_css_class("accent"); stop.set_visible(false);
    let s = st.clone();
    stop.connect_clicked(move |_| if let Some(p) = s.borrow().project() { p.stop() });
    meta.append(&stop);
    let send = gtk::Button::from_icon_name("go-up-symbolic");
    send.add_css_class("send");
    send.set_tooltip_text(Some("Send (Enter)"));
    meta.append(&send);
    bx.append(&meta);
    let clamp = adw::Clamp::builder().maximum_size(700).child(&bx).build();
    outer.append(&clamp);

    let do_send = {
        let (s, text) = (st.clone(), text.clone());
        Rc::new(move || {
            let b = text.buffer();
            let t = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
            if t.trim().is_empty() { return }
            b.set_text("");
            if let Some(p) = s.borrow_mut().project_mut() { p.prompt(&t) }
            schedule(&s);
        })
    };
    let ds = do_send.clone();
    send.connect_clicked(move |_| ds());
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let s = st.clone();
    keys.connect_key_pressed(move |_, k, _, m| {
        if (k == gdk::Key::Return || k == gdk::Key::KP_Enter) && !m.contains(gdk::ModifierType::SHIFT_MASK) {
            do_send();
            return glib::Propagation::Stop;
        }
        if k == gdk::Key::Escape {
            if let Some(p) = s.borrow().project() { if p.current().is_some_and(|t| t.busy()) { p.stop(); return glib::Propagation::Stop } }
        }
        glib::Propagation::Proceed
    });
    text.add_controller(keys);
    (outer, text, placeholder, meta_model, meta_mode, meta_tokens, stop, send)
}

fn setup_page(st: &Shared) -> (gtk::ScrolledWindow, gtk::Box, gtk::Box, gtk::SearchEntry) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_start(40); outer.set_margin_end(40); outer.set_margin_top(34); outer.set_margin_bottom(34);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let title = label("Connect your models", &["title"]);
    title.set_hexpand(true);
    head.append(&title);
    outer.append(&head);
    outer.append(&label("Tick any mix: everything you connect stays available to switch to.", &["soft"]));
    let setup = gtk::Box::new(gtk::Orientation::Vertical, 6);
    setup.set_margin_top(14);
    outer.append(&setup);
    let filter = gtk::SearchEntry::new();
    filter.add_css_class("field"); filter.add_css_class("mono");
    filter.set_margin_top(6);
    let models = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.append(&filter);
    outer.append(&models);
    let s = st.clone();
    filter.connect_search_changed(move |_| { SETUP_DIRTY.with(|d| d.set(true)); schedule(&s) });
    let clamp = adw::Clamp::builder().maximum_size(640).child(&outer).build();
    clamp.set_halign(gtk::Align::Fill);
    let sc = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();
    (sc, setup, models, filter)
}

fn render_setup(st: &Shared, a: &App, u: &Ui) {
    let Some(p) = a.project() else { return };
    clear(&u.setup);
    let sv = p.setup.clone().unwrap_or_default();
    let needed = sv["needed"].as_bool() != Some(false);
    if !needed {
        let done = gtk::Button::with_label("done");
        done.add_css_class("plain"); done.add_css_class("mono"); done.set_halign(gtk::Align::End);
        done.set_action_name(Some("app.setup"));
        u.setup.append(&done);
    }
    if let Some((m, err)) = &p.setup_msg {
        let l = label(m, &["mono", "small", if *err { "err" } else { "accent" }]);
        l.set_selectable(true); l.set_wrap(true);
        u.setup.append(&l);
    }
    for src in sv["sources"].as_array().into_iter().flatten() {
        let g = |k: &str| src[k].as_str().unwrap_or("").to_string();
        let (id, kind, connected) = (g("id"), g("kind"), src["connected"].as_bool().unwrap_or(false));
        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.add_css_class("source");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.append(&label(if connected { "●" } else { "○" }, &[if connected { "accent" } else { "faint" }]));
        let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
        col.set_hexpand(true);
        col.append(&label(&g("label"), &["medium", "ink"]));
        let note = if !g("note").is_empty() { g("note") } else if connected { g("account") } else { String::new() };
        if !note.is_empty() { col.append(&label(&note, &["mono", "faint", "small"])) }
        row.append(&col);
        let s = st.clone();
        let btn = match kind.as_str() {
            "signin" => {
                let b = gtk::Button::with_label(if connected { "sign in again" } else { "sign in" });
                b.set_sensitive(!p.setup_busy);
                b.connect_clicked(move |_| {
                    if let Some(p) = s.borrow_mut().project_mut() {
                        p.setup_busy = true;
                        p.setup_msg = Some(("opening your browser to sign in with ChatGPT…".into(), false));
                        p.send(json!({"type": "setup_chatgpt"}));
                    }
                    SETUP_DIRTY.with(|d| d.set(true)); schedule(&s);
                });
                Some(b)
            }
            "key" => {
                let b = gtk::Button::with_label(if connected { "change key" } else { "add key" });
                let i = id.clone();
                b.connect_clicked(move |_| {
                    { let mut a = s.borrow_mut(); a.key_for = if a.key_for.as_deref() == Some(&i) { None } else { Some(i.clone()) } }
                    SETUP_DIRTY.with(|d| d.set(true)); schedule(&s);
                });
                Some(b)
            }
            _ if !src["running"].as_bool().unwrap_or(false) => {
                let b = gtk::Button::with_label("get Ollama");
                b.connect_clicked(|b| gtk::UriLauncher::new("https://ollama.com/download")
                    .launch(b.root().and_downcast_ref::<gtk::Window>(), None::<&gio::Cancellable>, |_| {}));
                Some(b)
            }
            _ => None,
        };
        if let Some(b) = btn { b.set_valign(gtk::Align::Center); row.append(&b) }
        card.append(&row);
        if a.key_for.as_deref() == Some(id.as_str()) {
            let kr = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let e = gtk::PasswordEntry::new();
            e.set_placeholder_text(Some(&format!("paste your {id} API key")));
            e.set_hexpand(true);
            let save = gtk::Button::with_label("save");
            let submit = {
                let (s, e, id) = (st.clone(), e.clone(), id.clone());
                Rc::new(move || {
                    let key = e.text().to_string();
                    if key.is_empty() { return }
                    let mut a = s.borrow_mut();
                    a.key_for = None;
                    if let Some(p) = a.project_mut() {
                        p.setup_busy = true;
                        p.setup_msg = Some((format!("testing the {id} key…"), false));
                        p.send(json!({"type": "setup_key", "provider": id, "key": key}));
                    }
                    drop(a);
                    SETUP_DIRTY.with(|d| d.set(true)); schedule(&s);
                })
            };
            let f = submit.clone();
            e.connect_activate(move |_| f());
            save.connect_clicked(move |_| submit());
            kr.append(&e); kr.append(&save);
            card.append(&kr);
            let site = g("site");
            if let Some(url) = site.split(' ').next().filter(|u| u.starts_with("http")) {
                let l = gtk::LinkButton::with_label(url, &format!("get a key at {url}"));
                l.set_halign(gtk::Align::Start);
                card.append(&l);
            }
            glib::idle_add_local_once(move || { e.grab_focus(); });
        }
        u.setup.append(&card);
    }

    clear(&u.setup_models);
    u.setup_filter.set_visible(!p.models.is_empty());
    if p.models.is_empty() { return }
    u.setup_filter.set_placeholder_text(Some(&format!("search {} models", p.models.len())));
    let q = u.setup_filter.text().to_lowercase();
    let def = sv["default"].as_str().unwrap_or("").to_string();
    u.setup_models.prepend(&label("default model", &["mono", "soft", "small"]));
    for (id, from) in p.models.iter().filter(|(id, _)| q.is_empty() || id.to_lowercase().contains(&q)).take(60) {
        let b = gtk::Button::new();
        b.add_css_class("model");
        if *id == def { b.add_css_class("on") }
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        h.append(&label(id, &["ink"]));
        let f = label(from, &["faint"]);
        f.set_hexpand(true);
        h.append(&f);
        if *id == def { h.append(&label("default", &["accent"])) }
        b.set_child(Some(&h));
        let (s, m) = (st.clone(), id.clone());
        b.connect_clicked(move |_| if let Some(p) = s.borrow().project() { p.send(json!({"type": "setup_default", "model": m})) });
        u.setup_models.append(&b);
    }
}

fn pick_folder(st: &Shared) {
    let Some(u) = ui() else { return };
    let d = gtk::FileDialog::builder().title("Open a project folder").modal(true).build();
    let s = st.clone();
    d.select_folder(Some(&u.window), None::<&gio::Cancellable>, move |r| {
        if let Some(path) = r.ok().and_then(|f| f.path()) {
            s.borrow_mut().open(&path.to_string_lossy());
            schedule(&s);
        }
    });
}

/// The peek: a small floating window with one line; Enter asks the current project's current tab.
fn show_peek(st: &Shared) {
    let Some(u) = ui() else { return };
    if u.peek.borrow().is_none() {
        let w = gtk::Window::builder().title("qh peek").decorated(false).resizable(false).default_width(560).default_height(110).build();
        if let Some(app) = u.window.application() { w.set_application(Some(&app)) }
        w.add_css_class("qh"); w.add_css_class("peek");
        w.set_hide_on_close(true);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
        card.add_css_class("peekcard");
        card.set_valign(gtk::Align::Center);
        let e = gtk::Entry::new();
        let hint = label("", &["mono", "faint", "small"]);
        card.append(&e); card.append(&hint);
        w.set_child(Some(&card));
        let (s, win) = (st.clone(), w.clone());
        e.connect_activate(move |e| {
            let t = e.text().to_string();
            if t.trim().is_empty() { return }
            if let Some(p) = s.borrow_mut().project_mut() { p.prompt(&t) }
            e.set_text("");
            win.set_visible(false);
            schedule(&s);
        });
        let keys = gtk::EventControllerKey::new();
        let win = w.clone();
        keys.connect_key_pressed(move |_, k, _, _| {
            if k == gdk::Key::Escape { win.set_visible(false); glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        w.add_controller(keys);
        *u.peek.borrow_mut() = Some((w, e, hint));
    }
    schedule(st);
    let peek = u.peek.borrow().clone();
    if let Some((w, e, _)) = peek { w.present(); e.grab_focus(); }
}
