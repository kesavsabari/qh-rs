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
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const FRAME: Duration = std::time::Duration::from_millis(16);

pub enum Ui {
    Hub(String, Event),
    Term(TermEvent),
}

const GRID_MAX: usize = 4;
const MAX_ITEMS: usize = 400;
/// Transcript text wraps at this many columns, however wide the terminal.
const MAX_LINE: u16 = 100;
const HELP: &str = "/new <name> [profile]  new session (Ctrl+N: instant, auto-named)
/new --worktree [name]  new tab in its own git worktree (branch qh/<name>)
/worktree [name]       move this tab into its own git worktree
/merge [--discard]     merge this worktree tab into its base branch (diff first), or throw it away
/close (Ctrl+W)        close this session
/msg <name> <text>     message another session as yourself
/release               let this session's held messages through
/pause | /resume       hold / resume all automatic messaging
/layout tabs|grid      one session, or up to 4 side by side (Ctrl+G toggles)
/model [search]        pick this tab's model (local, Ollama cloud, any provider); /model default <m> for new tabs
/provider              connect providers: sign in, add/test/remove keys
/cwd [path]            this session's working folder
/image <path> (Ctrl+V)  attach an image to your next message; Backspace on an empty line removes it
/map (Ctrl+B)          live map of sessions, messages and shared folders
/team (Ctrl+T)         start or join a team; in a team, the team room: map, chat, requests, tasks, PRs
/dm <login> <text>     private message to a teammate (only the two of you can read it); @login in chat pings
/team group <name> <login>…  a private group chat; /team group lists yours
/project (Ctrl+P)      switch project: team and personal; the others keep running (Ctrl+Space or Ctrl+] flips team ⇄ personal)
/anyway                this tab was paused because a teammate is changing the same function: go ahead anyway
/answers [auto|ask|off] whether teammates' agents may ask yours (answered from a read-only copy, 20 an hour)
/promise [fn what]     make a teammate's last answer (or your own words) a checked promise: edits that break it are held
/review-request        ask the owners of the code you changed (git blame, CODEOWNERS) to have their agents review it
/memory [open|edit|forget <name>]  what qh remembers about you and this project (memory = false turns it off)
/tier                  how the context window was sized to your GPU
/mode [ask|auto-edit|auto|plan] (Shift+Tab)  /role [text]  /think  /tokens  /compact  /clear  /profile [name]
/docs (guide in your browser)  /update (newest qh)  /undo  /diff  /ask-other [model] [question]  /allow [pattern]  /sandbox  /tasks [stop <id>]  /sessions  /commands (your own /name prompts)  /mcp (servers)
Shift+←/→ (or Alt+←/→, Alt+1..9) switch tabs · Shift+↑/↓ or PgUp/PgDn scroll · Option+←/→ a word · Ctrl+O open last tool · Ctrl+R replies only · Ctrl+L log · Esc interrupt · Ctrl+Q quit";

use crate::tools::DANGER;

/// What the `/` autocomplete offers; custom commands are added from .qh/commands.
const COMMANDS: [(&str, &str); 46] = [
    ("/memory", "what qh remembers: list, open, edit, forget"),
    ("/model", "pick this tab's model"), ("/provider", "connect providers: sign in, keys"),
    ("/new", "new session tab"), ("/worktree", "this tab in its own git worktree"),
    ("/merge", "merge this worktree tab back (diff first)"), ("/merge --discard", "remove this worktree and its branch"),
    ("/close", "close this tab"), ("/mode", "ask · auto-edit · auto · plan"),
    ("/compact", "shrink the conversation"), ("/clear", "start this tab fresh"), ("/undo", "undo the agent's file changes"),
    ("/ask-other", "second opinion on the diff or plan from another model"),
    ("/diff", "show the agent's file changes"), ("/cwd", "this tab's working folder"), ("/think", "thinking on/off, or an effort level"),
    ("/tokens", "context use, tokens and cost"), ("/tier", "this tab's context window"), ("/profile", "tool set: coding, general, browser, all"),
    ("/role", "give this tab a role"), ("/map", "map of sessions and messages"), ("/layout", "tabs or grid"),
    ("/msg", "message another tab"), ("/sessions", "saved sessions"), ("/allow", "auto-approve rules; /allow localhost:3000 lets the browser open it"), ("/sandbox", "what commands and tools may touch"),
    ("/tasks", "background tasks; /tasks stop <id>"),
    ("/release", "let held messages through"), ("/pause", "hold automatic messaging"), ("/resume", "resume messaging"),
    ("/mcp", "MCP servers"), ("/team", "your team: start or join; in a team, opens the team room (Ctrl+T)"), ("/relay", "relay server for teammates nothing else reaches"), ("/commands", "your own /commands"), ("/help", "everything"), ("/docs", "open the docs in your browser"), ("/update", "install the newest qh"), ("/project", "switch project (Ctrl+P)"),
    ("/image", "attach an image to your next message (Ctrl+V pastes one)"),
    ("/anyway", "let this tab edit what a teammate is changing"),
    ("/answers", "may teammates' agents ask yours: auto · ask · off"),
    ("/dm", "private message to a teammate: /dm <login> <text>"),
    ("/promise", "hold teammates to their last answer: edits that break it are held"),
    ("/review-request", "ask the owners' agents to review what you'd push (also done before the agent's git push)"),
    ("/cd", "same as /cwd"), ("/providers", "same as /provider"),
];

/// What each command can take, shown under it in the autocomplete. `<…>` marks something you
/// type; picking such an entry fills the box instead of running it.
const SUBCOMMANDS: &[(&str, &str)] = &[
    ("/think on", "the model thinks, at its default effort"),
    ("/think off", "no thinking: fastest"),
    ("/think low", "think briefly"),
    ("/think medium", "think at medium effort"),
    ("/think high", "think hard: slower, uses more of the plan"),
    ("/provider add <name>", "save an API key (hidden while you paste)"),
    ("/provider connect <name>", "sign in, test, replace or remove"),
    ("/provider signin chatgpt", "use your ChatGPT plan: sign in in the browser"),
    ("/provider signout chatgpt", "forget the ChatGPT sign-in"),
    ("/provider test <name>", "check the key works (lists its models)"),
    ("/provider remove <name>", "delete a saved key"),
    ("/model <search>", "search the model list, or type an exact name"),
    ("/model local", "back to your local model"),
    ("/model default", "which model new tabs start on"),
    ("/model default <provider/model>", "set the model new tabs start on"),
    ("/mode ask", "asks before anything that changes something"),
    ("/mode auto-edit", "file edits run; bash and browser typing still ask"),
    ("/mode auto", "everything runs except dangerous commands"),
    ("/mode plan", "read-only: explores and writes a plan"),
    ("/profile coding", "read, write and edit files, bash, search (starts the tab fresh)"),
    ("/profile general", "read files, bash, web and browser; no editing"),
    ("/profile browser", "just the browser plus reading and writing files"),
    ("/profile all", "every tool"),
    ("/promise <function> <what must stay true>", "your own promise, checked when a teammate edits it"),
    ("/promise list", "the promises this tab holds"),
    ("/promise drop <function>", "stop holding a teammate to it"),
    ("/dm <login> <text>", "a private message: only the two of you can read it"),
    ("/team group <name> <login> <login>…", "a private group chat: only its members can read it"),
    ("/team group", "your groups"),
    ("/answers auto", "teammates' agents ask yours; up to 20 answers an hour"),
    ("/answers ask", "you allow each question first"),
    ("/answers off", "they only see your fact sheet"),
    ("/layout tabs", "one session at a time"),
    ("/layout grid", "all sessions side by side"),
    ("/new <name> [profile]", "open a new session tab"),
    ("/msg <tab> <text>", "send a message to another tab's agent"),
    ("/cwd <folder>", "change this tab's working folder"),
    ("/allow <pattern>", "auto-approve matching tool calls, e.g. bash:cargo *"),
    ("/role <text>", "tell this tab what it's for"),
];

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
        "waiting-model" => " …",
        "confirm" => " !",
        "asking" => " ↔",
        "held" => " ||",
        _ => "",
    }
}

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEAM: Color = Color::Magenta; // a team space's colour, so you always know where you are
const MAP_TAB: &str = "\u{0}map"; // the team space's map, as a tab

enum Item {
    User(String),
    Reply(String),
    Note(String),
    Cmd(String), // a /command as typed, so its answer has context
    Err(String),
    Tool { title: String, args: String, result: String, open: bool },
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
    project: std::path::PathBuf, // the project on screen; tabs of other projects keep running unseen
    hub: Arc<Hub>,
    views: HashMap<String, View>,
    active: String,
    grid: bool,
    recent: Vec<String>,
    log: VecDeque<String>,
    show_log: bool,
    replies: bool, // only your messages and the agent's answers; tool calls fold to a count
    input: Vec<char>,
    cur: usize,
    chips: Vec<String>, // images (base64) attached to the next message
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
    sugg_sel: usize, // highlighted row of the / autocomplete
    team_view: Option<crate::teamview::TeamView>, // Ctrl+T overview
    outside: Option<Vec<String>>, // a terminal step with the UI suspended: ["create", friends…] or ["join", repo]
    // the two spaces Ctrl+Space flips between: the last personal project and the last team one
    last_personal: std::path::PathBuf,
    last_team: std::path::PathBuf,
    wt_hinted: bool, // the "two tabs edit one repo: try /worktree" hint was shown
    map_was_open: bool, // the team space had its map up when you left it
    notifier: crate::notify::Notifier,
}

/// Popup list (`/model`, `/provider`): type to filter, arrows to move, Enter runs `cmd value`.
/// Items with a group are shown under a heading per group.
struct Picker {
    title: &'static str,
    cmd: String,
    all: Vec<(String, String, String)>, // (value passed to cmd, label shown, group heading or "")
    filter: String,
    sel: usize,
    cur: String,
}

fn projects_file() -> std::path::PathBuf {
    crate::config::state_dir().join("projects.txt")
}

/// Projects you've opened, newest first: (folder, unix time last shown).
fn recent_projects() -> Vec<(std::path::PathBuf, u64)> {
    let mut v: Vec<(std::path::PathBuf, u64)> = std::fs::read_to_string(projects_file()).unwrap_or_default().lines()
        .filter_map(|l| l.split_once('\t')).map(|(t, p)| (std::path::PathBuf::from(p), t.parse().unwrap_or(0))).collect();
    v.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    v
}

fn remember_project(root: &std::path::Path) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut v = recent_projects();
    v.retain(|(p, _)| p != root);
    v.insert(0, (root.to_path_buf(), now));
    v.truncate(40);
    let _ = std::fs::create_dir_all(crate::config::state_dir());
    let _ = std::fs::write(projects_file(), v.iter().map(|(p, t)| format!("{t}\t{}", p.display())).collect::<Vec<_>>().join("\n"));
}

fn ago(t: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    match now.saturating_sub(t) { d @ 0..60 => format!("{d}s ago"), d @ 60..3600 => format!("{} min ago", d / 60),
                                  d @ 3600..86400 => format!("{}h ago", d / 3600), d => format!("{}d ago", d / 86400) }
}

/// One level of the "open another folder" browser for /project.
fn project_browser(dir: &std::path::Path) -> Picker {
    let d = dir.display().to_string();
    let mut all: Vec<(String, String, String)> = vec![(format!("open {d}"), format!("✓ open {d}"), String::new())];
    if let Some(up) = dir.parent() {
        all.push((format!("browse {}", up.display()), "↑ up".into(), String::new()));
    }
    let mut subs: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.')).collect();
    subs.sort_by_key(|n| n.to_lowercase());
    for n in subs {
        all.push((format!("browse {}", dir.join(&n).display()), format!("{n}/"), d.clone()));
    }
    Picker { title: "open a folder", cmd: "/project".into(), all, filter: String::new(), sel: 0, cur: String::new() }
}

/// One level of the /team create folder browser.
fn folder_picker(dir: &std::path::Path) -> Picker {
    let d = dir.display().to_string();
    let mut all: Vec<(String, String, String)> = vec![];
    if crate::teamsetup::not_a_project(dir).is_ok() {
        all.push((format!("create-at {d}"), format!("✓ start the team in {d}"), String::new()));
    }
    all.push((format!("new-in {d}"), "＋ new folder here".into(), String::new()));
    if let Some(up) = dir.parent() {
        all.push((format!("browse {}", up.display()), "↑ up".into(), String::new()));
    }
    let mut subs: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.starts_with('.'))
        .collect();
    subs.sort_by_key(|n| n.to_lowercase());
    for n in subs {
        all.push((format!("browse {}", dir.join(&n).display()), format!("{n}/"), d.clone()));
    }
    Picker { title: "team: which folder?", cmd: "/team".into(), all, filter: String::new(), sel: 0, cur: String::new() }
}

fn picker_g(title: &'static str, all: Vec<(String, String, String)>) -> Picker {
    Picker { title, cmd: "/team".into(), all, filter: String::new(), sel: 0, cur: String::new() }
}

fn picker(title: &'static str, cmd: &str, all: Vec<(String, String)>) -> Picker {
    Picker { title, cmd: cmd.into(), all: all.into_iter().map(|(v, l)| (v, l, String::new())).collect(),
             filter: String::new(), sel: 0, cur: String::new() }
}

impl Picker {
    fn hits(&self) -> Vec<&(String, String, String)> {
        let f = self.filter.to_lowercase();
        self.all.iter().filter(|(_, l, g)| {
            let hay = format!("{} {}", g.to_lowercase(), l.to_lowercase());
            f.split_whitespace().all(|w| hay.contains(w))
        }).collect()
    }

    /// Display rows: a heading (None, text) whenever the group changes, then items (Some(hit index), label).
    fn rows(&self) -> Vec<(Option<usize>, String)> {
        let mut out = vec![];
        let mut last: Option<&str> = None;
        for (i, (_, l, g)) in self.hits().into_iter().enumerate() {
            if !g.is_empty() && last != Some(g.as_str()) {
                out.push((None, g.clone()));
            }
            last = Some(g.as_str());
            out.push((Some(i), l.clone()));
        }
        out
    }
}

