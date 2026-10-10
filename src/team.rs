//! Team presence through the project's own git host (prototype; GitHub only for now). Each
//! person owns one hidden ref, `refs/qh/peers/<login>`, whose commit message is a small JSON
//! summary of their sessions. It is pushed only when that summary changes (at most every
//! 10 s) plus a keep-alive every 2 minutes; everyone's refs are read with one conditional
//! request every 3 s, which costs nothing against GitHub's limits while nothing changes.
//! The team is whoever can push to the repo: no server, accounts or invites.
use crate::backend::http;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    pub sessions: Vec<(String, String, String)>, // name, status, role
    pub files: Vec<String>,
}

struct State {
    repo: String, // owner/name
    me: String,
    peers: HashMap<String, Peer>,
    error: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(true); // the worker has written its goodbye

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    // stdin closed: git must never read the terminal the UI owns
    let o = Command::new("git").current_dir(root).args(args).env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The repo's root and its GitHub owner/name, if `origin` is on GitHub.
pub fn github_repo(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let url = git(&root, &["remote", "get-url", "origin"])?;
    // https://github.com/o/r(.git) · git@github.com:o/r(.git) · ssh://git@github.com/o/r
    let rest = url.split_once("github.com").map(|(_, r)| r.trim_start_matches([':', '/']))?;
    let slug = rest.trim_end_matches('/').trim_end_matches(".git");
    (slug.split('/').count() == 2).then(|| (root, slug.to_string()))
}

/// A GitHub token: GH_TOKEN/GITHUB_TOKEN, the gh CLI's login, or git's stored credential.
fn token() -> Option<String> {
    for v in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(v) { if !t.is_empty() { return Some(t) } }
    }
    if let Ok(o) = Command::new("gh").args(["auth", "token"]).output() {
        let t = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if o.status.success() && !t.is_empty() { return Some(t) }
    }
    let mut c = Command::new("git").args(["credential", "fill"]).env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
    {
        use std::io::Write;
        let _ = c.stdin.take()?.write_all(b"protocol=https\nhost=github.com\n\n");
    }
    let out = String::from_utf8_lossy(&c.wait_with_output().ok()?.stdout).to_string();
    out.lines().find_map(|l| l.strip_prefix("password=")).map(String::from).filter(|t| !t.is_empty())
}

