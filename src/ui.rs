//! Immediate-mode TUI: every frame draws only what is on screen, so the number
//! of sessions costs nothing until one is shown. Input and hub events share one
//! channel; the loop sleeps until something arrives, then drains everything
//! queued before drawing once.
use crate::engine;
use crate::hub::{lock, Answer, Event, Hub, MAX_HOPS};
use crate::{context, session, tools};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers as Mods};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const FRAME: Duration = Duration::from_millis(16);

pub enum Ui {
    Hub(String, Event),
    Term(TermEvent),
}

const GRID_MAX: usize = 4;
const MAX_ITEMS: usize = 400;
const HELP: &str = "/new <name> [profile]  new session (Ctrl+N: instant, auto-named)
/close (Ctrl+W)        close this session
/msg <name> <text>     message another session as yourself
/release               let this session's held messages through
/pause | /resume       hold / resume all automatic messaging
/layout tabs|grid      one session, or up to 4 side by side (Ctrl+G toggles)
/model [search]        pick this tab's model (local, Ollama cloud, any provider); /model default <m> for new tabs
/provider [add|remove|test <name>]  API keys (hidden input, saved chmod 600)
/cwd [path]            this session's working folder
/map (Ctrl+B)          live map of sessions, messages and shared folders
/tier                  how the context window was sized to your GPU
/mode [ask|auto-edit|auto|plan] (Shift+Tab)  /role [text]  /think  /tokens  /compact  /clear  /profile [name]
/undo  /diff  /allow [pattern]  /sessions  /commands (your own /name prompts)  /mcp (servers)
Alt+1..9 or Alt+←/→ switch · PgUp/PgDn scroll · Ctrl+O open last tool · Ctrl+L log · Esc interrupt · Ctrl+Q quit";

use crate::tools::DANGER;

/// Prompt templates as markdown files; a project's own override the user's global ones.
fn custom_commands(cwd: &std::path::Path) -> Vec<(String, std::path::PathBuf)> {
    let mut out: Vec<(String, std::path::PathBuf)> = vec![];
    let dirs = cwd.ancestors().map(|d| d.join(".qh/commands"))
        .chain([crate::config::config_dir().join("commands")]);
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut found: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect();
        found.sort();
        for p in found {
            let n = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
            if !out.iter().any(|(x, _)| *x == n) {
                out.push((n, p));
            }
        }
    }
    out
}

fn icon(status: &str) -> &'static str {
    match status {
        "working" => " …",
        "waiting-model" => " ⏳",
        "confirm" => " ⚠",
        "asking" => " ↔",
        "held" => " ⏸",
        _ => "",
    }
}

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

enum Item {
    User(String),
    Reply(String),
    Note(String),
    Err(String),
    Tool { title: String, args: String, result: String, open: bool },
    Stat(String),
}

#[derive(Default)]
struct View {
    items: VecDeque<Item>,
    streaming: bool,
    scroll: usize,
    unread: bool,
}

impl View {
    fn push(&mut self, it: Item) {
        if self.items.len() >= MAX_ITEMS {
            self.items.pop_front();
        }
        self.items.push_back(it);
    }
}

struct Confirm {
    session: String,
    tool: String,
    args: Value,
    rule: String,
    reply: Sender<Answer>,
    cwd: std::path::PathBuf,
    warn: String, // another session's files this would touch
}

struct App {
    hub: Arc<Hub>,
    views: HashMap<String, View>,
    active: String,
    grid: bool,
    recent: Vec<String>,
    log: VecDeque<String>,
    show_log: bool,
    input: Vec<char>,
    cur: usize,
    hist: Vec<String>,
    hist_i: usize,
    confirms: VecDeque<Confirm>,
    picker: Option<Picker>,
    secret: Option<String>, // provider whose API key is being typed (input is masked, never kept in history)
    map: Option<crate::map::Map>,
    // where things were last drawn, for mouse clicks
    tab_hits: Vec<(u16, u16, String)>,
    body: Rect,
    picker_rows: Option<(Rect, usize)>, // list area and the first visible row's index
    quit: bool,
}

/// `/model` popup: type to filter, arrows to move, Enter picks, Esc closes.
struct Picker {
    all: Vec<(String, String)>, // (value for /model, label shown)
    filter: String,
    sel: usize,
    cur: String,
}

impl Picker {
    fn hits(&self) -> Vec<&(String, String)> {
        let f = self.filter.to_lowercase();
        self.all.iter().filter(|(_, l)| f.split_whitespace().all(|w| l.to_lowercase().contains(w))).collect()
    }
}

/// Every model the user can pick: the local Ollama (plus Ollama's cloud catalog through it)
/// and each provider that has a key. (value, label) pairs.
fn model_list(hub: &Hub) -> Vec<(String, String)> {
    let k = |c: Option<usize>| c.map(|c| format!(" · {}k", c / 1000)).unwrap_or_default();
    let mut out: Vec<(String, String)> = crate::backend::list_models(&hub.cfg.host).into_iter()
        .map(|n| { let cloud = if crate::backend::is_cloud(&n) { "  ☁" } else { "" }; (n.clone(), format!("{n}{cloud}")) }).collect();
    for p in crate::providers::usable().into_iter().filter(|p| p.name != "ollama") {
        let Ok(models) = crate::providers::models(&p) else { continue };
        for mi in models {
            let v = crate::providers::display(&p.name, &mi.id);
            let price = if mi.price_in > 0.0 { format!(" · ${:.2}/${:.2} per M", mi.price_in, mi.price_out) } else { String::new() };
            let loc = if p.local { "" } else { "  ☁" };
            out.push((v.clone(), format!("{v}{}{price}{loc}", k(mi.ctx))));
        }
    }
    out
}

pub fn emitter(tx: Sender<Ui>) -> crate::hub::Emit {
    let tx = std::sync::Mutex::new(tx);
    Box::new(move |name: &str, ev: Event| {
        let _ = lock(&tx).send(Ui::Hub(name.to_string(), ev));
    })
}

pub fn run(hub: Arc<Hub>, tx: Sender<Ui>, rx: Receiver<Ui>) -> std::io::Result<()> {
    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableBracketedPaste);
    if hub.cfg.mouse {
        let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
    }
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(Ui::Term(ev)).is_err() {
                break;
            }
        }
    });
    let mut app = App {
        hub: hub.clone(), views: HashMap::new(), active: String::new(), grid: false, recent: vec![],
        log: VecDeque::new(), show_log: false, input: vec![], cur: 0, hist: vec![], hist_i: 0,
        confirms: VecDeque::new(), picker: None, secret: None, map: None,
        tab_hits: vec![], body: Rect::default(), picker_rows: None, quit: false,
    };
    for n in hub.names() {
        app.load_view(&n);
    }
    if let Some(n) = hub.names().first() {
        app.set_active(n);
    }
    let res = loop {
        if let Err(e) = term.draw(|f| app.draw(f)) {
            break Err(e);
        }
        let drawn = Instant::now();
        // the map animates, so it redraws on a timer; otherwise wait for something to happen
        if app.map.is_some() {
            match rx.recv_timeout(FRAME * 3) {
                Ok(x) => app.handle(x),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => break Ok(()),
            }
        } else {
            let Ok(first) = rx.recv() else { break Ok(()) };
            app.handle(first);
        }
        // at most one frame per FRAME: a fast stream is batched, not drawn token by token
        loop {
            let left = FRAME.saturating_sub(drawn.elapsed());
            match if left.is_zero() { rx.try_recv().map_err(|_| ()) } else { rx.recv_timeout(left).map_err(|_| ()) } {
                Ok(x) => app.handle(x),
                Err(()) => break,
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), event::DisableBracketedPaste, event::DisableMouseCapture);
    ratatui::restore();
    res
}

