//! Pages over the chat (apple/qh/CoreUI.swift, windows/Pages.cs, Settings.cs): project memory,
//! the activity log, background tasks, settings, the team room and the model setup.

use crate::model::{self, App, Project};
use crate::theme;
use crate::ui::{self, clear, dirty, schedule, Shared, SETUP_DIRTY};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

thread_local! {
    static FILTER: RefCell<String> = RefCell::new("All".into());
    static SIG: RefCell<std::collections::HashMap<&'static str, String>> = RefCell::default();
    static MEM_SHOWN: Cell<(u32, usize)> = const { Cell::new((u32::MAX, 0)) };
}

fn changed(part: &'static str, sig: String) -> bool {
    SIG.with(|s| s.borrow_mut().insert(part, sig.clone()).as_ref() != Some(&sig))
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes { l.add_css_class(c) }
    l
}

/// A scrolling column with a title, an about line and Esc.
fn column(title: &str, about: &str, close: impl Fn() + 'static) -> (gtk::ScrolledWindow, gtk::Box) {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 14);
    col.set_margin_start(32); col.set_margin_end(32); col.set_margin_top(22); col.set_margin_bottom(32);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let t = label(title, &["ptitle"]);
    t.set_hexpand(true);
    head.append(&t);
    let x = gtk::Button::with_label("Esc");
    x.add_css_class("plain"); x.add_css_class("faint"); x.add_css_class("small");
    x.connect_clicked(move |_| close());
    head.append(&x);
    col.append(&head);
    if !about.is_empty() { let a = label(about, &["soft"]); a.set_wrap(true); col.append(&a) }
    let clamp = adw::Clamp::builder().maximum_size(760).child(&col).build();
    let sw = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();
    (sw, col)
}

fn close_page(st: &Shared) -> impl Fn() + 'static {
    let s = st.clone();
    move || { if let Some(p) = s.borrow_mut().project_mut() { p.page = None } schedule(&s) }
}

/// A row of pill buttons, one on.
fn segmented(opts: &[(&str, &str)], on: &str, f: impl Fn(String) + 'static) -> gtk::Box {
    let g = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    g.add_css_class("seg-group");
    g.set_valign(gtk::Align::Center);
    g.set_halign(gtk::Align::Start);
    let f = Rc::new(f);
    for (title, v) in opts {
        let b = gtk::Button::with_label(title);
        b.add_css_class("seg");
        if *v == on { b.add_css_class("on") }
        let (f, v) = (f.clone(), v.to_string());
        b.connect_clicked(move |_| f(v.clone()));
        g.append(&b);
    }
    g
}

pub struct Pages {
    pub setup_page: gtk::ScrolledWindow,
    setup: gtk::Box,
    setup_models: gtk::Box,
    setup_filter: gtk::SearchEntry,
    pub memory: gtk::ScrolledWindow,
    mem_text: gtk::TextView,
    mem_path: gtk::Label,
    mem_status: gtk::Label,
    mem_instr: gtk::Box,
    pub activity_page: gtk::ScrolledWindow,
    activity: gtk::Box,
    pub tasks_page: gtk::ScrolledWindow,
    tasks: gtk::Label,
    pub settings_page: gtk::ScrolledWindow,
    settings: gtk::Box,
    pub team_page: gtk::Box,
    team: gtk::Box,
    team_scroll: gtk::ScrolledWindow,
}

