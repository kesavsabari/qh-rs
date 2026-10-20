//! The scrolling conversation: user bubbles, streamed replies, quiet tool lines, file cards.

use crate::markdown::{render_with, settled, Links};
use crate::{appear, label};
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

struct Inner {
    scroller: gtk::ScrolledWindow,
    items: gtk::Box,
    stuck: Cell<bool>,
    links: RefCell<Links>,
    on_review: RefCell<Option<Rc<dyn Fn(&str)>>>,
    /// While `prepend` runs: push() inserts after this (None: at the very top) instead of appending.
    before: RefCell<Option<Option<gtk::Widget>>>,
    /// Distance from the content's end to the view's top, held while prepended rows lay out.
    hold: Cell<Option<f64>>,
    on_older: RefCell<Option<Rc<dyn Fn()>>>,
    asking: Cell<bool>,
}

/// The thread. `widget()` is a ScrolledWindow; keep the handle (it is cheap to clone).
#[derive(Clone)]
pub struct Thread(Rc<Inner>);

impl Default for Thread {
    fn default() -> Self {
        Self::new()
    }
}

impl Thread {
    pub fn new() -> Self {
        let items = gtk::Box::new(gtk::Orientation::Vertical, 14);
        items.add_css_class("qh-thread-items");
        items.set_valign(gtk::Align::Start);
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(760);
        clamp.set_tightening_threshold(600);
        clamp.set_child(Some(&items));
        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("qh-thread");
        scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&clamp));
        let t = Thread(Rc::new(Inner {
            scroller,
            items,
            stuck: Cell::new(true),
            links: RefCell::new(Links::default()),
            on_review: RefCell::new(None),
            before: RefCell::new(None),
            hold: Cell::new(None),
            on_older: RefCell::new(None),
            asking: Cell::new(false),
        }));
        // stick to the bottom while the user is there; scrolling up frees it
        let adj = t.0.scroller.vadjustment();
        let w = Rc::downgrade(&t.0);
        adj.connect_value_changed(move |a| {
            if let Some(i) = w.upgrade() {
                i.stuck.set(a.value() + a.page_size() >= a.upper() - 24.0);
                older(&i, a);
            }
        });
        let w = Rc::downgrade(&t.0);
        adj.connect_notify_local(Some("upper"), move |a, _| {
            if let Some(i) = w.upgrade() {
                if let Some(d) = i.hold.get() {
                    a.set_value(a.upper() - d); // older rows grew the top: keep the view where it was
                } else if i.stuck.get() {
                    a.set_value(a.upper() - a.page_size());
                }
                older(&i, a); // a short page can't be scrolled: fill until it can
            }
        });
        t
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.0.scroller
    }

    /// File links for replies, tool args and diff paths.
    pub fn set_links(&self, links: Links) {
        *self.0.links.borrow_mut() = links;
    }

    /// Called with the path when a diff card's Review is pressed (no button without it).
    pub fn set_on_review(&self, f: impl Fn(&str) + 'static) {
        *self.0.on_review.borrow_mut() = Some(Rc::new(f));
    }

    /// Called (from idle) when the view nears the top; it adds older rows with `prepend`.
    pub fn set_on_older(&self, f: impl Fn() + 'static) {
        *self.0.on_older.borrow_mut() = Some(Rc::new(f));
    }

    /// Every push_* inside `f` lands above what's shown, in order, with no entrance and no jump.
    pub fn prepend(&self, f: impl FnOnce()) {
        let a = self.0.scroller.vadjustment();
        self.0.hold.set(Some(a.upper() - a.value()));
        *self.0.before.borrow_mut() = Some(None);
        f();
        *self.0.before.borrow_mut() = None;
        // layout settles within a frame or two; then scrolling is the user's again
        let w = Rc::downgrade(&self.0);
        glib::timeout_add_local_once(Duration::from_millis(250), move || {
            if let Some(i) = w.upgrade() { i.hold.set(None) }
        });
    }

    pub fn is_at_bottom(&self) -> bool {
        self.0.stuck.get()
    }

    pub fn scroll_to_bottom(&self) {
        self.0.stuck.set(true);
        let a = self.0.scroller.vadjustment();
        a.set_value(a.upper() - a.page_size());
    }

    fn push(&self, w: &impl IsA<gtk::Widget>) {
        let mut before = self.0.before.borrow_mut();
        if let Some(after) = before.as_mut() {
            self.0.items.insert_child_after(w, after.as_ref());
            *after = Some(w.clone().upcast());
            return;
        }
        drop(before);
        self.0.items.append(w);
        appear(w);
    }

    pub fn push_user(&self, text: &str) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.set_margin_top(18);
        let l = label(text, &["qh-user"]);
        l.set_selectable(true);
        l.set_focusable(false);
        l.set_halign(gtk::Align::End);
        l.set_hexpand(true);
        l.set_margin_start(80);
        l.set_max_width_chars(60);
        row.append(&l);
        let older = self.0.before.borrow().is_some();
        self.push(&row);
        if !older { self.scroll_to_bottom() }
    }

    pub fn push_reply(&self) -> ReplyHandle {
        // the same gap render_with puts between blocks, for the finished-block pieces
        let slot = gtk::Box::new(gtk::Orientation::Vertical, 10);
        self.push(&slot);
        ReplyHandle(Rc::new(Reply {
            slot,
            text: RefCell::new(String::new()),
            done: Cell::new(0),
            pending: Cell::new(false),
            links: self.0.links.borrow().clone(),
        }))
    }

    pub fn push_tool(&self, name: &str, args: &str) -> ToolHandle {
        let t = ToolHandle::new(name, args, &self.0.links.borrow());
        self.push(&t.root);
        t
    }

    pub fn push_diff(&self, path: &str, added: u32, removed: u32) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("qh-diff");
        let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
        icon.add_css_class("qh-soft");
        row.append(&icon);
        let links = self.0.links.borrow().clone();
        let p = label("", &["qh-path"]);
        p.set_wrap(false);
        p.set_ellipsize(pango::EllipsizeMode::Middle);
        p.set_hexpand(true);
        p.set_markup(&crate::markdown::inline(&format!("`{path}`"), &links).replace("bgcolor=\"#8080801F\"", ""));
        let on_open = links.on_open.clone();
        p.connect_activate_link(move |_, uri| {
            if let (Some(f), Some(p)) = (&on_open, uri.strip_prefix("qhfile:")) {
                f(p);
            }
            glib::Propagation::Stop
        });
        row.append(&p);
        let n = label("", &["qh-mono", "qh-soft"]);
        n.set_markup(&format!("<span>+{added}</span> <span>−{removed}</span>"));
        row.append(&n);
        if let Some(f) = self.0.on_review.borrow().clone() {
            let b = gtk::Button::with_label("Review");
            b.add_css_class("qh-btn");
            b.add_css_class("qh-btn-ink");
            let path = path.to_string();
            b.connect_clicked(move |_| f(&path));
            row.append(&b);
        }
        self.push(&row);
    }

    pub fn push_notice(&self, text: &str) {
        let l = label(text, &["qh-notice"]);
        l.set_selectable(true);
        l.set_focusable(false);
        self.push(&l);
    }

    pub fn push_error(&self, text: &str) {
        let l = label(text, &["qh-error"]);
        l.set_selectable(true);
        l.set_focusable(false);
        self.push(&l);
    }

    /// A check run after edits: "✓ cmd · 2.1s", and when it failed the tail plus a Fix button.
    pub fn push_verify(&self, cmd: &str, ok: bool, secs: f64, tail: &str, on_fix: Option<Box<dyn Fn()>>) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        head.append(&label(if ok { "✓" } else { "✕" }, &["qh-mono", if ok { "qh-green" } else { "qh-red" }]));
        let c = label(cmd, &["qh-mono", "qh-soft"]);
        c.set_wrap(false);
        c.set_ellipsize(pango::EllipsizeMode::Middle);
        head.append(&c);
        head.append(&label(&format!("· {secs:.1}s"), &["qh-mono", "qh-faint"]));
        col.append(&head);
        if !ok && !tail.is_empty() {
            col.append(&out_box(tail));
        }
        if !ok {
            if let Some(f) = on_fix {
                let b = gtk::Button::with_label("Fix it");
                b.add_css_class("qh-btn");
                b.add_css_class("qh-btn-ink");
                b.set_halign(gtk::Align::Start);
                b.connect_clicked(move |_| f());
                col.append(&b);
            }
        }
        self.push(&col);
    }

    /// Any custom widget (an AskCard or PlanCard inline, say), with the same entrance.
    pub fn push_widget(&self, w: &impl IsA<gtk::Widget>) {
        self.push(w);
    }

    pub fn clear(&self) {
        while let Some(c) = self.0.items.first_child() {
            self.0.items.remove(&c);
        }
        self.0.stuck.set(true);
    }
}

