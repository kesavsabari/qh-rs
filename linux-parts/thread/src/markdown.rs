//! Assistant markdown as GTK widgets: one pango-markup Label per block, fenced code as its own
//! tile with a copy button. Cheap enough to rebuild on every streaming tick.

use crate::label;
use gtk::glib;
use gtk::prelude::*;
use std::rc::Rc;

/// How inline `code` becomes a file link: `is_file` decides, `on_open` is called with the path.
#[derive(Clone, Default)]
pub struct Links {
    pub is_file: Option<Rc<dyn Fn(&str) -> bool>>,
    pub on_open: Option<Rc<dyn Fn(&str)>>,
}

impl Links {
    pub fn new(is_file: impl Fn(&str) -> bool + 'static, on_open: impl Fn(&str) + 'static) -> Self {
        Links { is_file: Some(Rc::new(is_file)), on_open: Some(Rc::new(on_open)) }
    }
    pub(crate) fn file(&self, s: &str) -> bool {
        self.is_file.as_ref().is_some_and(|f| f(s))
    }
}

/// Render markdown with no file links.
pub fn render(md: &str) -> gtk::Widget {
    render_with(md, &Links::default())
}

/// Render markdown; inline code that `links.is_file` accepts opens via `links.on_open`.
pub fn render_with(md: &str, links: &Links) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.add_css_class("qh-md");
    let lines: Vec<&str> = md.lines().collect();
    let mut i = 0;
    let mut para: Vec<&str> = Vec::new();
    let mut list: Option<gtk::Box> = None;
    let flush = |para: &mut Vec<&str>, root: &gtk::Box| {
        if !para.is_empty() {
            root.append(&text_label(&inline(&para.join(" "), links), links, &[]));
            para.clear();
        }
    };
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        if let Some(fence) = t.strip_prefix("```") {
            flush(&mut para, &root);
            let _lang = fence.trim();
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            root.append(&code_block(&code.join("\n")));
            i += 1;
            continue;
        }
        if list_item(t).is_none() && !(list.is_some() && !t.is_empty() && line.starts_with("  ")) {
            list = None;
        }
        if t.is_empty() {
            flush(&mut para, &root);
        } else if let Some((lvl, rest)) = heading(t) {
            flush(&mut para, &root);
            let cls = ["qh-h1", "qh-h2", "qh-h3"][lvl.min(3) - 1];
            root.append(&text_label(&inline(rest, links), links, &[cls]));
        } else if let Some(q) = t.strip_prefix('>') {
            flush(&mut para, &root);
            let mut body = vec![q.trim_start()];
            while i + 1 < lines.len() && lines[i + 1].trim_start().starts_with('>') {
                i += 1;
                body.push(lines[i].trim_start()[1..].trim_start());
            }
            let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
            b.add_css_class("qh-quote");
            b.append(&text_label(&inline(&body.join(" "), links), links, &[]));
            root.append(&b);
        } else if let Some((mark, rest)) = list_item(t) {
            flush(&mut para, &root);
            let indent = (line.len() - t.len()) as i32;
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.set_margin_start(indent * 6);
            let b = label(&mark, &["qh-bullet"]);
            b.set_wrap(false);
            b.set_yalign(0.0);
            b.set_width_chars(2);
            row.append(&b);
            let l = text_label(&inline(rest, links), links, &[]);
            l.set_hexpand(true);
            row.append(&l);
            let lb = list.get_or_insert_with(|| {
                let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
                root.append(&b);
                b
            });
            lb.append(&row);
        } else {
            para.push(t);
        }
        i += 1;
    }
    flush(&mut para, &root);
    root.upcast()
}

fn heading(t: &str) -> Option<(usize, &str)> {
    let n = t.chars().take_while(|&c| c == '#').count();
    (1..=6).contains(&n).then(|| t[n..].strip_prefix(' ').map(|r| (n, r.trim()))).flatten()
}

pub(crate) fn list_item(t: &str) -> Option<(String, &str)> {
    for p in ["- ", "* ", "+ "] {
        if let Some(r) = t.strip_prefix(p) {
            return Some(("•".into(), r));
        }
    }
    let d = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if d > 0 && d < 4 {
        let rest = &t[d..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &t[..d]), r));
        }
    }
    None
}

