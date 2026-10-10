//! Team mode through the project's own git host (GitHub for now). The repo is a notice board:
//! each person owns one hidden ref, `refs/qh/peers/<login>`, a commit whose tree is their
//! "folder":
//!
//! - `pub`: their member public key (see teamcrypto)
//! - `keys/<login>`: the team key wrapped for each other member (only key holders write these)
//! - `data`: sealed with the team key: presence, sessions, recent transcripts, an outbox of
//!   messages to other people's sessions, and acks for messages received
//!
//! Folders are pushed only when they change (at most every 10 s) plus a keep-alive every
//! 2 minutes; everyone's refs are polled with one conditional request every 3 s (free while
//! nothing changes) and fetched with git when something did. The team is whoever can push to
//! the repo; on a public repo outsiders see only noise. A message into someone's session runs
//! on their machine, only after they allow it.
use crate::backend::http;
use crate::teamcrypto::{self as tc, Key};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://api.github.com";
const MIN_WRITE: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(3);
const OFFLINE_AFTER: u64 = 300; // seconds without a write before someone counts as offline

#[derive(Clone, Debug, Default)]
pub struct Peer {
    pub login: String,
    pub online: bool,
    pub ts: u64,
    pub locked: bool, // we can't read their data (no team key yet)
    pub sessions: Vec<(String, String, String)>, // name, status, role
    pub files: Vec<String>,
    pub transcripts: HashMap<String, Vec<(String, String)>>, // session -> (role, text)
    pub_key: String,
    wrapped: HashMap<String, String>, // keys/<login> in their folder
    outbox: Vec<Value>,
    acks: HashMap<String, u64>,
}

/// A message from a teammate into one of our sessions, waiting for our answer.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub from: String,
    pub id: u64,
    pub session: String,
    pub text: String,
}

struct State {
    repo: String,
    me: String,
    peers: HashMap<String, Peer>,
    error: String,
    key: Option<Key>,
    outbox: Vec<Value>,         // {id, to, session, text, ts}
    next_id: u64,
    seen: HashMap<String, u64>, // highest message id handled, per sender
    pending: Vec<Incoming>,
    trusted: Vec<String>,
    dirty: bool,                // something to publish now (sent message, decision)
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(true); // the worker has written its goodbye

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn git_in(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, String)]) -> Option<String> {
    let mut c = Command::new("git");
    c.current_dir(root).args(args).env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }) // never the UI's terminal
        .stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env { c.env(k, v); }
    let mut child = c.spawn().ok()?;
    if let (Some(data), Some(mut i)) = (input, child.stdin.take()) {
        let _ = i.write_all(data);
    }
    let o = child.wait_with_output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    git_in(root, args, None, &[])
}

/// The repo's root and its GitHub owner/name, if `origin` is on GitHub.
pub fn github_repo(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let url = git(&root, &["remote", "get-url", "origin"])?;
    let rest = url.split_once("github.com").map(|(_, r)| r.trim_start_matches([':', '/']))?;
    let slug = rest.trim_end_matches('/').trim_end_matches(".git");
    (slug.split('/').count() == 2).then(|| (root, slug.to_string()))
}

/// A GitHub token: GH_TOKEN/GITHUB_TOKEN, the gh CLI's login, or git's stored credential.
pub fn token() -> Option<String> {
    for v in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(v) { if !t.is_empty() { return Some(t) } }
    }
    if let Ok(o) = Command::new("gh").args(["auth", "token"]).stdin(Stdio::null()).output() {
        let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !t.is_empty() { return Some(t) }
    }
    let out = git_in(Path::new("."), &["credential", "fill"], Some(b"protocol=https\nhost=github.com\n\n"), &[])?;
    out.lines().find_map(|l| l.strip_prefix("password=")).map(String::from).filter(|t| !t.is_empty())
}

