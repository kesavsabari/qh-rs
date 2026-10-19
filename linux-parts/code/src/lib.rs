//! qh's code widgets for the Linux app: review (keep / undo per file), checkpoints with rewind,
//! a lazy file tree, a read-only code view with gutter selection + inline edit, and the
//! Chat | Split | Code switch. Same behaviour and look as apple/qh/ReviewView.swift,
//! Checkpoints.swift, CodeView.swift and windows/CodeView.cs. Load [`CSS`] once per display.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

type Cb<A> = Rc<RefCell<Option<Box<dyn Fn(A)>>>>;
type Cb0 = Rc<RefCell<Option<Box<dyn Fn()>>>>;

fn fire<A>(cb: &Cb<A>, a: A) {
    if let Some(f) = cb.borrow().as_ref() {
        f(a)
    }
}

/// Zen colours from apple/qh/Theme.swift, light then dark (GTK 4.20+ `prefers-color-scheme`).
pub const CSS: &str = r#"
.qh-code, .qh-switch { --qh-paper:#FBFBF9; --qh-side:#F0EFEA; --qh-ink:#0E0E0D; --qh-soft:#6B6B66; --qh-faint:#9A9A94;
  --qh-rule:#DCDCD5; --qh-hover:#E6E6E0; --qh-accent:#FF4D1C; --qh-card:#FBFBF9; --qh-tile:#F0EFEA; --qh-line:#E3E2DC;
  --qh-add:#E6F4E4; --qh-del:#FBE7E3; --qh-pick:rgba(255,77,28,0.13);
  --qh-k:#B3360F; --qh-s:#2F7D3A; --qh-c:#9A9A94; --qh-n:#373FF5; }
@media (prefers-color-scheme: dark) {
.qh-code, .qh-switch { --qh-paper:#1D1D1B; --qh-side:#262624; --qh-ink:#ECEBE6; --qh-soft:#9C9B94; --qh-faint:#85847D;
  --qh-rule:#34342F; --qh-hover:#2E2E2B; --qh-accent:#FF5A2A; --qh-card:#1D1D1B; --qh-tile:#262624; --qh-line:#31312E;
  --qh-add:#1E3320; --qh-del:#3A1E1B; --qh-pick:rgba(255,90,42,0.20);
  --qh-k:#FF8A5C; --qh-s:#8FD18A; --qh-c:#6F6E68; --qh-n:#9AA0FF; } }
.qh-code { background: var(--qh-card); color: var(--qh-ink); font-family: "Geist", sans-serif; font-size: 13px; }
.qh-code label { color: var(--qh-ink); }
.qh-code .qh-soft { color: var(--qh-soft); }
.qh-code .qh-faint { color: var(--qh-faint); }
.qh-code .qh-mono { font-family: "Geist Mono", monospace; font-size: 12px; }
.qh-code .qh-small { font-size: 11px; }
.qh-code .qh-title { font-weight: 500; font-size: 17px; }
.qh-code .qh-medium { font-weight: 500; }
.qh-code scrolledwindow, .qh-code viewport, .qh-code listview, .qh-code list { background: transparent; }
.qh-code button.qh-btn { min-height: 26px; padding: 0 11px; border-radius: 7px; font-size: 12px; font-weight: 500; box-shadow: none;
  background: transparent; color: var(--qh-soft); border: 1px solid var(--qh-rule); }
.qh-code button.qh-btn label { color: inherit; }
.qh-code button.qh-btn:hover { background: var(--qh-hover); }
.qh-code button.qh-btn.qh-card-btn { background: var(--qh-card); color: var(--qh-ink); }
.qh-code button.qh-btn.qh-card-btn:hover { background: var(--qh-hover); }
.qh-code button.qh-btn.qh-ink-btn { background: var(--qh-ink); color: var(--qh-card); border-color: var(--qh-ink); }
.qh-code button.qh-btn.qh-ink-btn:hover { opacity: 0.88; }
.qh-code button.qh-btn:disabled { opacity: 0.45; }
.qh-code .qh-files { background: var(--qh-side); border-right: 1px solid var(--qh-line); }
.qh-code .qh-file-row { border-radius: 7px; padding: 5px 8px; }
.qh-code .qh-file-row:hover { background: var(--qh-hover); }
.qh-code .qh-file-row.qh-on { background: var(--qh-card); }
.qh-code .qh-file-row label { color: var(--qh-soft); }
.qh-code .qh-file-row.qh-on label.qh-path { color: var(--qh-ink); }
.qh-code .qh-file-row label.qh-count { color: var(--qh-faint); }
.qh-code .qh-dl { padding: 0 16px; font-family: "Geist Mono", monospace; font-size: 12px; min-height: 19px; }
.qh-code .qh-dl label { color: var(--qh-soft); }
.qh-code .qh-dl.qh-add { background: var(--qh-add); }
.qh-code .qh-dl.qh-del { background: var(--qh-del); }
.qh-code .qh-dl.qh-add label, .qh-code .qh-dl.qh-del label { color: var(--qh-ink); }
.qh-code .qh-gap { color: var(--qh-faint); padding: 4px 16px; font-family: "Geist Mono", monospace; font-size: 12px; }
.qh-code .qh-tilecard { background: var(--qh-tile); border-radius: 10px; padding: 12px 14px; }
.qh-code .qh-box { background: var(--qh-tile); border-radius: 8px; }
.qh-code .qh-gutter { color: var(--qh-faint); }
.qh-code .qh-hl { background: var(--qh-tile); }
.qh-code .qh-pick { background: var(--qh-pick); }
.qh-code .qh-bar { border-top: 1px solid var(--qh-line); padding: 10px 16px 12px 16px; }
.qh-code .qh-head { border-bottom: 1px solid var(--qh-line); }
.qh-code entry, .qh-code spinbutton { background: var(--qh-tile); color: var(--qh-ink); border-radius: 7px; box-shadow: none;
  border: 1px solid var(--qh-rule); min-height: 28px; font-size: 13px; }
.qh-code entry:focus-within, .qh-code spinbutton:focus-within { border-color: var(--qh-accent); outline: none; }
.qh-code .qh-err { color: var(--qh-accent); }
.qh-code .qh-tree row { border-radius: 6px; padding: 0 4px; min-height: 24px; }
.qh-code .qh-tree row:hover { background: var(--qh-hover); }
.qh-code .qh-tree row:selected { background: var(--qh-card); }
.qh-code .qh-tree label { color: var(--qh-soft); font-size: 12.5px; }
.qh-code .qh-tree .qh-dir label { color: var(--qh-ink); }
.qh-code .qh-tree { background: var(--qh-side); }
.qh-switch { background: var(--qh-tile); border-radius: 8px; padding: 2px; }
.qh-switch button { min-height: 22px; padding: 0 12px; border-radius: 6px; background: transparent; box-shadow: none;
  border: none; color: var(--qh-soft); font-size: 12px; font-weight: 500; font-family: "Geist", sans-serif; }
.qh-switch button:hover { color: var(--qh-ink); }
.qh-switch button:checked { background: var(--qh-card); color: var(--qh-ink); box-shadow: 0 1px 2px rgba(0,0,0,0.08); }
"#;

/// Install [`CSS`] on the default display (call after GTK init).
pub fn load_css() {
    let p = gtk::CssProvider::new();
    p.load_from_string(CSS);
    // app providers don't follow the colour scheme on their own (GTK 4.20+ property)
    if let Some(s) = gtk::Settings::default() {
        if s.find_property("gtk-interface-color-scheme").is_some() && p.find_property("prefers-color-scheme").is_some() {
            s.bind_property("gtk-interface-color-scheme", &p, "prefers-color-scheme").sync_create().build();
        }
    }
    if let Some(d) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&d, &p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

#[link(name = "fontconfig")]
extern "C" {
    fn FcConfigAppFontAddFile(config: *mut std::ffi::c_void, file: *const std::ffi::c_char) -> i32;
}

/// Register the Geist .ttf files in `dir` for this process (call before GTK init). Returns how many loaded.
pub fn register_fonts(dir: &std::path::Path) -> usize {
    let mut n = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "ttf") {
            if let Ok(c) = std::ffi::CString::new(p.to_string_lossy().as_bytes()) {
                if unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), c.as_ptr()) } != 0 {
                    n += 1
                }
            }
        }
    }
    n
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    for c in classes {
        l.add_css_class(c)
    }
    l
}

