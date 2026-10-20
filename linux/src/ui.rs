//! The window (apple/qh/ContentView.swift, windows/MainWindow.cs): sidebar on the ground (or a 56px
//! rail in compact mode), the work as a card: tabs, Chat | Split | Code, the ⋯ pages, asks and the
//! plan above the composer. State lives in `model::App`; the thread is pushed live (threads.rs);
//! everything else is redrawn from the model in one coalesced pass, each part only when its data
//! changed.

use crate::client::Msg;
use crate::composer::Composer;
use crate::model::{self, App};
use crate::{pages, theme, threads};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use qh_code::{Checkpoints, CodeView, FileTree, Layout, LayoutSwitch, Review};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub type Shared = Rc<RefCell<App>>;

pub struct Ui {
    pub window: adw::ApplicationWindow,
    sidebar: gtk::Box,
    tiles: gtk::Box,
    side: gtk::Box,
    side_bottom: gtk::Box,
    dots: gtk::Box,
    rail: gtk::Box,
    tabs: gtk::Box,
    tab_right: gtk::Box,
    switch: LayoutSwitch,
    pub stack: gtk::Stack,
    work: gtk::Paned,
    pub threads: gtk::Stack,
    pub tree: FileTree,
    pub code: CodeView,
    review: Review,
    checkpoints: Checkpoints,
    empty: gtk::Box,
    status: gtk::Label,
    pub pages: pages::Pages,
    asks: gtk::Box,
    thinking: gtk::Revealer,
    think_secs: gtk::Label,
    pub composer: Composer,
    pub palette: qh_desktop::Palette,
    overlay: gtk::Overlay,
    notifier: Option<qh_desktop::Notifier>,
    tray: RefCell<Option<qh_desktop::TrayHandle>>,
    tray_last: Cell<(u32, u32)>,
    notified: RefCell<HashSet<String>>,
    peek: RefCell<Option<(gtk::Window, gtk::Entry, gtk::Label)>>,
}

/// Open the slide-in title bar (test runs, where there's no pointer).
pub fn show_titlebar() {
    if let Some(r) = ui().and_then(|u| u.overlay.child()).and_then(|c| c.first_child()).and_downcast::<gtk::Revealer>() { r.set_reveal_child(true) }
}

/// The slide-in title bar's height.
const TITLEBAR: i32 = 34;

thread_local! {
    static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
    static SHARED: RefCell<Option<Shared>> = const { RefCell::new(None) };
    static PENDING: Cell<bool> = const { Cell::new(false) };
    pub static SETUP_DIRTY: Cell<bool> = const { Cell::new(true) };
    static SIGS: RefCell<HashMap<&'static str, String>> = RefCell::new(HashMap::new());
    static TEAM_TICKS: Cell<u32> = const { Cell::new(0) };
    /// when each "path|tab" last heard from serve: the thinking row shows after 2 s of quiet
    static HEARD: RefCell<HashMap<String, std::time::Instant>> = RefCell::new(HashMap::new());
    static ASK_SHOWN: Cell<u64> = const { Cell::new(0) };
}

pub fn ui() -> Option<Rc<Ui>> { UI.with(|u| u.borrow().clone()) }
pub fn shared() -> Option<Shared> { SHARED.with(|s| s.borrow().clone()) }

/// True (and remembered) when `sig` differs from the last one for `part`.
fn changed(part: &'static str, sig: String) -> bool {
    SIGS.with(|s| s.borrow_mut().insert(part, sig.clone()).as_ref() != Some(&sig))
}
pub fn dirty(part: &'static str) { SIGS.with(|s| { s.borrow_mut().remove(part); }) }

/// Once, at startup: styles, actions and shortcuts, the event pump, tray and notifications.
pub fn install(app: &adw::Application, st: &Shared, rx: async_channel::Receiver<Msg>) {
    SHARED.with(|s| *s.borrow_mut() = Some(st.clone()));
    theme::install();
    theme::set_appearance(&std::env::var("QH_APP_THEME").unwrap_or_else(|_| st.borrow().cfg.theme.clone()));

    let action = |name: &str, accels: &[&str], f: Box<dyn Fn()>| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        app.add_action(&a);
        if !accels.is_empty() { app.set_accels_for_action(&format!("app.{name}"), accels) }
    };
    let s = st.clone();
    action("new-tab", &["<Control>t"], Box::new(move || if let Some(p) = s.borrow().project() { p.send(json!({"type": "new_tab"})) }));
    let s = st.clone();
    action("open-folder", &["<Control>o"], Box::new(move || pick_folder(&s)));
    let s = st.clone();
    action("peek", &[], Box::new(move || show_peek(&s)));
    let s = st.clone();
    action("palette", &["<Control>k"], Box::new(move || show_palette(&s)));
    let s = st.clone();
    action("compact", &["<Control>backslash"], Box::new(move || toggle_compact(&s)));
    let s = st.clone();
    action("team", &["<Control><Shift>t"], Box::new(move || { let on = !s.borrow().team; set_team(&s, on) }));
    let s = st.clone();
    action("settings", &["<Control>comma"], Box::new(move || toggle_settings(&s)));
    for (name, layout, key) in [("layout-chat", "chat", "<Control>1"), ("layout-split", "split", "<Control>2"), ("layout-code", "code", "<Control>3")] {
        let s = st.clone();
        action(name, &[key], Box::new(move || set_layout(&s, layout)));
    }
    for (name, page, key) in [("review", "review", "<Control>d"), ("checkpoints", "checkpoints", ""), ("memory", "memory", ""),
                              ("activity", "activity", ""), ("tasks", "tasks", "")] {
        let s = st.clone();
        let keys: &[&str] = if key.is_empty() { &[] } else { &[key] };
        action(name, keys, Box::new(move || {
            let (path, on) = { let a = s.borrow(); (a.current.clone(), a.project().and_then(|p| p.page.clone()).as_deref() == Some(page)) };
            if let Some(path) = path { show_page(&s, &path, if on { None } else { Some(page) }) }
        }));
    }
    let s = st.clone();
    action("setup", &[], Box::new(move || {
        if let Some(p) = s.borrow_mut().project_mut() { p.show_setup = !p.show_setup; p.page = None; if p.show_setup { p.load_setup() } }
        s.borrow_mut().settings = false;
        SETUP_DIRTY.with(|d| d.set(true));
        schedule(&s);
    }));
    action("present", &[], Box::new(|| if let Some(u) = ui() { u.window.present() }));
    let a = app.clone();
    action("quit", &["<Control>q"], Box::new(move || a.quit()));
    let open = gio::SimpleAction::new("open", Some(glib::VariantTy::STRING));
    let s = st.clone();
    open.connect_activate(move |_, v| {
        if let Some(p) = v.and_then(|v| v.get::<String>()) { s.borrow_mut().open(&p); schedule(&s) }
        if let Some(u) = ui() { u.window.present() }
    });
    app.add_action(&open);

    let s = st.clone();
    glib::spawn_future_local(async move {
        while let Ok(m) = rx.recv().await {
            match m {
                Msg::Event(path, v) => {
                    let setup = s.borrow_mut().by_path(&path).is_some_and(|p| p.handle(&v));
                    if setup { SETUP_DIRTY.with(|d| d.set(true)) }
                    post(&s, &path, &v);
                }
                Msg::Closed(path, why) => {
                    if let Some(p) = s.borrow_mut().by_path(&path) { p.state = format!("closed: {why}") }
                    SETUP_DIRTY.with(|d| d.set(true));
                }
            }
            schedule(&s);
        }
    });

    // the team room while it's on screen: every 3 s, or every 30 s once serve pushes changes itself
    let s = st.clone();
    glib::timeout_add_seconds_local(3, move || {
        let n = TEAM_TICKS.with(|t| { t.set(t.get() + 1); t.get() });
        let active = ui().is_some_and(|u| u.window.is_active()) || std::env::var_os("QH_APP_SNAPSHOT").is_some();
        let a = s.borrow();
        if let (true, true, Some(p)) = (a.team, active, a.project()) {
            if !p.team_push || n % 10 == 0 { p.send(json!({"type": "team", "chan": p.chan})) }
        }
        glib::ControlFlow::Continue
    });
}