pub fn api(method: &str, path: &str, tok: &str, body: Option<&Value>, etag: Option<&str>) -> Result<(u16, Option<String>, Value), String> {
    let mut req = http().request(method, &format!("{API}{path}")).timeout(Duration::from_secs(15))
        .set("Authorization", &format!("Bearer {tok}")).set("Accept", "application/vnd.github+json").set("User-Agent", "qh");
    if let Some(e) = etag { req = req.set("If-None-Match", e) }
    let r = match body { Some(b) => req.send_string(&b.to_string()), None => req.call() };
    match r {
        Ok(r) => {
            let (code, tag) = (r.status(), r.header("etag").map(String::from));
            let v = serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or(Value::Null);
            Ok((code, tag, v))
        }
        Err(ureq::Error::Status(c @ (304 | 404), r)) => Ok((c, r.header("etag").map(String::from), Value::Null)),
        Err(ureq::Error::Status(c, r)) => Err(format!("GitHub HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(160).collect::<String>())),
        Err(e) => Err(e.to_string()),
    }
}

/// git's auth for GitHub over HTTPS, passed through env so the token never shows in `ps`.
pub fn auth_env(tok: &str) -> Vec<(&'static str, String)> {
    let b = crate::oauth::b64url(format!("x-access-token:{tok}").as_bytes()).replace('-', "+").replace('_', "/");
    let pad = "=".repeat((4 - b.len() % 4) % 4);
    vec![("GIT_CONFIG_COUNT", "1".into()), ("GIT_CONFIG_KEY_0", "http.extraHeader".into()),
         ("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {b}{pad}"))]
}

fn blob(root: &Path, data: &str) -> Option<String> {
    git_in(root, &["hash-object", "-w", "--stdin"], Some(data.as_bytes()), &[])
}

/// Build my folder as a tree and push it as a history-less commit to my ref.
fn publish(root: &Path, repo: &str, me: &str, tok: &str, files: &[(String, String)], message: &Value) -> Result<(), String> {
    let (mut top, mut keys) = (String::new(), String::new());
    for (name, content) in files {
        let sha = blob(root, content).ok_or("git couldn't store a board file")?;
        match name.strip_prefix("keys/") {
            Some(k) => keys += &format!("100644 blob {sha}\t{k}\n"),
            None => top += &format!("100644 blob {sha}\t{name}\n"),
        }
    }
    if !keys.is_empty() {
        let t = git_in(root, &["mktree"], Some(keys.as_bytes()), &[]).ok_or("git couldn't build keys/")?;
        top += &format!("040000 tree {t}\tkeys\n");
    }
    let tree = git_in(root, &["mktree"], Some(top.as_bytes()), &[]).ok_or("git couldn't build the folder")?;
    let commit = git(root, &["-c", "user.name=qh", "-c", "user.email=qh@localhost", "commit-tree", &tree, "-m", &message.to_string()])
        .ok_or("git couldn't create the board commit")?;
    git_in(root, &["push", "-q", "-f", &format!("https://github.com/{repo}.git"), &format!("{commit}:refs/qh/peers/{me}")], None, &auth_env(tok))
        .map(|_| ()).ok_or_else(|| "push to the board failed".into())
}

/// Everyone's folders into refs/qh/board/*, listed as (login, commit).
fn fetch_board(root: &Path, repo: &str, tok: &str) -> Result<Vec<(String, String)>, String> {
    git_in(root, &["fetch", "-q", "--no-tags", "--prune", &format!("https://github.com/{repo}.git"), "+refs/qh/peers/*:refs/qh/board/*"],
           None, &auth_env(tok)).ok_or("fetching the board failed")?;
    let list = git(root, &["for-each-ref", "--format=%(refname:strip=3) %(objectname)", "refs/qh/board/"]).unwrap_or_default();
    Ok(list.lines().filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string()))).collect())
}

