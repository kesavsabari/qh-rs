//! Several named sessions in one process that can message each other. The hub
//! owns the turn threads, so it runs without a UI; the UI only renders events.
//! One GPU means one generation at a time: every model call takes `model`.
use crate::checkpoints::Checkpoints;
use crate::config::{state_dir, Config};
use crate::context::{content, role};
use crate::tools::schema;
use crate::{engine, prompt, session, tools};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const MAX_HOPS: u32 = 6; // automatic message chain length before waiting for the user
const ASK_TIMEOUT: u64 = 180; // seconds an ask() waits while the target sits idle without answering
const MUTATING: [&str; 3] = ["write_file", "edit_file", "rename_symbol"];
pub const MODES: [&str; 4] = ["ask", "auto-edit", "auto", "plan"];

pub fn mode_help(mode: &str) -> &'static str {
    match mode {
        "auto-edit" => "file edits run without asking; bash and browser typing still ask",
        "plan" => "read-only: the agent explores and writes a plan; nothing is changed until you switch mode",
        "auto" => "everything runs without asking except dangerous commands (rm, sudo, git push…) and pay/delete clicks",
        _ => "asks before anything that changes something",
    }
}

pub const HUB_TOOLS: [&str; 5] = ["list_sessions", "read_session", "send_message", "ask", "spawn"];

pub enum Answer {
    Yes,
    Always(String),
    No,
}

pub enum Event {
    Token(String),
    ToolCall { name: String, args: Value },
    ToolResult { #[allow(dead_code)] name: String, result: String },
    Compact { stage: &'static str, before: usize, after: usize },
    Notice(String),
    Done { tokens: u64 },
    /// `guard`: the call crosses a safety line (sandbox::guard); it asks in every mode and offers no "always"
    /// (network asks excepted: those may add a `net:` rule)
    Confirm { tool: String, args: Value, reply: Sender<Answer>, cwd: PathBuf, warn: String, guard: bool },
    Added,
    Log(String),
    Changed,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct Settings {
    pub think: bool,
    pub effort: String, // with think on: "" = the model's default, else low | medium | high
    pub model: String,
    pub provider: String, // "" = the local Ollama in config; else a [providers.x] name
    pub profile: String,
    pub allow: Vec<String>,
    pub tools: Vec<Value>,
}

pub struct Turn {
    pub usage: crate::usage::Usage, // this tab's tokens and $ this run
    pub plan_pct: Option<f64>, // ChatGPT plan window used, when the backend says
    pub browsing: bool, // the model has opened a page: send the rest of the browser tools
    pub tokens: u64,
    pub chars_at: usize,
    pub streamed: usize,
    pub ratio: f64,
    pub tps: f64,
    pub changed: BTreeSet<String>,
    pub ckpt: Checkpoints,
}

struct Msg {
    sender: String,
    text: String,
    hops: u32,
    reply: Option<Sender<String>>,
    report: bool, // a completion report: never itself reported back, so two sessions can't ping-pong
}

#[derive(Default)]
pub struct Ctl {
    inbox: VecDeque<Msg>,
    pub hops: u32,
    reply: Option<Sender<String>>,
    /// Sessions that get this turn's final reply automatically: the asker, or whoever sent the work.
    owed: Vec<String>,
    pub asking: String,
    pub running: bool,
}

pub struct Member {
    pub name: String,
    pub purpose: Mutex<String>, // the session's role: set by spawn, /role, or named from its first real request
    /// The role was only named from the first request: it labels the tab and routes work, but stays
    /// out of the system prompt (it would tell a lone session to refuse other work, and changing
    /// the prompt mid-conversation throws away the provider's prompt cache).
    pub named: AtomicBool,
    pub spawner: String,
    pub session: Mutex<PathBuf>,
    pub cwd: Mutex<PathBuf>, // this session's working folder; bash `cd` moves only this session
    /// the project, fixed when the session starts: file tools write here without asking, and
    /// sandboxed commands may write here (a `cd` elsewhere doesn't move it)
    pub root: PathBuf,
    /// this session's allowlist proxy for sandboxed commands, started on first use: (port, Unix socket)
    proxy: std::sync::OnceLock<Option<(u16, Option<PathBuf>)>>,
    pub msgs: Mutex<Vec<Value>>,
    pub set: Mutex<Settings>,
    pub turn: Mutex<Turn>,
    pub status: Mutex<&'static str>,
    pub ctl: Mutex<Ctl>,
    pub cancel: AtomicBool,
    pub thinking: AtomicBool, // the model is streaming its reasoning right now
    chars: AtomicUsize, // size of msgs, refreshed on every change so the UI never re-measures
}

impl Member {
    /// Call after changing msgs.
    pub fn touched(&self, msgs: &[Value]) {
        let now = crate::context::chars(msgs);
        self.chars.store(now, Relaxed);
        // shrunk (compact, /clear): the last request's count is stale, so re-estimate from the
        // size now, or the status bar keeps showing the old total until the next reply
        let mut t = lock(&self.turn);
        if now < t.chars_at {
            t.tokens = (now as f64 / t.ratio) as u64;
            (t.chars_at, t.streamed) = (now, 0);
        }
    }

    /// Add a message and append it to the transcript right away, so a long turn survives
    /// the process being killed.
    pub fn push(&self, msg: Value) {
        let mut msgs = lock(&self.msgs);
        self.chars.fetch_add(crate::context::chars(std::slice::from_ref(&msg)), Relaxed);
        session::append(&lock(&self.session), &msg);
        msgs.push(msg);
    }

    pub fn cwd(&self) -> PathBuf {
        lock(&self.cwd).clone()
    }

    /// The sandbox context as it stands, without starting the proxy (for /sandbox).
    pub fn sandbox_now(&self) -> crate::sandbox::Ctx {
        crate::sandbox::Ctx { root: self.root.clone(), proxy: self.proxy.get().cloned().flatten(), tab: self.name.clone() }
    }

    pub fn status(&self) -> &'static str {
        *lock(&self.status)
    }

    /// Exact count from the last model call plus an estimate of everything added since.
    pub fn live_tokens(&self) -> u64 {
        let chars = self.chars.load(Relaxed);
        let t = lock(&self.turn);
        t.tokens + t.streamed as u64 + (chars.saturating_sub(t.chars_at) as f64 / t.ratio) as u64
    }
}

pub type Emit = Box<dyn Fn(&str, Event) + Send + Sync>;

/// A message between sessions, kept briefly for the map view.
#[derive(Clone)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub kind: &'static str, // msg | ask | report | spawn
    pub at: std::time::Instant,
}

pub struct Hub {
    pub cfg: Config,
    pub id: String,
    emit: Emit,
    pub members: Mutex<Vec<Arc<Member>>>,
    pub model: Fifo,
    pub holder: Mutex<String>,
    files: Mutex<HashMap<String, String>>,
    pub written: Mutex<HashMap<PathBuf, String>>, // file -> the session that last wrote it this run
    pub links: Mutex<VecDeque<Link>>,
    pub paused: AtomicBool,
    pub auto_yes: bool,
    mode: AtomicU8, // index into MODES
    /// every tab's tokens and $ this run, closed tabs included
    pub spent: Mutex<crate::usage::Usage>,
    pub cost_warn: crate::usage::Warn,
}

fn hubs_dir() -> PathBuf {
    state_dir().join("hubs")
}

impl Hub {
    pub fn new(cfg: Config, id: Option<String>, emit: Emit, auto_yes: bool) -> Arc<Hub> {
        let cfg_mode = cfg.mode.clone();
        Arc::new(Hub {
            cfg, id: id.unwrap_or_else(session::stamp), emit, members: Mutex::new(vec![]),
            model: Fifo::default(), holder: Mutex::new(String::new()), files: Mutex::new(HashMap::new()),
            written: Mutex::new(HashMap::new()), links: Mutex::new(VecDeque::new()),
            mode: AtomicU8::new(MODES.iter().position(|m| *m == cfg_mode).unwrap_or(0) as u8),
            paused: AtomicBool::new(false), auto_yes,
            spent: Mutex::new(Default::default()), cost_warn: Default::default(),
        })
    }

/// The model new sessions start on: `/model default`, kept across runs.
    pub fn default_model(&self) -> (String, String) {
        let saved = std::fs::read_to_string(state_dir().join("default_model")).unwrap_or_default();
        match saved.trim() {
            "" => (self.cfg.default_provider.clone(), self.cfg.model.clone()),
            id => crate::providers::parse_model(id),
        }
    }

