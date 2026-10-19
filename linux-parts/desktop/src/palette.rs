//! Command palette: a 560px card in a popover — search entry over a list,
//! fuzzy filter (letters in order; file-name hits rank first), Up/Down/Enter/Esc.
use gtk::prelude::*;
use gtk::{gdk, glib};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone)]
pub struct Item {
    pub title: String,
    pub hint: String,
    pub action: Rc<dyn Fn()>,
}

impl Item {
    pub fn new(title: impl Into<String>, hint: impl Into<String>, action: impl Fn() + 'static) -> Self {
        Item { title: title.into(), hint: hint.into(), action: Rc::new(action) }
    }
}

/// Fuzzy score: `None` if the query's letters don't appear in order in
/// `title`. Higher is better. A match entirely inside the file name (after the
/// last '/') gets a large bonus so file-name hits rank first; contiguous runs,
/// word starts and early matches add more.
pub fn score(query: &str, title: &str) -> Option<i32> {
    let q: Vec<char> = query.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let base = title.rfind('/').map(|i| i + 1).unwrap_or(0);
    let name = &title[base..];
    if let Some(s) = subseq(&q, name) {
        return Some(10_000 + s);
    }
    subseq(&q, title)
}

fn subseq(q: &[char], s: &str) -> Option<i32> {
    let t: Vec<char> = s.chars().collect();
    let lower: Vec<char> = t.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let (mut qi, mut score, mut prev, mut first) = (0usize, 0i32, None::<usize>, None::<usize>);
    for (i, c) in lower.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            first.get_or_insert(i);
            score += 1;
            if prev == Some(i.wrapping_sub(1)) {
                score += 8;
            }
            if i == 0 || matches!(t[i - 1], '/' | '-' | '_' | '.' | ' ') || (t[i].is_uppercase() && t[i - 1].is_lowercase()) {
                score += 6;
            }
            prev = Some(i);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| score * 10 - first.unwrap_or(0) as i32 - t.len() as i32 / 8)
}