fn read_folder(root: &Path, sha: &str, login: &str, key: Option<&Key>) -> Peer {
    let msg: Value = serde_json::from_str(&git(root, &["log", "-1", "--format=%B", sha]).unwrap_or_default()).unwrap_or_default();
    let ts = msg["ts"].as_u64().unwrap_or(0);
    let mut p = Peer {
        login: login.into(), ts,
        online: msg["state"] == "online" && now().saturating_sub(ts) < OFFLINE_AFTER,
        pub_key: git(root, &["show", &format!("{sha}:pub")]).unwrap_or_default(),
        ..Default::default()
    };
    for name in git(root, &["ls-tree", "--name-only", &format!("{sha}:keys")]).unwrap_or_default().lines() {
        if let Some(w) = git(root, &["show", &format!("{sha}:keys/{name}")]) {
            p.wrapped.insert(name.to_string(), w);
        }
    }
    let sealed = git(root, &["show", &format!("{sha}:data")]).unwrap_or_default();
    let data: Option<Value> = key.and_then(|k| tc::open(k, &format!("data:{login}"), &sealed)).and_then(|b| serde_json::from_slice(&b).ok());
    match data {
        None => p.locked = !sealed.is_empty(),
        Some(d) => {
            p.sessions = d["sessions"].as_array().into_iter().flatten().map(|s| (
                s["name"].as_str().unwrap_or("").into(), s["status"].as_str().unwrap_or("").into(), s["role"].as_str().unwrap_or("").into(),
            )).collect();
            p.files = d["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
            if let Some(t) = d["transcripts"].as_object() {
                for (s, msgs) in t {
                    p.transcripts.insert(s.clone(), msgs.as_array().into_iter().flatten().map(|m| (
                        m[0].as_str().unwrap_or("").to_string(), m[1].as_str().unwrap_or("").to_string())).collect());
                }
            }
            p.outbox = d["outbox"].as_array().cloned().unwrap_or_default();
            if let Some(a) = d["acks"].as_object() {
                p.acks = a.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0))).collect();
            }
        }
    }
    p
}

fn per_repo(dir: &str, repo: &str) -> PathBuf {
    crate::config::state_dir().join(dir).join(repo.replace('/', "__"))
}

fn load_key(repo: &str) -> Option<Key> {
    let s = std::fs::read_to_string(per_repo("team-keys", repo)).ok()?;
    crate::oauth::b64url_decode(s.trim()).try_into().ok()
}

fn save_key(repo: &str, k: &Key) {
    let p = per_repo("team-keys", repo);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = crate::platform::write_private(&p, crate::oauth::b64url(k).as_bytes());
}

/// What our own sessions look like, supplied by the UI: {sessions:[{name,status,role}], files:[], transcripts:{name:[[role,text]]}}.
pub type Summary = Box<dyn Fn() -> Value + Send>;

/// Callbacks into the UI: say a line, deliver a message to a session, ask whether to allow one.
pub struct Hooks {
    pub say: Box<dyn Fn(String) + Send>,
    pub deliver: Box<dyn Fn(&str, &str) + Send>,
    pub ask: Box<dyn Fn(&Incoming) + Send>,
    pub refresh: Box<dyn Fn() + Send>, // redraw: the status line changed
}