/// After the model took an event: the live thread, the code view's inline edit, notifications.
fn post(st: &Shared, path: &str, v: &serde_json::Value) {
    let Some(u) = ui() else { return };
    heard(path, v["tab"].as_str().unwrap_or("main"));
    threads::on_event(st, path, v, &u.threads);
    let ty = v["type"].as_str().unwrap_or("");
    let current = st.borrow().current.as_deref() == Some(path);
    match ty {
        "inline_result" if current => u.code.show_inline_result(v["before"].as_str().unwrap_or(""), v["after"].as_str().unwrap_or("")),
        "error" if current && v["re"] == "inline_edit" => u.code.show_inline_error(v["message"].as_str().unwrap_or("error")),
        "inline_applied" if current => { u.code.close_inline(); dirty("code") }
        "inline_delta" if current => dirty("inline"),
        "diff" | "rewound" | "changes" if current => dirty("code"),
        "rewound" | "checkpoints" if current => u.checkpoints.set_busy(false),
        "confirm" => {
            let Some(n) = &u.notifier else { return };
            if u.window.is_active() { return }
            let (name, ask) = {
                let a = st.borrow();
                let Some(p) = a.projects.iter().find(|p| p.path == path) else { return };
                (p.name(), p.asks.iter().find(|x| Some(x.id) == v["id"].as_u64()).cloned())
            };
            if let Some(ask) = ask {
                let key = format!("{path}\u{1f}{}", ask.id);
                n.notify_ask(&name, &ask.tool, &ask.detail(), &key);
                u.notified.borrow_mut().insert(key);
            }
        }
        "confirm_closed" => {
            let key = format!("{path}\u{1f}{}", v["id"].as_u64().unwrap_or(0));
            if u.notified.borrow_mut().remove(&key) { if let Some(n) = &u.notifier { n.clear(&key) } }
        }
        _ => {}
    }
}

