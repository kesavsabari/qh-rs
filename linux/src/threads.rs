//! One qh_thread::Thread per tab, kept alive (with its scroll position) in a Stack. The saved
//! history fills it once; after that every live event is pushed into it as it arrives: streamed
//! text, tool lines that finish in place, diffs with Review, notices, errors, verify lines.

use crate::model::{self, Item};
use crate::ui::{self, Shared};
use qh_thread::{Links, ReplyHandle, Thread, ToolHandle};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

pub struct View {
    pub thread: Thread,
    reply: Option<ReplyHandle>,
    tools: VecDeque<ToolHandle>,
    /// Saved history not drawn yet, oldest first; pages come off its end as you scroll up.
    older: Vec<(u8, String)>,
}

/// Items drawn when a tab opens (a long history opens as fast as a short one), and per
/// scroll-up page: small enough to build inside one frame.
const PAGE: usize = 40;
const OLDER: usize = 20;

fn draw(thread: &Thread, items: Vec<(u8, String)>) {
    for (kind, s) in items {
        match kind {
            0 => thread.push_user(&s),
            1 => thread.push_reply().set_text(&s),
            _ => thread.push_tool(&s, "").finish(true, "", ""),
        }
    }
}

thread_local! {
    static VIEWS: RefCell<HashMap<String, View>> = RefCell::new(HashMap::new());
}

pub fn key(path: &str, tab: &str) -> String { format!("{path}\u{1f}{tab}") }

/// The thread widget for a tab, made (and filled from the saved history) on first use.
pub fn ensure(st: &Shared, path: &str, tab: &str, stack: &gtk::Stack) -> String {
    let k = key(path, tab);
    if VIEWS.with(|v| v.borrow().contains_key(&k)) { return k }
    let thread = Thread::new();
    thread.set_links(links(st, path));
    let (s, p) = (st.clone(), path.to_string());
    thread.set_on_review(move |_| ui::show_page(&s, &p, Some("review")));
    // the history, copied out so no borrow is held while widgets ask is_file()
    let items: Vec<(u8, String)> = st.borrow().projects.iter().find(|x| x.path == path)
        .and_then(|x| x.tabs.iter().find(|t| t.name == tab))
        .map(|t| t.items.iter().map(|i| match i {
            Item::User(s) => (0, s.clone()), Item::Reply(s) => (1, s.clone()), Item::Tool(s) => (2, s.clone()),
        }).collect()).unwrap_or_default();
    let mut older = items;
    let recent = older.split_off(older.len().saturating_sub(PAGE));
    draw(&thread, recent);
    let k2 = k.clone();
    thread.set_on_older(move || {
        // take the page out first: no map borrow while widgets are made
        let Some((t, page)) = with(&k2, |v| {
            let at = v.older.len().saturating_sub(OLDER);
            (v.thread.clone(), v.older.split_off(at))
        }) else { return };
        if !page.is_empty() { t.prepend(|| draw(&t, page)) }
    });
    stack.add_named(thread.widget(), Some(&k));
    VIEWS.with(|v| v.borrow_mut().insert(k.clone(), View { thread, reply: None, tools: VecDeque::new(), older }));
    k
}

/// Inline `code` that names one of the project's files (optionally :line) opens it in the code view.
fn links(st: &Shared, path: &str) -> Links {
    let (s1, p1, s2, p2) = (st.clone(), path.to_string(), st.clone(), path.to_string());
    Links::new(
        move |f| s1.try_borrow().map(|a| a.projects.iter().find(|x| x.path == p1).is_some_and(|x| x.file_ref(f).is_some())).unwrap_or(false),
        move |f| ui::open_file(&s2, &p2, f),
    )
}

/// Drop the views of a project (a fresh snapshot) or of one tab (a resumed history).
pub fn forget(stack: &gtk::Stack, path: &str, tab: Option<&str>) {
    let pre = key(path, "");
    VIEWS.with(|v| v.borrow_mut().retain(|k, view| {
        let hit = match tab { Some(t) => *k == key(path, t), None => k.starts_with(&pre) };
        if hit { stack.remove(view.thread.widget()) }
        !hit
    }));
}

fn with<R>(k: &str, f: impl FnOnce(&mut View) -> R) -> Option<R> {
    VIEWS.with(|v| v.borrow_mut().get_mut(k).map(f))
}

