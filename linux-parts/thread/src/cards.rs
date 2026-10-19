//! The permission card above the composer, and the plan checklist.

use crate::label;
use gtk::prelude::*;

use std::rc::Rc;

/// What the ask card is asking about.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    /// "bash", "edit", ...: shown as "qh wants to <tool>".
    pub tool: String,
    /// The exact command or diff.
    pub detail: String,
    /// A warning shown in accent (empty for none).
    pub warn: String,
    /// The rule "Always for" would add (empty: no Always button).
    pub always: String,
    /// Crosses a safety line: accent border, no Always.
    pub guarded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Once,
    Always,
    Deny,
}

pub struct AskCard {
    root: gtk::Box,
}

fn button(title: &str, key: &str, kind: &str) -> gtk::Button {
    let b = gtk::Button::new();
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    h.append(&gtk::Label::new(Some(title)));
    if !key.is_empty() {
        let k = label(key, &["qh-key"]);
        k.set_wrap(false);
        h.append(&k);
    }
    b.set_child(Some(&h));
    b.add_css_class("qh-btn");
    b.add_css_class(kind);
    b.set_cursor_from_name(Some("pointer"));
    b
}

fn shortcut(root: &gtk::Box, accel: &str, b: &gtk::Button) {
    let c = gtk::ShortcutController::new();
    c.set_scope(gtk::ShortcutScope::Global);
    let b = b.clone();
    let action = gtk::CallbackAction::new(move |_, _| {
        b.emit_clicked();
        gtk::glib::Propagation::Stop
    });
    c.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(action)));
    root.add_controller(c);
}

impl AskCard {
    /// A permission: Allow once (Ctrl+Enter) / Always for <rule> / Deny (Ctrl+Backspace).
    pub fn new(ask: &Ask, on_answer: impl Fn(Answer) + 'static) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.add_css_class("qh-ask");
        if ask.guarded {
            root.add_css_class("guarded");
        }
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        head.append(&label(&format!("qh wants to {}", ask.tool), &["qh-title"]));
        if ask.guarded {
            head.append(&label("crosses a safety line", &["qh-mono", "qh-accent"]));
        }
        root.append(&head);
        let d = label(&ask.detail, &["qh-detail"]);
        d.set_selectable(true);
        d.set_focusable(false);
        d.set_yalign(0.0);
        // short details sit inline; long ones scroll inside 220px
        if ask.detail.lines().count() > 10 || ask.detail.len() > 900 {
            let sw = gtk::ScrolledWindow::new();
            sw.set_hscrollbar_policy(gtk::PolicyType::Never);
            sw.set_min_content_height(220);
            sw.set_max_content_height(220);
            sw.set_child(Some(&d));
            root.append(&sw);
        } else {
            root.append(&d);
        }
        if !ask.warn.is_empty() {
            root.append(&label(&ask.warn, &["qh-warn"]));
        }
        let on: Rc<dyn Fn(Answer)> = Rc::new(on_answer);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let once = button("Allow once", "Ctrl+⏎", "qh-btn-ink");
        let f = on.clone();
        once.connect_clicked(move |_| f(Answer::Once));
        row.append(&once);
        shortcut(&root, "<Control>Return", &once);
        if !ask.always.is_empty() && !ask.guarded {
            let b = button(&format!("Always for {}", ask.always), "", "qh-btn-card");
            let f = on.clone();
            b.connect_clicked(move |_| f(Answer::Always));
            row.append(&b);
        }
        let deny = button("Deny", "Ctrl+⌫", "qh-btn-plain");
        let f = on.clone();
        deny.connect_clicked(move |_| f(Answer::Deny));
        row.append(&deny);
        shortcut(&root, "<Control>BackSpace", &deny);
        root.append(&row);
        AskCard { root }
    }