fn suggest_rule(tool: &str, a: &Value, rules: &[String]) -> String {
    if tool == "bash" {
        let cmd = a["cmd"].as_str().unwrap_or("");
        if DANGER.is_match(cmd) {
            return String::new();
        }
        // One rule per part of the chain that isn't already covered, joined with ", ".
        let Some(segs) = tools::segments(cmd) else { return String::new() };
        let mut out: Vec<String> = vec![];
        for seg in segs.iter().filter(|s| !crate::engine::seg_ok(rules, s)) {
            let w: Vec<&str> = seg.split_whitespace().collect();
            let r = match w.as_slice() {
                [] => continue,
                ["git", sub, ..] => format!("git {sub}*"),
                [first, ..] => format!("{first}*"),
            };
            if !out.contains(&r) {
                out.push(r);
            }
        }
        return out.join(", ");
    }
    let path = a["path"].as_str().unwrap_or("");
    match std::path::Path::new(path).parent().map(|p| p.display().to_string()) {
        Some(d) if !d.is_empty() && d != "." => format!("{tool}:{d}/*"),
        _ => format!("{tool}:*"),
    }
}

/// `@path` attaches that file's contents (capped) to the message.
fn expand_mentions(line: &str) -> String {
    let mut out = line.to_string();
    for w in line.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
        let p = if let Some(r) = w.strip_prefix("~/") { crate::config::home().join(r) } else { w.into() };
        if let Ok(body) = std::fs::read(&p) {
            let body = String::from_utf8_lossy(&body);
            let body: String = if body.chars().count() > 6000 { body.chars().take(6000).collect::<String>() + "\n…truncated" } else { body.into() };
            out += &format!("\n\n<file path=\"{}\">\n{body}\n</file>", p.display());
        }
    }
    out
}

/// Paths shown short: relative to the working directory, or ~ for home.
fn short_path(p: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = crate::config::home();
    let path = std::path::Path::new(p);
    if let Ok(r) = path.strip_prefix(&cwd) {
        return r.display().to_string();
    }
    match path.strip_prefix(&home) {
        Ok(r) if !home.as_os_str().is_empty() => format!("~/{}", r.display()),
        _ => p.to_string(),
    }
}

/// One readable line per tool call: the argument that matters, not raw JSON.
fn tool_title(tool: &str, a: &Value) -> String {
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    let one = |t: String, n: usize| clip(t.lines().next().unwrap_or(""), n);
    let size = |n: usize| if n >= 1000 { format!("{:.1}k chars", n as f64 / 1000.0) } else { format!("{n} chars") };
    match tool {
        "read_file" => {
            let range = match (a["start"].as_i64(), a["end"].as_i64()) {
                (Some(x), Some(y)) => format!(" L{x}-{y}"),
                (Some(x), None) if x > 1 => format!(" L{x}-"),
                _ => String::new(),
            };
            format!("▤ read {}{range}", short_path(&s("path")))
        }
        "write_file" => format!("✚ write {} ({})", short_path(&s("path")), size(s("content").chars().count())),
        "edit_file" => {
            let (o, n) = (s("old").lines().count(), s("new").lines().count());
            format!("✎ edit {} (−{o} +{n} lines)", short_path(&s("path")))
        }
        "bash" => format!("❯ {}", one(s("cmd"), 100)),
        "grep" => format!("⌕ grep {} {}", s("pattern"), short_path(&s("path"))),
        "find_references" => format!("⌕ references to {}", s("name")),
        "rename_symbol" => format!("⇄ rename {} → {}", s("old"), s("new")),
        "remember" => format!("✱ remember {}", one(s("fact"), 80)),
        "web_fetch" | "browse" => format!("↗ {tool} {}", clip(&s("url"), 90)),
        "click" => format!("◉ click [{}]", a["ref"]),
        "type" => format!("⌨ type [{}] \"{}\"{}", a["ref"], one(s("text"), 60),
                          if a["submit"].as_bool() == Some(true) { " ⏎" } else { "" }),
        "select" => format!("▾ select [{}] {}", a["ref"], s("option")),
        "scroll" => format!("↕ scroll {}", if s("direction").is_empty() { "down".into() } else { s("direction") }),
        "back" | "screenshot" | "snapshot" | "list_sessions" => format!("· {tool}"),
        "send_message" => format!("→ {}: {}", s("to"), one(s("text"), 90)),
        "ask" => format!("? {}: {}", s("to"), one(s("question"), 90)),
        "spawn" => format!("+ spawn {}: {}", s("name"), one(s("task"), 80)),
        "read_session" => format!("▤ read session {}", s("name")),
        _ => {
            let kv: Vec<String> = a.as_object().into_iter().flatten()
                .map(|(k, v)| format!("{k}={}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect();
            format!("⚙ {tool} {}", one(kv.join(" "), 90))
        }
    }
}

/// Tabs and control characters have no cell width, so the terminal and ratatui disagree on
/// where text ends and stale characters stay on screen. Expand tabs, drop the rest.
fn clean(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\t' => "    ".chars().collect::<Vec<_>>(),
            '\n' => vec!['\n'],
            c if c.is_control() => vec![],
            c => vec![c],
        })
        .collect()
}

fn clip(s: &str, n: usize) -> String {
    let s = &clean(s);
    let mut out: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        out.push('…');
    }
    out
}

impl App {
    fn view(&mut self, name: &str) -> &mut View {
        self.views.entry(name.to_string()).or_default()
    }

    /// A view starts with the last few exchanges of its saved history.
    fn load_view(&mut self, name: &str) {
        let Some(m) = self.hub.get(name) else { return };
        let msgs = lock(&m.msgs);
        let shown: Vec<&Value> = msgs.iter().skip(1)
            .filter(|x| matches!(context::role(x), "user" | "assistant") && !context::content(x).is_empty()).collect();
        let mut v = View::default();
        for x in &shown[shown.len().saturating_sub(10)..] {
            let c = context::content(x);
            v.push(if context::role(x) == "user" { Item::User(clip(c, 500)) } else { Item::Reply(c.to_string()) });
        }
        drop(msgs);
        self.views.insert(name.to_string(), v);
    }

