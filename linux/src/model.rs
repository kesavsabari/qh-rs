//! What the app knows: projects (one `qh serve` each), their tabs, asks, plans, changes and pages,
//! and the saved settings. Events from the server become state here (apple/qh/Project.swift
//! `handle`, windows/Workspace.cs); the UI draws it and pushes the live thread itself.

use crate::client::{Client, Msg};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

pub enum Item {
    User(String),
    Reply(String),
    Tool(String),
}

pub struct Tab {
    pub name: String,
    pub status: String,
    pub model: String,
    pub provider: String,
    pub purpose: String,
    pub tokens: u64,
    pub cost: Option<f64>,
    /// the saved history (snapshot / resumed session): what the thread is rebuilt from
    pub items: Vec<Item>,
    /// anything shown since: the thread isn't empty once this is > 0
    pub live: usize,
    pub plan: Vec<(String, String)>,
    pub plan_ready: bool,
}

impl Tab {
    fn new(name: &str) -> Tab {
        Tab { name: name.into(), status: "idle".into(), model: String::new(), provider: String::new(), purpose: String::new(),
              tokens: 0, cost: None, items: vec![], live: 0, plan: vec![], plan_ready: false }
    }
    pub fn busy(&self) -> bool { self.status == "working" || self.status == "confirm" }
    pub fn label(&self) -> &str { if self.purpose.is_empty() { &self.name } else { &self.purpose } }
    pub fn empty(&self) -> bool { self.items.is_empty() && self.live == 0 }
    fn update(&mut self, t: &Value) {
        let s = |k: &str| t[k].as_str().map(String::from);
        if let Some(v) = s("status") { self.status = v }
        if let Some(v) = s("model") { self.model = v }
        if let Some(v) = s("provider") { self.provider = v }
        if let Some(v) = s("purpose") { self.purpose = v }
        if let Some(v) = t["tokens"].as_u64() { self.tokens = v }
    }
}

pub struct Session {
    pub id: String,
    pub title: String,
    pub when: f64,
}

/// A permission (or budget) question waiting for an answer.
#[derive(Clone)]
pub struct Ask {
    pub id: u64,
    pub tab: String,
    pub tool: String,
    pub args: Value,
    pub warn: String,
    pub guard: bool,
    pub always: String,
}

impl Ask {
    /// What the card shows in full: the command, or the edit (windows/Workspace.cs Detail).
    pub fn detail(&self) -> String {
        let a = &self.args;
        if let Some(c) = a["cmd"].as_str() { return c.into() }
        if let Some(p) = a["path"].as_str() {
            if let (Some(o), Some(n)) = (a["old"].as_str(), a["new"].as_str()) {
                let mut v = vec![p.to_string()];
                v.extend(o.lines().map(|l| format!("- {l}")));
                v.extend(n.lines().map(|l| format!("+ {l}")));
                return v.join("\n");
            }
            if let Some(body) = a["content"].as_str() {
                let mut v = vec![p.to_string()];
                v.extend(body.lines().take(40).map(|l| format!("+ {l}")));
                return v.join("\n");
            }
            return p.into();
        }
        brief(a)
    }
}

pub struct Activity {
    pub t: f64,
    pub kind: String,
    pub text: String,
    pub ok: bool,
}

impl Activity {
    pub fn group(&self) -> &'static str {
        match self.kind.as_str() { "cmd" => "Commands", "edit" => "Edits", "net" => "Network", "ask" => "Approvals", _ => "Other" }
    }
}

/// An inline edit in the code view: the lines, the streamed answer, the result's id.
#[derive(Default)]
pub struct Inline {
    pub id: Option<u64>,
    pub preview: String,
}

pub struct Project {
    pub path: String,
    pub client: Client,
    pub tabs: Vec<Tab>,
    pub active: String,
    pub mode: String,
    pub state: String,
    pub show_setup: bool,
    pub setup: Option<Value>,
    pub models: Vec<(String, String)>,
    pub setup_msg: Option<(String, bool)>,
    pub setup_busy: bool,
    pub sessions: Vec<Session>,
    pub open: HashMap<String, String>,
    pub layout: String,
    /// a page over the chat: review, checkpoints, memory, activity, tasks
    pub page: Option<String>,
    pub asks: Vec<Ask>,
    pub commands: Vec<(String, String)>,
    pub files: Vec<String>,
    pub file_set: HashSet<String>,
    pub changes: Vec<(String, String, String)>,
    pub checkpoints: Vec<(usize, String, Vec<String>)>,
    pub memory: Option<(String, String, Vec<(String, String)>)>,
    pub memory_ver: u32,
    pub activity: Vec<Activity>,
    pub tasks: String,
    pub team: Value,
    pub team_push: bool,
    pub chan: String,
    /// the file in the code view: (relative path, line)
    pub code: Option<(String, Option<usize>)>,
    pub inline: Option<Inline>,
    /// put back in the composer (a rewind's prompt)
    pub draft: Option<String>,
}