    /// The spend cap: "Budget reached", $spent of $budget, Keep spending / Stop here.
    /// `on_answer(true)` means keep spending.
    pub fn budget(spent: f64, budget: f64, on_answer: impl Fn(bool) + 'static) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.add_css_class("qh-ask");
        root.add_css_class("guarded");
        root.append(&label("Budget reached", &["qh-title"]));
        let l = label("", &["qh-big"]);
        l.set_markup(&format!(
            "${spent:.2} <span size=\"70%\" alpha=\"60%\">of ${budget:.2}</span>"
        ));
        root.append(&l);
        root.append(&label("qh paused before spending more on this task.", &["qh-soft"]));
        let on: Rc<dyn Fn(bool)> = Rc::new(on_answer);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let keep = button("Keep spending", "Ctrl+⏎", "qh-btn-ink");
        let f = on.clone();
        keep.connect_clicked(move |_| f(true));
        row.append(&keep);
        shortcut(&root, "<Control>Return", &keep);
        let stop = button("Stop here", "Ctrl+⌫", "qh-btn-plain");
        let f = on.clone();
        stop.connect_clicked(move |_| f(false));
        row.append(&stop);
        shortcut(&root, "<Control>BackSpace", &stop);
        root.append(&row);
        AskCard { root }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Pending,
    Active,
    Done,
}

/// A plan checklist: ○ pending, ◐ active, ✓ done (struck). Collapsible; optional Approve.
#[derive(Clone)]
pub struct PlanCard {
    root: gtk::Box,
    list: gtk::Box,
    count: gtk::Label,
}

impl PlanCard {
    pub fn new(title: &str, steps: &[(String, Step)]) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.add_css_class("qh-plan");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let t = label(title, &["qh-title"]);
        t.set_hexpand(true);
        head.append(&t);
        let count = label("", &["qh-mono", "qh-faint"]);
        count.set_wrap(false);
        head.append(&count);
        let chev = label("▾", &["qh-faint"]);
        head.append(&chev);
        let btn = gtk::Button::new();
        btn.add_css_class("qh-tool-head");
        btn.set_child(Some(&head));
        root.append(&btn);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let rev = gtk::Revealer::new();
        rev.set_reveal_child(true);
        rev.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        rev.set_child(Some(&list));
        root.append(&rev);
        btn.connect_clicked(move |_| {
            let open = !rev.reveals_child();
            rev.set_reveal_child(open);
            chev.set_text(if open { "▾" } else { "▸" });
        });
        let p = PlanCard { root, list, count };
        p.set_steps(steps);
        p
    }

    pub fn set_steps(&self, steps: &[(String, Step)]) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        for (text, s) in steps {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let (m, cls) = match s {
                Step::Pending => ("○", "qh-faint"),
                Step::Active => ("◐", "qh-accent"),
                Step::Done => ("✓", "qh-faint"),
            };
            let mk = label(m, &["qh-mark", cls]);
            mk.set_wrap(false);
            mk.set_yalign(0.0);
            row.append(&mk);
            let l = label(text, &["qh-step"]);
            if *s == Step::Done {
                l.add_css_class("done");
            }
            l.set_hexpand(true);
            row.append(&l);
            self.list.append(&row);
        }
        let done = steps.iter().filter(|s| s.1 == Step::Done).count();
        self.count.set_text(&format!("{done}/{}", steps.len()));
    }

    /// Add Approve plus an edits entry; `on_approve` gets the entry text ("" for none).
    pub fn with_approve(self, on_approve: impl Fn(String) + 'static) -> Self {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.set_margin_top(4);
        let e = gtk::Entry::new();
        e.set_placeholder_text(Some("Edits to the plan (optional)"));
        e.set_hexpand(true);
        let b = button("Approve", "", "qh-btn-ink");
        let on = Rc::new(on_approve);
        let (ee, f) = (e.clone(), on.clone());
        b.connect_clicked(move |_| f(ee.text().to_string()));
        let f = on.clone();
        e.connect_activate(move |e| f(e.text().to_string()));
        row.append(&e);
        row.append(&b);
        self.root.append(&row);
        self
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }
}