pub fn activate(app: &adw::Application, st: &Shared, folder: Option<String>, peek: bool) {
    if ui().is_none() {
        build(app, st);
        let (reopen, first) = { let a = st.borrow(); (a.cfg.reopen, a.cfg.recent.iter().find(|p| std::path::Path::new(p).is_dir()).cloned()) };
        if folder.is_none() && reopen { if let Some(f) = first { st.borrow_mut().open(&f) } }
    }
    if let Some(f) = folder { st.borrow_mut().open(&f) }
    schedule(st);
    let u = ui().unwrap();
    let peek_snap = std::env::var("QH_APP_SNAPSHOT_PEEK").is_ok();
    if !peek || u.window.is_visible() { u.window.present() }
    if peek || peek_snap { show_peek(st) }
    crate::hooks::run(st, peek_snap);
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

pub fn clear(b: &gtk::Box) { while let Some(c) = b.first_child() { b.remove(&c) } }

fn build(app: &adw::Application, st: &Shared) {
    let (w, h) = { let a = st.borrow(); (a.cfg.width, a.cfg.height) };
    let window = adw::ApplicationWindow::builder().application(app).title("qh").default_width(w).default_height(h).build();
    window.add_css_class("qh");

    // sidebar: full, or the compact rail
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.add_css_class("sidebar");
    sidebar.set_hexpand(false);
    let tiles = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tiles.set_margin_start(12); tiles.set_margin_end(12); tiles.set_margin_top(14);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 2);
    side.set_margin_start(8); side.set_margin_end(8);
    let side_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&side).build();
    let side_bottom = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    side_bottom.set_margin_start(14); side_bottom.set_margin_end(14); side_bottom.set_margin_top(10); side_bottom.set_margin_bottom(10);
    let dots = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    side_bottom.append(&dots);
    side_bottom.append(&gtk::Box::builder().hexpand(true).build());
    let s = st.clone();
    side_bottom.append(&icon_button("folder-open-symbolic", "Open folder (Ctrl+O)", move || pick_folder(&s)));
    let s = st.clone();
    side_bottom.append(&icon_button("emblem-system-symbolic", "Settings (Ctrl+,)", move || toggle_settings(&s)));
    let rail = gtk::Box::new(gtk::Orientation::Vertical, 6);
    rail.set_margin_top(14); rail.set_margin_bottom(10);
    rail.set_vexpand(true);
    let top = gtk::WindowHandle::builder().child(&tiles).build();
    sidebar.append(&top);
    sidebar.append(&side_scroll);
    sidebar.append(&side_bottom);
    sidebar.append(&rail);

    // card: tab bar, pages, asks, composer
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("card");
    card.set_hexpand(true);
    card.set_overflow(gtk::Overflow::Hidden);
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    bar.add_css_class("tabbar");
    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    tabs.set_valign(gtk::Align::End);
    let tabs_scroll = gtk::ScrolledWindow::builder().vscrollbar_policy(gtk::PolicyType::Never).hscrollbar_policy(gtk::PolicyType::External)
        .child(&tabs).propagate_natural_width(true).build();
    tabs_scroll.set_valign(gtk::Align::End);
    bar.append(&tabs_scroll);
    let tab_right = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    tab_right.set_hexpand(true);
    let plus = gtk::Button::with_label("+");
    plus.add_css_class("plain"); plus.add_css_class("faint");
    plus.set_size_request(30, 34); plus.set_valign(gtk::Align::End);
    plus.set_tooltip_text(Some("New tab (Ctrl+T)"));
    plus.set_action_name(Some("app.new-tab"));
    tab_right.append(&plus);
    tab_right.append(&gtk::Box::builder().hexpand(true).build());
    let switch = LayoutSwitch::new();
    let s = st.clone();
    switch.on_change(move |l| set_layout(&s, match l { Layout::Chat => "chat", Layout::Split => "split", Layout::Code => "code" }));
    tab_right.append(switch.widget());
    let menu = gio::Menu::new();
    for (l, a) in [("Review changes", "app.review"), ("Checkpoints", "app.checkpoints"), ("Project memory", "app.memory"),
                   ("Activity", "app.activity"), ("Background tasks", "app.tasks"), ("Connect models", "app.setup")] {
        menu.append(Some(l), Some(a));
    }
    let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").menu_model(&menu).build();
    more.add_css_class("plain"); more.set_valign(gtk::Align::Center); more.set_margin_start(6);
    more.set_tooltip_text(Some("More"));
    tab_right.append(&more);
    bar.append(&tab_right);
    let handle = gtk::WindowHandle::builder().child(&bar).build();
    card.append(&handle);

    let threads = gtk::Stack::new();
    threads.set_hexpand(true);
    let tree = FileTree::new();
    let code = CodeView::new();
    let work = gtk::Paned::new(gtk::Orientation::Horizontal);
    work.set_shrink_start_child(false);
    work.set_shrink_end_child(false);
    work.set_wide_handle(false);
    work.set_start_child(Some(&threads));
    let review = Review::new();
    let checkpoints = Checkpoints::new();
    wire_code(st, &tree, &code, &review, &checkpoints);

    let empty = gtk::Box::new(gtk::Orientation::Vertical, 22);
    empty.set_valign(gtk::Align::Center);
    empty.set_margin_start(24); empty.set_margin_end(24); empty.set_margin_bottom(40);
    let empty_clamp = adw::Clamp::builder().maximum_size(560).child(&empty).vexpand(true).build();
    let status = label("", &["mono", "soft", "small"]);
    status.set_halign(gtk::Align::Center); status.set_valign(gtk::Align::Center); status.set_wrap(true);
    let pages = pages::Pages::new(st);
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&work, Some("work"));
    stack.add_named(&empty_clamp, Some("empty"));
    stack.add_named(&status, Some("status"));
    stack.add_named(&pages.setup_page, Some("setup"));
    stack.add_named(&page_frame("Review changes", "What qh changed this session: keep a file, or undo it. Ctrl+D", review.widget(), st), Some("review"));
    stack.add_named(&page_frame("", "", checkpoints.widget(), st), Some("checkpoints"));
    stack.add_named(&pages.memory, Some("memory"));
    stack.add_named(&pages.activity_page, Some("activity"));
    stack.add_named(&pages.tasks_page, Some("tasks"));
    stack.add_named(&pages.settings_page, Some("settings"));
    stack.add_named(&pages.team_page, Some("team"));
    card.append(&stack);

    // "Thinking" while a turn has been quiet for 2 s: a pulsing square, the word shimmering, the seconds
    let think = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    think.set_margin_start(24); think.set_margin_end(24); think.set_margin_bottom(6);
    let sq = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    sq.add_css_class("think-dot"); sq.set_valign(gtk::Align::Center);
    think.append(&sq);
    think.append(&label("Thinking", &["think"]));
    let think_secs = label("", &["mono", "faint", "small"]);
    think.append(&think_secs);
    let thinking = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::Crossfade).transition_duration(200)
        .child(&adw::Clamp::builder().maximum_size(760).child(&think).build()).build();
    card.append(&thinking);
    {
        let s = st.clone();
        glib::timeout_add_seconds_local(1, move || { tick_thinking(&s); glib::ControlFlow::Continue });
    }

    let asks = gtk::Box::new(gtk::Orientation::Vertical, 8);
    asks.set_margin_start(24); asks.set_margin_end(24);
    let asks_clamp = adw::Clamp::builder().maximum_size(760).child(&asks).build();
    card.append(&asks_clamp);
    let composer = Composer::new(st);
    card.append(&composer.root);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&sidebar);
    body.append(&card);

    // window controls: a title bar that slides in from the top edge and pushes the app down
    // (Hyprland draws no decorations of its own); it never covers the tab bar
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.set_margin_end(12); controls.set_hexpand(true); controls.set_halign(gtk::Align::End);
    let wc = |icon: &str, tip: &str, f: Box<dyn Fn(&adw::ApplicationWindow)>| {
        let b = gtk::Button::from_icon_name(icon);
        b.add_css_class("wctl"); b.set_tooltip_text(Some(tip)); b.set_valign(gtk::Align::Center);
        let win = window.clone();
        b.connect_clicked(move |_| f(&win));
        controls.append(&b);
    };
    wc("window-minimize-symbolic", "Minimize", Box::new(|w| w.minimize()));
    wc("window-maximize-symbolic", "Maximize", Box::new(|w| if w.is_maximized() { w.unmaximize() } else { w.maximize() }));
    wc("window-close-symbolic", "Close", Box::new(|w| w.close()));
    let titlebar = gtk::WindowHandle::builder().child(&controls).height_request(TITLEBAR).build();
    let reveal = gtk::Revealer::builder().transition_type(gtk::RevealerTransitionType::SlideDown).transition_duration(160)
        .child(&titlebar).build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&reveal);
    body.set_vexpand(true);
    root.append(&body);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&root));
    // in at the top edge, out once the pointer is below the bar (a gap so it doesn't flicker)
    let motion = gtk::EventControllerMotion::new();
    let rv = reveal.clone();
    motion.connect_motion(move |_, _, y| {
        let open = rv.reveals_child();
        if !open && y < 6.0 { rv.set_reveal_child(true) }
        else if open && y > TITLEBAR as f64 + 24.0 { rv.set_reveal_child(false) }
    });
    let rv = reveal.clone();
    motion.connect_leave(move |_| rv.set_reveal_child(false));
    overlay.add_controller(motion);
    window.set_content(Some(&overlay));

    // Esc closes a page (settings, review, …) when nothing nearer took it
    let keys = gtk::EventControllerKey::new();
    let s = st.clone();
    keys.connect_key_pressed(move |_, k, _, _| {
        if k != gdk::Key::Escape { return glib::Propagation::Proceed }
        let closed = {
            let mut a = s.borrow_mut();
            if a.settings { a.settings = false; true }
            else if let Some(p) = a.project_mut().filter(|p| p.page.is_some() || p.show_setup && p.state == "live" && p.setup.as_ref().is_some_and(|x| x["needed"] == false)) {
                p.page = None; p.show_setup = false; true
            } else { false }
        };
        if closed { schedule(&s); glib::Propagation::Stop } else { glib::Propagation::Proceed }
    });
    window.add_controller(keys);

    let s = st.clone();
    window.connect_close_request(move |w| {
        let (wd, ht) = w.default_size();
        let mut a = s.borrow_mut();
        if !w.is_maximized() && wd > 0 && ht > 0 { a.cfg.width = wd; a.cfg.height = ht; a.cfg.save() }
        glib::Propagation::Proceed
    });
    // compact follows the width unless Ctrl+\ set it
    let s = st.clone();
    window.connect_default_width_notify(move |_| schedule(&s));
    let s = st.clone();
    window.connect_maximized_notify(move |_| schedule(&s));
    let s = st.clone();
    window.connect_is_active_notify(move |w| {
        if w.is_active() { if let Some(p) = s.borrow().project() { p.send(json!({"type": "sessions"})) } }
    });

    let palette = qh_desktop::Palette::new();
    let notifier = if std::env::var_os("QH_APP_NO_NOTIFY").is_some() { None } else {
        let s = st.clone();
        qh_desktop::Notifier::new(move |key, ans| on_notification(&s, key, ans)).map_err(|e| eprintln!("qh-app: notifications: {e}")).ok()
    };
    let tray = if std::env::var_os("QH_APP_NO_TRAY").is_some() { None } else {
        let open = || glib::MainContext::default().invoke(|| { if let Some(a) = gio::Application::default() { a.activate_action("present", None) } });
        let quit = || glib::MainContext::default().invoke(|| { if let Some(a) = gio::Application::default() { a.quit() } });
        qh_desktop::Tray::start(open, quit).map_err(|e| eprintln!("qh-app: tray: {e}")).ok()
    };

    let u = Ui { window, sidebar, tiles, side, side_bottom, dots, rail, tabs, tab_right, switch, stack, work, threads, tree, code, review,
        checkpoints, empty, status, pages, asks, thinking, think_secs, composer, palette, overlay, notifier, tray: RefCell::new(tray), tray_last: Cell::new((u32::MAX, 0)),
        notified: RefCell::new(HashSet::new()), peek: RefCell::new(None) };
    UI.with(|x| *x.borrow_mut() = Some(Rc::new(u)));
}