/// Near the top: ask once for the next older page (from idle, never inside a scroll signal).
fn older(i: &Rc<Inner>, a: &gtk::Adjustment) {
    if a.value() >= 400.0 || i.hold.get().is_some() || i.asking.replace(true) { return }
    let Some(f) = i.on_older.borrow().clone() else { i.asking.set(false); return };
    let w = Rc::downgrade(i);
    glib::idle_add_local_once(move || {
        f();
        if let Some(i) = w.upgrade() { i.asking.set(false) }
    });
}

struct Reply {
    slot: gtk::Box,
    text: RefCell<String>,
    /// Bytes of `text` already drawn as finished blocks; only the rest is redrawn.
    done: Cell<usize>,
    pending: Cell<bool>,
    links: Links,
}

/// A streaming reply. `append` is cheap; the markdown is rebuilt at most every 60 ms.
#[derive(Clone)]
pub struct ReplyHandle(Rc<Reply>);

impl ReplyHandle {
    pub fn append(&self, delta: &str) {
        self.0.text.borrow_mut().push_str(delta);
        if !self.0.pending.replace(true) {
            let r = self.0.clone();
            glib::timeout_add_local_once(Duration::from_millis(60), move || redraw(&r));
        }
    }
    /// Replace the whole text (redrawn now).
    pub fn set_text(&self, text: &str) {
        *self.0.text.borrow_mut() = text.to_string();
        while let Some(c) = self.0.slot.first_child() { self.0.slot.remove(&c) }
        self.0.done.set(0);
        redraw(&self.0);
    }
    /// Draw whatever is buffered now (call at end of stream).
    pub fn flush(&self) {
        redraw(&self.0);
    }
    pub fn text(&self) -> String {
        self.0.text.borrow().clone()
    }
}

