//! The composer (apple/qh/ChatView.swift, windows/MainWindow*.cs): one line tall when empty, / commands
//! and @ files in a popup (Up/Down/Tab/Enter/Esc), image chips (+ or a pasted texture), the model and
//! mode as quiet buttons, a real up arrow to send. Ctrl+Enter / Ctrl+Backspace answer an open ask
//! even while the text has focus (the Windows app's composer swallowed them).

use crate::model;
use crate::ui::{self, Shared};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone)]
pub struct Composer {
    pub root: gtk::Box,
    pub text: gtk::TextView,
    pub placeholder: gtk::Label,
    pub model: gtk::Button,
    pub mode: gtk::Button,
    pub tokens: gtk::Label,
    pub attach: gtk::Button,
    pub stop: gtk::Button,
    pub send: gtk::Button,
    chips: gtk::Box,
    sugg: gtk::Box,
    attached: Rc<RefCell<Vec<(String, String)>>>,
    items: Rc<RefCell<Vec<(String, String, String)>>>,
    sel: Rc<Cell<usize>>,
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes { l.add_css_class(c) }
    l
}

/// An up arrow drawn in the button's own colour (go-up-symbolic reads as a chevron).
fn arrow() -> gtk::DrawingArea {
    let d = gtk::DrawingArea::new();
    d.set_content_width(14);
    d.set_content_height(14);
    d.set_halign(gtk::Align::Center);
    d.set_valign(gtk::Align::Center);
    d.set_draw_func(|w, cr, wd, ht| {
        let c = w.color();
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
        cr.set_line_width(1.9);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        // 14 px, centred whatever the allocation
        let (cx, top) = (wd as f64 / 2.0, (ht as f64 - 14.0) / 2.0);
        cr.move_to(cx, top + 12.5);
        cr.line_to(cx, top + 1.5);
        cr.move_to(cx - 5.0, top + 6.5);
        cr.line_to(cx, top + 1.5);
        cr.line_to(cx + 5.0, top + 6.5);
        let _ = cr.stroke();
    });
    d
}

/// Letters of `q` in order inside `s` (lower-cased): lower is better, None when they aren't.
fn fuzzy(s: &str, q: &str) -> Option<usize> {
    if q.is_empty() { return Some(0) }
    let name_at = s.rfind('/').map(|i| i + 1).unwrap_or(0);
    if let Some(i) = s[name_at..].find(q) { return Some(i) }
    if let Some(i) = s.find(q) { return Some(100 + i) }
    let (mut gaps, mut last, mut it) = (0, 0, s.char_indices());
    for qc in q.chars() {
        let (i, _) = it.by_ref().find(|(_, c)| *c == qc)?;
        gaps += i - last;
        last = i;
    }
    Some(1000 + gaps)
}

