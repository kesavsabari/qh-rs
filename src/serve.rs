//! `qh serve`: one core per project behind a user-only Unix socket, for the Mac app (and later
//! VS Code and the TUI). JSON lines both ways. A client's first line is
//! `{"type":"hello","token":"…"}`; it then gets a snapshot and every hub event, tagged with its tab.
//! Up: prompt, answer (to a confirm), stop, mode, model, new_tab, close_tab, files, plan,
//! plan_approve, memory, memory_save, fix_verify, activity,
//! inline_edit / inline_apply / inline_discard (Cmd+I: inline_delta then inline_result down).
//! On Windows the same socket is an AF_UNIX one (Windows 10 1803+), so clients there speak the
//! identical protocol (.NET: UnixDomainSocketEndPoint).

use crate::hub::{lock, Answer, Event, Hub};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
fn connect(p: &Path) -> std::io::Result<UnixStream> { UnixStream::connect(p) }
#[cfg(windows)]
use winsock::{connect, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

pub const PROTOCOL: u32 = 1;

/// Where the socket and token for a project live: state_dir/serve/<sha256(path)[..16]>.{sock,token}.
pub fn paths(project: &Path) -> (PathBuf, PathBuf) {
    let d = ring::digest::digest(&ring::digest::SHA256, project.to_string_lossy().as_bytes());
    let id: String = d.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = crate::config::state_dir().join("serve");
    let mut sock = dir.join(format!("{id}.sock"));
    // socket paths stop at ~104 bytes (macOS) / 108 (Linux): a long state dir moves just the socket
    if sock.as_os_str().len() > 100 {
        #[cfg(unix)]
        let who = unsafe { libc::getuid() }.to_string();
        #[cfg(windows)]
        let who = std::env::var("USERNAME").unwrap_or_default();
        sock = std::env::temp_dir().join(format!("qh-{who}")).join(format!("{id}.sock"));
    }
    (sock, dir.join(format!("{id}.token")))
}

fn token() -> String {
    let mut b = [0u8; 24];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b).expect("random token");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct State {
    clients: Mutex<Vec<UnixStream>>,
    /// the TUI's command and send code, shared by every client
    remote: Mutex<crate::ui::Remote>,
    confirms: Mutex<HashMap<u64, (String, Sender<Answer>)>>,
    next: AtomicU64,
    /// the channel last asked for; None until some client asks for the team room (then pushed)
    team_chan: Mutex<Option<String>>,
    /// inline edits waiting for inline_apply / inline_discard, by id
    inline: Mutex<HashMap<u64, crate::inline::Pending>>,
    /// each tab's last failed check (cmd, output tail), for `fix_verify`
    failed: Mutex<HashMap<String, (String, String)>>,
    /// the mode to return to when a plan is approved
    prev_mode: Mutex<String>,
}

impl State {
    fn broadcast(&self, v: &Value) {
        let line = format!("{v}\n");
        lock(&self.clients).retain_mut(|c| c.write_all(line.as_bytes()).is_ok());
    }
}

fn tab_json(hub: &Hub, name: &str, history: bool) -> Value {
    let Some(m) = hub.get(name) else { return json!({"name": name}) };
    let set = lock(&m.set);
    let model = if set.provider.is_empty() { set.model.clone() } else { format!("{}:{}", set.provider, set.model) };
    let mut v = json!({"name": name, "status": m.status(), "model": model, "provider": set.provider, "effort": set.effort,
        "think": set.think, "purpose": *lock(&m.purpose), "cwd": m.cwd(), "tokens": m.live_tokens()});
    drop(set);
    if history {
        // what a client draws: user and assistant text, tool calls by name; system prompts stay out
        let msgs: Vec<Value> = lock(&m.msgs).iter().filter_map(|x| match x["role"].as_str()? {
            "user" | "assistant" => Some(json!({"role": x["role"], "text": x["content"].as_str().unwrap_or(""),
                "tools": x["tool_calls"].as_array().map(|a| a.iter().map(|c| c["function"]["name"].clone()).collect::<Vec<_>>())})),
            _ => None,
        }).collect();
        v["messages"] = Value::Array(msgs);
    }
    v
}

/// Once per server: old sessions whose title is a long first request get a short one from the
/// model, in the background, using an idle tab; the sidebar list is sent again when done.
fn backfill_titles(hub: &Arc<Hub>, st: &Arc<State>) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) { return }
    let (hub, st) = (hub.clone(), st.clone());
    std::thread::spawn(move || {
        let Some(m) = hub.names().first().and_then(|n| hub.get(n)) else { return };
        let mut named = 0;
        for (path, first) in crate::session::untitled(&m.root, 30) {
            if m.status() != "idle" { break } // never compete with a running turn
            if let Some(t) = crate::engine::label(&hub, &m, &first) {
                crate::session::set_title(&path, &t);
                named += 1;
            }
        }
        if named > 0 { st.broadcast(&sessions_json(&hub, &m.root)) }
    });
}

/// The saved sessions for a folder's sidebar, and which tab shows each open one.
fn sessions_json(hub: &Hub, cwd: &std::path::Path) -> Value {
    let open: Vec<Value> = hub.names().iter().filter_map(|n| hub.get(n))
        .map(|m| json!({"tab": m.name, "id": lock(&m.session).file_stem().map(|s| s.to_string_lossy().to_string())})).collect();
    json!({"type": "sessions", "sessions": crate::session::for_project(cwd, 60), "open": open})
}