fn get(path: &str, tok: &str, etag: Option<&str>) -> Result<(u16, Option<String>, Value), String> {
    let mut req = http().get(&format!("{API}{path}")).timeout(Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {tok}")).set("Accept", "application/vnd.github+json")
        .set("User-Agent", "qh");
    if let Some(e) = etag { req = req.set("If-None-Match", e) }
    match req.call() {
        Ok(r) => {
            let (code, tag) = (r.status(), r.header("etag").map(String::from));
            let v = if code == 304 { Value::Null } else { serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or(Value::Null) };
            Ok((code, tag, v))
        }
        Err(ureq::Error::Status(304, r)) => Ok((304, r.header("etag").map(String::from), Value::Null)),
        Err(ureq::Error::Status(404, _)) => Ok((404, None, Value::Null)), // no peers yet
        Err(ureq::Error::Status(c, r)) => Err(format!("GitHub HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(120).collect::<String>())),
        Err(e) => Err(e.to_string()),
    }
}

/// Push our summary as the message of an empty-tree commit to our own ref. The token travels
/// in an env-passed git config header, so it never shows up in the process list.
fn publish(root: &Path, repo: &str, me: &str, tok: &str, summary: &Value) -> Result<(), String> {
    // the empty tree (stdin is closed, so this hashes and stores nothing)
    let tree = git(root, &["hash-object", "-t", "tree", "-w", "--stdin"]).ok_or("git couldn't write the empty tree")?;
    let msg = summary.to_string();
    let commit = git(root, &["-c", "user.name=qh", "-c", "user.email=qh@localhost", "commit-tree", &tree, "-m", &msg])
        .ok_or("git couldn't create the presence commit")?;
    let basic = crate::oauth::b64url(format!("x-access-token:{tok}").as_bytes()).replace('-', "+").replace('_', "/");
    let pad = "=".repeat((4 - basic.len() % 4) % 4);
    let o = Command::new("git").current_dir(root).stdin(std::process::Stdio::null())
        .args(["push", "-q", "-f", &format!("https://github.com/{repo}.git"), &format!("{commit}:refs/qh/peers/{me}")])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {basic}{pad}"))
        .output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(160).collect()) }
}

fn parse_peer(login: &str, msg: &str) -> Peer {
    let v: Value = serde_json::from_str(msg).unwrap_or_default();
    let ts = v["ts"].as_u64().unwrap_or(0);
    Peer {
        login: login.into(),
        online: v["state"] == "online" && now().saturating_sub(ts) < OFFLINE_AFTER,
        ts,
        sessions: v["sessions"].as_array().into_iter().flatten().map(|s| (
            s["name"].as_str().unwrap_or("").into(), s["status"].as_str().unwrap_or("").into(), s["role"].as_str().unwrap_or("").into(),
        )).collect(),
        files: v["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect(),
    }
}

/// Start presence for the repo at `cwd`. `summary` describes our sessions ({sessions, files});
/// `say` reports joins, leaves and errors to the UI.
pub fn start(cwd: &Path, summary: Box<dyn Fn() -> Value + Send>, say: Box<dyn Fn(String) + Send>) -> Result<String, String> {
    if RUNNING.load(Relaxed) {
        return Err("team presence is already on".into());
    }
    let (root, repo) = github_repo(cwd).ok_or("this folder's git remote isn't on GitHub (other hosts come later)")?;
    let tok = token().ok_or("no GitHub login found: run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, user) = get("/user", &tok, None)?;
    let me = user["login"].as_str().ok_or("GitHub didn't say who you are")?.to_string();
    // testing two people on one machine: act as someone else (never used otherwise)
    let me = std::env::var("QH_TEAM_LOGIN").ok().filter(|l| !l.is_empty()).unwrap_or(me);
    *STATE.lock().unwrap() = Some(State { repo: repo.clone(), me: me.clone(), peers: HashMap::new(), error: String::new() });
    RUNNING.store(true, Relaxed);
    FINISHED.store(false, Relaxed);
    let (repo2, me2) = (repo.clone(), me.clone());
    std::thread::Builder::new().name("qh-team".into()).spawn(move || {
        let (mut etag, mut shas): (Option<String>, HashMap<String, String>) = (None, HashMap::new());
        let (mut last_body, mut last_write, mut last_poll) = (Value::Null, Instant::now() - KEEPALIVE, Instant::now() - POLL);
        while RUNNING.load(Relaxed) {
            // our own state: on change (throttled) or as a keep-alive
            let body = summary();
            if (body != last_body && last_write.elapsed() >= MIN_WRITE) || last_write.elapsed() >= KEEPALIVE {
                let mut full = body.clone();
                full["login"] = me2.clone().into();
                full["state"] = "online".into();
                full["ts"] = now().into();
                match publish(&root, &repo2, &me2, &tok, &full) {
                    Ok(()) => { last_body = body; last_write = Instant::now() }
                    Err(e) => {
                        if let Some(s) = STATE.lock().unwrap().as_mut() { s.error = e.clone() }
                        last_write = Instant::now(); // don't hammer a failing push
                    }
                }
            }
            // everyone else: one conditional request; a 304 is free
            if last_poll.elapsed() >= POLL {
                last_poll = Instant::now();
                match get(&format!("/repos/{repo2}/git/matching-refs/qh/peers/"), &tok, etag.as_deref()) {
                    Ok((200, tag, v)) => {
                        etag = tag;
                        for r in v.as_array().into_iter().flatten() {
                            let (Some(name), Some(sha)) = (r["ref"].as_str().and_then(|x| x.strip_prefix("refs/qh/peers/")), r["object"]["sha"].as_str()) else { continue };
                            if name == me2 || shas.get(name).is_some_and(|s| s == sha) { continue }
                            if let Ok((200, _, c)) = get(&format!("/repos/{repo2}/git/commits/{sha}"), &tok, None) {
                                let peer = parse_peer(name, c["message"].as_str().unwrap_or(""));
                                shas.insert(name.to_string(), sha.to_string());
                                let mut g = STATE.lock().unwrap();
                                let Some(s) = g.as_mut() else { break };
                                let was = s.peers.get(name).is_some_and(|p| p.online);
                                if peer.online && !was { say(format!("👥 {name} is online")) }
                                if !peer.online && was { say(format!("👥 {name} went offline")) }
                                s.peers.insert(name.to_string(), peer);
                                s.error.clear();
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => { if let Some(s) = STATE.lock().unwrap().as_mut() { s.error = e } }
                }
                // someone who stopped writing goes offline without needing a fetch
                if let Some(s) = STATE.lock().unwrap().as_mut() {
                    for p in s.peers.values_mut() {
                        if p.online && now().saturating_sub(p.ts) >= OFFLINE_AFTER {
                            p.online = false;
                            say(format!("👥 {} went offline", p.login));
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // leaving: say so, best effort
        let mut bye = last_body;
        if bye.is_null() { bye = json!({}) }
        bye["login"] = me2.clone().into();
        bye["state"] = "offline".into();
        bye["ts"] = now().into();
        let _ = publish(&root, &repo2, &me2, &tok, &bye);
        FINISHED.store(true, Relaxed);
    }).map_err(|e| e.to_string())?;
    remember(&repo, true);
    Ok(format!("team presence on for {repo} as {me}"))
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

/// Repos where presence was turned on, so it comes back on next start.
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

/// "👥 you · rahul ● · anu ○ 2h ago" for the status bar, while presence is on.
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
    if !s.error.is_empty() { out += " · ⚠ sync" }
    Some(out)
}

/// The `/team` panel.
pub fn report() -> String {
    let g = STATE.lock().unwrap();
    let Some(s) = g.as_ref().filter(|_| running()) else {
        return "team presence is off. `/team on` in a repo whose remote is on GitHub shares your sessions' status with \
everyone who can push to it.".into();
    };
    let mut out = format!("team {} · you are {}\n", s.repo, s.me);
    if s.peers.is_empty() {
        out += "nobody else has turned presence on in this repo yet\n";
    }
    let mut peers: Vec<&Peer> = s.peers.values().collect();
    peers.sort_by_key(|p| (!p.online, std::cmp::Reverse(p.ts)));
    for p in peers {
        out += &format!("{} {}  {}\n", if p.online { "●" } else { "○" }, p.login, if p.online { "online".into() } else { format!("last seen {}", ago(p.ts)) });
        for (n, st, role) in &p.sessions {
            out += &format!("    {n}: {st}{}\n", if role.is_empty() { String::new() } else { format!(" · {role}") });
        }
        if !p.files.is_empty() {
            out += &format!("    editing {}\n", p.files.join(", "));
        }
    }
    if !s.error.is_empty() {
        out += &format!("⚠ last sync problem: {}\n", s.error);
    }
    out + "/team off stops sharing"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_state_and_staleness() {
        let fresh = format!(r#"{{"state":"online","ts":{},"sessions":[{{"name":"api","status":"working","role":"backend"}}],"files":["src/a.rs"]}}"#, now());
        let p = parse_peer("rahul", &fresh);
        assert!(p.online && p.sessions[0].0 == "api" && p.files == ["src/a.rs"]);
        let stale = format!(r#"{{"state":"online","ts":{}}}"#, now() - 600);
        assert!(!parse_peer("rahul", &stale).online, "no write for 10 minutes = offline");
        assert!(!parse_peer("rahul", r#"{"state":"offline","ts":1}"#).online);
    }
}