fn button(text: &str, kind: &str) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    b.add_css_class("qh-btn");
    if !kind.is_empty() {
        b.add_css_class(kind)
    }
    b.set_valign(gtk::Align::Center);
    b
}

fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c)
    }
}

fn empty(title: &str, hint: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    b.set_valign(gtk::Align::Center);
    b.set_halign(gtk::Align::Center);
    b.set_vexpand(true);
    b.set_hexpand(true);
    let t = label(title, &["qh-medium"]);
    t.set_halign(gtk::Align::Center);
    let h = label(hint, &["qh-soft"]);
    h.set_halign(gtk::Align::Center);
    h.set_wrap(true);
    h.set_justify(gtk::Justification::Center);
    b.append(&t);
    b.append(&h);
    b
}

// MARK: diff

/// One diff line: `' '` same, `'+'` added, `'-'` removed.
pub type DiffLine = (char, String);

fn split(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return vec![];
    }
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// Line diff by longest common subsequence; big files fall back to a whole-file replace.
pub fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let (a, b) = (split(before), split(after));
    let mut out = Vec::new();
    if a.len() as u64 * b.len() as u64 > 4_000_000 {
        out.extend(a.iter().map(|l| ('-', l.to_string())));
        out.extend(b.iter().map(|l| ('+', l.to_string())));
        return out;
    }
    let w = b.len() + 1;
    let mut lcs = vec![0u32; (a.len() + 1) * w];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * w + j] = if a[i] == b[j] { lcs[(i + 1) * w + j + 1] + 1 } else { lcs[(i + 1) * w + j].max(lcs[i * w + j + 1]) };
        }
    }
    let (mut x, mut y) = (0, 0);
    while x < a.len() && y < b.len() {
        if a[x] == b[y] {
            out.push((' ', a[x].to_string()));
            x += 1;
            y += 1;
        } else if lcs[(x + 1) * w + y] >= lcs[x * w + y + 1] {
            out.push(('-', a[x].to_string()));
            x += 1;
        } else {
            out.push(('+', b[y].to_string()));
            y += 1;
        }
    }
    out.extend(a[x..].iter().map(|l| ('-', l.to_string())));
    out.extend(b[y..].iter().map(|l| ('+', l.to_string())));
    out
}

/// (added, removed) line counts.
pub fn diff_count(before: &str, after: &str) -> (usize, usize) {
    let d = diff_lines(before, after);
    (d.iter().filter(|l| l.0 == '+').count(), d.iter().filter(|l| l.0 == '-').count())
}