impl Composer {
    pub fn new(st: &Shared) -> Composer {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_margin_start(24); root.set_margin_end(24); root.set_margin_bottom(16); root.set_margin_top(4);
        let sugg = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sugg.add_css_class("sugg");
        sugg.set_visible(false);
        let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
        bx.add_css_class("composer");
        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chips.set_visible(false);
        bx.append(&chips);
        let text = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).accepts_tab(false)
            .top_margin(0).bottom_margin(0).left_margin(0).right_margin(0).build();
        let tscroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true)
            .vscrollbar_policy(gtk::PolicyType::External).max_content_height(240).child(&text).build();
        let ts = tscroll.clone();
        text.buffer().connect_changed(move |b| ts.set_vscrollbar_policy(if b.line_count() > 9 { gtk::PolicyType::Automatic } else { gtk::PolicyType::External }));
        let placeholder = label("Ask anything · / commands · @ files", &["placeholder"]);
        placeholder.set_can_target(false);
        placeholder.set_valign(gtk::Align::Start);
        placeholder.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let ov = gtk::Overlay::new();
        ov.set_child(Some(&tscroll));
        ov.add_overlay(&placeholder);
        bx.append(&ov);

        let meta = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let attach = gtk::Button::from_icon_name("list-add-symbolic");
        attach.add_css_class("metab");
        attach.set_tooltip_text(Some("Attach images (or paste one)"));
        let model = gtk::Button::with_label("");
        model.add_css_class("metab");
        model.set_tooltip_text(Some("Change model"));
        let mode = gtk::Button::with_label("");
        mode.add_css_class("metab");
        mode.set_tooltip_text(Some("Next mode"));
        let tokens = label("", &["meta"]);
        tokens.set_margin_start(6);
        meta.append(&attach); meta.append(&model); meta.append(&mode); meta.append(&tokens);
        meta.append(&gtk::Box::builder().hexpand(true).build());
        let stop = gtk::Button::with_label("stop");
        stop.add_css_class("plain"); stop.add_css_class("accent"); stop.set_visible(false);
        stop.set_margin_end(8);
        meta.append(&stop);
        let send = gtk::Button::new();
        send.set_child(Some(&arrow()));
        send.add_css_class("send");
        send.set_tooltip_text(Some("Send (Enter)"));
        send.set_valign(gtk::Align::Center);
        meta.append(&send);
        bx.append(&meta);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.append(&sugg);
        col.append(&bx);
        let clamp = adw::Clamp::builder().maximum_size(760).child(&col).build();
        root.append(&clamp);

        let c = Composer { root, text, placeholder, model, mode, tokens, attach, stop, send, chips, sugg,
            attached: Rc::default(), items: Rc::default(), sel: Rc::default() };
        c.wire(st);
        c
    }

    fn wire(&self, st: &Shared) {
        let (ph, me, s) = (self.placeholder.clone(), self.clone(), st.clone());
        self.text.buffer().connect_changed(move |b| {
            ph.set_visible(b.char_count() == 0);
            me.suggest(&s);
            me.send_state(&s);
        });
        let s = st.clone();
        self.stop.connect_clicked(move |_| if let Some(p) = s.borrow().project() { p.stop() });
        let (s, me) = (st.clone(), self.clone());
        self.send.connect_clicked(move |_| {
            let busy = s.borrow().project().and_then(|p| p.current()).is_some_and(|t| t.busy());
            if busy && me.is_empty() { if let Some(p) = s.borrow().project() { p.stop() } } else { me.submit(&s) }
        });
        let s = st.clone();
        self.model.connect_clicked(move |_| ui::prompt(&s, "/model", vec![]));
        let s = st.clone();
        self.mode.connect_clicked(move |_| {
            if let Some(p) = s.borrow().project() { p.send(json!({"type": "mode", "mode": p.next_mode()})) }
        });
        let me = self.clone();
        self.attach.connect_clicked(move |_| me.pick_images());

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (s, me) = (st.clone(), self.clone());
        keys.connect_key_pressed(move |_, k, _, m| {
            let ctrl = m.contains(gdk::ModifierType::CONTROL_MASK);
            let enter = k == gdk::Key::Return || k == gdk::Key::KP_Enter;
            // an open ask takes Ctrl+Enter / Ctrl+Backspace before the text does
            if ctrl && (enter || k == gdk::Key::BackSpace) && ui::answer_current(&s, if enter { "yes" } else { "no" }) {
                return glib::Propagation::Stop;
            }
            if me.sugg.is_visible() {
                let n = me.items.borrow().len();
                match k {
                    gdk::Key::Up => { me.sel.set((me.sel.get() + n - 1) % n.max(1)); me.draw_sugg(); return glib::Propagation::Stop }
                    gdk::Key::Down => { me.sel.set((me.sel.get() + 1) % n.max(1)); me.draw_sugg(); return glib::Propagation::Stop }
                    gdk::Key::Tab | gdk::Key::Return | gdk::Key::KP_Enter => { me.accept(me.sel.get()); return glib::Propagation::Stop }
                    gdk::Key::Escape => { me.hide_sugg(); return glib::Propagation::Stop }
                    _ => {}
                }
            }
            if enter && !m.contains(gdk::ModifierType::SHIFT_MASK) {
                me.submit(&s);
                return glib::Propagation::Stop;
            }
            if k == gdk::Key::Escape {
                if let Some(p) = s.borrow().project() { if p.current().is_some_and(|t| t.busy()) { p.stop(); return glib::Propagation::Stop } }
            }
            if ctrl && (k == gdk::Key::v || k == gdk::Key::V) && me.paste_image() { return glib::Propagation::Stop }
            glib::Propagation::Proceed
        });
        self.text.add_controller(keys);
    }

    pub fn is_empty(&self) -> bool { self.text.buffer().char_count() == 0 && self.attached.borrow().is_empty() }

    pub fn text(&self) -> String {
        let b = self.text.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    }

    pub fn set_text(&self, t: &str) {
        let b = self.text.buffer();
        b.set_text(t);
        b.place_cursor(&b.end_iter());
        self.text.grab_focus();
    }

    fn submit(&self, st: &Shared) {
        let t = self.text();
        let images: Vec<String> = self.attached.borrow().iter().map(|a| a.1.clone()).collect();
        if t.trim().is_empty() && images.is_empty() { return }
        self.text.buffer().set_text("");
        self.attached.borrow_mut().clear();
        self.draw_chips();
        self.hide_sugg();
        ui::prompt(st, &t, images);
    }

    /// The send button: ink when there's something to send, accent (stop) while working.
    pub fn send_state(&self, st: &Shared) {
        let busy = st.try_borrow().ok().and_then(|a| a.project().and_then(|p| p.current()).map(|t| t.busy())).unwrap_or(false);
        let has = !self.is_empty();
        self.send.set_sensitive(has || busy);
        if std::env::var_os("QH_APP_DEBUG").is_some() { eprintln!("send_state busy={busy} has={has}") }
        if busy && !has { self.send.add_css_class("busy"); self.send.set_tooltip_text(Some("Stop (Esc)")) }
        else { self.send.remove_css_class("busy"); self.send.set_tooltip_text(Some("Send (Enter)")) }
    }

    // MARK: suggestions

    fn suggest(&self, st: &Shared) {
        let text = self.text();
        let mut items = vec![];
        if let Ok(a) = st.try_borrow() {
            if let Some(p) = a.project().filter(|_| !a.team) {
                if text.starts_with('/') && !text.contains([' ', '\n']) {
                    let q = text.to_lowercase();
                    items = p.commands.iter().filter(|c| c.0.to_lowercase().starts_with(&q)).take(8)
                        .map(|c| (format!("{} ", c.0), c.0.clone(), c.1.clone())).collect();
                } else if let Some(w) = text.rsplit([' ', '\n']).next().filter(|w| w.starts_with('@')) {
                    let q = w[1..].to_lowercase();
                    let mut hits: Vec<(usize, &String)> = p.files.iter().filter_map(|f| fuzzy(&f.to_lowercase(), &q).map(|s| (s, f))).collect();
                    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
                    items = hits.into_iter().take(8).map(|(_, f)| (format!("@{f} "), f.clone(), String::new())).collect();
                }
            }
        }
        // a lone exact match is nothing to suggest
        if items.len() == 1 && text.ends_with(items[0].0.trim_end()) && items[0].0.starts_with('/') && text == items[0].0.trim_end() { items.clear() }
        *self.items.borrow_mut() = items;
        self.sel.set(0);
        self.draw_sugg();
    }

    fn draw_sugg(&self) {
        while let Some(c) = self.sugg.first_child() { self.sugg.remove(&c) }
        let items = self.items.borrow();
        self.sugg.set_visible(!items.is_empty());
        for (i, (_, title, hint)) in items.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("sugg-row");
            if i == self.sel.get() { row.add_css_class("on") }
            let t = label(title, &["mono", "ink"]);
            t.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            t.set_hexpand(true);
            row.append(&t);
            if !hint.is_empty() {
                let h = label(hint, &["faint", "small"]);
                h.set_ellipsize(gtk::pango::EllipsizeMode::End);
                h.set_max_width_chars(48);
                row.append(&h);
            }
            let click = gtk::GestureClick::new();
            let me = self.clone();
            click.connect_released(move |_, _, _, _| me.accept(i));
            row.add_controller(click);
            self.sugg.append(&row);
        }
    }

    fn hide_sugg(&self) {
        self.items.borrow_mut().clear();
        self.draw_sugg();
    }

    fn accept(&self, i: usize) {
        let Some((insert, _, _)) = self.items.borrow().get(i).cloned() else { return };
        let text = self.text();
        let new = if insert.starts_with('/') { insert } else {
            let cut = text.rfind([' ', '\n']).map(|i| i + 1).unwrap_or(0);
            format!("{}{insert}", &text[..cut])
        };
        self.set_text(&new);
        self.hide_sugg();
    }

    // MARK: images: picked or pasted, sent as base64 of the file bytes in the prompt's "images"

    fn add_image(&self, name: String, bytes: &[u8]) {
        self.attached.borrow_mut().push((name, glib::base64_encode(bytes).to_string()));
        self.draw_chips();
    }

    fn draw_chips(&self) {
        while let Some(c) = self.chips.first_child() { self.chips.remove(&c) }
        let list = self.attached.borrow().clone();
        self.chips.set_visible(!list.is_empty());
        for (i, (name, _)) in list.iter().enumerate() {
            let chip = gtk::Box::new(gtk::Orientation::Horizontal, 4);
            chip.add_css_class("chip");
            chip.append(&label(name, &["small", "soft"]));
            let x = gtk::Button::with_label("×");
            x.add_css_class("plain");
            x.set_tooltip_text(Some("Remove"));
            let me = self.clone();
            x.connect_clicked(move |_| { me.attached.borrow_mut().remove(i); me.draw_chips(); });
            chip.append(&x);
            self.chips.append(&chip);
        }
        if let Some(st) = ui::shared() { self.send_state(&st) }
    }

    fn pick_images(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        for m in ["image/png", "image/jpeg", "image/gif", "image/webp"] { filter.add_mime_type(m) }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let d = gtk::FileDialog::builder().title("Attach images").modal(true).filters(&filters).build();
        let me = self.clone();
        let win = self.root.root().and_downcast::<gtk::Window>();
        d.open_multiple(win.as_ref(), None::<&gio::Cancellable>, move |r| {
            let Ok(list) = r else { return };
            for f in list.iter::<gio::File>().flatten() {
                if let (Some(p), Some(n)) = (f.path(), f.basename()) {
                    if let Ok(b) = std::fs::read(&p) { me.add_image(n.to_string_lossy().into(), &b) }
                }
            }
        });
    }

    /// Ctrl+V with an image (and no text) on the clipboard attaches it as a PNG. True when taken.
    fn paste_image(&self) -> bool {
        let cb = self.text.clipboard();
        let f = cb.formats();
        if f.contains_type(glib::types::Type::STRING) || !f.contains_type(gdk::Texture::static_type()) { return false }
        let me = self.clone();
        cb.read_texture_async(None::<&gio::Cancellable>, move |r| {
            if let Ok(Some(t)) = r {
                let bytes = t.save_to_png_bytes();
                let name = format!("qh-paste-{}.png", glib::DateTime::now_local().map(|d| d.format("%Y%m%d-%H%M%S").map(|s| s.to_string()).unwrap_or_default()).unwrap_or_default());
                let _ = std::fs::write(std::env::temp_dir().join(&name), &bytes);
                me.add_image(name, &bytes);
            }
        });
        true
    }

    /// The model / mode / tokens line and the placeholder, from the current tab.
    pub fn render(&self, a: &model::App) {
        if a.team {
            let chan = a.project().map(|p| p.chan.clone()).unwrap_or_else(|| "#team".into());
            self.placeholder.set_text(&format!("Message {chan}"));
            for w in [self.model.upcast_ref::<gtk::Widget>(), self.mode.upcast_ref(), self.attach.upcast_ref(), self.stop.upcast_ref()] { w.set_visible(false) }
            self.tokens.set_text("");
            return;
        }
        self.attach.set_visible(true);
        let (Some(p), Some(t)) = (a.project(), a.project().and_then(|p| p.current())) else { return };
        let m = model::short_model(&t.model, &t.provider);
        self.model.set_label(&m);
        self.model.set_visible(!m.is_empty());
        self.mode.set_label(&p.mode);
        self.mode.set_visible(!p.mode.is_empty());
        let mut bits = vec![];
        if t.tokens >= 1000 { bits.push(format!("{}k tokens", t.tokens / 1000)) }
        if let Some(c) = t.cost.filter(|c| *c > 0.0) { bits.push(format!("${c:.3}")) }
        self.tokens.set_text(&bits.join("   "));
        self.stop.set_visible(t.busy());
        self.placeholder.set_text(if t.busy() { "qh is working — Esc to stop, or type to queue" } else { "Ask anything · / commands · @ files" });
    }
}