/// A page with a title, a line about it and Esc to close, around one of the code crate's widgets.
fn page_frame(title: &str, about: &str, child: &gtk::Widget, st: &Shared) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
    b.add_css_class("pagebox");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.set_margin_start(24); head.set_margin_end(20); head.set_margin_top(16); head.set_margin_bottom(4);
    let col = gtk::Box::new(gtk::Orientation::Vertical, 3);
    col.set_hexpand(true);
    if !title.is_empty() { col.append(&label(title, &["ptitle"])) }
    if !about.is_empty() { let a = label(about, &["soft", "small"]); a.set_wrap(true); col.append(&a) }
    head.append(&col);
    let x = gtk::Button::with_label("Esc");
    x.add_css_class("plain"); x.add_css_class("faint"); x.add_css_class("small");
    x.set_valign(gtk::Align::Start);
    let s = st.clone();
    x.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.page = None } schedule(&s) });
    head.append(&x);
    b.append(&head);
    child.set_vexpand(true);
    b.append(child);
    b
}

fn wire_code(st: &Shared, tree: &FileTree, code: &CodeView, review: &Review, cps: &Checkpoints) {
    let s = st.clone();
    tree.on_open(move |f| { let path = s.borrow().current.clone(); if let Some(path) = path { open_file(&s, &path, &f) } });
    code.on_open_editor(|path, line| open_in_editor(&path, line));
    let s = st.clone();
    code.on_inline(move |start, end, instruction| {
        let mut a = s.borrow_mut();
        let Some(p) = a.project_mut() else { return };
        let Some((path, _)) = p.code.clone() else { return };
        p.inline = Some(model::Inline { id: None, preview: String::new() });
        p.cmd("inline_edit", json!({"path": path, "start": start, "end": end, "instruction": instruction}));
    });
    let s = st.clone();
    code.on_inline_apply(move || {
        let a = s.borrow();
        if let Some((p, Some(id))) = a.project().and_then(|p| p.inline.as_ref().map(|i| (p, i.id))) { p.cmd("inline_apply", json!({"id": id})) }
    });
    let s = st.clone();
    code.on_inline_discard(move || {
        let mut a = s.borrow_mut();
        if let Some(p) = a.project_mut() {
            if let Some(id) = p.inline.take().and_then(|i| i.id) { p.cmd("inline_discard", json!({"id": id})) }
        }
        drop(a);
        if let Some(u) = ui() { u.code.close_inline() }
    });
    let s = st.clone();
    review.on_accept(move |f| if let Some(p) = s.borrow().project() { p.cmd("accept", json!({"path": full(p, &f)})) });
    let s = st.clone();
    review.on_reject(move |f| if let Some(p) = s.borrow().project() { p.cmd("reject", json!({"path": full(p, &f)})) });
    let s = st.clone();
    cps.on_rewind(move |turn, chat| if let Some(p) = s.borrow().project() { p.cmd("rewind", json!({"turn": turn, "chat": chat})) });
}

/// The path serve knows a change by, from the relative one the review shows.
fn full(p: &model::Project, rel: &str) -> String {
    let root = format!("{}/", p.path);
    p.changes.iter().map(|c| c.0.clone()).find(|f| f == rel || f.strip_prefix(&root) == Some(rel)).unwrap_or_else(|| rel.to_string())
}

/// $VISUAL (VS Code-likes get --goto file:line), else whatever xdg-open picks.
fn open_in_editor(rel: &str, line: usize) {
    let Some(root) = shared().and_then(|s| s.borrow().current.clone()) else { return };
    let abs = std::path::Path::new(&root).join(rel).to_string_lossy().to_string();
    let visual = std::env::var("VISUAL").ok().filter(|v| !v.trim().is_empty());
    let mut cmd = match visual {
        Some(v) => {
            let mut parts = v.split_whitespace();
            let bin = parts.next().unwrap_or("").to_string();
            let mut c = std::process::Command::new(&bin);
            c.args(parts);
            let base = std::path::Path::new(&bin).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if ["code", "codium", "code-oss", "cursor", "zed"].contains(&base.as_str()) {
                if base == "zed" { c.arg(format!("{abs}:{line}")); } else { c.args(["--goto", &format!("{abs}:{line}")]); }
            } else { c.arg(&abs); }
            c
        }
        None => { let mut c = std::process::Command::new("xdg-open"); c.arg(&abs); c }
    };
    if let Err(e) = cmd.current_dir(&root).spawn() { eprintln!("qh-app: open in editor: {e}") }
}

/// Open `f` (a project-relative path, optionally :line) in the code view; Chat becomes Split.
pub fn open_file(st: &Shared, path: &str, f: &str) {
    {
        let mut a = st.borrow_mut();
        a.team = false;
        a.settings = false;
        if a.current.as_deref() != Some(path) { a.open(path) }
        let Some(p) = a.project_mut() else { return };
        let r = p.file_ref(f).unwrap_or((f.trim_start_matches("./").to_string(), None));
        p.code = Some(r);
        p.page = None;
        if p.layout == "chat" { p.layout = "split".into() }
    }
    dirty("code");
    schedule(st);
}

pub fn show_page(st: &Shared, path: &str, page: Option<&str>) {
    {
        let mut a = st.borrow_mut();
        a.settings = false;
        a.team = false;
        if a.current.as_deref() != Some(path) { a.open(path) }
        let Some(p) = a.project_mut() else { return };
        p.page = page.map(String::from);
        p.show_setup = false;
        if let Some(pg) = page { p.load_page(pg) }
    }
    schedule(st);
}

fn set_layout(st: &Shared, l: &str) {
    if let Some(p) = st.borrow_mut().project_mut() { p.layout = l.into(); p.page = None; p.show_setup = false; if l != "chat" && p.files.is_empty() { p.cmd("files", json!({})) } }
    { let mut a = st.borrow_mut(); a.settings = false; a.team = false; }
    schedule(st);
}

fn toggle_settings(st: &Shared) {
    { let mut a = st.borrow_mut(); a.settings = !a.settings; }
    dirty("settings");
    schedule(st);
}

fn set_team(st: &Shared, on: bool) {
    {
        let mut a = st.borrow_mut();
        a.team = on;
        a.settings = false;
        if let (true, Some(p)) = (on, a.project()) { p.send(json!({"type": "team", "chan": p.chan})) }
    }
    schedule(st);
}

fn toggle_compact(st: &Shared) {
    let now = SIGS.with(|s| s.borrow().get("compact").cloned()) == Some("1".into());
    { let mut a = st.borrow_mut(); a.cfg.compact = Some(!now); if std::env::var_os("QH_APP_COMPACT").is_none() { a.cfg.save() } }
    schedule(st);
}

/// Answer the first open ask on the current tab (Ctrl+Enter / Ctrl+Backspace). False when none.
pub fn answer_current(st: &Shared, ans: &str) -> bool {
    let done = {
        let mut a = st.borrow_mut();
        let Some(p) = a.project_mut() else { return false };
        let tab = p.current_name();
        match p.asks.iter().find(|x| x.tab == tab).map(|x| x.id) { Some(id) => { p.answer(id, ans); true } None => false }
    };
    if done { schedule(st) }
    done
}