    fn note(&mut self, name: &str, s: impl Into<String>) {
        self.view(name).push(Item::Note(s.into()));
    }

    fn err(&mut self, name: &str, s: impl Into<String>) {
        self.view(name).push(Item::Err(s.into()));
    }

    fn set_active(&mut self, name: &str) {
        if self.hub.get(name).is_none() {
            return;
        }
        self.active = name.to_string();
        self.recent.retain(|n| n != name);
        self.recent.insert(0, name.to_string());
        self.view(name).unread = false;
    }

    fn shown(&self) -> Vec<String> {
        let names = self.hub.names();
        if !self.grid {
            return vec![self.active.clone()];
        }
        let pick: Vec<&String> = self.recent.iter().filter(|n| names.contains(n)).take(GRID_MAX).collect();
        names.into_iter().filter(|n| pick.contains(&n)).collect()
    }

    // ---- events -----------------------------------------------------------

    fn handle(&mut self, ui: Ui) {
        match ui {
            Ui::Hub(name, ev) => self.hub_event(name, ev),
            Ui::Term(TermEvent::Key(k)) if k.kind != KeyEventKind::Release => self.key(k),
            Ui::Term(TermEvent::Mouse(ev)) => self.mouse(ev),
            Ui::Term(TermEvent::Paste(s)) => {
                for c in s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }) {
                    self.input.insert(self.cur, c);
                    self.cur += 1;
                }
            }
            Ui::Term(_) => {}
        }
    }

    fn hub_event(&mut self, name: String, ev: Event) {
        if !name.is_empty() && name != self.active && !matches!(ev, Event::Changed | Event::Added | Event::Log(_)) {
            self.view(&name).unread = true;
        }
        match ev {
            Event::Token(t) => {
                let v = self.view(&name);
                match v.items.back_mut() {
                    Some(Item::Reply(s)) if v.streaming => s.push_str(&t),
                    _ => {
                        v.push(Item::Reply(t));
                        v.streaming = true;
                    }
                }
            }
            Event::ToolCall { name: tool, args } => {
                let a = args.to_string();
                if matches!(tool.as_str(), "send_message" | "ask" | "spawn") {
                    let to = args["to"].as_str().or(args["name"].as_str()).unwrap_or("");
                    let text = ["text", "question", "task"].iter().find_map(|k| args[*k].as_str()).unwrap_or("");
                    self.log_line(format!("{name} → {to}: {text}"));
                }
                let v = self.view(&name);
                v.streaming = false;
                v.push(Item::Tool { title: tool_title(&tool, &args), args: a, result: String::new(), open: false });
            }
            Event::ToolResult { result, .. } => {
                // a turn's calls are listed together and their results arrive in the same order,
                // so the result belongs to the earliest unanswered call in the trailing run of tool lines
                let v = self.view(&name);
                let run = v.items.iter().rev().take_while(|i| matches!(i, Item::Tool { .. })).count();
                let start = v.items.len() - run;
                if let Some(Item::Tool { title, result: r, .. }) = v.items.iter_mut().skip(start)
                    .find(|i| matches!(i, Item::Tool { result, .. } if result.is_empty())) {
                    title.push_str(&format!("  → {}", clip(result.lines().next().unwrap_or(""), 100)));
                    *r = if result.is_empty() { " ".into() } else { clip(&result, 4000) };
                }
            }
            Event::Compact { stage, before, after } => self.note(&name, format!("compacted ({stage}): ≈{before} → ≈{after} tokens")),
            Event::Notice(t) => {
                self.view(&name).streaming = false;
                self.note(&name, t)
            }
            Event::Done { tokens, tps } => {
                let v = self.view(&name);
                v.streaming = false;
                v.push(Item::Stat(if tps > 0.0 { format!("{tokens} tokens · {tps} tok/s") } else { format!("{tokens} tokens") }));
            }
            Event::Confirm { tool, args, reply, cwd, warn } => {
                let rules = self.hub.get(&name).map(|m| lock(&m.set).allow.clone()).unwrap_or_default();
                let rule = suggest_rule(&tool, &args, &rules);
                self.confirms.push_back(Confirm { session: name, tool, args, rule, reply, cwd, warn });
            }
            Event::Added => {
                if !self.views.contains_key(&name) {
                    self.load_view(&name);
                }
            }
            Event::Log(s) => self.log_line(s),
            Event::Changed => {}
        }
    }

    fn log_line(&mut self, s: String) {
        if self.log.len() >= 500 {
            self.log.pop_front();
        }
        self.log.push_back(s);
    }

    fn key(&mut self, k: KeyEvent) {
        if let Some(c) = self.confirms.front() {
            let ans = match k.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(Answer::Yes),
                KeyCode::Char('a') if !c.rule.is_empty() => Some(Answer::Always(c.rule.clone())),
                KeyCode::Char('n') | KeyCode::Esc => Some(Answer::No),
                _ => None,
            };
            if let Some(a) = ans {
                let c = self.confirms.pop_front().unwrap();
                let _ = c.reply.send(a);
            }
            return;
        }
        if let Some(map) = self.map.as_mut() {
            let ids = map.ids(&self.hub);
            match k.code {
                KeyCode::Esc => self.map = None,
                KeyCode::Char('b') if k.modifiers.contains(Mods::CONTROL) => self.map = None,
                KeyCode::Up | KeyCode::BackTab => map.sel = map.sel.checked_sub(1).unwrap_or(ids.len().saturating_sub(1)),
                KeyCode::Down | KeyCode::Tab => map.sel = (map.sel + 1) % ids.len().max(1),
                KeyCode::Char('f') => map.files = !map.files,
                KeyCode::Enter => {
                    if let Some(id) = ids.get(map.sel).filter(|i| self.hub.get(i).is_some()).cloned() {
                        self.map = None;
                        self.set_active(&id);
                    }
                }
                KeyCode::Char('q') if k.modifiers.contains(Mods::CONTROL) => self.quit = true,
                _ => {}
            }
            return;
        }
        if let Some(p) = self.picker.as_mut() {
            let n = p.hits().len();
            match k.code {
                KeyCode::Esc => self.picker = None,
                KeyCode::Up => p.sel = p.sel.saturating_sub(1),
                KeyCode::Down => p.sel = (p.sel + 1).min(n.saturating_sub(1)),
                KeyCode::PageUp => p.sel = p.sel.saturating_sub(10),
                KeyCode::PageDown => p.sel = (p.sel + 10).min(n.saturating_sub(1)),
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.sel = 0;
                }
                KeyCode::Char(c) if !k.modifiers.contains(Mods::CONTROL) => {
                    p.filter.push(c);
                    p.sel = 0;
                }
                KeyCode::Enter => {
                    let pick = p.hits().get(p.sel).map(|s| s.0.clone());
                    self.picker = None;
                    if let Some(name) = pick {
                        self.command(&format!("/model {name}"));
                    }
                }
                _ => {}
            }
            return;
        }
        let (ctrl, alt) = (k.modifiers.contains(Mods::CONTROL), k.modifiers.contains(Mods::ALT));
        match k.code {
            KeyCode::Char('q') | KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('n') if ctrl => {
                let names = self.hub.names();
                let n = (2..).map(|i| format!("s{i}")).find(|n| !names.contains(n)).unwrap();
                self.command(&format!("/new {n}"));
            }
            KeyCode::Char('w') if ctrl => self.command("/close"),
            KeyCode::Char('g') if ctrl => self.grid = !self.grid,
            KeyCode::Char('b') if ctrl => self.map = Some(crate::map::Map::default()),
            KeyCode::BackTab => {
                let m = self.hub.cycle_mode();
                let a = self.active.clone();
                self.note(&a, format!("mode: {m} — {}", crate::hub::mode_help(m)));
            }
            KeyCode::Char('l') if ctrl => self.show_log = !self.show_log,
            KeyCode::Char('o') if ctrl => {
                let a = self.active.clone();
                if let Some(Item::Tool { open, .. }) = self.view(&a).items.iter_mut().rev().find(|i| matches!(i, Item::Tool { .. })) {
                    *open = !*open;
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.input.drain(..self.cur);
                self.cur = 0;
            }
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = self.input.len(),
            KeyCode::Char(d @ '1'..='9') if alt || ctrl => {
                if let Some(n) = self.hub.names().get(d as usize - '1' as usize) {
                    let n = n.clone();
                    self.set_active(&n);
                }
            }
            KeyCode::Left | KeyCode::Right if alt || ctrl => self.cycle(if k.code == KeyCode::Left { -1 } else { 1 }),
            KeyCode::PageUp | KeyCode::PageDown if ctrl => self.cycle(if k.code == KeyCode::PageUp { -1 } else { 1 }),
            KeyCode::Esc if self.secret.is_some() => {
                self.secret = None;
                self.input.clear();
                self.cur = 0;
                let a = self.active.clone();
                self.note(&a, "key entry cancelled");
            }
            KeyCode::Esc => self.hub.interrupt(&self.active),
            KeyCode::PageUp => {
                let a = self.active.clone();
                self.view(&a).scroll += 15;
            }
            KeyCode::PageDown => {
                let a = self.active.clone();
                let v = self.view(&a);
                v.scroll = v.scroll.saturating_sub(15);
            }
            KeyCode::Enter if self.secret.is_some() => {
                let key: String = self.input.drain(..).collect();
                self.cur = 0;
                let name = self.secret.take().unwrap_or_default();
                let a = self.active.clone();
                if key.trim().is_empty() {
                    return self.note(&a, "no key entered");
                }
                let msg = match crate::providers::set_key(&name, &key).and_then(|_| crate::providers::test(&name)) {
                    Ok(n) => format!("✓ {name}: key saved (chmod 600), {n} models available. /model {name} to pick one"),
                    Err(e) => format!("✗ {name}: key saved but the test call failed: {e}"),
                };
                self.note(&a, msg);
            }
            KeyCode::Enter => {
                let line: String = self.input.drain(..).collect();
                self.cur = 0;
                let line = line.trim().to_string();
                if line.is_empty() {
                    return;
                }
                self.hist.push(line.clone());
                self.hist_i = self.hist.len();
                let a = self.active.clone();
                self.view(&a).scroll = 0;
                if line.starts_with('/') {
                    self.command(&line);
                } else {
                    self.send(&line);
                }
            }
            KeyCode::Up | KeyCode::Down if !self.hist.is_empty() => {
                self.hist_i = if k.code == KeyCode::Up { self.hist_i.saturating_sub(1) } else { (self.hist_i + 1).min(self.hist.len()) };
                self.input = self.hist.get(self.hist_i).map(|s| s.chars().collect()).unwrap_or_default();
                self.cur = self.input.len();
            }
            KeyCode::Backspace if self.cur > 0 => {
                self.cur -= 1;
                self.input.remove(self.cur);
            }
            KeyCode::Delete if self.cur < self.input.len() => {
                self.input.remove(self.cur);
            }
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(self.input.len()),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = self.input.len(),
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cur, c);
                self.cur += 1;
            }
            _ => {}
        }
    }

    fn cycle(&mut self, d: isize) {
        let names = self.hub.names();
        if let Some(i) = names.iter().position(|n| *n == self.active) {
            let j = (i as isize + d).rem_euclid(names.len() as isize) as usize;
            let n = names[j].clone();
            self.set_active(&n);
        }
    }

    fn send(&mut self, line: &str) {
        let a = self.active.clone();
        let Some(m) = self.hub.get(&a) else { return };
        self.view(&a).push(Item::User(line.to_string()));
        if lock(&m.ctl).running {
            self.note(&a, "queued: sent when the current turn ends");
        }
        self.hub.send(&a, &expand_mentions(line), "user", 0, None);
    }

    fn command(&mut self, line: &str) {
        let a = self.active.clone();
        let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
        let arg = arg.trim();
        let Some(m) = self.hub.get(&a) else { return };
        let busy = lock(&m.ctl).running;
        match cmd {
            "/help" => self.note(&a, HELP),
            "/new" => {
                let mut p = arg.split_whitespace();
                let Some(n) = p.next() else { return self.err(&a, "usage: /new <name> [profile]") };
                match self.hub.add(n, p.next(), "", "", None, false, Some(m.cwd())) {
                    Ok(_) => {
                        if !self.views.contains_key(n) {
                            self.load_view(n);
                        }
                        self.set_active(n);
                    }
                    Err(e) => self.err(&a, e),
                }
            }
            "/close" => {
                if self.hub.names().len() == 1 {
                    return self.err(&a, "can't close the last session; Ctrl+Q quits");
                }
                let names = self.hub.names();
                let i = names.iter().position(|n| *n == a).unwrap_or(0);
                self.hub.close(&a);
                self.views.remove(&a);
                self.recent.retain(|n| *n != a);
                let names = self.hub.names();
                let next = self.recent.first().cloned().unwrap_or_else(|| names[i.min(names.len() - 1)].clone());
                self.set_active(&next);
            }
            "/msg" => {
                let (to, text) = arg.split_once(' ').unwrap_or((arg, ""));
                if text.is_empty() || !self.hub.send(to, text, "user", 0, None) {
                    return self.err(&a, format!("usage: /msg <{}> <text>", self.hub.names().join("|")));
                }
                self.view(to).push(Item::User(text.into()));
                self.note(&a, format!("sent to {to}"));
            }
            "/layout" => match arg {
                "tabs" | "grid" => self.grid = arg == "grid",
                _ => self.err(&a, "usage: /layout tabs|grid"),
            },
            "/release" => self.hub.release(&a),
            "/pause" | "/resume" => {
                self.hub.pause(cmd == "/pause");
                self.note(&a, if cmd == "/pause" { "messaging paused" } else { "messaging resumed" });
            }
            "/think" => {
                let mut s = lock(&m.set);
                s.think = !s.think;
                let t = s.think;
                drop(s);
                self.note(&a, format!("think {}", if t { "on" } else { "off" }));
            }
            "/tokens" => {
                let (t, r) = { let t = lock(&m.turn); (t.tokens, t.ratio) };
                let now = context::estimate(&lock(&m.msgs), r);
                self.note(&a, { let (c, at) = self.hub.ctx_for(&m, false); format!("last request {t}/{c} · now ≈{now} · compacts at {at}") });
            }
            "/compact" if busy => self.err(&a, "busy: wait for the turn to end or press Esc"),
            "/compact" => {
                let (hub, m) = (self.hub.clone(), m.clone());
                std::thread::spawn(move || {
                    if let Err(e) = engine::compact(&hub, &m, true) {
                        hub.emit(&m.name, Event::Notice(format!("compact failed: {e}")));
                    }
                    hub.changed();
                });
            }
            "/clear" | "/profile" if busy => self.err(&a, "busy: wait for the turn to end or press Esc"),
            "/clear" => {
                self.hub.reset(&m, None);
                self.views.insert(a.clone(), View::default());
                self.note(&a, "new session");
            }
            "/profile" => {
                if !arg.is_empty() {
                    if !tools::PROFILES.contains(&arg) {
                        return self.err(&a, format!("unknown profile, choose: {}", tools::PROFILES.join(", ")));
                    }
                    self.hub.reset(&m, Some(arg));
                    self.views.insert(a.clone(), View::default());
                }
                let s = lock(&m.set);
                let names: Vec<&str> = s.tools.iter().filter_map(|t| t["function"]["name"].as_str()).collect();
                let msg = format!("profile {}: {}", s.profile, names.join(", "));
                drop(s);
                self.note(&a, msg);
            }
            "/model" => {
                // `/model` opens the picker, `/model text` searches it; a single match, an exact
                // name, or a name nothing matches is selected. `/model default X` sets new tabs' model.
                if let Some(d) = arg.strip_prefix("default") {
                    let d = d.trim();
                    if d.is_empty() {
                        let (p, mo) = self.hub.default_model();
                        return self.note(&a, format!("new tabs start on {}", crate::providers::display(&p, &mo)));
                    }
                    let (p, mo) = crate::providers::parse_model(d);
                    self.hub.set_default_model(&p, &mo);
                    return self.note(&a, format!("new tabs start on {}", crate::providers::display(&p, &mo)));
                }
                let mut pick = arg.to_string();
                let mut note = String::new();
                if arg != "local" {
                    let all = model_list(&self.hub);
                    let exact = all.iter().any(|(v, _)| v == arg);
                    let words: Vec<String> = arg.to_lowercase().split_whitespace().map(String::from).collect();
                    let hits: Vec<&(String, String)> = all.iter()
                        .filter(|(_, l)| words.iter().all(|w| l.to_lowercase().contains(w.as_str()))).collect();
                    if !exact && (arg.is_empty() || hits.len() > 1) {
                        let cur = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
                        let sel = if arg.is_empty() { all.iter().position(|(v, _)| *v == cur).unwrap_or(0) } else { 0 };
                        self.picker = Some(Picker { all, filter: arg.to_string(), sel, cur });
                        return;
                    }
                    match hits.as_slice() {
                        [one] if !exact => pick = one.0.clone(),
                        [] => note = " (not in the model list, using it as typed)".into(),
                        _ => {}
                    }
                }
                {
                    let mut s = lock(&m.set);
                    (s.provider, s.model) = if pick == "local" {
                        (String::new(), self.hub.cfg.model.clone())
                    } else {
                        crate::providers::parse_model(&pick)
                    };
                }
                let (p, mo) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
                let (c, _) = self.hub.ctx_for(&m, true);
                let where_ = if crate::providers::is_local(&p, &mo) { "local GPU, takes turns" } else { "cloud, runs in parallel" };
                self.note(&a, format!("model {}{note} · {}k context · {where_}", crate::providers::display(&p, &mo), c / 1024));
                self.hub.save();
            }
            "/provider" | "/providers" => {
                let mut w = arg.split_whitespace();
                match (w.next(), w.next(), w.next()) {
                    (Some("add"), Some(name), key) => {
                        if crate::providers::get(name).is_none() {
                            return self.err(&a, format!("no provider {name}. Built in: {}. Others need a [providers.{name}] table (kind, base_url) in config",
                                crate::providers::all().iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")));
                        }
                        if let Some(k) = key {
                            self.input = k.chars().collect();
                            self.secret = Some(name.into());
                            return self.key(KeyEvent::new(KeyCode::Enter, Mods::NONE));
                        }
                        self.secret = Some(name.into());
                        self.note(&a, format!("paste the API key for {name} and press Enter (hidden; Esc cancels)"));
                    }
                    (Some("remove"), Some(name), _) => match crate::providers::remove_key(name) {
                        Ok(()) => self.note(&a, format!("removed the key for {name}")),
                        Err(e) => self.err(&a, e),
                    },
                    (Some("test"), Some(name), _) => match crate::providers::test(name) {
                        Ok(n) => self.note(&a, format!("✓ {name}: {n} models")),
                        Err(e) => self.err(&a, format!("✗ {name}: {e}")),
                    },
                    _ => {
                        let rows: Vec<String> = crate::providers::all().iter().map(|p| {
                            let state = if p.local { "local".to_string() } else if p.key.is_empty() {
                                format!("no key (/provider add {})", p.name)
                            } else { "key set".into() };
                            format!("  {:<13} {:<9} {state}", p.name, format!("{:?}", p.kind).to_lowercase())
                        }).collect();
                        self.note(&a, format!("providers:\n{}\n/provider add|remove|test <name> · keys in ~/.config/qwen-harness/credentials.toml", rows.join("\n")));
                    }
                }
            }
            "/map" => self.map = Some(crate::map::Map::default()),
            "/tier" => {
                let model = lock(&m.set).model.clone();
                let ctx = self.hub.cfg.ctx;
                let src = if self.hub.cfg.ctx_set { "set in config" } else { "sized to the GPU at start" };
                let live = crate::tier::detect(&self.hub.cfg.host, &model).map(|d| d.why).unwrap_or_else(|| "no NVIDIA GPU or model info".into());
                self.note(&a, format!("local window {}k (tier {}, {src})\nright now: {live}", ctx / 1024, crate::tier::tier(ctx)));
            }
            "/cwd" | "/cd" => {
                if !arg.is_empty() {
                    let p = tools::resolve(&m.cwd(), arg);
                    if !p.is_dir() {
                        return self.err(&a, format!("no such folder: {}", p.display()));
                    }
                    *lock(&m.cwd) = p;
                    self.hub.save();
                }
                let msg = format!("{} works in {}", a, m.cwd().display());
                self.note(&a, msg);
            }
            "/undo" => {
                let r = lock(&m.turn).ckpt.undo();
                self.note(&a, r);
            }
            "/diff" => {
                let r = lock(&m.turn).ckpt.diff();
                self.note(&a, r);
            }
            "/allow" => {
                let mut s = lock(&m.set);
                if !arg.is_empty() {
                    s.allow.push(arg.into());
                }
                let msg = format!("auto-approve: {}", if s.allow.is_empty() { "none".into() } else { s.allow.join(", ") });
                drop(s);
                self.note(&a, msg);
            }
            "/sessions" => self.note(&a, session::list(10)),
            "/mode" => {
                if !arg.is_empty() && !self.hub.set_mode(arg) {
                    return self.err(&a, "usage: /mode ask|auto-edit|auto|plan (Shift+Tab cycles)");
                }
                let m = self.hub.mode_name();
                self.note(&a, format!("mode: {m} — {}", crate::hub::mode_help(m)));
            }
            "/role" => {
                if !arg.is_empty() {
                    self.hub.set_role(&m, arg);
                }
                let r = lock(&m.purpose).clone();
                self.note(&a, if r.is_empty() { "no role yet: it is named from your first real request, or set one with /role <text>".into() } else { format!("role: {r}") });
            }
            "/mcp" => self.note(&a, crate::mcp::status()),
            "/commands" => {
                let list = custom_commands(&m.cwd());
                self.note(&a, if list.is_empty() {
                    "no custom commands: add <name>.md to .qh/commands/ (project) or ~/.config/qwen-harness/commands/; \
$ARGUMENTS is replaced by what you type after the command".into()
                } else {
                    list.iter().map(|(n, p)| format!("/{n}  {}", p.display())).collect::<Vec<_>>().join("\n")
                });
            }
            _ => match custom_commands(&m.cwd()).into_iter().find(|(n, _)| *n == cmd[1..]) {
                Some((_, path)) => {
                    let body = std::fs::read_to_string(&path).unwrap_or_default();
                    let text = if body.contains("$ARGUMENTS") { body.replace("$ARGUMENTS", arg) }
                               else if arg.is_empty() { body } else { format!("{body}\n\n{arg}") };
                    self.view(&a).push(Item::User(line.into()));
                    self.hub.send(&a, &expand_mentions(text.trim()), "user", 0, None);
                }
                None => self.err(&a, format!("unknown command {cmd}, try /help or /commands")),
            },
        }
    }

    // ---- drawing ----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let [tabs, body, status, input, foot] = Layout::vertical([
            Constraint::Length(1), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1),
        ]).areas(f.area());
        self.tab_hits = self.draw_tabs(f, tabs);
        self.body = body;
        let panes = if self.show_log {
            let [p, l] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(body);
            let block = Block::bordered().title(" message log ").border_style(Style::new().fg(DIM));
            let inner = block.inner(l);
            f.render_widget(block, l);
            let lines: Vec<Line> = self.log.iter().map(|s| Line::raw(s.as_str())).collect();
            let skip = lines.len().saturating_sub(inner.height as usize);
            f.render_widget(Paragraph::new(lines[skip..].to_vec()).wrap(ratatui::widgets::Wrap { trim: false }), inner);
            p
        } else {
            body
        };
        let shown = self.shown();
        if let Some(mut map) = self.map.take() {
            map.draw(f, panes, &self.hub);
            self.map = Some(map);
        } else if self.grid && shown.len() > 1 {
            let rows = if shown.len() > 2 { 2 } else { 1 };
            let row_areas = Layout::vertical(vec![Constraint::Fill(1); rows]).split(panes);
            for (i, name) in shown.iter().enumerate() {
                let r = row_areas[if rows == 2 { i / 2 } else { 0 }];
                let in_row = if rows == 2 { if i / 2 == 0 { 2 } else { shown.len() - 2 } } else { shown.len() };
                let cells = Layout::horizontal(vec![Constraint::Fill(1); in_row]).split(r);
                let cell = cells[if rows == 2 { i % 2 } else { i }];
                let m = self.hub.get(name);
                let title = m.map(|m| {
                    let role = lock(&m.purpose).clone();
                    let role = if role.is_empty() { String::new() } else { format!(" · {role}") };
                    let model = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
                    format!(" {}{}{role} · {} · {model} ", name, icon(m.status()), m.status())
                }).unwrap_or_default();
                let color = if *name == self.active { ACCENT } else { DIM };
                let block = Block::bordered().title(title).border_style(Style::new().fg(color));
                let inner = block.inner(cell);
                f.render_widget(block, cell);
                self.draw_chat(f, name, inner);
            }
        } else {
            let a = self.active.clone();
            self.draw_chat(f, &a, panes);
        }
        f.render_widget(Paragraph::new(self.status_line()).style(Style::new().fg(DIM)), status);
        let prompt = "› ";
        // an API key being typed is never drawn
        let text: String = if self.secret.is_some() { "•".repeat(self.input.len()) } else { self.input.iter().collect() };
        let before: usize = self.input[..self.cur].iter().map(|c| c.width().unwrap_or(0)).sum();
        let w = input.width.saturating_sub(3) as usize;
        let (shown_text, cx) = if before > w {
            let skip = before - w;
            let mut acc = 0;
            let s: String = text.chars().skip_while(|c| { acc += c.width().unwrap_or(0); acc <= skip }).collect();
            (s, w)
        } else {
            (text, before)
        };
        let line = if self.input.is_empty() {
            Line::from(vec![Span::styled(prompt, Style::new().fg(ACCENT)), Span::styled(match &self.secret {
                Some(p) => format!("API key for {p} (hidden, Esc cancels)"),
                None => "message, /help, @file to attach".into(),
            }, Style::new().fg(DIM))])
        } else {
            Line::from(vec![Span::styled(prompt, Style::new().fg(ACCENT)), Span::raw(shown_text)])
        };
        f.render_widget(Paragraph::new(line), input);
        f.set_cursor_position((input.x + 2 + cx as u16, input.y));
        let hints = "⇧Tab mode  ^N new  ^W close  ^G grid  ^B map  ^L log  Alt+←/→ tabs  PgUp/Dn scroll  ^O tool  Esc stop  ^Q quit";
        f.render_widget(Paragraph::new(hints).style(Style::new().fg(DIM)), foot);
        self.picker_rows = self.picker.as_ref().map(|p| draw_picker(f, p));
        if let Some(c) = self.confirms.front() {
            draw_confirm(f, c, self.confirms.len());
        }
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) -> Vec<(u16, u16, String)> {
        let names = self.hub.names();
        let labels: Vec<(String, bool, bool)> = names.iter().map(|n| {
            let st = self.hub.get(n).map(|m| m.status()).unwrap_or("idle");
            let unread = self.views.get(n).is_some_and(|v| v.unread) && *n != self.active;
            (format!(" {n}{}{} ", icon(st), if unread { " ●" } else { "" }), *n == self.active, unread)
        }).collect();
        let widths: Vec<usize> = labels.iter().map(|l| l.0.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() + 1).collect();
        // scroll the strip so the active tab is always visible
        let act = labels.iter().position(|l| l.1).unwrap_or(0);
        let mut start = 0;
        while start < act && widths[start..=act].iter().sum::<usize>() > area.width as usize {
            start += 1;
        }
        let mut spans = vec![];
        let mut hits = vec![];
        let mut x = area.x;
        if start > 0 {
            spans.push(Span::styled("‹", Style::new().fg(DIM)));
            x += 1;
        }
        for (i, (label, active, unread)) in labels[start..].iter().enumerate() {
            let w = widths[start + i] as u16;
            hits.push((x, x + w, names[start + i].clone()));
            x += w;
            let st = if *active {
                Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
            } else if *unread {
                Style::new().fg(ACCENT)
            } else {
                Style::new().fg(DIM)
            };
            spans.push(Span::styled(label.as_str(), st));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        hits
    }

    /// Wheel scrolls the chat (or the picker), clicks pick tabs, picker rows and map nodes.
    fn mouse(&mut self, ev: event::MouseEvent) {
        use event::{MouseButton, MouseEventKind as K};
        let (col, row) = (ev.column, ev.row);
        match ev.kind {
            K::ScrollUp | K::ScrollDown => {
                let up = matches!(ev.kind, K::ScrollUp);
                if let Some(p) = self.picker.as_mut() {
                    p.sel = if up { p.sel.saturating_sub(1) } else { (p.sel + 1).min(p.hits().len().saturating_sub(1)) };
                } else if self.map.is_none() {
                    let a = self.active.clone();
                    let v = self.view(&a);
                    v.scroll = if up { v.scroll + 3 } else { v.scroll.saturating_sub(3) };
                }
            }
            K::Down(MouseButton::Left) => {
                if let Some(p) = self.picker.as_mut() {
                    if let Some((r, top)) = self.picker_rows {
                        if row >= r.y && row < r.y + r.height && col >= r.x && col < r.x + r.width {
                            let i = top + (row - r.y) as usize;
                            if i < p.hits().len() {
                                if p.sel == i {
                                    return self.key(KeyEvent::new(KeyCode::Enter, Mods::NONE));
                                }
                                p.sel = i;
                            }
                        }
                    }
                    return;
                }
                if let Some(map) = self.map.as_mut() {
                    if let Some(id) = map.hit(self.body, col, row) {
                        let ids = map.ids(&self.hub);
                        let i = ids.iter().position(|x| *x == id).unwrap_or(0);
                        if map.sel == i && self.hub.get(&id).is_some() {
                            self.map = None;
                            return self.set_active(&id);
                        }
                        map.sel = i;
                    }
                    return;
                }
                if row == 0 {
                    if let Some((_, _, n)) = self.tab_hits.iter().find(|(a, b, _)| col >= *a && col < *b).cloned() {
                        self.set_active(&n);
                    }
                }
            }
            _ => {}
        }
    }

    fn status_line(&self) -> String {
        let Some(m) = self.hub.get(&self.active) else { return String::new() };
        let s = lock(&m.set);
        let (model, profile, think) = (crate::providers::display(&s.provider, &s.model), s.profile.clone(), s.think);
        drop(s);
        let live = m.live_tokens();
        let ctx = self.hub.ctx_for(&m, false).0 as u64;
        let bar = (20 * live / ctx.max(1)).min(20) as usize;
        let tps = lock(&m.turn).tps;
        let busy = self.hub.names().iter().filter(|n| **n != self.active)
            .filter(|n| self.hub.get(n).is_some_and(|m| m.status() != "idle")).count();
        let holder = lock(&self.hub.holder).clone();
        let role = lock(&m.purpose).clone();
        let role = if role.is_empty() { String::new() } else { format!(" ({role})") };
        let mode = self.hub.mode_name();
        let cwd = m.cwd();
        let home = crate::config::home();
        let dir = cwd.strip_prefix(&home).map(|r| format!("~/{}", r.display())).unwrap_or_else(|_| cwd.display().to_string());
        let dir = if dir.chars().count() > 28 {
            let parts: Vec<&str> = dir.rsplit(['/', '\\']).take(2).collect();
            format!("…/{}/{}", parts.get(1).unwrap_or(&""), parts[0])
        } else { dir };
        let mut out = format!("▶ {}{role} · {dir} · [{mode}] · {model} · {profile} · think {} · [{}{}] {live}/{ctx}", self.active,
                              if think { "on" } else { "off" }, "█".repeat(bar), "░".repeat(20 - bar));
        let cost = lock(&m.turn).cost;
        if cost > 0.0 { out += &if cost < 0.01 { format!(" · ${cost:.4}") } else { format!(" · ${cost:.2}") } }
        if tps > 0.0 { out += &format!(" · {tps} tok/s") }
        if !holder.is_empty() { out += &format!(" · model: {holder}") }
        if busy > 0 { out += &format!(" · {busy} other busy") }
        if self.hub.paused.load(std::sync::atomic::Ordering::Relaxed) { out += " · PAUSED" }
        if m.status() == "held" { out += &format!(" · held (> {MAX_HOPS} hops): /release") }
        out
    }

    /// Wrap only as many items as fit, walking up from the newest.
    fn draw_chat(&mut self, f: &mut Frame, name: &str, area: Rect) {
        let width = area.width.saturating_sub(2).max(10) as usize;
        let h = area.height as usize;
        let v = self.views.entry(name.to_string()).or_default();
        let need = h + v.scroll;
        let mut lines: Vec<Line<'static>> = vec![];
        for it in v.items.iter().rev() {
            let mut ls = item_lines(it, width, need - lines.len());
            ls.append(&mut lines);
            lines = ls;
            if lines.len() >= need {
                break;
            }
        }
        let total = lines.len();
        v.scroll = v.scroll.min(total.saturating_sub(h));
        let end = total - v.scroll;
        let start = end.saturating_sub(h);
        let inner = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), inner);
        if v.scroll > 0 {
            let tag = format!(" ↓ {} more ", v.scroll);
            let r = Rect { x: area.right().saturating_sub(tag.len() as u16 + 1), y: area.bottom() - 1, width: tag.len() as u16, height: 1 };
            f.render_widget(Paragraph::new(tag).style(Style::new().fg(Color::Black).bg(DIM)), r);
        }
    }
}