    pub fn set_default_model(&self, provider: &str, model: &str) {
        let _ = std::fs::create_dir_all(state_dir());
        let _ = std::fs::write(state_dir().join("default_model"), crate::providers::display(provider, model));
    }

    pub fn emit(&self, name: &str, ev: Event) {
        (self.emit)(name, ev)
    }

    pub fn changed(&self) {
        self.emit("", Event::Changed)
    }

    pub fn get(&self, name: &str) -> Option<Arc<Member>> {
        lock(&self.members).iter().find(|m| m.name == name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        lock(&self.members).iter().map(|m| m.name.clone()).collect()
    }

    pub fn system_for(&self, name: &str, profile: &str, cwd: &Path) -> String {
        format!("{}\n\nYou are session '{name}'; the user sees sessions as tabs by name (\"the tests tab\" = \
session tests). Messages from other sessions start with [from <name>]. qh saves every session as it goes; \
`qh --resume` reopens the last workspace after quitting.", prompt::system(profile, cwd, &crate::tier::Limits::for_ctx(self.cfg.ctx)))
    }

    /// The tools sent with a request. Everything stays callable, but tool definitions are
    /// resent every call, so groups are only shown when they can matter: the browser's
    /// follow-up tools once a page is open, session messaging once there is another session.
    pub fn visible_tools(&self, m: &Member) -> Vec<Value> {
        let (tools, profile) = { let s = lock(&m.set); (s.tools.clone(), s.profile.clone()) };
        let solo = lock(&self.members).len() < 2;
        let browsing = profile == "browser" || lock(&m.turn).browsing;
        let (provider, model) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
        let sees = crate::providers::resolve(&provider)
            .is_none_or(|p| p.kind != crate::providers::Kind::Ollama || crate::tier::caps(&p.base, &model).vision);
        let team = crate::team::running_here(&m.cwd());
        tools.into_iter().filter(|t| {
            let n = t["function"]["name"].as_str().unwrap_or("");
            if (n == "screenshot" && !sees) || (matches!(n, "ask_team" | "tell_team" | "read_team_file" | "list_team_files") && !team) {
                return false;
            }
            !(solo && matches!(n, "list_sessions" | "read_session" | "send_message" | "ask"))
                && (browsing || n == "browse" || !crate::browser::TOOLS.contains(&n))
        }).collect()
    }

    /// System prompt plus the session's role, if it has one.
    pub fn system_msg(&self, name: &str, profile: &str, role: &str, cwd: &Path) -> String {
        let mut s = self.system_for(name, profile, cwd);
        if let Some(n) = crate::ide::note() {
            s += "\n";
            s += n;
        }
        if !role.is_empty() {
            s += &format!("\nYour role in this workspace: {role}. Stay on it; send other work to the session it belongs to.");
        }
        s
    }

    /// Name a session's role from its first request: the label only, the prompt is unchanged.
    pub fn name_role(&self, m: &Member, role: &str) {
        *lock(&m.purpose) = role.to_string();
        m.named.store(true, Relaxed);
        session::set_title(&lock(&m.session), role);
        self.save();
        self.changed();
    }

    /// The role that goes into the system prompt: none when it was only named automatically.
    fn prompt_role(m: &Member) -> String {
        if m.named.load(Relaxed) { String::new() } else { lock(&m.purpose).clone() }
    }

    /// Give a session a role and put it into its system prompt.
    pub fn set_role(&self, m: &Member, role: &str) {
        *lock(&m.purpose) = role.to_string();
        m.named.store(false, Relaxed);
        let profile = lock(&m.set).profile.clone();
        let sys = self.system_msg(&m.name, &profile, role, &m.cwd());
        let mut msgs = lock(&m.msgs);
        if let Some(first) = msgs.first_mut() {
            first["content"] = sys.into();
        }
        m.touched(&msgs);
        drop(msgs);
        self.save();
        self.changed();
    }

    /// A spawned worker gets no `spawn`: it does its task and reports back, instead of
    /// spawning more copies of itself.
    pub fn tools_for(&self, profile: &str, spawned: bool) -> Vec<Value> {
        let mut t = tools::build(profile, &self.cfg);
        t.extend(hub_schemas().into_iter().filter(|s| !(spawned && s["function"]["name"] == "spawn")));
        // a second opinion: not in a tier S window, where every schema costs
        if profile != "browser" && crate::tier::tier(self.cfg.ctx) != "S" {
            t.push(crate::route::ask_schema());
        }
        if !self.cfg.fetch_model.is_empty() && profile != "browser" {
            t.push(crate::route::fetch_schema());
        }
        // inside VS Code: the editor's context, problems, and opening files there
        if crate::ide::active() && profile != "browser" {
            t.extend(crate::ide::schemas());
        }
        t
    }

    pub fn add(&self, name: &str, profile: Option<&str>, purpose: &str, spawner: &str,
               path: Option<PathBuf>, think: bool, cwd: Option<PathBuf>) -> Result<Arc<Member>, String> {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err("session names are one word".into());
        }
        if self.get(name).is_some() {
            return Err(format!("session {name} already exists"));
        }
        let profile = profile.unwrap_or(&self.cfg.profile).to_string();
        let mut msgs = path.as_deref().map(session::load).unwrap_or_default();
        let path = path.unwrap_or_else(session::new_path);
        let cwd = cwd.filter(|c| c.is_dir()).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| crate::config::home()));
        let sys = json!({"role": "system", "content": self.system_msg(name, &profile, purpose, &cwd)});
        let default = self.default_model();
        if msgs.first().is_some_and(|m| role(m) == "system") {
            msgs[0] = sys;
        } else {
            msgs.insert(0, sys);
        }
        let m = Arc::new(Member {
            name: name.into(), purpose: Mutex::new(purpose.into()), spawner: spawner.into(),
            session: Mutex::new(path.clone()),
            root: crate::sandbox::canon(&cwd),
            proxy: std::sync::OnceLock::new(),
            cwd: Mutex::new(cwd),
            msgs: Mutex::new(msgs),
            set: Mutex::new(Settings {
                effort: String::new(),
                think, model: default.1, provider: default.0, tools: self.tools_for(&profile, !spawner.is_empty()), profile,
                allow: self.cfg.allow.clone(),
            }),
            turn: Mutex::new(new_turn(&path)),
            status: Mutex::new("idle"),
            ctl: Mutex::new(Ctl::default()),
            cancel: AtomicBool::new(false),
            thinking: AtomicBool::new(false),
            chars: AtomicUsize::new(0),
            named: AtomicBool::new(false),
        });
        m.touched(&lock(&m.msgs));
        lock(&self.members).push(m.clone());
        self.save();
        self.emit(name, Event::Added);
        Ok(m)
    }

    /// Fresh session for a member (/clear, /profile): new transcript, same name.
    pub fn reset(&self, m: &Member, profile: Option<&str>) {
        let path = session::new_path();
        let mut set = lock(&m.set);
        if let Some(p) = profile {
            set.profile = p.into();
            set.tools = self.tools_for(p, !m.spawner.is_empty());
        }
        let msgs = vec![json!({"role": "system", "content": self.system_msg(&m.name, &set.profile, &Self::prompt_role(m), &m.cwd())})];
        m.touched(&msgs);
        *lock(&m.msgs) = msgs;
        *lock(&m.turn) = new_turn(&path);
        *lock(&m.session) = path;
        drop(set);
        self.save();
    }

    pub fn close(&self, name: &str) {
        let m = {
            let mut ms = lock(&self.members);
            let i = ms.iter().position(|m| m.name == name);
            i.map(|i| ms.remove(i))
        };
        if let Some(m) = m {
            m.cancel.store(true, Relaxed);
            crate::bg::kill_tab(name);
            self.release_files(name);
        }
        self.save();
        self.changed();
    }

    pub fn save(&self) {
        let ms: Vec<Value> = lock(&self.members).iter().map(|m| json!({
            "name": m.name, "profile": lock(&m.set).profile, "purpose": *lock(&m.purpose), "named": m.named.load(Relaxed), "spawner": m.spawner,
            "session": lock(&m.session).display().to_string(), "cwd": m.cwd().display().to_string(),
            "root": m.root.display().to_string(), "model": lock(&m.set).model.clone(), "provider": lock(&m.set).provider.clone()})).collect();
        let _ = std::fs::create_dir_all(hubs_dir());
        let _ = std::fs::write(hubs_dir().join(format!("{}.json", self.id)),
                               serde_json::to_string_pretty(&json!({"members": ms})).unwrap_or_default());
    }

    pub fn latest() -> Option<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(hubs_dir()).into_iter().flatten().flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        v.sort();
        v.pop()
    }

    pub fn restore(&self, path: &Path) {
        let data: Value = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for d in data["members"].as_array().into_iter().flatten() {
            let g = |k: &str| d[k].as_str().unwrap_or("").to_string();
            let sp = PathBuf::from(g("session"));
            let p = g("profile");
            let cwd = Some(PathBuf::from(g("cwd"))).filter(|c| !c.as_os_str().is_empty());
            let root = Some(PathBuf::from(g("root"))).filter(|c| c.is_dir()).or(cwd.clone());
            let named = d["named"].as_bool().unwrap_or(false);
            let role = if named { String::new() } else { g("purpose") };
            if let Ok(m) = self.add(&g("name"), Some(p.as_str()).filter(|p| !p.is_empty()), &role, &g("spawner"),
                                    Some(sp).filter(|p| p.exists()), false, root) {
                if let Some(c) = cwd.filter(|c| c.is_dir()) {
                    *lock(&m.cwd) = c; // where its last `cd` left it, inside or outside the project
                }
                if named {
                    *lock(&m.purpose) = g("purpose");
                    m.named.store(true, Relaxed);
                }
                let mut s = lock(&m.set);
                if !g("model").is_empty() {
                    s.model = g("model");
                    s.provider = g("provider");
                }
            }
        }
    }

    // ---- turns ------------------------------------------------------------

    fn link(&self, from: &str, to: &str, kind: &'static str) {
        let mut l = lock(&self.links);
        if l.len() >= 200 {
            l.pop_front();
        }
        l.push_back(Link { from: from.into(), to: to.into(), kind, at: std::time::Instant::now() });
    }

    /// Queue a message; wake the recipient if it is idle.
    pub fn send(self: &Arc<Self>, to: &str, text: &str, sender: &str, hops: u32, reply: Option<Sender<String>>) -> bool {
        let Some(m) = self.get(to) else { return false };
        if sender != "user" {
            self.link(sender, to, if reply.is_some() { "ask" } else { "msg" });
        }
        if sender != "user" {
            self.emit("", Event::Log(format!("{sender} → {to}: {text}")));
        }
        lock(&m.ctl).inbox.push_back(Msg { sender: sender.into(), text: text.into(), hops, reply, report: false });
        self.next(&m);
        self.changed();
        true
    }

    /// Tell a session that work it sent has finished.
    fn send_report(self: &Arc<Self>, to: &str, text: &str, sender: &str, hops: u32) {
        let Some(m) = self.get(to) else { return };
        self.link(sender, to, "report");
        self.emit("", Event::Log(format!("{sender} → {to}: {}", text.lines().next().unwrap_or(""))));
        lock(&m.ctl).inbox.push_back(Msg { sender: sender.into(), text: text.into(), hops, reply: None, report: true });
        self.next(&m);
        self.changed();
    }

    /// Who should hear when a turn on these messages ends: the senders of work, but not for a
    /// completion report (two sessions would ping-pong), the user (who sees the turn), or a
    /// worker this session spawned (it works for this session, not the other way round).
    fn owed_for(&self, me: &str, batch: &[(String, bool)]) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for (sender, report) in batch {
            let worker = self.get(sender).is_some_and(|s| s.spawner == me);
            if !report && sender != "user" && !worker && self.get(sender).is_some() && !out.contains(sender) {
                out.push(sender.clone());
            }
        }
        out
    }

    /// Messages that belong in the turn already running: completion reports, and follow-ups from
    /// a session waiting on this turn's result. Taken off the inbox so they don't start a turn of
    /// their own after this one.
    pub fn take_followups(&self, m: &Member) -> Vec<String> {
        if self.paused.load(Relaxed) {
            return vec![];
        }
        let mut c = lock(&m.ctl);
        let owed = c.owed.clone();
        let (mut out, mut keep) = (vec![], VecDeque::new());
        while let Some(msg) = c.inbox.pop_front() {
            if msg.reply.is_none() && msg.hops <= MAX_HOPS && (msg.report || owed.contains(&msg.sender)) {
                c.hops = c.hops.max(msg.hops);
                out.push(format!("[from {}] {}", msg.sender, msg.text));
            } else {
                keep.push_back(msg);
            }
        }
        c.inbox = keep;
        out
    }

    fn next(self: &Arc<Self>, m: &Arc<Member>) {
        // looked up before taking ctl: never lock the member list while holding a session's ctl
        if self.get(&m.name).is_none() {
            return;
        }
        let mut c = lock(&m.ctl);
        if c.running || c.inbox.is_empty() {
            return;
        }
        if self.paused.load(Relaxed) || c.inbox[0].hops > MAX_HOPS {
            *lock(&m.status) = "held";
            drop(c);
            self.changed();
            return;
        }
        let msg = c.inbox.pop_front().unwrap();
        c.hops = msg.hops;
        c.running = true;
        let mut text = if msg.sender == "user" { msg.text } else { format!("[from {}] {}", msg.sender, msg.text) };
        // other sessions' messages that queued up while this one was busy go in the same turn,
        // so it answers once instead of once per message
        let mut batch = vec![(msg.sender.clone(), msg.report)];
        if msg.sender != "user" && msg.reply.is_none() {
            while c.inbox.front().is_some_and(|n| n.sender != "user" && n.reply.is_none() && n.hops <= MAX_HOPS) {
                let n = c.inbox.pop_front().unwrap();
                c.hops = c.hops.max(n.hops);
                text += &format!("\n\n[from {}] {}", n.sender, n.text);
                batch.push((n.sender, n.report));
            }
        }
        if msg.reply.is_some() {
            text += &format!("\n(reply with your answer as your final message; it is returned to {})", msg.sender);
        }
        let asker = msg.reply.is_some().then(|| msg.sender.clone());
        c.reply = msg.reply;
        drop(c);
        // set before the turn starts, so its send_message already knows who gets the result
        let owed = match asker { Some(a) => vec![a], None => self.owed_for(&m.name, &batch) };
        lock(&m.ctl).owed = owed;
        m.cancel.store(false, Relaxed);
        *lock(&m.status) = "working";
        let (hub, m, sender) = (self.clone(), m.clone(), msg.sender);
        // a session the user opened gets its role named from its first real request
        let name_it = sender == "user" && lock(&m.purpose).is_empty() && is_work(&text);
        let request = if name_it { text.clone() } else { String::new() };
        std::thread::Builder::new().name(format!("qh-{}", m.name)).spawn(move || {
            if name_it {
                let (hub, m) = (hub.clone(), m.clone());
                std::thread::spawn(move || {
                    if let Some(role) = engine::label(&hub, &m, &request) {
                        if lock(&m.purpose).is_empty() {
                            hub.name_role(&m, &role);
                        }
                    }
                });
            }
            let request = text.clone();
            let fin = engine::run(&hub, &m, text);
            hub.release_files(&m.name);
            // in a team project: refresh this session's fact sheet (decided / next) for teammates
            if crate::team::running_here(&m.cwd()) && !fin.starts_with("error") && fin != "interrupted" {
                let (hub, m, fin) = (hub.clone(), m.clone(), fin.clone());
                std::thread::spawn(move || {
                    if let Some((decided, next)) = engine::sheet(&hub, &m, &request, &fin) {
                        crate::team::set_sheet(&m.name, &m.cwd(), &decided, &next);
                    }
                });
            }
            let (reply, owed, hops) = {
                let mut c = lock(&m.ctl);
                (c.reply.take(), std::mem::take(&mut c.owed), c.hops)
            };
            if let Some(r) = reply {
                let _ = r.send(if fin.is_empty() { "(no answer)".into() } else { fin });
            } else {
                // whoever sent the work hears when it is done, so it can tell the user
                for to in owed {
                    let what = if to == m.spawner { "task done" } else { "finished what you sent" };
                    hub.send_report(&to, &format!("{what}: {fin}\n(This is a completion report; no reply needed. \
                        Tell the user what happened if it matters to them.)"), &m.name, hops + 1);
                }
            }
            *lock(&m.status) = "idle";
            // a teammate's answer queued during this turn's last model call would otherwise wait
            // for the user's next message: take it now (under ctl, so none slips in between)
            let late = { let mut c = lock(&m.ctl); c.running = false; crate::team::answers_for(&m.name) };
            for a in late {
                hub.emit(&m.name, Event::Notice("teammate's answer arrived".into()));
                hub.send(&m.name, &a, "user", 0, None);
            }
            hub.changed();
            hub.next(&m);
        }).expect("spawn turn thread");
    }

    pub fn mode_name(&self) -> &'static str {
        MODES[self.mode.load(Relaxed) as usize % MODES.len()]
    }

    pub fn set_mode(&self, name: &str) -> bool {
        match MODES.iter().position(|m| *m == name) {
            Some(i) => {
                self.mode.store(i as u8, Relaxed);
                self.changed();
                true
            }
            None => false,
        }
    }

    pub fn cycle_mode(&self) -> &'static str {
        let next = (self.mode.load(Relaxed) as usize + 1) % MODES.len();
        self.mode.store(next as u8, Relaxed);
        self.changed();
        MODES[next]
    }

    /// Whether the current mode approves this call without asking.
    fn mode_allows(&self, tool: &str, a: &Value) -> bool {
        let edit = matches!(tool, "write_file" | "edit_file" | "rename_symbol");
        match self.mode_name() {
            "auto-edit" => edit,
            "auto" => match tool {
                "bash" => !tools::DANGER.is_match(a["cmd"].as_str().unwrap_or("")),
                "click" => !tools::COSTLY_CLICK.is_match(&crate::browser::label(a)),
                _ => true,
            },
            _ => false,
        }
    }

    /// Ask the user. `guard` is why the call crosses a safety line: then neither the mode nor -y
    /// approves it, and with nobody to answer (headless) it is refused.
    pub fn confirm(&self, m: &Member, tool: &str, args: &Value, guard: Option<&str>) -> bool {
        if guard.is_none() && (self.auto_yes || self.mode_allows(tool, args)) {
            return true;
        }
        *lock(&m.status) = "confirm";
        self.changed();
        let (tx, rx) = channel();
        let warn = guard.map(String::from).unwrap_or_else(|| self.others_files(m, tool, args));
        self.emit(&m.name, Event::Confirm { tool: tool.into(), args: args.clone(), reply: tx, cwd: m.cwd(), warn, guard: guard.is_some() });
        let ans = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(a) => break a,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !m.cancel.load(Relaxed) => continue,
                _ => break Answer::No,
            }
        };
        *lock(&m.status) = "working";
        self.changed();
        match ans {
            Answer::Yes => true,
            Answer::Always(rule) => {
                self.emit(&m.name, Event::Notice(format!("auto-approving {rule} from now on (/allow lists rules)")));
                lock(&m.set).allow.extend(rule.split(", ").map(String::from));
                true
            }
            Answer::No => false,
        }
    }

    /// What this session's commands run with: its project and its allowlist proxy, whose every new
    /// host outside the allowlist is asked about (in every mode; "always" adds a `net:host` rule).
    pub fn sandbox_ctx(self: &Arc<Self>, m: &Arc<Member>) -> crate::sandbox::Ctx {
        let proxy = m.proxy.get_or_init(|| {
            if !crate::sandbox::enabled(&self.cfg) || cfg!(windows) || self.cfg.sandbox_network != "allowlist" {
                return None;
            }
            let (hub, name) = (Arc::downgrade(self), m.name.clone());
            let decide: crate::proxy::Decide = Arc::new(move |host: &str, port: u16| {
                let Some(hub) = hub.upgrade() else { return false };
                if crate::sandbox::host_listed(host, &hub.cfg.sandbox_hosts) {
                    return true;
                }
                let Some(m) = hub.get(&name) else { return false };
                let target = format!("net:{host}");
                if lock(&m.set).allow.iter().any(|r| r.starts_with("net:") && crate::engine::glob(r.as_bytes(), target.as_bytes())) {
                    return true;
                }
                hub.confirm(&m, "network", &json!({"host": host, "port": port}),
                            Some(&format!("a command wants to reach {host}:{port}, which isn't on the allowlist")))
            });
            match crate::proxy::start(decide) {
                #[cfg(unix)]
                Ok(p) => Some((p.port, p.sock)),
                #[cfg(not(unix))]
                Ok(p) => Some((p.port, None)),
                Err(_) => None, // no proxy: commands get no network
            }
        });
        crate::sandbox::Ctx { root: m.root.clone(), proxy: proxy.clone(), tab: m.name.clone() }
    }

    /// (window, compact-at) for a session. A local model gets the configured window (what fits
    /// in VRAM); a cloud model its whole own window, since it runs on the provider's machines.
    /// `cloud_ctx` (0 = no cap) still caps it for anyone saving plan usage.
    pub fn ctx_for(&self, m: &Member, fetch: bool) -> (usize, usize) {
        let (provider, model) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
        if crate::providers::is_local(&provider, &model) {
            return (self.cfg.ctx, self.cfg.compact_at);
        }
        // with fetch, list the provider's models once so the window comes from it, not the fallback
        // (once per provider per run: a failed list isn't cached, and this runs every turn)
        // (without fetch, e.g. the status bar before the first turn, the list is fetched in the
        // background so the real window shows on a later redraw instead of the fallback)
        if crate::providers::info(&provider, &model).is_none() {
            static TRIED: Mutex<Vec<String>> = Mutex::new(Vec::new());
            let first = { let mut t = lock(&TRIED); !t.contains(&provider) && { t.push(provider.clone()); true } };
            if let (true, Some(p)) = (first, crate::providers::resolve(&provider)) {
                if fetch { let _ = crate::providers::models(&p); } else { std::thread::spawn(move || { let _ = crate::providers::models(&p); }); }
            }
        }
        let known = crate::providers::info(&provider, &model).and_then(|i| i.ctx).or_else(|| {
            let p = crate::providers::resolve(&provider)?;
            (p.kind == crate::providers::Kind::Ollama && p.local)
                .then(|| crate::backend::cloud_ctx(&self.cfg.host, &model, fetch)).flatten()
        });
        let cap = if self.cfg.cloud_ctx == 0 { usize::MAX } else { self.cfg.cloud_ctx };
        let c = known.unwrap_or(32768).min(cap).max(8192);
        (c, c * 88 / 100)
    }

    /// Size limits for a session, from the window its model actually has.
    pub fn limits(&self, m: &Member) -> crate::tier::Limits {
        crate::tier::Limits::for_ctx(self.ctx_for(m, false).0)
    }

    /// Every session idle with nothing queued: no work is in flight anywhere.
    pub fn quiet(&self) -> bool {
        // a copy of the list: holding it while taking a session's ctl lock would deadlock with
        // next(), which holds ctl and looks sessions up
        let ms = lock(&self.members).clone();
        ms.iter().all(|m| {
            let c = lock(&m.ctl);
            !c.running && (c.inbox.is_empty() || m.status() == "held")
        })
    }

    pub fn interrupt(&self, name: &str) {
        if let Some(m) = self.get(name) {
            m.cancel.store(true, Relaxed);
        }
    }

    pub fn pause(self: &Arc<Self>, on: bool) {
        self.paused.store(on, Relaxed);
        if !on {
            for m in lock(&self.members).clone() {
                if m.status() == "held" {
                    self.release(&m.name);
                }
            }
        }
        self.changed();
    }

    /// Let a member's held (hop-limited) messages through.
    pub fn release(self: &Arc<Self>, name: &str) {
        let Some(m) = self.get(name) else { return };
        for msg in lock(&m.ctl).inbox.iter_mut() {
            msg.hops = 0;
        }
        *lock(&m.status) = "idle";
        self.next(&m);
    }

    // ---- shared resources -------------------------------------------------

    pub fn claim_file(&self, me: &str, tool: &str, a: &Value, cwd: &Path) -> Option<String> {
        if !MUTATING.contains(&tool) {
            return None;
        }
        let path = a["path"].as_str().unwrap_or("");
        let key = if !path.is_empty() && tool != "rename_symbol" {
            tools::resolve(cwd, path).display().to_string()
        } else {
            "*project*".into()
        };
        let mut f = lock(&self.files);
        let owner = f.get(&key).or_else(|| f.get("*project*")).filter(|o| *o != me);
        if let Some(o) = owner {
            let what = if key == "*project*" { "the project".into() } else {
                Path::new(&key).file_name().unwrap_or_default().to_string_lossy().to_string()
            };
            return Some(format!("error: {what} is being edited by {o}; ask {o} or wait"));
        }
        f.insert(key, me.into());
        None
    }

    /// Files a session currently holds, for team presence.
    pub fn claimed_files(&self) -> Vec<String> {
        lock(&self.files).keys().cloned().collect()
    }

    fn release_files(&self, name: &str) {
        lock(&self.files).retain(|_, o| o != name);
    }

    /// Remember who wrote a file, so a later delete by someone else can be flagged.
    pub fn note_write(&self, me: &str, path: &str, cwd: &Path) {
        lock(&self.written).insert(tools::resolve(cwd, path), me.into());
    }

    /// For a dangerous bash command: files other sessions wrote this run that it would touch,
    /// e.g. "deletes 3 files s2 wrote (api-gateway/src/main.rs, …)". Empty when none.
    pub fn others_files(&self, m: &Member, tool: &str, a: &Value) -> String {
        let cmd = a["cmd"].as_str().unwrap_or("");
        if tool != "bash" || !tools::DANGER.is_match(cmd) {
            return String::new();
        }
        let cwd = m.cwd();
        let targets: Vec<PathBuf> = cmd.split(|c: char| c.is_whitespace() || ";&|".contains(c))
            .filter(|w| !w.is_empty() && !w.starts_with('-'))
            .flat_map(expand_braces)
            .map(|w| tools::resolve(&cwd, w.trim_matches(|c| c == '"' || c == '\'')))
            .filter(|p| p.exists() && p.components().count() > 3) // never match / or /home/x
            .collect();
        let mut hit: HashMap<String, Vec<String>> = HashMap::new();
        for (f, who) in lock(&self.written).iter() {
            if *who != m.name && targets.iter().any(|t| f.starts_with(t)) {
                hit.entry(who.clone()).or_default().push(f.strip_prefix(&cwd).unwrap_or(f).display().to_string());
            }
        }
        hit.into_iter().map(|(who, fs)| {
            let more = if fs.len() > 2 { ", …" } else { "" };
            format!("touches {} file(s) {who} wrote ({}{more})", fs.len(), fs[..fs.len().min(2)].join(", "))
        }).collect::<Vec<_>>().join("; ")
    }

    // ---- tools given to every member -------------------------------------

    pub fn tool(self: &Arc<Self>, me: &Member, name: &str, a: &Value) -> String {
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        let others = || self.names().join(", ");
        match name {
            "list_sessions" => lock(&self.members).iter().map(|m| {
                let mut row = format!("{}{}: {}", m.name, if m.name == me.name { " (you)" } else { "" }, m.status());
                let role = lock(&m.purpose).clone();
                if !role.is_empty() {
                    row += &format!(" — {role}");
                }
                if m.name != me.name {
                    let (task, out) = (last(m, "user"), last(m, "assistant"));
                    row += &if task.is_empty() { "\n  (no messages yet)".into() } else { format!("\n  task: {task}") };
                    if !out.is_empty() {
                        row += &format!("\n  latest: {out}");
                    }
                }
                row
            }).collect::<Vec<_>>().join("\n"),
            "read_session" => {
                let Some(m) = self.get(&s("name")) else { return format!("error: no session {}. Sessions: {}", s("name"), others()) };
                let n = a["last"].as_u64().unwrap_or(8) as usize;
                let msgs = lock(&m.msgs);
                let mut lines = vec![];
                for x in &msgs[1.max(msgs.len().saturating_sub(n * 2))..] {
                    if role(x) == "tool" {
                        lines.push(format!("  [{} → {}]", x["tool_name"].as_str().unwrap_or(""), squash(content(x), 120)));
                    } else if let Some(tc) = x["tool_calls"].as_array() {
                        for c in tc {
                            lines.push(format!("  [calls {} {}]", c["function"]["name"].as_str().unwrap_or(""),
                                               squash(&c["function"]["arguments"].to_string(), 120)));
                        }
                    } else if !content(x).is_empty() {
                        lines.push(format!("{}: {}", role(x), squash(content(x), 400)));
                    }
                }
                let body = lines[lines.len().saturating_sub(n * 3)..].join("\n");
                format!("{} is {}. Recent conversation:\n{}", m.name, m.status(), if body.is_empty() { "(empty)" } else { &body })
            }
            "send_message" => {
                let to = s("to");
                if to == me.name {
                    return "error: cannot message yourself".into();
                }
                // the result already goes back on its own; a message too would make it answer twice
                if lock(&me.ctl).owed.contains(&to) {
                    return format!("error: not sent. {to} gets your final reply automatically when you finish this turn; \
put your result or answer there.");
                }
                let hops = lock(&me.ctl).hops + 1;
                if !self.send(&to, &s("text"), &me.name, hops, None) {
                    // a teammate's login: that's another person's machine, which send_message can't reach
                    let login = to.trim_start_matches('@').split('/').next().unwrap_or("");
                    if crate::team::people().iter().any(|p| p.0 == login) {
                        return format!("error: {login} is a teammate on another machine; send_message only reaches your own tabs. \
Use tell_team (to=\"{login}\") to hand them work, or ask_team to ask them something.");
                    }
                    return format!("error: no session {to}. Sessions: {}", others());
                }
                let held = if hops > MAX_HOPS { " (held: message chain too long, the user must release it)" } else { "" };
                format!("sent to {to}{held}")
            }
            "ask" => {
                let to = s("to");
                if to == me.name {
                    return "error: cannot ask yourself".into();
                }
                if self.get(&to).is_none() {
                    return format!("error: no session {to}. Sessions: {}", others());
                }
                // A asks B while B is blocked asking A (directly or down a chain) would deadlock
                let (mut seen, mut cur) = (BTreeSet::new(), to.clone());
                while !cur.is_empty() && seen.insert(cur.clone()) {
                    if cur == me.name {
                        return format!("error: {to} is waiting on you; answer it instead of asking back");
                    }
                    cur = self.get(&cur).map(|m| lock(&m.ctl).asking.clone()).unwrap_or_default();
                }
                let hops = {
                    let mut c = lock(&me.ctl);
                    c.asking = to.clone();
                    c.hops + 1
                };
                *lock(&me.status) = "asking";
                self.changed();
                let (tx, rx) = channel();
                self.send(&to, &s("question"), &me.name, hops, Some(tx));
                // Wait as long as the target is working or queued (a busy GPU can take many
                // minutes); only time out once it has sat idle with nothing to do. Esc cancels.
                let mut idle = 0u64;
                let got = loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(ans) => break Ok(ans),
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Err("stopped without answering".to_string()),
                        Err(_) if me.cancel.load(Relaxed) => break Err("ask cancelled".to_string()),
                        Err(_) => {
                            let busy = self.get(&to).is_some_and(|t| {
                                let c = lock(&t.ctl);
                                c.running || !c.inbox.is_empty()
                            });
                            idle = if busy { 0 } else { idle + 1 };
                            if idle >= ASK_TIMEOUT {
                                break Err(format!("sat idle {ASK_TIMEOUT}s without answering"));
                            }
                        }
                    }
                };
                let out = match got {
                    Ok(ans) => {
                        self.emit("", Event::Log(format!("  {to} → {}: {ans}", me.name)));
                        format!("{to} answered: {ans}")
                    }
                    Err(why) => format!("error: {to} {why}"),
                };
                lock(&me.ctl).asking.clear();
                *lock(&me.status) = "working";
                self.changed();
                out
            }
            "spawn" => {
                let (name, task) = (s("name"), s("task"));
                if self.get(&name).is_some() {
                    return format!("error: session {name} exists; use send_message");
                }
                let profile = Some(s("profile")).filter(|p| !p.is_empty());
                let purpose: String = task.chars().take(80).collect();
                let cwd = me.cwd();
                match self.add(&name, profile.as_deref(), &purpose, &me.name, None, false, Some(cwd.clone())) {
                    Ok(w) => {
                        // a worker runs on its parent's model and provider
                        let (model, provider) = { let s = lock(&me.set); (s.model.clone(), s.provider.clone()) };
                        let mut s = lock(&w.set);
                        (s.model, s.provider) = (model, provider);
                    }
                    Err(e) => return format!("error: {e}"),
                }
                let hops = lock(&me.ctl).hops + 1;
                self.send(&name, &format!("{task}\nYou are working in {} (relative paths resolve there).\n\
When finished, reply with a short summary; it is sent back automatically.", cwd.display()),
                          &me.name, hops, None);
                format!("spawned {name}; its result will arrive as a message from {name}")
            }
            _ => format!("error: unknown tool {name}"),
        }
    }
}