fn text_label(markup: &str, links: &Links, classes: &[&str]) -> gtk::Label {
    let l = label("", classes);
    l.set_markup(markup);
    l.set_selectable(true);
    l.set_focusable(false);
    let on_open = links.on_open.clone();
    l.connect_activate_link(move |_, uri| match uri.strip_prefix("qhfile:") {
        Some(p) => {
            if let Some(f) = &on_open {
                f(p);
            }
            glib::Propagation::Stop
        }
        None => glib::Propagation::Proceed,
    });
    l
}

fn code_block(code: &str) -> gtk::Widget {
    let o = gtk::Overlay::new();
    o.add_css_class("qh-code");
    let l = gtk::Label::new(Some(code));
    l.set_xalign(0.0);
    l.set_selectable(true);
    l.set_focusable(false);
    let sw = gtk::ScrolledWindow::new();
    sw.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
    sw.set_child(Some(&l));
    o.set_child(Some(&sw));
    let copy = gtk::Button::with_label("copy");
    copy.add_css_class("qh-copy");
    copy.set_halign(gtk::Align::End);
    copy.set_valign(gtk::Align::Start);
    let text = code.to_string();
    copy.connect_clicked(move |b| {
        b.clipboard().set_text(&text);
        b.set_label("copied");
        let b = b.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || b.set_label("copy"));
    });
    o.add_overlay(&copy);
    o.upcast()
}

fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

/// Inline markdown to pango markup: `code`, **bold**, *italic*/_italic_, [text](url).
pub(crate) fn inline(s: &str, links: &Links) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        match c {
            '`' => {
                if let Some(end) = rest[1..].find('`') {
                    let code = &rest[1..1 + end];
                    let span = format!("<span font_family=\"Geist Mono\" size=\"90%\" bgcolor=\"#8080801F\">{}</span>", esc(code));
                    if links.file(code) {
                        out += &format!("<a href=\"qhfile:{}\">{}</a>", esc(code), span);
                    } else {
                        out += &span;
                    }
                    rest = &rest[end + 2..];
                    continue;
                }
            }
            '*' | '_' if rest[1..].starts_with(c) => {
                let mark = &rest[..2];
                if let Some(end) = rest[2..].find(mark).filter(|&e| e > 0) {
                    out += &format!("<b>{}</b>", inline(&rest[2..2 + end], links));
                    rest = &rest[end + 4..];
                    continue;
                }
            }
            '*' | '_' if rest.len() > 1 && !rest[1..].starts_with(' ') => {
                let prev_word = out.chars().last().is_some_and(|p| p.is_alphanumeric());
                if !(c == '_' && prev_word) {
                    if let Some(end) = rest[1..].find(c).filter(|&e| e > 0) {
                        out += &format!("<i>{}</i>", inline(&rest[1..1 + end], links));
                        rest = &rest[end + 2..];
                        continue;
                    }
                }
            }
            '[' => {
                if let Some(mid) = rest.find("](") {
                    if let Some(end) = rest[mid + 2..].find(')') {
                        let text = &rest[1..mid];
                        let url = &rest[mid + 2..mid + 2 + end];
                        out += &format!("<a href=\"{}\">{}</a>", esc(url), inline(text, links));
                        rest = &rest[mid + 3 + end..];
                        continue;
                    }
                }
            }
            _ => {}
        }
        out += &esc(&rest[..c.len_utf8()]);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Where `md` can be cut so everything before it renders the same on its own: the end of the
/// last blank line outside a code fence that isn't followed by more of a list. 0 for none.
pub(crate) fn settled(md: &str) -> usize {
    let (mut fence, mut cut, mut pos, mut blank_at) = (false, 0, 0, None);
    for line in md.split_inclusive('\n') {
        let t = line.trim_start();
        if !line.ends_with('\n') { break } // the last, still-growing line
        if t.starts_with("```") { fence = !fence }
        if !fence {
            if let Some(b) = blank_at.take() {
                if !t.trim().is_empty() && list_item(t).is_none() && !line.starts_with("  ") { cut = b }
            }
            if t.trim().is_empty() { blank_at = Some(pos + line.len()) }
        }
        pos += line.len();
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::settled;
    #[test]
    fn cuts_only_at_safe_blank_lines() {
        assert_eq!(settled("one\n\ntwo"), 0); // "two" may still be part of anything
        assert_eq!(settled("one\n\ntwo\n"), 5);
        assert_eq!(settled("```\na\n\nb\n```\n\nnext\n"), 14);
        assert_eq!(settled("- a\n\n- b\n"), 0); // a list keeps going
        assert_eq!(settled("- a\n\n  more\n"), 0);
        assert_eq!(settled("partial line"), 0);
    }
}