/// Every model the user can pick, grouped by where it runs: the local Ollama, Ollama's cloud
/// catalog through it, then each provider that has a key or sign-in. (value, label, group).
fn model_list(hub: &Hub) -> Vec<(String, String, String)> {
    let k = |c: Option<usize>| c.map(|c| format!(" · {}k", c / 1000)).unwrap_or_default();
    let mut out: Vec<(String, String, String)> = crate::backend::list_models(&hub.cfg.host).into_iter()
        .map(|n| {
            let g = if crate::backend::is_cloud(&n) { "Ollama cloud" } else { "Local (Ollama)" };
            (n.clone(), n, g.to_string())
        }).collect();
    out.sort_by_key(|(_, _, g)| g != "Local (Ollama)"); // stable: keeps each group's order
    for p in crate::providers::usable().into_iter().filter(|p| p.name != "ollama") {
        let Ok(models) = crate::providers::models(&p) else { continue };
        let group = match p.name.as_str() {
            "chatgpt" => "ChatGPT (your plan)".to_string(),
            n => format!("{n}{}", if p.local { " (local)" } else { "" }),
        };
        for mi in models {
            let v = crate::providers::display(&p.name, &mi.id);
            let price = if mi.price_in > 0.0 { format!(" · ${:.2}/${:.2} per M", mi.price_in, mi.price_out) } else { String::new() };
            out.push((v, format!("{}{}{price}", mi.id, k(mi.ctx)), group.clone()));
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

static KEYS_PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn run(hub: Arc<Hub>, tx: Sender<Ui>, rx: Receiver<Ui>) -> std::io::Result<()> {
    let mut term = ratatui::init();
    let _ = execute!(std::io::stdout(), event::EnableBracketedPaste, event::EnableFocusChange);
    if hub.cfg.mouse {
        let _ = execute!(std::io::stdout(), event::EnableMouseCapture);
    }
    std::thread::spawn(move || loop {
        // paused while a terminal step (team setup) owns stdin, or it would eat the user's keys
        if KEYS_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(30));
            continue;
        }
        match event::poll(std::time::Duration::from_millis(30)) {
            Ok(false) => continue,
            Ok(true) => match event::read() {
                Ok(ev) => if tx.send(Ui::Term(ev)).is_err() { break },
                Err(_) => break,
            },
            Err(_) => break,
        }
    });
    let mut app = App::blank(hub.clone());
    for n in hub.names() {
        app.load_view(&n);
    }
    if let Some(n) = hub.names().first() {
        app.set_active(n);
    }
    // presence comes back on in every team project you have tabs in; the one on screen last
    let mut done: Vec<std::path::PathBuf> = vec![];
    let active = app.active.clone();
    for n in hub.names().into_iter().filter(|n| *n != active).chain(std::iter::once(active.clone())) {
        let Some(cwd) = hub.get(&n).map(|m| m.cwd()) else { continue };
        let root = crate::teamview::project_root(&cwd);
        if done.contains(&root) || !crate::team::enabled_for(&cwd) { continue }
        done.push(root);
        match app.start_team_in(&n) {
            Ok(s) => app.note(&n, s),
            Err(e) => app.err(&n, format!("team presence: {e}")),
        }
    }
    // started outside every team project: the last one used stays connected in the background
    if done.is_empty() {
        if let Some(p) = recent_projects().into_iter().map(|(p, _)| p).find(|p| p.is_dir() && crate::team::enabled_for(p)) {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            match app.start_team_at(p.clone(), &active) {
                Ok(_) => { app.last_team = p; app.note(&active, format!("team {name} · Ctrl+] to open")) }
                Err(e) => app.err(&active, format!("team presence: {e}")),
            }
        }
    }
    app.project = std::path::PathBuf::new(); // so the team on screen follows the active tab
    app.set_active(&active);
    #[cfg(windows)]
    if crate::sandbox::enabled(&hub.cfg) {
        if let Some(s) = crate::sandbox_windows::offer_once() { app.note(&active, s) }
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
        if app.outside.as_ref().is_some_and(|s| s.first().is_some_and(|k| k == "edit")) {
            // /memory edit: the user's editor on one memory file, then back
            let path = std::path::PathBuf::from(app.outside.take().and_then(|mut s| s.pop()).unwrap_or_default());
            KEYS_PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80));
            let _ = execute!(std::io::stdout(), event::DisableBracketedPaste, event::DisableMouseCapture, event::DisableFocusChange);
            ratatui::restore();
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "vi".into() });
            let mut words = editor.split_whitespace();
            let ran = std::process::Command::new(words.next().unwrap_or("vi")).args(words).arg(&path).status();
            term = ratatui::init();
            KEYS_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = execute!(std::io::stdout(), event::EnableBracketedPaste, event::EnableFocusChange);
            if app.hub.cfg.mouse { let _ = execute!(std::io::stdout(), event::EnableMouseCapture); }
            let _ = term.clear();
            if let Some(d) = path.parent() { crate::memory::reindex(d) }
            let a = app.active.clone();
            match ran {
                Ok(_) => app.note(&a, format!("saved {}", path.display())),
                Err(e) => app.err(&a, format!("couldn't run {editor}: {e}")),
            }
        }
        if let Some(mut step) = app.outside.take() {
            let kind = if step.is_empty() { String::new() } else { step.remove(0) };
            // hand the terminal to the guided team setup, then take it back
            KEYS_PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(80)); // let an in-flight poll finish
            let _ = execute!(std::io::stdout(), event::DisableBracketedPaste, event::DisableMouseCapture, event::DisableFocusChange);
            ratatui::restore();
            let cwd = app.hub.get(&app.active).map(|m| m.cwd()).unwrap_or_default();
            let prev = std::env::current_dir().ok();
            let _ = std::env::set_current_dir(&cwd);
            println!("\n— qh team setup —\n");
            // no "press Enter" here: qh's key reader can swallow it and leave you stuck
            let res = if kind == "join" {
                crate::teamsetup::join_inner(step.first().map(String::as_str).unwrap_or(""), step.get(1).map(String::as_str)).map(|dir| {
                    // work in the downloaded project from now on
                    if let Some(m) = app.hub.get(&app.active) { *lock(&m.cwd) = dir; }
                    app.hub.save();
                })
            } else {
                crate::teamsetup::create_inner(&step)
            };
            let ok = res.is_ok();
            if let Some(p) = prev { let _ = std::env::set_current_dir(p); }
            term = ratatui::init();
            KEYS_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
            let _ = execute!(std::io::stdout(), event::EnableBracketedPaste, event::EnableFocusChange);
            if app.hub.cfg.mouse { let _ = execute!(std::io::stdout(), event::EnableMouseCapture); }
            let _ = term.clear();
            let a = app.active.clone();
            if let Err(e) = &res { app.err(&a, format!("team setup: {e}")) }
            if ok && !crate::team::running() {
                match app.start_team() {
                    Ok(s) => app.note(&a, s),
                    Err(e) => app.err(&a, format!("team mode: {e}")),
                }
            }
        }
        if app.quit {
            break Ok(());
        }
    };
    let _ = execute!(std::io::stdout(), event::DisableBracketedPaste, event::DisableMouseCapture, event::DisableFocusChange);
    ratatui::restore();
    crate::team::stop_and_flush(); // tell teammates we left
    res
}