/// Unified diff widget, three lines of context, ⋯ for gaps.
fn diff_view(before: &str, after: &str) -> gtk::Box {
    let lines = diff_lines(before, after);
    let mut keep = HashSet::new();
    for (i, l) in lines.iter().enumerate() {
        if l.0 != ' ' {
            for k in i.saturating_sub(3)..=(i + 3).min(lines.len().saturating_sub(1)) {
                keep.insert(k);
            }
        }
    }
    let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
    col.set_margin_bottom(16);
    let mut gap = false;
    for (i, (k, text)) in lines.iter().enumerate() {
        if !keep.contains(&i) {
            if !gap {
                col.append(&label("  ⋯", &["qh-gap"]));
            }
            gap = true;
            continue;
        }
        gap = false;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        row.add_css_class("qh-dl");
        match k {
            '+' => row.add_css_class("qh-add"),
            '-' => row.add_css_class("qh-del"),
            _ => {}
        }
        let sign = if *k == '-' { '−' } else { *k };
        row.append(&label(&format!("{sign} {text}"), &[]));
        col.append(&row);
    }
    col
}

// MARK: review

struct ReviewState {
    changes: Vec<(String, String, String)>,
    pick: Option<String>,
}

/// Changed files on the left (keep all / undo all), the selected file's diff on the right.
#[derive(Clone)]
pub struct Review {
    root: gtk::Box,
    st: Rc<RefCell<ReviewState>>,
    accept: Cb<String>,
    reject: Cb<String>,
}

impl Default for Review {
    fn default() -> Self {
        Self::new()
    }
}

impl Review {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("qh-code");
        let r = Review {
            root,
            st: Rc::new(RefCell::new(ReviewState { changes: vec![], pick: None })),
            accept: Rc::default(),
            reject: Rc::default(),
        };
        r.render();
        r
    }
    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
    /// (path, before, after) per changed file.
    pub fn set_changes(&self, changes: Vec<(String, String, String)>) {
        self.st.borrow_mut().changes = changes;
        self.render();
    }
    pub fn on_accept(&self, f: impl Fn(String) + 'static) {
        *self.accept.borrow_mut() = Some(Box::new(f));
    }
    pub fn on_reject(&self, f: impl Fn(String) + 'static) {
        *self.reject.borrow_mut() = Some(Box::new(f));
    }

    fn render(&self) {
        clear(&self.root);
        let st = self.st.borrow();
        if st.changes.is_empty() {
            self.root.append(&empty("Nothing changed yet.", "Edits qh makes show up here to keep or undo."));
            return;
        }
        let cur = st.changes.iter().find(|c| Some(&c.0) == st.pick.as_ref()).unwrap_or(&st.changes[0]).clone();

        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        list.set_margin_start(12);
        list.set_margin_end(8);
        list.set_margin_top(16);
        list.set_margin_bottom(16);
        let all = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        all.set_margin_bottom(10);
        let paths: Vec<String> = st.changes.iter().map(|c| c.0.clone()).collect();
        let ka = button("keep all", "qh-card-btn");
        let (cb, ps) = (self.accept.clone(), paths.clone());
        ka.connect_clicked(move |_| ps.iter().for_each(|p| fire(&cb, p.clone())));
        let ua = button("undo all", "");
        let (cb, ps) = (self.reject.clone(), paths);
        ua.connect_clicked(move |_| ps.iter().for_each(|p| fire(&cb, p.clone())));
        all.append(&ka);
        all.append(&ua);
        list.append(&all);
        for (path, before, after) in &st.changes {
            let (a, r) = diff_count(before, after);
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("qh-file-row");
            if *path == cur.0 {
                row.add_css_class("qh-on")
            }
            let p = label(path, &["qh-mono", "qh-path"]);
            p.set_hexpand(true);
            p.set_ellipsize(pango::EllipsizeMode::Start);
            p.set_tooltip_text(Some(path));
            row.append(&p);
            row.append(&label(&format!("+{a} −{r}"), &["qh-mono", "qh-small", "qh-count"]));
            let g = gtk::GestureClick::new();
            let (me, path) = (self.clone(), path.clone());
            g.connect_released(move |_, _, _, _| {
                me.st.borrow_mut().pick = Some(path.clone());
                me.render();
            });
            row.add_controller(g);
            list.append(&row);
        }
        let ls = gtk::ScrolledWindow::new();
        ls.set_hscrollbar_policy(gtk::PolicyType::Never);
        ls.set_child(Some(&list));
        ls.set_size_request(240, -1);
        ls.add_css_class("qh-files");
        ls.set_hexpand(false);
        self.root.append(&ls);

        let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
        right.set_hexpand(true);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.set_margin_start(16);
        head.set_margin_end(16);
        head.set_margin_top(14);
        head.set_margin_bottom(10);
        let t = label(&cur.0, &["qh-mono"]);
        t.set_hexpand(true);
        t.set_ellipsize(pango::EllipsizeMode::Start);
        head.append(&t);
        let undo = button("undo file", "");
        let (cb, p) = (self.reject.clone(), cur.0.clone());
        undo.connect_clicked(move |_| fire(&cb, p.clone()));
        let keep = button("keep", "qh-ink-btn");
        let (cb, p) = (self.accept.clone(), cur.0.clone());
        keep.connect_clicked(move |_| fire(&cb, p.clone()));
        head.append(&undo);
        head.append(&keep);
        right.append(&head);
        let sw = gtk::ScrolledWindow::new();
        sw.set_vexpand(true);
        sw.set_child(Some(&diff_view(&cur.1, &cur.2)));
        right.append(&sw);
        self.root.append(&right);
    }
}