const CSS: &str = r#"
.qh-palette > contents { padding: 0; border-radius: 12px; background: #FBFBF9; border: 1px solid #E3E2DC; box-shadow: 0 12px 40px rgba(0,0,0,0.18); }
.qh-card { background: #FBFBF9; color: #0E0E0D; border-radius: 12px; font-family: "Geist", sans-serif; }
.qh-card entry { background: transparent; border: none; box-shadow: none; outline: none; font-size: 15px; padding: 14px; min-height: 0; color: #0E0E0D; caret-color: #FF4D1C; }
.qh-card .qh-rule { background: #DCDCD5; min-height: 1px; }
.qh-card entry > text > selection { background: rgba(255,77,28,0.28); color: inherit; }
.qh-card list { background: transparent; padding: 6px; }
.qh-card row, .qh-card label { color: #0E0E0D; }
.qh-card row { min-height: 32px; padding: 0 12px; border-radius: 6px; background: transparent; outline: none; }
.qh-card row:selected { background: rgba(255,77,28,0.14); color: #0E0E0D; }
.qh-card row:hover:not(:selected) { background: #E6E6E0; }
.qh-card .qh-title { font-size: 13px; }
.qh-card .qh-mono { font-family: "Geist Mono", monospace; font-size: 12.5px; }
.qh-card label.qh-hint { font-family: "Geist Mono", monospace; font-size: 11px; color: #9A9A94; }
.qh-card label.qh-empty { font-size: 12.5px; color: #9A9A94; padding: 14px; }
/* dark: .qh-dark on card + popover */
.qh-palette.qh-dark > contents { background: #1D1D1B; border-color: #31312E; box-shadow: 0 12px 40px rgba(0,0,0,0.5); }
.qh-card.qh-dark { background: #1D1D1B; color: #ECEBE6; }
.qh-card.qh-dark row, .qh-card.qh-dark label { color: #ECEBE6; }
.qh-card.qh-dark entry { color: #ECEBE6; caret-color: #FF5A2A; }
.qh-card.qh-dark .qh-rule { background: #34342F; }
.qh-card.qh-dark row:selected { background: rgba(255,90,42,0.14); }
.qh-card.qh-dark row:hover:not(:selected) { background: #2E2E2B; }
.qh-card.qh-dark label.qh-hint, .qh-card.qh-dark label.qh-empty { color: #85847D; }
"#;

struct State {
    items: Vec<Item>,
    shown: Vec<usize>,
}

#[derive(Clone)]
pub struct Palette {
    popover: gtk::Popover,
    card: gtk::Box,
    entry: gtk::Entry,
    list: gtk::ListBox,
    state: Rc<RefCell<State>>,
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

impl Palette {
    /// Call after gtk::init(). Installs the palette CSS on the default display.
    pub fn new() -> Self {
        if let Some(d) = gdk::Display::default() {
            let css = gtk::CssProvider::new();
            css.load_from_string(CSS);
            gtk::style_context_add_provider_for_display(&d, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("qh-card");
        card.set_size_request(560, -1);
        let entry = gtk::Entry::builder().placeholder_text("Search files, tabs and commands").hexpand(true).build();
        let rule = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        rule.add_css_class("qh-rule");
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(true);
        let empty = gtk::Label::new(Some("Nothing matches."));
        empty.add_css_class("qh-empty");
        empty.set_xalign(0.0);
        empty.set_valign(gtk::Align::Start);
        list.set_placeholder(Some(&empty));
        let scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).min_content_height(360).max_content_height(360).build();
        card.append(&entry);
        card.append(&rule);
        card.append(&scroll);
        let popover = gtk::Popover::builder().has_arrow(false).autohide(true).child(&card).build();
        popover.add_css_class("qh-palette");

        let p = Palette { popover, card, entry, list, state: Rc::new(RefCell::new(State { items: vec![], shown: vec![] })) };

        let q = p.clone();
        p.entry.connect_changed(move |_| q.refilter());
        let q = p.clone();
        p.list.connect_row_activated(move |_, row| q.run(row.index()));
        let q = p.clone();
        p.entry.connect_activate(move |_| {
            if let Some(r) = q.list.selected_row() {
                q.run(r.index());
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let q = p.clone();
        keys.connect_key_pressed(move |_, key, _, _| match key {
            gdk::Key::Up | gdk::Key::KP_Up => { q.step(-1); glib::Propagation::Stop }
            gdk::Key::Down | gdk::Key::KP_Down => { q.step(1); glib::Propagation::Stop }
            gdk::Key::Escape => { q.hide(); glib::Propagation::Stop }
            _ => glib::Propagation::Proceed,
        });
        p.card.add_controller(keys);
        p.set_dark(system_dark());
        p
    }

    /// Force the dark or light Zen palette (auto-detected at construction).
    pub fn set_dark(&self, dark: bool) {
        for w in [self.card.upcast_ref::<gtk::Widget>(), self.popover.upcast_ref()] {
            if dark { w.add_css_class("qh-dark") } else { w.remove_css_class("qh-dark") }
        }
    }

    pub fn set_items(&self, items: Vec<Item>) {
        self.state.borrow_mut().items = items;
        self.refilter();
    }

    /// Pop the palette up over `on` (usually the window's content), focused
    /// and cleared.
    pub fn show(&self, on: &impl IsA<gtk::Widget>) {
        let on = on.as_ref();
        if self.popover.parent().as_ref() != Some(on) {
            if self.popover.parent().is_some() {
                self.popover.unparent();
            }
            self.popover.set_parent(on);
        }
        // Anchor near the top centre of the parent, like the Mac palette.
        let w = on.width().max(1);
        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(w / 2, 48, 1, 1)));
        self.popover.set_position(gtk::PositionType::Bottom);
        self.entry.set_text("");
        self.refilter();
        self.popover.popup();
        self.entry.grab_focus();
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

    /// The card itself (to embed elsewhere or snapshot).
    pub fn card(&self) -> &gtk::Box {
        &self.card
    }
    pub fn popover(&self) -> &gtk::Popover {
        &self.popover
    }
    pub fn entry(&self) -> &gtk::Entry {
        &self.entry
    }

    fn refilter(&self) {
        let q = self.entry.text().to_string();
        let mut st = self.state.borrow_mut();
        let mut hits: Vec<(i32, usize)> = st.items.iter().enumerate().filter_map(|(i, it)| score(&q, &it.title).map(|s| (s, i))).collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.truncate(200);
        st.shown = hits.into_iter().map(|(_, i)| i).collect();
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        for &i in &st.shown {
            let it = &st.items[i];
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let title = gtk::Label::new(Some(&it.title));
            title.add_css_class(if it.title.contains('/') || it.title.contains('.') { "qh-mono" } else { "qh-title" });
            title.set_xalign(0.0);
            title.set_hexpand(true);
            title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            let hint = gtk::Label::new(Some(&it.hint));
            hint.add_css_class("qh-hint");
            hint.set_ellipsize(gtk::pango::EllipsizeMode::End);
            hint.set_max_width_chars(28);
            row.append(&title);
            row.append(&hint);
            self.list.append(&row);
        }
        drop(st);
        if let Some(r) = self.list.row_at_index(0) {
            self.list.select_row(Some(&r));
        }
    }

    fn step(&self, d: i32) {
        let n = self.state.borrow().shown.len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.list.selected_row().map(|r| r.index()).unwrap_or(0);
        let next = (cur + d).clamp(0, n - 1);
        if let Some(r) = self.list.row_at_index(next) {
            self.list.select_row(Some(&r));
            r.grab_focus();
            self.entry.grab_focus_without_selecting();
        }
    }

    fn run(&self, idx: i32) {
        let action = {
            let st = self.state.borrow();
            st.shown.get(idx as usize).map(|&i| st.items[i].action.clone())
        };
        if let Some(a) = action {
            self.hide();
            a();
        }
    }
}

/// Dark if GTK prefers dark, the theme name says so, or the GNOME/portal
/// colour-scheme key is prefer-dark (Omarchy sets that one).
pub fn system_dark() -> bool {
    if let Some(st) = gtk::Settings::default() {
        if st.is_gtk_application_prefer_dark_theme() || st.gtk_theme_name().is_some_and(|n| n.to_lowercase().contains("dark")) {
            return true;
        }
    }
    let src = gtk::gio::SettingsSchemaSource::default();
    if src.and_then(|s| s.lookup("org.gnome.desktop.interface", true)).is_some_and(|sc| sc.has_key("color-scheme")) {
        return gtk::gio::Settings::new("org.gnome.desktop.interface").string("color-scheme") == "prefer-dark";
    }
    false
}