pub fn start(cwd: &Path, summary: Summary, hooks: Hooks) -> Result<String, String> {
    if RUNNING.load(Relaxed) {
        return Err("team mode is already on".into());
    }
    let (root, repo) = github_repo(cwd).ok_or("this folder's git remote isn't on GitHub (other hosts come later)")?;
    let tok = token().ok_or("no GitHub login found: run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, user) = api("GET", "/user", &tok, None, None)?;
    let me = user["login"].as_str().ok_or("GitHub didn't say who you are")?.to_string();
    let me = std::env::var("QH_TEAM_LOGIN").ok().filter(|l| !l.is_empty()).unwrap_or(me); // testing two people on one machine
    let (secret, my_pub) = tc::identity();
    let seen: HashMap<String, u64> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-seen", &repo)).unwrap_or_default()).unwrap_or_default();
    *STATE.lock().unwrap() = Some(State {
        repo: repo.clone(), me: me.clone(), peers: HashMap::new(), error: String::new(), key: load_key(&repo),
        outbox: vec![], next_id: now() * 1000, seen, pending: vec![], trusted: vec![], dirty: true,
    });
    RUNNING.store(true, Relaxed);
    FINISHED.store(false, Relaxed);
    let (repo2, me2) = (repo.clone(), me.clone());
    std::thread::Builder::new().name("qh-team".into()).spawn(move || {
        let mut etag: Option<String> = None;
        let mut shas: HashMap<String, String> = HashMap::new();
        let (mut last_files, mut last_write, mut last_poll) = (vec![], Instant::now() - KEEPALIVE, Instant::now() - POLL);
        let mut fetched = false;
        let build = |state_word: &str| -> (Vec<(String, String)>, Value) {
            let mut files = vec![("pub".to_string(), my_pub.clone())];
            let d = summary(); // outside the lock: it reads the hub
            let g = STATE.lock().unwrap();
            if let Some(s) = g.as_ref() {
                if let Some(k) = &s.key {
                    let mut wraps: Vec<(String, String)> = s.peers.values().filter(|p| !p.pub_key.is_empty())
                        .filter_map(|p| Some((format!("keys/{}", p.login), tc::wrap(k, &secret, &p.pub_key, &s.repo)?))).collect();
                    wraps.sort();
                    files.extend(wraps);
                    let mut d = d;
                    d["outbox"] = Value::from(s.outbox.clone());
                    d["acks"] = json!(s.seen);
                    files.push(("data".into(), tc::seal(k, &format!("data:{}", s.me), d.to_string().as_bytes())));
                }
            }
            (files, json!({"v": 2, "state": state_word, "ts": now()}))
        };
        while RUNNING.load(Relaxed) {
            // read the board: one conditional request, a git fetch only when something changed
            if last_poll.elapsed() >= POLL {
                last_poll = Instant::now();
                match api("GET", &format!("/repos/{repo2}/git/matching-refs/qh/peers/"), &tok, None, etag.as_deref()) {
                    Ok((200, tag, v)) => {
                        let changed = v.as_array().into_iter().flatten().any(|r| {
                            let name = r["ref"].as_str().unwrap_or("").trim_start_matches("refs/qh/peers/");
                            name != me2 && shas.get(name).map(|s| s.as_str()) != r["object"]["sha"].as_str()
                        });
                        etag = tag;
                        if changed || !fetched {
                            match fetch_board(&root, &repo2, &tok) {
                                Ok(list) => { fetched = true; absorb(&root, &list, &mut shas, &secret, &hooks); (hooks.refresh)() }
                                Err(e) => set_error(e),
                            }
                        }
                    }
                    Ok((404, _, _)) => { fetched = true; settle_key() } // an empty board
                    Ok(_) => {}
                    Err(e) => set_error(e),
                }
                stale_check(&hooks);
            }
            // write my folder: when it changed (throttled), when asked (dirty), or as a keep-alive.
            // Wrapped keys are fresh nonces each time, so compare without them.
            let (files, msg) = build("online");
            let dirty = STATE.lock().unwrap().as_ref().is_some_and(|s| s.dirty);
            let plain = |f: &[(String, String)]| -> Vec<String> { f.iter().map(|(n, c)| if n == "data" || n.starts_with("keys/") { n.clone() } else { format!("{n}={c}") }).collect() };
            let summary_now = STATE.lock().unwrap().as_ref().and_then(|s| s.key.map(|_| ())).is_some();
            let changed = plain(&files) != plain(&last_files) || (summary_now && data_changed(&files, &last_files));
            if fetched && (((changed || dirty) && last_write.elapsed() >= MIN_WRITE) || last_write.elapsed() >= KEEPALIVE) {
                match publish(&root, &repo2, &me2, &tok, &files, &msg) {
                    Ok(()) => {
                        last_files = files;
                        if let Some(s) = STATE.lock().unwrap().as_mut() { s.dirty = false; s.error.clear() }
                    }
                    Err(e) => set_error(e),
                }
                last_write = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let (files, msg) = build("offline");
        let _ = publish(&root, &repo2, &me2, &tok, &files, &msg);
        FINISHED.store(true, Relaxed);
    }).map_err(|e| e.to_string())?;
    remember(&repo, true);
    Ok(format!("team mode on for {repo} as {me} (shared data is encrypted for the team)"))
}

/// Whether the sealed `data` says something different (it's re-sealed with a new nonce every
/// build, so compare what it decrypts to).
fn data_changed(a: &[(String, String)], b: &[(String, String)]) -> bool {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref() else { return false };
    let Some(k) = s.key else { return false };
    let get = |f: &[(String, String)]| f.iter().find(|(n, _)| n == "data").and_then(|(_, c)| tc::open(&k, &format!("data:{}", s.me), c));
    get(a) != get(b)
}

fn set_error(e: String) {
    if let Some(s) = STATE.lock().unwrap().as_mut() { s.error = e }
}

/// Take in freshly fetched folders: our key if someone wrapped it for us, presence,
/// transcripts, and messages addressed to our sessions.
fn absorb(root: &Path, list: &[(String, String)], shas: &mut HashMap<String, String>,
          secret: &x25519_dalek::StaticSecret, hooks: &Hooks) {
    let (me, repo, mut key) = {
        let g = STATE.lock().unwrap();
        let Some(s) = g.as_ref() else { return };
        (s.me.clone(), s.repo.clone(), s.key)
    };
    if key.is_none() {
        for (login, sha) in list.iter().filter(|(l, _)| *l != me) {
            let p = read_folder(root, sha, login, None);
            if let Some(k) = p.wrapped.get(&me).and_then(|w| tc::unwrap(w, secret, &p.pub_key, &repo)) {
                save_key(&repo, &k);
                key = Some(k);
                (hooks.say)(format!("👥 {login} let you into the team: you can see everyone's sessions now"));
                break;
            }
        }
    }
    for (login, sha) in list.iter().filter(|(l, _)| *l != me) {
        shas.insert(login.clone(), sha.clone());
        let p = read_folder(root, sha, login, key.as_ref());
        let mut g = STATE.lock().unwrap();
        let Some(s) = g.as_mut() else { return };
        s.key = key;
        let was = s.peers.get(login).is_some_and(|x| x.online);
        if p.online && !was { (hooks.say)(format!("👥 {login} is online")) }
        if !p.online && was { (hooks.say)(format!("👥 {login} went offline")) }
        if !s.peers.contains_key(login) && !p.pub_key.is_empty() { s.dirty = true } // a new member to wrap the key for
        // their acks let us drop what they already handled
        let acked = p.acks.get(&me).copied().unwrap_or(0);
        let before = s.outbox.len();
        s.outbox.retain(|m| !(m["to"] == login.as_str() && m["id"].as_u64().unwrap_or(0) <= acked));
        if s.outbox.len() != before { s.dirty = true }
        for m in p.outbox.iter().filter(|m| m["to"] == me.as_str()) {
            let id = m["id"].as_u64().unwrap_or(0);
            if id <= s.seen.get(login).copied().unwrap_or(0) || s.pending.iter().any(|x| x.from == *login && x.id == id) {
                continue;
            }
            let inc = Incoming { from: login.clone(), id, session: m["session"].as_str().unwrap_or("main").into(),
                                 text: m["text"].as_str().unwrap_or("").into() };
            if s.trusted.contains(login) {
                (hooks.deliver)(&inc.session, &format!("[from {} via team] {}", inc.from, inc.text));
                s.seen.insert(login.clone(), id);
                s.dirty = true;
            } else {
                (hooks.ask)(&inc);
                s.pending.push(inc);
            }
        }
        s.peers.insert(login.clone(), p);
    }
    save_seen();
    settle_key();
}

/// Nobody on the board holds a team key yet: the first member creates it. If someone already
/// did (their data is sealed, or they wrap keys), wait for them to wrap a copy for us.
fn settle_key() {
    let mut g = STATE.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    if s.key.is_some() {
        return;
    }
    if !s.peers.values().any(|p| !p.wrapped.is_empty() || p.locked) {
        let k = tc::new_team_key();
        save_key(&s.repo, &k);
        s.key = Some(k);
        s.dirty = true;
    }
}

fn stale_check(hooks: &Hooks) {
    if let Some(s) = STATE.lock().unwrap().as_mut() {
        for p in s.peers.values_mut() {
            if p.online && now().saturating_sub(p.ts) >= OFFLINE_AFTER {
                p.online = false;
                (hooks.say)(format!("👥 {} went offline", p.login));
            }
        }
    }
}

fn save_seen() {
    if let Some(s) = STATE.lock().unwrap().as_ref() {
        let p = per_repo("team-seen", &s.repo);
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        let _ = std::fs::write(&p, json!(s.seen).to_string());
    }
}

/// Queue a message into a teammate's session; it travels with our next write.
pub fn send(to: &str, session: &str, text: &str) -> Result<String, String> {
    let mut g = STATE.lock().unwrap();
    let s = g.as_mut().filter(|_| running()).ok_or("team mode is off: /team on")?;
    if s.key.is_none() {
        return Err("you're not in the team yet: waiting for a teammate to let you in".into());
    }
    let p = s.peers.get(to).ok_or_else(|| format!("no teammate {to}; /team lists them"))?;
    if !p.sessions.iter().any(|(n, _, _)| n == session) {
        return Err(format!("{to} has no session {session}"));
    }
    let online = p.online;
    s.next_id += 1;
    let id = s.next_id;
    s.outbox.push(json!({"id": id, "to": to, "session": session, "text": text, "ts": now()}));
    s.dirty = true;
    Ok(format!("sent to {to} / {session}: it runs on their machine once they allow it{}",
               if online { "" } else { " (they're offline, so it waits for them)" }))
}

/// Our answer to a waiting message: "once", "deny" or "always" (trust that person from now on).
/// Returns (session, text) to deliver unless denied.
pub fn decide(from: &str, id: u64, answer: &str) -> Option<(String, String)> {
    let mut g = STATE.lock().unwrap();
    let s = g.as_mut()?;
    let i = s.pending.iter().position(|x| x.from == from && x.id == id)?;
    let inc = s.pending.remove(i);
    let prev = s.seen.get(from).copied().unwrap_or(0);
    s.seen.insert(from.to_string(), id.max(prev));
    s.dirty = true;
    if answer == "always" && !s.trusted.contains(&inc.from) {
        s.trusted.push(inc.from.clone());
    }
    drop(g);
    save_seen();
    (answer != "deny").then(|| (inc.session.clone(), format!("[from {} via team] {}", inc.from, inc.text)))
}

pub fn pending() -> Vec<Incoming> {
    STATE.lock().unwrap().as_ref().map(|s| s.pending.clone()).unwrap_or_default()
}

/// Teammates' sessions as (login, session, label), online people first.
pub fn sessions() -> Vec<(String, String, String)> {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref() else { return vec![] };
    let mut peers: Vec<&Peer> = s.peers.values().collect();
    peers.sort_by_key(|p| (!p.online, p.login.clone()));
    peers.iter().flat_map(|p| p.sessions.iter().map(|(n, st, role)| (
        p.login.clone(), n.clone(),
        format!("{n}  {st}{}{}", if role.is_empty() { String::new() } else { format!(" · {role}") }, if p.online { "" } else { "  (offline)" }),
    ))).collect()
}

pub fn transcript(login: &str, session: &str) -> Result<String, String> {
    let g = STATE.lock().unwrap();
    let s = g.as_ref().ok_or("team mode is off")?;
    let p = s.peers.get(login).ok_or_else(|| format!("no teammate {login}"))?;
    let t = p.transcripts.get(session).filter(|t| !t.is_empty()).ok_or_else(|| format!("no messages in {login} / {session} yet"))?;
    let body: Vec<String> = t.iter().map(|(r, x)| format!("{} {x}", if r == "user" { "▸" } else { "◂" })).collect();
    Ok(format!("{login} / {session} · last {} messages · read-only (/team send {login} {session} <text> to write into it)\n{}",
               t.len(), body.join("\n")))
}

pub fn stop() {
    RUNNING.store(false, Relaxed);
}

/// Stop and wait (up to 5 s) for the "offline" write to land, at quit.
pub fn stop_and_flush() {
    if RUNNING.swap(false, Relaxed) {
        let t0 = Instant::now();
        while !FINISHED.load(Relaxed) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn running() -> bool {
    RUNNING.load(Relaxed)
}

fn enabled_path() -> PathBuf {
    crate::config::state_dir().join("team-repos.txt")
}

/// Repos where team mode was turned on, so it comes back on next start.
pub fn remember(repo: &str, on: bool) {
    let mut v: Vec<String> = std::fs::read_to_string(enabled_path()).unwrap_or_default().lines().map(String::from).collect();
    v.retain(|r| r != repo);
    if on { v.push(repo.into()) }
    let _ = std::fs::create_dir_all(crate::config::state_dir());
    let _ = std::fs::write(enabled_path(), v.join("\n"));
}

pub fn enabled_for(cwd: &Path) -> bool {
    github_repo(cwd).is_some_and(|(_, r)| std::fs::read_to_string(enabled_path()).unwrap_or_default().lines().any(|l| l == r))
}

fn ago(ts: u64) -> String {
    let d = now().saturating_sub(ts);
    match d { 0..60 => "just now".into(), 60..3600 => format!("{}m ago", d / 60), 3600..86400 => format!("{}h ago", d / 3600), _ => format!("{}d ago", d / 86400) }
}

/// "👥 you · rahul ● · anu ○ 2h ago" for the status bar, while team mode is on.
pub fn status_line() -> Option<String> {
    let g = STATE.lock().unwrap();
    let s = g.as_ref().filter(|_| running())?;
    let mut peers: Vec<&Peer> = s.peers.values().collect();
    peers.sort_by_key(|p| (!p.online, std::cmp::Reverse(p.ts)));
    let mut out = format!("👥 {}", s.me);
    for p in peers.iter().take(4) {
        out += &if p.online { format!(" · {} ●", p.login) } else { format!(" · {} ○ {}", p.login, ago(p.ts)) };
    }
    if peers.len() > 4 { out += &format!(" · +{}", peers.len() - 4) }
    if s.key.is_none() { out += " · 🔒 waiting to be let in" }
    if !s.pending.is_empty() { out += &format!(" · ✉ {} waiting (/team)", s.pending.len()) }
    if !s.error.is_empty() { out += " · ⚠ sync" }
    Some(out)
}

/// The `/team` text panel.
pub fn report() -> String {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref().filter(|_| running()) else {
        return "team mode is off.\n\
  /team on      in a project that's already on GitHub: see and message everyone who can push to it\n\
  /team create  put this folder on GitHub (private) and invite friends by GitHub username\n\
Friends join with:  qh join <owner>/<repo>".into();
    };
    let mut out = format!("team {} · you are {}\n", s.repo, s.me);
    if s.key.is_none() {
        out += "🔒 waiting for a teammate who's already in to come online and let you in\n";
    }
    if s.peers.is_empty() {
        out += "nobody else has turned team mode on in this repo yet\n";
    }
    let mut peers: Vec<&Peer> = s.peers.values().collect();
    peers.sort_by_key(|p| (!p.online, std::cmp::Reverse(p.ts)));
    for p in peers {
        out += &format!("{} {}  {}\n", if p.online { "●" } else { "○" }, p.login, if p.online { "online".into() } else { format!("last seen {}", ago(p.ts)) });
        if p.locked {
            out += "    (encrypted: you don't have the team key yet)\n";
        }
        for (n, st, role) in &p.sessions {
            out += &format!("    {n}: {st}{}\n", if role.is_empty() { String::new() } else { format!(" · {role}") });
        }
        if !p.files.is_empty() {
            out += &format!("    editing {}\n", p.files.join(", "));
        }
    }
    for i in &s.pending {
        out += &format!("✉ {} → your {}: {}\n", i.from, i.session, i.text);
    }
    if !s.error.is_empty() {
        out += &format!("⚠ last sync problem: {}\n", s.error);
    }
    out + "/team open · /team send <who> <session> <text> · /team off"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_is_standard_base64() {
        assert_eq!(auth_env("abc")[2].1, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46YWJj");
    }
}
