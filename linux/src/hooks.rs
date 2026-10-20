//! Test hooks, all off unless their variable is set, for driving the app off-screen:
//!   QH_APP_PROMPT=text|@file   send a prompt 3 s in (QH_APP_PROMPT_DELAY)
//!   QH_APP_LAYOUT=chat|split|code, QH_APP_OPEN=path[:line]   at 2 s
//!   QH_APP_INLINE=instruction (+ QH_APP_INLINE_LINES=a-b)   an inline edit on the open file
//!   QH_APP_PAGE=review|checkpoints|memory|activity|tasks|settings|team|palette|code (QH_APP_PAGE_DELAY)
//!   QH_APP_ANSWER=yes|no|always (QH_APP_ANSWER_DELAY)   answer the open ask like Ctrl+Enter / Ctrl+Backspace
//!   QH_APP_MODE=ask|auto-edit|auto|plan   at 2 s
//!   QH_APP_SEQ=secs:page:out.png;…   several pages, each snapshotted 2 s after it shows
//!   QH_APP_RESUME=1   resume the newest saved session at 2 s
//!   QH_APP_COMPACT=1|0   force compact on/off (not saved)
//!   QH_APP_SNAPSHOT=out.png (QH_APP_SNAPSHOT_DELAY, default 6 s), QH_APP_EXIT=1 quits after it
//!   QH_APP_RSS=1   print VmRSS when the snapshot is taken

use crate::ui::{self, Shared};
use gtk::glib;
use gtk::prelude::*;

fn env(k: &str) -> Option<String> { std::env::var(k).ok().filter(|v| !v.is_empty()) }
fn secs(k: &str, d: u32) -> u32 { env(k).and_then(|v| v.parse().ok()).unwrap_or(d) }

fn later(s: u32, f: impl FnOnce() + 'static) { glib::timeout_add_seconds_local_once(s, f); }

pub fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())).unwrap_or(0)
}

fn show(s: &Shared, page: &str) {
    let path = s.borrow().current.clone();
    match page {
        "settings" => { s.borrow_mut().settings = true; ui::dirty("settings") }
        "team" => { if let Some(app) = gtk::gio::Application::default() { app.activate_action("team", None) } }
        "personal" => { if let Some(app) = gtk::gio::Application::default() { app.activate_action("team", None) } }
        "palette" => { ui::show_palette(s); ui::palette_inline(true) }
        "chat" | "split" | "code" => { s.borrow_mut().settings = false; if let Some(p) = s.borrow_mut().project_mut() { p.layout = page.into(); p.page = None } }
        "none" => { s.borrow_mut().settings = false; if let Some(p) = s.borrow_mut().project_mut() { p.page = None } }
        pg => if let Some(path) = path { ui::show_page(s, &path, Some(pg)) },
    }
    ui::schedule(s);
}