// MARK: checkpoints

/// The checkpoint list, newest first, with rewind buttons.
#[derive(Clone)]
pub struct Checkpoints {
    root: gtk::Box,
    st: Rc<RefCell<(Vec<(usize, String, Vec<String>)>, bool)>>,
    rewind: Cb<(usize, bool)>,
}

impl Default for Checkpoints {
    fn default() -> Self {
        Self::new()
    }
}

impl Checkpoints {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("qh-code");
        let c = Checkpoints { root, st: Rc::default(), rewind: Rc::default() };
        c.render();
        c
    }
    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
    /// (turn, prompt, files), oldest first as recorded; shown newest first.
    pub fn set_checkpoints(&self, cps: Vec<(usize, String, Vec<String>)>) {
        self.st.borrow_mut().0 = cps;
        self.render();
    }
    /// While a turn runs the rewind buttons are replaced by a note.
    pub fn set_busy(&self, busy: bool) {
        self.st.borrow_mut().1 = busy;
        self.render();
    }
    /// `f(turn, chat)`: chat = also rewind the conversation.
    pub fn on_rewind(&self, f: impl Fn(usize, bool) + 'static) {
        *self.rewind.borrow_mut() = Some(Box::new(move |(t, c)| f(t, c)));
    }

    fn render(&self) {
        clear(&self.root);
        let st = self.st.borrow();
        if st.0.is_empty() {
            self.root.append(&empty("No checkpoints yet.", "Each message you send saves the files qh is about to change, so you can go back."));
            return;
        }
        let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
        col.set_margin_start(32);
        col.set_margin_end(32);
        col.set_margin_top(24);
        col.set_margin_bottom(24);
        let clamp = gtk::Box::new(gtk::Orientation::Vertical, 0);
        clamp.set_halign(gtk::Align::Fill);
        let t = label("Checkpoints", &["qh-title"]);
        t.set_margin_bottom(6);
        col.append(&t);
        for (turn, prompt, files) in st.0.iter().rev() {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
            row.add_css_class("qh-tilecard");
            let p = label(if prompt.is_empty() { "" } else { prompt }, &[]);
            if prompt.is_empty() {
                p.set_text(&format!("message {}", turn + 1))
            }
            p.set_wrap(true);
            p.set_lines(2);
            p.set_ellipsize(pango::EllipsizeMode::End);
            row.append(&p);
            let f = if files.is_empty() {
                "no files changed".to_string()
            } else {
                let mut s = files.iter().take(6).cloned().collect::<Vec<_>>().join("  ");
                if files.len() > 6 {
                    s += &format!("  +{}", files.len() - 6)
                }
                s
            };
            let fl = label(&f, &["qh-mono", "qh-faint"]);
            fl.set_ellipsize(pango::EllipsizeMode::End);
            row.append(&fl);
            let acts = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let a = button("Rewind files", "qh-card-btn");
            let b = button("Rewind files and chat", "");
            let (cb, t) = (self.rewind.clone(), *turn);
            a.connect_clicked(move |_| fire(&cb, (t, false)));
            let cb = self.rewind.clone();
            b.connect_clicked(move |_| fire(&cb, (t, true)));
            a.set_sensitive(!st.1);
            b.set_sensitive(!st.1);
            acts.append(&a);
            acts.append(&b);
            if st.1 {
                let n = label("stop the turn to rewind", &["qh-faint"]);
                n.set_margin_start(4);
                acts.append(&n);
            }
            row.append(&acts);
            col.append(&row);
        }
        let sw = gtk::ScrolledWindow::new();
        sw.set_vexpand(true);
        sw.set_hscrollbar_policy(gtk::PolicyType::Never);
        let wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        col.set_hexpand(true);
        col.set_size_request(-1, -1);
        wrap.append(&col);
        // keep the column readable on wide windows, like the Mac's 760pt
        let limit = gtk::Box::new(gtk::Orientation::Vertical, 0);
        limit.append(&wrap);
        limit.set_halign(gtk::Align::Center);
        limit.set_width_request(760);
        sw.set_child(Some(&limit));
        self.root.append(&sw);
    }
}

// MARK: file tree

/// Folders first, collapsible, lazy: rows are only built for expanded folders.
#[derive(Clone)]
pub struct FileTree {
    root: gtk::ScrolledWindow,
    view: gtk::ListView,
    open: Cb<String>,
}

