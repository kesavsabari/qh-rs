//! The team room (Ctrl+T): everything about the team on one screen. Tabs for the Map / People
//! (with the selected session shown live), Chat (team, private and group channels), Requests
//! (work teammates asked of your sessions: nothing runs until you accept), Tasks (open issues),
//! Pull requests (with CI) and Activity; a bar at the bottom for anything waiting on you.
use crate::hub::{lock, Hub};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `f` relative to the project root, with forward slashes.
pub fn rel(root: &Path, f: &str) -> String {
    let p = Path::new(f);
    match p.strip_prefix(root) {
        Ok(r) => r.to_path_buf(),
        Err(_) => strip_same(&canon(p), &canon(root)).unwrap_or_else(|| p.to_path_buf()),
    }.to_string_lossy().replace('\\', "/")
}

/// The real path, so `~/downloads/x` typed on a Mac and git's `~/Downloads/x` compare equal.
fn canon(p: &Path) -> std::path::PathBuf {
    // a file not written yet: its nearest existing folder, then the rest as given
    let c = p.ancestors().find_map(|a| std::fs::canonicalize(a).ok().map(|c| match p.strip_prefix(a) {
            Ok(rest) if !rest.as_os_str().is_empty() => c.join(rest),
            _ => c,
        }))
        .unwrap_or_else(|| p.to_path_buf());
    // Windows hands back \\?\C:\…, git says C:/…
    match c.to_str().and_then(|s| s.strip_prefix(r"\\?\")) { Some(s) => s.into(), None => c }
}

/// `p` below `root`, the rest of it; letter case is ignored where the disk ignores it (macOS, Windows).
fn strip_same(p: &Path, root: &Path) -> Option<std::path::PathBuf> {
    let same = |a: &std::ffi::OsStr, b: &std::ffi::OsStr| if cfg!(any(target_os = "macos", windows)) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else { a == b };
    let mut pc = p.components();
    for rc in root.components() {
        if !same(pc.next()?.as_os_str(), rc.as_os_str()) { return None }
    }
    Some(pc.as_path().to_path_buf())
}

/// The project a folder belongs to: its git root, with a task folder (`<name>-tasks/<n>`)
/// counted as part of the project it came from. Plain folders are their own project.
pub fn project_root(dir: &Path) -> std::path::PathBuf {
    for d in dir.ancestors() {
        if d.join(".git").exists() {
            if let (Some(tasks), Some(top)) = (d.parent(), d.parent().and_then(|p| p.parent())) {
                if let Some(name) = tasks.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix("-tasks")) {
                    if top.join(name).join(".git").exists() { return top.join(name) }
                }
            }
            return d.to_path_buf();
        }
    }
    dir.to_path_buf()
}

/// Where a project's task branches get their own folders: next to it, in `<name>-tasks/`.
pub fn tasks_dir(root: &Path) -> std::path::PathBuf {
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("project".into());
    root.parent().unwrap_or(root).join(format!("{name}-tasks"))
}

/// Whether `dir` belongs to the project at `root` (the project itself or one of its task folders).
pub fn in_project(dir: &Path, root: &Path) -> bool {
    dir.starts_with(root) || dir.starts_with(tasks_dir(root))
        || [root.to_path_buf(), tasks_dir(root)].iter().any(|r| strip_same(&canon(dir), &canon(r)).is_some())
}

/// The git branch checked out at `dir` (or a short commit id), read from .git without running git.
pub fn branch(dir: &Path) -> String {
    for d in dir.ancestors() {
        let g = d.join(".git");
        let gitdir = if g.is_dir() { g } else if g.is_file() {
            // a worktree: ".git" is a file pointing at its real git dir
            match std::fs::read_to_string(&g).ok().and_then(|t| t.trim().strip_prefix("gitdir:").map(|x| d.join(x.trim()))) {
                Some(p) => p,
                None => continue,
            }
        } else { continue };
        let head = std::fs::read_to_string(gitdir.join("HEAD")).unwrap_or_default();
        let head = head.trim();
        return head.strip_prefix("ref: refs/heads/").map(String::from).unwrap_or_else(|| head.chars().take(7).collect());
    }
    String::new()
}

/// What a session is about, for teammates: (topic, what it's doing now, files it changed).
/// The topic is its role, or else its first real request; "now" is its latest tool call while busy.
pub fn session_info(hub: &Hub, m: &crate::hub::Member, root: &Path) -> (String, String, Vec<String>) {
    let msgs = lock(&m.msgs);
    let one = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut topic = lock(&m.purpose).clone();
    if topic.is_empty() {
        topic = msgs.iter().filter(|x| x["role"] == "user")
            .filter_map(|x| x["content"].as_str())
            .find(|c| !c.starts_with('[') && c.split_whitespace().count() > 2)
            .map(one).unwrap_or_default();
    }
    let busy = !matches!(m.status(), "idle" | "");
    let now = if !busy { String::new() } else {
        msgs.iter().rev().find_map(|x| x["tool_calls"].as_array().and_then(|c| c.last()).cloned()).map(|c| {
            let (name, a) = (c["function"]["name"].as_str().unwrap_or(""), &c["function"]["arguments"]);
            let path = a["path"].as_str().map(|p| rel(root, &crate::tools::resolve(&m.cwd(), p).to_string_lossy()));
            match (name, path) {
                ("edit_file" | "write_file" | "rename_symbol", Some(p)) => format!("editing {p}"),
                ("read_file", Some(p)) => format!("reading {p}"),
                ("bash", _) => format!("running {}", a["command"].as_str().map(one).unwrap_or_default()),
                ("grep" | "find_references", _) => format!("searching {}", a["pattern"].as_str().or(a["symbol"].as_str()).unwrap_or("")),
                (n, _) if n.starts_with("browser") => "using the browser".into(),
                (n, _) => n.replace('_', " "),
            }
        }).unwrap_or_else(|| "thinking".into())
    };
    drop(msgs);
    let mut files: Vec<String> = lock(&hub.written).iter().filter(|(_, w)| **w == m.name)
        .map(|(f, _)| rel(root, &f.to_string_lossy())).collect();
    files.sort();
    (topic, now, files)
}

// ---- GitHub: issues and pull requests, fetched in the background ----

#[derive(Clone, Default)]
struct Gh {
    repo: String,
    issues: Vec<(u64, String, String)>,              // number, title, assignees
    prs: Vec<(u64, String, String, String, String)>, // number, title, author, branch, CI mark
    error: String,
    loading: bool,
}

static SEEN_EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

static GH: Mutex<Option<(Instant, Gh)>> = Mutex::new(None);

fn gh_refresh(repo: &str, force: bool) {
    {
        let mut g = GH.lock().unwrap();
        let fresh = g.as_ref().is_some_and(|(t, d)| d.repo == repo && (d.loading || t.elapsed() < Duration::from_secs(60)));
        if fresh && !force { return }
        let mut d = g.as_ref().filter(|(_, d)| d.repo == repo).map(|(_, d)| d.clone()).unwrap_or_default();
        d.repo = repo.into();
        d.loading = true;
        *g = Some((Instant::now(), d));
    }
    let repo = repo.to_string();
    std::thread::spawn(move || {
        let mut d = Gh { repo: repo.clone(), ..Default::default() };
        match crate::team::token() {
            None => d.error = "no GitHub login: run gh auth login".into(),
            Some(tok) => {
                let get = |p: &str| crate::team::api("GET", p, &tok, None, None).map(|(_, _, v)| v);
                match get(&format!("/repos/{repo}/issues?state=open&per_page=50")) {
                    Ok(v) => d.issues = v.as_array().into_iter().flatten().filter(|i| i["pull_request"].is_null()).map(|i| (
                        i["number"].as_u64().unwrap_or(0), i["title"].as_str().unwrap_or("").to_string(),
                        i["assignees"].as_array().into_iter().flatten().filter_map(|a| a["login"].as_str()).collect::<Vec<_>>().join(", "),
                    )).collect(),
                    Err(e) => d.error = e,
                }
                // pushes and pull requests, into the Activity tab (oldest first, each once)
                if let Ok(v) = get(&format!("/repos/{repo}/events?per_page=30")) {
                    let mut seen = SEEN_EVENTS.lock().unwrap();
                    let first = seen.is_empty();
                    for e in v.as_array().into_iter().flatten().rev() {
                        let id = e["id"].as_str().unwrap_or("").to_string();
                        if id.is_empty() || seen.contains(&id) { continue }
                        seen.push(id);
                        let who = e["actor"]["login"].as_str().unwrap_or("someone");
                        let line = match e["type"].as_str().unwrap_or("") {
                            "PushEvent" => {
                                let br = e["payload"]["ref"].as_str().unwrap_or("").trim_start_matches("refs/heads/");
                                let n = e["payload"]["size"].as_u64().or(e["payload"]["distinct_size"].as_u64()).unwrap_or(0);
                                if n > 0 { format!("↑ {who} pushed {n} commit{} to {br}", if n == 1 { "" } else { "s" }) }
                                else { format!("↑ {who} pushed to {br}") }
                            }
                            "PullRequestEvent" => format!("⇄ {who} {} PR #{}: {}", e["payload"]["action"].as_str().unwrap_or("updated"),
                                e["payload"]["number"], e["payload"]["pull_request"]["title"].as_str().unwrap_or("")),
                            "IssuesEvent" => format!("● {who} {} issue #{}: {}", e["payload"]["action"].as_str().unwrap_or("updated"),
                                e["payload"]["issue"]["number"], e["payload"]["issue"]["title"].as_str().unwrap_or("")),
                            _ => continue,
                        };
                        // the first load only fills in history quietly; later ones are news
                        if !first || seen.len() <= 30 { crate::team::log_to(&repo, &line) }
                    }
                }
                if let Ok(v) = get(&format!("/repos/{repo}/pulls?state=open&per_page=30")) {
                    for p in v.as_array().into_iter().flatten() {
                        let sha = p["head"]["sha"].as_str().unwrap_or("");
                        let ci = get(&format!("/repos/{repo}/commits/{sha}/check-runs")).map(|c| ci_mark(&c)).unwrap_or_default();
                        d.prs.push((p["number"].as_u64().unwrap_or(0), p["title"].as_str().unwrap_or("").into(),
                                    p["user"]["login"].as_str().unwrap_or("").into(), p["head"]["ref"].as_str().unwrap_or("").into(), ci));
                    }
                }
            }
        }
        *GH.lock().unwrap() = Some((Instant::now(), d));
    });
}

fn ci_mark(v: &Value) -> String {
    let runs: Vec<&Value> = v["check_runs"].as_array().into_iter().flatten().collect();
    if runs.is_empty() { return String::new() }
    if runs.iter().any(|r| matches!(r["conclusion"].as_str(), Some("failure" | "timed_out" | "cancelled"))) { return "✗".into() }
    if runs.iter().all(|r| r["conclusion"] == "success" || r["conclusion"] == "skipped") { return "✓".into() }
    "…".into()
}

fn gh() -> Gh {
    GH.lock().unwrap().as_ref().map(|(_, d)| d.clone()).unwrap_or_default()
}

// ---- the room ----

const TABS: [&str; 6] = ["Map", "Chat", "Requests", "Tasks", "Pull requests", "Activity"];

struct Session {
    who: String, // "" = you
    name: String,
    status: String,
    topic: String,
    now: String,
    files: Vec<String>,
    branch: String,
    edits: Vec<(String, Vec<String>, u64)>, // file, functions, latest edit
    waiting: Option<crate::team::Wait>,
    uses: Vec<String>, // names its fresh code uses
}

struct Person {
    who: String,
    label: String,
    online: bool,
    sessions: Vec<Session>,
}

fn people(hub: &Hub, root: &Path) -> Vec<Person> {
    let members: Vec<_> = lock(&hub.members).iter().filter(|m| in_project(&m.cwd(), root)).cloned().collect();
    let mine = members.iter().map(|m| {
        let (topic, now, files) = session_info(hub, m, root);
        let (edits, waiting, uses) = crate::team::mine(&m.name);
        Session { who: String::new(), name: m.name.clone(), status: m.status().to_string(), topic, now, files, branch: branch(&m.cwd()), edits, waiting, uses }
    }).collect::<Vec<_>>();
    let mut out = vec![Person { who: String::new(), label: "you".into(), online: true, sessions: mine }];
    for p in crate::team::overview() {
        out.push(Person {
            label: format!("{}{}{}", p.login, if p.online { String::new() } else { format!("  {}", p.seen) }, if p.view_only { " · view only" } else { "" }),
            online: p.online,
            sessions: p.sessions.into_iter().map(|s| Session { who: p.login.clone(), name: s.name, status: s.status, topic: s.topic, now: s.now,
                files: s.files, branch: s.branch, edits: s.edits, waiting: s.waiting, uses: s.uses }).collect(),
            who: p.login,
        });
    }
    // a big team: people sorted by the part of the code they're in, so the map reads by area
    if out.len() > GROUP_AT { group(&mut out) }
    out
}

/// You first, then everyone by area (away people last), by name within an area.
fn group(out: &mut Vec<Person>) {
    let rest = out.split_off(1.min(out.len()));
    let mut rest: Vec<(String, Person)> = rest.into_iter().map(|p| (code_area(&p), p)).collect();
    rest.sort_by(|a, b| ((a.0 == "away"), &a.0, &a.1.who).cmp(&((b.0 == "away"), &b.0, &b.1.who)));
    out.extend(rest.into_iter().map(|x| x.1));
}

const GROUP_AT: usize = 12; // more people than this: the map groups them by area

/// The part of the code someone is in: the top folder of what their busiest session is changing.
fn code_area(p: &Person) -> String {
    if !p.online && !p.who.is_empty() { return "away".into() }
    let s = p.sessions.iter().find(|s| !matches!(s.status.as_str(), "idle" | "")).or(p.sessions.first());
    let file = s.and_then(|s| s.edits.first().map(|e| e.0.clone()).or_else(|| s.files.last().cloned())).unwrap_or_default();
    match file.split_once('/') {
        Some((dir, _)) => format!("{dir}/"),
        None if file.is_empty() => "no code yet".into(),
        None => "top level".into(),
    }
}

/// What the room asks qh to do after a key.
pub enum Act {
    None,
    Close,
    Open(String),         // switch to one of your sessions
    Work(String, String), // (role, text): send into your active session
    Task(u64, String),    // start issue #n in its own branch, folder and session
    Note(String),
    Err(String),
}

/// What's being typed at the bottom of the room.
#[derive(Clone)]
enum To {
    Chat(String),             // a channel: "#team", "@login", "+<group id>"
    Task(String, String),     // work for a teammate's session (waits in their Requests)
    Question(String, String), // a question for their agent (answered read-only)
    Reply(String, u64),       // writing back to a waiting request
    SendBack(String, String), // what a held push needs before it goes: (pusher, commit)
}

/// Pushes handed to you that you haven't decided yet: they head the Requests tab.
fn open_holds() -> Vec<crate::team::Hold> {
    crate::team::holds().into_iter().filter(|h| h.decided.is_none()).collect()
}

const CHAT: usize = 1;
const REQUESTS: usize = 2;
const ISSUES: usize = 3;
const PRS: usize = 4;

#[derive(Default)]
pub struct TeamView {
    tab: usize,
    sel: [usize; 6],
    typing: Option<(To, String)>,
    scroll: u16,
    person: usize, // the map's selected person
    detail: bool,  // the map opened on one person
}

impl TeamView {
    pub fn open(cwd: &Path) -> Self {
        if let Some((_, r)) = crate::team::github_repo(cwd) { gh_refresh(&r, false) }
        // a request waiting: open on it
        let tab = if crate::team::pending().is_empty() && open_holds().is_empty() { 0 } else { REQUESTS };
        Self { tab, ..Self::default() }
    }

    fn root(cwd: &Path) -> std::path::PathBuf {
        crate::team::github_repo(cwd).map(|(r, _)| r).unwrap_or_else(|| cwd.to_path_buf())
    }

    /// The chat channel picked in the Chat tab.
    fn channel(&self) -> crate::team::Channel {
        let all = crate::team::channels();
        all.get(self.sel[CHAT].min(all.len().saturating_sub(1))).cloned().unwrap_or(crate::team::Channel {
            id: "#team".into(), label: "# team".into(), unread: 0, pinged: false, online: true })
    }

    /// Accept (or with "always", accept and trust) a waiting request: work goes into its session.
    fn accept(hub: &std::sync::Arc<Hub>, i: &crate::team::Incoming, ans: &str) -> Act {
        match crate::team::decide(&i.from, i.id, ans) {
            Some((session, note)) if session.is_empty() => Act::Note(note),
            Some((session, text)) => {
                let to = if hub.get(&session).is_some() { session } else { hub.names().first().cloned().unwrap_or_default() };
                hub.send(&to, &text, "user", 0, None);
                Act::Note(format!("accepted {}'s request: it went to your {to}", i.from))
            }
            None => Act::Note("that request isn't waiting any more".into()),
        }
    }

    pub fn key(&mut self, k: KeyEvent, hub: &std::sync::Arc<Hub>, cwd: &Path) -> Act {
        // writing a message: the keys are the text
        if let Some((to, text)) = self.typing.as_mut() {
            match k.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    let (to, text) = (to.clone(), text.trim().to_string());
                    self.typing = None;
                    if text.is_empty() { return Act::None }
                    let r = match to {
                        To::Chat(c) => crate::team::channel_send(&c, &text),
                        To::Question(who, s) => crate::team::ask_person_agent(&who, &s, &text),
                        To::Task(who, s) => crate::team::send(&who, &s, &text),
                        To::Reply(from, id) => crate::team::reply_request(&from, id, &text),
                        To::SendBack(from, head) => crate::team::decide_hold(&from, &head, false, &text),
                    };
                    return match r { Ok(m) if m.is_empty() => Act::None, Ok(m) => Act::Note(m), Err(e) => Act::Err(e) };
                }
                KeyCode::Backspace => { text.pop(); }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            return Act::None;
        }
        // someone asking to join: l lets them in, n doesn't
        if let Some(who) = crate::team::join_requests().first().cloned() {
            match k.code {
                KeyCode::Char('l') => { crate::team::let_in(&who); return Act::Note(format!("letting {who} in")) }
                KeyCode::Char('n') => { crate::team::not_now(&who); return Act::None }
                _ => {}
            }
        }
        // requests are answered only in their own tab, one picked request at a time: a key
        // pressed elsewhere in the room never accepts anything
        if self.tab == REQUESTS {
            let (holds, pending) = (open_holds(), crate::team::pending());
            self.sel[REQUESTS] = self.sel[REQUESTS].min((holds.len() + pending.len()).saturating_sub(1));
            // a held push: approve it, send it back with what to change, or talk it through
            if let Some(h) = holds.get(self.sel[REQUESTS]).cloned() {
                match k.code {
                    KeyCode::Enter | KeyCode::Char('a') => return match crate::team::decide_hold(&h.from, &h.head, true, "") {
                        Ok(m) => Act::Note(format!("{m}: it goes out as soon as their qh hears")), Err(e) => Act::Err(e) },
                    KeyCode::Char('s') => { self.typing = Some((To::SendBack(h.from.clone(), h.head.clone()), String::new())); return Act::None }
                    KeyCode::Char('r') => { self.typing = Some((To::Chat(format!("@{}", h.from)), String::new())); return Act::None }
                    _ => {}
                }
            }
            if let Some(i) = self.sel[REQUESTS].checked_sub(holds.len()).and_then(|n| pending.get(n)).cloned() {
                match k.code {
                    KeyCode::Enter | KeyCode::Char('a') => return Self::accept(hub, &i, "once"),
                    KeyCode::Char('A') if i.question.is_some() => return Self::accept(hub, &i, "always"),
                    KeyCode::Char('d') => {
                        let q = i.question.is_some();
                        crate::team::decide(&i.from, i.id, "deny");
                        return Act::Note(format!("declined {}'s {}{}", i.from, if q { "question" } else { "request" },
                                                 if q { "" } else { "; they're told privately" }));
                    }
                    KeyCode::Char('r') => { self.typing = Some((To::Reply(i.from.clone(), i.id), String::new())); return Act::None }
                    _ => {}
                }
            }
        }
        let root = Self::root(cwd);
        if self.tab == 0 && !self.detail {
            let ppl = people(hub, &root);
            let n = ppl.len();
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => { self.person = self.person.checked_sub(1).unwrap_or(n.saturating_sub(1)); return Act::None }
                KeyCode::Down | KeyCode::Char('j') => { self.person = (self.person + 1) % n.max(1); return Act::None }
                KeyCode::Enter => {
                    // open on this person: their busiest session picked
                    let before: usize = ppl.iter().take(self.person).map(|p| p.sessions.len()).sum();
                    let off = ppl.get(self.person).map(|p| p.sessions.iter().position(|s| !matches!(s.status.as_str(), "idle" | "")).unwrap_or(0)).unwrap_or(0);
                    self.sel[0] = before + off;
                    self.detail = true;
                    self.scroll = 0;
                    return Act::None;
                }
                KeyCode::Char('a') => {
                    if let Some(p) = ppl.get(self.person).filter(|p| !p.who.is_empty()) {
                        if let Some(s) = p.sessions.iter().find(|s| !matches!(s.status.as_str(), "idle" | "")).or(p.sessions.first()) {
                            self.typing = Some((To::Question(p.who.clone(), s.name.clone()), String::new()));
                        } else { return Act::Note(format!("{} has no sessions open", p.who)) }
                    }
                    return Act::None;
                }
                KeyCode::Char('p') => {
                    // a private message to the picked person
                    if let Some(p) = ppl.get(self.person).filter(|p| !p.who.is_empty()) {
                        let at = format!("@{}", p.who);
                        if let Some(i) = crate::team::channels().iter().position(|c| c.id == at) { self.sel[CHAT] = i }
                        self.tab = CHAT;
                        self.typing = Some((To::Chat(at), String::new()));
                    }
                    return Act::None;
                }
                _ => {}
            }
        }
        if self.tab == 0 && self.detail && k.code == KeyCode::Esc { self.detail = false; return Act::None }
        let n = self.rows(hub, &root);
        let sel = &mut self.sel[self.tab];
        match k.code {
            KeyCode::Esc => return Act::Close,
            KeyCode::Char('t') if k.modifiers.contains(KeyModifiers::CONTROL) => return Act::Close,
            KeyCode::Right | KeyCode::Tab => { self.tab = (self.tab + 1) % TABS.len(); self.scroll = 0 }
            KeyCode::Left | KeyCode::BackTab => { self.tab = (self.tab + TABS.len() - 1) % TABS.len(); self.scroll = 0 }
            KeyCode::Char(c @ '1'..='6') => { self.tab = c as usize - '1' as usize; self.scroll = 0 }
            KeyCode::Up | KeyCode::Char('k') => { *sel = sel.checked_sub(1).unwrap_or(n.saturating_sub(1)); self.scroll = 0 }
            KeyCode::Down | KeyCode::Char('j') => { *sel = (*sel + 1) % n.max(1); self.scroll = 0 }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('r') => if let Some((_, r)) = crate::team::github_repo(cwd) { gh_refresh(&r, true) },
            KeyCode::Char('x') if self.tab == CHAT => {
                let c = self.channel();
                if c.id == "#team" { return Act::Note("the team room can't be cleared; x clears a private or group thread".into()) }
                crate::team::channel_clear(&c.id);
                return Act::Note(format!("cleared {} for you (they keep their copy)", c.label));
            }
            KeyCode::Char('m') | KeyCode::Char('c') if self.tab <= CHAT => {
                self.tab = CHAT;
                self.typing = Some((To::Chat(self.channel().id), String::new()));
            }
            KeyCode::Enter => return self.enter(hub, &root),
            _ => {}
        }
        Act::None
    }

    fn rows(&self, hub: &Hub, root: &Path) -> usize {
        match self.tab {
            0 => people(hub, root).iter().map(|p| p.sessions.len()).sum(),
            CHAT => crate::team::channels().len(),
            REQUESTS => open_holds().len() + crate::team::pending().len(),
            ISSUES => gh().issues.len(),
            PRS => gh().prs.len(),
            _ => crate::team::activity().len(),
        }
    }

    fn picked_session(&self, hub: &Hub, root: &Path) -> Option<Session> {
        people(hub, root).into_iter().flat_map(|p| p.sessions).nth(self.sel[0])
    }

    fn enter(&mut self, hub: &Hub, root: &Path) -> Act {
        match self.tab {
            0 => match self.picked_session(hub, root) {
                Some(s) if s.who.is_empty() => Act::Open(s.name),
                Some(s) => { self.typing = Some((To::Task(s.who, s.name), String::new())); Act::None }
                None => Act::None,
            },
            CHAT => { self.typing = Some((To::Chat(self.channel().id), String::new())); Act::None }
            ISSUES => match gh().issues.get(self.sel[ISSUES]) {
                Some((n, t, _)) => Act::Task(*n, t.clone()),
                None => Act::None,
            },
            PRS => match gh().prs.get(self.sel[PRS]) {
                Some((n, t, who, _, _)) => Act::Work(format!("review PR #{n}"), format!(
                    "Review pull request #{n} ({t}) by {who}. Read it with `gh pr view {n}` and `gh pr diff {n}`, then list real problems \
                     (bugs, missing tests, risky changes) with file and line. Don't post anything to GitHub; tell me first.")),
                None => Act::None,
            },
            _ => Act::None,
        }
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, hub: &Hub, cwd: &Path) {
        let root = Self::root(cwd);
        let repo = crate::team::repo();
        if let Some(r) = &repo { gh_refresh(r, false) }
        let ppl = people(hub, &root);
        let online = ppl.iter().skip(1).filter(|p| p.online).count();
        let accent = Color::Cyan;
        let outer = Block::bordered().title(Line::styled(
            format!(" {} · team ", repo.clone().unwrap_or_else(|| "not in a team".into())), Style::new().fg(accent).add_modifier(Modifier::BOLD)))
            .border_style(Style::new().fg(accent));
        let inner = outer.inner(area);
        f.render_widget(outer, area);

        if repo.is_none() {
            f.render_widget(Paragraph::new(vec![
                Line::raw(""),
                Line::styled("  You're not in a team in this folder.", Style::new().add_modifier(Modifier::BOLD)),
                Line::raw(""),
                Line::raw("  /team          start a team here or in another folder, or join a friend's"),
                Line::raw("  Esc            back to your sessions"),
            ]), inner);
            return;
        }

        let pending = crate::team::pending();
        let joins = crate::team::join_requests();
        let wait_h = if joins.is_empty() && (pending.is_empty() || self.tab == REQUESTS) { 0 } else { 3 };
        let input_h = if self.typing.is_some() { 1 } else { 0 };
        let [tabs, body, wait, input, hint] = Layout::vertical([
            Constraint::Length(2), Constraint::Min(3), Constraint::Length(wait_h), Constraint::Length(input_h), Constraint::Length(1),
        ]).areas(inner);

        // tab strip
        let gh = gh();
        let counts = [ppl.iter().map(|p| p.sessions.len()).sum::<usize>(), crate::team::chat_unread(), pending.len() + open_holds().len(), gh.issues.len(), gh.prs.len(), 0];
        let mut spans: Vec<Span> = vec![Span::raw(" ")];
        for (i, t) in TABS.iter().enumerate() {
            let label = if i > 0 && i <= PRS && counts[i] > 0 { format!(" {} {t} ({}) ", i + 1, counts[i]) } else { format!(" {} {t} ", i + 1) };
            spans.push(if i == self.tab {
                Span::styled(label, Style::new().fg(Color::Black).bg(accent).add_modifier(Modifier::BOLD))
            } else if i == REQUESTS && counts[i] > 0 {
                Span::styled(label, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
            } else {
                Span::styled(label, Style::new().fg(Color::Gray))
            });
            spans.push(Span::raw(" "));
        }
        let right = format!("● {online} online ");
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        spans.push(Span::raw(" ".repeat((tabs.width as usize).saturating_sub(used + right.chars().count()))));
        spans.push(Span::styled(right, Style::new().fg(if online > 0 { Color::Green } else { Color::DarkGray })));
        f.render_widget(Paragraph::new(Line::from(spans)).block(Block::new().borders(Borders::BOTTOM).border_style(Style::new().fg(Color::DarkGray))), tabs);

        match self.tab {
            0 if self.detail => self.draw_people(f, body, hub, &ppl, &gh),
            0 => self.draw_map(f, body, &ppl, &root),
            CHAT => self.draw_chat(f, body),
            REQUESTS => self.draw_requests(f, body, &open_holds(), &pending),
            ISSUES => self.draw_list(f, body, gh.issues.iter().map(|(n, t, a)| (format!("#{n}"), t.clone(), if a.is_empty() { "open".into() } else { format!("→ {a}") })).collect(),
                                gh.loading, &gh.error, "no open issues"),
            PRS => self.draw_list(f, body, gh.prs.iter().map(|(n, t, who, br, ci)| (format!("#{n}"), t.clone(), format!("{who} · {br} {ci}"))).collect(),
                                gh.loading, &gh.error, "no open pull requests"),
            _ => self.draw_list(f, body, crate::team::activity().into_iter().map(|(t, x)| (t, x, String::new())).collect(), false, "", "nothing yet this session"),
        }

        if let Some(who) = joins.first() {
            let more = if joins.len() > 1 { format!("  (+{} more)", joins.len() - 1) } else { String::new() };
            f.render_widget(Paragraph::new(vec![
                Line::styled(format!(" {who} wants to join the team{more}"), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Line::styled("   l let them in · n not now", Style::new().fg(Color::Yellow)),
            ]).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray))), wait);
        } else if let Some(i) = pending.first().filter(|_| self.tab != REQUESTS) {
            let n = pending.len();
            f.render_widget(Paragraph::new(vec![
                Line::from(vec![Span::styled(format!(" {n} request{} waiting: ", if n == 1 { "" } else { "s" }), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                                Span::raw(format!("{} → your {}: \"{}\"", i.from, i.session, clip(&i.text, (wait.width as usize).saturating_sub(40))))]),
                Line::styled(format!("   {} Requests to accept, reply or decline: nothing runs until you accept", REQUESTS + 1), Style::new().fg(Color::Yellow)),
            ]).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(Color::DarkGray))), wait);
        }
        if let Some((to, text)) = &self.typing {
            let label = match to {
                To::Chat(c) if c == "#team" => " # team › ".to_string(),
                To::Chat(c) if c.starts_with('@') => format!(" {c} (private) › "),
                To::Chat(_) => format!(" {} (group) › ", self.channel().label),
                To::Question(who, s) => format!(" question for {who}'s {s} (answered read-only) › "),
                To::Task(who, s) => format!(" task for {who}'s {s} (runs once they accept it) › "),
                To::Reply(who, _) => format!(" reply to {who} (no work starts) › "),
                To::SendBack(who, _) => format!(" send {who}'s push back: what must change › "),
            };
            f.render_widget(Paragraph::new(Line::from(vec![
                Span::styled(label, Style::new().fg(accent).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{text}▏")),
            ])), input);
        }
        let keys = match (self.typing.is_some(), self.tab) {
            (true, _) => "Enter send · Esc cancel · @login pings someone, @team everyone",
            (_, 0) if !self.detail => "↑↓ pick · Enter open · a ask their agent · p private message · m message the team · ←→ or 1-6 tabs · Esc back",
            (_, 0) => "↑↓ sessions · Enter open yours / give their agent a task · m message · Esc back to the map",
            (_, CHAT) => "↑↓ channel · Enter or m write · x clear a private thread · /dm <login> and /team group <name> <logins> from the prompt · ←→ tabs · Esc back",
            (_, REQUESTS) if self.sel[REQUESTS] < open_holds().len() => "↑↓ pick · Enter or a approve the push · s send back with a note · r talk it through · ←→ tabs · Esc back",
            (_, REQUESTS) => "↑↓ pick · Enter or a accept · r reply · d decline · A always answer their questions · ←→ tabs · Esc back",
            (_, ISSUES) => "↑↓ pick · Enter start it in its own branch and session · r refresh · ←→ tabs · Esc back",
            (_, PRS) => "↑↓ pick · Enter have your current session review it · r refresh · ←→ tabs · Esc back",
            _ => "↑↓ scroll · ←→ tabs · Esc back",
        };
        f.render_widget(Paragraph::new(Line::styled(format!(" {keys}"), Style::new().fg(Color::DarkGray))), hint);
    }

    /// Requests: work teammates (or their agents) asked of your sessions, and questions when
    /// /answers is "ask". Nothing in here has reached any session.
    fn draw_requests(&mut self, f: &mut Frame, area: Rect, holds: &[crate::team::Hold], pending: &[crate::team::Incoming]) {
        let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        if pending.is_empty() && holds.is_empty() {
            f.render_widget(Paragraph::new("\n  No requests. When a teammate or their agent asks one of your sessions to do something, it waits here:\n  nothing runs until you accept it.")
                .style(Style::new().fg(Color::DarkGray)).wrap(Wrap { trim: false }), area);
            return;
        }
        self.sel[REQUESTS] = self.sel[REQUESTS].min(holds.len() + pending.len() - 1);
        let w = area.width as usize;
        let mut lines: Vec<Line> = vec![Line::raw("")];
        let mut sel_line = 0;
        // held pushes first: the reviewer's reason and the diff, coloured, whole on the picked one
        for (n, h) in holds.iter().enumerate() {
            let on = n == self.sel[REQUESTS];
            if on { sel_line = lines.len() }
            let head = format!("{} {} · push held for you to decide · {}", if on { "▸" } else { " " }, h.from, h.ago);
            lines.push(Line::styled(clip(&head, w), if on { Style::new().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD) }
                                                   else { Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD) }));
            let body: Vec<String> = h.text.lines().flat_map(|l| if l.starts_with(['+', '-', '@']) { vec![clip(l, w.saturating_sub(4))] }
                                                             else { crate::ui::wrap(l, w.saturating_sub(4).max(10)) }).collect();
            let shown = if on { body.len() } else { body.len().min(2) };
            for l in &body[..shown] {
                let st = if l.starts_with("@@") { Style::new().fg(Color::Cyan) } else if l.starts_with('+') { Style::new().fg(Color::Green) }
                         else if l.starts_with('-') { Style::new().fg(Color::Red) } else { Style::new() };
                lines.push(Line::styled(format!("    {l}"), st));
            }
            if body.len() > shown { lines.push(Line::styled(format!("    … {} more lines (↑↓ to pick it)", body.len() - shown), Style::new().fg(Color::DarkGray))) }
            lines.push(Line::raw(""));
        }
        for (n, i) in pending.iter().enumerate() {
            let on = n + holds.len() == self.sel[REQUESTS];
            if on { sel_line = lines.len() }
            let from = if i.from_session.is_empty() { i.from.clone() } else { format!("{}'s {}", i.from, i.from_session) };
            let kind = if i.question.is_some() { "question for" } else { "work for" };
            let head = format!("{} {from} · {kind} your {} · {}{}", if on { "▸" } else { " " }, i.session, crate::team::ago(i.ts),
                               if i.replied { " · you replied" } else { "" });
            lines.push(Line::styled(clip(&head, w), if on { Style::new().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD) }
                                                   else { Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD) }));
            // the whole request, wrapped, on the picked one; two lines on the others
            let text: String = i.text.chars().filter(|c| !c.is_control() || *c == '\n').collect();
            let body: Vec<String> = if on { crate::ui::wrap(&text, w.saturating_sub(4).max(10)) }
                                    else { text.lines().map(|l| clip(l, w.saturating_sub(4))).collect() };
            let shown = if on { body.len() } else { body.len().min(2) };
            for l in &body[..shown] { lines.push(Line::raw(format!("    {l}"))) }
            if body.len() > shown { lines.push(Line::styled(format!("    … {} more lines (↑↓ to pick it)", body.len() - shown), Style::new().fg(Color::DarkGray))) }
            lines.push(Line::raw(""));
        }
        // the picked request from its top, so a long one is readable whole
        let top = sel_line.saturating_sub(1) as u16;
        f.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    /// The map: one line per person saying what they're on and where, the one problem worth
    /// knowing about on top, the quiet parts of the code counted, and the latest happenings.
    fn draw_map(&mut self, f: &mut Frame, area: Rect, ppl: &[Person], root: &Path) {
        let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        let w = area.width as usize;
        self.person = self.person.min(ppl.len().saturating_sub(1));
        let mut lines: Vec<Line> = vec![Line::raw("")];
        // problems first: two people changing the same function (red), someone paused (yellow),
        // two people in the same file but different functions (just so you know)
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let who_of = |p: &Person| if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
        let mut funcs: std::collections::BTreeMap<(String, String), Vec<String>> = Default::default();
        let mut owners: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        for p in ppl {
            for s in &p.sessions {
                for (fl, fns, t) in &s.edits {
                    if now.saturating_sub(*t) >= 600 { continue }
                    for f in fns {
                        let e = funcs.entry((fl.clone(), f.clone())).or_default();
                        if !e.contains(&who_of(p)) { e.push(who_of(p)) }
                    }
                }
                for fl in s.files.iter().map(|f| f.as_str()).chain(s.edits.iter().map(|e| e.0.as_str())) {
                    let e = owners.entry(fl).or_default();
                    if !e.contains(&who_of(p)) { e.push(who_of(p)) }
                }
            }
        }
        let mut problems: Vec<Line> = vec![];
        for ((fl, f), who) in funcs.iter().filter(|(_, v)| v.len() > 1) {
            problems.push(Line::styled(clip(&format!("! {} both changed {f} in {fl}", who.join(" and ")), w),
                                       Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)));
        }
        for p in ppl {
            for s in p.sessions.iter().filter_map(|s| s.waiting.as_ref().map(|w| (s, w))) {
                let (s, wt) = s;
                let who = if p.who.is_empty() { format!("your {}", s.name) } else { p.who.clone() };
                let on = if wt.on.is_empty() { "someone".to_string() } else { wt.on.clone() };
                problems.push(Line::styled(clip(&format!("{who} waits for {on} · {} in {}", wt.func, wt.file), w), Style::new().fg(Color::Yellow)));
            }
        }
        if problems.is_empty() {
            if let Some((fl, who)) = owners.iter().find(|(_, v)| v.len() > 1) {
                problems.push(Line::styled(clip(&format!("· {} both have changes in {fl} (different functions)", who.join(" and ")), w),
                                           Style::new().fg(Color::DarkGray)));
            }
        }
        let shown = problems.len().min(3);
        let more = problems.len() - shown;
        lines.extend(problems.into_iter().take(shown));
        if more > 0 { lines.push(Line::styled(format!("  + {more} more"), Style::new().fg(Color::DarkGray))) }
        if shown > 0 { lines.push(Line::raw("")) }
        let name_w = ppl.iter().map(|p| if p.who.is_empty() { 3 } else { p.who.chars().count() }).max().unwrap_or(3).max(5) + 2;
        let grouped = ppl.len() > GROUP_AT;
        let mut last_area = String::new();
        let mut sel_line = 0;
        for (i, p) in ppl.iter().enumerate() {
            let on = i == self.person;
            if grouped && i > 0 {
                let a = code_area(p);
                if a != last_area {
                    let n = ppl.iter().skip(1).filter(|q| code_area(q) == a).count();
                    lines.push(Line::styled(format!("  {a} · {n} {}", if n == 1 { "person" } else { "people" }), Style::new().fg(Color::DarkGray).add_modifier(Modifier::BOLD)));
                    last_area = a;
                }
            }
            if on { sel_line = lines.len() }
            let color = person_color(i);
            let busy: Vec<&Session> = p.sessions.iter().filter(|s| !matches!(s.status.as_str(), "idle" | "")).collect();
            let s = busy.first().copied().or(p.sessions.first());
            let (what, place, when) = match s {
                _ if !p.online => ("away".to_string(), String::new(), p.label.split("  ").nth(1).unwrap_or("").to_string()),
                None => ("no sessions open".to_string(), String::new(), String::new()),
                Some(s) => {
                    let topic = if s.topic.is_empty() { "no task yet".to_string() } else { s.topic.clone() };
                    let what = match &s.waiting {
                        Some(wt) => format!("waiting for {} · {topic}", wt.on),
                        None if matches!(s.status.as_str(), "held" | "waiting") => format!("waiting · {topic}"),
                        None => topic,
                    };
                    // where: the function it's changing, else the file it's on
                    let place = s.edits.iter().find(|e| now.saturating_sub(e.2) < 600)
                        .map(|(fl, fns, _)| match fns.first() { Some(f) => format!("{fl} › {f}"), None => fl.clone() })
                        .or_else(|| s.now.split_once(' ').map(|x| x.1.to_string()).filter(|x| x.contains('/') || x.contains('.')))
                        .or_else(|| s.files.last().cloned()).unwrap_or_default();
                    let when = if !busy.is_empty() { "now".to_string() } else { "idle".to_string() };
                    (what, place, when)
                }
            };
            let extra = if p.sessions.len() > 1 { format!(" +{}", p.sessions.len() - 1) } else { String::new() };
            let name = if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
            let place_w = 30.min(w / 3);
            let when_w = 8;
            let what_w = w.saturating_sub(name_w + place_w + when_w + 6);
            let base = if on { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
            lines.push(Line::from(vec![
                Span::styled(if on { "▸ " } else { "  " }, Style::new().fg(color)),
                Span::styled(format!("{name:<name_w$}"), Style::new().fg(color).add_modifier(Modifier::BOLD) .patch(if on { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() })),
                Span::styled(format!("{:<what_w$}", clip(&format!("{what}{extra}"), what_w)), if p.online { base } else { base.fg(Color::DarkGray) }),
                Span::styled(format!("  {:<place_w$}", clip(&place, place_w)), Style::new().fg(Color::DarkGray)),
                Span::styled(format!("  {when:>when_w$}"), Style::new().fg(if when == "now" { Color::Green } else { Color::DarkGray })),
            ]));
        }
        // who builds on whom: merge the one they build on first
        let c = contracts(ppl);
        if !c.is_empty() {
            lines.push(Line::raw(""));
            for (a, b, n) in c.iter().take(2) {
                let mutual = c.iter().any(|(x, y, _)| x == b && y == a);
                let poss = |x: &str| if x == "you" { "your".to_string() } else { format!("{x}'s") };
                let text = if mutual { format!("  ⇄ {a} and {b} build on each other's changes ({n}…): merge them together") }
                           else { format!("  → merge {} work before {}: it uses {n}, which {a} changed", poss(a), if b == "you" { "yours".to_string() } else { format!("{b}'s") }) };
                lines.push(Line::styled(clip(&text, w), Style::new().fg(Color::Cyan)));
            }
        }
        // questions out to teammates' agents, not answered yet
        let open = crate::team::open_questions();
        if !open.is_empty() {
            if c.is_empty() { lines.push(Line::raw("")) }
            for (sess, to, q, t) in open.iter().rev().take(2) {
                let from = if sess.is_empty() { "you".to_string() } else { format!("your {sess}") };
                lines.push(Line::styled(clip(&format!("  ? {from} asked {to}: {q}  ({t}, waiting)"), w), Style::new().fg(Color::Yellow)));
            }
        }
        // the parts of the code nobody is in
        let touched: Vec<String> = owners.keys().map(|f| f.split('/').next().unwrap_or("").to_string()).collect();
        let mut free: Vec<String> = std::fs::read_dir(root).into_iter().flatten().flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.') && !["target", "node_modules", "dist", "build", "__pycache__", "venv", ".venv", "vendor"].contains(&n.as_str()) && !touched.contains(n))
            .collect();
        free.sort();
        lines.push(Line::raw(""));
        if !free.is_empty() {
            let shown: Vec<String> = free.iter().take(4).map(|n| format!("{n}/")).collect();
            let more = if free.len() > 4 { format!("   + {} more", free.len() - 4) } else { String::new() };
            lines.push(Line::styled(format!("  free:  {}{more}", shown.join("   ")), Style::new().fg(Color::DarkGray)));
        }
        // the latest happenings
        let act = crate::team::activity();
        lines.push(Line::raw(""));
        lines.push(Line::styled("  lately", Style::new().fg(Color::DarkGray)));
        if act.is_empty() { lines.push(Line::styled("    (quiet)", Style::new().fg(Color::DarkGray))) }
        for (t, x) in act.iter().take(5) {
            lines.push(Line::from(vec![
                Span::raw(format!("    {}", clip(x, w.saturating_sub(14)))),
                Span::styled(format!("  {t}"), Style::new().fg(Color::DarkGray)),
            ]));
        }
        // keep the picked person on screen in a long list
        let top = sel_line.saturating_sub((area.height as usize).saturating_sub(4)) as u16;
        f.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    /// Chat: channels on the left (# team, a DM per teammate, your groups), the picked one on the right.
    fn draw_chat(&mut self, f: &mut Frame, area: Rect) {
        let chans = crate::team::channels();
        self.sel[CHAT] = self.sel[CHAT].min(chans.len().saturating_sub(1));
        let [left, right] = Layout::horizontal([Constraint::Length(22.min(area.width / 3)), Constraint::Min(10)]).areas(area);
        let lw = left.width.saturating_sub(2) as usize;
        let list: Vec<Line> = chans.iter().enumerate().map(|(i, c)| {
            let on = i == self.sel[CHAT];
            let badge = match (c.unread, c.pinged) { (0, _) => String::new(), (n, true) => format!(" {n}@"), (n, false) => format!(" {n}") };
            let name = clip(&c.label, lw.saturating_sub(badge.chars().count() + 1));
            let pad = lw.saturating_sub(name.chars().count() + badge.chars().count() + 1);
            let st = if on { Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) }
                     else if c.id.starts_with('@') && !c.online { Style::new().fg(Color::DarkGray) } else { Style::new() };
            Line::from(vec![
                Span::styled(format!(" {name}{}", " ".repeat(pad)), st),
                Span::styled(badge, if on { st } else if c.pinged { Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Green) }),
            ])
        }).collect();
        f.render_widget(Paragraph::new(list).block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(Color::DarkGray))), left);

        let Some(c) = chans.get(self.sel[CHAT]) else { return };
        crate::team::channel_read(&c.id); // on screen: read
        let right = Rect { x: right.x + 2, width: right.width.saturating_sub(3), ..right };
        let members = crate::team::channel_members(&c.id);
        let about = if c.id == "#team" { "everyone in the team · people only, agents never read it".to_string() }
            else if c.id.starts_with('@') {
                format!("private: only {} can read it{}", members.join(" and "),
                        if c.online { String::new() } else { " · they're offline: it reaches them when they're back".into() })
            } else { format!("group: {} · only its members can read it", members.join(", ")) };
        let mut lines = vec![Line::styled(clip(&about, right.width as usize), Style::new().fg(Color::DarkGray)), Line::raw("")];
        let rows = crate::team::channel_msgs(&c.id);
        if rows.is_empty() {
            lines.push(Line::styled("No messages yet. Press Enter to write; @login pings someone, @team everyone.", Style::new().fg(Color::DarkGray)));
        }
        for (t, who, x, ping) in &rows {
            let text = if *ping { Span::styled(x.clone(), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)) } else { Span::raw(x.clone()) };
            lines.push(Line::from(vec![
                Span::styled(format!("{who:>10} "), Style::new().fg(if who == "you" { Color::Cyan } else { Color::Green }).add_modifier(Modifier::BOLD)),
                text,
                Span::styled(format!("  {t}"), Style::new().fg(Color::DarkGray)),
            ]));
        }
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        let h = p.line_count(right.width) as u16;
        let top = h.saturating_sub(right.height).saturating_sub(self.scroll);
        f.render_widget(p.scroll((top, 0)), right);
    }

    fn draw_people(&mut self, f: &mut Frame, area: Rect, hub: &Hub, ppl: &[Person], gh: &Gh) {
        let [left, right] = Layout::horizontal([Constraint::Length(34.min(area.width / 2)), Constraint::Min(10)]).areas(area);
        let total: usize = ppl.iter().map(|p| p.sessions.len()).sum();
        self.sel[0] = self.sel[0].min(total.saturating_sub(1));
        let mut lines: Vec<Line> = vec![];
        let mut idx = 0;
        let mut picked: Option<&Session> = None;
        let w = left.width.saturating_sub(2) as usize;
        for p in ppl {
            let dot = if p.online { "●" } else { "○" };
            let color = if p.who.is_empty() { Color::Cyan } else if p.online { Color::Green } else { Color::DarkGray };
            // how their updates reach us: a direct line (instant) or git (every few seconds)
            let how = match (p.online && !p.who.is_empty()).then(|| crate::team::line_to(&p.who)) {
                Some(Some(l)) if l.starts_with("via ") => format!("  line {l}"),
                Some(Some(l)) => format!("  direct line ({l})"),
                Some(None) => "  by git".to_string(),
                None => String::new(),
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {dot} {}", p.label), Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(how, Style::new().fg(Color::DarkGray)),
            ]));
            if p.sessions.is_empty() {
                lines.push(Line::styled("    no sessions", Style::new().fg(Color::DarkGray)));
            }
            for s in &p.sessions {
                let on = idx == self.sel[0];
                if on { picked = Some(s) }
                let busy = !matches!(s.status.as_str(), "idle" | "");
                let what = if s.topic.is_empty() { String::new() } else { format!("  {}", s.topic) };
                let text = clip(&format!("{} {}{what}", if busy { "◆" } else { " " }, s.name), w.saturating_sub(2));
                let st = if on { Style::new().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD) }
                         else if busy { Style::new().fg(Color::Yellow) } else { Style::new() };
                lines.push(Line::styled(format!("  {text:<width$}", width = w.saturating_sub(2)), st));
                idx += 1;
            }
            lines.push(Line::raw(""));
        }
        f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(Color::DarkGray))), left);

        let Some(s) = picked else { return };
        let right = Rect { x: right.x + 2, width: right.width.saturating_sub(3), ..right };
        let who = if s.who.is_empty() { "you".to_string() } else { s.who.clone() };
        let busy = !matches!(s.status.as_str(), "idle" | "");
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!("{who} · {}", s.name), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}", s.status), Style::new().fg(if busy { Color::Yellow } else { Color::DarkGray })),
            ]),
            Line::styled(if s.topic.is_empty() { "nothing asked yet".to_string() } else { s.topic.clone() },
                         Style::new().fg(if s.topic.is_empty() { Color::DarkGray } else { Color::White })),
        ];
        if !s.now.is_empty() { lines.push(Line::styled(format!("now: {}", s.now), Style::new().fg(Color::Yellow))) }
        if let Some(wt) = &s.waiting {
            lines.push(Line::styled(format!("waiting for {} to finish {} in {}", wt.on, wt.func, wt.file), Style::new().fg(Color::Yellow)));
        }
        // its fact sheet's own words: what it settled on and what's next
        if let Some(sh) = crate::team::sheet_of(&s.who, &s.name) {
            if !sh.decided.is_empty() { lines.push(Line::styled(format!("decided: {}", sh.decided), Style::new().fg(Color::Gray))) }
            if !sh.next.is_empty() { lines.push(Line::styled(format!("next: {}", sh.next), Style::new().fg(Color::Gray))) }
        }
        for (func, text) in crate::team::promises_of(&s.who, &s.name).into_iter().take(3) {
            lines.push(Line::styled(clip(&format!("⇢ counts on {func}: {text}"), 200), Style::new().fg(Color::Cyan)));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for (fl, fns, t) in s.edits.iter().take(4) {
            let hot = now.saturating_sub(*t) < 600;
            let what = if fns.is_empty() { fl.clone() } else { format!("{fl} › {}", fns.join(", ")) };
            lines.push(Line::styled(format!("{} {what}", if hot { "✎ changing" } else { "  changed " }),
                                    Style::new().fg(if hot { Color::LightYellow } else { Color::Gray })));
        }
        // its contracts: what it uses from teammates' changes, and who uses its changes
        let mine_made: Vec<&String> = s.edits.iter().flat_map(|e| e.1.iter()).collect();
        for p in ppl {
            let pw = if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
            if p.who == s.who { continue }
            let theirs: Vec<&String> = p.sessions.iter().flat_map(|x| x.edits.iter().flat_map(|e| e.1.iter())).collect();
            let relies: Vec<&String> = s.uses.iter().filter(|n| theirs.contains(n)).take(4).collect();
            if !relies.is_empty() {
                lines.push(Line::styled(format!("⇢ relies on {pw}'s {}", relies.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")), Style::new().fg(Color::Cyan)));
            }
            let used: Vec<&String> = p.sessions.iter().flat_map(|x| x.uses.iter()).filter(|n| mine_made.contains(n)).take(4).collect();
            if !used.is_empty() {
                lines.push(Line::styled(format!("⇠ {pw} {} on its {}", if pw == "you" { "rely" } else { "relies" }, used.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")), Style::new().fg(Color::Cyan)));
            }
        }
        if s.edits.is_empty() && !s.files.is_empty() { lines.push(Line::styled(format!("files: {}", s.files.join(", ")), Style::new().fg(Color::Gray))) }
        if !s.branch.is_empty() {
            let pr = gh.prs.iter().find(|p| p.3 == s.branch);
            let mut spans = vec![Span::styled(format!("branch {}", s.branch), Style::new().fg(Color::Gray))];
            if let Some((n, _, _, _, ci)) = pr {
                spans.push(Span::styled(format!("  · PR #{n}"), Style::new().fg(Color::Gray)));
                if !ci.is_empty() {
                    spans.push(Span::styled(format!("  CI {ci}"), Style::new().fg(match ci.as_str() { "✓" => Color::Green, "✗" => Color::Red, _ => Color::Yellow })));
                }
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled("─".repeat(right.width as usize), Style::new().fg(Color::DarkGray)));
        let rows: Vec<(String, String)> = if s.who.is_empty() {
            hub.get(&s.name).map(|m| lock(&m.msgs).iter()
                .filter(|x| matches!(x["role"].as_str(), Some("user" | "assistant")))
                .filter_map(|x| x["content"].as_str().filter(|c| !c.trim().is_empty() && !c.starts_with("[harness]"))
                    .map(|c| (x["role"].as_str().unwrap_or("").to_string(), c.to_string())))
                .collect()).unwrap_or_default()
        } else {
            crate::team::transcript_rows(&s.who, &s.name)
        };
        if rows.is_empty() { lines.push(Line::styled("no conversation yet", Style::new().fg(Color::DarkGray))) }
        for (r, t) in rows.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
            let user = r == "user";
            lines.push(Line::from(vec![
                Span::styled(if user { "▸ " } else { "◂ " }, Style::new().fg(if user { Color::Cyan } else { Color::Green })),
                Span::styled(t.trim().to_string(), if user { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() }),
            ]));
        }
        // stick to the bottom, like a chat; PgUp scrolls back
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        let h = p.line_count(right.width) as u16;
        let top = h.saturating_sub(right.height).saturating_sub(self.scroll);
        f.render_widget(p.scroll((top, 0)), right);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect, rows: Vec<(String, String, String)>, loading: bool, err: &str, empty: &str) {
        let n = rows.len();
        self.sel[self.tab] = self.sel[self.tab].min(n.saturating_sub(1));
        let sel = self.sel[self.tab];
        if n == 0 {
            let msg = if loading { "loading from GitHub…" } else if !err.is_empty() { err } else { empty };
            f.render_widget(Paragraph::new(format!("\n  {msg}")).style(Style::new().fg(Color::DarkGray)), area);
            return;
        }
        let h = area.height as usize;
        let top = sel.saturating_sub(h.saturating_sub(1));
        let kw = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(4).min(14);
        let lines: Vec<Line> = rows.iter().enumerate().skip(top).take(h).map(|(i, (k, t, side))| {
            let on = self.tab != TABS.len() - 1 && i == sel;
            let base = if on { Style::new().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::new() };
            let room = (area.width as usize).saturating_sub(kw + side.chars().count() + 6);
            let t = clip(t, room);
            Line::from(vec![
                Span::styled(format!(" {k:<kw$}  "), if on { base } else { Style::new().fg(Color::DarkGray) }),
                Span::styled(format!("{t:<room$}"), base),
                Span::styled(format!("  {side} "), if on { base } else if side.ends_with('✗') { Style::new().fg(Color::Red) }
                             else if side.ends_with('✓') { Style::new().fg(Color::Green) } else { Style::new().fg(Color::Gray) }),
            ])
        }).collect();
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// Contracts between people: (producer, consumer, name) where the consumer's fresh code uses
/// something the producer changed lately. The producer's work should be merged first.
fn contracts(ppl: &[Person]) -> Vec<(String, String, String)> {
    let who = |p: &Person| if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
    let mut out = vec![];
    for a in ppl {
        let made: Vec<&String> = a.sessions.iter().flat_map(|s| s.edits.iter().flat_map(|e| e.1.iter())).collect();
        for b in ppl.iter().filter(|b| b.who != a.who) {
            if let Some(n) = b.sessions.iter().flat_map(|s| s.uses.iter()).find(|n| made.contains(n)) {
                out.push((who(a), who(b), n.clone()));
            }
        }
    }
    out
}

/// Each person's colour, the same everywhere on the map.
fn person_color(i: usize) -> Color {
    [Color::Cyan, Color::Magenta, Color::Yellow, Color::Green, Color::LightBlue, Color::LightRed, Color::LightGreen][i % 7]
}

fn clip(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn person(who: &str, online: bool, file: &str) -> Person {
        let s = Session { who: who.into(), name: "main".into(), status: "working".into(), topic: String::new(), now: String::new(),
                          files: vec![], branch: String::new(), edits: if file.is_empty() { vec![] } else { vec![(file.into(), vec![], 0)] },
                          waiting: None, uses: vec![] };
        Person { who: who.into(), label: who.into(), online, sessions: vec![s] }
    }

    #[test]
    fn big_teams_read_by_area() {
        let mut v = vec![person("", true, "ui/x.ts"), person("zed", true, "api/a.rs"), person("amy", false, "api/b.rs"),
                         person("bea", true, "ui/y.ts"), person("cal", true, "api/c.rs"), person("dee", true, "")];
        group(&mut v);
        let order: Vec<(&str, String)> = v.iter().map(|p| (p.who.as_str(), code_area(p))).collect();
        assert_eq!(order, [("", "ui/".into()), ("cal", "api/".into()), ("zed", "api/".into()), ("dee", "no code yet".into()),
                           ("bea", "ui/".into()), ("amy", "away".into())]);
    }
}

#[cfg(all(test, unix))]
mod path_tests {
    use super::*;

    #[test]
    fn a_session_opened_by_another_spelling_of_the_folder_is_in_the_project() {
        let d = std::env::temp_dir().join(format!("qh-tv-{}", std::process::id()));
        let root = d.join("Downloads/team");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::os::unix::fs::symlink(d.join("Downloads"), d.join("downloads-link")).unwrap();
        let typed = d.join("downloads-link/team/src");
        assert!(in_project(&typed, &root));
        assert_eq!(rel(&root, &typed.join("a.rs").to_string_lossy()), "src/a.rs");
        assert!(!in_project(&d.join("Downloads"), &root));
        if cfg!(target_os = "macos") {
            assert!(in_project(&d.join("downloads/team"), &root), "the Mac disk ignores case");
        }
        std::fs::remove_dir_all(&d).ok();
    }
}