pub fn run(st: &Shared, peek_snap: bool) {
    if let Some(c) = env("QH_APP_COMPACT") { st.borrow_mut().cfg.compact = Some(c == "1") }
    let snap_delay = secs("QH_APP_SNAPSHOT_DELAY", 6);
    // off-screen (a hidden workspace) the frame clock is paused, so fade-in animations would sit at
    // opacity 0 and live thread rows never show in a snapshot: turn animations off for test runs
    if env("QH_APP_SNAPSHOT").is_some() || env("QH_APP_SEQ").is_some() {
        if let Some(g) = gtk::Settings::default() { g.set_gtk_enable_animations(false) }
    }
    if let Some(text) = env("QH_APP_PROMPT").map(|t| t.strip_prefix('@').and_then(|f| std::fs::read_to_string(f).ok()).unwrap_or(t)) {
        let s = st.clone();
        later(secs("QH_APP_PROMPT_DELAY", 3), move || ui::prompt(&s, &text, vec![]));
    }
    if env("QH_APP_LAYOUT").is_some() || env("QH_APP_OPEN").is_some() {
        let s = st.clone();
        later(secs("QH_APP_OPEN_DELAY", 2), move || {
            if let Some(l) = env("QH_APP_LAYOUT") { if let Some(p) = s.borrow_mut().project_mut() { p.layout = l } }
            if let Some(f) = env("QH_APP_OPEN") {
                let path = s.borrow().current.clone();
                if let Some(path) = path {
                    let keep = s.borrow().project().map(|p| p.layout.clone());
                    ui::open_file(&s, &path, &f);
                    if let (Some(l), Some(p)) = (keep.filter(|_| env("QH_APP_LAYOUT").is_some()), s.borrow_mut().project_mut()) { p.layout = l }
                }
            }
            ui::schedule(&s);
        });
    }
    if let Some(instr) = env("QH_APP_INLINE") {
        let s = st.clone();
        later(secs("QH_APP_INLINE_DELAY", 3), move || {
            let (a, b) = env("QH_APP_INLINE_LINES").and_then(|r| { let (a, b) = r.split_once('-')?; Some((a.parse().ok()?, b.parse().ok()?)) }).unwrap_or((1, 2));
            let Some(u) = ui::ui() else { return };
            u.code.scroll_to(a);
            u.code.open_bar();
            let mut st = s.borrow_mut();
            if let Some(p) = st.project_mut() {
                if let Some((path, _)) = p.code.clone() {
                    p.inline = Some(crate::model::Inline { id: None, preview: String::new() });
                    p.cmd("inline_edit", serde_json::json!({"path": path, "start": a, "end": b, "instruction": instr}));
                }
            }
        });
    }
    if env("QH_APP_RESUME").is_some() {
        let s = st.clone();
        later(2, move || if let Some(p) = s.borrow().project() {
            if let Some(x) = p.sessions.first() { p.send(serde_json::json!({"type": "resume", "id": x.id})) }
        });
    }
    if let Some(mode) = env("QH_APP_MODE") {
        let s = st.clone();
        later(2, move || if let Some(p) = s.borrow().project() { p.send(serde_json::json!({"type": "mode", "mode": mode})) });
    }
    if let Some(ans) = env("QH_APP_ANSWER") {
        let s = st.clone();
        later(secs("QH_APP_ANSWER_DELAY", 20), move || {
            let id = s.borrow().project().and_then(|p| { let t = p.current_name(); p.asks.iter().find(|a| a.tab == t).map(|a| a.id) });
            match (ans.as_str(), id) {
                ("always", Some(id)) => { if let Some(p) = s.borrow_mut().project_mut() { p.answer(id, "always") } ui::schedule(&s) }
                (a, _) => { eprintln!("qh-app: answer {a}: {}", ui::answer_current(&s, a)); }
            }
        });
    }
    if let Some(page) = env("QH_APP_PAGE") {
        let s = st.clone();
        later(secs("QH_APP_PAGE_DELAY", snap_delay.saturating_sub(2).max(1)), move || show(&s, &page));
    }
    // QH_APP_SEQ="secs:page:out.png;…": show each page, snapshot it 2 s later
    for step in env("QH_APP_SEQ").unwrap_or_default().split([';', ',']).filter(|x| !x.is_empty()) {
        let mut it = step.splitn(3, ':');
        let (Some(t), Some(page), Some(out)) = (it.next().and_then(|t| t.parse::<u32>().ok()), it.next(), it.next()) else { continue };
        let (s, page, out) = (st.clone(), page.to_string(), out.to_string());
        later(t, move || {
            show(&s, &page);
            later(2, move || {
                let Some(u) = ui::ui() else { return };
                match ui::snapshot_png(u.window.upcast_ref(), &out) { Ok(()) => eprintln!("qh-app: snapshot {out}"), Err(e) => eprintln!("qh-app: snapshot failed: {e}") }
                if page == "palette" { ui::palette_inline(false) }
            });
        });
    }
    if let Some(path) = env("QH_APP_SNAPSHOT") {
        later(snap_delay, move || {
            let Some(u) = ui::ui() else { return };
            let w: gtk::Widget = if peek_snap { ui::peek_window().map(|p| p.upcast()).unwrap_or(u.window.clone().upcast()) } else { u.window.clone().upcast() };
            match ui::snapshot_png(&w, &path) { Ok(()) => eprintln!("qh-app: snapshot {path}"), Err(e) => eprintln!("qh-app: snapshot failed: {e}") }
            if env("QH_APP_RSS").is_some() { eprintln!("qh-app: rss {} kB ({})", rss_kb(), std::env::var("GSK_RENDERER").unwrap_or_default()) }
            if env("QH_APP_EXIT").is_some() { if let Some(a) = gtk::gio::Application::default() { a.quit() } }
        });
    }
}