impl Default for FileTree {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct Dir {
    dirs: BTreeMap<String, Dir>,
    files: Vec<String>,
}

fn sort_key(s: &str) -> String {
    s.to_lowercase()
}

/// Children of `dir` as a ListModel of StringObjects ("full/path/" for folders, "full/path" for files).
fn dir_model(d: &Dir, prefix: &str) -> gio::ListModel {
    let mut dirs: Vec<&String> = d.dirs.keys().collect();
    dirs.sort_by_key(|s| sort_key(s));
    let mut files = d.files.clone();
    files.sort_by_key(|s| sort_key(s));
    let mut items: Vec<String> = dirs.iter().map(|n| format!("{prefix}{n}/")).collect();
    items.extend(files.iter().map(|n| format!("{prefix}{n}")));
    let refs: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
    gtk::StringList::new(&refs).upcast()
}

impl FileTree {
    pub fn new() -> Self {
        let root = gtk::ScrolledWindow::new();
        root.add_css_class("qh-code");
        root.set_hscrollbar_policy(gtk::PolicyType::Never);
        let view = gtk::ListView::new(None::<gtk::NoSelection>, None::<gtk::ListItemFactory>);
        view.add_css_class("qh-tree");
        view.set_single_click_activate(true);
        root.set_child(Some(&view));
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, it| {
            let it = it.downcast_ref::<gtk::ListItem>().unwrap();
            let ex = gtk::TreeExpander::new();
            ex.set_indent_for_icon(true);
            let l = label("", &[]);
            l.set_ellipsize(pango::EllipsizeMode::End);
            ex.set_child(Some(&l));
            it.set_child(Some(&ex));
        });
        factory.connect_bind(|_, it| {
            let it = it.downcast_ref::<gtk::ListItem>().unwrap();
            let row = it.item().and_downcast::<gtk::TreeListRow>().unwrap();
            let path = row.item().and_downcast::<gtk::StringObject>().unwrap().string();
            let ex = it.child().and_downcast::<gtk::TreeExpander>().unwrap();
            ex.set_list_row(Some(&row));
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
            let l = ex.child().and_downcast::<gtk::Label>().unwrap();
            l.set_text(&name);
            if path.ends_with('/') {
                ex.add_css_class("qh-dir")
            } else {
                ex.remove_css_class("qh-dir")
            }
        });
        view.set_factory(Some(&factory));
        let open: Cb<String> = Rc::default();
        let cb = open.clone();
        view.connect_activate(move |v, pos| {
            let Some(m) = v.model() else { return };
            let Some(row) = m.item(pos).and_downcast::<gtk::TreeListRow>() else { return };
            let p = row.item().and_downcast::<gtk::StringObject>().unwrap().string();
            if p.ends_with('/') {
                row.set_expanded(!row.is_expanded())
            } else {
                fire(&cb, p.to_string())
            }
        });
        FileTree { root, view, open }
    }
    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
    /// "/"-separated relative paths.
    pub fn set_files(&self, files: Vec<String>) {
        let mut top = Dir::default();
        for f in &files {
            let mut parts: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
            let Some(name) = parts.pop() else { continue };
            let mut d = &mut top;
            for p in parts {
                d = d.dirs.entry(p.to_string()).or_default();
            }
            d.files.push(name.to_string());
        }
        let top = Rc::new(top);
        let t2 = top.clone();
        let tree = gtk::TreeListModel::new(dir_model(&top, ""), false, false, move |obj| {
            let p = obj.downcast_ref::<gtk::StringObject>()?.string();
            if !p.ends_with('/') {
                return None;
            }
            let mut d: &Dir = &t2;
            for part in p.trim_end_matches('/').split('/') {
                d = d.dirs.get(part)?;
            }
            Some(dir_model(d, &p))
        });
        self.view.set_model(Some(&gtk::SingleSelection::new(Some(tree))));
    }
    pub fn on_open(&self, f: impl Fn(String) + 'static) {
        *self.open.borrow_mut() = Some(Box::new(f));
    }
}

// MARK: code view

const LINE: i32 = 19;

/// Light syntax colouring: comments, strings, numbers and keywords of common languages.
fn highlight(text: &str, path: &str) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let ext = path.rsplit('.').next().unwrap_or("");
    let hash = matches!(ext, "py" | "sh" | "rb" | "toml" | "yaml" | "yml" | "pl" | "r");
    let known = matches!(
        ext,
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "swift" | "c" | "h" | "cpp" | "hpp" | "cs" | "java" | "kt" | "sh" | "rb" | "toml" | "yaml" | "yml" | "php" | "zig"
    );
    if !known || text.len() > 400_000 {
        return glib::markup_escape_text(text).to_string();
    }
    let re = RE.get_or_init(|| {
        regex::Regex::new(concat!(
            r#"(?P<c>//[^\n]*|/\*(?s:.*?)\*/|#[^\n]*)|(?P<s>"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.){0,3}'|`[^`]*`)|(?P<n>\b\d[\d_]*(?:\.\d+)?\b)|"#,
            r"(?P<k>\b(?:fn|let|mut|pub|use|mod|impl|struct|enum|trait|match|if|else|for|while|loop|return|break|continue|in|as|const|static|self|Self|true|false|None|Some|Ok|Err|def|class|import|from|elif|lambda|with|yield|async|await|function|var|new|this|func|package|type|interface|switch|case|default|guard|struct|nil|null|void|int|bool|string|public|private|protected|try|catch|throw|where|super|extends|export|is|not|and|or|pass|raise|except|finally|unsafe|dyn|ref|move|crate)\b)",
        ))
        .unwrap()
    });
    let mut out = String::with_capacity(text.len() * 2);
    let mut last = 0;
    for c in re.captures_iter(text) {
        let m = c.get(0).unwrap();
        let (cls, ok) = if let Some(x) = c.name("c") {
            ("--qh-c", !x.as_str().starts_with('#') || hash)
        } else if c.name("s").is_some() {
            ("--qh-s", true)
        } else if c.name("n").is_some() {
            ("--qh-n", true)
        } else {
            ("--qh-k", true)
        };
        if !ok {
            continue;
        }
        out.push_str(&glib::markup_escape_text(&text[last..m.start()]));
        let col = color_for(cls);
        out.push_str(&format!("<span foreground=\"{col}\">{}</span>", glib::markup_escape_text(m.as_str())));
        last = m.end();
    }
    out.push_str(&glib::markup_escape_text(&text[last..]));
    out
}