/// Every file the agent changed this session, before and now, for the review list.
fn changes_json(tab: &str, root: &std::path::Path, c: &crate::checkpoints::Checkpoints) -> Value {
    let files: Vec<Value> = c.changes().into_iter().map(|(p, b, a)| json!({"path": p, "before": b, "after": a})).collect();
    json!({"type": "changes", "tab": tab, "root": root, "files": files})
}

/// The turns that changed files, for the app's checkpoint timeline.
fn checkpoints_json(tab: &str, c: &crate::checkpoints::Checkpoints) -> Value {
    let items: Vec<Value> = c.turns().into_iter().map(|(turn, prompt, files)| json!({"turn": turn, "prompt": prompt, "files": files})).collect();
    json!({"type": "checkpoints", "tab": tab, "items": items})
}

fn snapshot(hub: &Hub) -> Value {
    let tabs: Vec<Value> = hub.names().iter().map(|n| tab_json(hub, n, true)).collect();
    json!({"type": "snapshot", "protocol": PROTOCOL, "version": crate::update::VERSION, "mode": hub.mode_name(),
        "project": std::env::current_dir().unwrap_or_default(), "tabs": tabs,
        "needs_setup": crate::setup::no_model(&hub.cfg)})
}

/// Hub events → JSON for every client. Confirms keep their reply channel here, keyed by id.
fn pump(hub: Arc<Hub>, st: Arc<State>, rx: Receiver<crate::ui::Ui>) {
    let mut last: HashMap<String, Option<(String, Value)>> = HashMap::new();
    while let Ok(crate::ui::Ui::Hub(tab, ev)) = rx.recv() {
        match ev {
            Event::Confirm { tool, args, reply, cwd, warn, guard } => {
                let id = st.next.fetch_add(1, Relaxed);
                let rules = hub.get(&tab).map(|m| lock(&m.set).allow.clone()).unwrap_or_default();
                let rule = if tool == "budget" { "budget".to_string() } else if guard && tool != "network" { String::new() } else { crate::ui::suggest_rule(&tool, &args, &rules) };
                lock(&st.confirms).insert(id, (rule.clone(), reply));
                st.broadcast(&json!({"type": "confirm", "tab": tab, "id": id, "tool": tool, "args": args,
                    "cwd": cwd, "warn": warn, "guard": guard, "always": rule}));
            }
            Event::Changed | Event::Added => {
                let tabs: Vec<Value> = hub.names().iter().map(|n| tab_json(&hub, n, false)).collect();
                st.broadcast(&json!({"type": "tabs", "mode": hub.mode_name(), "tabs": tabs}));
            }
            Event::Done { tokens } => {
                st.broadcast(&json!({"type": "turn_done", "tab": tab, "tokens": tokens}));
                if hub.mode_name() == "plan" { st.broadcast(&json!({"type": "plan_ready", "tab": tab})) }
            }
            Event::Log(l) => st.broadcast(&json!({"type": "log", "tab": tab, "message": l})),
            ev => {
                if let Event::Verify { cmd, ok, tail, .. } = &ev {
                    if *ok { lock(&st.failed).remove(&tab); } else { lock(&st.failed).insert(tab.clone(), (cmd.clone(), tail.clone())); }
                }
                // the app shows what a tool returned, not just its one-line summary
                let output = match &ev { Event::ToolResult { result, .. } => Some(result.chars().take(8000).collect::<String>()), _ => None };
                for mut v in crate::jsonl::events(&ev, last.entry(tab.clone()).or_default()) {
                    if v["type"] == "tool_result" { if let Some(o) = &output { v["output"] = json!(o) } }
                    v["tab"] = json!(tab);
                    st.broadcast(&v);
                }
                if matches!(&ev, Event::ToolResult { name, .. } if name == "todo") { st.broadcast(&plan_json(&tab)) }
            }
        }
    }
}

fn plan_json(tab: &str) -> Value {
    let items: Vec<Value> = crate::engine::plan_items(tab).into_iter().map(|(t, s)| json!({"text": t, "status": s})).collect();
    json!({"type": "plan", "tab": tab, "items": items})
}

/// The text sent when a plan is approved, with the user's edits if any.
fn approve_text(edits: &str) -> String {
    let e = edits.trim();
    if e.is_empty() { "Go ahead with the plan.".into() } else { format!("Go ahead with the plan, with these changes:\n{e}") }
}

/// The project's NOTES.md (yours to edit), the read-only instruction file qh loads, and the
/// memories `remember` saved (user and project scope; see memory.rs).
fn memory_json(root: &Path) -> Value {
    let path = root.join("NOTES.md");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let instr: Vec<Value> = crate::prompt::instructions(root).into_iter()
        .map(|(p, t)| json!({"path": p, "text": t, "readonly": true})).collect();
    let mems: Vec<Value> = [("user", crate::memory::global_dir()), ("project", crate::memory::project_dir(root))].iter()
        .flat_map(|(scope, d)| crate::memory::list(d).into_iter().map(move |m| json!({"scope": scope, "name": m.name,
            "description": m.description, "type": m.kind, "updated": m.updated, "path": m.file, "text": m.body})))
        .collect();
    json!({"type": "memory", "path": path, "text": text, "instructions": instr, "memories": mems})
}