pub fn suggest_rule(tool: &str, a: &Value, rules: &[String]) -> String {
    if tool == "network" {
        return format!("net:{}", a["host"].as_str().unwrap_or(""));
    }
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

/// `@path` attaches that file's contents (capped) to the message; `@login` of a teammate tells
/// the model who that is, what they're on, and how to hand them their part.
fn expand_mentions(line: &str) -> String {
    let mut out = line.to_string();
    let people = crate::team::people();
    let mut named: Vec<String> = vec![];
    for w in line.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
        let login = w.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'));
        if let Some((who, _, seen, sessions, role)) = people.iter().find(|p| p.0.eq_ignore_ascii_case(login)) {
            if !named.contains(who) {
                named.push(who.clone());
                out += &format!("\n\n<teammate login=\"{who}\" role=\"{role}\" status=\"{seen}\" sessions=\"{}\">\n\
{who} is another person on this team, with their own qh agent on their own machine. Work the user gives to {who} is theirs: \
don't do it yourself. Agree the shared parts first (API routes, data shapes, file layout), then send {who} their part with \
tell_team, and use ask_team for questions about what they own. {}\n</teammate>", sessions.join(","), crate::hub::TEAM_FILES);
            }
            continue;
        }
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
        "web_search" => format!("⌕ search {}", one(s("query"), 90)),
        "ask_model" => format!("⇆ ask {} {}", s("model"), one(s("question"), 80)),
        "fetch" => format!("⇣ fetch {}", one(s("goal"), 90)),
        "web_fetch" | "browse" => format!("↗ {tool} {}", clip(&s("url"), 90)),
        "click" => format!("◉ click [{}]", a["ref"]),
        "type" => format!("⌨ type [{}] \"{}\"{}", a["ref"], one(s("text"), 60),
                          if a["submit"].as_bool() == Some(true) { " ⏎" } else { "" }),
        "select" => format!("▾ select [{}] {}", a["ref"], s("option")),
        "scroll" => format!("↕ scroll {}", if s("direction").is_empty() { "down".into() } else { s("direction") }),
        "back" | "screenshot" | "snapshot" | "list_sessions" => format!("· {tool}"),
        "send_message" => format!("→ {}: {}", s("to"), one(s("text"), 90)),
        "tell_team" => format!("→ @{}: {}", s("to").trim_start_matches('@'), one(s("text"), 90)),
        "read_team_file" => format!("▤ @{}:{}", s("to").trim_start_matches('@'), s("path")),
        "qh_status" => "· status".to_string(),
        "list_team_files" => format!("▤ @{} changed files", s("to").trim_start_matches('@')),
        "ask" => format!("? {}: {}", s("to"), one(s("question"), 90)),
        "spawn" => format!("+ spawn {}: {}", s("name"), one(s("task"), 80)),
        "read_session" => format!("▤ read session {}", s("name")),
        _ => {
            let kv: Vec<String> = a.as_object().into_iter().flatten()
                .map(|(k, v)| format!("{k}={}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect();
            format!("• {tool} {}", one(kv.join(" "), 90))
        }
    }
}

/// Tabs and control characters have no cell width, so the terminal and ratatui disagree on
/// where text ends and stale characters stay on screen. Expand tabs, drop the rest.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
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
    fn blank(hub: Arc<Hub>) -> App {
        App {
            project: std::path::PathBuf::new(),
            hub, views: HashMap::new(), active: String::new(), grid: false, recent: vec![],
            log: VecDeque::new(), show_log: false, replies: false, input: vec![], cur: 0, chips: vec![], hist: vec![], hist_i: 0,
            confirms: VecDeque::new(), picker: None, secret: None, map: None,
            tab_hits: vec![], body: Rect::default(), picker_rows: None, quit: false, sugg_sel: 0, team_view: None, outside: None,
            last_personal: std::path::PathBuf::new(), last_team: std::path::PathBuf::new(), map_was_open: false, wt_hinted: false,
            notifier: Default::default(),
        }
    }

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
        let s = s.into();
        let v = self.view(name);
        // pressing a key again that only prints a note shouldn't stack copies
        if !matches!(v.items.back(), Some(Item::Note(last)) if *last == s) {
            v.push(Item::Note(s));
        }
    }

    fn err(&mut self, name: &str, s: impl Into<String>) {
        self.view(name).push(Item::Err(s.into()));
    }

    /// The tabs of the project on screen, in order.
    fn tabs(&self) -> Vec<String> {
        let all = self.hub.names();
        let mine: Vec<String> = all.iter().filter(|n| self.hub.get(n)
            .is_some_and(|m| crate::teamview::project_root(&m.cwd()) == self.project)).cloned().collect();
        if mine.is_empty() { all } else { mine }
    }

    fn set_active(&mut self, name: &str) {
        let Some(m) = self.hub.get(name) else { return };
        let root = crate::teamview::project_root(&m.cwd());
        if root != self.project {
            // the team room and status bar follow the project on screen
            if let Some((_, repo)) = crate::team::github_repo(&root) {
                if crate::team::running_for(&repo) { crate::team::set_current(&repo) } else { crate::team::set_current("") }
            } else {
                crate::team::set_current("");
            }
            remember_project(&root);
            if crate::team::enabled_for(&root) { self.last_team = root.clone() } else { self.last_personal = root.clone() }
            self.project = root;
        }
        self.active = name.to_string();
        self.recent.retain(|n| n != name);
        self.recent.insert(0, name.to_string());
        self.view(name).unread = false;
    }

    fn shown(&self) -> Vec<String> {
        let names = self.tabs();
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
                let cwd = self.hub.get(&self.active).map(|m| m.cwd()).unwrap_or_default();
                if self.secret.is_none() {
                    if let Some(p) = crate::image::path_in(&s, &cwd) {
                        let p = p.display().to_string();
                        let all = vec![(p.clone(), "attach it as an image".to_string()), (format!("--text {s}"), "paste the path as text".to_string())];
                        self.picker = Some(picker("that's an image file: attach it?", "/image", all));
                        return;
                    }
                }
                for c in s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }) {
                    self.input.insert(self.cur, c);
                    self.cur += 1;
                }
            }
            Ui::Term(TermEvent::FocusGained) => self.notifier.focused = Some(true),
            Ui::Term(TermEvent::FocusLost) => self.notifier.focused = Some(false),
            Ui::Term(_) => {}
        }
    }

    fn hub_event(&mut self, name: String, ev: Event) {
        if !name.is_empty() && name != self.active && !matches!(ev, Event::Changed | Event::Added | Event::Log(_)) {
            self.view(&name).unread = true;
        }
        let mode = crate::notify::mode(&self.hub.cfg.notify);
        let now = std::time::Instant::now();
        match &ev {
            Event::Token(_) | Event::ToolCall { .. } | Event::ToolResult { .. } => self.notifier.activity(&name, now),
            Event::Done { .. } if self.notifier.done(mode, &name, now) => crate::notify::send(mode, &format!("qh: {name} finished")),
            Event::Confirm { .. } if self.notifier.confirm(mode, &name, now) => crate::notify::send(mode, &format!("qh: {name} needs your OK")),
            _ => {}
        }
        // team events (requests, pings, direct messages, answers): one notification each
        for t in crate::team::take_alerts() {
            if self.notifier.team_event(mode) { crate::notify::send(mode, &t) }
        }
        match ev {
            Event::Token(t) => {
                let v = self.view(&name);
                // a /command typed meanwhile adds lines below the reply; the reply still grows in place
                let streaming = v.streaming;
                match v.items.iter_mut().rev().find(|i| matches!(i, Item::Reply(_))) {
                    Some(Item::Reply(s)) if streaming => s.push_str(&t),
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
                // with routing on, each step says which model handled it
                let mut title = tool_title(&tool, &args);
                if !self.hub.cfg.fetch_model.is_empty() {
                    let by = if tool == "fetch" { self.hub.cfg.fetch_model.clone() } else {
                        self.hub.get(&name).map(|m| { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) }).unwrap_or_default()
                    };
                    title = format!("[{by}] {title}");
                }
                let v = self.view(&name);
                v.streaming = false;
                // edits show their diff straight away (Ctrl+O folds it); other tools start folded
                let open = matches!(tool.as_str(), "edit_file" | "write_file");
                v.push(Item::Tool { title, args: a, result: String::new(), open });
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
                self.worktree_hint(&name);
            }
            Event::Verify { cmd, ok, secs, tail } => {
                let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
                let msg = if ok { format!("✓ check passed: {cmd} ({secs:.1} s)") } else { format!("✗ check failed: {cmd} ({secs:.1} s): {}", clip(last, 100)) };
                self.note(&name, msg)
            }
            Event::Compact { stage, before, after } => self.note(&name, format!("compacted ({stage}): ≈{before} → ≈{after} tokens")),
            Event::Notice(t) => {
                self.view(&name).streaming = false;
                self.note(&name, t)
            }
            Event::Done { .. } => self.view(&name).streaming = false,
            Event::Confirm { tool, args, reply, cwd, warn, guard } => {
                let rules = self.hub.get(&name).map(|m| lock(&m.set).allow.clone()).unwrap_or_default();
                let rule = if tool == "budget" { "budget".to_string() } else if tool == "review_hold" { "handoff".to_string() } else if guard && tool != "network" { String::new() } else { suggest_rule(&tool, &args, &rules) };
                self.confirms.push_back(Confirm { session: name, tool, args, rule, reply, cwd, warn });
            }
            Event::Added => {
                if !self.views.contains_key(&name) {
                    self.load_view(&name);
                }
            }
            Event::Log(s) => self.log_line(s),
            Event::Remembered { action, scope, name: what, .. } => {
                let verb = match action.as_str() { "forgot" => "forgot", "proposed" => "proposed to the team", "declined" => "team note declined", _ => "remembered" };
                self.note(&name, format!("{verb}: {what} ({scope}) · /memory"))
            }
            Event::Changed => {}
        }
    }

    /// Once per run: two plain tabs in one checkout have both edited files: suggest /worktree.
    fn worktree_hint(&mut self, name: &str) {
        if self.wt_hinted {
            return;
        }
        let edited = |n: &str| self.hub.get(n).filter(|m| !lock(&m.turn).changed.is_empty())
            .map(|m| m.cwd()).filter(|c| crate::worktree::info(c).is_none()).and_then(|c| crate::worktree::checkout(&c));
        let Some(root) = edited(name) else { return };
        if let Some(other) = self.hub.names().into_iter().find(|o| o != name && edited(o).as_ref() == Some(&root)) {
            self.wt_hinted = true;
            self.note(name, format!("{name} and {other} are both editing {}: /worktree gives this tab its own checkout and branch, /merge brings it back", root.display()));
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
                KeyCode::Char('a') if !c.rule.is_empty() && c.tool != "review_hold" => Some(Answer::Always(c.rule.clone())),
                KeyCode::Char('h') if c.tool == "review_hold" => Some(Answer::Always(c.rule.clone())),
                KeyCode::Char('n') | KeyCode::Esc => Some(Answer::No),
                _ => None,
            };
            if let Some(a) = ans {
                let c = self.confirms.pop_front().unwrap();
                let _ = c.reply.send(a);
            }
            return;
        }
        if let Some(tv) = self.team_view.as_mut() {
            let cwd = self.hub.get(&self.active).map(|m| m.cwd()).unwrap_or_default();
            if k.code == KeyCode::Char('q') && k.modifiers.contains(Mods::CONTROL) {
                self.quit = true;
                return;
            }
            // Ctrl+] too: input methods (fcitx, macOS input sources) often take Ctrl+Space.
            // Most terminals send Ctrl+] as 0x1D, which arrives as Ctrl+5.
            if matches!(k.code, KeyCode::Char(' ' | ']' | '5')) && k.modifiers.contains(Mods::CONTROL) {
                return self.flip_space();
            }
            // a session tab: Shift/Alt+←/→ leave the map for the sessions
            if k.modifiers.intersects(Mods::ALT | Mods::SHIFT) && matches!(k.code, KeyCode::Right | KeyCode::Left) {
                self.team_view = None;
                return;
            }
            let a = self.active.clone();
            match tv.key(k, &self.hub, &cwd) {
                crate::teamview::Act::None => {}
                crate::teamview::Act::Close => self.team_view = None,
                crate::teamview::Act::Open(s) => { self.team_view = None; self.set_active(&s) }
                crate::teamview::Act::Work(role, text) => {
                    self.team_view = None;
                    if let Some(m) = self.hub.get(&a) { self.hub.set_role(&m, &role) }
                    self.view(&a).push(Item::User(text.clone()));
                    self.hub.send(&a, &text, "user", 0, None);
                }
                crate::teamview::Act::Task(n, title) => {
                    self.team_view = None;
                    match self.start_task(&cwd, n, &title) {
                        Ok(name) => { self.load_view(&name); self.set_active(&name) }
                        Err(e) => self.err(&a, e),
                    }
                }
                crate::teamview::Act::Note(n) => self.note(&a, n),
                crate::teamview::Act::Err(e) => self.err(&a, e),
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
                    let cmd = p.cmd.clone();
                    self.picker = None;
                    if let Some(name) = pick {
                        self.command(&format!("{cmd} {name}"));
                    }
                }
                _ => {}
            }
            return;
        }
        let (ctrl, alt, shift) = (k.modifiers.contains(Mods::CONTROL), k.modifiers.contains(Mods::ALT), k.modifiers.contains(Mods::SHIFT));
        // `/` autocomplete: ↑↓ choose, Tab fills in, Enter runs the highlighted command
        let sugg = self.suggestions();
        if !sugg.is_empty() {
            match k.code {
                KeyCode::Up => { self.sugg_sel = self.sugg_sel.checked_sub(1).unwrap_or(sugg.len() - 1); return }
                KeyCode::Down => { self.sugg_sel = (self.sugg_sel + 1) % sugg.len(); return }
                KeyCode::Tab => {
                    let c = &sugg[self.sugg_sel.min(sugg.len() - 1)].0;
                    self.input = match c.split_once('<') { Some((head, _)) => head.to_string(), None => format!("{c} ") }.chars().collect();
                    self.cur = self.input.len();
                    self.sugg_sel = 0;
                    return;
                }
                // Enter runs what's highlighted, so what you see is what runs; one that still
                // needs something typed (`<who>`) is filled in instead
                KeyCode::Enter => {
                    let c = sugg[self.sugg_sel.min(sugg.len() - 1)].0.clone();
                    if let Some((head, _)) = c.split_once('<') {
                        self.input = head.chars().collect();
                        self.cur = self.input.len();
                        self.sugg_sel = 0;
                        return;
                    }
                    self.input = c.chars().collect();
                    self.cur = self.input.len();
                }
                _ => {}
            }
        }
        if matches!(k.code, KeyCode::Char(_) | KeyCode::Backspace) {
            self.sugg_sel = 0;
        }
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
            KeyCode::Char('t') if ctrl => self.open_map(),
            KeyCode::Char(' ' | ']' | '5') if ctrl => self.flip_space(),
            KeyCode::Char('p') if ctrl => self.command("/project"),
            KeyCode::BackTab => {
                let m = self.hub.cycle_mode();
                let a = self.active.clone();
                self.note(&a, format!("mode: {m} — {}", crate::hub::mode_help(m)));
            }
            KeyCode::Char('l') if ctrl => self.show_log = !self.show_log,
            KeyCode::Char('r') if ctrl => self.replies = !self.replies,
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
            KeyCode::Char('v') if ctrl && self.secret.is_none() => {
                let a = self.active.clone();
                match crate::image::clipboard() {
                    Some(b) => match crate::image::encode(&b) {
                        Ok(img) => self.chips.push(img),
                        Err(e) => self.err(&a, e),
                    },
                    None => self.note(&a, "no image on the clipboard (Linux needs wl-paste or xclip); /image <path> attaches a file"),
                }
            }
            KeyCode::Char('e') if ctrl => self.cur = self.input.len(),
            KeyCode::Char(d @ '1'..='9') if alt || ctrl => {
                if let Some(n) = self.tabs().get(d as usize - '1' as usize) {
                    let n = n.clone();
                    self.set_active(&n);
                }
            }
            // Shift works everywhere; macOS takes Ctrl+←/→ for desktops and Terminal turns Option+←/→ into Alt+b/f
            KeyCode::Left | KeyCode::Right if alt || ctrl || shift => self.cycle(if k.code == KeyCode::Left { -1 } else { 1 }),
            KeyCode::Char('b') if alt => self.cur = word_left(&self.input, self.cur),
            KeyCode::Char('f') if alt => self.cur = word_right(&self.input, self.cur),
            // no PgUp/PgDn on a Mac keyboard, and Terminal keeps Fn+↑/↓ for itself
            KeyCode::Up | KeyCode::Down if shift => {
                let a = self.active.clone();
                let v = self.view(&a);
                v.scroll = if k.code == KeyCode::Up { v.scroll + 3 } else { v.scroll.saturating_sub(3) };
            }
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
                    Err(e) => { let _ = crate::providers::remove_key(&name); format!("✗ {name}: {e} (not saved)") }
                };
                self.note(&a, msg);
            }
            KeyCode::Enter => {
                let line: String = self.input.drain(..).collect();
                self.cur = 0;
                let mut line = line.trim().to_string();
                if line.is_empty() && self.chips.is_empty() {
                    return;
                }
                if line.is_empty() {
                    line = "(see the attached image)".into();
                }
                self.hist.push(line.clone());
                self.hist_i = self.hist.len();
                let a = self.active.clone();
                self.view(&a).scroll = 0;
                if line.starts_with('/') {
                    // never echo an API key typed inline
                    let shown = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                        ["/provider" | "/providers", "add", name, _key, ..] => format!("/provider add {name} ••••••"),
                        _ => line.clone(),
                    };
                    self.view(&a).push(Item::Cmd(shown));
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
            KeyCode::Backspace if self.input.is_empty() && !self.chips.is_empty() => {
                self.chips.pop();
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
            // AltGr arrives as Ctrl+Alt; Alt alone is a shortcut, not text
            KeyCode::Char(c) if ctrl == alt => {
                self.input.insert(self.cur, c);
                self.cur += 1;
            }
            _ => {}
        }
    }

    fn cycle(&mut self, d: isize) {
        let names = self.tabs();
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
        if !self.chips.is_empty() {
            let imgs = std::mem::take(&mut self.chips);
            let (provider, model) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
            let n = imgs.len();
            crate::image::stage(&a, imgs);
            let what = if n == 1 { "1 image".to_string() } else { format!("{n} images") };
            if crate::image::likely_vision(&provider, &model) {
                self.note(&a, format!("+ {what}"));
            } else {
                self.note(&a, format!("+ {what} (sent anyway, but {model} may not read images)"));
            }
        }
        if lock(&m.ctl).running {
            self.note(&a, "queued: sent when the current turn ends");
        }
        self.hub.send(&a, &expand_mentions(line), "user", 0, None);
    }

    /// `/image <path>`; a secret path asks first, like a read_file of it would.
    fn attach(&mut self, a: &str, cwd: &std::path::Path, arg: &str) {
        if let Some(text) = arg.strip_prefix("--text ") {
            for c in text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }) {
                self.input.insert(self.cur, c);
                self.cur += 1;
            }
            return;
        }
        let (sure, arg) = match arg.strip_prefix("--yes ") { Some(r) => (true, r.trim()), None => (false, arg) };
        if arg.is_empty() {
            return self.note(a, "/image <path>  attach a PNG, JPEG, WebP or GIF (up to 5 MB) to your next message; Ctrl+V pastes one from the clipboard");
        }
        let p = crate::tools::resolve(cwd, arg.trim_matches(|c| c == '\'' || c == '"'));
        if !sure {
            if let Some(why) = crate::sandbox::guard("read_file", &serde_json::json!({"path": p.display().to_string()}), cwd, cwd, &self.hub.cfg) {
                // "no" first: a stray Enter must never send a key to a provider
                let all = vec![("--no".to_string(), "no, don't attach it".to_string()),
                               (format!("--yes {}", p.display()), format!("yes, send it to the model ({why})"))];
                self.picker = Some(picker("attach a secret file?", "/image", all));
                return;
            }
        }
        if arg == "--no" {
            return;
        }
        match crate::image::load(&p) {
            Ok(img) => self.chips.push(img),
            Err(e) => self.err(a, e),
        }
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
                let wt = arg.split_whitespace().any(|w| w == "--worktree");
                let mut p = arg.split_whitespace().filter(|w| *w != "--worktree");
                let auto;
                let n = match p.next() {
                    Some(n) => n,
                    None if wt => {
                        let names = self.hub.names();
                        let main = crate::worktree::info(&m.cwd()).map(|w| w.main).or_else(|| crate::worktree::checkout(&m.cwd()));
                        auto = (1..).map(|i| format!("wt{i}")).find(|n| !names.contains(n)
                            && !main.as_ref().is_some_and(|d| d.join(crate::worktree::DIR).join(n).exists())).unwrap();
                        &auto
                    }
                    None => return self.err(&a, "usage: /new <name> [profile]  ·  /new --worktree [name]"),
                };
                let mut cwd = m.cwd();
                if wt {
                    if self.hub.get(n).is_some() {
                        return self.err(&a, format!("a tab named {n} already exists"));
                    }
                    match crate::worktree::create(&cwd, n) {
                        Ok(w) => cwd = w.path,
                        Err(e) => return self.err(&a, e),
                    }
                }
                match self.hub.add(n, p.next(), "", "", None, false, Some(cwd.clone())) {
                    Ok(_) => {
                        if !self.views.contains_key(n) {
                            self.load_view(n);
                        }
                        self.set_active(n);
                        if wt {
                            self.note(n, format!("worktree {} on qh/{n}; /merge brings it back", cwd.display()));
                        }
                    }
                    Err(e) => self.err(&a, e),
                }
            }
            "/worktree" => {
                if let Some(w) = crate::worktree::info(&m.cwd()) {
                    return self.note(&a, format!("{a} already works in {} on {}", w.path.display(), w.branch()));
                }
                if busy {
                    return self.err(&a, "wait for this tab's turn to finish (Esc interrupts)");
                }
                let n = if arg.is_empty() { a.clone() } else { arg.to_string() };
                match crate::worktree::create(&m.cwd(), &n) {
                    Ok(w) => {
                        let msg = format!("{a} now works in {} on {}; /merge brings it back", w.path.display(), w.branch());
                        *lock(&m.cwd) = w.path;
                        self.hub.save();
                        self.note(&a, msg);
                    }
                    Err(e) => self.err(&a, e),
                }
            }
            "/merge" => {
                let Some(w) = crate::worktree::info(&m.cwd()) else {
                    return self.err(&a, "this tab isn't in a worktree (/new --worktree or /worktree)");
                };
                if busy {
                    return self.err(&a, "wait for this tab's turn to finish (Esc interrupts)");
                }
                match arg {
                    "" => match crate::worktree::summary(&w) {
                        Ok(s) => {
                            let base = crate::worktree::base(&w);
                            self.note(&a, s);
                            let all = vec![("confirm".to_string(), format!("yes: commit and merge {} into {base}", w.branch())),
                                           ("cancel".to_string(), "no, keep working".to_string())];
                            self.picker = Some(picker("merge this worktree?", "/merge", all));
                        }
                        Err(e) => self.err(&a, e),
                    },
                    "--discard" => {
                        let s = crate::worktree::summary(&w).unwrap_or_default();
                        self.note(&a, s);
                        let all = vec![("discard-confirm".to_string(), format!("yes: delete {} and branch {}", w.path.display(), w.branch())),
                                       ("cancel".to_string(), "no, keep it".to_string())];
                        self.picker = Some(picker("throw this worktree away?", "/merge", all));
                    }
                    "cancel" => {}
                    "confirm" => {
                        let last = self.views.get(&a).and_then(|v| v.items.iter().rev().find_map(|i| match i {
                            Item::Reply(t) => t.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| l.trim_start_matches('#').trim().to_string()),
                            _ => None,
                        }));
                        let msg = last.filter(|l| !l.is_empty()).map(|l| clip(&l, 72)).unwrap_or_else(|| format!("qh: {}", w.name));
                        match crate::worktree::merge(&w, &msg) {
                            Ok(r) => self.note(&a, r),
                            Err(e) => self.err(&a, e),
                        }
                    }
                    "discard-confirm" => match crate::worktree::discard(&w) {
                        Ok(r) => {
                            *lock(&m.cwd) = w.main.clone();
                            self.hub.save();
                            self.note(&a, format!("{r}\n{a} now works in {}", w.main.display()));
                        }
                        Err(e) => self.err(&a, e),
                    },
                    _ => self.err(&a, "usage: /merge  ·  /merge --discard"),
                }
            }
            "/close" => {
                if self.hub.names().len() == 1 {
                    return self.err(&a, "can't close the last session; Ctrl+Q quits");
                }
                let names = self.tabs();
                let i = names.iter().position(|n| *n == a).unwrap_or(0);
                self.hub.close(&a);
                self.views.remove(&a);
                self.recent.retain(|n| *n != a);
                let names = self.tabs();
                let next = self.recent.iter().find(|n| names.contains(n)).cloned()
                    .unwrap_or_else(|| names[i.min(names.len().saturating_sub(1))].clone());
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
                match arg {
                    "" => s.think = !s.think,
                    "on" => (s.think, s.effort) = (true, String::new()),
                    "off" => s.think = false,
                    "low" | "medium" | "high" => (s.think, s.effort) = (true, arg.to_string()),
                    _ => { drop(s); self.err(&a, "usage: /think [on|off|low|medium|high]"); return; }
                }
                let msg = match (s.think, s.effort.as_str()) {
                    (false, _) => "thinking off".to_string(),
                    (true, "") => "thinking on (the model's default effort)".to_string(),
                    (true, e) => format!("thinking on, {e} effort (models without levels just think)"),
                };
                drop(s);
                self.note(&a, msg);
            }
            "/tokens" => {
                self.note(&a, self.hub.tokens_text(&m));
            }
            "/ask-other" => {
                let (model, q) = crate::route::parse_ask_other(arg);
                let ctx = crate::route::subject(&m.cwd(), &lock(&m.msgs));
                if ctx.is_empty() && q.is_empty() {
                    return self.err(&a, "nothing to review yet: no diff and no answer. Usage: /ask-other [model] [question]");
                }
                self.note(&a, format!("asking {} for a second opinion…", model.as_deref().unwrap_or("the second model")));
                let (hub, m) = (self.hub.clone(), m.clone());
                std::thread::spawn(move || {
                    let r = engine::ask_other(&hub, &m, model.as_deref().unwrap_or(""), &q, &ctx);
                    hub.emit(&m.name, Event::Notice(r));
                    hub.changed();
                });
            }
            "/compact" if busy =>self.err(&a, "busy: wait for the turn to end or press Esc"),
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
                    let exact = all.iter().any(|(v, _, _)| v == arg);
                    let words: Vec<String> = arg.to_lowercase().split_whitespace().map(String::from).collect();
                    let hits: Vec<&(String, String, String)> = all.iter()
                        .filter(|(v, _, _)| words.iter().all(|w| v.to_lowercase().contains(w.as_str()))).collect();
                    if !exact && (arg.is_empty() || hits.len() > 1) {
                        let cur = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
                        let sel = if arg.is_empty() { all.iter().position(|(v, _, _)| *v == cur).unwrap_or(0) } else { 0 };
                        self.picker = Some(Picker { title: "model", cmd: "/model".into(), all, filter: arg.to_string(), sel, cur });
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
                    (Some("connect"), Some(name), _) => {
                        let Some(p) = crate::providers::get(name) else { return self.err(&a, format!("no provider {name}")) };
                        if p.kind == crate::providers::Kind::ChatGPT {
                            if crate::chatgpt::signed_in() {
                                let all = vec![("test chatgpt".to_string(), "test the sign-in (lists your plan's models)".to_string()),
                                               ("signin chatgpt".to_string(), "sign in again".to_string()),
                                               ("signout chatgpt".to_string(), "sign out".to_string())];
                                self.picker = Some(picker("ChatGPT", "/provider", all));
                            } else {
                                self.command("/provider signin chatgpt");
                            }
                        } else if p.local {
                            self.command(&format!("/provider test {name}"));
                        } else if p.key.is_empty() {
                            self.command(&format!("/provider add {name}"));
                        } else {
                            let all = vec![(format!("test {name}"), "test the key".to_string()),
                                           (format!("add {name}"), "replace the key".to_string()),
                                           (format!("remove {name}"), "remove the key".to_string())];
                            self.picker = Some(picker("provider", "/provider", all));
                        }
                    }
                    (Some("signin"), Some("chatgpt"), _) => {
                        // the browser round trip blocks, so it runs off the UI thread and reports back as notes
                        let (hub, a2) = (self.hub.clone(), a.clone());
                        self.note(&a, "signing in with ChatGPT…");
                        std::thread::spawn(move || {
                            let say = |s: &str| hub.emit(&a2, Event::Notice(s.to_string()));
                            let r = crate::chatgpt::login(&say);
                            crate::providers::init(&crate::config::load());
                            say(&match r {
                                Ok(e) => format!("✓ signed in with ChatGPT as {e}. /model chatgpt to pick a model"),
                                Err(e) => format!("✗ ChatGPT sign-in: {e}"),
                            });
                        });
                    }
                    (Some("signout"), Some("chatgpt"), _) => {
                        crate::chatgpt::sign_out();
                        crate::providers::init(&crate::config::load());
                        self.note(&a, "signed out of ChatGPT");
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
                        let all: Vec<(String, String)> = crate::providers::all().iter().map(|p| {
                            let state = match (p.kind == crate::providers::Kind::ChatGPT, p.local, p.key.is_empty()) {
                                (true, _, true) => "sign in with your ChatGPT plan".to_string(),
                                (true, _, false) => format!("✓ signed in {}", crate::chatgpt::account().unwrap_or_default()),
                                (_, true, _) => "local".into(),
                                (_, _, true) => "add a key".into(),
                                _ => "✓ key set".into(),
                            };
                            (p.name.clone(), format!("{:<14}{state}", p.name))
                        }).collect();
                        self.picker = Some(picker("providers", "/provider connect", all));
                    }
                }
            }
            "/map" => self.map = Some(crate::map::Map::default()),
            "/tier" => {
                let (provider, model) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
                let shown = crate::providers::display(&provider, &model);
                if crate::providers::is_local(&provider, &model) {
                    let ctx = self.hub.cfg.ctx;
                    let src = if self.hub.cfg.ctx_set { "set in config" } else { "sized to the GPU at start" };
                    let live = match (crate::tier::vram(), crate::tier::detect(&self.hub.cfg.host, &model)) {
                        (_, Some(d)) => d.why,
                        (None, None) => crate::tier::NO_GPU.into(),
                        (Some(_), None) => format!("Ollama didn't describe {model} (is it running, and is the model pulled?)"),
                    };
                    self.note(&a, format!("{shown}: {}k window (tier {}, {src})\nright now: {live}", ctx / 1024, crate::tier::tier(ctx)));
                } else {
                    // a cloud model: its own window, unless cloud_ctx caps it
                    let (ctx, _) = self.hub.ctx_for(&m, true);
                    let cap = self.hub.cfg.cloud_ctx;
                    let why = match crate::providers::info(&provider, &model).and_then(|i| i.ctx) {
                        Some(k) if cap > 0 && k > cap => format!("{shown} takes {}k; qh keeps {}k (cloud_ctx in config.toml), since every request resends the whole conversation", k / 1024, cap / 1024),
                        Some(_) => format!("{shown}'s whole window; every request resends the conversation, so long sessions use more of your plan (cap it with cloud_ctx)"),
                        None => format!("{} doesn't publish {shown}'s window, so qh assumes 32k", provider),
                    };
                    self.note(&a, format!("{shown}: {}k window (tier {}), a cloud model: no GPU needed\n{why}", ctx / 1024, crate::tier::tier(ctx)));
                }
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
                // a web address (localhost:3000, http://127.0.0.1:8080/x, dev.lan) is for the browser
                let rules = crate::browser::allow_rules(arg).unwrap_or_else(|| vec![arg.to_string()]);
                if let Some(first) = rules.first().and_then(|r| r.strip_prefix("browse:")) {
                    self.note(&a, format!("the browser may open {first} in this tab's session"));
                }
                for r in rules.into_iter().filter(|r| !r.is_empty()) {
                    if !s.allow.contains(&r) { s.allow.push(r) }
                }
                let msg = format!("auto-approve: {}", if s.allow.is_empty() { "none".into() } else { s.allow.join(", ") });
                drop(s);
                self.note(&a, msg);
            }
            "/memory" => {
                if !self.hub.cfg.memory {
                    return self.note(&a, "memory is off (memory = false in config.toml)");
                }
                let team = crate::team::running_here(&m.root).then(|| crate::team::team_notes(&m.root)).flatten();
                let (sub, rest) = arg.split_once(' ').map(|(x, y)| (x, y.trim())).unwrap_or((arg, ""));
                match sub {
                    "" | "list" => self.note(&a, crate::memory::overview(&m.root, team.as_deref())),
                    "open" | "edit" | "forget" => {
                        let Some(mem) = crate::memory::locate(&m.root, rest) else {
                            return self.err(&a, format!("no memory named {rest:?}; /memory lists them"));
                        };
                        match sub {
                            "open" => self.note(&a, format!("{}\n{}", mem.file.display(), std::fs::read_to_string(&mem.file).unwrap_or_default())),
                            "edit" => self.outside = Some(vec!["edit".into(), mem.file.to_string_lossy().into()]),
                            _ => match crate::memory::forget(mem.file.parent().unwrap_or(std::path::Path::new(".")), &mem.name) {
                                Ok(_) => self.note(&a, format!("forgot: {}", mem.name)),
                                Err(e) => self.err(&a, e),
                            },
                        }
                    }
                    _ => self.err(&a, "usage: /memory [open|edit|forget <name>]"),
                }
            }
            "/tasks" => {
                let d = match arg.strip_prefix("stop").map(str::trim) {
                    Some(id) if !id.is_empty() => crate::bg::stop(&m.name, id).unwrap_or_else(|e| e),
                    Some(_) => return self.err(&a, "usage: /tasks stop <id>"),
                    None => crate::bg::list(&m.name),
                };
                self.note(&a, d);
            }
            #[cfg(windows)]
            "/sandbox" if matches!(arg, "setup" | "remove") => {
                self.note(&a, if arg == "setup" { "setting up the sandbox users: Windows asks for admin…" } else { "removing the sandbox users: Windows asks for admin…" });
                match if arg == "setup" { crate::sandbox_windows::setup() } else { crate::sandbox_windows::remove() } {
                    Ok(s) => self.note(&a, s),
                    Err(e) => self.err(&a, format!("sandbox {arg}: {e}")),
                }
            }
            "/sandbox" => {
                let d = crate::sandbox::describe(&m.sandbox_now(), &self.hub.cfg);
                self.note(&a, d);
            }
            "/sessions" => self.note(&a, session::list(10)),
            "/image" => self.attach(&a, &m.cwd(), arg),
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
            "/relay" => match arg {
                "" => {
                    let now = crate::config::load().relay;
                    let dflt = match crate::link::DEFAULT_RELAY {
                        Some(_) => "default  the qh project's relay: works with no setup".to_string(),
                        None => "default  the qh project's relay (none running yet: same as none)".to_string(),
                    };
                    let all = vec![
                        ("default".to_string(), dflt),
                        ("none".to_string(), "none     direct lines and teammates only; strict networks fall back to git".to_string()),
                        ("custom".to_string(), "your own the line `qh relay` printed on your server".to_string()),
                    ];
                    let mut p = picker("relay server", "/relay", all);
                    p.cur = if ["default", "none"].contains(&now.as_str()) { now } else { "custom".into() };
                    self.picker = Some(p);
                }
                "custom" => {
                    self.input = "/relay ".chars().collect();
                    self.cur = self.input.len();
                    self.note(&a, "paste host:port#key from `qh relay` on your server (several: comma-separated), then Enter");
                }
                v => {
                    let ok = matches!(v, "default" | "none") || v.split(',').all(|spec| {
                        let (host, key) = spec.trim().split_once('#').map_or((spec.trim(), None), |(h, k)| (h, Some(k)));
                        !host.is_empty() && !host.contains(' ') && key.is_none_or(|k| crate::oauth::b64url_decode(k).len() == 32)
                    });
                    if !ok {
                        return self.err(&a, "that doesn't look like a relay: host:port#key (the key is the 43 characters `qh relay` printed)");
                    }
                    match crate::config::set("relay", v) {
                        Ok(path) => {
                            crate::team::set_relays(v);
                            self.note(&a, format!("relay: {v} (saved in {}; teammates learn it with your addresses)", path.display()))
                        }
                        Err(e) => self.err(&a, format!("couldn't save the setting: {e}")),
                    }
                }
            },
            "/project" | "/projects" => {
                if let Some(p) = arg.strip_prefix("browse ") {
                    self.picker = Some(project_browser(std::path::Path::new(p)));
                } else if let Some(p) = arg.strip_prefix("open ") {
                    let p = tools::resolve(&m.cwd(), p.trim());
                    if !p.is_dir() { return self.err(&a, format!("no such folder: {}", p.display())) }
                    self.open_project(&p);
                } else {
                    self.picker = Some(self.project_picker());
                }
            }
            "/anyway" => {
                crate::team::allow_anyway(&a);
                self.note(&a, "going ahead: this tab may edit what teammates are changing for the next 10 minutes (they'll see a note)");
                if m.status() == "idle" {
                    let text = "The user says go ahead anyway: retry the edit you were paused on.";
                    self.view(&a).push(Item::User(text.into()));
                    self.hub.send(&a, text, "user", 0, None);
                }
            }
            "/review-request" => {
                // the owners' agents answer within a minute or so: wait off the UI thread, report as notes
                let (hub, a2, cwd, wait) = (self.hub.clone(), a.clone(), m.cwd(), self.hub.cfg.review_wait.max(60));
                match crate::review::request(&a, &cwd) {
                    Err(e) => self.err(&a, e),
                    Ok((ids, line)) => {
                        self.note(&a, format!("{line} · their answers show here and in the team room"));
                        std::thread::spawn(move || {
                            let got = crate::review::wait_for(&ids, std::time::Duration::from_secs(wait), &|| false);
                            hub.emit(&a2, Event::Notice(crate::review::summary(&got)));
                        });
                    }
                }
            }
            "/promise" => match crate::team::promise(&a, arg) {
                Ok(t) => self.note(&a, t),
                Err(e) => self.err(&a, e),
            },
            "/dm" => match arg.split_once(' ') {
                Some((who, text)) if !text.trim().is_empty() => match crate::team::dm_send(who, text.trim()) {
                    Ok(m) => self.note(&a, m),
                    Err(e) => self.err(&a, e),
                },
                _ => self.err(&a, "usage: /dm <login> <text> (private: only the two of you can read it)"),
            },
            "/answers" if arg.is_empty() => match crate::team::answers_mode() {
                Some(mode) => self.note(&a, format!("teammates' agents asking yours: {mode} · /answers auto|ask|off")),
                None => self.err(&a, "team mode is off"),
            },
            "/answers" => match crate::team::set_answers(arg) {
                Ok(t) => self.note(&a, t),
                Err(e) => self.err(&a, e),
            },
            "/update" => {
                // the download blocks, so it runs off the UI thread and reports back as notes
                let (hub, a2) = (self.hub.clone(), a.clone());
                self.note(&a, format!("looking for a newer qh (this is {})…", crate::update::VERSION));
                std::thread::spawn(move || {
                    let say = |s: &str| hub.emit(&a2, Event::Notice(s.to_string()));
                    say(&match crate::update::run(&say) {
                        Ok(m) => m,
                        Err(e) => format!("✗ update: {e}"),
                    });
                });
            }
            "/docs" => {
                crate::platform::open_url(crate::platform::DOCS);
                self.note(&a, format!("opening the docs: {}", crate::platform::DOCS));
            }
            "/team" => match arg {
                "on" if crate::team::github_repo(&m.cwd()).is_none() => self.note(&a,
                    "This folder isn't on GitHub yet, so there's nowhere to meet your team.\n\
Type /team create to put it on GitHub (private) and invite friends by their GitHub username."),
                "on" => match self.start_team() {
                    Ok(s) => self.note(&a, s),
                    Err(e) => self.err(&a, e),
                },
                "private" => match crate::teamsetup::set_visibility(&m.cwd(), true) {
                    Ok(s) => self.note(&a, s),
                    Err(e) => self.err(&a, e),
                },
                "public" => {
                    // "no" first: a stray Enter must never publish someone's code
                    let all = vec![("status".to_string(), "no, keep it as it is".to_string()),
                                   ("public confirm".to_string(), "yes, make it public: anyone on the internet can see the code".to_string())];
                    self.picker = Some(picker("make this repo public?", "/team", all));
                }
                "public confirm" => match crate::teamsetup::set_visibility(&m.cwd(), false) {
                    Ok(s) => self.note(&a, s),
                    Err(e) => self.err(&a, e),
                },
                "remove" => self.err(&a, "usage: /team remove <github-username>"),
                _ if arg.starts_with("remove confirm ") => {
                    let who = arg[15..].trim().trim_start_matches('@').to_string();
                    let Some((_, repo)) = crate::team::github_repo(&m.cwd()) else {
                        return self.err(&a, "this folder isn't on GitHub, so it has no team");
                    };
                    let (hub, a2) = (self.hub.clone(), a.clone());
                    self.note(&a, format!("removing {who}…"));
                    std::thread::spawn(move || {
                        let r = crate::team::remove(&repo, &who).unwrap_or_else(|e| format!("✗ {e}"));
                        hub.emit(&a2, Event::Notice(r));
                    });
                }
                _ if arg.starts_with("remove ") => {
                    // "no" first: a stray Enter must never throw someone out
                    let who = arg[7..].trim().trim_start_matches('@').to_string();
                    let all = vec![("status".to_string(), "no, keep them".to_string()),
                                   (format!("remove confirm {who}"), format!("yes: new team key without {who}, and remove their access to the repo"))];
                    self.picker = Some(picker("remove them from the team?", "/team", all));
                }
                "invite" => self.err(&a, "usage: /team invite <github-username> [more…] [viewer|member|trusted]"),
                _ if arg.starts_with("invite ") => {
                    let mut users: Vec<String> = arg[7..].split_whitespace().map(|u| u.trim_start_matches('@').to_string()).collect();
                    let role = match users.last() {
                        Some(r) if crate::team::ROLES.contains(&r.as_str()) => users.pop().unwrap(),
                        _ => "member".to_string(),
                    };
                    if users.is_empty() {
                        return self.err(&a, "usage: /team invite <github-username> [more…] [viewer|member|trusted]");
                    }
                    let (hub, a2, cwd) = (self.hub.clone(), a.clone(), m.cwd());
                    self.note(&a, format!("inviting {} as {role}…", users.join(", ")));
                    std::thread::spawn(move || {
                        let mut r = crate::teamsetup::invite(&cwd, &users).unwrap_or_else(|e| format!("✗ {e}"));
                        if let Some((_, repo)) = crate::team::github_repo(&cwd) {
                            for u in &users {
                                if let Ok(m) = crate::team::set_role(&repo, u, &role) { r = format!("{r}\n{m}") }
                            }
                        }
                        hub.emit(&a2, Event::Notice(r));
                    });
                }
                "role" => {
                    // pick a person, then a role
                    let all: Vec<(String, String)> = crate::team::sessions().into_iter().map(|(who, _, _)| who)
                        .collect::<std::collections::BTreeSet<_>>().into_iter()
                        .map(|w| (format!("role {w}"), w)).collect();
                    if all.is_empty() {
                        return self.err(&a, "usage: /team role <github-username> viewer|member|trusted");
                    }
                    self.picker = Some(picker("whose role?", "/team", all));
                }
                _ if arg.starts_with("role ") => {
                    let w: Vec<&str> = arg[5..].split_whitespace().collect();
                    let Some((_, repo)) = crate::team::github_repo(&m.cwd()) else {
                        return self.err(&a, "this folder isn't on GitHub, so it has no team");
                    };
                    match w.as_slice() {
                        [who] => {
                            let all = crate::team::ROLES.iter().map(|r| (format!("role {who} {r}"), format!("{r:<8} {}", crate::team::role_help(r)))).collect();
                            self.picker = Some(picker("role", "/team", all));
                        }
                        [who, role] => match crate::team::set_role(&repo, who.trim_start_matches('@'), role) {
                            Ok(msg) => self.note(&a, msg),
                            Err(e) => self.err(&a, e),
                        },
                        _ => self.err(&a, "usage: /team role <github-username> viewer|member|trusted"),
                    }
                }
                // folder browser: start the team in an existing folder or a new one
                "create" => {
                    let start = m.cwd();
                    let start = if crate::teamsetup::not_a_project(&start).is_ok() { start } else { crate::platform::home() };
                    self.picker = Some(folder_picker(&start));
                }
                _ if arg.starts_with("browse ") => self.picker = Some(folder_picker(std::path::Path::new(&arg[7..]))),
                _ if arg.starts_with("new-in ") => {
                    // picked "new folder": the user types the name after the path
                    self.input = format!("/team create-new {}{}", arg[7..].trim_end_matches(['/', '\\']), std::path::MAIN_SEPARATOR).chars().collect();
                    self.cur = self.input.len();
                    self.note(&a, "type the new folder's name, then Enter");
                }
                _ if arg.starts_with("create-new ") || arg.starts_with("create-at ") => {
                    let (new, path) = if let Some(p) = arg.strip_prefix("create-new ") { (true, p) } else { (false, &arg[10..]) };
                    let p = tools::resolve(&m.cwd(), path.trim());
                    if new {
                        if p.exists() {
                            return self.err(&a, format!("{} already exists: pick it from /team create instead", p.display()));
                        }
                        if let Err(e) = std::fs::create_dir_all(&p) {
                            return self.err(&a, format!("couldn't make {}: {e}", p.display()));
                        }
                    }
                    if let Err(e) = crate::teamsetup::not_a_project(&p) {
                        return self.err(&a, e);
                    }
                    *lock(&m.cwd) = p.clone();
                    self.hub.save();
                    self.note(&a, format!("{a} now works in {}", p.display()));
                    self.outside = Some(vec!["create".into()]);
                }
                _ if arg.starts_with("create ") => {
                    self.outside = Some(std::iter::once("create".to_string()).chain(arg.split_whitespace().skip(1).map(String::from)).collect());
                }
                "off" => {
                    if let Some((_, repo)) = crate::team::github_repo(&m.cwd()) { crate::team::remember(&repo, false) }
                    crate::team::stop();
                    self.note(&a, "team presence off (teammates see you go offline)");
                }
                "status" => self.note(&a, crate::team::report()),
                _ if arg.starts_with("open ") => {
                    let mut w = arg[5..].split_whitespace();
                    match (w.next(), w.next()) {
                        (Some(who), Some(s)) => match crate::team::transcript(who, s) {
                            Ok(t) => self.note(&a, t),
                            Err(e) => self.err(&a, e),
                        },
                        _ => self.err(&a, "usage: /team open <who> <session>"),
                    }
                }
                _ if arg.starts_with("send ") => {
                    let mut w = arg[5..].splitn(3, ' ');
                    match (w.next(), w.next(), w.next()) {
                        (Some(who), Some(s), Some(text)) if !text.trim().is_empty() => match crate::team::send(who, s, text.trim()) {
                            Ok(m) => self.note(&a, m),
                            Err(e) => self.err(&a, e),
                        },
                        (Some(who), Some(s), _) => {
                            // picked from the list: put the command in the input box to type the message
                            self.input = format!("/team send {who} {s} ").chars().collect();
                            self.cur = self.input.len();
                        }
                        _ => self.err(&a, "usage: /team send <who> <session> <text>"),
                    }
                }
                "group" => {
                    let g = crate::team::groups();
                    self.note(&a, if g.is_empty() { "no groups yet: /team group <name> <login> <login>…".to_string() } else {
                        g.iter().map(|(n, m)| format!("+ {n}: {}", m.join(", "))).collect::<Vec<_>>().join("\n")
                    });
                }
                _ if arg.starts_with("group ") => {
                    let w: Vec<String> = arg[6..].split_whitespace().map(String::from).collect();
                    match w.split_first() {
                        Some((name, logins)) if !logins.is_empty() => match crate::team::group_create(name, logins) {
                            Ok(m) => self.note(&a, format!("{m}. Its channel is in the team room's Chat tab (Ctrl+T)")),
                            Err(e) => self.err(&a, e),
                        },
                        _ => self.err(&a, "usage: /team group <name> <login> <login>…"),
                    }
                }
                _ if arg.starts_with("answer ") => {
                    let w: Vec<&str> = arg.split_whitespace().collect();
                    let (Some(from), Some(id), Some(ans)) = (w.get(1), w.get(2).and_then(|x| x.parse::<u64>().ok()), w.get(3)) else {
                        return self.err(&a, "usage: /team answer <who> <id> once|always|deny");
                    };
                    match crate::team::decide(from, id, ans) {
                        Some((session, note)) if session.is_empty() => self.note(&a, note),
                        Some((session, text)) => {
                            let to = if self.hub.get(&session).is_some() { session } else { a.clone() };
                            self.hub.send(&to, &text, "user", 0, None);
                            self.note(&a, format!("accepted: delivered to {to}"));
                        }
                        None if *ans == "deny" => self.note(&a, format!("declined {from}'s request")),
                        None => self.err(&a, "that message isn't waiting any more"),
                    }
                }
                "join" => {
                    self.input = "/team join ".chars().collect();
                    self.cur = self.input.len();
                    self.note(&a, "paste the line your friend sent (owner/repo and the code), then Enter");
                }
                _ if arg.starts_with("join ") => self.outside = Some(std::iter::once("join".to_string()).chain(arg[5..].split_whitespace().map(String::from)).collect()),
                "invite-ask" => {
                    self.input = "/team invite ".chars().collect();
                    self.cur = self.input.len();
                    self.note(&a, "type your friend's GitHub username, then Enter");
                }
                "overview" => self.team_view = Some(crate::teamview::TeamView::open(&m.cwd())),
                "settings" => {
                    let all = vec![
                        ("private".to_string(), "Only the team can see the code".to_string()),
                        ("public".to_string(), "Everyone can see the code (asks first)".to_string()),
                        ("off".to_string(), "Go offline".to_string()),
                    ];
                    self.picker = Some(picker("team settings", "/team", all));
                }
                _ if arg.starts_with("person ") => {
                    let who = arg[7..].trim();
                    let Some((_, _, _, sessions, role)) = crate::team::people().into_iter().find(|p| p.0 == who) else {
                        return self.err(&a, format!("no teammate {who}"));
                    };
                    let mut all: Vec<(String, String, String)> = vec![];
                    for s in &sessions {
                        all.push((format!("open {who} {s}"), format!("Read {s}"), String::new()));
                    }
                    for s in &sessions {
                        all.push((format!("send {who} {s}"), format!("Send a message to {s}"), String::new()));
                    }
                    if sessions.is_empty() {
                        all.push(("status".into(), format!("{who} has no sessions open"), String::new()));
                    }
                    all.push(if role == "viewer" {
                        (format!("role {who} member"), format!("Let {who} send me messages"), String::new())
                    } else {
                        (format!("role {who} viewer"), format!("View only: {who} can't send me messages"), String::new())
                    });
                    self.picker = Some(Picker { title: "teammate", cmd: "/team".into(), all, filter: String::new(), sel: 0, cur: String::new() });
                }
                _ => {
                    // one menu for everything; what's in it depends on where you are
                    let mut all: Vec<(String, String, String)> = vec![];
                    if !crate::team::running() {
                        let cwd = m.cwd();
                        if crate::team::github_repo(&cwd).is_some() {
                            all.push(("on".into(), "Connect to this project's team".into(), String::new()));
                        } else if crate::teamsetup::not_a_project(&cwd).is_ok() {
                            all.push((format!("create-at {}", cwd.display()), "Start a team in this folder".into(), String::new()));
                        }
                        all.push(("create".into(), "Start a team in another folder…".into(), String::new()));
                        all.push(("join".into(), "Join a friend's team…".into(), String::new()));
                        self.picker = Some(picker_g("team", all));
                        return;
                    }
                    // in a team: everything happens in the room
                    if arg.is_empty() {
                        self.team_view = Some(crate::teamview::TeamView::open(&m.cwd()));
                        return;
                    }
                    for i in crate::team::pending() {
                        if i.question.is_some() {
                            let g = format!("{}'s agent asks your {}: \"{}\"", i.from, i.session, clip(&i.text, 50));
                            all.push((format!("answer {} {} once", i.from, i.id), "Answer (read-only copy)".into(), g.clone()));
                            all.push((format!("answer {} {} always", i.from, i.id), format!("Always answer {}'s questions", i.from), g.clone()));
                            all.push((format!("answer {} {} deny", i.from, i.id), "Decline".into(), g));
                        } else {
                            let g = format!("{} requests work in your {}: \"{}\"", i.from, i.session, clip(&i.text, 50));
                            all.push((format!("answer {} {} once", i.from, i.id), "Accept: it goes into your session".into(), g.clone()));
                            all.push((format!("answer {} {} deny", i.from, i.id), "Decline (they're told)".into(), g));
                        }
                    }
                    let team = format!("team · {}", crate::team::repo().unwrap_or_default());
                    for (who, online, seen, sessions, role) in crate::team::people() {
                        let n = sessions.len();
                        all.push((format!("person {who}"), format!("{} {who}   {seen} · {n} session{}{}", if online { "●" } else { "○" },
                            if n == 1 { "" } else { "s" }, if role == "viewer" { " · view only" } else { "" }), team.clone()));
                    }
                    if crate::team::people().is_empty() {
                        all.push(("status".into(), "Nobody else is here yet: invite a friend".into(), team.clone()));
                    }
                    all.push(("overview".into(), "Overview: who's working on what (Ctrl+T)".into(), team.clone()));
                    all.push(("invite-ask".into(), "+ Invite a friend".into(), team.clone()));
                    all.push(("settings".into(), "Settings (who sees the code, go offline)".into(), team));
                    self.picker = Some(picker_g("team", all));
                }
            },
            "/commands" => {
                let list = custom_commands(&m.cwd());
                self.note(&a, if list.is_empty() {
                    "no custom commands: add <name>.md to .qh/commands/ (project) or ~/.config/qh/commands/; \
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
                    self.hub.send(&a, &expand_mentions(text.trim()), "user", 0, None);
                }
                None => self.err(&a, format!("unknown command {cmd}, try /help or /commands")),
            },
        }
    }

    // ---- drawing ----------------------------------------------------------

    /// Commands matching what's typed, while a bare `/word` is being typed.
    fn suggestions(&self) -> Vec<(String, String)> {
        let typed: String = self.input.iter().collect();
        if self.secret.is_some() || !typed.starts_with('/') {
            return vec![];
        }
        // after a command (or on an exact one): its subcommands, until you type past the word
        let cmd = typed.split(' ').next().unwrap_or("");
        let cmd = if cmd == "/providers" { "/provider" } else if cmd == "/cd" { "/cwd" } else { cmd };
        let rest = typed.split_once(' ').map(|(_, r)| r).unwrap_or("");
        let subs: Vec<(String, String)> = SUBCOMMANDS.iter()
            .filter(|(c, _)| {
                let (head, tail) = c.split_once(' ').unwrap_or((c, ""));
                let fixed = tail.split('<').next().unwrap_or("");
                head == cmd && fixed.starts_with(rest) && !(rest == fixed && tail.len() > fixed.len())
            })
            .map(|(c, d)| (c.to_string(), d.to_string())).collect();
        if typed.contains(' ') {
            // what's typed exactly, highlighted first: Enter on `/mode auto` runs auto, not auto-edit
            let mut subs = subs;
            subs.sort_by_key(|(c, _)| *c != typed);
            return subs;
        }
        let cwd = self.hub.get(&self.active).map(|m| m.cwd()).unwrap_or_default();
        let mut out: Vec<(String, String)> = COMMANDS.iter().filter(|(c, _)| c.starts_with(&typed))
            .map(|(c, d)| (c.to_string(), d.to_string())).collect();
        for (n, p) in custom_commands(&cwd) {
            let c = format!("/{n}");
            if c.starts_with(&typed) && !out.iter().any(|(x, _)| *x == c) {
                out.push((c, format!("custom · {}", p.file_name().unwrap_or_default().to_string_lossy())));
            }
        }
        out.sort_by_key(|(c, _)| *c != typed); // an exact match first, so it's the highlighted one
        if out.first().is_some_and(|(c, _)| *c == typed) {
            out.extend(subs);
        }
        out
    }

    fn draw(&mut self, f: &mut Frame) {
        let [tabs, body, input_box, status, foot] = Layout::vertical([
            Constraint::Length(1), Constraint::Min(1), Constraint::Length(3), Constraint::Length(1), Constraint::Length(1),
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
        if let Some(mut tv) = self.team_view.take() {
            let cwd = self.hub.get(&self.active).map(|m| m.cwd()).unwrap_or_default();
            tv.draw(f, panes, &self.hub, &cwd);
            self.team_view = Some(tv);
        } else if let Some(mut map) = self.map.take() {
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
                self.draw_pane(f, name, cell);
            }
        } else {
            let a = self.active.clone();
            self.draw_pane(f, &a, panes);
        }
        f.render_widget(Paragraph::new(self.status_line()).style(Style::new().fg(DIM)), status);
        let prompt = "› ";
        let pc = if self.in_team_space() { TEAM } else { ACCENT };
        // what the active session is doing, on the input box where the eye already is
        let doing = self.hub.get(&self.active).and_then(|m| match m.status() {
            "working" if m.thinking.load(std::sync::atomic::Ordering::Relaxed) => Some(" ✻ thinking… "),
            "working" | "waiting-model" => Some(" … working "),
            _ => None,
        });
        let mut input_block = Block::bordered().border_style(Style::new().fg(DIM));
        if let Some(d) = doing {
            input_block = input_block.title(Span::styled(d, Style::new().fg(ACCENT)));
        }
        let input = input_block.inner(input_box);
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
        let chips: String = self.chips.iter().enumerate()
            .map(|(i, c)| format!("[image {}: {}] ", i + 1, crate::image::size(crate::image::bytes_of(c)))).collect();
        let cx = cx + chips.len();
        let line = if !chips.is_empty() {
            Line::from(vec![Span::styled(prompt, Style::new().fg(pc)), Span::styled(chips, Style::new().fg(ACCENT)), Span::raw(shown_text)])
        } else if self.input.is_empty() {
            Line::from(vec![Span::styled(prompt, Style::new().fg(pc)), Span::styled(match &self.secret {
                Some(p) => format!("API key for {p} (hidden, Esc cancels)"),
                None => "message, /help, @file to attach".into(),
            }, Style::new().fg(DIM))])
        } else {
            Line::from(vec![Span::styled(prompt, Style::new().fg(pc)), Span::raw(shown_text)])
        };
        f.render_widget(Paragraph::new(line), input);
        f.render_widget(input_block, input_box);
        f.set_cursor_position((input.x + 2 + cx as u16, input.y));
        // macOS writes the control key as ⌃, the way its own menus show it
        let c = if cfg!(target_os = "macos") { "⌃" } else { "^" };
        let hints = format!("⇧Tab mode  ⇧←/→ tabs  {c}N new tab  {c}G grid  {c}R replies  {c}P projects  {c}T team  Esc stop  {c}Q quit  ·  /help for all keys");
        // a newer qh: a standing notice at the right end of the bottom line, over the hints
        let foot = match crate::update::available() {
            Some(v) => {
                let note = format!(" Update available ({v})! Run: {} ", crate::update::how());
                let w = (note.chars().count() as u16).min(foot.width);
                let [rest, right] = Layout::horizontal([Constraint::Min(0), Constraint::Length(w)]).areas(foot);
                f.render_widget(Paragraph::new(note).style(Style::new().fg(Color::Yellow)), right);
                rest
            }
            None => foot,
        };
        f.render_widget(Paragraph::new(hints).style(Style::new().fg(DIM)), foot);
        let sugg = self.suggestions();
        if !sugg.is_empty() && self.picker.is_none() {
            let rows = sugg.len().min(12) as u16;
            let pad = sugg.iter().map(|(c, _)| c.chars().count()).max().unwrap_or(14).max(14);
            let wide = sugg.iter().map(|(_, d)| pad + d.chars().count() + 4).max().unwrap_or(70) as u16;
            let r = Rect { x: input.x, y: input.y.saturating_sub(rows + 2), width: input.width.min(wide.max(40)), height: rows + 2 };
            let top = self.sugg_sel.saturating_sub(rows as usize - 1);
            let lines: Vec<Line> = sugg.iter().enumerate().skip(top).take(rows as usize).map(|(i, (c, d))| {
                let st = if i == self.sugg_sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
                Line::from(vec![Span::styled(format!("{c:<pad$}"), st.fg(ACCENT)), Span::styled(format!(" {d}"), st.fg(DIM))])
            }).collect();
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(lines).block(Block::bordered().border_style(Style::new().fg(DIM))
                .title_bottom(Line::styled(" ↑↓ · Tab fill · Enter run ", Style::new().fg(DIM)))), r);
        }
        self.picker_rows = self.picker.as_ref().map(|p| draw_picker(f, p));
        if let Some(c) = self.confirms.front() {
            draw_confirm(f, c, self.confirms.len());
        }
    }

    fn draw_tabs(&self, f: &mut Frame, area: Rect) -> Vec<(u16, u16, String)> {
        let team = self.in_team_space();
        let accent = if team { TEAM } else { ACCENT };
        // which space you're in, always in the corner
        let chip = if team {
            format!(" ◆ {} ", crate::team::repo().unwrap_or_default().rsplit('/').next().unwrap_or("team"))
        } else {
            let waiting = crate::team::needs_you().then_some(());
            format!(" ◇ personal{} ", if waiting.is_some() { " · team needs you ^Space" } else { "" })
        };
        let chip_w = chip.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() as u16 + 1;
        f.render_widget(Paragraph::new(Line::styled(chip.as_str(), if team {
            Style::new().fg(Color::Black).bg(TEAM).add_modifier(Modifier::BOLD)
        } else { Style::new().fg(DIM) })), Rect { width: chip_w.min(area.width), ..area });
        let area = Rect { x: area.x + chip_w, width: area.width.saturating_sub(chip_w), ..area };
        let mut names = self.tabs();
        if team { names.insert(0, MAP_TAB.to_string()) }
        let labels: Vec<(String, bool, bool)> = names.iter().map(|n| {
            if n == MAP_TAB { return (" map ".to_string(), self.team_view.is_some(), false) }
            let st = self.hub.get(n).map(|m| m.status()).unwrap_or("idle");
            let unread = self.views.get(n).is_some_and(|v| v.unread) && *n != self.active;
            (format!(" {n}{}{} ", icon(st), if unread { " ●" } else { "" }), *n == self.active && !(team && self.team_view.is_some()), unread)
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
                Style::new().fg(Color::Black).bg(accent).add_modifier(Modifier::BOLD)
            } else if *unread {
                Style::new().fg(accent)
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
                            let i = p.rows().get(top + (row - r.y) as usize).and_then(|x| x.0).unwrap_or(usize::MAX);
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
                        if n == MAP_TAB { self.open_map() } else { self.team_view = None; self.set_active(&n) }
                    }
                }
            }
            _ => {}
        }
    }

    /// Every project you've used: the ones with open tabs, then recent ones.
    fn project_picker(&self) -> Picker {
        let mut roots: Vec<std::path::PathBuf> = vec![];
        for n in self.hub.names() {
            if let Some(m) = self.hub.get(&n) {
                let r = crate::teamview::project_root(&m.cwd());
                if !roots.contains(&r) { roots.push(r) }
            }
        }
        let recent = recent_projects();
        for (p, _) in &recent {
            if !roots.contains(p) && p.is_dir() { roots.push(p.clone()) }
        }
        let used = |p: &std::path::Path| recent.iter().find(|(r, _)| r == p).map(|(_, t)| *t).unwrap_or(0);
        roots.sort_by_key(|p| (*p != self.project, std::cmp::Reverse(used(p))));
        let mut team = vec![];
        let mut personal = vec![];
        for r in roots {
            let name = r.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| r.display().to_string());
            let tabs = self.hub.names().into_iter().filter(|n| self.hub.get(n).is_some_and(|m| crate::teamview::project_root(&m.cwd()) == r)).count();
            let here = if r == self.project { "  ◂ here" } else { "" };
            let repo = crate::team::github_repo(&r).map(|(_, x)| x).filter(|x| crate::team::enabled(x));
            match repo {
                Some(repo) => {
                    let what = match crate::team::glance(&repo) {
                        Some((on, waiting)) => format!("{}{}",
                            if on.is_empty() { "nobody else online".to_string() } else { format!("● {}", on.join(", ")) },
                            if waiting > 0 { format!(" · {waiting} waiting") } else { String::new() }),
                        None => "○ not connected".into(),
                    };
                    team.push((format!("open {}", r.display()), format!("{name:<20} {what}{here}"), "Team".to_string()));
                }
                None => {
                    let when = match used(&r) { 0 => String::new(), t => format!(" · {}", ago(t)) };
                    let n = if tabs == 0 { "closed".to_string() } else { format!("{tabs} session{}", if tabs == 1 { "" } else { "s" }) };
                    personal.push((format!("open {}", r.display()), format!("{name:<20} {n}{when}{here}"), "Personal".to_string()));
                }
            }
        }
        let mut all = team;
        all.extend(personal);
        all.push((format!("browse {}", crate::config::home().display()), "+ Open another folder…".into(), String::new()));
        Picker { title: "projects (Ctrl+P)", cmd: "/project".into(), all, filter: String::new(), sel: 0, cur: String::new() }
    }

    fn in_team_space(&self) -> bool {
        crate::team::running() && crate::team::enabled_for(&self.project)
    }

    fn open_map(&mut self) {
        let cwd = self.hub.get(&self.active).map(|m| m.cwd()).unwrap_or_default();
        self.team_view = Some(crate::teamview::TeamView::open(&cwd));
    }

    /// Ctrl+Space: from your own work to the team's and back, instantly. Both keep running.
    fn flip_space(&mut self) {
        let a = self.active.clone();
        if self.in_team_space() || crate::team::enabled_for(&self.project) {
            self.map_was_open = self.team_view.is_some();
            self.team_view = None;
            // a project can turn into a team one after it was noted as personal
            let ok = |p: &std::path::Path| !p.as_os_str().is_empty() && !crate::team::enabled_for(p);
            if !ok(&self.last_personal) {
                match recent_projects().into_iter().map(|(p, _)| p).find(|p| ok(p) && *p != self.project) {
                    Some(p) => self.last_personal = p,
                    None => return self.note(&a, "no personal project yet: Ctrl+P opens one, then Ctrl+Space or Ctrl+] flips between them"),
                }
            }
            let p = self.last_personal.clone();
            self.open_project(&p);
        } else {
            if self.last_team.as_os_str().is_empty() || !crate::team::enabled_for(&self.last_team) {
                // a team project used before, if any
                match recent_projects().into_iter().map(|(p, _)| p).find(|p| crate::team::enabled_for(p)) {
                    Some(p) => self.last_team = p,
                    None => return self.note(&a, "no team project yet: /team starts one or joins a friend's"),
                }
            }
            let p = self.last_team.clone();
            self.open_project(&p);
            if self.map_was_open || self.team_view.is_none() && crate::team::running() { self.open_map() }
        }
    }

    /// Show a project: its last-used tab, or a new session there. Its team connects if it's a
    /// team project; everything else keeps running in the background.
    fn open_project(&mut self, dir: &std::path::Path) {
        let root = crate::teamview::project_root(dir);
        let mine: Vec<String> = self.hub.names().into_iter()
            .filter(|n| self.hub.get(n).is_some_and(|m| crate::teamview::project_root(&m.cwd()) == root)).collect();
        let pick = self.recent.iter().find(|n| mine.contains(n)).cloned().or_else(|| mine.first().cloned());
        let name = match pick {
            Some(n) => n,
            None => {
                let base: String = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("project".into())
                    .chars().map(|c| if c.is_whitespace() { '-' } else { c }).collect();
                let names = self.hub.names();
                let name = std::iter::once(base.clone()).chain((2..).map(|i| format!("{base}-{i}"))).find(|n| !names.contains(n)).unwrap();
                if let Err(e) = self.hub.add(&name, None, "", "", None, false, Some(root.clone())) {
                    let a = self.active.clone();
                    return self.err(&a, e);
                }
                self.load_view(&name);
                name
            }
        };
        self.set_active(&name);
        if crate::team::enabled_for(&root) && !crate::team::github_repo(&root).is_some_and(|(_, r)| crate::team::running_for(&r)) {
            match self.start_team_in(&name) {
                Ok(s) => self.note(&name, s),
                Err(e) => self.err(&name, format!("team: {e}")),
            }
        }
        self.hub.save();
    }

    /// Issue #n in its own branch and folder (a git worktree next to the project) with its own
    /// session, so it never collides with anyone's work on main. Returns the new tab's name.
    fn start_task(&mut self, cwd: &std::path::Path, n: u64, title: &str) -> Result<String, String> {
        let (root, _) = crate::team::github_repo(cwd).ok_or("this folder isn't a GitHub project")?;
        let slug: String = title.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>()
            .split('-').filter(|w| !w.is_empty()).take(5).collect::<Vec<_>>().join("-");
        let branch = format!("qh/{n}-{slug}");
        let dir = crate::teamview::tasks_dir(&root).join(n.to_string());
        let git = |args: &[&str]| std::process::Command::new("git").current_dir(&root).args(args)
            .stdin(std::process::Stdio::null()).output().map_err(|e| e.to_string())
            .and_then(|o| if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) });
        if !dir.exists() {
            let _ = std::fs::create_dir_all(dir.parent().unwrap_or(&dir));
            let d = dir.to_string_lossy().to_string();
            git(&["worktree", "add", "-q", "-b", &branch, &d]).or_else(|_| git(&["worktree", "add", "-q", &d, &branch]))
                .map_err(|e| format!("couldn't make a folder for #{n}: {e}"))?;
        }
        let name = format!("issue{n}");
        if self.hub.get(&name).is_none() {
            self.hub.add(&name, None, "", "", None, false, Some(dir.clone()))?;
        }
        let m = self.hub.get(&name).ok_or("couldn't open the session")?;
        self.hub.set_role(&m, &format!("#{n} {title}"));
        let text = format!(
            "You're working on GitHub issue #{n}: {title}\n\
You're in your own folder on branch {branch}, so other sessions and teammates aren't affected.\n\
Read the issue with `gh issue view {n}`, fix it, run the tests and commit.\n\
When it's done, tell me what you changed. Don't push or open a pull request until I say so; \
then it's `git push -u origin {branch}` and `gh pr create --fill`.");
        self.view(&name).push(Item::User(text.clone()));
        self.hub.send(&name, &text, "user", 0, None);
        crate::team::log(&format!("you started #{n} in {name} (branch {branch})"));
        Ok(name)
    }

    /// Share this hub's sessions as presence on the active tab's repo.
    fn start_team(&self) -> Result<String, String> {
        self.start_team_in(&self.active.clone())
    }

    /// Connect the team of the project `tab` is in (its notes go to that tab).
    fn start_team_in(&self, tab: &str) -> Result<String, String> {
        self.start_team_at(self.hub.get(tab).map(|m| m.cwd()).unwrap_or_default(), tab)
    }

    /// Connect the team of the project at `cwd`, which needn't have a tab open (its notes go to `tab`).
    fn start_team_at(&self, cwd: std::path::PathBuf, tab: &str) -> Result<String, String> {
        let (hub, a) = (self.hub.clone(), tab.to_string());
        let root = crate::team::github_repo(&cwd).map(|(r, _)| r).unwrap_or_else(|| cwd.clone());
        let summary = Box::new(move || {
            // only this project's sessions: other projects are none of this team's business
            let members: Vec<_> = lock(&hub.members).iter().filter(|m| crate::teamview::in_project(&m.cwd(), &root)).cloned().collect();
            let sessions: Vec<Value> = members.iter().map(|m| {
                let (topic, now, files) = crate::teamview::session_info(&hub, m, &root);
                serde_json::json!({"name": m.name, "status": m.status(), "role": lock(&m.purpose).clone(),
                                   "topic": topic, "now": now, "files": files, "branch": crate::teamview::branch(&m.cwd())})
            }).collect();
            // the recent conversation of each session, text only, so teammates can follow along
            let mut transcripts = serde_json::Map::new();
            for m in &members {
                let msgs = lock(&m.msgs);
                let rows: Vec<Value> = msgs.iter().filter(|x| matches!(context::role(x), "user" | "assistant"))
                    .filter(|x| !context::content(x).trim().is_empty())
                    .rev().take(30).collect::<Vec<_>>().into_iter().rev()
                    .map(|x| serde_json::json!([context::role(x), context::content(x).chars().take(1500).collect::<String>()])).collect();
                transcripts.insert(m.name.clone(), rows.into());
            }
            // paths relative to the project, so the same file matches on everyone's machine
            let files: Vec<String> = hub.claimed_files().into_iter().filter(|f| crate::teamview::in_project(std::path::Path::new(f), &root))
                .map(|f| crate::teamview::rel(&root, &f)).collect();
            serde_json::json!({"sessions": sessions, "files": files, "transcripts": transcripts})
        });
        let (h1, h2, h3, h4, a1, a2) = (self.hub.clone(), self.hub.clone(), self.hub.clone(), self.hub.clone(), a.clone(), a.clone());
        let (h5, h6) = (self.hub.clone(), self.hub.clone());
        let hooks = crate::team::Hooks {
            answer: std::sync::Arc::new(move |q: crate::team::Question| {
                // a read-only copy of the asked session answers; the session itself carries on
                let hub = h5.clone();
                std::thread::spawn(move || {
                    let who = if q.from_session.is_empty() { q.from.clone() } else { format!("{}/{}", q.from, q.from_session) };
                    let Some(m) = hub.get(&q.session).or_else(|| hub.names().first().and_then(|n| hub.get(n))) else {
                        crate::team::reply(&q, "(not answered: that session is closed)", &[]);
                        return;
                    };
                    let (text, relied) = crate::engine::answer_question(&hub, &m, &who, &q.text);
                    hub.emit(&m.name, Event::Notice(format!("answered {who}: {}", text.lines().next().unwrap_or("").chars().take(100).collect::<String>())));
                    crate::team::reply(&q, &text, &relied);
                });
            }),
            answered: Box::new(move |session: &str, text: &str| {
                // busy: it reads the answer before its next model call; idle: it's woken with it
                // (ctl held while queueing: the turn's end takes the queue under the same lock)
                let Some(m) = h6.get(session) else { return };
                let c = lock(&m.ctl);
                if c.running {
                    crate::team::queue_answer(session, text);
                } else {
                    drop(c);
                    h6.send(session, text, "user", 0, None);
                }
            }),
            say: Box::new(move |s: String| h1.emit(&a1, Event::Notice(s))),
            deliver: Box::new(move |session: &str, text: &str| {
                let to = if h2.get(session).is_some() { session.to_string() } else { h2.names().first().cloned().unwrap_or_default() };
                h2.send(&to, text, "user", 0, None);
            }),
            ask: Box::new(move |i: &crate::team::Incoming| h3.emit(&a2, Event::Notice(format!(
                "⚑ {} {} your {}: \"{}\" · nothing runs until you accept: Ctrl+T → Requests (accept, reply or decline)",
                i.from, if i.question.is_some() { "asks" } else { "sent a request for" }, i.session,
                i.text.chars().filter(|c| !c.is_control()).take(200).collect::<String>())))),
            refresh: Box::new(move || h4.emit("", Event::Changed)),
        };
        crate::team::start(&cwd, summary, hooks)
    }

    fn status_line(&self) -> String {
        let Some(m) = self.hub.get(&self.active) else { return String::new() };
        let s = lock(&m.set);
        let (profile, think) = (s.profile.clone(), match (s.think, s.effort.as_str()) {
            (false, _) => String::new(),
            (true, "") => " · think".to_string(),
            (true, e) => format!(" · think:{e}"),
        });
        drop(s);
        let live = m.live_tokens();
        let ctx = self.hub.ctx_for(&m, false).0 as u64;
        let bar = (20 * live / ctx.max(1)).min(20) as usize;
        let tps = lock(&m.turn).tps;
        let busy = self.hub.names().iter().filter(|n| **n != self.active)
            .filter(|n| self.hub.get(n).is_some_and(|m| m.status() != "idle")).count();
        let holder = lock(&self.hub.holder).clone();
        let mode = self.hub.mode_name();
        let cwd = m.cwd();
        let home = crate::config::home();
        let dir = cwd.strip_prefix(&home).map(|r| format!("~/{}", r.display())).unwrap_or_else(|_| cwd.display().to_string());
        let dir = if dir.chars().count() > 28 {
            let parts: Vec<&str> = dir.rsplit(['/', '\\']).take(2).collect();
            format!("…/{}/{}", parts.get(1).unwrap_or(&""), parts[0])
        } else { dir };
        let profile = if profile == self.hub.cfg.profile { String::new() } else { format!(" · {profile}") };
        // name, role and model are in the pane's title
        let mut out = format!(" {dir} · [{mode}]{profile}{think} · [{}{}] {live}/{ctx}",
                              "█".repeat(bar), "░".repeat(20 - bar));
        let (usage, pct) = { let t = lock(&m.turn); (t.usage, t.plan_pct) };
        out += &usage.status();
        if let Some(p) = pct { out += &format!(" · plan {p:.0}%") }
        if tps > 0.0 { out += &format!(" · {tps} tok/s") }
        if !holder.is_empty() { out += &format!(" · model: {holder}") }
        if busy > 0 { out += &format!(" · {busy} other busy") }
        let bg = crate::bg::running(&m.name);
        if bg > 0 { out += &format!(" · {bg} bg") }
        if self.hub.paused.load(std::sync::atomic::Ordering::Relaxed) { out += " · PAUSED" }
        if m.status() == "held" { out += &format!(" · held (> {MAX_HOPS} hops): /release") }
        if let Some(t) = crate::team::status_line() { out += &format!(" · {t}") }
        out
    }

    /// Wrap only as many items as fit, walking up from the newest.
    /// A session's chat in a bordered pane titled with its name, role, status and model;
    /// the active one's border is the accent colour.
    fn draw_pane(&mut self, f: &mut Frame, name: &str, area: Rect) {
        let title = self.hub.get(name).map(|m| {
            let role = lock(&m.purpose).clone();
            let role = if role.is_empty() { String::new() } else { format!(" · {role}") };
            let model = { let s = lock(&m.set); crate::providers::display(&s.provider, &s.model) };
            let status = if m.status() == "working" && m.thinking.load(std::sync::atomic::Ordering::Relaxed) { "thinking" } else { m.status() };
            let br = crate::worktree::branch(&m.cwd()).map(|b| format!(" · ⎇ {b}")).unwrap_or_default();
            format!(" {}{}{role}{br} · {status} · {model} ", name, icon(m.status()))
        }).unwrap_or_default();
        let color = if name == self.active { ACCENT } else { DIM };
        let block = Block::bordered().title(title).border_style(Style::new().fg(color));
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.draw_chat(f, name, inner);
    }

    fn draw_chat(&mut self, f: &mut Frame, name: &str, area: Rect) {
        // long lines are hard to read on a wide screen
        let width = area.width.saturating_sub(2).clamp(10, MAX_LINE) as usize;
        let h = area.height as usize;
        let replies = self.replies;
        let v = self.views.entry(name.to_string()).or_default();
        let need = h + v.scroll;
        let mut lines: Vec<Line<'static>> = vec![];
        let mut steps = 0; // tool calls folded away since the last item shown
        for (i, it) in v.items.iter().enumerate().rev() {
            if replies && matches!(it, Item::Tool { .. }) {
                steps += 1;
                if i > 0 {
                    continue;
                }
            }
            let mut ls = if replies && matches!(it, Item::Tool { .. }) { vec![] } else { item_lines(it, width, need - lines.len()) };
            if steps > 0 {
                let s = if steps == 1 { "" } else { "s" };
                ls.push(Line::styled(format!("  · {steps} step{s}"), Style::new().fg(DIM)));
                steps = 0;
            }
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

const USER_ROWS: usize = 6; // a longer message of yours shows this many rows, then "… N more"

/// Rows for an item; a long reply is wrapped from its end and stops after `need` rows.
fn item_lines(it: &Item, w: usize, need: usize) -> Vec<Line<'static>> {
    let styled = |text: &str, st: Style, indent: usize| -> Vec<Line<'static>> {
        wrap(text, w.saturating_sub(indent)).into_iter()
            .map(|l| Line::from(Span::styled(" ".repeat(indent) + &l, st))).collect()
    };
    match it {
        // your turn: a bar down the left, plain text, a long paste folded to its first rows
        Item::User(s) => {
            let rows = wrap(s, w.saturating_sub(2));
            let shown = if rows.len() > USER_ROWS + 2 { USER_ROWS } else { rows.len() };
            let bar = || Span::styled("▌ ", Style::new().fg(ACCENT));
            let mut v = vec![Line::raw("")];
            v.extend(rows[..shown].iter().map(|l| Line::from(vec![bar(), Span::styled(l.clone(), Style::new().add_modifier(Modifier::BOLD))])));
            if shown < rows.len() {
                v.push(Line::from(vec![bar(), Span::styled(format!("… {} more lines", rows.len() - shown), Style::new().fg(DIM))]));
            }
            v.push(Line::raw(""));
            v
        }
        Item::Reply(s) => {
            // whole blocks from the end until the screen is full: wrapping is the costly part
            let mut rows = vec![];
            for b in md_blocks(s).into_iter().rev() {
                if rows.len() >= need {
                    break;
                }
                let mut ls = md_block(&b, w);
                ls.append(&mut rows);
                rows = ls;
            }
            rows
        }
        Item::Note(s) => styled(s, Style::new().fg(Color::Blue), 0),
        Item::Cmd(s) => styled(&format!("› {s}"), Style::new().fg(ACCENT), 0),
        Item::Err(s) => styled(s, Style::new().fg(Color::Red), 0),
        Item::Tool { title, args, result, open } => {
            let n = result.lines().count();
            let size = if *open || n < 2 { String::new() } else { format!(" · {n} lines") };
            let mut v = styled(&format!("{} {title}{size}", if *open { "▾" } else { "▸" }), Style::new().fg(DIM), 0);
            if *open {
                let a: Value = serde_json::from_str(args).unwrap_or_default();
                if let (Some(old), Some(new)) = (a["old"].as_str(), a["new"].as_str()) {
                    v.extend(diff_lines(old, new, w));
                } else if let (Some(_), Some(c)) = (a["path"].as_str(), a["content"].as_str()) {
                    v.extend(diff_lines("", c, w));
                } else {
                    v.extend(styled(args, Style::new().fg(Color::Magenta), 2));
                    let body: Vec<&str> = result.lines().take(40).collect();
                    v.extend(styled(&body.join("\n"), Style::new(), 2));
                }
            }
            v
        }
    }
}