fn on_notification(st: &Shared, key: &str, ans: &str) {
    let Some((path, id)) = key.split_once('\u{1f}') else { return };
    let id: u64 = id.parse().unwrap_or(u64::MAX);
    if ans == "yes" || ans == "no" {
        if let Some(p) = st.borrow_mut().by_path(path) { p.answer(id, ans) }
    } else {
        // the body was clicked: bring the window up on that tab
        let mut a = st.borrow_mut();
        a.team = false;
        a.settings = false;
        if a.current.as_deref() != Some(path) { a.open(path) }
        if let Some(p) = a.project_mut() {
            if let Some(t) = p.asks.iter().find(|x| x.id == id).map(|x| x.tab.clone()) { p.active = t }
            p.page = None;
        }
        drop(a);
        if let Some(u) = ui() { u.window.present() }
    }
    schedule(st);
}

fn fade_in(w: &impl IsA<gtk::Widget>, ms: u32) {
    let w = w.as_ref().clone();
    w.set_opacity(0.0);
    let ww = w.clone();
    let a = adw::TimedAnimation::new(&w, 0.0, 1.0, ms, adw::CallbackAnimationTarget::new(move |v| ww.set_opacity(v)));
    a.set_easing(adw::Easing::EaseOutCubic);
    a.play();
}

fn heard(path: &str, tab: &str) {
    HEARD.with(|h| h.borrow_mut().insert(format!("{path}|{tab}"), std::time::Instant::now()));
}

fn tick_thinking(st: &Shared) {
    let Some(u) = ui() else { return };
    let quiet = st.try_borrow().ok().and_then(|a| {
        if a.team || a.settings { return None }
        let p = a.project()?;
        let t = p.current().filter(|t| t.status == "working" && p.page.is_none())?;
        let key = format!("{}|{}", p.path, t.name);
        HEARD.with(|h| h.borrow().get(&key).map(|i| i.elapsed().as_secs()))
    }).filter(|q| *q >= 2);
    if let Some(q) = quiet { u.think_secs.set_text(&format!("{q}s")) }
    if u.thinking.reveals_child() != quiet.is_some() { u.thinking.set_reveal_child(quiet.is_some()) }
}

/// Send what the composer (or peek) holds: to the team channel in the team space, else a prompt.
pub fn prompt(st: &Shared, text: &str, images: Vec<String>) {
    let Some(u) = ui() else { return };
    let shown = {
        let mut a = st.borrow_mut();
        if a.team {
            if let Some(p) = a.project() { p.send(json!({"type": "team_send", "chan": p.chan, "text": text.trim()})) }
            None
        } else {
            a.project_mut().and_then(|p| { let tab = p.current_name(); p.prompt(text, &images).map(|s| (p.path.clone(), tab, s)) })
        }
    };
    if let Some((path, tab, (shown, user))) = shown {
        heard(&path, &tab);
        threads::push_user(st, &path, &tab, &shown, user, &u.threads);
    }
    schedule(st);
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
    let draft = st.borrow_mut().project_mut().and_then(|p| p.draft.take());
    if let Some(d) = draft { u.composer.set_text(&d) }
    let a = st.borrow();
    let compact = a.cfg.compact.unwrap_or_else(|| { let w = u.window.width(); w > 0 && w < 900 });
    if changed("compact", if compact { "1".into() } else { "0".into() }) {
        u.sidebar.set_size_request(if compact { 56 } else { 260 }, -1);
        u.sidebar.set_width_request(if compact { 56 } else { 260 });
        if compact { u.sidebar.add_css_class("compact") } else { u.sidebar.remove_css_class("compact") }
        dirty("sidebar");
        // the rail and the full sidebar cross-fade (not on the first frame)
        if u.window.is_mapped() { fade_in(&u.sidebar, 200) }
    }
    for w in [u.tiles.parent().unwrap(), u.side.parent().and_then(|p| p.parent()).unwrap(), u.side_bottom.clone().upcast()] { w.set_visible(!compact) }
    u.rail.set_visible(compact);
    if compact { render_rail(st, &a, &u) } else if a.team { render_team_sidebar(st, &a, &u) } else { render_sidebar(st, &a, &u) }
    render_tabs(st, &a, &u);
    let p = a.project();
    let page: &str = if a.settings { "settings" } else if a.team { "team" } else {
        match p {
            None => { u.status.set_text("Open a folder to start.  Ctrl+O"); "status" }
            Some(p) if p.show_setup => "setup",
            Some(p) if p.state != "live" => {
                u.status.set_text(&if p.state == "starting" { format!("starting qh in {}…", p.name()) } else { p.state.clone() });
                "status"
            }
            Some(p) => match p.page.as_deref() {
                Some(pg) => pg,
                None if p.layout == "code" => "work",
                None if p.current().is_none_or(|t| t.empty()) && !(p.layout == "split" && p.code.is_some()) => "empty",
                None => "work",
            },
        }
    };
    let chatty = matches!(page, "work" | "empty" | "team") && !(page == "work" && p.is_some_and(|p| p.layout == "code"));
    u.composer.root.set_visible(chatty);
    u.composer.render(&a);
    u.composer.send_state(st);
    u.asks.set_visible(chatty && !a.team);
    if page == "setup" && SETUP_DIRTY.with(|d| d.replace(false)) { u.pages.render_setup(st, &a) }
    if page == "empty" { render_empty(&a, &u) }
    if let Some(p) = p {
        if page == "work" || page == "empty" { render_work(st, p, &u) }
        render_code_parts(p, &u);
        render_asks(st, p, &u);
        match page {
            "memory" => u.pages.render_memory(p),
            "activity" => u.pages.render_activity(st, p),
            "tasks" => u.pages.render_tasks(p),
            _ => {}
        }
    }
    if page == "team" { u.pages.render_team(st, &a) }
    if page == "settings" { u.pages.render_settings(st, &a) }
    if u.stack.visible_child_name().as_deref() != Some(page) { u.stack.set_visible_child_name(page) }
    render_tray(&a, &u);
    if let Some((_, e, hint)) = u.peek.borrow().as_ref() {
        let (proj, tab) = p.map(|p| (p.name(), p.current().map(|t| t.label().to_string()).unwrap_or_default())).unwrap_or_default();
        e.set_placeholder_text(Some(&if proj.is_empty() { "Open a project in qh first".to_string() } else { format!("Ask qh in {proj}…") }));
        hint.set_text(&if proj.is_empty() { String::new() } else { format!("⏎ sends to {proj} · {tab}    Esc hides") });
    };
}

fn render_tray(a: &App, u: &Ui) {
    let running = a.projects.iter().map(|p| p.tabs.iter().filter(|t| t.busy()).count() as u32).sum();
    let waiting = a.projects.iter().map(|p| p.asks.len() as u32).sum();
    if u.tray_last.replace((running, waiting)) != (running, waiting) {
        if let Some(t) = u.tray.borrow().as_ref() { t.update(running, waiting) }
    }
}