/// Finished blocks are drawn once and kept; only the open tail is rebuilt, so a long
/// reply streams at the same cost per redraw as a short one.
fn redraw(r: &Reply) {
    r.pending.set(false);
    let text = r.text.borrow();
    let rest = &text[r.done.get()..];
    if let Some(c) = r.slot.last_child() {
        r.slot.remove(&c); // the old tail (or nothing yet)
    }
    let cut = settled(rest);
    if cut > 0 {
        r.slot.append(&render_with(&rest[..cut], &r.links));
        r.done.set(r.done.get() + cut);
    }
    let tail = &text[r.done.get()..];
    let w = render_with(tail, &r.links);
    w.set_visible(!tail.trim().is_empty()); // no stray gap under the last finished block
    r.slot.append(&w);
}

fn out_box(text: &str) -> gtk::ScrolledWindow {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_yalign(0.0);
    l.set_selectable(true);
    l.set_focusable(false);
    let sw = gtk::ScrolledWindow::new();
    sw.add_css_class("qh-out");
    // ~17px a line of 11.5px mono plus padding, capped: then it scrolls
    let lines = text.lines().count().max(1) as i32;
    sw.set_min_content_height((lines * 17 + 18).min(280));
    sw.set_max_content_height(280);
    sw.set_child(Some(&l));
    sw
}

/// One quiet tool line: ◐ while running, ✓/✕ when done; click to see the output.
#[derive(Clone)]
pub struct ToolHandle {
    root: gtk::Box,
    mark: gtk::Label,
    chevron: gtk::Label,
    body: gtk::Box,
    revealer: gtk::Revealer,
}

impl ToolHandle {
    fn new(name: &str, args: &str, links: &Links) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let mark = label("◐", &["qh-accent"]);
        mark.set_wrap(false);
        head.append(&mark);
        let n = label(name, &["qh-soft"]);
        n.set_wrap(false);
        head.append(&n);
        let a = label("", &["qh-faint"]);
        a.set_wrap(false);
        a.set_ellipsize(pango::EllipsizeMode::Middle);
        a.set_hexpand(true);
        if links.file(args) {
            a.set_markup(&format!("<a href=\"qhfile:{0}\">{0}</a>", glib::markup_escape_text(args)));
            let on_open = links.on_open.clone();
            a.connect_activate_link(move |_, uri| {
                if let (Some(f), Some(p)) = (&on_open, uri.strip_prefix("qhfile:")) {
                    f(p);
                }
                glib::Propagation::Stop
            });
        } else {
            a.set_text(args);
        }
        head.append(&a);
        let chevron = label("", &["qh-faint"]);
        head.append(&chevron);
        let btn = gtk::Button::new();
        btn.add_css_class("qh-tool-head");
        btn.set_child(Some(&head));
        btn.set_cursor_from_name(Some("pointer"));
        root.append(&btn);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        revealer.set_transition_duration(200);
        revealer.set_child(Some(&body));
        root.append(&revealer);
        let t = ToolHandle { root, mark, chevron, body, revealer };
        let tt = t.clone();
        btn.connect_clicked(move |_| {
            if tt.body.first_child().is_some() {
                let open = !tt.revealer.reveals_child();
                tt.revealer.set_reveal_child(open);
                tt.chevron.set_text(if open { "▾" } else { "▸" });
            }
        });
        t
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn finish(&self, ok: bool, summary: &str, output: &str) {
        self.mark.set_text(if ok { "✓" } else { "✕" });
        for c in ["qh-accent", "qh-faint", "qh-red"] {
            self.mark.remove_css_class(c);
        }
        self.mark.add_css_class(if ok { "qh-faint" } else { "qh-red" });
        while let Some(c) = self.body.first_child() {
            self.body.remove(&c);
        }
        if !summary.is_empty() {
            let s = label(summary, &["qh-mono", "qh-soft"]);
            s.set_margin_start(20);
            s.set_selectable(true);
            s.set_focusable(false);
            self.body.append(&s);
        }
        if !output.is_empty() {
            self.body.append(&out_box(output));
        }
        self.chevron.set_text(if self.body.first_child().is_some() { "▸" } else { "" });
    }

    /// Open or close the output.
    pub fn set_expanded(&self, open: bool) {
        self.revealer.set_reveal_child(open);
        self.chevron.set_text(if open { "▾" } else if self.body.first_child().is_some() { "▸" } else { "" });
    }
}