fn is_dark() -> bool {
    gtk::Settings::default().is_some_and(|s| {
        s.find_property("gtk-interface-color-scheme").is_some_and(|_| {
            let v = s.property_value("gtk-interface-color-scheme");
            glib::EnumValue::from_value(&v).is_some_and(|(_, e)| e.nick() == "dark")
        }) || s.is_gtk_application_prefer_dark_theme()
    })
}

fn color_for(var: &str) -> &'static str {
    let d = is_dark();
    match var {
        "--qh-k" => if d { "#FF8A5C" } else { "#B3360F" },
        "--qh-s" => if d { "#8FD18A" } else { "#2F7D3A" },
        "--qh-n" => if d { "#9AA0FF" } else { "#373FF5" },
        _ => if d { "#6F6E68" } else { "#9A9A94" },
    }
}

fn line_attrs() -> pango::AttrList {
    let a = pango::AttrList::new();
    a.insert(pango::AttrInt::new_line_height_absolute(LINE * pango::SCALE));
    a
}

struct CodeState {
    path: String,
    text: String,
    lines: usize,
    line: Option<usize>,
    sel: Option<(usize, usize)>,
}

/// Read-only code with a line-number gutter, line highlight, gutter selection and an inline-edit bar.
#[derive(Clone)]
pub struct CodeView {
    root: gtk::Box,
    title: gtk::Label,
    gutter: gtk::Label,
    body: gtk::Label,
    marks: gtk::Fixed,
    scroll: gtk::ScrolledWindow,
    bar: gtk::Box,
    hint: gtk::Label,
    st: Rc<RefCell<CodeState>>,
    open_editor: Cb<(String, usize)>,
    inline: Cb<(usize, usize, String)>,
    apply: Cb0,
    discard: Cb0,
}

impl Default for CodeView {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeView {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("qh-code");
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.set_margin_start(16);
        head.set_margin_end(16);
        head.set_margin_top(12);
        head.set_margin_bottom(8);
        let title = label("", &["qh-mono", "qh-soft"]);
        title.set_hexpand(true);
        title.set_ellipsize(pango::EllipsizeMode::Start);
        let edit = button("Edit lines", "");
        edit.set_tooltip_text(Some("Ctrl+I"));
        let ed = button("Open in editor", "");
        head.append(&title);
        head.append(&edit);
        head.append(&ed);
        root.append(&head);

        let gutter = label("", &["qh-mono", "qh-gutter"]);
        gutter.set_xalign(1.0);
        gutter.set_valign(gtk::Align::Start);
        gutter.set_width_chars(5);
        gutter.set_attributes(Some(&line_attrs()));
        let body = label("", &["qh-mono"]);
        body.set_valign(gtk::Align::Start);
        body.set_selectable(true);
        body.set_attributes(Some(&line_attrs()));
        body.set_hexpand(true);
        let rows = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        rows.append(&gutter);
        rows.append(&body);
        rows.set_margin_end(16);
        rows.set_margin_bottom(16);
        // highlights sit on a Fixed underneath the text
        let marks = gtk::Fixed::new();
        marks.set_can_target(false);
        let ov = gtk::Overlay::new();
        ov.set_child(Some(&marks));
        ov.add_overlay(&rows);
        ov.set_measure_overlay(&rows, true);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_child(Some(&ov));
        root.append(&scroll);
        let hint = label("", &["qh-faint"]);
        hint.set_margin_start(16);
        hint.set_margin_top(6);
        hint.set_margin_bottom(10);
        hint.set_visible(false);
        root.append(&hint);
        let bar = gtk::Box::new(gtk::Orientation::Vertical, 8);
        bar.add_css_class("qh-bar");
        bar.set_visible(false);
        root.append(&bar);