/// The thread (or file tree) beside the code view, by layout.
fn render_work(st: &Shared, p: &model::Project, u: &Ui) {
    let tab = p.current_name();
    let k = threads::ensure(st, &p.path, &tab, &u.threads);
    if u.threads.visible_child_name().as_deref() != Some(k.as_str()) { u.threads.set_visible_child_name(&k) }
    let code_layout = p.layout == "code";
    let start: gtk::Widget = if code_layout { u.tree.widget().clone() } else { u.threads.clone().upcast() };
    if u.work.start_child().as_ref() != Some(&start) { u.work.set_start_child(Some(&start)) }
    let show_code = code_layout || (p.layout == "split" && p.code.is_some());
    let end = u.work.end_child();
    if show_code && end.is_none() { u.work.set_end_child(Some(u.code.widget())) }
    if !show_code && end.is_some() { u.work.set_end_child(None::<&gtk::Widget>) }
    if changed("paned", format!("{}|{show_code}", p.layout)) && show_code {
        let w = u.work.width().max(u.window.width() - 300);
        u.work.set_position(if code_layout { 260 } else { (w / 2).max(320) });
    }
}

/// The code crate's widgets follow the current project: files, the open file, changes, checkpoints.
fn render_code_parts(p: &model::Project, u: &Ui) {
    if changed("files", format!("{}|{}", p.path, p.files.len())) { u.tree.set_files(p.files.clone()) }
    if changed("code", format!("{}|{:?}", p.path, p.code)) {
        if let Some((f, line)) = &p.code { u.code.set_file(f, &p.read_file(f), *line) }
    }
    if let Some(il) = &p.inline {
        if changed("inline", format!("{}|{}", il.preview.len(), il.id.unwrap_or(0))) && !il.preview.is_empty() { u.code.show_inline_preview(&il.preview) }
    }
    let sum = |v: &[(String, String, String)]| v.iter().map(|c| format!("{}:{}:{};", c.0, c.1.len(), c.2.len())).collect::<String>();
    if changed("changes", format!("{}|{}", p.path, sum(&p.changes))) {
        // serve sends absolute paths; show them relative to the project (callbacks map back)
        let root = format!("{}/", p.path);
        u.review.set_changes(p.changes.iter().map(|(f, b, a)| (f.strip_prefix(&root).unwrap_or(f).to_string(), b.clone(), a.clone())).collect())
    }
    if changed("checkpoints", format!("{}|{:?}", p.path, p.checkpoints)) {
        let root = format!("{}/", p.path);
        u.checkpoints.set_checkpoints(p.checkpoints.iter().map(|(t, pr, fs)| (*t, pr.clone(),
            fs.iter().map(|f| f.strip_prefix(&root).unwrap_or(f).to_string()).collect())).collect())
    }
}

/// The open ask (one at a time, so its shortcuts are unambiguous) and the plan, above the composer.
fn render_asks(st: &Shared, p: &model::Project, u: &Ui) {
    let tab = p.current_name();
    let mine: Vec<&model::Ask> = p.asks.iter().filter(|a| a.tab == tab).collect();
    let t = p.current();
    let plan = t.map(|t| (t.plan.clone(), t.plan_ready)).unwrap_or_default();
    let sig = format!("{}|{tab}|{:?}|{:?}", p.path, mine.iter().map(|a| a.id).collect::<Vec<_>>(), plan);
    if !changed("asks", sig) { return }
    clear(&u.asks);
    if let Some(ask) = mine.first() {
        let id = ask.id;
        let card = if ask.tool == "budget" {
            let s = st.clone();
            qh_thread::AskCard::budget(ask.args["spent"].as_f64().unwrap_or(0.0), ask.args["budget"].as_f64().unwrap_or(0.0), move |keep| {
                if let Some(p) = s.borrow_mut().project_mut() { p.answer(id, if keep { "yes" } else { "no" }) }
                schedule(&s);
            })
        } else {
            let q = qh_thread::Ask { tool: ask.tool.clone(), detail: ask.detail(), warn: ask.warn.clone(), always: ask.always.clone(), guarded: ask.guard };
            let s = st.clone();
            qh_thread::AskCard::new(&q, move |ans| {
                let ans = match ans { qh_thread::Answer::Once => "yes", qh_thread::Answer::Always => "always", qh_thread::Answer::Deny => "no" };
                if let Some(p) = s.borrow_mut().project_mut() { p.answer(id, ans) }
                schedule(&s);
            })
        };
        u.asks.append(card.widget());
        // a new ask rises in; re-rendering the same one (a plan update) doesn't replay it
        if ASK_SHOWN.with(|x| x.replace(id)) != id { qh_thread::appear(card.widget()) }
        if mine.len() > 1 { u.asks.append(&label(&format!("+ {} more waiting", mine.len() - 1), &["faint", "small"])) }
    }
    if !plan.0.is_empty() || plan.1 {
        let steps: Vec<(String, qh_thread::Step)> = plan.0.iter().map(|(t, s)| (t.clone(), match s.as_str() {
            "done" | "completed" => qh_thread::Step::Done, "active" | "doing" | "in_progress" => qh_thread::Step::Active, _ => qh_thread::Step::Pending,
        })).collect();
        let mut card = qh_thread::PlanCard::new(if plan.1 { "Plan ready" } else { "Plan" }, &steps);
        if plan.1 {
            let (s, tab) = (st.clone(), tab.clone());
            card = card.with_approve(move |edits| {
                if let Some(p) = s.borrow_mut().project_mut() {
                    p.send(json!({"type": "plan_approve", "tab": tab, "edits": edits}));
                    p.tab(&tab).plan_ready = false;
                }
                schedule(&s);
            });
        }
        u.asks.append(card.widget());
    }
    u.asks.set_margin_bottom(if u.asks.first_child().is_some() { 4 } else { 0 });
}

fn render_sidebar(st: &Shared, a: &App, u: &Ui) {
    let p = a.project();
    let sig = format!("{:?}|{:?}|{}", a.cfg.recent, a.current, p.map(|p| format!("{}|{}|{}", p.active,
        p.tabs.iter().map(|t| format!("{}:{}:{}", t.name, t.label(), t.busy())).collect::<Vec<_>>().join(","),
        p.sessions.iter().take(30).map(|s| format!("{}{}{}", s.id, s.title, model::ago(s.when))).collect::<String>())).unwrap_or_default());
    if !changed("sidebar", format!("full|{sig}")) { return }
    clear(&u.tiles);
    let mine = a.cfg.recent.clone();
    if mine.is_empty() {
        let l = label("Open a folder to start.  Ctrl+O", &["soft", "small"]);
        l.set_margin_start(8);
        u.tiles.append(&l);
    }
    for chunk in mine.chunks(3) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_homogeneous(true);
        for path in chunk {
            let b = gtk::Button::new();
            let l = gtk::Label::new(Some(&model::name_of(path)));
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
    if let Some(p) = p {
        let nc = row_button(&[("list-add-symbolic", ""), ("", "New chat"), ("", "~Ctrl+T")], false);
        nc.set_action_name(Some("app.new-tab"));
        nc.set_margin_top(12);
        u.side.append(&nc);
        u.side.append(&label(&p.name().to_uppercase(), &["header"]));
        for t in &p.tabs {
            let b = row_button(&[("", t.label()), ("", if t.busy() { "~working" } else { "" })], t.name == p.active);
            let (s, n) = (st.clone(), t.name.clone());
            b.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.active = n.clone(); p.page = None } schedule(&s) });
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
                        p.page = None;
                        match &open_in { Some(t) => p.active = t.clone(), None => p.send(json!({"type": "resume", "id": id})) }
                    }
                    schedule(&s)
                });
                u.side.append(&b);
            }
        }
    }
    render_dots(st, a, &u.dots);
}