/// `a/{b,c}/d` -> `a/b/d`, `a/c/d` (one level, enough for the paths models write).
fn expand_braces(w: &str) -> Vec<String> {
    match (w.find('{'), w.find('}')) {
        (Some(i), Some(j)) if i < j => w[i + 1..j].split(',').map(|x| format!("{}{x}{}", &w[..i], &w[j + 1..])).collect(),
        _ => vec![w.to_string()],
    }
}

/// A request that starts a job, not a question or chit-chat: worth naming the session after.
fn is_work(text: &str) -> bool {
    const ASKING: [&str; 12] = ["who", "what", "when", "where", "why", "how", "is", "are", "can", "could", "do", "does"];
    let t = text.trim();
    let first = t.split_whitespace().next().unwrap_or("").to_lowercase();
    t.split_whitespace().count() >= 5 && !t.ends_with('?') && !ASKING.contains(&first.as_str())
}

/// The model lock, granted in arrival order: a busy session can't keep winning it.
#[derive(Default)]
pub struct Fifo {
    q: Mutex<(u64, u64)>, // (next ticket, now serving)
    cv: std::sync::Condvar,
}

pub struct FifoGuard<'a>(&'a Fifo);

impl Drop for FifoGuard<'_> {
    fn drop(&mut self) {
        lock(&self.0.q).1 += 1;
        self.0.cv.notify_all();
    }
}

