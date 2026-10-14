//! `qh serve`: one core per project behind a user-only Unix socket, for the Mac app (and later
//! VS Code and the TUI). JSON lines both ways. A client's first line is
//! `{"type":"hello","token":"…"}`; it then gets a snapshot and every hub event, tagged with its tab.
//! Up: prompt, answer (to a confirm), stop, mode, model, new_tab, close_tab.

use crate::hub::{lock, Answer, Event, Hub};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
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
        sock = std::env::temp_dir().join(format!("qh-{}", unsafe { libc::getuid() })).join(format!("{id}.sock"));
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
    let mut v = json!({"name": name, "status": m.status(), "model": model, "effort": set.effort,
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
                let rule = if guard && tool != "network" { String::new() } else { crate::ui::suggest_rule(&tool, &args, &rules) };
                lock(&st.confirms).insert(id, (rule.clone(), reply));
                st.broadcast(&json!({"type": "confirm", "tab": tab, "id": id, "tool": tool, "args": args,
                    "cwd": cwd, "warn": warn, "guard": guard, "always": rule}));
            }
            Event::Changed | Event::Added => {
                let tabs: Vec<Value> = hub.names().iter().map(|n| tab_json(&hub, n, false)).collect();
                st.broadcast(&json!({"type": "tabs", "mode": hub.mode_name(), "tabs": tabs}));
            }
            Event::Done { tokens } => st.broadcast(&json!({"type": "turn_done", "tab": tab, "tokens": tokens})),
            Event::Log(l) => st.broadcast(&json!({"type": "log", "tab": tab, "message": l})),
            ev => {
                // the app shows what a tool returned, not just its one-line summary
                let output = match &ev { Event::ToolResult { result, .. } => Some(result.chars().take(8000).collect::<String>()), _ => None };
                for mut v in crate::jsonl::events(&ev, last.entry(tab.clone()).or_default()) {
                    if v["type"] == "tool_result" { if let Some(o) = &output { v["output"] = json!(o) } }
                    v["tab"] = json!(tab);
                    st.broadcast(&v);
                }
            }
        }
    }
}

/// One client's commands. Returns an error line for a bad command, nothing when it worked.
fn command(hub: &Arc<Hub>, st: &State, v: &Value) -> Option<String> {
    let tab = v["tab"].as_str().unwrap_or("main");
    match v["type"].as_str().unwrap_or("") {
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
        "changes" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let files: Vec<Value> = lock(&m.turn).ckpt.changes().into_iter()
                .map(|(p, b, a)| json!({"path": p, "before": b, "after": a})).collect();
            st.broadcast(&json!({"type": "changes", "tab": tab, "root": m.root, "files": files}));
        }
        "accept" | "reject" => {
            let Some(m) = hub.get(tab) else { return Some(format!("no tab {tab}")) };
            let path = v["path"].as_str().unwrap_or("");
            let mut t = lock(&m.turn);
            if v["type"] == "accept" { t.ckpt.accept(path) } else if let Err(e) = t.ckpt.reject(path) { return Some(e) }
            let files: Vec<Value> = t.ckpt.changes().into_iter().map(|(p, b, a)| json!({"path": p, "before": b, "after": a})).collect();
            st.broadcast(&json!({"type": "changes", "tab": tab, "root": m.root, "files": files}));
        }
        "mode" => if !hub.set_mode(v["mode"].as_str().unwrap_or("")) { return Some("unknown mode".into()) } else { hub.changed() },
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
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        // the socket may sit in shared /tmp: a folder someone else made there could be watched
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(d).map(|m| m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0).unwrap_or(true) {
            eprintln!("qh serve: {} isn't a private folder of yours; refusing", d.display());
            return 1;
        }
    }
    if UnixStream::connect(&sock).is_ok() {
        // a client attaches to that one instead: same line shape as "listening"
        println!("{}", json!({"type": "running", "socket": sock, "token_file": tok_path, "protocol": PROTOCOL}));
        return 0;
    }
    let _ = std::fs::remove_file(&sock); // stale, nobody answered
    let tok = token();
    let wrote = std::fs::OpenOptions::new().write(true).create(true).truncate(true)
        .mode(0o600).open(&tok_path).and_then(|mut f| f.write_all(tok.as_bytes()));
    if let Err(e) = wrote {
        eprintln!("qh serve: can't write {}: {e}", tok_path.display());
        return 1;
    }
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => { eprintln!("qh serve: {}: {e}", sock.display()); return 1 }
    };
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    // the app reads this line to know where to connect
    println!("{}", json!({"type": "listening", "socket": sock, "token_file": tok_path, "protocol": PROTOCOL, "pid": std::process::id()}));
    let _ = std::io::stdout().flush();
    let st = Arc::new(State { clients: Mutex::new(vec![]), remote: Mutex::new(crate::ui::Remote::new(hub.clone())), confirms: Mutex::new(HashMap::new()), next: AtomicU64::new(1) });
    { let (h, s) = (hub.clone(), st.clone()); std::thread::spawn(move || pump(h, s, rx)); }
    let want = Arc::new(tok);
    for s in listener.incoming().map_while(Result::ok) {
        let (h, st, want) = (hub.clone(), st.clone(), want.clone());
        std::thread::spawn(move || client(h, st, s, want));
    }
    0
}