/// Personal · Team: two dots at the bottom of the sidebar (Ctrl+Shift+T).
fn render_dots(st: &Shared, a: &App, dots: &gtk::Box) {
    clear(dots);
    for (label_, team) in [("Personal", false), ("Team", true)] {
        let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        d.add_css_class("dot");
        if a.team == team { d.add_css_class("on") }
        d.set_halign(gtk::Align::Center); d.set_valign(gtk::Align::Center);
        let b = gtk::Button::new();
        b.set_child(Some(&d));
        b.add_css_class("plain");
        b.set_size_request(26, 26);
        b.set_tooltip_text(Some(&format!("{label_} space (Ctrl+Shift+T)")));
        let s = st.clone();
        b.connect_clicked(move |_| set_team(&s, team));
        dots.append(&b);
    }
}

/// Compact: a rail of project initials, the space dots and the gear.
fn render_rail(st: &Shared, a: &App, u: &Ui) {
    if !changed("sidebar", format!("rail|{:?}|{:?}|{}", a.cfg.recent, a.current, a.team)) { return }
    clear(&u.rail);
    for path in a.cfg.recent.iter().take(10) {
        let name = model::name_of(path);
        let init: String = name.chars().filter(|c| c.is_alphanumeric()).take(2).collect();
        let b = gtk::Button::with_label(&init);
        b.add_css_class("tile"); b.add_css_class("railtile");
        if a.current.as_ref() == Some(path) { b.add_css_class("on") }
        b.set_tooltip_text(Some(&name));
        b.set_halign(gtk::Align::Center);
        let (s, p) = (st.clone(), path.clone());
        b.connect_clicked(move |_| { s.borrow_mut().open(&p); schedule(&s) });
        u.rail.append(&b);
    }
    u.rail.append(&gtk::Box::builder().vexpand(true).build());
    let dots = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dots.set_halign(gtk::Align::Center);
    render_dots(st, a, &dots);
    u.rail.append(&dots);
    let s = st.clone();
    let g = icon_button("emblem-system-symbolic", "Settings (Ctrl+,)", move || toggle_settings(&s));
    g.set_halign(gtk::Align::Center);
    u.rail.append(&g);
}

/// The team space's sidebar: channels, people, their agents (windows/MainWindow.cs RenderTeam).
fn render_team_sidebar(st: &Shared, a: &App, u: &Ui) {
    let team = a.project().map(|p| p.team.clone()).unwrap_or_default();
    let chan = a.project().map(|p| p.chan.clone()).unwrap_or_default();
    if !changed("sidebar", format!("team|{:?}|{team}|{chan}", a.current)) { return }
    clear(&u.tiles);
    clear(&u.side);
    let head = label("Team", &["ptitle"]);
    head.set_margin_start(8);
    u.tiles.append(&head);
    if team["running"].as_bool() != Some(true) {
        u.side.append(&label("TEAM", &["header"]));
        render_dots(st, a, &u.dots);
        return;
    }
    let status = team["status"].as_str().unwrap_or("");
    if !status.is_empty() { let l = label(status, &["soft", "small"]); l.set_margin_start(8); l.set_wrap(true); u.tiles.append(&l) }
    u.side.append(&label("CHANNELS", &["header"]));
    for c in team["channels"].as_array().into_iter().flatten() {
        let id = c["id"].as_str().unwrap_or("").to_string();
        let unread = c["unread"].as_u64().unwrap_or(0);
        let tail = if unread > 0 { format!("~{unread}") } else { String::new() };
        let b = row_button(&[("", c["label"].as_str().unwrap_or(&id)), ("", &tail)], id == chan);
        if c["pinged"].as_bool() == Some(true) && unread > 0 { b.add_css_class("pinged") }
        let s = st.clone();
        b.connect_clicked(move |_| {
            if let Some(p) = s.borrow_mut().project_mut() { p.chan = id.clone(); p.send(json!({"type": "team", "chan": id})) }
            schedule(&s)
        });
        u.side.append(&b);
    }
    u.side.append(&label("PEOPLE", &["header"]));
    let me = team["me"].as_str().unwrap_or("");
    for x in team["people"].as_array().into_iter().flatten() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_start(10); row.set_margin_top(3); row.set_margin_bottom(3);
        let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        d.add_css_class("pdot");
        if x["online"].as_bool() == Some(true) { d.add_css_class("on") }
        d.set_valign(gtk::Align::Center);
        row.append(&d);
        let login = x["login"].as_str().unwrap_or("");
        row.append(&label(&if login == me { format!("{login} (you)") } else { login.to_string() }, &["ink"]));
        let s = x["status"].as_str().unwrap_or("");
        if !s.is_empty() { let l = label(s, &["faint", "small"]); l.set_ellipsize(gtk::pango::EllipsizeMode::End); row.append(&l) }
        u.side.append(&row);
    }
    let agents: Vec<&serde_json::Value> = team["agents"].as_array().into_iter().flatten().collect();
    if !agents.is_empty() {
        u.side.append(&label("AGENTS", &["header"]));
        for g in agents {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 1);
            col.set_margin_start(10); col.set_margin_top(3); col.set_margin_bottom(3);
            col.append(&label(&format!("{} · {}", g["login"].as_str().unwrap_or(""), g["name"].as_str().unwrap_or("")), &["ink", "small"]));
            let waiting = g["waiting"].as_bool() == Some(true);
            let now = g["now"].as_str().filter(|s| !s.is_empty()).or(g["status"].as_str()).unwrap_or("");
            let l = label(if waiting { "waiting for its owner" } else { now }, &[if waiting { "accent" } else { "faint" }, "small"]);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            col.append(&l);
            u.side.append(&col);
        }
    }
    render_dots(st, a, &u.dots);
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
            h.append(&label(t, &["faint", "small"]));
        } else if !text.is_empty() {
            let l = label(text, &["ink"]);
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
    let p = a.project();
    u.tab_right.set_visible(p.is_some() && !a.team && !a.settings);
    if let Some(p) = p {
        u.switch.set_layout(match p.layout.as_str() { "split" => Layout::Split, "code" => Layout::Code, _ => Layout::Chat });
    }
    let sig = format!("{}|{}|{:?}", a.team, a.settings, p.map(|p| (p.path.clone(), p.active.clone(), p.page.clone(),
        p.tabs.iter().map(|t| format!("{}:{}:{}:{}", t.name, t.label(), t.status, p.asks.iter().any(|a| a.tab == t.name))).collect::<Vec<_>>())));
    if !changed("tabs", sig) { return }
    clear(&u.tabs);
    if a.settings { return }
    if a.team {
        if let Some(p) = p { let l = label(&p.chan, &["tab", "on"]); l.set_valign(gtk::Align::End); u.tabs.append(&l) }
        return;
    }
    let Some(p) = p else { return };
    for t in &p.tabs {
        let b = gtk::Button::new();
        let h = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        let asking = p.asks.iter().any(|a| a.tab == t.name);
        if asking || t.status == "working" {
            let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            d.add_css_class("tdot");
            d.add_css_class(if asking { "asking" } else { "working" });
            d.set_valign(gtk::Align::Center);
            d.set_tooltip_text(Some(if asking { "waiting for you" } else { "working" }));
            h.append(&d);
        }
        h.append(&gtk::Label::new(Some(t.label())));
        b.set_child(Some(&h));
        b.add_css_class("tab");
        if t.name == p.active && p.page.is_none() { b.add_css_class("on") }
        b.set_tooltip_text(Some("Middle-click to close"));
        let (s, n) = (st.clone(), t.name.clone());
        b.connect_clicked(move |_| { if let Some(p) = s.borrow_mut().project_mut() { p.active = n.clone(); p.page = None } schedule(&s) });
        let mid = gtk::GestureClick::builder().button(2).build();
        let (s, n) = (st.clone(), t.name.clone());
        mid.connect_released(move |_, _, _, _| if let Some(p) = s.borrow().project() { p.send(json!({"type": "close_tab", "tab": n})) });
        b.add_controller(mid);
        u.tabs.append(&b);
    }
}

