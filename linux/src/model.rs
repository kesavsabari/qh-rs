//! What the app knows: projects (one `qh serve` each), their tabs and threads, the saved settings.
//! Events from the server become state here (apple/qh/Project.swift `handle`); the UI draws it.

use crate::client::{Client, Msg};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

pub enum Item {
    User(String),
    Reply(String),
    Tool(String, String),
    Notice(String),
    Error(String),
}

pub struct Tab {
    pub name: String,
    pub status: String,
    pub model: String,
    pub provider: String,
    pub purpose: String,
    pub tokens: u64,
    pub items: Vec<Item>,
}

impl Tab {
    fn new(name: &str) -> Tab {
        Tab { name: name.into(), status: "idle".into(), model: String::new(), provider: String::new(),
              purpose: String::new(), tokens: 0, items: vec![] }
    }
    pub fn busy(&self) -> bool { self.status == "working" || self.status == "confirm" }
    pub fn label(&self) -> &str { if self.purpose.is_empty() { &self.name } else { &self.purpose } }
    fn update(&mut self, t: &Value) {
        let s = |k: &str| t[k].as_str().map(String::from);
        if let Some(v) = s("status") { self.status = v }
        if let Some(v) = s("model") { self.model = v }
        if let Some(v) = s("provider") { self.provider = v }
        if let Some(v) = s("purpose") { self.purpose = v }
        if let Some(v) = t["tokens"].as_u64() { self.tokens = v }
    }
    /// Streaming text joins the reply it belongs to; anything else in between starts a new one.
    fn append(&mut self, text: &str) {
        if let Some(Item::Reply(r)) = self.items.last_mut() { r.push_str(text) } else { self.items.push(Item::Reply(text.into())) }
    }
}

pub struct Session {
    pub id: String,
    pub title: String,
    pub when: f64,
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
}

impl Project {
    pub fn name(&self) -> String { name_of(&self.path) }
    pub fn current(&self) -> Option<&Tab> { self.tabs.iter().find(|t| t.name == self.active).or(self.tabs.first()) }
    fn tab(&mut self, name: &str) -> &mut Tab {
        if let Some(i) = self.tabs.iter().position(|t| t.name == name) { return &mut self.tabs[i] }
        self.tabs.push(Tab::new(name));
        self.tabs.last_mut().unwrap()
    }
    pub fn send(&self, v: Value) { self.client.send(v) }

    pub fn prompt(&mut self, text: &str) {
        let t = text.trim();
        let Some(name) = self.current().map(|c| c.name.clone()) else { return };
        if t.is_empty() { return }
        let tab = self.tab(&name);
        if t.starts_with('/') { tab.items.push(Item::Notice(format!("› {t}"))) } else {
            tab.items.push(Item::User(t.into()));
            tab.status = "working".into();
        }
        self.send(json!({"type": "prompt", "tab": name, "text": t}));
    }
    pub fn stop(&self) { if let Some(t) = self.current() { self.send(json!({"type": "stop", "tab": t.name})) } }
    pub fn load_setup(&self) {
        self.send(json!({"type": "setup_state"}));
        self.send(json!({"type": "setup_models"}));
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
                self.send(json!({"type": "sessions"}));
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
            "text" => self.tab(&tab).append(e["delta"].as_str().unwrap_or("")),
            "tool_call" => {
                let args = brief(&e["args"]);
                self.tab(&tab).items.push(Item::Tool(s("name"), args))
            }
            "notice" | "compact" => {
                let m = e["message"].as_str().unwrap_or("compacted the conversation").to_string();
                self.tab(&tab).items.push(Item::Notice(m))
            }
            "error" => {
                let m = e["message"].as_str().unwrap_or("error").to_string();
                let t = if e["re"].is_null() { tab } else { self.active.clone() };
                self.tab(&t).items.push(Item::Error(m))
            }
            "turn_done" => {
                let t = self.tab(&tab);
                t.tokens = e["tokens"].as_u64().unwrap_or(t.tokens);
                self.send(json!({"type": "sessions"}));
            }
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
        for m in t["messages"].as_array().into_iter().flatten() {
            let text = m["text"].as_str().unwrap_or("");
            if m["role"] == "user" {
                if !text.is_empty() { tab.items.push(Item::User(text.into())) }
            } else {
                if !text.is_empty() { tab.items.push(Item::Reply(text.into())) }
                for n in m["tools"].as_array().into_iter().flatten() {
                    tab.items.push(Item::Tool(n.as_str().unwrap_or("").into(), String::new()))
                }
            }
        }
    }
}

/// A tool call's arguments in one short line.
fn brief(a: &Value) -> String {
    let s = match a {
        Value::Object(m) => m.values().filter_map(|v| v.as_str().map(String::from))
            .next().unwrap_or_default(),
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

/// ~/.config/qh/app.json (or $XDG_CONFIG_HOME): recent projects and the window size. QH_APP_CONFIG overrides.
pub struct Config {
    pub recent: Vec<String>,
    pub width: i32,
    pub height: i32,
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
        }
    }
    pub fn save(&self) {
        let p = cfg_path();
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        let v = json!({"recent": self.recent, "width": self.width, "height": self.height});
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, format!("{v:#}\n")).is_ok() { let _ = std::fs::rename(&tmp, &p); }
    }
}

pub struct App {
    pub cfg: Config,
    pub projects: Vec<Project>,
    pub current: Option<String>,
    pub team: bool,
    pub key_for: Option<String>,
    pub tx: async_channel::Sender<Msg>,
}

impl App {
    pub fn new(tx: async_channel::Sender<Msg>) -> App {
        App { cfg: Config::load(), projects: vec![], current: None, team: false, key_for: None, tx }
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
                open: HashMap::new(), layout: "chat".into(),
            });
        }
        self.current = Some(path.into());
        self.cfg.recent.retain(|r| r != path);
        self.cfg.recent.insert(0, path.into());
        self.cfg.recent.truncate(12);
        self.cfg.save();
    }

    pub fn shutdown(&self) {
        for p in &self.projects { p.client.stop() }
    }
}