fn flush(v: &mut View) {
    if let Some(r) = v.reply.take() { r.flush() }
}

pub fn push_user(st: &Shared, path: &str, tab: &str, text: &str, user: bool, stack: &gtk::Stack) {
    let k = ensure(st, path, tab, stack);
    let t = with(&k, |v| { flush(v); v.thread.clone() });
    if let Some(t) = t { if user { t.push_user(text) } else { t.push_notice(text) } }
}

/// A live event for one project. Called after the model took it, with no borrow held.
pub fn on_event(st: &Shared, path: &str, e: &Value, stack: &gtk::Stack) {
    let ty = e["type"].as_str().unwrap_or("");
    let tab = e["tab"].as_str().unwrap_or("main").to_string();
    let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
    match ty {
        "snapshot" => forget(stack, path, None),
        "history" => forget(stack, path, Some(e["name"].as_str().unwrap_or(&tab))),
        "text" | "tool_call" | "tool_result" | "diff" | "notice" | "compact" | "verify" | "turn_done" | "rewound" | "inline_applied" | "error" => {
            let tab = if ty == "error" && !e["re"].is_null() {
                st.borrow().projects.iter().find(|p| p.path == path).map(|p| p.current_name()).unwrap_or(tab)
            } else { tab };
            let k = ensure(st, path, &tab, stack);
            // take what's needed out of the view, then touch widgets without the map borrowed
            match ty {
                "text" => {
                    let d = s("delta");
                    let (t, r) = with(&k, |v| (v.thread.clone(), v.reply.clone())).unwrap();
                    let r = r.unwrap_or_else(|| { let r = t.push_reply(); with(&k, |v| v.reply = Some(r.clone())); r });
                    r.append(&d);
                }
                "tool_call" => {
                    let t = with(&k, |v| { flush(v); v.thread.clone() }).unwrap();
                    let h = t.push_tool(&s("name"), &model::brief(&e["args"]));
                    with(&k, |v| v.tools.push_back(h));
                }
                "tool_result" => {
                    if let Some(Some(h)) = with(&k, |v| v.tools.pop_front()) {
                        h.finish(e["ok"].as_bool().unwrap_or(true), &s("summary"), &s("output"));
                    }
                }
                "turn_done" => { with(&k, |v| { flush(v); for h in v.tools.drain(..) { h.finish(true, "", "") } }); }
                _ => {
                    let t = with(&k, |v| { flush(v); v.thread.clone() }).unwrap();
                    match ty {
                        "diff" => t.push_diff(&s("path"), e["added"].as_u64().unwrap_or(0) as u32, e["removed"].as_u64().unwrap_or(0) as u32),
                        // the user bubble already says "+ N image(s)"; only serve's warning variant adds anything
                        "notice" if is_image_note(&s("message")) => {}
                        "notice" => t.push_notice(&s("message")),
                        "compact" => t.push_notice(e["message"].as_str().unwrap_or("compacted the conversation")),
                        "error" => t.push_error(e["message"].as_str().unwrap_or("error")),
                        "rewound" => t.push_notice(&format!("rewound to before message {}", e["turn"].as_u64().unwrap_or(0) + 1)),
                        "inline_applied" => t.push_notice(&format!("inline edit kept: {}", s("path"))),
                        "verify" => {
                            let (st2, p2, tab2) = (st.clone(), path.to_string(), tab.clone());
                            let fix: Box<dyn Fn()> = Box::new(move || {
                                if let Some(p) = st2.borrow_mut().by_path(&p2) { p.send(json!({"type": "fix_verify", "tab": tab2})) }
                            });
                            t.push_verify(&s("cmd"), e["ok"].as_bool().unwrap_or(false), e["secs"].as_f64().unwrap_or(0.0), &s("tail"), Some(fix));
                        }
                        _ => {}
                    }
                }
            }
        }
        _ => {}
    }
}

/// serve's plain "+ 1 image" / "+ 2 images" note for a prompt's attachments.
fn is_image_note(m: &str) -> bool {
    m.strip_prefix("+ ").and_then(|r| r.split_once(' ')).is_some_and(|(n, w)| n.parse::<u32>().is_ok() && (w == "image" || w == "images"))
}