/// Rows for an item; a long reply is wrapped from its end and stops after `need` rows.
fn item_lines(it: &Item, w: usize, need: usize) -> Vec<Line<'static>> {
    let styled = |text: &str, st: Style, indent: usize| -> Vec<Line<'static>> {
        wrap(text, w.saturating_sub(indent)).into_iter()
            .map(|l| Line::from(Span::styled(" ".repeat(indent) + &l, st))).collect()
    };
    match it {
        Item::User(s) => {
            let mut v = vec![Line::raw("")];
            v.extend(styled(&format!("› {s}"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD), 0));
            v
        }
        Item::Reply(s) => {
            let fence = |l: &str| l.trim_start().starts_with("```");
            // walking backwards, the state after the last line is the parity of all fences
            let mut code = s.lines().filter(|l| fence(l)).count() % 2 == 1;
            let (mut rows, mut n) = (vec![], 0);
            for l in s.lines().rev() {
                if n >= need {
                    break;
                }
                if fence(l) {
                    code = !code;
                    rows.push(vec![Line::from(Span::styled(l.to_string(), Style::new().fg(DIM)))]);
                    n += 1;
                    continue;
                }
                let st = if code {
                    Style::new().fg(Color::Yellow)
                } else if l.starts_with('#') {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                let l = if code { l.to_string() } else { l.replace("**", "") };
                let ls = styled(&l, st, 0);
                n += ls.len();
                rows.push(ls);
            }
            rows.into_iter().rev().flatten().collect()
        }
        Item::Note(s) => styled(s, Style::new().fg(Color::Blue), 0),
        Item::Err(s) => styled(s, Style::new().fg(Color::Red), 0),
        Item::Stat(s) => styled(s, Style::new().fg(DIM), 0),
        Item::Tool { title, args, result, open } => {
            let mut v = styled(&format!("{} {title}", if *open { "▾" } else { "▸" }), Style::new().fg(DIM), 0);
            if *open {
                v.extend(styled(args, Style::new().fg(Color::Magenta), 2));
                let body: Vec<&str> = result.lines().take(40).collect();
                v.extend(styled(&body.join("\n"), Style::new(), 2));
            }
            v
        }
    }
}