/// Overwrite the project's NOTES.md atomically (temp file + rename, both inside the root).
fn save_memory(root: &Path, text: &str) -> Result<PathBuf, String> {
    let path = root.join("NOTES.md");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("NOTES.md is a symlink; refusing to write".into());
    }
    let tmp = root.join(format!(".NOTES.md.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| { let _ = std::fs::remove_file(&tmp); e.to_string() })?;
    Ok(path)
}

/// The team room: who's here, channels, what's waiting for you, recent activity, and one
/// channel's messages (marked read). `running: false` when this project has no team on.
fn team_state(chan: Option<&str>) -> Value {
    if !crate::team::running() {
        return json!({"type": "team", "running": false});
    }
    let people: Vec<Value> = crate::team::people().into_iter().map(|(login, online, status, sessions, role)|
        json!({"login": login, "online": online, "status": status, "sessions": sessions, "role": role})).collect();
    let channels: Vec<Value> = crate::team::channels().into_iter().map(|c|
        json!({"id": c.id, "label": c.label, "unread": c.unread, "pinged": c.pinged, "online": c.online})).collect();
    let waiting: Vec<Value> = crate::team::pending().into_iter().map(|i| json!({"from": i.from, "id": i.id, "session": i.session,
        "text": i.text, "question": i.question.is_some(), "replied": i.replied})).collect();
    let activity: Vec<Value> = crate::team::activity().into_iter().take(40).map(|(ago, x)| json!({"ago": ago, "text": x})).collect();
    // teammates' agents, with what each is doing and the files it's changing (the claims view)
    let agents: Vec<Value> = crate::team::overview().into_iter().flat_map(|p| {
        let (login, online) = (p.login.clone(), p.online);
        p.sessions.into_iter().map(move |v| json!({"login": login, "online": online, "name": v.name, "status": v.status,
            "topic": v.topic, "now": v.now, "branch": v.branch, "files": v.files, "waiting": v.waiting.is_some(),
            "edits": v.edits.into_iter().map(|(f, fns, _)| json!({"file": f, "functions": fns})).collect::<Vec<_>>()}))
    }).collect();
    let chan = chan.unwrap_or("#team");
    let msgs: Vec<Value> = crate::team::channel_msgs(chan).into_iter().map(|(when, who, text, ping)|
        json!({"who": who, "when": when, "text": text, "ping": ping})).collect();
    crate::team::channel_read(chan);
    json!({"type": "team", "running": true, "me": crate::team::me(), "repo": crate::team::repo(),
           "status": crate::team::status_line(), "people": people, "channels": channels, "waiting": waiting,
           "activity": activity, "chan": chan, "messages": msgs, "agents": agents, "reviews": crate::review::list(), "team_push": true})
}

/// Debounce for pushed team updates: send when the generation moved and the last push is at
/// least `gap` old, so a burst of changes goes out as one update.
fn push_due(seen: u64, now_gen: u64, since_last: Duration, gap: Duration) -> bool {
    now_gen != seen && since_last >= gap
}

/// Push the team room to every client when team state changes (after a client first asked).
fn team_pusher(st: Arc<State>) {
    const GAP: Duration = Duration::from_millis(250);
    let mut seen = crate::team::generation();
    let mut last = Instant::now() - GAP;
    loop {
        std::thread::sleep(GAP);
        let g = crate::team::generation();
        if !push_due(seen, g, last.elapsed(), GAP) { continue }
        seen = g;
        if lock(&st.clients).is_empty() { continue }
        // a review asked or answered: its own event, so an app can show it without the team room open
        for e in crate::review::take_events() { st.broadcast(&e) }
        let Some(chan) = lock(&st.team_chan).clone() else { continue };
        st.broadcast(&team_state(Some(&chan)));
        last = Instant::now();
    }
}

/// The project's files for the app's tree and @ picker, relative and sorted: git's view (tracked plus
/// untracked, .gitignore respected) in a repo, else a walk that skips the usual build folders. 20k max.
fn project_files(root: &Path) -> Vec<String> {
    const MAX: usize = 20000;
    let git = std::process::Command::new("git").args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .current_dir(root).stderr(std::process::Stdio::null()).output();
    let mut out: Vec<String> = match git {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).split('\0')
            .filter(|f| !f.is_empty() && root.join(f).is_file()).take(MAX).map(String::from).collect(),
        _ => {
            const SKIP: [&str; 10] = [".git", "node_modules", "target", "build", ".build", "dist", ".next", "DerivedData", "__pycache__", ".venv"];
            let (mut out, mut stack) = (vec![], vec![root.to_path_buf()]);
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let (p, name) = (e.path(), e.file_name().to_string_lossy().to_string());
                    if name.starts_with('.') || SKIP.contains(&name.as_str()) { continue }
                    if p.is_dir() { stack.push(p) } else if let Ok(r) = p.strip_prefix(root) {
                        // "/" on every OS, as git ls-files prints them: clients match paths across both
                        out.push(r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
                    }
                }
                if out.len() >= MAX { break }
            }
            out.truncate(MAX);
            out
        }
    };
    out.sort();
    out.dedup();
    out
}