/// A reply cut into what renders as a unit: a fenced code block, a table, or one line.
enum Md<'a> {
    Code(&'a str, Vec<&'a str>),
    Table(Vec<&'a str>),
    Line(&'a str),
}

/// Only slices, no wrapping: cheap enough to redo for every streamed token.
fn md_blocks(s: &str) -> Vec<Md<'_>> {
    let ls: Vec<&str> = s.lines().collect();
    let fence = |l: &str| l.trim_start().starts_with("```");
    let row = |l: &str| l.trim_start().starts_with('|');
    let rule = |l: &str| row(l) && l.contains('-') && l.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '));
    let (mut v, mut i) = (vec![], 0);
    while i < ls.len() {
        let l = ls[i];
        if fence(l) {
            // an unclosed fence is code to the end: the reply is still streaming
            let end = ls[i + 1..].iter().position(|l| fence(l)).map_or(ls.len(), |p| i + 1 + p);
            v.push(Md::Code(l.trim_start()[3..].trim(), ls[i + 1..end].to_vec()));
            i = end + 1;
        } else if row(l) && ls.get(i + 1).is_some_and(|n| rule(n)) {
            let end = ls[i..].iter().position(|l| !row(l)).map_or(ls.len(), |p| i + p);
            v.push(Md::Table(ls[i..end].to_vec()));
            i = end;
        } else {
            v.push(Md::Line(l));
            i += 1;
        }
    }
    v
}