/// Word wrap by display width; overlong words are broken hard.
fn wrap(text: &str, w: usize) -> Vec<String> {
    let text = &clean(text);
    let w = w.max(1);
    let mut out = vec![];
    for para in text.split('\n') {
        let mut line = String::new();
        let mut lw = 0;
        for word in para.split_inclusive(' ') {
            let ww: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
            if lw + ww > w && lw > 0 {
                out.push(std::mem::take(&mut line).trim_end().to_string());
                lw = 0;
            }
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if lw + cw > w {
                    out.push(std::mem::take(&mut line));
                    lw = 0;
                }
                line.push(c);
                lw += cw;
            }
        }
        out.push(line);
    }
    out
}

/// Returns the list rows' area and the index of the first visible row, for mouse clicks.
fn draw_picker(f: &mut Frame, p: &Picker) -> (Rect, usize) {
    let hits = p.hits();
    let area = f.area();
    let w = area.width.saturating_sub(4).min(80);
    let h = (hits.len() as u16 + 4).clamp(6, area.height.saturating_sub(4).max(6)).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let block = Block::bordered().border_style(Style::new().fg(ACCENT))
        .title(" model ").title_bottom(Line::styled(" ↑↓ move · type to filter · Enter pick · Esc close ", Style::new().fg(DIM)));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines = vec![Line::from(vec![Span::styled("search: ", Style::new().fg(DIM)), Span::raw(p.filter.clone()), Span::styled("▏", Style::new().fg(ACCENT))])];
    let rows = inner.height.saturating_sub(1) as usize;
    let top = p.sel.saturating_sub(rows.saturating_sub(1));
    if hits.is_empty() {
        lines.push(Line::styled("no match", Style::new().fg(DIM)));
    }
    for (i, (v, label)) in hits.iter().map(|h| (&h.0, &h.1)).enumerate().skip(top).take(rows) {
        let mark = if *v == p.cur { "● " } else { "  " };
        let st = if i == p.sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        lines.push(Line::styled(format!("{mark}{}", clip(label, w as usize - 4)), st));
    }
    f.render_widget(Paragraph::new(lines), inner);
    (Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner }, top)
}