/// One client's commands. Returns an error line for a bad command, nothing when it worked.
fn command(hub: &Arc<Hub>, st: &Arc<State>, v: &Value) -> Option<String> {
    let tab = v["tab"].as_str().unwrap_or("main");
    match v["type"].as_str().unwrap_or("") {
        "fix_verify" => {
            let Some((cmd, tail)) = lock(&st.failed).remove(tab) else { return Some(format!("no failed check on {tab}")) };
            if hub.get(tab).is_none() { return Some(format!("no tab {tab}")) }
            let out = lock(&st.remote).input(tab, &format!("The check `{cmd}` failed:\n{tail}\nFix it."), vec![]);
            for e in out { st.broadcast(&e) }
            hub.changed();
        }
        "prompt" => {
            // typed text and /commands both: exactly what Enter does in the TUI
            let text = v["text"].as_str().unwrap_or("");
            let images: Vec<String> = v["images"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            if text.is_empty() && images.is_empty() { return Some("prompt needs text".into()) }
            if hub.get(tab).is_none() { return Some(format!("no tab {tab}")) }
            let out = lock(&st.remote).input(tab, text, images);
            for e in out { st.broadcast(&e) }
            hub.changed();
        }
        "commands" => {
            let list = lock(&st.remote).commands(tab);
            st.broadcast(&json!({"type": "commands", "tab": tab, "commands": list}));
        }
        "answer" => {
            let id = v["id"].as_u64().unwrap_or(u64::MAX);
            let Some((rule, reply)) = lock(&st.confirms).remove(&id) else { return Some(format!("no open confirm {id}")) };
            let a = match v["answer"].as_str() {
                Some("yes") => Answer::Yes,
                Some("always") if !rule.is_empty() => Answer::Always(rule),
                _ => Answer::No,
            };
            let _ = reply.send(a);
            st.broadcast(&json!({"type": "confirm_closed", "id": id}));
        }
        "stop" => hub.interrupt(tab),
        "sessions" => {
            backfill_titles(hub, st);
            let cwd = hub.get(tab).map(|m| m.root.clone()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            st.broadcast(&sessions_json(hub, &cwd));
        }
        "delete_session" => {
            // the transcript and its checkpoints; a tab showing it closes first, unless it's the last one
            let id = v["id"].as_str().unwrap_or("");
            let dir = crate::session::dir();
            let path = dir.join(format!("{id}.jsonl"));
            if id.is_empty() || id.contains(['/', '\\', '.']) || !path.exists() { return Some(format!("no saved session {id}")) }
            if let Some(m) = hub.names().iter().filter_map(|n| hub.get(n)).find(|m| *lock(&m.session) == path) {
                if hub.names().len() == 1 { return Some("that session is the only open tab: open another one first".into()) }
                hub.close(&m.name);
            }
            if let Err(e) = std::fs::remove_file(&path) { return Some(e.to_string()) }
            let _ = std::fs::remove_dir_all(dir.join("checkpoints").join(id));
            let _ = std::fs::remove_file(path.with_extension("title"));
            hub.changed();
            let cwd = hub.names().first().and_then(|n| hub.get(n)).map(|m| m.root.clone()).unwrap_or_default();
            st.broadcast(&sessions_json(hub, &cwd));
        }
        "resume" => {
            let id = v["id"].as_str().unwrap_or("");
            let path = crate::session::dir().join(format!("{id}.jsonl"));
            if id.is_empty() || id.contains(['/', '\\']) || !path.exists() { return Some(format!("no saved session {id}")) }
            // already open in a tab: just go there
            if let Some(m) = hub.names().iter().filter_map(|n| hub.get(n)).find(|m| *lock(&m.session) == path) {
                st.broadcast(&json!({"type": "active", "tab": m.name}));
                return None;
            }
            let name = (1..).map(|i| if i == 1 { "resumed".to_string() } else { format!("resumed{i}") }).find(|n| hub.get(n).is_none()).unwrap_or_default();
            let root = hub.names().first().and_then(|n| hub.get(n)).map(|m| m.root.clone());
            let title = crate::session::for_project(&root.clone().unwrap_or_default(), 200).into_iter()
                .find(|x| x["id"] == id).and_then(|x| x["title"].as_str().map(String::from)).unwrap_or_default();
            let m = match hub.add(&name, None, "", "", Some(path), false, root) { Ok(m) => m, Err(e) => return Some(e) };
            // the tab carries the session's title, not "resumed2": labels only, never the system prompt
            if !title.is_empty() {
                *lock(&m.purpose) = title;
                m.named.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            hub.changed();
            let mut h = tab_json(hub, &name, true);
            h["type"] = json!("history");
            st.broadcast(&h);
            st.broadcast(&json!({"type": "active", "tab": name}));
        }
        "team" => {
            let chan = v["chan"].as_str().unwrap_or("#team").to_string();
            st.broadcast(&team_state(Some(&chan)));
            *lock(&st.team_chan) = Some(chan);
        }
        "team_send" => {
            let (chan, text) = (v["chan"].as_str().unwrap_or("#team"), v["text"].as_str().unwrap_or(""));
            if let Err(e) = crate::team::channel_send(chan, text) { return Some(e) }
            st.broadcast(&team_state(Some(chan)));
            *lock(&st.team_chan) = Some(chan.to_string());
        }
        "team_transcript" => {
            let (login, session) = (v["login"].as_str().unwrap_or(""), v["session"].as_str().unwrap_or(""));
            let rows: Vec<Value> = crate::team::transcript_rows(login, session).into_iter()
                .map(|(role, text)| json!({"role": role, "text": text})).collect();
            st.broadcast(&json!({"type": "team_transcript", "login": login, "session": session, "rows": rows}));
        }
        "files" => {
            let root = hub.get(tab).map(|m| m.root.clone()).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            st.broadcast(&json!({"type": "files", "tab": tab, "root": root, "files": project_files(&root)}));
        }
        "activity" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let items = crate::activity::read(&lock(&m.session), crate::activity::MAX);
            st.broadcast(&json!({"type": "activity", "tab": tab, "items": items}));
        }
        "tasks" => st.broadcast(&json!({"type": "tasks", "tab": tab, "text": crate::bg::list(tab)})),
        "changes" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            st.broadcast(&changes_json(tab, &m.root, &lock(&m.turn).ckpt));
        }
        "accept" | "reject" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let path = v["path"].as_str().unwrap_or("");
            let mut t = lock(&m.turn);
            if v["type"] == "accept" { t.ckpt.accept(path) } else if let Err(e) = t.ckpt.reject(path) { return Some(e) }
            st.broadcast(&changes_json(tab, &m.root, &t.ckpt));
        }
        "inline_edit" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let rel = v["path"].as_str().unwrap_or("").to_string();
            let (start, end) = (v["start"].as_u64().unwrap_or(0) as usize, v["end"].as_u64().unwrap_or(0) as usize);
            let instruction = v["instruction"].as_str().unwrap_or("").to_string();
            if instruction.trim().is_empty() { return Some("inline_edit needs an instruction".into()) }
            let abs = match crate::inline::resolve(&m.root, &rel) { Ok(p) => p, Err(e) => return Some(e) };
            let text = match std::fs::read_to_string(&abs) { Ok(t) => t, Err(e) => return Some(format!("{rel}: {e}")) };
            let (pre, sel, post) = match crate::inline::slice(&text, start, end, crate::inline::CONTEXT) { Ok(x) => x, Err(e) => return Some(e) };
            let id = st.next.fetch_add(1, Relaxed);
            let (provider, model) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
            let ctx = hub.ctx_for(&m, false).0;
            let user = crate::inline::user_msg(&rel, &pre, &sel, &post, &instruction);
            let (st, hub, tab) = (st.clone(), hub.clone(), tab.to_string());
            std::thread::spawn(move || {
                let mut on = |d: &str| st.broadcast(&json!({"type": "inline_delta", "id": id, "tab": tab, "delta": d}));
                match crate::inline::run(&provider, &model, ctx, &hub.cfg.keep_alive, &user, &mut on) {
                    Ok(after) => {
                        let p = crate::inline::Pending { tab, abs, rel, start, end, before: sel, after };
                        st.broadcast(&crate::inline::result_json(id, &p));
                        lock(&st.inline).insert(id, p);
                    }
                    Err(e) => st.broadcast(&json!({"type": "error", "message": e, "re": "inline_edit", "id": id, "tab": tab})),
                }
            });
        }
        "inline_apply" => {
            let id = v["id"].as_u64().unwrap_or(0);
            let Some(p) = lock(&st.inline).remove(&id) else { return Some(format!("no inline edit {id}")) };
            let Some(m) = hub.get(&p.tab) else { return Some(format!("no tab {}", p.tab)) };
            let text = match std::fs::read_to_string(&p.abs) { Ok(t) => t, Err(e) => return Some(format!("{}: {e}", p.rel)) };
            // the file moved on since the model saw it: refuse rather than clobber
            match crate::inline::slice(&text, p.start, p.end, 0) {
                Ok((_, sel, _)) if sel == p.before => {}
                _ => return Some(format!("{} changed since the inline edit; ask again", p.rel)),
            }
            let (turn, prompt) = {
                let msgs = lock(&m.msgs);
                let users: Vec<&Value> = msgs.iter().filter(|x| x["role"] == "user").collect();
                (users.len().saturating_sub(1), format!("inline edit {}:{}-{}", p.rel, p.start, p.end))
            };
            let mut t = lock(&m.turn);
            t.ckpt.snapshot("write_file", &json!({"path": p.abs.to_string_lossy()}), &m.root, turn, &prompt);
            if let Err(e) = std::fs::write(&p.abs, crate::inline::splice(&text, p.start, p.end, &p.after)) {
                return Some(format!("{}: {e}", p.rel));
            }
            st.broadcast(&json!({"type": "inline_applied", "id": id, "tab": p.tab, "path": p.rel}));
            st.broadcast(&changes_json(&p.tab, &m.root, &t.ckpt));
        }
        "inline_discard" => { lock(&st.inline).remove(&v["id"].as_u64().unwrap_or(0)); }
        "checkpoints" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            st.broadcast(&checkpoints_json(tab, &lock(&m.turn).ckpt));
        }
        "rewind" => {
            // files back to before user message `turn`; with "chat", the conversation too
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            if m.status() != "idle" { return Some("busy: stop the turn first".into()) }
            let Some(turn) = v["turn"].as_u64().map(|t| t as usize) else { return Some("rewind needs turn".into()) };
            let restored = lock(&m.turn).ckpt.rewind(turn);
            let mut prompt = String::new();
            if v["chat"].as_bool().unwrap_or(false) {
                let mut msgs = lock(&m.msgs);
                if let Some(i) = msgs.iter().enumerate().filter(|(_, x)| x["role"] == "user").nth(turn).map(|(i, _)| i) {
                    prompt = msgs[i]["content"].as_str().unwrap_or("").to_string();
                    msgs.truncate(i);
                    crate::session::save(&lock(&m.session), &msgs);
                    m.touched(&msgs);
                }
                drop(msgs);
                hub.save();
                let mut h = tab_json(hub, tab, true);
                h["type"] = json!("history");
                st.broadcast(&h);
            }
            st.broadcast(&json!({"type": "rewound", "tab": tab, "turn": turn, "restored": restored, "prompt": prompt}));
            let t = lock(&m.turn);
            st.broadcast(&checkpoints_json(tab, &t.ckpt));
            st.broadcast(&changes_json(tab, &m.root, &t.ckpt));
            drop(t);
            hub.changed();
        }
        "mode" => {
            let (was, to) = (hub.mode_name(), v["mode"].as_str().unwrap_or(""));
            if !hub.set_mode(to) { return Some("unknown mode".into()) }
            if to == "plan" && was != "plan" { *lock(&st.prev_mode) = was.into() }
            hub.changed()
        }
        "plan" => st.broadcast(&plan_json(tab)),
        "plan_approve" => {
            if hub.get(tab).is_none() { return Some(format!("no tab {tab}")) }
            if hub.mode_name() == "plan" { hub.set_mode(&lock(&st.prev_mode).clone()); }
            let out = lock(&st.remote).input(tab, &approve_text(v["edits"].as_str().unwrap_or("")), vec![]);
            for e in out { st.broadcast(&e) }
            hub.changed();
        }
        "memory" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            st.broadcast(&memory_json(&m.root));
        }
        "memory_save" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let Some(text) = v["text"].as_str() else { return Some("memory_save needs text".into()) };
            if let Err(e) = save_memory(&m.root, text) { return Some(e) }
            st.broadcast(&memory_json(&m.root));
        }
        "model" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let mut s = lock(&m.set);
            (s.provider, s.model) = crate::providers::parse_model(v["model"].as_str().unwrap_or(""));
            drop(s);
            hub.changed();
        }
        "new_tab" => {
            let name = v["name"].as_str().map(String::from).unwrap_or_else(|| {
                (2..).map(|i| format!("tab{i}")).find(|n| hub.get(n).is_none()).unwrap_or_default()
            });
            if let Err(e) = hub.add(&name, None, "", "", None, false, None) { return Some(e) }
            hub.changed();
        }
        "close_tab" => { hub.close(tab); hub.changed() }
        "snapshot" => return None, // handled by the caller
        t => return Some(format!("unknown command {t}")),
    }
    None
}