fn tw(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn md_block(b: &Md, w: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(DIM);
    let pad = |n: usize| Span::raw(" ".repeat(n));
    match b {
        Md::Code(lang, ls) => {
            let mut v = vec![Line::styled(format!("╭─ {lang}").trim_end().to_string(), dim)];
            for l in ls {
                v.extend(fill(code_spans(&clean(l)), w, Span::styled("│ ", dim), Span::styled("│ ", dim), false));
            }
            v.push(Line::styled("╰─", dim));
            v
        }
        Md::Table(ls) => {
            let rows: Vec<Vec<Vec<(String, Style)>>> = ls.iter().enumerate().filter(|(i, _)| *i != 1).map(|(i, l)| {
                let t = clean(l);
                let t = t.trim().trim_start_matches('|');
                let t = t.strip_suffix('|').unwrap_or(t);
                let st = if i == 0 { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
                t.split('|').map(|c| inline(c.trim(), st)).collect()
            }).collect();
            let cw_of = |c: &Vec<(String, Style)>| c.iter().map(|(s, _)| tw(s)).sum::<usize>();
            let mut cw: Vec<usize> = vec![];
            for r in &rows {
                for (i, c) in r.iter().enumerate() {
                    if i >= cw.len() { cw.push(0) }
                    cw[i] = cw[i].max(cw_of(c));
                }
            }
            if cw.iter().sum::<usize>() + 3 * cw.len().saturating_sub(1) > w {
                return ls.iter().flat_map(|l| fill(vec![(clean(l), Style::new())], w, pad(0), pad(0), true)).collect();
            }
            let mut v = vec![];
            for (ri, r) in rows.iter().enumerate() {
                let mut spans = vec![];
                for (i, &width) in cw.iter().enumerate() {
                    if i > 0 { spans.push(Span::styled(" │ ", dim)) }
                    let c = r.get(i).cloned().unwrap_or_default();
                    let n = cw_of(&c);
                    spans.extend(c.into_iter().map(|(s, st)| Span::styled(s, st)));
                    if i + 1 < cw.len() { spans.push(pad(width - n)) }
                }
                v.push(Line::from(spans));
                if ri == 0 {
                    let rule: Vec<String> = cw.iter().map(|&n| "─".repeat(n)).collect();
                    v.push(Line::styled(rule.join("─┼─"), dim));
                }
            }
            v
        }
        Md::Line(l) => {
            let l = clean(l);
            let t = l.trim_start();
            let ind = l.len() - t.len();
            let h = t.chars().take_while(|&c| c == '#').count();
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if t.is_empty() {
                vec![Line::raw("")]
            } else if (1..=6).contains(&h) && t[h..].starts_with(' ') {
                fill(inline(t[h..].trim(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)), w, pad(0), pad(0), true)
            } else if let Some(q) = t.strip_prefix('>') {
                fill(inline(q.trim_start(), Style::new().add_modifier(Modifier::ITALIC)), w, Span::styled("│ ", dim), Span::styled("│ ", dim), true)
            } else if ["- ", "* ", "+ "].iter().any(|m| t.starts_with(m)) {
                // wrapped rows hang under the text, not the bullet
                fill(inline(&t[2..], Style::new()), w, Span::raw(" ".repeat(ind) + "• "), pad(ind + 2), true)
            } else if digits > 0 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
                let m = &t[..digits + 2];
                fill(inline(&t[digits + 2..], Style::new()), w, Span::raw(" ".repeat(ind) + m), pad(ind + m.len()), true)
            } else if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*')) {
                vec![Line::styled("─".repeat(w.min(40)), dim)]
            } else {
                fill(inline(&l, Style::new()), w, pad(0), pad(0), true)
            }
        }
    }
}