fn render_empty(a: &App, u: &Ui) {
    let Some(p) = a.project() else { return };
    if !changed("empty", p.name()) { return }
    clear(&u.empty);
    let g = label(&format!("What should we work on in {}?", p.name()), &["greeting"]);
    g.set_wrap(true);
    u.empty.append(&g);
    let grid = gtk::Grid::builder().column_spacing(8).row_spacing(8).column_homogeneous(true).build();
    for (i, s) in ["Explain this project", "Find bugs in recent changes", "Write tests for ", "What changed since yesterday?"].iter().enumerate() {
        let shown = if s.ends_with(' ') { format!("{s}…") } else { s.to_string() };
        let b = gtk::Button::new();
        let l = label(&shown, &[]);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        b.set_child(Some(&l));
        b.add_css_class("starter");
        b.set_hexpand(true);
        b.set_cursor_from_name(Some("pointer"));
        let (c, fill) = (u.composer.clone(), s.to_string());
        b.connect_clicked(move |_| c.set_text(&fill));
        grid.attach(&b, (i % 2) as i32, (i / 2) as i32, 1, 1);
    }
    u.empty.append(&grid);
    u.empty.append(&label("Ctrl+T new chat  ·  / commands  ·  @ files  ·  Ctrl+K everything", &["mono", "faint", "small"]));
}

// MARK: Ctrl+K palette (apple/qh/Palette.swift): actions, layouts, tabs, commands, files, projects, settings

fn palette_items(st: &Shared) -> Vec<qh_desktop::Item> {
    use qh_desktop::Item;
    let a = st.borrow();
    let mut v: Vec<Item> = vec![];
    let act = |name: &'static str| move || { if let Some(app) = gio::Application::default() { app.activate_action(name, None) } };
    v.push(Item::new("New tab", "Ctrl+T", act("new-tab")));
    v.push(Item::new("Open folder…", "Ctrl+O", act("open-folder")));
    let compact = SIGS.with(|s| s.borrow().get("compact").cloned()) == Some("1".into());
    v.push(Item::new(if compact { "Full sidebar" } else { "Compact mode" }, "Ctrl+\\", act("compact")));
    v.push(Item::new(if a.team { "Personal space" } else { "Team space" }, "Ctrl+Shift+T", act("team")));
    v.push(Item::new("Settings", "Ctrl+,", act("settings")));
    if let Some(p) = a.project() {
        v.push(Item::new("Chat", "Ctrl+1", act("layout-chat")));
        v.push(Item::new("Chat and code", "Ctrl+2", act("layout-split")));
        v.push(Item::new("Code", "Ctrl+3", act("layout-code")));
        v.push(Item::new("Review changes", "Ctrl+D", act("review")));
        v.push(Item::new("Checkpoints", "rewind", act("checkpoints")));
        v.push(Item::new("Project memory", "NOTES.md", act("memory")));
        v.push(Item::new("Activity log", "commands, edits, network", act("activity")));
        v.push(Item::new("Background tasks", "", act("tasks")));
        v.push(Item::new("Connect models", "", act("setup")));
        v.push(Item::new(&format!("Mode: {}", p.next_mode()), "next mode", {
            let s = st.clone();
            move || if let Some(p) = s.borrow().project() { p.send(json!({"type": "mode", "mode": p.next_mode()})) }
        }));
        for th in ["system", "light", "dark"] {
            let s = st.clone();
            v.push(Item::new(format!("Theme: {th}"), "appearance", move || {
                s.borrow_mut().cfg.theme = th.into();
                s.borrow().cfg.save();
                theme::set_appearance(th);
                dirty("settings");
                schedule(&s);
            }));
        }
        for t in &p.tabs {
            let (s, n) = (st.clone(), t.name.clone());
            v.push(Item::new(format!("Tab: {}", t.label()), "tab", move || {
                if let Some(p) = s.borrow_mut().project_mut() { p.active = n.clone(); p.page = None }
                schedule(&s)
            }));
        }
        for (cmd, about) in &p.commands {
            let c = cmd.clone();
            v.push(Item::new(cmd.clone(), about.clone(), move || if let Some(u) = ui() { u.composer.set_text(&format!("{c} ")) }));
        }
        for f in p.files.iter().take(5000) {
            let (s, path, f2) = (st.clone(), p.path.clone(), f.clone());
            v.push(Item::new(f.clone(), "file", move || open_file(&s, &path, &f2)));
        }
    }
    for r in a.cfg.recent.iter().filter(|r| Some(*r) != a.current.as_ref()) {
        let (s, r2) = (st.clone(), r.clone());
        v.push(Item::new(format!("Project: {}", model::name_of(r)), r.clone(), move || { s.borrow_mut().open(&r2); schedule(&s) }));
    }
    v
}

pub fn show_palette(st: &Shared) {
    let Some(u) = ui() else { return };
    let items = palette_items(st);
    u.palette.set_items(items);
    u.palette.show(&u.overlay);
}

/// For snapshots: the palette's popover is its own surface, never mapped off-screen, so its card
/// can be shown inside the window where the popover would sit (and put back).
pub fn palette_inline(on: bool) {
    let Some(u) = ui() else { return };
    let card = u.palette.card().clone();
    let pop = u.palette.popover().clone();
    if on {
        pop.popdown();
        pop.set_child(None::<&gtk::Widget>);
        card.set_halign(gtk::Align::Center);
        card.set_valign(gtk::Align::Start);
        card.set_margin_top(56);
        card.add_css_class("inline-palette");
        u.overlay.add_overlay(&card);
    } else if card.parent().is_some() {
        u.overlay.remove_overlay(&card);
        pop.set_child(Some(&card));
    }
}

/// Renders a window to a PNG without the screen (debug hook for tests).
pub fn snapshot_png(w: &gtk::Widget, path: &str) -> Result<(), String> {
    let (wd, ht) = (w.width(), w.height());
    if wd == 0 || ht == 0 { return Err("window not allocated".into()) }
    // a toplevel paints nothing through WidgetPaintable: paint its child over the ground colour
    let child = w.first_child().ok_or("empty window")?;
    let snap = gtk::Snapshot::new();
    let peek = w.has_css_class("peek");
    let bg = gdk::RGBA::parse(if peek { "rgba(0,0,0,0)" } else { theme::ground(adw::StyleManager::default().is_dark()) }).unwrap();
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
pub fn show_peek(st: &Shared) {
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
            prompt(&s, &t, vec![]);
            e.set_text("");
            win.set_visible(false);
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

/// For the test hooks: the peek window, if any.
pub fn peek_window() -> Option<gtk::Window> { ui().and_then(|u| u.peek.borrow().as_ref().map(|p| p.0.clone())) }