fn client(hub: Arc<Hub>, st: Arc<State>, s: UnixStream, want: Arc<String>) {
    let Ok(mut w) = s.try_clone() else { return };
    let mut lines = BufReader::new(s).lines();
    let hello: Value = lines.next().and_then(Result::ok).and_then(|l| serde_json::from_str(&l).ok()).unwrap_or_default();
    if hello["type"] != "hello" || hello["token"].as_str() != Some(want.as_str()) {
        let _ = writeln!(w, "{}", json!({"type": "error", "message": "bad hello or token"}));
        return;
    }
    // snapshot and joining the broadcast list under one lock, so no event falls between them
    {
        let mut cs = lock(&st.clients);
        if writeln!(w, "{}", snapshot(&hub)).is_err() { return }
        for (id, (rule, _)) in lock(&st.confirms).iter() {
            let _ = writeln!(w, "{}", json!({"type": "confirm_pending", "id": id, "always": rule}));
        }
        if let Ok(c) = w.try_clone() { cs.push(c) }
    }
    for line in lines.map_while(Result::ok) {
        let v: Value = match serde_json::from_str(&line) { Ok(v) => v, Err(_) => continue };
        if v["type"] == "snapshot" {
            let _ = writeln!(w, "{}", snapshot(&hub));
        } else if v["type"].as_str().is_some_and(|t| t.starts_with("setup")) {
            // setup talks to the network (key tests, sign-in): off this thread, answered to this client
            let (Ok(out), h) = (w.try_clone(), hub.clone()) else { continue };
            std::thread::spawn(move || setup(&h, &v, out));
        } else if let Some(e) = command(&hub, &st, &v) {
            let _ = writeln!(w, "{}", json!({"type": "error", "message": e, "re": v["type"]}));
        }
    }
}