/// `code`, **bold** and *italic* within a line; a bold or italic not yet closed runs to the end.
fn inline(s: &str, base: Style) -> Vec<(String, Style)> {
    let cs: Vec<char> = s.chars().collect();
    let (mut v, mut cur, mut b, mut it, mut i) = (vec![], String::new(), false, false, 0);
    let st = |b: bool, it: bool| {
        let s = if b { base.add_modifier(Modifier::BOLD) } else { base };
        if it { s.add_modifier(Modifier::ITALIC) } else { s }
    };
    while i < cs.len() {
        let c = cs[i];
        let mark = if c == '`' {
            cs[i + 1..].iter().position(|&c| c == '`').map(|p| {
                v.push((std::mem::take(&mut cur), st(b, it)));
                v.push((cs[i + 1..i + 1 + p].iter().collect(), Style::new().fg(Color::Yellow)));
                p + 2
            })
        } else if c == '*' && cs.get(i + 1) == Some(&'*') {
            v.push((std::mem::take(&mut cur), st(b, it)));
            b = !b;
            Some(2)
        } else if c == '*' && (it || cs.get(i + 1).is_some_and(|n| !n.is_whitespace())) {
            v.push((std::mem::take(&mut cur), st(b, it)));
            it = !it;
            Some(1)
        } else {
            None
        };
        match mark {
            Some(n) => i += n,
            None => {
                cur.push(c);
                i += 1;
            }
        }
    }
    v.push((cur, st(b, it)));
    v.retain(|(s, _)| !s.is_empty());
    v
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "impl", "struct", "enum", "match", "if", "else", "for", "while", "loop",
    "return", "in", "def", "class", "import", "from", "const", "var", "function", "async", "await", "self",
    "true", "false", "None", "null", "static", "type", "mod", "trait", "where", "as", "func", "package",
];