        let cv = CodeView {
            root,
            title,
            gutter: gutter.clone(),
            body,
            marks,
            scroll,
            bar,
            hint,
            st: Rc::new(RefCell::new(CodeState { path: String::new(), text: String::new(), lines: 0, line: None, sel: None })),
            open_editor: Rc::default(),
            inline: Rc::default(),
            apply: Rc::default(),
            discard: Rc::default(),
        };
        let me = cv.clone();
        ed.connect_clicked(move |_| {
            let st = me.st.borrow();
            let l = st.sel.map(|s| s.0).or(st.line).unwrap_or(1);
            fire(&me.open_editor, (st.path.clone(), l));
        });
        let me = cv.clone();
        edit.connect_clicked(move |_| me.open_bar());
        let g = gtk::GestureClick::new();
        gutter.set_tooltip_text(Some("Click a line, shift-click for a range, then Ctrl+I to edit it"));
        let me = cv.clone();
        g.connect_pressed(move |g, _, _, y| {
            let n = me.st.borrow().lines.max(1);
            let line = ((y as i32 / LINE) + 1).clamp(1, n as i32) as usize;
            let shift = g.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
            me.pick(line, shift);
        });
        gutter.add_controller(g);
        let sc = gtk::ShortcutController::new();
        sc.set_scope(gtk::ShortcutScope::Managed);
        let me = cv.clone();
        sc.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>i"),
            Some(gtk::CallbackAction::new(move |_, _| {
                me.open_bar();
                glib::Propagation::Stop
            })),
        ));
        cv.root.add_controller(sc);
        cv.root.set_focusable(true);
        cv
    }
    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
    /// Show `text` as `path`; `line` (1-based) is marked and scrolled to.
    pub fn set_file(&self, path: &str, text: &str, line: Option<usize>) {
        let text = text.replace("\r\n", "\n");
        let n = split(&text).len().max(1);
        {
            let mut st = self.st.borrow_mut();
            st.path = path.into();
            st.lines = n;
            st.line = line;
            st.sel = None;
            st.text = text.clone();
        }
        self.title.set_text(path);
        self.gutter.set_text(&(1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join("\n"));
        let body = text.strip_suffix('\n').unwrap_or(&text);
        if n <= 6000 {
            self.body.set_markup(&highlight(body, path));
            self.body.set_attributes(Some(&line_attrs()));
        } else {
            self.body.set_text(body);
        }
        self.bar.set_visible(false);
        self.refresh_marks();
        if let Some(l) = line {
            self.scroll_to(l)
        }
    }
    /// Scroll so `line` sits near the top and mark it.
    pub fn scroll_to(&self, line: usize) {
        self.st.borrow_mut().line = Some(line);
        self.refresh_marks();
        let sc = self.scroll.clone();
        glib::idle_add_local_once(move || {
            let adj = sc.vadjustment();
            adj.set_value((line.saturating_sub(8) as f64) * LINE as f64);
        });
    }
    /// The picked range (start, end), 1-based inclusive.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.st.borrow().sel
    }
    pub fn on_open_editor(&self, f: impl Fn(String, usize) + 'static) {
        *self.open_editor.borrow_mut() = Some(Box::new(move |(p, l)| f(p, l)));
    }
    pub fn on_inline(&self, f: impl Fn(usize, usize, String) + 'static) {
        *self.inline.borrow_mut() = Some(Box::new(move |(a, b, s)| f(a, b, s)));
    }
    pub fn on_inline_apply(&self, f: impl Fn() + 'static) {
        *self.apply.borrow_mut() = Some(Box::new(f));
    }
    pub fn on_inline_discard(&self, f: impl Fn() + 'static) {
        *self.discard.borrow_mut() = Some(Box::new(f));
    }

    fn pick(&self, line: usize, extend: bool) {
        {
            let mut st = self.st.borrow_mut();
            st.sel = match (extend, st.sel) {
                (true, Some((a, _))) => Some((a.min(line), a.max(line))),
                _ => Some((line, line)),
            };
        }
        self.refresh_marks();
    }

    fn refresh_marks(&self) {
        while let Some(c) = self.marks.first_child() {
            self.marks.remove(&c)
        }
        let st = self.st.borrow();
        let w = 4000;
        let mark = |from: usize, to: usize, cls: &str| {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
            b.add_css_class(cls);
            b.set_size_request(w, LINE * (to - from + 1) as i32);
            self.marks.put(&b, 0.0, ((from - 1) as i32 * LINE) as f64);
        };
        if let Some((a, b)) = st.sel {
            mark(a, b.min(st.lines), "qh-pick");
        } else if let Some(l) = st.line.filter(|l| *l >= 1 && *l <= st.lines) {
            mark(l, l, "qh-hl");
        }
        // spacer so the Fixed spans the whole text height
        let sp = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sp.set_size_request(1, LINE * st.lines as i32 + 16);
        self.marks.put(&sp, 0.0, 0.0);
        match st.sel {
            Some((a, b)) if !self.bar.is_visible() => {
                self.hint.set_text(&format!("lines {a}–{b} picked · Ctrl+I to edit them"));
                self.hint.set_visible(true);
            }
            _ => self.hint.set_visible(false),
        }
    }

    fn bar_head(&self, a: usize, b: usize) {
        clear(&self.bar);
        let path = self.st.borrow().path.clone();
        self.bar.append(&label(&format!("Edit {path}:{a}–{b}"), &["qh-medium"]));
    }

    /// Open the instruction bar for the picked lines (or the marked line).
    pub fn open_bar(&self) {
        let (a, b, n) = {
            let st = self.st.borrow();
            let (a, b) = st.sel.or(st.line.map(|l| (l, l))).unwrap_or((1, 1));
            (a, b, st.lines.max(1))
        };
        self.bar_head(a, b);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let s = gtk::SpinButton::with_range(1.0, n as f64, 1.0);
        s.set_value(a as f64);
        let e = gtk::SpinButton::with_range(1.0, n as f64, 1.0);
        e.set_value(b as f64);
        let entry = gtk::Entry::new();
        entry.set_placeholder_text(Some("Change these lines… (Enter to send, Esc to cancel)"));
        entry.set_hexpand(true);
        row.append(&label("from", &["qh-faint"]));
        row.append(&s);
        row.append(&label("to", &["qh-faint"]));
        row.append(&e);
        row.append(&entry);
        self.bar.append(&row);
        let acts = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let send = button("Send", "qh-ink-btn");
        let cancel = button("Discard", "");
        acts.append(&send);
        acts.append(&cancel);
        self.bar.append(&acts);
        let go = {
            let (me, s, e, entry) = (self.clone(), s.clone(), e.clone(), entry.clone());
            move || {
                let t = entry.text().to_string();
                if t.trim().is_empty() {
                    return;
                }
                let (a, b) = (s.value() as usize, e.value() as usize);
                let (a, b) = (a.min(b), a.max(b));
                me.st.borrow_mut().sel = Some((a, b));
                me.bar_head(a, b);
                me.bar.append(&label(&format!("› {t}"), &["qh-soft"]));
                me.refresh_marks();
                fire(&me.inline, (a, b, t));
            }
        };
        let g2 = go.clone();
        entry.connect_activate(move |_| g2());
        send.connect_clicked(move |_| go());
        let me = self.clone();
        cancel.connect_clicked(move |_| me.close_bar(true));
        let k = gtk::EventControllerKey::new();
        let me = self.clone();
        k.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                me.close_bar(true);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(k);
        self.bar.set_visible(true);
        self.hint.set_visible(false);
        entry.grab_focus();
    }

    fn close_bar(&self, notify: bool) {
        self.bar.set_visible(false);
        clear(&self.bar);
        self.refresh_marks();
        if notify {
            if let Some(f) = self.discard.borrow().as_ref() {
                f()
            }
        }
    }

    /// The streamed answer so far (shown while the edit runs).
    pub fn show_inline_preview(&self, text: &str) {
        // keep head + instruction, replace anything after
        let mut kids = vec![];
        let mut c = self.bar.first_child();
        while let Some(w) = c {
            c = w.next_sibling();
            kids.push(w);
        }
        for w in kids.iter().skip(2) {
            self.bar.remove(w)
        }
        let l = label(if text.is_empty() { "thinking…" } else { text }, &["qh-mono", "qh-soft"]);
        l.set_wrap(true);
        l.set_margin_start(12);
        l.set_margin_end(12);
        l.set_margin_top(8);
        l.set_margin_bottom(8);
        let sw = gtk::ScrolledWindow::new();
        sw.add_css_class("qh-box");
        sw.set_max_content_height(220);
        sw.set_propagate_natural_height(true);
        sw.set_child(Some(&l));
        self.bar.append(&sw);
        self.bar_actions(false);
        self.bar.set_visible(true);
    }

    /// The finished edit as a −/+ mini diff with Keep / Discard.
    pub fn show_inline_result(&self, before: &str, after: &str) {
        let mut kids = vec![];
        let mut c = self.bar.first_child();
        while let Some(w) = c {
            c = w.next_sibling();
            kids.push(w);
        }
        for w in kids.iter().skip(2) {
            self.bar.remove(w)
        }
        let d = diff_view(before, after);
        d.set_margin_top(6);
        d.set_margin_bottom(6);
        let sw = gtk::ScrolledWindow::new();
        sw.add_css_class("qh-box");
        sw.set_max_content_height(260);
        sw.set_propagate_natural_height(true);
        sw.set_child(Some(&d));
        self.bar.append(&sw);
        self.bar_actions(true);
        self.bar.set_visible(true);
    }

    /// An error in place of the preview.
    pub fn show_inline_error(&self, msg: &str) {
        let l = label(msg, &["qh-err"]);
        l.set_wrap(true);
        self.bar.append(&l);
        self.bar_actions(false);
    }

    /// Hide the inline bar (after the app applied or dropped the edit).
    pub fn close_inline(&self) {
        self.close_bar(false)
    }

    fn bar_actions(&self, done: bool) {
        let acts = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        if done {
            let k = button("Keep", "qh-ink-btn");
            let me = self.clone();
            k.connect_clicked(move |_| {
                if let Some(f) = me.apply.borrow().as_ref() {
                    f()
                }
                me.close_bar(false);
            });
            acts.append(&k);
        }
        let d = button("Discard", "");
        let me = self.clone();
        d.connect_clicked(move |_| me.close_bar(true));
        acts.append(&d);
        self.bar.append(&acts);
    }
}