impl Pages {
    pub fn new(st: &Shared) -> Pages {
        let (setup_page, setup, setup_models, setup_filter) = setup_page(st);

        let (memory, col) = column("Project memory", "What qh remembers about this project (NOTES.md). It reads this at the start of every chat.", close_page(st));
        let mem_path = label("", &["mono", "faint", "small"]);
        mem_path.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        col.append(&mem_path);
        let mem_text = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).monospace(true)
            .top_margin(10).bottom_margin(10).left_margin(12).right_margin(12).build();
        mem_text.add_css_class("memtext");
        let mem_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).min_content_height(260).max_content_height(520)
            .propagate_natural_height(true).child(&mem_text).build();
        mem_scroll.add_css_class("membox");
        col.append(&mem_scroll);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let save = gtk::Button::with_label("Save");
        save.add_css_class("qh-btn"); save.add_css_class("qh-btn-ink");
        let mem_status = label("", &["faint", "small"]);
        row.append(&save); row.append(&label("Ctrl+S", &["faint", "small"])); row.append(&mem_status);
        col.append(&row);
        let mem_instr = gtk::Box::new(gtk::Orientation::Vertical, 10);
        col.append(&mem_instr);
        let do_save = {
            let (s, t, status) = (st.clone(), mem_text.clone(), mem_status.clone());
            Rc::new(move || {
                let b = t.buffer();
                let text = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
                if let Some(p) = s.borrow().project() { p.cmd("memory_save", json!({"text": text})) }
                status.set_text("saved");
            })
        };
        let f = do_save.clone();
        save.connect_clicked(move |_| f());
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, k, _, m| {
            if m.contains(gdk::ModifierType::CONTROL_MASK) && (k == gdk::Key::s || k == gdk::Key::S) { do_save(); glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        mem_text.add_controller(keys);
        let st2 = mem_status.clone();
        mem_text.buffer().connect_changed(move |_| st2.set_text(""));

        let (activity_page, activity) = column("Activity", "What qh did in this chat: commands run, files edited, network asks, approvals. Newest first.", close_page(st));
        let (tasks_page, col) = column("Background tasks", "Commands qh started in the background for this chat.", close_page(st));
        let tasks = label("", &["mono", "small", "ink"]);
        tasks.set_selectable(true);
        tasks.set_wrap(true);
        col.append(&tasks);
        let s = st.clone();
        let (settings_page, settings) = column("Settings", "", move || { s.borrow_mut().settings = false; schedule(&s) });

        let team_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let team = gtk::Box::new(gtk::Orientation::Vertical, 12);
        team.set_margin_start(32); team.set_margin_end(32); team.set_margin_top(20); team.set_margin_bottom(20);
        let clamp = adw::Clamp::builder().maximum_size(760).child(&team).build();
        let team_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();
        team_page.append(&team_scroll);

        Pages { setup_page, setup, setup_models, setup_filter, memory, mem_text, mem_path, mem_status, mem_instr, activity_page, activity,
            tasks_page, tasks, settings_page, settings, team_page, team, team_scroll }
    }

    // MARK: memory: what's typed survives redraws; a fresh "memory" from serve replaces it

    pub fn render_memory(&self, p: &Project) {
        let key = (p.memory_ver, p as *const _ as usize);
        if MEM_SHOWN.with(|m| m.replace(key)) == key { return }
        let Some((path, text, instr)) = &p.memory else { self.mem_path.set_text("loading…"); MEM_SHOWN.with(|m| m.set((u32::MAX, 0))); return };
        self.mem_path.set_text(path);
        self.mem_text.buffer().set_text(text);
        self.mem_status.set_text(if text.is_empty() { "Nothing remembered yet: one note per line." } else { "" });
        clear(&self.mem_instr);
        if !instr.is_empty() {
            let h = label("Instruction files (read-only)", &["medium", "ink"]);
            h.set_margin_top(12);
            self.mem_instr.append(&h);
        }
        for (path, text) in instr {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
            b.add_css_class("tilebox");
            b.append(&label(path, &["mono", "soft", "small"]));
            let t = label(text, &["mono", "soft", "small"]);
            t.set_wrap(true); t.set_selectable(true);
            let sw = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).max_content_height(260).propagate_natural_height(true).child(&t).build();
            b.append(&sw);
            self.mem_instr.append(&b);
        }
    }

    // MARK: activity, with filters

    pub fn render_activity(&self, st: &Shared, p: &Project) {
        let filter = FILTER.with(|f| f.borrow().clone());
        let sig = format!("{}|{filter}|{}|{}", p.path, p.activity.len(), p.activity.first().map(|a| a.t).unwrap_or(0.0));
        if !changed("activity", sig) { return }
        // keep the title row and the about line
        while let Some(c) = self.activity.last_child() {
            if self.activity.first_child().and_then(|f| f.next_sibling()).as_ref() == Some(&c) { break }
            self.activity.remove(&c);
        }
        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        chips.add_css_class("seg-group");
        chips.set_halign(gtk::Align::Start);
        for f in ["All", "Commands", "Edits", "Network", "Approvals"] {
            let n = if f == "All" { p.activity.len() } else { p.activity.iter().filter(|a| a.group() == f).count() };
            let b = gtk::Button::with_label(&if n > 0 { format!("{f}  {n}") } else { f.to_string() });
            b.add_css_class("seg");
            if f == filter { b.add_css_class("on") }
            let s = st.clone();
            b.connect_clicked(move |_| { FILTER.with(|x| *x.borrow_mut() = f.to_string()); schedule(&s) });
            chips.append(&b);
        }
        self.activity.append(&chips);
        let rows: Vec<&model::Activity> = p.activity.iter().filter(|a| filter == "All" || a.group() == filter).collect();
        if rows.is_empty() { self.activity.append(&label("Nothing here yet.", &["faint"])) }
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for a in rows {
            let g = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            g.add_css_class("actrow");
            let when = glib::DateTime::from_unix_local(a.t as i64).ok().and_then(|d| d.format("%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default();
            let w = label(&when, &["mono", "faint", "small"]);
            w.set_width_chars(8);
            g.append(&w);
            let k = label(a.group(), &["soft", "small"]);
            k.set_width_chars(10);
            g.append(&k);
            let t = label(&a.text, &["mono", "ink", "small"]);
            t.set_hexpand(true);
            t.set_ellipsize(gtk::pango::EllipsizeMode::End);
            t.set_tooltip_text(Some(&a.text));
            g.append(&t);
            g.append(&label(if a.ok { "✓" } else { "✗" }, &[if a.ok { "faint" } else { "accent" }]));
            list.append(&g);
        }
        self.activity.append(&list);
    }

    pub fn render_tasks(&self, p: &Project) {
        self.tasks.set_text(if p.tasks.trim().is_empty() { "No background tasks." } else { &p.tasks });
    }

    // MARK: settings

    pub fn render_settings(&self, st: &Shared, a: &App) {
        let c = &a.cfg;
        if !changed("settings", format!("{}|{}|{}|{:?}", c.theme, c.default_layout, c.reopen, c.compact)) { return }
        while let Some(ch) = self.settings.last_child() {
            if self.settings.first_child().as_ref() == Some(&ch) { break }
            self.settings.remove(&ch);
        }
        let row = |title: &str, about: &str, control: &gtk::Widget| {
            let r = gtk::Box::new(gtk::Orientation::Horizontal, 16);
            r.add_css_class("setrow");
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            col.set_hexpand(true);
            col.append(&label(title, &["medium", "ink"]));
            let ab = label(about, &["soft", "small"]);
            ab.set_wrap(true);
            col.append(&ab);
            r.append(&col);
            control.set_valign(gtk::Align::Center);
            r.append(control);
            self.settings.append(&r);
        };
        let s = st.clone();
        row("Appearance", "Follow the system, or pick one.", segmented(&[("System", "system"), ("Light", "light"), ("Dark", "dark")], &c.theme, move |v| {
            theme::set_appearance(&v);
            s.borrow_mut().cfg.theme = v;
            s.borrow().cfg.save();
            schedule(&s);
        }).upcast_ref());
        let s = st.clone();
        row("Default layout", "How a project opens: the chat alone, or the chat with code beside it.",
            segmented(&[("Chat", "chat"), ("Split", "split")], &c.default_layout, move |v| {
                s.borrow_mut().cfg.default_layout = v;
                s.borrow().cfg.save();
                schedule(&s);
            }).upcast_ref());
        let sw = gtk::Switch::new();
        sw.set_active(c.reopen);
        let s = st.clone();
        sw.connect_active_notify(move |w| { s.borrow_mut().cfg.reopen = w.is_active(); s.borrow().cfg.save(); dirty("settings"); schedule(&s) });
        row("Reopen last project", "Start where you left off.", sw.upcast_ref());
        let cm = match c.compact { None => "auto", Some(true) => "on", Some(false) => "off" };
        let s = st.clone();
        row("Compact sidebar", "A rail of project initials. Auto turns it on under 900 px. Ctrl+\\",
            segmented(&[("Auto", "auto"), ("On", "on"), ("Off", "off")], cm, move |v| {
                s.borrow_mut().cfg.compact = match v.as_str() { "on" => Some(true), "off" => Some(false), _ => None };
                s.borrow().cfg.save();
                schedule(&s);
            }).upcast_ref());
        let peek = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let line = "bind = CTRL ALT, space, exec, qh-app --peek";
        let l = label(line, &["mono", "small", "ink", "tilebox"]);
        l.set_selectable(true);
        peek.append(&l);
        let copy = gtk::Button::with_label("Copy");
        copy.add_css_class("qh-btn"); copy.add_css_class("qh-btn-plain");
        copy.set_halign(gtk::Align::Start);
        copy.connect_clicked(move |b| b.clipboard().set_text(line));
        peek.append(&copy);
        row("Peek", "Ask qh from anywhere: Ctrl+Alt+Space. Add this line to ~/.config/hypr/bindings.conf (or bind qh-app --peek in your desktop's shortcuts).", peek.upcast_ref());
        let m = gtk::Button::with_label("Connect models…");
        m.add_css_class("qh-btn"); m.add_css_class("qh-btn-card");
        let s = st.clone();
        m.connect_clicked(move |_| {
            s.borrow_mut().settings = false;
            if let Some(p) = s.borrow_mut().project_mut() { p.show_setup = true; p.load_setup() }
            SETUP_DIRTY.with(|d| d.set(true));
            schedule(&s);
        });
        row("Models", "API keys, ChatGPT sign-in, the default model.", m.upcast_ref());
        self.settings.append(&label("Shortcuts: Ctrl+K everything · Ctrl+T new tab · Ctrl+1/2/3 layouts · Ctrl+D review · Ctrl+Shift+T team · Ctrl+\\ compact", &["faint", "small"]));
    }

    // MARK: team room

    pub fn render_team(&self, _st: &Shared, a: &App) {
        let team = a.project().map(|p| p.team.clone()).unwrap_or_default();
        if !changed("team", format!("{:?}|{team}", a.current)) { return }
        clear(&self.team);
        let Some(_) = a.project() else { self.team.append(&label("Open a folder to start.", &["faint"])); return };
        if team.is_null() { self.team.append(&label("Loading the team room…", &["faint"])); return }
        if team["running"].as_bool() != Some(true) {
            let l = label("No team in this project yet. Start one from a terminal with qh's /team, then come back here.", &["faint"]);
            l.set_wrap(true);
            self.team.append(&l);
            return;
        }
        for w in team["waiting"].as_array().into_iter().flatten() {
            let q = if w["question"].as_bool() == Some(true) { " asks" } else { "" };
            let l = label(&format!("{}{q}: {}", w["from"].as_str().unwrap_or(""), w["text"].as_str().unwrap_or("")), &["ink", "tilebox"]);
            l.set_wrap(true);
            self.team.append(&l);
        }
        let msgs: Vec<&serde_json::Value> = team["messages"].as_array().into_iter().flatten().collect();
        for m in &msgs {
            let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            head.append(&label(m["who"].as_str().unwrap_or(""), &["medium", "ink"]));
            let when = match &m["when"] { serde_json::Value::String(s) => s.clone(), v => v.to_string() };
            head.append(&label(&when, &["faint", "small"]));
            col.append(&head);
            let t = label(m["text"].as_str().unwrap_or(""), &["reply"]);
            t.set_wrap(true); t.set_selectable(true);
            col.append(&t);
            self.team.append(&col);
        }
        if msgs.is_empty() && self.team.first_child().is_none() { self.team.append(&label("Nothing here yet. Say hi.", &["faint"])) }
        let adj = self.team_scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper()));
    }

    // MARK: setup (connect models)

    pub fn render_setup(&self, st: &Shared, a: &App) {
        let Some(p) = a.project() else { return };
        clear(&self.setup);
        let sv = p.setup.clone().unwrap_or_default();
        let needed = sv["needed"].as_bool() != Some(false);
        if !needed {
            let done = gtk::Button::with_label("done");
            done.add_css_class("plain"); done.add_css_class("mono"); done.set_halign(gtk::Align::End);
            done.set_action_name(Some("app.setup"));
            self.setup.append(&done);
        }
        if let Some((m, err)) = &p.setup_msg {
            let l = label(m, &["mono", "small", if *err { "err" } else { "accent" }]);
            l.set_selectable(true); l.set_wrap(true);
            self.setup.append(&l);
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
            self.setup.append(&card);
        }

        clear(&self.setup_models);
        self.setup_filter.set_visible(!p.models.is_empty());
        if p.models.is_empty() { return }
        self.setup_filter.set_placeholder_text(Some(&format!("search {} models", p.models.len())));
        let q = self.setup_filter.text().to_lowercase();
        let def = sv["default"].as_str().unwrap_or("").to_string();
        self.setup_models.prepend(&label("default model", &["mono", "soft", "small"]));
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
            self.setup_models.append(&b);
        }
    }
}

fn setup_page(st: &Shared) -> (gtk::ScrolledWindow, gtk::Box, gtk::Box, gtk::SearchEntry) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.set_margin_start(40); outer.set_margin_end(40); outer.set_margin_top(34); outer.set_margin_bottom(34);
    outer.append(&label("Connect your models", &["title"]));
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
    filter.connect_search_changed(move |_| { SETUP_DIRTY.with(|d| d.set(true)); ui::schedule(&s) });
    let clamp = adw::Clamp::builder().maximum_size(640).child(&outer).build();
    let sc = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();
    (sc, setup, models, filter)
}