/// Just enough colour to read code: keywords, strings, comments.
fn code_spans(l: &str) -> Vec<(String, Style)> {
    let cs: Vec<char> = l.chars().collect();
    let mut v: Vec<(String, Style)> = vec![];
    let mut put = |s: String, st: Style| match v.last_mut() {
        Some((p, ps)) if *ps == st => p.push_str(&s),
        _ => v.push((s, st)),
    };
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if (c == '/' && cs.get(i + 1) == Some(&'/')) || (c == '#' && cs[..i].iter().all(|c| c.is_whitespace())) {
            put(cs[i..].iter().collect(), Style::new().fg(DIM));
            break;
        } else if c == '"' {
            let mut j = i + 1;
            while j < cs.len() && cs[j] != c {
                j += if cs[j] == '\\' { 2 } else { 1 };
            }
            let j = (j + 1).min(cs.len());
            put(cs[i..j].iter().collect(), Style::new().fg(Color::Green));
            i = j;
        } else if c.is_alphanumeric() || c == '_' {
            let j = cs[i..].iter().position(|c| !(c.is_alphanumeric() || *c == '_')).map_or(cs.len(), |p| i + p);
            let word: String = cs[i..j].iter().collect();
            let st = if KEYWORDS.contains(&word.as_str()) { Style::new().fg(Color::Magenta) } else { Style::new().fg(Color::Yellow) };
            put(word, st);
            i = j;
        } else {
            put(c.to_string(), Style::new().fg(Color::Yellow));
            i += 1;
        }
    }
    v
}

