//! The team room (Ctrl+T): everything about the team on one screen. Tabs for the Map / People
//! (with the selected session shown live), Chat (team, private and group channels), Requests
//! (work teammates asked of your sessions: nothing runs until you accept), Tasks (open issues),
//! Pull requests (with CI) and Activity; a bar at the bottom for anything waiting on you.
use crate::hub::{lock, Hub};
use crate::team::Status;
use crate::theme;
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
    unmerged: Vec<(String, Vec<String>)>, // not on main yet (from git), shown even while they're away
}

fn people(hub: &Hub, root: &Path) -> Vec<Person> {
    let members: Vec<_> = lock(&hub.members).iter().filter(|m| in_project(&m.cwd(), root)).cloned().collect();
    let mine = members.iter().map(|m| {
        let (topic, now, files) = session_info(hub, m, root);
        let (edits, waiting, uses) = crate::team::mine(&m.name);
        Session { who: String::new(), name: m.name.clone(), status: m.status().to_string(), topic, now, files, branch: branch(&m.cwd()), edits, waiting, uses }
    }).collect::<Vec<_>>();
    let mut out = vec![Person { who: String::new(), label: "you".into(), online: true, sessions: mine, unmerged: crate::team::my_unmerged() }];
    for p in crate::team::overview() {
        out.push(Person {
            label: format!("{}{}{}", p.login, if p.online { String::new() } else { format!("  {}", p.seen) }, if p.view_only { " · view only" } else { "" }),
            online: p.online,
            sessions: p.sessions.into_iter().map(|s| Session { who: p.login.clone(), name: s.name, status: s.status, topic: s.topic, now: s.now,
                files: s.files, branch: s.branch, edits: s.edits, waiting: s.waiting, uses: s.uses }).collect(),
            who: p.login,
            unmerged: p.unmerged,
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

/// "shop.py › discount, total · web.py", for the line under a person on the map.
fn unmerged_text(u: &[(String, Vec<String>)]) -> String {
    u.iter().map(|(f, fns)| if fns.is_empty() { f.clone() } else { format!("{f} › {}", fns.join(", ")) }).collect::<Vec<_>>().join(" · ")
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
    Decline(String, u64),     // why we're saying no to a request (may be left empty)
    Topic(String),            // a topic a trusted teammate's promises may cover
}

/// Pushes handed to you that you haven't decided yet: they head the Requests tab.
fn open_holds() -> Vec<crate::team::Hold> {
    crate::team::holds().into_iter().filter(|h| h.decided.is_none()).collect()
}

const CHAT: usize = 1;
const REQUESTS: usize = 2;
const ISSUES: usize = 3;
const PRS: usize = 4;
const ACTIVITY: usize = 5;
const OUTGOING: usize = 6;
const EDITING: usize = 7;
const AGREE: usize = 8;
const TAKEN: usize = 9;
const ANSWERED: usize = 10;

/// The menu down the left, in order: (page, group heading it starts, name). Number keys pick by
/// place (0 is the tenth); the last is reached with ← then ↑↓.
const MENU: [(usize, Option<&str>, &str); 11] = [
    (0, None, "Home"),
    (REQUESTS, Some("NEEDS YOU"), "Asked you"),
    (OUTGOING, None, "You asked"),
    (TAKEN, Some("YOUR WORK"), "Taken over"),
    (EDITING, None, "Who's editing"),
    (AGREE, None, "Agreements"),
    (ISSUES, None, "Tasks"),
    (PRS, None, "Pull requests"),
    (CHAT, Some("TEAM"), "Chat"),
    (ANSWERED, None, "Agent answered"),
    (ACTIVITY, None, "Activity"),
];
const MENU_W: u16 = 34;

fn menu_pos(tab: usize) -> usize {
    MENU.iter().position(|m| m.0 == tab).unwrap_or(0)
}

#[derive(Default)]
pub struct TeamView {
    tab: usize,
    sel: [usize; 11],
    later: Option<(String, u64, usize)>, // picking when a put-off request comes back: (from, id, choice)
    typing: Option<(To, String)>,
    scroll: u16,
    person: usize, // the map's selected person
    detail: bool,  // the map opened on one person
    in_menu: bool, // ← moved focus to the menu: ↑/↓ change page, → or Enter go back in
    menu_at: Rect, // where the menu was drawn, for clicks
    away_shown: bool, // "while you were away" was on screen on Home
    away_done: bool,  // ...and you left Home since: it's not shown again this time
}

impl TeamView {
    pub fn open(cwd: &Path) -> Self {
        if let Some((_, r)) = crate::team::github_repo(cwd) { gh_refresh(&r, false) }
        // a request waiting: open on it
        let quiet = crate::team::pending().iter().all(|i| i.snoozed) && open_holds().is_empty();
        let tab = if quiet || crate::team::focusing().is_some() { 0 } else { REQUESTS };
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

    /// A click on a menu entry opens that page. True when the click landed on the menu.
    pub fn click(&mut self, col: u16, row: u16) -> bool {
        let m = self.menu_at;
        if col < m.x || col >= m.x + m.width || row < m.y || row >= m.y + m.height { return false }
        let mut y = m.y;
        for (tab, group, _) in MENU.iter() {
            if group.is_some() { y += 2 }
            if row == y || row == y + 1 {
                self.tab = *tab;
                self.scroll = 0;
                self.detail = false;
                self.in_menu = false;
                return true;
            }
            y += 2;
        }
        true
    }

    pub fn key(&mut self, k: KeyEvent, hub: &std::sync::Arc<Hub>, cwd: &Path) -> Act {
        // writing a message: the keys are the text
        if let Some((to, text)) = self.typing.as_mut() {
            match k.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Enter => {
                    let (to, text) = (to.clone(), text.trim().to_string());
                    self.typing = None;
                    if let To::Decline(from, id) = &to {
                        let why = Some(text.clone()).filter(|t| !t.is_empty());
                        return match crate::team::act(from, *id, crate::team::Action::Decline(why)) {
                            Ok(_) => Act::Note(format!("declined {from}'s request; they're told privately{}", if text.is_empty() { "" } else { ", with your reason" })),
                            Err(e) => Act::Err(e),
                        };
                    }
                    if text.is_empty() { return Act::None }
                    let r = match to {
                        To::Topic(who) => crate::team::set_topic(&who, &text, false),
                        To::Chat(c) => crate::team::channel_send(&c, &text),
                        To::Question(who, s) => crate::team::ask_person_agent(&who, &s, &text),
                        To::Task(who, s) => crate::team::send(&who, &s, &text),
                        To::Reply(from, id) => crate::team::reply_request(&from, id, &text),
                        To::SendBack(from, head) => crate::team::decide_hold(&from, &head, false, &text),
                        To::Decline(..) => unreachable!(),
                    };
                    return match r { Ok(m) if m.is_empty() => Act::None, Ok(m) => Act::Note(m), Err(e) => Act::Err(e) };
                }
                KeyCode::Backspace => { text.pop(); }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                _ => {}
            }
            return Act::None;
        }
        // later: until when?
        if let Some((from, id, pick)) = self.later.as_mut() {
            let choices = crate::team::later_choices();
            match k.code {
                KeyCode::Esc => self.later = None,
                KeyCode::Up | KeyCode::Char('k') => *pick = pick.checked_sub(1).unwrap_or(choices.len() - 1),
                KeyCode::Down | KeyCode::Char('j') => *pick = (*pick + 1) % choices.len(),
                KeyCode::Enter => {
                    let (from, id, (words, at)) = (from.clone(), *id, choices[(*pick).min(choices.len() - 1)].clone());
                    self.later = None;
                    return match crate::team::act(&from, id, crate::team::Action::Later(at)) {
                        Ok(_) if at == 0 => Act::Note(format!("later: {from}'s request is off the card, still in Asked you")),
                        Ok(_) => Act::Note(format!("later: {from}'s card comes back {words}; they see when you'll get to it")),
                        Err(e) => Act::Err(e),
                    };
                }
                _ => {}
            }
            return Act::None;
        }
        // f: focus till the next half hour after an hour, f again stops
        if k.code == KeyCode::Char('f') && !(self.tab == 0 && self.detail) {
            let until = if crate::team::focusing().is_some() { 0 } else { crate::team::focus_default() };
            return match crate::team::set_focus(until) { Ok(m) => Act::Note(m), Err(e) => Act::Err(e) };
        }
        // g on any page: go to the request on the card, picked, to answer it there
        if k.code == KeyCode::Char('g') && self.tab != REQUESTS && crate::team::focusing().is_none() {
            let pending = crate::team::pending();
            if let Some(n) = pending.iter().position(|i| !i.snoozed) {
                self.tab = REQUESTS;
                self.sel[REQUESTS] = open_holds().len() + n;
                self.scroll = 0;
                return Act::None;
            }
        }
        // someone asking to join: l lets them in, n doesn't
        if let Some(who) = crate::team::join_requests().first().cloned() {
            match k.code {
                KeyCode::Char('l') => { crate::team::let_in(&who); return Act::Note(format!("letting {who} in")) }
                KeyCode::Char('n') => { crate::team::not_now(&who); return Act::None }
                _ => {}
            }
        }
        // the menu has the focus: arrows move through pages, nothing on the page reacts
        if self.in_menu {
            let step = |t: usize, d: usize| MENU[(menu_pos(t) + d) % MENU.len()].0;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => { self.tab = step(self.tab, 1); self.scroll = 0; self.detail = false; return Act::None }
                KeyCode::Up | KeyCode::Char('k') => { self.tab = step(self.tab, MENU.len() - 1); self.scroll = 0; self.detail = false; return Act::None }
                KeyCode::Right | KeyCode::Enter => { self.in_menu = false; return Act::None }
                KeyCode::Left => return Act::None,
                _ => self.in_menu = false,
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
                    KeyCode::Char('d') if i.question.is_some() => {
                        crate::team::decide(&i.from, i.id, "deny");
                        return Act::Note(format!("declined {}'s question", i.from));
                    }
                    // work: say why (or just Enter); the sender reads it with the no
                    KeyCode::Char('d') => { self.typing = Some((To::Decline(i.from.clone(), i.id), String::new())); return Act::None }
                    KeyCode::Char('l') => { self.later = Some((i.from.clone(), i.id, 0)); return Act::None }
                    KeyCode::Char('r') => { self.typing = Some((To::Reply(i.from.clone(), i.id), String::new())); return Act::None }
                    _ => {}
                }
            }
        }
        if self.tab == OUTGOING {
            let sent = crate::team::outgoing();
            if let Some(o) = sent.get(self.sel[OUTGOING].min(sent.len().saturating_sub(1))) {
                match k.code {
                    KeyCode::Char('s') | KeyCode::Char('r') if o.status() == Status::Expired => return match crate::team::send_again(o.id) {
                        Ok(_) => Act::Note(format!("sent to {} again, as a new request", o.to)), Err(e) => Act::Err(e) },
                    KeyCode::Char('x') if o.status().done() => { crate::team::let_go(o.id); return Act::Note("let it go".into()) }
                    KeyCode::Char('s') => return match crate::team::resend(o.id) { Ok(()) => Act::Note(format!("sent to {} again", o.to)), Err(e) => Act::Err(e) },
                    KeyCode::Char('w') => return match crate::team::withdraw(o.id) { Ok(()) => Act::Note(format!("took back your request to {}; they're told", o.to)), Err(e) => Act::Err(e) },
                    _ => {}
                }
            }
        }
        if self.tab == TAKEN {
            let t = crate::team::taken();
            if let Some(i) = t.get(self.sel[TAKEN].min(t.len().saturating_sub(1))) {
                match k.code {
                    KeyCode::Enter => return Act::Work(format!("{}'s handover", i.from), handover_prompt(i)),
                    KeyCode::Char('m') => {
                        let at = format!("@{}", i.from);
                        if let Some(n) = crate::team::channels().iter().position(|c| c.id == at) { self.sel[CHAT] = n }
                        self.tab = CHAT;
                        self.typing = Some((To::Chat(at), String::new()));
                        return Act::None;
                    }
                    KeyCode::Char('d') => { crate::team::taken_done(&i.from, i.id); return Act::Note(format!("{}'s handover is done: off the list", i.from)) }
                    _ => {}
                }
            }
        }
        if self.tab == ANSWERED && k.code == KeyCode::Char('u') {
            return match crate::team::take_back(self.sel[ANSWERED]) { Ok(m) => Act::Note(m), Err(e) => Act::Err(e) };
        }
        if self.tab == 0 && self.detail {
            if let Some(s) = self.picked_session(hub, &Self::root(cwd)).filter(|s| !s.who.is_empty()) {
                match k.code {
                    KeyCode::Char('t') => {
                        let to = if crate::team::role_of(&s.who) == "trusted" { "member" } else { "trusted" };
                        let r = crate::team::repo().ok_or("team mode is off".to_string()).and_then(|r| crate::team::set_role(&r, &s.who, to));
                        return match r { Ok(_) if to == "trusted" => Act::Note(format!("{} is trusted: your agent answers them alone, promises too (+ to limit it to topics)", s.who)),
                                         Ok(_) => Act::Note(format!("stopped trusting {}: answers that promise something wait for you", s.who)),
                                         Err(e) => Act::Err(e) };
                    }
                    KeyCode::Char('+') => { self.typing = Some((To::Topic(s.who.clone()), String::new())); return Act::None }
                    _ => {}
                }
            }
        }
        let root = Self::root(cwd);
        if self.tab != 0 && self.away_shown { self.away_done = true }
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
            KeyCode::Left => self.in_menu = true,
            KeyCode::Tab => { self.tab = MENU[(menu_pos(self.tab) + 1) % MENU.len()].0; self.scroll = 0 }
            KeyCode::BackTab => { self.tab = MENU[(menu_pos(self.tab) + MENU.len() - 1) % MENU.len()].0; self.scroll = 0 }
            KeyCode::Char(c @ '1'..='9') => { self.tab = MENU[c as usize - '1' as usize].0; self.scroll = 0 }
            KeyCode::Char('0') => { self.tab = MENU[9].0; self.scroll = 0 }
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
            OUTGOING => crate::team::outgoing().len(),
            EDITING => editing_rows(&people(hub, root)).len(),
            AGREE => contracts(&people(hub, root)).len(),
            TAKEN => crate::team::taken().len(),
            ANSWERED => crate::team::answer_log().len(),
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
        let accent = theme::orange();
        // a header line, a hairline, then the menu on the left and the page on the right
        let [top, inner] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
        let head = format!(" {} ", repo.clone().unwrap_or_else(|| "not in a team".into()));
        let right = format!("● {online} online · {} teammates ", ppl.len().saturating_sub(1));
        let need = crate::team::pending().iter().filter(|i| !i.snoozed).count() + open_holds().len();
        let focus = crate::team::focusing();
        let need_s = match focus {
            Some(t) => format!("   focusing till {} · {need} waiting", crate::team::clock(t)),
            None if need > 0 => format!("   {need} need{} you", if need == 1 { "s" } else { "" }),
            None => String::new(),
        };
        let gap = (top.width as usize).saturating_sub(head.chars().count() + 5 + need_s.chars().count() + right.chars().count());
        f.render_widget(Paragraph::new(Line::from(vec![
            Span::styled(head, theme::bold(theme::ink())), Span::styled("team", theme::fg(theme::dim())),
            Span::styled(need_s, if focus.is_some() { theme::fg(theme::dim()) } else { theme::bold(theme::orange()) }),
            Span::raw(" ".repeat(gap)), Span::styled(right, theme::fg(theme::ink2())),
        ])).block(Block::new().borders(Borders::BOTTOM).border_style(theme::fg(theme::line()))), top);

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
        // the card: the oldest request not put off till later, docked under every page but Asked you
        let card = pending.iter().find(|i| !i.snoozed).filter(|_| self.tab != REQUESTS && crate::team::focusing().is_none()).cloned();
        let clash = card.as_ref().map(crate::team::clashes).unwrap_or_default();
        let later_n = crate::team::later_choices().len() as u16;
        let wait_h = if self.later.is_some() { later_n + 2 } else if !joins.is_empty() { 3 } else if card.is_some() { 4 + u16::from(!clash.is_empty()) } else { 0 };
        let input_h = if self.typing.is_some() { 1 } else { 0 };
        let [menu, inner] = Layout::horizontal([Constraint::Length(MENU_W.min(inner.width / 2)), Constraint::Min(10)]).areas(inner);
        self.menu_at = menu;
        let [body, wait, input, hint] = Layout::vertical([
            Constraint::Min(3), Constraint::Length(wait_h), Constraint::Length(input_h), Constraint::Length(1),
        ]).areas(inner);

        let gh = gh();
        self.draw_menu(f, menu, &ppl, &pending, &gh);

        match self.tab {
            0 if self.detail => self.draw_people(f, body, hub, &ppl, &gh),
            // read live: what was sent while you were offline arrives a few seconds after the screen opens
            0 if !self.away_done && { let a = crate::team::away(); !a.needs_you.is_empty() || !a.fyi.is_empty() } => {
                let a = crate::team::away();
                self.away_shown = true;
                let mut lines = vec![Line::raw(""), Line::styled(format!("  While you were away · since {}", crate::team::ago(a.since_ts)), theme::bold(theme::ink()))];
                for i in &a.needs_you {
                    lines.push(Line::from(vec![Span::styled("  ● ", theme::bold(theme::orange())), Span::styled(i.from.clone(), theme::bold(theme::ink())),
                        Span::styled(format!("  {}", clip(&i.text.replace('\n', " "), (body.width as usize).saturating_sub(20))), theme::fg(theme::ink2()))]));
                }
                for x in &a.fyi {
                    lines.push(Line::styled(format!("    {} · {}", x.kind, clip(&x.text.replace('\n', " "), (body.width as usize).saturating_sub(16))), theme::fg(theme::dim())));
                }
                if !a.needs_you.is_empty() { lines.push(Line::styled(format!("  {} to answer what needs you", menu_pos(REQUESTS) + 1), theme::fg(theme::dim()))) }
                let h = (lines.len() as u16 + 1).min(body.height / 2);
                let [top, rest] = Layout::vertical([Constraint::Length(h), Constraint::Min(1)]).areas(body);
                f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::BOTTOM).border_style(theme::fg(theme::line()))), top);
                self.draw_map(f, rest, &ppl, &root)
            }
            0 => self.draw_map(f, body, &ppl, &root),
            CHAT => self.draw_chat(f, body),
            REQUESTS => self.draw_requests(f, body, &open_holds(), &pending),
            ISSUES => self.draw_list(f, body, gh.issues.iter().map(|(n, t, a)| (format!("#{n}"), t.clone(), if a.is_empty() { "open".into() } else { format!("→ {a}") })).collect(),
                                gh.loading, &gh.error, "no open issues"),
            OUTGOING => self.draw_outgoing(f, body),
            TAKEN => self.draw_taken(f, body),
            ANSWERED => self.draw_list(f, body, crate::team::answer_log().into_iter().map(|a| (crate::team::clock(a.ts), format!("{}: {}", a.to, a.question),
                                format!("\"{}\" · {}", clip(&a.answer.replace('\n', " "), 40), if a.taken_back { "taken back" } else if a.promise { "promise" } else { "fact" }))).collect(),
                                false, "", "your agent hasn't answered anyone without you yet"),
            EDITING => { let rows = editing_rows(&ppl); self.draw_list(f, body, rows, false, "", "nobody is changing code that isn't on main") }
            AGREE => self.draw_list(f, body, contracts(&ppl).into_iter().map(|(a, b, n)| (n, format!("{b} counts on it"), format!("made by {a}"))).collect(),
                                false, "", "no agreements yet: when a teammate's code uses a name yours made, it shows here"),
            PRS => self.draw_list(f, body, gh.prs.iter().map(|(n, t, who, br, ci)| (format!("#{n}"), t.clone(), format!("{who} · {br} {ci}"))).collect(),
                                gh.loading, &gh.error, "no open pull requests"),
            _ => self.draw_list(f, body, crate::team::activity().into_iter().map(|(t, x)| (t, x, String::new())).collect(), false, "", "nothing yet this session"),
        }

        if let Some((from, _, pick)) = &self.later {
            let mut lines = vec![Line::styled(format!(" LATER, UNTIL WHEN? · {from}'s request"), theme::bold(theme::ink()))];
            for (n, (words, _)) in crate::team::later_choices().into_iter().enumerate() {
                lines.push(if n == *pick { Line::from(vec![Span::styled("  › ", theme::bold(theme::orange())), Span::styled(words, theme::bold(theme::ink()))]) }
                           else { Line::styled(format!("    {words}"), theme::fg(theme::ink2())) });
            }
            f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::TOP).border_style(theme::fg(theme::line()))), wait);
        } else if let Some(who) = joins.first() {
            let more = if joins.len() > 1 { format!("  (+{} more)", joins.len() - 1) } else { String::new() };
            f.render_widget(Paragraph::new(vec![
                Line::styled(format!(" {who} wants to join the team{more}"), Style::new().fg(theme::orange()).add_modifier(Modifier::BOLD)),
                Line::styled("   l let them in · n not now", Style::new().fg(theme::orange())),
            ]).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(theme::dim()))), wait);
        } else if let Some(i) = card {
            let more = pending.iter().filter(|x| !x.snoozed).count() - 1;
            let what = if i.question.is_some() { "asks your" } else { "hands you work for your" };
            let ww = wait.width as usize;
            let mut lines = vec![
                Line::from(vec![Span::styled(" ● ", theme::bold(theme::orange())), Span::styled(i.from.clone(), theme::bold(theme::ink())),
                                Span::styled(format!(" {what} {} · {}", i.session, crate::team::ago(i.ts)), theme::fg(theme::dim())),
                                Span::styled(if more > 0 { format!("   + {more} more") } else { String::new() }, theme::fg(theme::ink2()))]),
                Line::styled(format!("   {}", clip(&i.text.replace('\n', " "), ww.saturating_sub(4))), theme::fg(theme::ink())),
            ];
            if let Some((who, files)) = clash.first() {
                lines.push(Line::styled(format!("   {}", clip(&format!("⚠ {who} also handed you {}", files.join(", ")), ww.saturating_sub(3))), theme::bold(theme::orange())));
            }
            lines.push(Line::styled("   g go to it: accept, reply, decline or later there · nothing runs until you accept", theme::fg(theme::dim())));
            f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::TOP).border_style(theme::fg(theme::line()))), wait);
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
                To::Decline(who, _) => format!(" why not, {who}? (Enter alone just says no) › "),
                To::Topic(who) => format!(" a topic {who}'s questions may get promises on (e.g. refunds) › "),
            };
            f.render_widget(Paragraph::new(Line::from(vec![
                Span::styled(label, Style::new().fg(accent).add_modifier(Modifier::BOLD)),
                Span::raw(format!("{text}▏")),
            ])), input);
        }
        let keys = match (self.typing.is_some(), self.tab) {
            _ if self.later.is_some() => "↑↓ pick · Enter put it off till then · Esc cancel",
            (false, _) if self.in_menu => "↑↓ pages · → or Enter go into the page · Esc back",
            (true, _) => "Enter send · Esc cancel · @login pings someone, @team everyone",
            (_, 0) if !self.detail => "↑↓ pick · Enter open · a ask their agent · p private message · m message the team · 1-9 pages · Esc back",
            (_, 0) => "↑↓ sessions · Enter open yours / give their agent a task · t trust or stop · + topic · m message · Esc back to the map",
            (_, CHAT) => "↑↓ channel · Enter or m write · x clear a private thread · /dm <login> and /team group <name> <logins> from the prompt · ← menu · Esc back",
            (_, REQUESTS) if self.sel[REQUESTS] < open_holds().len() => "↑↓ pick · Enter or a approve the push · s send back with a note · r talk it through · ← menu · Esc back",
            (_, REQUESTS) => "↑↓ pick · Enter or a accept · r reply · d decline (say why) · l later · A always answer their questions · ← menu · Esc back",
            (_, OUTGOING) => "↑↓ pick · s send again · w take it back · x let an expired one go · ← menu · Esc back",
            (_, TAKEN) => "↑↓ pick · Enter start on what's left · m ask them · d done · ← menu · Esc back",
            (_, ANSWERED) => "↑↓ pick · u take back an answer (they get a correction) · ← menu · Esc back",
            (_, ISSUES) => "↑↓ pick · Enter start it in its own branch and session · r refresh · ← menu · Esc back",
            (_, PRS) => "↑↓ pick · Enter have your current session review it · r refresh · ← menu · Esc back",
            _ => "↑↓ scroll · ← menu · Esc back",
        };
        let keys = if self.later.is_some() || self.typing.is_some() { keys.to_string() }
                   else { format!("f {} · {keys}", if crate::team::focusing().is_some() { "stop focusing" } else { "focus" }) };
        f.render_widget(Paragraph::new(Line::styled(format!(" {keys}"), Style::new().fg(theme::dim()))), hint);
    }

    /// The menu: each page with a count at the right and a grey line of what's in it.
    fn draw_menu(&self, f: &mut Frame, area: Rect, ppl: &[Person], pending: &[crate::team::Incoming], gh: &Gh) {
        let holds = open_holds().len();
        let sent = crate::team::outgoing();
        let open_sent = sent.iter().filter(|o| !matches!(o.status(), Status::Accepted | Status::Declined | Status::Withdrawn | Status::Expired)).count();
        let edits = editing_rows(ppl).len();
        let agree = contracts(ppl).len();
        let unread = crate::team::chat_unread();
        let mode = if crate::config::load().claims_lock { "lock" } else { "warn" };
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        let sub = |tab: usize| -> (String, String, bool) {
            match tab {
                0 => (String::new(), "who's here, what's moving".into(), false),
                REQUESTS => {
                    let n = pending.len() + holds;
                    let mut v = vec![];
                    let q = pending.iter().filter(|i| i.question.is_some()).count();
                    if pending.len() > q { v.push(plural(pending.len() - q, "request")) }
                    if q > 0 { v.push(plural(q, "question")) }
                    if holds > 0 { v.push(plural(holds, "held push")) }
                    (n.to_string(), if v.is_empty() { "nothing waiting".into() } else { v.join(" · ") }, n > 0)
                }
                OUTGOING => (open_sent.to_string(), format!("{open_sent} open · {} in all", sent.len()), false),
                EDITING => (edits.to_string(), format!("not on main · {mode}"), false),
                AGREE => (agree.to_string(), "names teammates' code uses".into(), false),
                ISSUES => (gh.issues.len().to_string(), "open issues".into(), false),
                PRS => (gh.prs.len().to_string(), "open pull requests".into(), false),
                CHAT => (unread.to_string(), if unread > 0 { format!("{unread} unread") } else { "team, private, groups".into() }, unread > 0),
                TAKEN => { let n = crate::team::taken().len(); (n.to_string(), "handovers you accepted".into(), false) }
                ANSWERED => {
                    let log = crate::team::answer_log();
                    let p = log.iter().filter(|a| a.promise && !a.taken_back).count();
                    (log.len().to_string(), if p > 0 { format!("without you · {p} promised") } else { "without you".into() }, false)
                }
                _ => (String::new(), "everything that happened".into(), false),
            }
        };
        let w = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = vec![];
        for (i, (tab, group, name)) in MENU.iter().enumerate() {
            if let Some(g) = group {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!(" {g}"), theme::bold(theme::dim())));
            }
            let here = self.tab == *tab;
            let (count, what, hot) = sub(*tab);
            let count = if count == "0" { String::new() } else { count };
            let num = match i { 0..=8 => (i + 1).to_string(), 9 => "0".into(), _ => " ".into() };
            let left = format!(" {num}  {name}");
            let pad = w.saturating_sub(left.chars().count() + count.chars().count());
            lines.push(Line::from(vec![
                Span::styled(format!(" {num}"), if here { theme::bold(theme::orange()) } else { theme::fg(theme::dim()) }),
                Span::styled(format!("{}{name}", if here && self.in_menu { " ▸" } else { "  " }), theme::bold(if here { theme::ink() } else { theme::ink2() })),
                Span::raw(" ".repeat(pad)),
                Span::styled(count, if hot { theme::bold(theme::orange()) } else { theme::fg(theme::ink2()) }),
            ]));
            lines.push(Line::styled(format!("    {}", clip(&what, w.saturating_sub(4))), theme::fg(theme::dim())));
        }
        f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(theme::fg(theme::line()))), area);
    }

    /// Handovers you accepted: the packet the sender's agent wrote, and any clash.
    fn draw_taken(&mut self, f: &mut Frame, area: Rect) {
        let t = crate::team::taken();
        let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        if t.is_empty() {
            f.render_widget(Paragraph::new("\n  Nothing taken over. When you accept a teammate's handover, it stays here with what's done, what's left and the files.")
                .style(theme::fg(theme::dim())).wrap(Wrap { trim: false }), area);
            return;
        }
        self.sel[TAKEN] = self.sel[TAKEN].min(t.len() - 1);
        let w = area.width as usize;
        let mut lines = vec![Line::raw("")];
        let mut sel_line = 0;
        for (n, i) in t.iter().enumerate() {
            let on = n == self.sel[TAKEN];
            if on { sel_line = lines.len() }
            let at = i.history.iter().rev().find(|h| h.0 == Status::Accepted).map_or(i.ts, |h| h.1);
            lines.push(Line::styled(clip(&format!("{} {} · from {} · accepted {}", if on { "▸" } else { " " },
                clip(crate::team::bare(&i.text).lines().next().unwrap_or(""), 60), i.from, crate::team::ago(at)), w),
                if on { theme::inverted() } else { theme::bold(theme::ink()) }));
            let p = &i.packet;
            if !p.is_object() {
                lines.push(Line::styled("    no packet: their agent didn't write one (m asks them)", theme::fg(theme::dim())));
            }
            if let Some(b) = p["branch"].as_str().filter(|b| !b.is_empty()) { lines.push(Line::styled(format!("    branch {b}"), theme::fg(theme::ink2()))) }
            let list = |k: &str| p[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>();
            if on {
                for (head, k, mark) in [("DONE", "done", "✓"), ("LEFT", "left", "○")] {
                    let v = list(k);
                    if v.is_empty() { continue }
                    lines.push(Line::styled(format!("    {head}"), theme::bold(theme::ink())));
                    for x in v { lines.push(Line::styled(pad(&format!("{mark} {x}"), w), theme::fg(if k == "left" { theme::ink() } else { theme::ink2() }))) }
                }
                if let Some(x) = p["watch"].as_str().filter(|x| !x.is_empty()) {
                    lines.push(Line::styled("    WATCH OUT", theme::bold(theme::ink())));
                    for l in crate::ui::wrap(x, w.saturating_sub(6).max(10)) { lines.push(Line::styled(format!("    {l}"), theme::fg(theme::ink2()))) }
                }
            } else if p.is_object() {
                lines.push(Line::styled(format!("    {} left", list("left").len()), theme::fg(theme::dim())));
            }
            let files = crate::team::packet_files(i);
            if !files.is_empty() { lines.push(Line::styled(pad(&format!("files: {}", files.join(" · ")), w), theme::fg(theme::dim()))) }
            for (who, both) in crate::team::clashes(i) {
                lines.push(Line::styled(pad(&format!("⚠ clashes with {who}'s handover: {}", both.join(", ")), w), theme::bold(theme::orange())));
            }
            lines.push(Line::raw(""));
        }
        f.render_widget(Paragraph::new(lines).scroll((sel_line.saturating_sub(1) as u16, 0)), area);
    }

    /// What you asked teammates, newest first, with where each one stands.
    fn draw_outgoing(&mut self, f: &mut Frame, area: Rect) {
        let rows = crate::team::outgoing().into_iter().map(|o| {
            let st = o.status();
            let words = if o.reply.is_empty() { String::new() } else { format!(": \"{}\"", o.reply) };
            let side = match crate::team::expires_in(&o) {
                // from day 12 of 14: say so, and whether they ever opened it
                Some(d) => format!("expires in {d}d{}", if o.history.iter().any(|h| h.0 == Status::Seen) { "" } else { ", never opened" }),
                None if st == Status::Expired => format!("expired {} · s send again · x let it go", crate::team::ago(o.history.last().map_or(o.ts, |h| h.1))),
                None => format!("{}{words} · {}", st.as_str(), crate::team::ago(o.history.last().map_or(o.ts, |h| h.1))),
            };
            (o.to.clone(), clip(&o.text, 80), side)
        }).collect();
        self.draw_list(f, area, rows, false, "", "you haven't asked anyone anything yet: a on Home asks a teammate's agent")
    }

    /// Requests: work teammates (or their agents) asked of your sessions, and questions when
    /// /answers is "ask". Nothing in here has reached any session.
    fn draw_requests(&mut self, f: &mut Frame, area: Rect, holds: &[crate::team::Hold], pending: &[crate::team::Incoming]) {
        let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
        if pending.is_empty() && holds.is_empty() {
            f.render_widget(Paragraph::new("\n  No requests. When a teammate or their agent asks one of your sessions to do something, it waits here:\n  nothing runs until you accept it.")
                .style(Style::new().fg(theme::dim())).wrap(Wrap { trim: false }), area);
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
            lines.push(Line::styled(clip(&head, w), if on { theme::inverted() }
                                                   else { Style::new().fg(theme::ink()).add_modifier(Modifier::BOLD) }));
            let body: Vec<String> = h.text.lines().flat_map(|l| if l.starts_with(['+', '-', '@']) { vec![clip(l, w.saturating_sub(4))] }
                                                             else { crate::ui::wrap(l, w.saturating_sub(4).max(10)) }).collect();
            let shown = if on { body.len() } else { body.len().min(2) };
            for l in &body[..shown] {
                let st = if l.starts_with("@@") { Style::new().fg(theme::ink2()) } else if l.starts_with('+') { Style::new().fg(theme::added()) }
                         else if l.starts_with('-') { Style::new().fg(theme::removed()) } else { Style::new() };
                lines.push(Line::styled(format!("    {l}"), st));
            }
            if body.len() > shown { lines.push(Line::styled(format!("    … {} more lines (↑↓ to pick it)", body.len() - shown), Style::new().fg(theme::dim()))) }
            lines.push(Line::raw(""));
        }
        for (n, i) in pending.iter().enumerate() {
            let on = n + holds.len() == self.sel[REQUESTS];
            if on { sel_line = lines.len() }
            // the picked one is open in front of them: now the sender may hear it was seen
            if on && !i.seen() { crate::team::mark_seen(&i.from, i.id) }
            let from = if i.from_session.is_empty() { i.from.clone() } else { format!("{}'s {}", i.from, i.from_session) };
            let kind = if i.question.is_some() { "question for" } else { "work for" };
            let head = format!("{} {from} · {kind} your {} · {}{}", if on { "▸" } else { " " }, i.session, crate::team::ago(i.ts),
                               if i.replied { " · you replied" } else if i.snoozed { " · later" } else { "" });
            lines.push(Line::styled(clip(&head, w), if on { theme::hot() }
                                                   else { Style::new().fg(theme::orange()).add_modifier(Modifier::BOLD) }));
            // the whole request, wrapped, on the picked one; two lines on the others
            let text: String = i.text.chars().filter(|c| !c.is_control() || *c == '\n').collect();
            let body: Vec<String> = if on { crate::ui::wrap(&text, w.saturating_sub(4).max(10)) }
                                    else { text.lines().map(|l| clip(l, w.saturating_sub(4))).collect() };
            let shown = if on { body.len() } else { body.len().min(2) };
            for l in &body[..shown] { lines.push(Line::raw(format!("    {l}"))) }
            if body.len() > shown { lines.push(Line::styled(format!("    … {} more lines (↑↓ to pick it)", body.len() - shown), Style::new().fg(theme::dim()))) }
            if let Some(why) = i.packet["held"].as_str() {
                lines.push(Line::styled(pad(&format!("{why}"), w), theme::fg(theme::orange())));
                if let (true, Some((a, _))) = (on, &i.draft) {
                    for l in crate::ui::wrap(&format!("answer ready: {a}"), w.saturating_sub(4).max(10)) { lines.push(Line::styled(format!("    {l}"), theme::fg(theme::ink2()))) }
                    lines.push(Line::styled("    Enter sends it as written · d says no", theme::fg(theme::dim())));
                }
            } else if i.packet.is_object() {
                let left = i.packet["left"].as_array().map_or(0, |a| a.len());
                let files = crate::team::packet_files(i);
                lines.push(Line::styled(pad(&format!("packet: {} done · {left} left{}", i.packet["done"].as_array().map_or(0, |a| a.len()),
                    if files.is_empty() { String::new() } else { format!(" · {}", files.join(", ")) }), w), theme::fg(theme::ink2())));
            }
            for (who, files) in crate::team::clashes(i) {
                lines.push(Line::styled(pad(&format!("⚠ clashes: {who} also handed you {}", files.join(", ")), w), theme::bold(theme::orange())));
            }
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
                                       Style::new().fg(theme::orange()).add_modifier(Modifier::BOLD)));
        }
        for p in ppl {
            for s in p.sessions.iter().filter_map(|s| s.waiting.as_ref().map(|w| (s, w))) {
                let (s, wt) = s;
                let who = if p.who.is_empty() { format!("your {}", s.name) } else { p.who.clone() };
                let on = if wt.on.is_empty() { "someone".to_string() } else { wt.on.clone() };
                problems.push(Line::styled(clip(&format!("{who} waits for {on} · {} in {}", wt.func, wt.file), w), Style::new().fg(theme::orange())));
            }
        }
        if problems.is_empty() {
            if let Some((fl, who)) = owners.iter().find(|(_, v)| v.len() > 1) {
                problems.push(Line::styled(clip(&format!("· {} both have changes in {fl} (different functions)", who.join(" and ")), w),
                                           Style::new().fg(theme::dim())));
            }
        }
        let shown = problems.len().min(3);
        let more = problems.len() - shown;
        lines.extend(problems.into_iter().take(shown));
        if more > 0 { lines.push(Line::styled(format!("  + {more} more"), Style::new().fg(theme::dim()))) }
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
                    lines.push(Line::styled(format!("  {a} · {n} {}", if n == 1 { "person" } else { "people" }), Style::new().fg(theme::dim()).add_modifier(Modifier::BOLD)));
                    last_area = a;
                }
            }
            if on { sel_line = lines.len() }
            let color = person_color(i);
            let busy: Vec<&Session> = p.sessions.iter().filter(|s| !matches!(s.status.as_str(), "idle" | "")).collect();
            let s = busy.first().copied().or(p.sessions.first());
            let fo = if p.who.is_empty() { 0 } else { crate::team::focus_of(&p.who) };
            let (what, place, when) = match s {
                _ if p.online && fo > now => (format!("◐ focusing till {}", crate::team::clock(fo)), String::new(), String::new()),
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
                Span::styled(format!("{:<what_w$}", clip(&format!("{what}{extra}"), what_w)), if p.online { base } else { base.fg(theme::dim()) }),
                Span::styled(format!("  {:<place_w$}", clip(&place, place_w)), Style::new().fg(theme::dim())),
                Span::styled(format!("  {when:>when_w$}"), Style::new().fg(if when == "now" { theme::ink() } else { theme::dim() })),
            ]));
            if !p.unmerged.is_empty() {
                let pad = name_w + 2;
                lines.push(Line::styled(format!("{:pad$}{}", "", clip(&format!("not on main yet: {}", unmerged_text(&p.unmerged)), w.saturating_sub(pad))),
                                        Style::new().fg(theme::dim())));
            }
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
                lines.push(Line::styled(clip(&text, w), Style::new().fg(theme::ink2())));
            }
        }
        // questions out to teammates' agents, not answered yet
        let open = crate::team::open_questions();
        if !open.is_empty() {
            if c.is_empty() { lines.push(Line::raw("")) }
            for (sess, to, q, t) in open.iter().rev().take(2) {
                let from = if sess.is_empty() { "you".to_string() } else { format!("your {sess}") };
                lines.push(Line::styled(clip(&format!("  ? {from} asked {to}: {q}  ({t}, waiting)"), w), Style::new().fg(theme::orange())));
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
            lines.push(Line::styled(format!("  free:  {}{more}", shown.join("   ")), Style::new().fg(theme::dim())));
        }
        // the latest happenings
        let act = crate::team::activity();
        lines.push(Line::raw(""));
        lines.push(Line::styled("  lately", Style::new().fg(theme::dim())));
        if act.is_empty() { lines.push(Line::styled("    (quiet)", Style::new().fg(theme::dim()))) }
        for (t, x) in act.iter().take(5) {
            lines.push(Line::from(vec![
                Span::raw(format!("    {}", clip(x, w.saturating_sub(14)))),
                Span::styled(format!("  {t}"), Style::new().fg(theme::dim())),
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
            let st = if on { theme::inverted() }
                     else if c.id.starts_with('@') && !c.online { Style::new().fg(theme::dim()) } else { Style::new() };
            Line::from(vec![
                Span::styled(format!(" {name}{}", " ".repeat(pad)), st),
                Span::styled(badge, if on { st } else if c.pinged { Style::new().fg(theme::orange()).add_modifier(Modifier::BOLD) } else { Style::new().fg(theme::ink()) }),
            ])
        }).collect();
        f.render_widget(Paragraph::new(list).block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(theme::dim()))), left);

        let Some(c) = chans.get(self.sel[CHAT]) else { return };
        crate::team::channel_read(&c.id); // on screen: read
        let right = Rect { x: right.x + 2, width: right.width.saturating_sub(3), ..right };
        let members = crate::team::channel_members(&c.id);
        let about = if c.id == "#team" { "everyone in the team · people only, agents never read it".to_string() }
            else if c.id.starts_with('@') {
                format!("private: only {} can read it{}", members.join(" and "),
                        if c.online { String::new() } else { " · they're offline: it reaches them when they're back".into() })
            } else { format!("group: {} · only its members can read it", members.join(", ")) };
        let mut lines = vec![Line::styled(clip(&about, right.width as usize), Style::new().fg(theme::dim())), Line::raw("")];
        let rows = crate::team::channel_msgs(&c.id);
        if rows.is_empty() {
            lines.push(Line::styled("No messages yet. Press Enter to write; @login pings someone, @team everyone.", Style::new().fg(theme::dim())));
        }
        for (t, who, x, ping) in &rows {
            let text = if *ping { Span::styled(x.clone(), Style::new().fg(theme::orange()).add_modifier(Modifier::BOLD)) } else { Span::raw(x.clone()) };
            lines.push(Line::from(vec![
                Span::styled(format!("{who:>10} "), Style::new().fg(if who == "you" { theme::ink2() } else { theme::ink() }).add_modifier(Modifier::BOLD)),
                text,
                Span::styled(format!("  {t}"), Style::new().fg(theme::dim())),
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
            let color = if p.who.is_empty() { theme::ink2() } else if p.online { theme::ink() } else { theme::dim() };
            // how their updates reach us: a direct line (instant) or git (every few seconds)
            let how = match (p.online && !p.who.is_empty()).then(|| crate::team::line_detail(&p.who)) {
                Some((Some(l), _)) if l.starts_with("via ") => format!("  line {l}"),
                Some((Some(l), _)) => format!("  direct line ({l})"),
                Some((None, Some(_))) => "  by git · /team link says why".to_string(),
                Some((None, None)) => "  by git".to_string(),
                None => String::new(),
            };
            lines.push(Line::styled(format!(" {dot} {}", p.label), Style::new().fg(color).add_modifier(Modifier::BOLD)));
            // its own line: beside the name it gets clipped in the narrow column
            if !how.is_empty() {
                lines.push(Line::styled(format!("  {}", clip(how.trim_start(), w.saturating_sub(2))), Style::new().fg(theme::dim())));
            }
            if p.sessions.is_empty() {
                lines.push(Line::styled("    no sessions", Style::new().fg(theme::dim())));
            }
            for s in &p.sessions {
                let on = idx == self.sel[0];
                if on { picked = Some(s) }
                let busy = !matches!(s.status.as_str(), "idle" | "");
                let what = if s.topic.is_empty() { String::new() } else { format!("  {}", s.topic) };
                let text = clip(&format!("{} {}{what}", if busy { "◆" } else { " " }, s.name), w.saturating_sub(2));
                let st = if on { theme::inverted() }
                         else if busy { Style::new().fg(theme::orange()) } else { Style::new() };
                lines.push(Line::styled(format!("  {text:<width$}", width = w.saturating_sub(2)), st));
                idx += 1;
            }
            lines.push(Line::raw(""));
        }
        f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::RIGHT).border_style(Style::new().fg(theme::dim()))), left);

        let Some(s) = picked else { return };
        let right = Rect { x: right.x + 2, width: right.width.saturating_sub(3), ..right };
        let who = if s.who.is_empty() { "you".to_string() } else { s.who.clone() };
        let busy = !matches!(s.status.as_str(), "idle" | "");
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!("{who} · {}", s.name), Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {}", s.status), Style::new().fg(if busy { theme::orange() } else { theme::dim() })),
            ]),
            Line::styled(if s.topic.is_empty() { "nothing asked yet".to_string() } else { s.topic.clone() },
                         Style::new().fg(if s.topic.is_empty() { theme::dim() } else { theme::ink() })),
        ];
        if !s.who.is_empty() {
            // what your agent sends them alone
            let trusted = crate::team::role_of(&s.who) == "trusted";
            let topics = crate::team::topics(&s.who);
            lines.push(Line::from(vec![Span::styled("✓ ", theme::fg(theme::dim())), Span::styled("facts from the code", theme::fg(theme::ink2())),
                                       Span::styled(" · everyone gets these", theme::fg(theme::dim()))]));
            lines.push(if trusted {
                Line::from(vec![Span::styled("✓ ", theme::bold(theme::orange())), Span::styled("promises", theme::bold(theme::ink())),
                                Span::styled(if topics.is_empty() { " · on anything (+ to pick topics)".to_string() } else { format!(" · on: {}", topics.join(", ")) }, theme::fg(theme::dim()))])
            } else {
                Line::from(vec![Span::styled("✗ ", theme::fg(theme::dim())), Span::styled("promises", theme::fg(theme::ink2())),
                                Span::styled(" · wait for you (t trusts them)", theme::fg(theme::dim()))])
            });
            lines.push(Line::from(vec![Span::styled("✗ ", theme::fg(theme::dim())), Span::styled("handovers", theme::fg(theme::ink2())),
                                       Span::styled(" · always you, no matter what", theme::fg(theme::dim()))]));
            let fo = crate::team::focus_of(&s.who);
            if fo > crate::team::now() { lines.push(Line::styled(format!("◐ focusing till {}", crate::team::clock(fo)), theme::fg(theme::ink2()))) }
        }
        if !s.now.is_empty() { lines.push(Line::styled(format!("now: {}", s.now), Style::new().fg(theme::orange()))) }
        if let Some(wt) = &s.waiting {
            lines.push(Line::styled(format!("waiting for {} to finish {} in {}", wt.on, wt.func, wt.file), Style::new().fg(theme::orange())));
        }
        // its fact sheet's own words: what it settled on and what's next
        if let Some(sh) = crate::team::sheet_of(&s.who, &s.name) {
            if !sh.decided.is_empty() { lines.push(Line::styled(format!("decided: {}", sh.decided), Style::new().fg(theme::ink2()))) }
            if !sh.next.is_empty() { lines.push(Line::styled(format!("next: {}", sh.next), Style::new().fg(theme::ink2()))) }
        }
        for (func, text) in crate::team::promises_of(&s.who, &s.name).into_iter().take(3) {
            lines.push(Line::styled(clip(&format!("⇢ counts on {func}: {text}"), 200), Style::new().fg(theme::ink2())));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for (fl, fns, t) in s.edits.iter().take(4) {
            let hot = now.saturating_sub(*t) < 600;
            let what = if fns.is_empty() { fl.clone() } else { format!("{fl} › {}", fns.join(", ")) };
            lines.push(Line::styled(format!("{} {what}", if hot { "✎ changing" } else { "  changed " }),
                                    Style::new().fg(if hot { theme::orange() } else { theme::ink2() })));
        }
        // its contracts: what it uses from teammates' changes, and who uses its changes
        let mine_made: Vec<&String> = s.edits.iter().flat_map(|e| e.1.iter()).collect();
        for p in ppl {
            let pw = if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
            if p.who == s.who { continue }
            let theirs: Vec<&String> = p.sessions.iter().flat_map(|x| x.edits.iter().flat_map(|e| e.1.iter())).collect();
            let relies: Vec<&String> = s.uses.iter().filter(|n| theirs.contains(n)).take(4).collect();
            if !relies.is_empty() {
                lines.push(Line::styled(format!("⇢ relies on {pw}'s {}", relies.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")), Style::new().fg(theme::ink2())));
            }
            let used: Vec<&String> = p.sessions.iter().flat_map(|x| x.uses.iter()).filter(|n| mine_made.contains(n)).take(4).collect();
            if !used.is_empty() {
                lines.push(Line::styled(format!("⇠ {pw} {} on its {}", if pw == "you" { "rely" } else { "relies" }, used.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")), Style::new().fg(theme::ink2())));
            }
        }
        if s.edits.is_empty() && !s.files.is_empty() { lines.push(Line::styled(format!("files: {}", s.files.join(", ")), Style::new().fg(theme::ink2()))) }
        if !s.branch.is_empty() {
            let pr = gh.prs.iter().find(|p| p.3 == s.branch);
            let mut spans = vec![Span::styled(format!("branch {}", s.branch), Style::new().fg(theme::ink2()))];
            if let Some((n, _, _, _, ci)) = pr {
                spans.push(Span::styled(format!("  · PR #{n}"), Style::new().fg(theme::ink2())));
                if !ci.is_empty() {
                    spans.push(Span::styled(format!("  CI {ci}"), Style::new().fg(match ci.as_str() { "✓" => theme::ink(), "✗" => theme::orange(), _ => theme::orange() })));
                }
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::styled("─".repeat(right.width as usize), Style::new().fg(theme::dim())));
        let rows: Vec<(String, String)> = if s.who.is_empty() {
            hub.get(&s.name).map(|m| lock(&m.msgs).iter()
                .filter(|x| matches!(x["role"].as_str(), Some("user" | "assistant")))
                .filter_map(|x| x["content"].as_str().filter(|c| !c.trim().is_empty() && !c.starts_with("[harness]"))
                    .map(|c| (x["role"].as_str().unwrap_or("").to_string(), c.to_string())))
                .collect()).unwrap_or_default()
        } else {
            crate::team::transcript_rows(&s.who, &s.name)
        };
        if rows.is_empty() { lines.push(Line::styled("no conversation yet", Style::new().fg(theme::dim()))) }
        for (r, t) in rows.iter().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
            let user = r == "user";
            lines.push(Line::from(vec![
                Span::styled(if user { "▸ " } else { "◂ " }, Style::new().fg(if user { theme::ink2() } else { theme::ink() })),
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
            f.render_widget(Paragraph::new(format!("\n  {msg}")).style(Style::new().fg(theme::dim())), area);
            return;
        }
        let h = area.height as usize;
        let top = sel.saturating_sub(h.saturating_sub(1));
        let kw = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(4).min(14);
        let lines: Vec<Line> = rows.iter().enumerate().skip(top).take(h).map(|(i, (k, t, side))| {
            let on = self.tab != ACTIVITY && i == sel;
            let base = if on { theme::inverted() } else { Style::new() };
            let room = (area.width as usize).saturating_sub(kw + side.chars().count() + 6);
            let t = clip(t, room);
            Line::from(vec![
                Span::styled(format!(" {k:<kw$}  "), if on { base } else { Style::new().fg(theme::dim()) }),
                Span::styled(format!("{t:<room$}"), base),
                Span::styled(format!("  {side} "), if on { base } else if side.ends_with('✗') { Style::new().fg(theme::orange()) }
                             else if side.ends_with('✓') { Style::new().fg(theme::ink()) } else { Style::new().fg(theme::ink2()) }),
            ])
        }).collect();
        f.render_widget(Paragraph::new(lines), area);
    }
}

/// What your agent is told when you start on a handover: the request and its packet, beginning
/// with the first thing left.
fn handover_prompt(i: &crate::team::Incoming) -> String {
    let p = &i.packet;
    let list = |k: &str| p[k].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| format!("- {x}")).collect::<Vec<_>>().join("\n");
    let mut t = format!("{} handed this over to you: {}\n", i.from, crate::team::bare(&i.text));
    if p.is_object() {
        if let Some(b) = p["branch"].as_str().filter(|b| !b.is_empty()) { t += &format!("\nTheir commits are on branch {b} (switch to it first if you're not on it; ask me if the working tree isn't clean).\n") }
        for (head, k) in [("Done", "done"), ("Left", "left")] {
            let l = list(k);
            if !l.is_empty() { t += &format!("\n{head}:\n{l}\n") }
        }
        if let Some(x) = p["watch"].as_str().filter(|x| !x.is_empty()) { t += &format!("\nWatch out: {x}\n") }
        let files = crate::team::packet_files(i);
        if !files.is_empty() { t += &format!("\nFiles: {}\n", files.join(", ")) }
        if let Some(first) = p["left"].as_array().and_then(|a| a.first()).and_then(|x| x.as_str()) { t += &format!("\nStart with: {first}") }
    }
    for (who, both) in crate::team::clashes(i) { t += &format!("\nCareful: {who} also handed me {}; check their changes there first.", both.join(", ")) }
    t
}

/// Contracts between people: (producer, consumer, name) where the consumer's fresh code uses
/// something the producer changed lately. The producer's work should be merged first.
/// Who has code that isn't on main yet, file by file: (person, file and functions, "").
fn editing_rows(ppl: &[Person]) -> Vec<(String, String, String)> {
    ppl.iter().filter(|p| !p.unmerged.is_empty()).flat_map(|p| {
        let who = if p.who.is_empty() { "you".to_string() } else { p.who.clone() };
        p.unmerged.iter().map(move |(f, fns)| (who.clone(), if fns.is_empty() { f.clone() } else { format!("{f} › {}", fns.join(", ")) },
                                                if p.online { String::new() } else { "away".into() }))
    }).collect()
}

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
    // one accent: people are told apart by name and place, not colour
    let _ = i;
    theme::ink()
}

/// `clip` for a line indented four spaces (clip itself squeezes leading space out).
fn pad(s: &str, n: usize) -> String {
    format!("    {}", clip(s, n.saturating_sub(4)))
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
        Person { who: who.into(), label: who.into(), online, sessions: vec![s], unmerged: vec![] }
    }

    #[test]
    fn unpushed_work_reads_as_files_and_functions() {
        let u = vec![("shop.py".to_string(), vec!["discount".to_string(), "total".to_string()]), ("web.py".to_string(), vec![])];
        assert_eq!(unmerged_text(&u), "shop.py › discount, total · web.py");
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

#[cfg(test)]
mod flow_screen_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(v: &mut TeamView, hub: &Hub, cwd: &Path) -> String {
        let mut t = Terminal::new(TestBackend::new(150, 44)).unwrap();
        t.draw(|f| v.draw(f, f.area(), hub, cwd)).unwrap();
        let b = t.backend().buffer().clone();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>().join("\n")
    }

    fn key(v: &mut TeamView, hub: &std::sync::Arc<Hub>, cwd: &Path, c: KeyCode) -> Act {
        v.key(KeyEvent::new(c, KeyModifiers::NONE), hub, cwd)
    }

    /// Steps 6-8 on screen: every new page and key, drawn from one demo team. QH_SCREENS=<dir>
    /// keeps each screen as a text file to look at.
    #[test]
    fn handovers_trust_and_focus_on_screen() {
        let _env = crate::hub::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        // decisions write seen-lists and roles: into a scratch folder, not your real state
        let root = std::env::temp_dir().join(format!("qh-flowscr-{}", std::process::id()));
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let repo = format!("flow/screens{}", std::process::id());
        crate::team::tests::install_flow_demo(&repo);
        let hub = Hub::new(crate::config::load_defaults(), Some(format!("flowscr-{}", std::process::id())), Box::new(|_, _| {}), true);
        let cwd = std::env::temp_dir();
        let mut v = TeamView { tab: 0, ..TeamView::default() };
        let keep = |name: &str, s: &str| if let Ok(d) = std::env::var("QH_SCREENS") { let _ = std::fs::write(Path::new(&d).join(format!("{name}.txt")), s); };
        let ok = |s: &str, want: &[&str]| for w in want { assert!(s.contains(w), "missing {w:?} in\n{s}") };

        // the menu, and the card on Home warns of the clash; g jumps to it
        let s = screen(&mut v, &hub, &cwd);
        keep("1-home", &s);
        ok(&s, &["Taken over", "Agent answered", "priya", "⚠ sam also handed you checkout.tsx", "g go to it", "f focus"]);
        // the menu: ← takes it, ↓ moves pages (not people on the map), → goes back in
        let person = v.person;
        key(&mut v, &hub, &cwd, KeyCode::Left);
        key(&mut v, &hub, &cwd, KeyCode::Down);
        assert_eq!((v.tab, v.person), (REQUESTS, person), "↓ in the menu goes to Asked you, the map's pick stays");
        key(&mut v, &hub, &cwd, KeyCode::Up);
        key(&mut v, &hub, &cwd, KeyCode::Right);
        assert_eq!(v.tab, 0);
        key(&mut v, &hub, &cwd, KeyCode::Right);
        assert_eq!(v.tab, 0, "→ on a page never changes the page");
        key(&mut v, &hub, &cwd, KeyCode::Char('g'));
        assert_eq!(v.tab, REQUESTS);
        let s = screen(&mut v, &hub, &cwd);
        keep("2-asked-you", &s);
        ok(&s, &["packet: 2 done · 1 left · checkout.tsx", "⚠ clashes: sam also handed you checkout.tsx", "held for you: it promises total"]);

        // later asks until when; the card goes away
        key(&mut v, &hub, &cwd, KeyCode::Char('l'));
        let s = screen(&mut v, &hub, &cwd);
        keep("3-later", &s);
        ok(&s, &["LATER, UNTIL WHEN?", "in 1 hour", "tomorrow morning", "when I'm done here"]);
        assert!(matches!(key(&mut v, &hub, &cwd, KeyCode::Enter), Act::Note(n) if n.contains("comes back")));
        let p = crate::team::pending();
        assert!(p[0].snoozed && p[0].until > crate::team::now(), "priya's card is put off for an hour");
        let s = screen(&mut v, &hub, &cwd);
        assert!(s.contains("· later"));

        // the held question: Enter sends the ready answer as written
        key(&mut v, &hub, &cwd, KeyCode::Down);
        let s = screen(&mut v, &hub, &cwd);
        keep("4-held-question", &s);
        ok(&s, &["answer ready: yes, always cents", "Enter sends it as written"]);
        assert!(matches!(key(&mut v, &hub, &cwd, KeyCode::Enter), Act::Note(n) if n.contains("sent your agent's answer to ravi")));

        // You asked: about to expire, and an expired one to send again or let go
        key(&mut v, &hub, &cwd, KeyCode::Char('3'));
        let s = screen(&mut v, &hub, &cwd);
        keep("5-you-asked", &s);
        ok(&s, &["expires in 2d, never opened", "s send again · x let it go"]);

        // Taken over: the packet, the clash, Enter starts on the first thing left
        key(&mut v, &hub, &cwd, KeyCode::Char('4'));
        let s = screen(&mut v, &hub, &cwd);
        keep("6-taken-over", &s);
        ok(&s, &["refund button wiring · from sam", "branch sam/refund-btn", "LEFT", "○ partial amount field", "WATCH OUT", "pence",
                 "⚠ clashes with priya's handover: checkout.tsx"]);
        match key(&mut v, &hub, &cwd, KeyCode::Enter) {
            Act::Work(role, text) => {
                assert_eq!(role, "sam's handover");
                assert!(text.contains("Start with: partial amount field") && text.contains("branch sam/refund-btn") && text.contains("priya also handed me"), "{text}");
            }
            _ => panic!("Enter on a taken handover starts the work"),
        }

        // Agent answered: facts and promises, and u takes one back
        key(&mut v, &hub, &cwd, KeyCode::Char('0'));
        assert_eq!(v.tab, ANSWERED);
        let s = screen(&mut v, &hub, &cwd);
        keep("7-agent-answered", &s);
        ok(&s, &["can I rely on refunds being idempotent?", "promise", "fact", "ravi: is total in cents?"]);

        // focus: the header goes grey with the count, the card holds; f again stops
        key(&mut v, &hub, &cwd, KeyCode::Char('1'));
        assert!(matches!(key(&mut v, &hub, &cwd, KeyCode::Char('f')), Act::Note(n) if n.starts_with("focusing till")));
        let s = screen(&mut v, &hub, &cwd);
        keep("8-focus", &s);
        ok(&s, &["focusing till", "f stop focusing", "◐ focusing till"]);
        assert!(!s.contains("g go to it"), "no card while focusing");
        assert!(matches!(key(&mut v, &hub, &cwd, KeyCode::Char('f')), Act::Note(n) if n.starts_with("focus off")));

        // People › sam: what your agent sends sam alone
        key(&mut v, &hub, &cwd, KeyCode::Enter);
        assert!(v.detail);
        key(&mut v, &hub, &cwd, KeyCode::Down); // priya's ui, then sam's
        let s = screen(&mut v, &hub, &cwd);
        keep("9-people-sam", &s);
        ok(&s, &["sam · ui", "facts from the code", "promises", "on: refunds", "handovers", "always you, no matter what", "◐ focusing till"]);
        // + adds a topic, t stops trusting
        key(&mut v, &hub, &cwd, KeyCode::Char('+'));
        for c in "payouts".chars() { key(&mut v, &hub, &cwd, KeyCode::Char(c)); }
        assert!(matches!(key(&mut v, &hub, &cwd, KeyCode::Enter), Act::Note(n) if n.contains("about payouts")));
        assert_eq!(crate::team::topics("sam"), ["refunds", "payouts"]);
        crate::team::tests::drop_demo(&repo);
    }
}