// MARK: layout switch

/// Which panes the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Chat,
    Split,
    Code,
}

/// Chat | Split | Code segmented control.
#[derive(Clone)]
pub struct LayoutSwitch {
    root: gtk::Box,
    btns: Vec<gtk::ToggleButton>,
    change: Cb<Layout>,
}

impl Default for LayoutSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutSwitch {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        root.add_css_class("qh-switch");
        root.set_valign(gtk::Align::Center);
        let change: Cb<Layout> = Rc::default();
        let mut btns: Vec<gtk::ToggleButton> = vec![];
        for (i, (name, l)) in [("Chat", Layout::Chat), ("Split", Layout::Split), ("Code", Layout::Code)].into_iter().enumerate() {
            let b = gtk::ToggleButton::with_label(name);
            b.set_tooltip_text(Some(&format!("Ctrl+{}", i + 1)));
            if let Some(f) = btns.first() {
                b.set_group(Some(f))
            }
            let cb = change.clone();
            b.connect_toggled(move |b| {
                if b.is_active() {
                    fire(&cb, l)
                }
            });
            root.append(&b);
            btns.push(b);
        }
        btns[0].set_active(true);
        LayoutSwitch { root, btns, change }
    }
    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
    /// Select without firing on_change.
    pub fn set_layout(&self, l: Layout) {
        let cb = self.change.borrow_mut().take();
        self.btns[l as usize].set_active(true);
        *self.change.borrow_mut() = cb;
    }
    pub fn layout(&self) -> Layout {
        [Layout::Chat, Layout::Split, Layout::Code][self.btns.iter().position(|b| b.is_active()).unwrap_or(0)]
    }
    pub fn on_change(&self, f: impl Fn(Layout) + 'static) {
        *self.change.borrow_mut() = Some(Box::new(f));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_basic() {
        let d = diff_lines("a\nb\nc\n", "a\nx\nc\n");
        assert_eq!(d, vec![(' ', "a".into()), ('-', "b".into()), ('+', "x".into()), (' ', "c".into())]);
        assert_eq!(diff_count("", "a\nb"), (2, 0));
    }
}