/// The setup screen's commands: state, key, chatgpt, models, default. Each answers with a
/// `setup_result` (and the fresh `setup` state after anything that changed it).
fn setup(hub: &Arc<Hub>, v: &Value, w: UnixStream) {
    let say = |x: &Value| { let _ = writeln!(&w, "{x}"); };
    let step = v["type"].as_str().unwrap_or("").trim_start_matches("setup_").to_string();
    let r: Result<Value, String> = match step.as_str() {
        "setup" | "state" => Ok(Value::Null),
        "key" => crate::setup::connect_key(v["provider"].as_str().unwrap_or(""), v["key"].as_str().unwrap_or(""))
            .map(|n| json!({"models": n})),
        "chatgpt" => {
            let live = Mutex::new(w.try_clone().ok());
            crate::setup::sign_in_chatgpt(&|m| if let Some(o) = lock(&live).as_mut() {
                let _ = writeln!(o, "{}", json!({"type": "setup_progress", "message": m}));
            }).map(|who| json!({"account": who}))
        }
        "models" => Ok(json!({"models": crate::setup::all_models().into_iter()
            .map(|(id, from)| json!({"id": id, "from": from})).collect::<Vec<_>>()})),
        "default" => {
            let m = v["model"].as_str().unwrap_or("");
            crate::setup::set_default(m).map(|_| {
                let (p, id) = crate::providers::parse_model(m);
                hub.set_default_model(&p, &id);
                // tabs still on the fallback model move to the new default
                for n in hub.names() {
                    if let Some(t) = hub.get(&n) { let mut s = lock(&t.set); (s.provider, s.model) = (p.clone(), id.clone()); }
                }
                hub.changed();
                json!({"model": m})
            })
        }
        other => Err(format!("unknown setup step {other}")),
    };
    match r {
        Ok(x) => {
            if !x.is_null() { say(&json!({"type": "setup_result", "step": step, "ok": true, "data": x})) }
            say(&crate::setup::state());
        }
        Err(e) => say(&json!({"type": "setup_result", "step": step, "ok": false, "error": e})),
    }
}