fn draw_confirm(f: &mut Frame, c: &Confirm, queued: usize) {
    let a = &c.args;
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    let (summary, detail, danger): (String, Vec<Line>, bool) = match c.tool.as_str() {
        "bash" => {
            let cmd = s("cmd");
            let d = DANGER.is_match(&cmd);
            (format!("in {}", c.cwd.display()),
             cmd.lines().map(|l| Line::styled(l.to_string(), Style::new().fg(Color::Yellow))).collect(), d)
        }
        "edit_file" => {
            let mut v: Vec<Line> = s("old").lines().map(|l| Line::styled(format!("- {l}"), Style::new().fg(Color::Red))).collect();
            v.extend(s("new").lines().map(|l| Line::styled(format!("+ {l}"), Style::new().fg(Color::Green))));
            (s("path"), v, false)
        }
        "write_file" => {
            let (path, body) = (s("path"), s("content"));
            let exists = std::path::Path::new(&path).exists();
            (format!("{} {path}  {} lines", if exists { "overwrite" } else { "create" }, body.lines().count()),
             body.lines().take(200).enumerate().map(|(i, l)| Line::raw(format!("{:>4} {l}", i + 1))).collect(), exists)
        }
        "rename_symbol" => (format!("{} → {} across the project", s("old"), s("new")), vec![], false),
        _ => (clip(&a.to_string(), 80), vec![Line::raw(a.to_string())], false),
    };
    let area = f.area();
    let w = area.width.saturating_sub(4).min(90);
    let body_h = (detail.len() as u16).min(area.height.saturating_sub(12));
    let h = (body_h + 7).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let color = if danger || !c.warn.is_empty() { Color::Red } else { Color::Yellow };
    let more = if queued > 1 { format!(" (+{} waiting) ", queued - 1) } else { String::new() };
    let block = Block::bordered().border_style(Style::new().fg(color))
        .title(Line::from(vec![Span::styled(format!(" ⚙ {} ", c.tool), Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)),
                               Span::raw(format!("  {}  wants to run{more}", c.session))]));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines = vec![Line::styled(summary, Style::new().fg(DIM))];
    if !c.warn.is_empty() {
        lines.push(Line::styled(format!("⚠ {}", c.warn), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)));
    }
    lines.push(Line::raw(""));
    lines.extend(detail.into_iter().take(body_h as usize));
    lines.push(Line::raw(""));
    if !c.rule.is_empty() {
        lines.push(Line::from(vec![Span::styled("always = auto-approve ", Style::new().fg(DIM)), Span::styled(c.rule.clone(), Style::new().add_modifier(Modifier::BOLD))]));
    }
    let keys = if c.rule.is_empty() { "[y] allow   [n] deny" } else { "[y] allow   [a] always   [n] deny" };
    lines.push(Line::styled(keys, Style::new().add_modifier(Modifier::BOLD)));
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
}

pub fn channel_pair() -> (Sender<Ui>, Receiver<Ui>) {
    channel()
}