impl Fifo {
    /// Take a ticket; `waiting` runs once if the lock is not free right away.
    pub fn acquire(&self, waiting: impl FnOnce()) -> FifoGuard<'_> {
        let mut q = lock(&self.q);
        let me = q.0;
        q.0 += 1;
        if q.1 != me {
            waiting();
            while q.1 != me {
                q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        }
        FifoGuard(self)
    }
}

fn new_turn(path: &Path) -> Turn {
    let dir = path.parent().unwrap_or(Path::new(".")).join("checkpoints")
        .join(path.file_stem().unwrap_or_default());
    Turn { usage: Default::default(), plan_pct: None, browsing: false, tokens: 0, chars_at: 0, streamed: 0, ratio: 3.5, tps: 0.0, changed: BTreeSet::new(), ckpt: Checkpoints::new(dir) }
}

fn squash(s: &str, n: usize) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect()
}

fn last(m: &Member, r: &str) -> String {
    lock(&m.msgs).iter().rev().find(|x| role(x) == r && !content(x).is_empty() && !content(x).starts_with("[Summary"))
        .map(|x| squash(content(x), 160)).unwrap_or_default()
}

/// How files move between teammates, for every place the model learns about them.
pub const TEAM_FILES: &str = "Each teammate has their own git clone on their own machine: there is no shared workspace, and \
nothing you write reaches them by itself. They see your files only with read_team_file (live, read-only) or after you commit \
and push. Never tell a teammate files are \"in the shared workspace\". To hand off finished work: commit it to a branch \
qh/<your login>/<topic>, push it (git push asks the user first), then tell_team them the branch name.";