/// Serve this hub until killed. Refuses when another `qh serve` already answers for the project.
pub fn run(hub: Arc<Hub>, rx: Receiver<crate::ui::Ui>) -> i32 {
    let project = std::env::current_dir().unwrap_or_default();
    let (sock, tok_path) = paths(&project);
    for d in [sock.parent(), tok_path.parent()].into_iter().flatten() {
        let _ = std::fs::create_dir_all(d);
        // Windows: both live under %LOCALAPPDATA% (or the user's %TEMP%), private to the user by default
        #[cfg(unix)]
        {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
            // the socket may sit in shared /tmp: a folder someone else made there could be watched
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(d).map(|m| m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0).unwrap_or(true) {
                eprintln!("qh serve: {} isn't a private folder of yours; refusing", d.display());
                return 1;
            }
        }
    }
    if connect(&sock).is_ok() {
        // a client attaches to that one instead: same line shape as "listening"
        println!("{}", json!({"type": "running", "socket": sock, "token_file": tok_path, "protocol": PROTOCOL}));
        return 0;
    }
    let _ = std::fs::remove_file(&sock); // stale, nobody answered
    let tok = token();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let wrote = opts.open(&tok_path).and_then(|mut f| f.write_all(tok.as_bytes()));
    if let Err(e) = wrote {
        eprintln!("qh serve: can't write {}: {e}", tok_path.display());
        return 1;
    }
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => { eprintln!("qh serve: {}: {e}", sock.display()); return 1 }
    };
    #[cfg(unix)]
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    // the app reads this line to know where to connect
    println!("{}", json!({"type": "listening", "socket": sock, "token_file": tok_path, "protocol": PROTOCOL, "pid": std::process::id()}));
    let _ = std::io::stdout().flush();
    let st = Arc::new(State { clients: Mutex::new(vec![]), remote: Mutex::new(crate::ui::Remote::new(hub.clone())), confirms: Mutex::new(HashMap::new()),
        next: AtomicU64::new(1), team_chan: Mutex::new(None), inline: Mutex::new(HashMap::new()), failed: Mutex::new(HashMap::new()),
        prev_mode: Mutex::new("auto-edit".into()) });
    { let s = st.clone(); std::thread::spawn(move || team_pusher(s)); }
    { let (h, s) = (hub.clone(), st.clone()); std::thread::spawn(move || pump(h, s, rx)); }
    match lock(&st.remote).start_team() {
        Some(Ok(s)) => eprintln!("team: {s}"),
        Some(Err(e)) => eprintln!("team presence: {e}"),
        None => {}
    }
    let want = Arc::new(tok);
    for s in listener.incoming().map_while(Result::ok) {
        let (h, st, want) = (hub.clone(), st.clone(), want.clone());
        std::thread::spawn(move || client(h, st, s, want));
    }
    0
}

/// AF_UNIX stream sockets on Windows, shaped like std's Unix ones. A connected socket is wrapped
/// in a TcpStream: std's send/recv/try_clone work on any stream socket, whatever its family.
#[cfg(windows)]
mod winsock {
    use std::io;
    use std::net::TcpStream;
    use std::os::windows::io::{FromRawSocket, RawSocket};
    use std::path::Path;
    use windows_sys::Win32::Networking::WinSock as ws;

    pub type UnixStream = TcpStream;

    pub struct UnixListener(ws::SOCKET);

    // a sockaddr_un: family, then a NUL-terminated path of at most 108 bytes
    #[repr(C)]
    struct SockaddrUn { family: u16, path: [u8; 108] }