/// Wraps styled text into rows `w` wide; `lead` starts the first row, `hang` the rest.
/// `words` breaks between words, else hard at the edge (code).
fn fill(spans: Vec<(String, Style)>, w: usize, lead: Span<'static>, hang: Span<'static>, words: bool) -> Vec<Line<'static>> {
    let room = w.saturating_sub(lead.width()).max(1);
    let (mut out, mut row, mut rw): (Vec<Vec<Span<'static>>>, Vec<Span<'static>>, usize) = (vec![], vec![], 0);
    for (t, st) in &spans {
        let parts: Vec<&str> = if words { t.split_inclusive(' ').collect() } else { vec![t] };
        for p in parts {
            if words && rw > 0 && rw + tw(p.trim_end()) > room {
                out.push(std::mem::take(&mut row));
                rw = 0;
            }
            let mut cur = String::new();
            for c in p.chars() {
                let cw = c.width().unwrap_or(0);
                if words && c == ' ' && (rw + cw > room || rw == 0 && !out.is_empty()) {
                    continue; // spaces at a break vanish
                }
                if rw + cw > room {
                    if !cur.is_empty() { row.push(Span::styled(std::mem::take(&mut cur), *st)) }
                    out.push(std::mem::take(&mut row));
                    rw = 0;
                }
                cur.push(c);
                rw += cw;
            }
            if !cur.is_empty() { row.push(Span::styled(cur, *st)) }
        }
    }
    out.push(row);
    out.into_iter().enumerate().map(|(i, mut r)| {
        if let Some(last) = r.last_mut() {
            last.content = last.content.trim_end().to_string().into();
        }
        r.insert(0, if i == 0 { lead.clone() } else { hang.clone() });
        Line::from(r)
    }).collect()
}

const DIFF_ROWS: usize = 40; // an edit longer than this shows its start, then "… N more"

/// An edit as Claude Code shows it: unchanged lines around it dim, removed red, added green.
/// A plain line diff: the common start and end are context, the middle is what changed.
fn diff_lines(old: &str, new: &str, w: usize) -> Vec<Line<'static>> {
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let pre = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let suf = o[pre..].iter().rev().zip(n[pre..].iter().rev()).take_while(|(a, b)| a == b).count();
    let row = |mark: &str, l: &str, st: Style| {
        let text: String = format!("  {mark} {l}").chars().take(w.max(4)).collect();
        Line::from(Span::styled(text, st))
    };
    let (dim, red, green) = (Style::new().fg(DIM), Style::new().fg(Color::Red), Style::new().fg(Color::Green));
    let mut v: Vec<Line<'static>> = vec![];
    v.extend(o[pre.saturating_sub(2)..pre].iter().map(|l| row(" ", l, dim)));
    v.extend(o[pre..o.len() - suf].iter().map(|l| row("-", l, red)));
    v.extend(n[pre..n.len() - suf].iter().map(|l| row("+", l, green)));
    v.extend(o[o.len() - suf..].iter().take(2).map(|l| row(" ", l, dim)));
    if v.len() > DIFF_ROWS {
        let more = v.len() - DIFF_ROWS;
        v.truncate(DIFF_ROWS);
        v.push(row(" ", &format!("… {more} more lines"), dim));
    }
    v
}

/// The start of the word before `cur`, for Alt+b (Option+← in macOS Terminal).
fn word_left(input: &[char], cur: usize) -> usize {
    let mut i = cur.min(input.len());
    while i > 0 && !input[i - 1].is_alphanumeric() { i -= 1 }
    while i > 0 && input[i - 1].is_alphanumeric() { i -= 1 }
    i
}

/// The end of the word after `cur`, for Alt+f (Option+→).
fn word_right(input: &[char], cur: usize) -> usize {
    let mut i = cur.min(input.len());
    while i < input.len() && !input[i].is_alphanumeric() { i += 1 }
    while i < input.len() && input[i].is_alphanumeric() { i += 1 }
    i
}

/// Word wrap by display width; overlong words are broken hard.
pub fn wrap(text: &str, w: usize) -> Vec<String> {
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
        .title(format!(" {} ", p.title)).title_bottom(Line::styled(" ↑↓ move · type to filter · Enter pick · Esc close ", Style::new().fg(DIM)));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let mut lines = vec![Line::from(vec![Span::styled("search: ", Style::new().fg(DIM)), Span::raw(p.filter.clone()), Span::styled("▏", Style::new().fg(ACCENT))])];
    let rows = inner.height.saturating_sub(1) as usize;
    let all_rows = p.rows();
    let at = all_rows.iter().position(|(i, _)| *i == Some(p.sel)).unwrap_or(0);
    // keep the selection's heading in view when scrolling back up to a group's first item
    let at = if at > 0 && all_rows[at - 1].0.is_none() { at - 1 } else { at };
    let top = at.saturating_sub(rows.saturating_sub(1)).min(at);
    if hits.is_empty() {
        lines.push(Line::styled("no match", Style::new().fg(DIM)));
    }
    for (i, label) in all_rows.iter().skip(top).take(rows) {
        match i {
            None => lines.push(Line::styled(label.to_string(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))),
            Some(i) => {
                let mark = if hits[*i].0 == p.cur { "● " } else { "  " };
                let st = if *i == p.sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
                let indent = if hits[*i].2.is_empty() { "" } else { "  " };
                lines.push(Line::styled(format!("{indent}{mark}{}", clip(label, (w as usize).saturating_sub(6))), st));
            }
        }
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
        "review_hold" => (format!("{} asked for changes: push anyway?", s("owners")),
            s("text").lines().map(|l| Line::raw(l.to_string())).collect(), true),
        "team_note" => ("add to the team notes?".into(), vec![Line::styled(s("text"), Style::new().fg(Color::Yellow))], false),
        "network" => (format!("{}:{}", s("host"), a["port"]), vec![Line::raw("a sandboxed command is connecting to a host that isn't on the allowlist")], true),
        _ => (clip(&a.to_string(), 80), vec![Line::raw(a.to_string())], false),
    };
    let area = f.area();
    let w = area.width.saturating_sub(4).min(90);
    // rows a line takes once wrapped inside the border, so the keys line never falls off the bottom
    let iw = w.saturating_sub(2).max(1);
    let rows = |l: &Line| Paragraph::new(l.clone()).wrap(ratatui::widgets::Wrap { trim: false }).line_count(iw).max(1) as u16;
    let mut head = vec![Line::styled(summary, Style::new().fg(DIM))];
    if !c.warn.is_empty() {
        head.push(Line::styled(format!("! {}", c.warn), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)));
    }
    head.push(Line::raw(""));
    let mut foot = vec![Line::raw("")];
    if !c.rule.is_empty() && c.tool != "review_hold" {
        foot.push(Line::from(vec![Span::styled("always = auto-approve ", Style::new().fg(DIM)), Span::styled(c.rule.clone(), Style::new().add_modifier(Modifier::BOLD))]));
    }
    let keys = if c.tool == "review_hold" { "[y] push anyway   [h] hand to them in team chat   [n] hold and fix".to_string() }
        else if c.rule.is_empty() { "[y] allow   [n] deny".to_string() } else { "[y] allow   [a] always   [n] deny".to_string() };
    foot.push(Line::styled(keys, Style::new().add_modifier(Modifier::BOLD)));
    let fixed: u16 = head.iter().chain(&foot).map(|l| rows(l)).sum::<u16>() + 2;
    let budget = area.height.saturating_sub(5).saturating_sub(fixed);
    let (mut body_h, mut body) = (0u16, vec![]);
    for l in detail {
        let r = rows(&l);
        if body_h + r > budget { break }
        body_h += r;
        body.push(l);
    }
    let h = (fixed + body_h).min(area.height);
    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
    f.render_widget(Clear, r);
    let color = if danger || !c.warn.is_empty() { Color::Red } else { Color::Yellow };
    let more = if queued > 1 { format!(" (+{} waiting) ", queued - 1) } else { String::new() };
    let block = Block::bordered().border_style(Style::new().fg(color))
        .title(Line::from(vec![Span::styled(format!(" {} ", c.tool), Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)),
                               Span::raw(format!("  {}  wants to run{more}", c.session))]));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines: Vec<Line> = head.into_iter().chain(body).chain(foot).collect();
    f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
}

pub fn channel_pair() -> (Sender<Ui>, Receiver<Ui>) {
    channel()
}

#[cfg(test)]
mod diff_tests {
    use super::diff_lines;

    fn text(old: &str, new: &str) -> Vec<String> {
        diff_lines(old, new, 80).iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn only_the_changed_middle_is_marked() {
        assert_eq!(text("a\nb\nc\nd", "a\nb\nX\nd"), ["    a", "    b", "  - c", "  + X", "    d"]);
    }

    #[test]
    fn a_new_file_is_all_added_and_long_ones_are_cut() {
        let big: String = (0..100).map(|i| format!("{i}\n")).collect();
        let v = text("", &big);
        assert!(v[0].starts_with("  + 0"));
        assert_eq!(v.len(), super::DIFF_ROWS + 1);
        assert!(v.last().unwrap().contains("60 more lines"));
    }
}

#[cfg(test)]
mod md_tests {
    use ratatui::style::{Modifier, Style};

    fn text(s: &str, w: usize) -> Vec<String> {
        super::md_blocks(s).iter().flat_map(|b| super::md_block(b, w)).map(|l| l.to_string()).collect()
    }

    #[test]
    fn headings_lose_their_hashes() {
        assert_eq!(text("## Plan\n#include", 40), ["Plan", "#include"]);
    }

    #[test]
    fn list_items_hang_under_their_text() {
        assert_eq!(text("- one two three four\n  2. alpha beta gamma", 12), ["• one two", "  three four", "  2. alpha", "     beta", "     gamma"]);
    }

    #[test]
    fn fences_keep_indentation_and_an_unclosed_one_runs_to_the_end() {
        assert_eq!(text("```rs\n    let x = \"a\"; // c\n```\nafter", 40), ["╭─ rs", "│     let x = \"a\"; // c", "╰─", "after"]);
        assert_eq!(text("```\nfn a()\n**not bold**", 40), ["╭─", "│ fn a()", "│ **not bold**", "╰─"]);
    }

    #[test]
    fn inline_marks_are_styled_and_dropped() {
        let v = super::inline("use `a*b` **now** or *later", Style::new());
        let t: Vec<&str> = v.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(t, ["use ", "a*b", " ", "now", " or ", "later"]);
        assert!(v[3].1.add_modifier.contains(Modifier::BOLD));
        assert!(v[5].1.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(text("a * b", 40), ["a * b"]);
    }

    #[test]
    fn tables_align_and_fall_back_when_too_wide() {
        assert_eq!(text("| a | bb |\n|---|---|\n| ccc | d |", 40), ["a   │ bb", "────┼───", "ccc │ d"]);
        assert_eq!(text("| a | bb |\n|---|---|", 5), ["| a |", "bb |", "|---|", "---|"]);
    }
}

/// The TUI's input without a terminal, for `qh serve` clients: a line goes through the same
/// `/command` code and the same send (images, @mentions, queueing) as typing it in the TUI, so
/// the Mac app has every command. What the TUI would draw comes back as JSON events.
pub struct Remote(App);

impl Remote {
    pub fn new(hub: Arc<Hub>) -> Remote {
        Remote(App::blank(hub))
    }

    /// Team presence on, as the TUI does at start, when this project has a team. Returns what it says.
    pub fn start_team(&mut self) -> Option<Result<String, String>> {
        let app = &mut self.0;
        let n = app.hub.names().first()?.clone();
        let cwd = app.hub.get(&n)?.cwd();
        if !crate::team::enabled_for(&cwd) { return None }
        app.set_active(&n);
        Some(app.start_team_in(&n))
    }

    /// The commands and descriptions `/` offers in `tab`, custom ones included.
    pub fn commands(&self, tab: &str) -> Vec<Value> {
        let cwd = self.0.hub.get(tab).map(|m| m.cwd()).unwrap_or_default();
        COMMANDS.iter().map(|(c, d)| json!({"cmd": c, "about": d}))
            .chain(custom_commands(&cwd).into_iter().map(|(n, p)| json!({"cmd": format!("/{n}"), "about": p.display().to_string()})))
            .chain(SUBCOMMANDS.iter().map(|(c, d)| json!({"cmd": c, "about": d, "sub": true})))
            .collect()
    }

    pub fn input(&mut self, tab: &str, line: &str, images: Vec<String>) -> Vec<Value> {
        let app = &mut self.0;
        if app.hub.get(tab).is_some() && app.active != tab {
            app.set_active(tab);
        }
        for v in app.views.values_mut() {
            v.items.clear();
        }
        let line = line.trim();
        if let Some(name) = app.secret.take().filter(|_| !line.starts_with('/')) {
            // the line is the API key a command asked for: never echoed, never kept
            let msg = match crate::providers::set_key(&name, line).and_then(|_| crate::providers::test(&name)) {
                Ok(n) => format!("✓ {name}: key saved (chmod 600), {n} models available. /model {name} to pick one"),
                Err(e) => { let _ = crate::providers::remove_key(&name); format!("✗ {name}: {e} (not saved)") }
            };
            let a = app.active.clone();
            app.note(&a, msg);
        } else if line.starts_with('/') {
            app.command(line);
        } else if !line.is_empty() || !images.is_empty() {
            app.chips = images;
            app.send(if line.is_empty() { "(see the attached image)" } else { line });
        }
        let mut out = vec![];
        for (name, v) in app.views.iter_mut() {
            for it in v.items.drain(..) {
                match it {
                    Item::Note(m) => out.push(json!({"type": "notice", "tab": name, "message": m})),
                    Item::Err(m) => out.push(json!({"type": "error", "tab": name, "message": m})),
                    _ => {}
                }
            }
        }
        if let Some(p) = app.picker.take() {
            let items: Vec<Value> = p.all.iter().map(|(v, l, g)| json!({"value": v, "label": l, "group": g})).collect();
            out.push(json!({"type": "picker", "tab": app.active, "title": p.title, "cmd": p.cmd, "current": p.cur, "items": items}));
        }
        if let Some(name) = &app.secret {
            out.push(json!({"type": "secret", "tab": app.active, "provider": name}));
        }
        if let Some(step) = app.outside.take().filter(|s| s.first().is_some_and(|k| k == "edit")) {
            out.push(json!({"type": "notice", "tab": app.active, "message": format!("memory file: {}", step.last().cloned().unwrap_or_default())}));
        } else if let Some(step) = app.outside.take() {
            out.push(json!({"type": "notice", "tab": app.active,
                "message": format!("this step needs a terminal for now: run `qh team {}` in Terminal", step.join(" "))}));
        }
        if app.map.take().is_some() || app.team_view.take().is_some() {
            out.push(json!({"type": "notice", "tab": app.active, "message": "the map and team room aren't drawn in the app yet"}));
        }
        out.push(json!({"type": "active", "tab": app.active}));
        app.quit = false;
        out
    }
}