fn hub_schemas() -> Vec<Value> {
    vec![
        schema("list_sessions", "List sessions (tabs): status, task, latest output.", &[], &[]),
        schema("read_session", "Read another session's recent conversation.",
               &[("name", "string"), ("last", "integer")], &["name"]),
        schema("send_message", "Send a message to another session (does not wait for a reply).",
               &[("to", "string"), ("text", "string")], &["to", "text"]),
        schema("ask", "Ask another session a question and wait for its answer.",
               &[("to", "string"), ("question", "string")], &["to", "question"]),
        schema("spawn", "Start a worker session on a task; its result comes back to you.",
               &[("name", "string"), ("task", "string"), ("profile", "string")], &["name", "task"]),
        schema("ask_team", "Ask a teammate (another person on this team project, with their own agent and their own git clone \
on their own machine) about code they own or are changing: a function's shape, an API, what they're about to change. Name the \
file or function in the question and it goes to whoever is changing or using it. First call: their fact sheet (cheap). deep=true: \
their agent answers from its live session; the answer arrives later as a message, so keep working meanwhile. to = \"login\" or \
\"login/session\" (optional). To see their actual code, use read_team_file instead.",
               &[("question", "string"), ("to", "string"), ("deep", "boolean")], &["question"]),
        schema("tell_team", &format!("Send a teammate's agent a message: hand them their part of a task, or tell them a decision \
they build on (an API shape, a route, a file layout). It waits in their Requests until the person accepts it, then runs in their session on their machine; they may reply or decline instead. to = \"login\" or \
\"login/session\" (their first session when no session is named). Be specific: what to build, and the agreed interface. {TEAM_FILES}"),
               &[("to", "string"), ("text", "string")], &["to", "text"]),
        schema("read_team_file", "Read a file live from a teammate's working copy (their clone, on their machine), uncommitted \
changes included; read-only. Their qh answers within seconds if they're online; secrets, git-ignored files and files outside \
the project are refused. path is relative to the project root. Lines are numbered; pass start_line/end_line for a range \
(at most 2000 lines per call). to = their login.",
               &[("to", "string"), ("path", "string"), ("start_line", "integer"), ("end_line", "integer")], &["to", "path"]),
        schema("list_team_files", "List the files a teammate has changed in their working copy against their last commit \
(git status on their machine), and their branch: a cheap way to find what to read with read_team_file. to = their login.",
               &[("to", "string")], &["to"]),
    ]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // tests that point QH_STATE_DIR somewhere run one at a time
    pub(crate) static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn delete_of_another_sessions_files_is_flagged() {
        let _env = lock(&ENV);
        let root = std::env::temp_dir().join(format!("qh-hub-{}", std::process::id()));
        std::fs::create_dir_all(root.join("api-gateway/src")).unwrap();
        std::fs::write(root.join("api-gateway/src/main.rs"), "x").unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let hub = Hub::new(crate::config::load(), None, Box::new(|_, _| {}), false);
        let core = hub.add("core", None, "", "", None, false, Some(root.clone())).unwrap();
        let s2 = hub.add("s2", None, "", "", None, false, Some(root.clone())).unwrap();
        hub.note_write("s2", "api-gateway/src/main.rs", &s2.cwd());
        let w = hub.others_files(&core, "bash", &json!({"cmd": "rm -rf api-gateway business-logic && mkdir -p x"}));
        assert!(w.contains("s2") && w.contains("main.rs"), "{w}");
        let w = hub.others_files(&core, "bash", &json!({"cmd": "rm -rf ./{gateway,api-gateway}"}));
        assert!(w.contains("s2"), "braces: {w}");
        // its own files and harmless commands raise nothing
        assert!(hub.others_files(&s2, "bash", &json!({"cmd": "rm -rf api-gateway"})).is_empty());
        assert!(hub.others_files(&core, "bash", &json!({"cmd": "ls api-gateway"})).is_empty());
    }

    #[test]
    fn a_named_role_labels_the_tab_but_stays_out_of_the_prompt() {
        let _env = lock(&ENV);
        let root = std::env::temp_dir().join(format!("qh-named-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let sys = |m: &Member| lock(&m.msgs)[0]["content"].as_str().unwrap_or("").to_string();
        let hub = Hub::new(crate::config::load(), Some("n1".into()), Box::new(|_, _| {}), false);
        let a = hub.add("a", None, "", "", None, false, Some(root.clone())).unwrap();
        let before = sys(&a);
        hub.name_role(&a, "fix shop tax calculation");
        assert_eq!(*lock(&a.purpose), "fix shop tax calculation");
        assert_eq!(sys(&a), before, "naming must not change the system prompt");
        // after a restart it is still a label only
        let hub2 = Hub::new(crate::config::load(), Some("n2".into()), Box::new(|_, _| {}), false);
        hub2.restore(&hubs_dir().join("n1.json"));
        let a2 = hub2.get("a").unwrap();
        assert_eq!(*lock(&a2.purpose), "fix shop tax calculation");
        assert!(!sys(&a2).contains("Your role"));
        // a role the user sets does go in
        hub2.set_role(&a2, "tests");
        assert!(sys(&a2).contains("Your role in this workspace: tests."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn helper_results_reach_the_waiting_session_once() {
        let _env = lock(&ENV);
        let root = std::env::temp_dir().join(format!("qh-owed-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let hub = Hub::new(crate::config::load(), Some("o1".into()), Box::new(|_, _| {}), false);
        let main = hub.add("main", None, "", "", None, false, Some(root.clone())).unwrap();
        hub.add("worker", None, "do x", "main", None, false, Some(root.clone())).unwrap();
        hub.add("peer", None, "", "", None, false, Some(root.clone())).unwrap();
        // work from a peer is reported back; the user, reports and main's own worker are not
        let batch = [("peer".to_string(), false), ("user".to_string(), false), ("worker".to_string(), false),
                     ("peer".to_string(), true), ("peer".to_string(), false)];
        assert_eq!(hub.owed_for("main", &batch), vec!["peer".to_string()]);
        assert_eq!(hub.owed_for("worker", &[("main".to_string(), false)]), vec!["main".to_string()]);
        // mid-turn: reports and the waiting session's follow-ups join the turn; the rest wait
        let msg = |sender: &str, report: bool, reply: Option<Sender<String>>| Msg { sender: sender.into(), text: "t".into(), hops: 1, reply, report };
        {
            let mut c = lock(&main.ctl);
            c.owed = vec!["peer".into()];
            c.inbox.push_back(msg("worker", true, None));
            c.inbox.push_back(msg("user", false, None));
            c.inbox.push_back(msg("peer", false, None));
            c.inbox.push_back(msg("peer", false, Some(channel().0)));
            c.inbox.push_back(msg("other", false, None));
        }
        assert_eq!(hub.take_followups(&main), vec!["[from worker] t".to_string(), "[from peer] t".to_string()]);
        let left: Vec<String> = lock(&main.ctl).inbox.iter().map(|m| m.sender.clone()).collect();
        assert_eq!(left, ["user", "peer", "other"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resume_keeps_workers_as_workers() {
        let _env = lock(&ENV);
        let root = std::env::temp_dir().join(format!("qh-resume-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let has_spawn = |m: &Member| lock(&m.set).tools.iter().any(|t| t["function"]["name"] == "spawn");
        let hub = Hub::new(crate::config::load(), Some("r1".into()), Box::new(|_, _| {}), false);
        let main = hub.add("main", None, "", "", None, false, Some(root.clone())).unwrap();
        let w = hub.add("worker", None, "do x", "main", None, false, Some(root.join("sub").clone())).unwrap();
        assert!(has_spawn(&main) && !has_spawn(&w));
        hub.save();
        let again = Hub::new(crate::config::load(), Some("r2".into()), Box::new(|_, _| {}), false);
        again.restore(&root.join("state/hubs/r1.json"));
        let w2 = again.get("worker").unwrap();
        assert!(!has_spawn(&w2), "a resumed worker must not get spawn back");
        assert_eq!(w2.spawner, "main");
    }

    #[test]
    fn work_requests_name_sessions_questions_dont() {
        assert!(is_work("build a rust web app with axum and tests"));
        assert!(!is_work("who are the current attendees in this call"));
        assert!(!is_work("hi there"));
    }

    #[test]
    fn fifo_serves_in_order() {
        let f = Arc::new(Fifo::default());
        let order = Arc::new(Mutex::new(vec![]));
        let g = f.acquire(|| {});
        let hs: Vec<_> = (0..4).map(|i| {
            let (f, o) = (f.clone(), order.clone());
            std::thread::sleep(Duration::from_millis(20));
            std::thread::spawn(move || { let _g = f.acquire(|| {}); o.lock().unwrap().push(i); })
        }).collect();
        std::thread::sleep(Duration::from_millis(50));
        drop(g);
        for h in hs { h.join().unwrap(); }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3]);
    }
}