    fn addr(p: &Path) -> io::Result<SockaddrUn> {
        let b = p.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path isn't UTF-8"))?.as_bytes();
        if b.len() >= 108 { return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long")) }
        let mut a = SockaddrUn { family: ws::AF_UNIX, path: [0; 108] };
        a.path[..b.len()].copy_from_slice(b);
        Ok(a)
    }

    fn socket() -> io::Result<ws::SOCKET> {
        // std starts Winsock on its first use; this does the same for our raw calls
        let _ = std::net::UdpSocket::bind("127.0.0.1:0");
        let s = unsafe { ws::socket(ws::AF_UNIX as i32, ws::SOCK_STREAM, 0) };
        if s == ws::INVALID_SOCKET { Err(last()) } else { Ok(s) }
    }

    fn last() -> io::Error { io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() }) }

    impl UnixListener {
        pub fn bind(p: &Path) -> io::Result<Self> {
            let (s, a) = (socket()?, addr(p)?);
            let len = std::mem::size_of::<SockaddrUn>() as i32;
            if unsafe { ws::bind(s, &a as *const _ as *const ws::SOCKADDR, len) } != 0 || unsafe { ws::listen(s, 16) } != 0 {
                let e = last();
                unsafe { ws::closesocket(s) };
                return Err(e);
            }
            Ok(UnixListener(s))
        }
        pub fn incoming(&self) -> impl Iterator<Item = io::Result<UnixStream>> + '_ {
            std::iter::from_fn(move || {
                let c = unsafe { ws::accept(self.0, std::ptr::null_mut(), std::ptr::null_mut()) };
                Some(if c == ws::INVALID_SOCKET { Err(last()) } else { Ok(unsafe { TcpStream::from_raw_socket(c as RawSocket) }) })
            })
        }
    }

    impl Drop for UnixListener {
        fn drop(&mut self) { unsafe { ws::closesocket(self.0) }; }
    }

    pub fn connect(p: &Path) -> io::Result<UnixStream> {
        let (s, a) = (socket()?, addr(p)?);
        if unsafe { ws::connect(s, &a as *const _ as *const ws::SOCKADDR, std::mem::size_of::<SockaddrUn>() as i32) } != 0 {
            let e = last();
            unsafe { ws::closesocket(s) };
            return Err(e);
        }
        Ok(unsafe { TcpStream::from_raw_socket(s as RawSocket) })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn approve_text_adds_edits() {
        assert_eq!(super::approve_text("  "), "Go ahead with the plan.");
        assert!(super::approve_text("skip step 2").ends_with("\nskip step 2"));
    }

    #[test]
    fn memory_save_and_read() {
        let d = std::env::temp_dir().join(format!("qh-mem-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("AGENTS.md"), "be nice").unwrap();
        let p = super::save_memory(&d, "- fact one\n").unwrap();
        assert_eq!(p, d.join("NOTES.md"));
        let v = super::memory_json(&d);
        assert_eq!(v["text"], "- fact one\n");
        assert_eq!(v["instructions"][0]["text"], "be nice");
        assert!(std::fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn todo_updates_plan_items() {
        let r = crate::engine::todo_for_test("plan-test-tab", &serde_json::json!({"items": [
            {"text": "a", "status": "completed"}, {"text": "b", "status": "in_progress"}, {"text": "c"}]}));
        assert!(r.starts_with("todo saved"));
        let v = super::plan_json("plan-test-tab");
        assert_eq!(v["items"], serde_json::json!([{"text":"a","status":"done"},{"text":"b","status":"active"},{"text":"c","status":"pending"}]));
    }
    use super::*;

    #[test]
    fn team_push_debounces() {
        let gap = Duration::from_millis(250);
        assert!(!push_due(3, 3, gap * 4, gap), "nothing changed");
        assert!(!push_due(3, 9, Duration::from_millis(100), gap), "burst waits for the gap");
        assert!(push_due(3, 9, gap, gap));
        let g = crate::team::generation();
        crate::team::bump();
        assert!(crate::team::generation() > g);
    }

    #[test]
    fn project_files_follow_gitignore() {
        let d = std::env::temp_dir().join(format!("qh-serve-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (f, body) in [("src/a.rs", "x"), (".gitignore", "out/\n*.log\n"), ("out/b.js", "x"), ("debug.log", "x"), ("README.md", "x")] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        // no repo yet: a plain walk, hidden files skipped
        assert_eq!(project_files(&d), ["README.md", "debug.log", "out/b.js", "src/a.rs"]);
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&d).status().is_ok_and(|s| s.success());
        if ok {
            // a repo: untracked files show, ignored ones don't
            assert_eq!(project_files(&d), [".gitignore", "README.md", "src/a.rs"]);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The socket every client uses, both ways at once from two threads (the hub broadcasts while
    /// a client's reader is blocked): on Windows this is the AF_UNIX shim in `winsock`.
    #[test]
    fn socket_round_trip_full_duplex() {
        let p = std::env::temp_dir().join(format!("qh-sock-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let l = UnixListener::bind(&p).unwrap();
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            let c = connect(&p2).unwrap();
            let mut w = c.try_clone().unwrap();
            writeln!(w, "hello").unwrap();
            BufReader::new(c).lines().next().unwrap().unwrap()
        });
        let s = l.incoming().next().unwrap().unwrap();
        let mut w = s.try_clone().unwrap();
        // a reader parked on the socket must not block a write on its clone
        let r = std::thread::spawn(move || BufReader::new(s).lines().next().unwrap().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(50));
        writeln!(w, "events").unwrap();
        assert_eq!(r.join().unwrap(), "hello");
        assert_eq!(t.join().unwrap(), "events");
        assert!(connect(&p.with_extension("none")).is_err());
        drop(l);
        let _ = std::fs::remove_file(&p);
    }
}