pub const MODES: [&str; 4] = ["ask", "auto-edit", "auto", "plan"];

impl Project {
    pub fn name(&self) -> String { name_of(&self.path) }
    pub fn current(&self) -> Option<&Tab> { self.tabs.iter().find(|t| t.name == self.active).or(self.tabs.first()) }
    pub fn current_name(&self) -> String { self.current().map(|t| t.name.clone()).unwrap_or_else(|| "main".into()) }
    pub fn tab(&mut self, name: &str) -> &mut Tab {
        if let Some(i) = self.tabs.iter().position(|t| t.name == name) { return &mut self.tabs[i] }
        self.tabs.push(Tab::new(name));
        self.tabs.last_mut().unwrap()
    }
    pub fn send(&self, v: Value) { self.client.send(v) }
    /// A command for the current tab: {"type": ty, "tab": current, ..more}.
    pub fn cmd(&self, ty: &str, more: Value) {
        let mut v = if more.is_object() { more } else { json!({}) };
        v["type"] = json!(ty);
        if v.get("tab").is_none() { v["tab"] = json!(self.current_name()) }
        self.send(v)
    }

    /// Sends a prompt (text and/or base64 images). Returns what the thread should show for it:
    /// Some((text, is_user)).
    pub fn prompt(&mut self, text: &str, images: &[String]) -> Option<(String, bool)> {
        let t = text.trim().to_string();
        let name = self.current().map(|c| c.name.clone())?;
        let n = images.len();
        if t.is_empty() && n == 0 { return None }
        let tab = self.tab(&name);
        tab.live += 1;
        let shown = if t.starts_with('/') { (format!("› {t}"), false) } else {
            tab.status = "working".into();
            let imgs = if n > 0 { format!("+ {n} image{}", if n > 1 { "s" } else { "" }) } else { String::new() };
            (match (t.is_empty(), imgs.is_empty()) { (_, true) => t.clone(), (true, _) => imgs, _ => format!("{t}\n{imgs}") }, true)
        };
        let mut v = json!({"type": "prompt", "tab": name, "text": t});
        if n > 0 { v["images"] = json!(images) }
        self.send(v);
        Some(shown)
    }
    pub fn stop(&self) { if let Some(t) = self.current() { self.send(json!({"type": "stop", "tab": t.name})) } }
    pub fn load_setup(&self) {
        self.send(json!({"type": "setup_state"}));
        self.send(json!({"type": "setup_models"}));
    }
    pub fn answer(&mut self, id: u64, answer: &str) {
        self.send(json!({"type": "answer", "id": id, "answer": answer}));
        self.asks.retain(|a| a.id != id);
    }
    pub fn next_mode(&self) -> &'static str {
        let i = MODES.iter().position(|m| *m == self.mode).map(|i| i + 1).unwrap_or(0);
        MODES[i % MODES.len()]
    }
    /// "src/a.rs:12" or "src/a.rs" → (path, line) when it's one of the project's files.
    pub fn file_ref(&self, s: &str) -> Option<(String, Option<usize>)> {
        let s = s.trim().trim_start_matches("./");
        if self.file_set.contains(s) { return Some((s.into(), None)) }
        let (p, rest) = s.split_once(':')?;
        let line = rest.split(':').next().and_then(|l| l.parse().ok());
        self.file_set.contains(p).then(|| (p.to_string(), line))
    }
    pub fn read_file(&self, rel: &str) -> String {
        std::fs::read(std::path::Path::new(&self.path).join(rel)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
    }
    /// Ask serve for what a page shows.
    pub fn load_page(&self, page: &str) {
        match page {
            "review" => self.cmd("changes", json!({})),
            "checkpoints" => self.cmd("checkpoints", json!({})),
            "memory" => self.cmd("memory", json!({})),
            "activity" => self.cmd("activity", json!({})),
            "tasks" => self.cmd("tasks", json!({})),
            _ => {}
        }
    }

    /// One event line from `qh serve`. Returns true when the setup screen's data changed.
    pub fn handle(&mut self, e: &Value) -> bool {
        let ty = e["type"].as_str().unwrap_or("");
        let tab = e["tab"].as_str().unwrap_or("main").to_string();
        let s = |k: &str| e[k].as_str().unwrap_or("").to_string();
        match ty {
            "snapshot" => {
                self.state = "live".into();
                self.mode = s("mode");
                self.tabs.clear();
                for t in e["tabs"].as_array().into_iter().flatten() { self.fill(t) }
                if !self.tabs.iter().any(|t| t.name == self.active) { self.active = self.tabs.first().map(|t| t.name.clone()).unwrap_or("main".into()) }
                for ty in ["sessions", "files", "commands"] { self.cmd(ty, json!({})) }
                self.cmd("checkpoints", json!({}));
                self.cmd("changes", json!({}));
                if e["needs_setup"].as_bool() == Some(true) { self.show_setup = true; self.load_setup(); return true }
            }
            "history" => { self.fill(e); self.send(json!({"type": "sessions"})) }
            "tabs" => {
                if let Some(m) = e["mode"].as_str() { self.mode = m.into() }
                let list = e["tabs"].as_array().cloned().unwrap_or_default();
                let names: Vec<String> = list.iter().filter_map(|t| t["name"].as_str().map(String::from)).collect();
                for t in &list { self.tab(t["name"].as_str().unwrap_or("")).update(t) }
                self.tabs.retain(|t| names.contains(&t.name));
                if !names.contains(&self.active) { self.active = names.first().cloned().unwrap_or("main".into()) }
            }
            "active" => {
                let a = s("tab");
                if self.tabs.iter().any(|t| t.name == a) { self.active = a }
                self.send(json!({"type": "sessions"}));
            }
            "sessions" => {
                self.sessions = e["sessions"].as_array().into_iter().flatten().map(|x| Session {
                    id: x["id"].as_str().unwrap_or("").into(), title: x["title"].as_str().unwrap_or("").into(),
                    when: x["when"].as_f64().unwrap_or(0.0) }).collect();
                self.open = e["open"].as_array().into_iter().flatten().filter_map(|o|
                    Some((o["id"].as_str()?.to_string(), o["tab"].as_str()?.to_string()))).collect();
            }
            "text" | "tool_call" | "notice" | "compact" | "verify" | "diff" => {
                self.tab(&tab).live += 1;
                if ty == "diff" { self.cmd("changes", json!({"tab": tab})) }
            }
            "error" => {
                if e["re"] == "inline_edit" { return false }
                let t = if e["re"].is_null() { tab } else { self.active.clone() };
                self.tab(&t).live += 1;
            }
            "usage" => { if let Some(c) = e["cost"].as_f64() { self.tab(&tab).cost = Some(c) } }
            "turn_done" => {
                let t = self.tab(&tab);
                t.tokens = e["tokens"].as_u64().unwrap_or(t.tokens);
                self.send(json!({"type": "sessions"}));
                self.cmd("checkpoints", json!({"tab": tab}));
            }
            "confirm" => {
                let id = e["id"].as_u64().unwrap_or(u64::MAX);
                if !self.asks.iter().any(|a| a.id == id) {
                    self.asks.push(Ask { id, tab, tool: s("tool"), args: e["args"].clone(), warn: s("warn"),
                        guard: e["guard"].as_bool().unwrap_or(false), always: s("always") });
                }
            }
            "confirm_closed" => { let id = e["id"].as_u64().unwrap_or(u64::MAX); self.asks.retain(|a| a.id != id) }
            "plan" => {
                let t = self.tab(&tab);
                t.plan = e["items"].as_array().into_iter().flatten()
                    .map(|x| (x["text"].as_str().unwrap_or("").into(), x["status"].as_str().unwrap_or("pending").into())).collect();
                if t.plan.is_empty() { t.plan_ready = false }
            }
            "plan_ready" => self.tab(&tab).plan_ready = true,
            "commands" => {
                self.commands = e["commands"].as_array().into_iter().flatten().filter(|c| c.get("sub").is_none())
                    .map(|c| (c["cmd"].as_str().unwrap_or("").into(), c["about"].as_str().unwrap_or("").into())).collect();
            }
            "files" => {
                self.files = e["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
                self.file_set = self.files.iter().cloned().collect();
            }
            "changes" => {
                self.changes = e["files"].as_array().into_iter().flatten().map(|f| (f["path"].as_str().unwrap_or("").into(),
                    f["before"].as_str().unwrap_or("").into(), f["after"].as_str().unwrap_or("").into())).collect();
            }
            "checkpoints" => {
                self.checkpoints = e["items"].as_array().into_iter().flatten().map(|c| (c["turn"].as_u64().unwrap_or(0) as usize,
                    c["prompt"].as_str().unwrap_or("").into(),
                    c["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect())).collect();
            }
            "rewound" => {
                let p = s("prompt");
                if !p.is_empty() { self.draft = Some(p) }
                self.tab(&tab).live += 1;
            }
            "memory" => {
                let instr = e["instructions"].as_array().into_iter().flatten()
                    .map(|x| (x["path"].as_str().unwrap_or("").into(), x["text"].as_str().unwrap_or("").into())).collect();
                self.memory = Some((s("path"), s("text"), instr));
                self.memory_ver += 1;
            }
            "activity" => {
                if tab != self.current_name() { return false }
                self.activity = e["items"].as_array().into_iter().flatten().map(|x| {
                    let raw = x["t"].as_f64().unwrap_or(0.0);
                    Activity { t: if raw > 1e12 { raw / 1000.0 } else { raw }, kind: x["kind"].as_str().unwrap_or("").into(),
                        text: x["text"].as_str().unwrap_or("").into(), ok: x["ok"].as_bool().unwrap_or(true) }
                }).rev().collect();
            }
            "tasks" => self.tasks = s("text"),
            "team" => {
                if e["team_push"].as_bool() == Some(true) { self.team_push = true }
                if let Some(c) = e["chan"].as_str() { self.chan = c.into() }
                self.team = e.clone();
            }
            "inline_delta" => {
                if let Some(il) = self.inline.as_mut() {
                    if il.id.is_none() || il.id == e["id"].as_u64() { il.id = e["id"].as_u64(); il.preview.push_str(e["delta"].as_str().unwrap_or("")) }
                }
            }
            "inline_result" => { if let Some(il) = self.inline.as_mut() { il.id = e["id"].as_u64() } }
            "inline_applied" => { self.inline = None; self.tab(&tab).live += 1 }
            "setup" => {
                if e["needed"].as_bool() == Some(true) { self.show_setup = true }
                self.setup = Some(e.clone());
                return true;
            }
            "setup_progress" => { self.setup_msg = Some((s("message"), false)); return true }
            "setup_result" => {
                self.setup_busy = false;
                let step = s("step");
                let ok = e["ok"].as_bool().unwrap_or(false);
                let d = &e["data"];
                if !ok {
                    self.setup_msg = Some((e["error"].as_str().unwrap_or("that didn't work").into(), true));
                } else if step == "models" {
                    self.models = d["models"].as_array().into_iter().flatten()
                        .map(|m| (m["id"].as_str().unwrap_or("").into(), m["from"].as_str().unwrap_or("").into())).collect();
                    let needed = self.setup.as_ref().is_some_and(|x| x["needed"].as_bool() != Some(false));
                    if !self.models.is_empty() && self.setup_msg.is_none() && needed { self.setup_msg = Some(("pick a default model below".into(), false)) }
                } else if step == "default" {
                    self.setup_msg = Some((format!("default model: {}", d["model"].as_str().unwrap_or("")), false));
                } else {
                    let m = if step == "chatgpt" { format!("signed in as {}", d["account"].as_str().unwrap_or("")) }
                        else { format!("connected: {} models", d["models"].as_u64().unwrap_or(0)) };
                    self.setup_msg = Some((m, false));
                    self.send(json!({"type": "setup_models"}));
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// A tab from the server with its saved history (snapshot, or a resumed session).
    fn fill(&mut self, t: &Value) {
        let tab = self.tab(t["name"].as_str().unwrap_or("main"));
        tab.update(t);
        tab.items.clear();
        tab.live = 0;
        for m in t["messages"].as_array().into_iter().flatten() {
            let text = m["text"].as_str().unwrap_or("");
            if m["role"] == "user" {
                if !text.is_empty() { tab.items.push(Item::User(text.into())) }
            } else {
                if !text.is_empty() { tab.items.push(Item::Reply(text.into())) }
                for n in m["tools"].as_array().into_iter().flatten() {
                    tab.items.push(Item::Tool(n.as_str().unwrap_or("").into()))
                }
            }
        }
    }
}

/// A tool call's arguments in one short line: its command, path or query.
pub fn brief(a: &Value) -> String {
    let s = match a {
        Value::Object(m) => ["cmd", "path", "pattern", "query", "url", "host", "name"].iter()
            .find_map(|k| m.get(*k).and_then(|v| v.as_str()).map(String::from))
            .or_else(|| m.values().find_map(|v| v.as_str().map(String::from))).unwrap_or_default(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let line = s.lines().next().unwrap_or("").to_string();
    if line.chars().count() > 90 { line.chars().take(90).collect::<String>() + "…" } else { line }
}

pub fn name_of(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or(path.into())
}

/// "openrouter:anthropic/claude-opus-5.5" → "claude-opus-5.5"; a local "qwen3.5:9b" stays whole.
/// Only the provider qh says the tab uses is cut, so a model's own tag never is.
pub fn short_model(m: &str, provider: &str) -> String {
    let rest = if !provider.is_empty() { m.strip_prefix(&format!("{provider}:")).unwrap_or(m) } else { m };
    let s = rest.rsplit('/').next().unwrap_or(rest);
    if s.is_empty() { m.into() } else { s.into() }
}

pub fn ago(when: f64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let d = (now - when).max(0.0) as u64;
    match d { 0..=59 => "now".into(), 60..=3599 => format!("{}m", d / 60), 3600..=86399 => format!("{}h", d / 3600), _ => format!("{}d", d / 86400) }
}

/// ~/.config/qh/app.json (or $XDG_CONFIG_HOME): recent projects, window size and the settings.
/// QH_APP_CONFIG overrides the path.
pub struct Config {
    pub recent: Vec<String>,
    pub width: i32,
    pub height: i32,
    /// None: follow the width (compact under 900 px); Some: what Ctrl+\ last set
    pub compact: Option<bool>,
    pub theme: String,
    pub default_layout: String,
    pub reopen: bool,
}

fn cfg_path() -> PathBuf {
    if let Ok(p) = std::env::var("QH_APP_CONFIG") { return p.into() }
    gtk::glib::user_config_dir().join("qh").join("app.json")
}

impl Config {
    pub fn load() -> Config {
        let v: Value = std::fs::read_to_string(cfg_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Config {
            recent: v["recent"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(),
            width: v["width"].as_i64().unwrap_or(1180) as i32,
            height: v["height"].as_i64().unwrap_or(780) as i32,
            compact: v["compact"].as_bool(),
            theme: v["theme"].as_str().unwrap_or("system").into(),
            default_layout: v["default_layout"].as_str().unwrap_or("chat").into(),
            reopen: v["reopen"].as_bool().unwrap_or(true),
        }
    }
    pub fn save(&self) {
        let p = cfg_path();
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let v = json!({"recent": self.recent, "width": self.width, "height": self.height, "compact": self.compact,
            "theme": self.theme, "default_layout": self.default_layout, "reopen": self.reopen});
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, format!("{v:#}\n")).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }
}

pub struct App {
    pub cfg: Config,
    pub projects: Vec<Project>,
    pub current: Option<String>,
    pub team: bool,
    pub settings: bool,
    pub key_for: Option<String>,
    pub tx: async_channel::Sender<Msg>,
}

impl App {
    pub fn new(tx: async_channel::Sender<Msg>) -> App {
        App { cfg: Config::load(), projects: vec![], current: None, team: false, settings: false, key_for: None, tx }
    }
    pub fn project(&self) -> Option<&Project> { self.projects.iter().find(|p| Some(&p.path) == self.current.as_ref()) }
    pub fn project_mut(&mut self) -> Option<&mut Project> {
        let c = self.current.clone();
        self.projects.iter_mut().find(|p| Some(&p.path) == c.as_ref())
    }
    pub fn by_path(&mut self, path: &str) -> Option<&mut Project> { self.projects.iter_mut().find(|p| p.path == path) }

    /// Opens (or switches to) a project folder: starts its `qh serve` the first time.
    pub fn open(&mut self, path: &str) {
        if !self.projects.iter().any(|p| p.path == path) {
            let client = Client::start(path.to_string(), self.tx.clone());
            self.projects.push(Project {
                path: path.into(), client, tabs: vec![], active: "main".into(), mode: String::new(), state: "starting".into(),
                show_setup: false, setup: None, models: vec![], setup_msg: None, setup_busy: false, sessions: vec![],
                open: HashMap::new(), layout: self.cfg.default_layout.clone(), page: None, asks: vec![], commands: vec![],
                files: vec![], file_set: HashSet::new(), changes: vec![], checkpoints: vec![], memory: None, memory_ver: 0,
                activity: vec![], tasks: String::new(), team: Value::Null, team_push: false, chan: "#team".into(), code: None,
                inline: None, draft: None,
            });
        }
        self.current = Some(path.into());
        self.settings = false;
        self.cfg.recent.retain(|r| r != path);
        self.cfg.recent.insert(0, path.into());
        self.cfg.recent.truncate(12);
        self.cfg.save();
    }

    pub fn shutdown(&self) {
        for p in &self.projects { p.client.stop() }
    }
}
