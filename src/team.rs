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
//!
//! Removing someone (`/team remove`, repo admins only) makes a new team key, one `epoch` higher,
//! wrapped for everyone but them, and lists them in `removed`; it also takes away their access to
//! the repo. Members switch to the new key when they see it from someone GitHub says is an admin,
//! so whatever is written after that is noise to the removed person.
//!
//! A newcomer is let in (sent the team key) only once a member approves them: everyone you
//! invited is approved up front, others ask. An invite also leaves the key sealed with a one-off
//! code in `invites/<login>`, so the newcomer gets in with that code even when nobody is online.
//! Several projects can be connected at once; the UI shows one at a time (`set_current`).
//!
//! Git is slow (seconds) but durable. Teammates who are online also hold a direct line (see
//! link.rs): the same sealed `data`, minus transcripts, goes over it the moment it changes, and
//! is taken in exactly like a fetched folder. Everything still goes through git as well, so a
//! dropped line loses nothing; it only gets slower.
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
const MIN_LINK_WRITE: Duration = Duration::from_secs(1); // floor for the address-change exemption
const KEEPALIVE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(3);
const OFFLINE_AFTER: u64 = 300; // seconds without a write before someone counts as offline
const CLAIM_HOT: u64 = 600;     // a function someone edited this recently is theirs: others pause

/// Who changed which functions, per session: file -> function -> when (unix seconds).
pub type Claims = HashMap<String, HashMap<String, u64>>;

/// A session paused because a teammate is changing the function it wanted to edit.
#[derive(Clone, Debug, Default)]
pub struct Wait {
    pub session: String,
    pub file: String,
    pub func: String,
    pub on: String, // the teammate's login
    pub since: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Peer {
    pub login: String,
    pub online: bool,
    pub ts: u64,
    pub locked: bool, // they have data we can't read (we have no team key yet)
    pub joined: bool, // their data opened with our key: they're in the team
    pub sessions: Vec<(String, String, String)>, // name, status, role
    pub files: Vec<String>,
    pub transcripts: HashMap<String, Vec<(String, String)>>, // session -> (role, text)
    pub_key: String,
    wrapped: HashMap<String, String>, // keys/<login> in their folder
    outbox: Vec<Value>,
    acks: HashMap<String, u64>,
    roles: HashMap<String, String>, // the roles they gave people, so we know ours with them
    details: HashMap<String, SessionView>, // session -> topic, now, files, branch
    invites: HashMap<String, String>,       // invites/<login>: the team key sealed with an invite code
    epoch: u64,                             // which team key generation their folder is on
    removed: Vec<String>,                   // who they say was removed from the team
    chat: Vec<(u64, u64, String)>,          // their team chat lines: id, ts, text
    dms: Vec<(u64, u64, String)>,           // their direct messages: id, ts, box (sealed for one person; not even the team key opens it)
    groups: Vec<(String, Vec<String>)>,     // groups they made: id, the group sealed for each member
    gmsgs: Vec<(u64, u64, String, String)>, // their group lines: id, ts, group id, box (sealed with that group's key)
    notes: Vec<(u64, String, String, Vec<String>)>, // their change notes: id, text, file, functions/names changed
    pub claims: HashMap<String, Claims>,    // session -> what it changed lately
    pub uses: HashMap<String, HashMap<String, u64>>, // session -> names its fresh code uses -> when
    pub waiting: Vec<Wait>,                 // their sessions paused on someone
    sheets: HashMap<String, Sheet>,         // session -> what it decided and does next
    promises: Vec<Promise>,                 // what their sessions count on staying true
    link: Vec<String>,                      // where their qh-link listens ("lan=1.2.3.4:5"), sealed like the rest
    link_ms: u64,                           // when those addresses were current (their clock, ms)
    seq: u64,                               // when this copy was made (ms): an older git copy can't undo a live one
    sign: String,                           // their signing key (Ed25519), named in the folder
    anchor: String,                         // the commit GitHub vouches for their keys on
    signed: bool,                           // the folder's signature checks out against `sign`
    qh: String,                             // the qh version they run (empty before 0.1.4)
}

/// A member's keys as we know them: (member key, signing key, GitHub confirmed them).
type Pin = (String, String, bool);

/// The model-written part of a session's fact sheet; the rest comes from claims and uses.
#[derive(Clone, Debug, Default)]
pub struct Sheet {
    pub decided: String,
    pub next: String,
    pub ts: u64,
}

/// A message from a teammate into one of our sessions, waiting for our answer: a request for
/// work (only ever delivered when we accept it) or, with /answers ask, a question.
#[derive(Clone, Debug, Default)]
pub struct Incoming {
    pub from: String,
    pub id: u64,
    pub session: String,
    pub text: String,
    pub question: Option<Question>, // a question for our agent, answered from a side copy
    pub ts: u64,
    pub from_session: String, // the session of theirs that sent it ("" = the person)
    pub replied: bool,        // we wrote back but haven't accepted or declined yet
}

/// A private group: only its members hold its key.
#[derive(Clone, Debug)]
struct Group {
    name: String,
    members: Vec<String>, // logins, the owner included
    owner: String,
    key: Key,
}

/// One line in a direct-message or group channel.
#[derive(Clone, Debug, PartialEq)]
struct Msg {
    ts: u64,
    chan: String, // "@login" (a DM with them) or "+<group id>"
    from: String,
    text: String,
    id: u64,      // the sender's id for it
}

/// A teammate's agent asking one of our sessions something.
#[derive(Clone, Debug)]
pub struct Question {
    pub repo: String,
    pub from: String,         // their login
    pub from_session: String, // their session, where the answer goes ("" = the person asked)
    pub id: u64,
    pub session: String,      // our session that's asked
    pub text: String,
}

/// A behaviour contract: "whoever changes `name` keeps this true", held by the session that
/// relies on it. Checked by a model when someone edits that function (opt-in: made by /promise).
#[derive(Clone, Debug, Default)]
pub struct Promise {
    pub session: String, // our session (or theirs, on a peer) that counts on it
    pub name: String,    // the function
    pub text: String,    // what must stay true
    pub ts: u64,
}

/// One of our questions to a teammate's agent, until it's answered.
#[derive(Clone, Debug)]
struct Asked {
    id: u64,
    session: String,
    to: String,
    q: String,
    ts: u64,
}

const ANSWERS_PER_HOUR: usize = 20; // questions our agents answer for teammates, at our cost

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
    roles: HashMap<String, String>, // login -> viewer | trusted (member when absent); our own rules for our sessions
    approved: Vec<String>,          // people we'll let in without asking (invited, or let in before)
    asking: Vec<String>,            // newcomers waiting for "let in"
    invites: HashMap<String, String>, // login -> sealed key we publish for them
    epoch: u64,                       // our team key's generation (0: never changed)
    removed: Vec<String>,             // people removed from the team: never given the key again
    admins: HashMap<String, bool>,    // repo admins, as GitHub told us (who may change the key)
    pins: HashMap<String, Pin>,       // login -> the keys their folders must be signed with
    gh_login: String,                 // who GitHub says we are (differs from `me` only in tests)
    git_ms: HashMap<String, u64>,     // newest folder taken from git, per person: older ones are replays
    asked_github: HashMap<String, Instant>, // "login sign" -> when we last asked GitHub to vouch for it
    warned: Vec<String>,              // warnings already shown this run
    my_sha: String,                   // the folder commit we pushed last
    chat: Vec<(u64, String, String)>, // ts, from, text: the team's chat, oldest first
    my_chat: Vec<Value>,            // my last chat lines, published: {id, ts, text}
    chat_seen: HashMap<String, u64>, // highest chat id taken in, per person
    my_notes: Vec<Value>,           // change notes for teammates' agents, published: {id, ts, file, text}
    notes_seen: HashMap<String, u64>, // highest note id taken in, per person
    claims: HashMap<String, Claims>,  // our sessions' recent changes, published
    uses: HashMap<String, HashMap<String, u64>>, // names our sessions' fresh code uses, published
    waiting: Vec<Wait>,               // our sessions paused on a teammate
    sheets: HashMap<String, Sheet>,   // our sessions' decided/next lines, published
    asked: Vec<Asked>,                // our questions still out
    answered: Vec<u64>,               // when we answered teammates' questions (the budget)
    answers: String,                  // auto | ask | off: whether teammates' agents may ask ours
    answerer: Option<std::sync::Arc<dyn Fn(Question) + Send + Sync>>,
    promises: Vec<Promise>,           // what our sessions count on teammates keeping true, published
    last_answer: HashMap<String, (String, Vec<String>, String)>, // session -> (who, names, answer): /promise turns it into promises
    unread: usize,
    secret: Option<x25519_dalek::StaticSecret>, // our member key: opens what teammates sealed for us alone
    my_dms: Vec<Value>,             // direct messages, published: {id, ts, box}
    my_groups: Vec<Value>,          // groups we made, published: {gid, boxes}
    my_gmsgs: Vec<Value>,           // our group lines, published: {id, ts, g, box}
    groups: HashMap<String, Group>, // group id -> group, for groups we're in
    msgs: Vec<Msg>,                 // direct-message and group lines, oldest first
    msg_ids: std::collections::HashSet<(String, u64)>, // (sender, id) boxes already tried
    chan_unread: HashMap<String, usize>, // DM / group channel -> unread lines
    pinged: Vec<String>,            // channels with an unread @mention of us
    dirty: bool,                    // something to publish now (sent message, decision)
    running: std::sync::Arc<AtomicBool>,
    finished: std::sync::Arc<AtomicBool>,
}

static STATES: Mutex<Option<HashMap<String, State>>> = Mutex::new(None);
static CURRENT: Mutex<String> = Mutex::new(String::new());

/// Run `f` on one project's state.
fn with<R>(repo: &str, f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATES.lock().unwrap().get_or_insert_with(HashMap::new).get_mut(repo).map(f)
}

/// Run `f` on the project the UI is showing.
fn cur<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let repo = CURRENT.lock().unwrap().clone();
    with(&repo, |s| if s.running.load(Relaxed) { Some(f(s)) } else { None }).flatten()
}

/// Which connected project the UI shows (team room, status bar, /team).
pub fn set_current(repo: &str) {
    *CURRENT.lock().unwrap() = repo.to_string();
}

pub fn running_for(repo: &str) -> bool {
    with(repo, |s| s.running.load(Relaxed)).unwrap_or(false)
}

pub fn running() -> bool {
    cur(|_| ()).is_some()
}

pub fn repo() -> Option<String> {
    cur(|s| s.repo.clone())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn git_in(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, String)]) -> Option<String> {
    git_raw(root, args, input, env).map(|o| String::from_utf8_lossy(&o).trim_end().to_string())
}

fn git_raw(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, String)]) -> Option<Vec<u8>> {
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
    o.status.success().then_some(o.stdout)
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

/// Build my folder as a tree and push it to my ref, as a commit whose only parent is my identity
/// commit (so that stays on the board). Names may have one folder level ("keys/bob").
fn publish(root: &Path, repo: &str, me: &str, tok: &str, files: &[(String, String)], message: &Value, parent: &str) -> Result<String, String> {
    let commit = folder_commit(root, files, message, parent)?;
    git_in(root, &["push", "-q", "-f", &format!("https://github.com/{repo}.git"), &format!("{commit}:refs/qh/peers/{me}")], None, &auth_env(tok))
        .map(|_| commit).ok_or_else(|| "push to the board failed".into())
}

fn folder_commit(root: &Path, files: &[(String, String)], message: &Value, parent: &str) -> Result<String, String> {
    let mut top = String::new();
    let mut dirs: std::collections::BTreeMap<String, String> = Default::default();
    for (name, content) in files {
        let sha = blob(root, content).ok_or("git couldn't store a board file")?;
        match name.split_once('/') {
            Some((d, f)) => *dirs.entry(d.to_string()).or_default() += &format!("100644 blob {sha}\t{f}\n"),
            None => top += &format!("100644 blob {sha}\t{name}\n"),
        }
    }
    for (d, list) in dirs {
        let t = git_in(root, &["mktree"], Some(list.as_bytes()), &[]).ok_or("git couldn't build a board folder")?;
        top += &format!("040000 tree {t}\t{d}\n");
    }
    let tree = git_in(root, &["mktree"], Some(top.as_bytes()), &[]).ok_or("git couldn't build the folder")?;
    git(root, &["-c", "user.name=qh", "-c", "user.email=qh@localhost", "commit-tree", &tree, "-p", parent, "-m", &message.to_string()])
        .ok_or_else(|| "git couldn't create the board commit".into())
}

/// The commit GitHub vouches for our keys on (see `vouch`): a tree of our member key and signing
/// key, with fixed dates so the same keys always give the same commit.
fn identity_commit(root: &Path, me: &str, my_pub: &str, sign_pub: &str) -> Option<String> {
    let list = format!("100644 blob {}\tpub\n100644 blob {}\tsign\n", blob(root, my_pub)?, blob(root, sign_pub)?);
    let tree = git_in(root, &["mktree"], Some(list.as_bytes()), &[])?;
    let date = "1700000000 +0000".to_string();
    git_in(root, &["-c", "user.name=qh", "-c", "user.email=qh@localhost", "commit-tree", &tree, "-m", &format!("qh identity {me}")],
           None, &[("GIT_AUTHOR_DATE", date.clone()), ("GIT_COMMITTER_DATE", date)])
}

/// Git doesn't record who pushed a ref, but GitHub does record who posted a commit status. So
/// once per key and repo we post a "qh/identity" status naming our keys on our identity commit;
/// a teammate meeting us checks that GitHub says *we* posted it (`vouched`).
fn vouch(repo: &str, tok: &str, idc: &str, sign_pub: &str, my_pub: &str) {
    let done = per_repo("team-vouched", repo);
    if std::fs::read_to_string(&done).is_ok_and(|d| d.trim() == idc) {
        return;
    }
    let body = json!({"state": "success", "context": "qh/identity", "description": format!("{sign_pub} {my_pub}")});
    match api("POST", &format!("/repos/{repo}/statuses/{idc}"), tok, Some(&body), None) {
        Ok((201, _, _)) => {
            let _ = std::fs::create_dir_all(done.parent().unwrap_or(&done));
            let _ = std::fs::write(&done, idc);
        }
        r => {
            let why = match r { Ok((c, _, _)) => format!("GitHub answered {c}"), Err(e) => e };
            warn_once(repo, None, "vouch", format!("couldn't get GitHub to vouch for your member key ({why}): teammates meeting you for the first time will see you as unconfirmed"));
        }
    }
}

/// Whether GitHub vouches that `p`'s keys are really `p.login`'s (see `vouch`). Asked at most
/// every ten minutes per login and key.
fn vouched(repo: &str, tok: &str, p: &Peer) -> bool {
    if p.anchor.len() != 40 || !p.anchor.bytes().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let tag = format!("{} {}", p.login, p.sign);
    let Some((due, poster)) = with(repo, |s| {
        let due = s.asked_github.get(&tag).is_none_or(|t| t.elapsed() >= Duration::from_secs(600));
        if due { s.asked_github.insert(tag.clone(), Instant::now()); }
        // tests run several pretend people from one GitHub account: that account posts for all
        (due, if testing() { s.gh_login.clone() } else { p.login.clone() })
    }) else { return false };
    if !due {
        return false;
    }
    let want = format!("{} {}", p.sign, p.pub_key);
    api("GET", &format!("/repos/{repo}/commits/{}/statuses?per_page=100", p.anchor), tok, None, None).is_ok_and(|(c, _, v)| {
        c == 200 && v.as_array().into_iter().flatten().any(|st| st["context"] == "qh/identity" && st["description"] == want.as_str()
            && st["creator"]["login"].as_str().is_some_and(|l| l.eq_ignore_ascii_case(&poster)))
    })
}

fn testing() -> bool {
    std::env::var("QH_TEAM_LOGIN").is_ok_and(|l| !l.is_empty())
}

/// Say something once per run (a repeated attack shouldn't flood the screen).
fn warn_once(repo: &str, hooks: Option<&Hooks>, tag: &str, text: String) {
    let first = with(repo, |s| if s.warned.iter().any(|w| w == tag) { false } else { s.warned.push(tag.to_string()); true }).unwrap_or(false);
    if first {
        match hooks {
            Some(h) => say(repo, h, text),
            None => log_to(repo, &text),
        }
    }
}

/// Whether to take this folder: it must be signed with the keys we know for that login. Keys
/// seen for the first time are trusted as first seen, or confirmed if GitHub vouches for them;
/// a change of keys is taken only if GitHub vouches for the new ones.
fn vet(repo: &str, tok: &str, p: &Peer, hooks: &Hooks) -> bool {
    let login = &p.login;
    if !p.signed {
        warn_once(repo, Some(hooks), &format!("unsigned {login}"), format!(
            "ignored {login}'s board folder: it isn't signed with their member key. Most likely they run a qh older than 0.1.4: ask them to run the install line again (kesavsabari.github.io/qh-rs). Otherwise someone else wrote it."));
        return false;
    }
    let Some((pin, newest)) = with(repo, |s| (s.pins.get(login).cloned(), s.git_ms.get(login).copied().unwrap_or(0))) else { return false };
    if p.seq < newest {
        log_to(repo, &format!("ignored an older copy of {login}'s folder"));
        return false;
    }
    let ok = match pin {
        Some((pk, sk, confirmed)) if pk == p.pub_key && sk == p.sign => {
            if !confirmed && vouched(repo, tok, p) {
                pin_keys(repo, p, true);
            }
            true
        }
        Some((old, _, _)) => {
            if vouched(repo, tok, p) {
                pin_keys(repo, p, true);
                if let Some(l) = link() { l.forget(&old) }
                say(repo, hooks, format!("{login}'s member key changed (a new machine?): GitHub confirms it's them"));
                true
            } else {
                warn_once(repo, Some(hooks), &format!("key {login} {}", p.sign), format!(
                    "someone wrote a board folder as {login} with keys that aren't theirs: ignored (GitHub doesn't vouch for them)"));
                false
            }
        }
        None => {
            let confirmed = vouched(repo, tok, p);
            pin_keys(repo, p, confirmed);
            if !confirmed {
                log_to(repo, &format!("{login}'s member key isn't confirmed by GitHub yet: trusting it as first seen"));
            }
            true
        }
    };
    if ok {
        with(repo, |s| s.git_ms.insert(login.clone(), p.seq));
    }
    ok
}

fn pin_keys(repo: &str, p: &Peer, confirmed: bool) {
    with(repo, |s| {
        s.pins.insert(p.login.clone(), (p.pub_key.clone(), p.sign.clone(), confirmed));
        save_json("team-pins", &s.repo, &json!(s.pins));
    });
}

fn confirmed(repo: &str, login: &str) -> bool {
    with(repo, |s| s.pins.get(login).is_some_and(|p| p.2)).unwrap_or(false)
}

/// A folder as pushed: the commit message and every file (path, exact contents).
struct Folder {
    msg: String,
    files: Vec<(String, String)>,
}

impl Folder {
    fn get(&self, name: &str) -> &str {
        self.files.iter().find(|(n, _)| n == name).map_or("", |(_, c)| c.as_str())
    }
}

fn folder_files(root: &Path, sha: &str) -> Folder {
    let commit = git_raw(root, &["cat-file", "commit", sha], None, &[]).unwrap_or_default();
    let commit = String::from_utf8_lossy(&commit);
    let msg = commit.split_once("\n\n").map_or("", |(_, m)| m).to_string();
    let list = git_raw(root, &["ls-tree", "-r", "-z", sha], None, &[]).unwrap_or_default();
    let entries: Vec<(String, String)> = list.split(|b| *b == 0).filter_map(|e| {
        let (meta, path) = std::str::from_utf8(e).ok()?.split_once('\t')?;
        let m: Vec<&str> = meta.split(' ').collect();
        (m.get(1) == Some(&"blob")).then(|| m.get(2).map(|b| (b.to_string(), path.to_string()))).flatten()
    }).collect();
    let mut files = vec![];
    if !entries.is_empty() {
        let ask: String = entries.iter().map(|(b, _)| format!("{b}\n")).collect();
        let out = git_raw(root, &["cat-file", "--batch"], Some(ask.as_bytes()), &[]).unwrap_or_default();
        let mut at = 0;
        for (_, path) in &entries {
            let Some(nl) = out[at..].iter().position(|b| *b == b'\n') else { break };
            let head = String::from_utf8_lossy(&out[at..at + nl]).to_string();
            let Some(size) = head.rsplit(' ').next().and_then(|n| n.parse::<usize>().ok()) else { break };
            let start = at + nl + 1;
            let Some(body) = out.get(start..start + size) else { break };
            files.push((path.clone(), String::from_utf8_lossy(body).to_string()));
            at = start + size + 1;
        }
    }
    Folder { msg, files }
}

/// Everyone's folders into refs/qh/board/*, listed as (login, commit).
fn fetch_board(root: &Path, repo: &str, tok: &str) -> Result<Vec<(String, String)>, String> {
    git_in(root, &["fetch", "-q", "--no-tags", "--prune", &format!("https://github.com/{repo}.git"), "+refs/qh/peers/*:refs/qh/board/*"],
           None, &auth_env(tok)).ok_or("fetching the board failed")?;
    let list = git(root, &["for-each-ref", "--format=%(refname:strip=3) %(objectname)", "refs/qh/board/"]).unwrap_or_default();
    Ok(list.lines().filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string()))).collect())
}

fn read_folder(f: &Folder, repo: &str, login: &str, key: Option<&Key>) -> Peer {
    let msg: Value = serde_json::from_str(&f.msg).unwrap_or_default();
    let ts = msg["ts"].as_u64().unwrap_or(0);
    let sign = f.get("sign").trim().to_string();
    let mut p = Peer {
        login: login.into(), ts, seq: msg["ms"].as_u64().unwrap_or(ts * 1000),
        online: msg["state"] == "online" && now().saturating_sub(ts) < OFFLINE_AFTER,
        pub_key: f.get("pub").trim().to_string(),
        signed: !sign.is_empty() && tc::folder_signed_by(&sign, f.get("sig"), repo, login, &f.msg, &f.files),
        anchor: f.get("anchor").trim().to_string(), sign,
        qh: msg["qh"].as_str().unwrap_or("").to_string(),
        ..Default::default()
    };
    for (name, content) in &f.files {
        if let Some(n) = name.strip_prefix("keys/") {
            p.wrapped.insert(n.to_string(), content.trim().to_string());
        } else if let Some(n) = name.strip_prefix("invites/") {
            p.invites.insert(n.to_string(), content.trim().to_string());
        }
    }
    p.epoch = f.get("epoch").trim().parse().unwrap_or(0);
    p.removed = f.get("removed").lines().map(String::from).filter(|l| !l.is_empty()).collect();
    let sealed = f.get("data").trim();
    let data: Option<Value> = key.and_then(|k| tc::open(k, &format!("data:{login}"), sealed)).and_then(|b| serde_json::from_slice(&b).ok());
    match data {
        None => p.locked = !sealed.is_empty(),
        Some(d) => parse_data(&mut p, &d),
    }
    p
}

/// A teammate's sealed data, opened: their sessions, claims, notes, messages…
fn parse_data(p: &mut Peer, d: &Value) {
    p.joined = true;
    p.sessions = d["sessions"].as_array().into_iter().flatten().map(|s| (
        s["name"].as_str().unwrap_or("").into(), s["status"].as_str().unwrap_or("").into(), s["role"].as_str().unwrap_or("").into(),
    )).collect();
    for s in d["sessions"].as_array().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or("").to_string();
        p.details.insert(name, SessionView {
            topic: s["topic"].as_str().unwrap_or("").into(),
            now: s["now"].as_str().unwrap_or("").into(),
            branch: s["branch"].as_str().unwrap_or("").into(),
            files: s["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect(),
            ..Default::default()
        });
    }
    p.files = d["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect();
    if let Some(t) = d["transcripts"].as_object() {
        for (s, msgs) in t {
            p.transcripts.insert(s.clone(), msgs.as_array().into_iter().flatten().map(|m| (
                m[0].as_str().unwrap_or("").to_string(), m[1].as_str().unwrap_or("").to_string())).collect());
        }
    }
    p.outbox = d["outbox"].as_array().cloned().unwrap_or_default();
    if let Some(r) = d["roles"].as_object() {
        p.roles = r.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("member").to_string())).collect();
    }
    p.chat = d["chat"].as_array().into_iter().flatten().map(|c| (
        c["id"].as_u64().unwrap_or(0), c["ts"].as_u64().unwrap_or(0), c["text"].as_str().unwrap_or("").to_string())).collect();
    p.dms = d["dms"].as_array().into_iter().flatten().map(|c| (
        c["id"].as_u64().unwrap_or(0), c["ts"].as_u64().unwrap_or(0), c["box"].as_str().unwrap_or("").to_string())).collect();
    p.groups = d["groups"].as_array().into_iter().flatten().map(|g| (g["gid"].as_str().unwrap_or("").to_string(),
        g["boxes"].as_array().into_iter().flatten().filter_map(|b| b.as_str().map(String::from)).collect())).collect();
    p.gmsgs = d["gmsgs"].as_array().into_iter().flatten().map(|c| (c["id"].as_u64().unwrap_or(0), c["ts"].as_u64().unwrap_or(0),
        c["g"].as_str().unwrap_or("").to_string(), c["box"].as_str().unwrap_or("").to_string())).collect();
    if let Some(c) = d["claims"].as_object() {
        p.claims = c.iter().map(|(sess, files)| (sess.clone(), files.as_object().into_iter().flatten().map(|(f, fns)| (f.clone(),
            fns.as_object().into_iter().flatten().map(|(n, t)| (n.clone(), t.as_u64().unwrap_or(0))).collect())).collect())).collect();
    }
    if let Some(u) = d["uses"].as_object() {
        p.uses = u.iter().map(|(sess, names)| (sess.clone(), names.as_object().into_iter().flatten()
            .map(|(n, t)| (n.clone(), t.as_u64().unwrap_or(0))).collect())).collect();
    }
    p.waiting = d["waiting"].as_array().into_iter().flatten().map(|w| Wait {
        session: w["session"].as_str().unwrap_or("").into(), file: w["file"].as_str().unwrap_or("").into(),
        func: w["func"].as_str().unwrap_or("").into(), on: w["on"].as_str().unwrap_or("").into(), since: w["since"].as_u64().unwrap_or(0),
    }).collect();
    p.promises = d["promises"].as_array().into_iter().flatten().map(promise_of).collect();
    p.sheets = d["sheets"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), Sheet {
        decided: v["decided"].as_str().unwrap_or("").into(), next: v["next"].as_str().unwrap_or("").into(), ts: v["ts"].as_u64().unwrap_or(0),
    })).collect();
    p.notes = d["notes"].as_array().into_iter().flatten().map(|n| (
        n["id"].as_u64().unwrap_or(0), n["text"].as_str().unwrap_or("").to_string(), n["file"].as_str().unwrap_or("").to_string(),
        n["names"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect())).collect();
    if let Some(a) = d["acks"].as_object() {
        p.acks = a.iter().map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0))).collect();
    }
    p.link = d["link"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
    p.link_ms = d["link_ms"].as_u64().unwrap_or(0);
}

fn per_repo(dir: &str, repo: &str) -> PathBuf {
    crate::config::state_dir().join(dir).join(repo.replace('/', "__"))
}

pub const ROLES: [&str; 3] = ["viewer", "member", "trusted"];

/// What each role may do in *our* sessions. We enforce it ourselves: their messages only
/// run on our machine if our qh lets them in.
pub fn role_help(role: &str) -> &'static str {
    match role {
        "viewer" => "sees your sessions and who's online; can't write into them",
        "trusted" => "sees your sessions; their agents' questions are answered without asking (work still waits for you to accept)",
        _ => "sees your sessions; you allow each thing they write",
    }
}

fn load_roles(repo: &str) -> HashMap<String, String> {
    serde_json::from_str(&std::fs::read_to_string(per_repo("team-roles", repo)).unwrap_or_default()).unwrap_or_default()
}

/// Give `login` a role in `repo`; works whether or not team mode is on.
pub fn set_role(repo: &str, login: &str, role: &str) -> Result<String, String> {
    if !ROLES.contains(&role) {
        return Err(format!("roles are {}", ROLES.join(", ")));
    }
    let mut roles = load_roles(repo);
    if role == "member" { roles.remove(login); } else { roles.insert(login.to_string(), role.to_string()); }
    let p = per_repo("team-roles", repo);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    std::fs::write(&p, serde_json::to_string(&roles).unwrap_or_default()).map_err(|e| e.to_string())?;
    with(repo, |s| { s.roles = roles; s.dirty = true; });
    Ok(format!("{login} is now a {role} in your sessions: {}", role_help(role)))
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

fn load_list(dir: &str, repo: &str) -> Vec<String> {
    serde_json::from_str(&std::fs::read_to_string(per_repo(dir, repo)).unwrap_or_default()).unwrap_or_default()
}

fn save_json(dir: &str, repo: &str, v: &Value) {
    let p = per_repo(dir, repo);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = crate::platform::write_private(&p, v.to_string().as_bytes());
}

/// Let `login` in without asking when they turn up (you invited them).
pub fn approve(repo: &str, login: &str) {
    let mut v = load_list("team-approved", repo);
    if !v.iter().any(|x| x == login) { v.push(login.to_string()) }
    save_json("team-approved", repo, &json!(v));
    with(repo, |s| {
        if !s.approved.iter().any(|x| x == login) { s.approved.push(login.to_string()) }
        s.asking.retain(|x| x != login);
        s.dirty = true;
    });
}

/// Make sure a team key exists for `repo` before anyone has connected (team create does this,
/// so the first invites can carry the key).
pub fn ensure_key(repo: &str) {
    if load_key(repo).is_none() { save_key(repo, &tc::new_team_key()) }
}

/// An invite for `login`: approve them and, if we hold the team key, publish it sealed with a
/// fresh code. Returns the code to give them (`qh join owner/repo CODE`).
pub fn add_invite(repo: &str, login: &str) -> Option<String> {
    approve(repo, login);
    let key = load_key(repo)?;
    let code: [u8; 32] = crate::oauth::random();
    let sealed = tc::seal(&code, &format!("invite:{repo}:{login}"), &key);
    let mut all: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-invites", repo)).unwrap_or_default()).unwrap_or_default();
    all.insert(login.to_string(), sealed.clone());
    save_json("team-invites", repo, &json!(all));
    with(repo, |s| { s.invites.insert(login.to_string(), sealed); s.dirty = true; });
    Some(crate::oauth::b64url(&code))
}

/// The code from `qh join owner/repo CODE`, kept until it has opened the team key.
pub fn set_join_code(repo: &str, code: &str) {
    let p = per_repo("team-codes", repo);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = crate::platform::write_private(&p, code.trim().as_bytes());
}

/// What our own sessions look like, supplied by the UI: {sessions:[{name,status,role,topic,now,files,branch}], files:[], transcripts:{name:[[role,text]]}}.
pub type Summary = Box<dyn Fn() -> Value + Send>;

/// Callbacks into the UI: say a line, deliver a message to a session, ask whether to allow one.
pub struct Hooks {
    pub say: Box<dyn Fn(String) + Send>,
    pub deliver: Box<dyn Fn(&str, &str) + Send>,
    pub ask: Box<dyn Fn(&Incoming) + Send>,
    pub refresh: Box<dyn Fn() + Send>, // redraw: the status line changed
    /// Answer a teammate's question from a read-only copy of the asked session (returns at once).
    pub answer: std::sync::Arc<dyn Fn(Question) + Send + Sync>,
    /// An answer came back for one of our sessions: hand it over (now, or before its next call).
    pub answered: Box<dyn Fn(&str, &str) + Send>,
}

/// Connect this folder's project (or just show it, if it's already connected).
pub fn start(cwd: &Path, summary: Summary, hooks: Hooks) -> Result<String, String> {
    let (root, repo) = github_repo(cwd).ok_or("this folder's git remote isn't on GitHub (other hosts come later)")?;
    if running_for(&repo) {
        set_current(&repo);
        return Ok(format!("team {repo} is connected"));
    }
    let tok = token().ok_or("no GitHub login found: run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, user) = api("GET", "/user", &tok, None, None)?;
    let me = user["login"].as_str().ok_or("GitHub didn't say who you are")?.to_string();
    let gh_login = me.clone();
    let me = std::env::var("QH_TEAM_LOGIN").ok().filter(|l| !l.is_empty()).unwrap_or(me); // testing two people on one machine
    let (secret, my_pub) = tc::identity();
    let sign_pub = tc::sign_pub(&secret);
    let idc = identity_commit(&root, &me, &my_pub, &sign_pub).ok_or("git couldn't create your identity commit")?;
    let seen: HashMap<String, u64> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-seen", &repo)).unwrap_or_default()).unwrap_or_default();
    let invites: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-invites", &repo)).unwrap_or_default()).unwrap_or_default();
    let chat: Vec<(u64, String, String)> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-chat", &repo)).unwrap_or_default()).unwrap_or_default();
    let mine: Vec<&(u64, String, String)> = chat.iter().filter(|(_, f, _)| *f == me).collect();
    let my_chat: Vec<Value> = mine[mine.len().saturating_sub(30)..].iter().copied()
        .enumerate().map(|(i, (ts, _, t))| json!({"id": ts * 1000 + i as u64, "ts": ts, "text": t})).collect();
    let (running, finished) = (std::sync::Arc::new(AtomicBool::new(true)), std::sync::Arc::new(AtomicBool::new(false)));
    STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), State {
        repo: repo.clone(), me: me.clone(), peers: HashMap::new(), error: String::new(), key: load_key(&repo),
        outbox: vec![], next_id: now() * 1000, seen, pending: vec![], roles: load_roles(&repo),
        approved: load_list("team-approved", &repo), asking: vec![], invites, chat,
        epoch: std::fs::read_to_string(per_repo("team-epoch", &repo)).ok().and_then(|e| e.trim().parse().ok()).unwrap_or(0),
        removed: load_list("team-removed", &repo), admins: HashMap::new(),
        pins: serde_json::from_str(&std::fs::read_to_string(per_repo("team-pins", &repo)).unwrap_or_default()).unwrap_or_default(),
        gh_login: gh_login.clone(), git_ms: HashMap::new(), asked_github: HashMap::new(), warned: vec![], my_sha: String::new(), my_chat, chat_seen: HashMap::new(), unread: 0,
        my_notes: vec![], notes_seen: HashMap::new(), claims: HashMap::new(), waiting: vec![], uses: HashMap::new(),
        sheets: HashMap::new(), asked: vec![], answered: vec![], answerer: Some(hooks.answer.clone()),
        promises: serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(per_repo("team-promises", &repo)).unwrap_or_default())
            .unwrap_or_default().iter().map(promise_of).collect(),
        last_answer: HashMap::new(),
        answers: std::fs::read_to_string(per_repo("team-answers", &repo)).ok().map(|s| s.trim().to_string())
            .filter(|s| ["auto", "ask", "off"].contains(&s.as_str())).unwrap_or_else(|| "auto".into()),
        secret: Some(secret.clone()), my_dms: vec![], my_groups: vec![], my_gmsgs: vec![], groups: HashMap::new(), msgs: vec![],
        msg_ids: Default::default(), chan_unread: HashMap::new(), pinged: vec![],
        dirty: true, running: running.clone(), finished: finished.clone(),
    });
    with(&repo, load_private);
    set_current(&repo);
    let (repo2, me2) = (repo.clone(), me.clone());
    let (live_tx, live_rx) = std::sync::mpsc::channel();
    LIVE.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), live_tx);
    let line = link();
    std::thread::Builder::new().name(format!("qh-team-{repo}")).spawn(move || {
        let mut etag: Option<String> = None;
        let mut shas: HashMap<String, String> = HashMap::new();
        let (mut last_files, mut last_write, mut last_poll) = (vec![], Instant::now() - KEEPALIVE, Instant::now() - POLL);
        let mut last_link_ms = 0u64; // the address stamp in the last folder we wrote
        let mut fetched = false;
        let mut live = Live { last: None, at: Instant::now(), pulse: 0, told: vec![] };
        // my folder: the files for git, and the commit message (presence); plus the data without
        // transcripts, for the direct line
        let build = |state_word: &str| -> (Vec<(String, String)>, Value, Option<Value>) {
            let mut quick = None;
            let mut files = vec![("pub".to_string(), my_pub.clone())];
            let d = summary(); // outside the lock: it reads the hub
            with(&repo2, |s| {
                if let Some(k) = &s.key {
                    // the key goes to members, and to newcomers we approved; nobody else
                    let mut wraps: Vec<(String, String)> = s.peers.values()
                        .filter(|p| !p.pub_key.is_empty() && (p.joined || s.approved.contains(&p.login)) && !s.removed.contains(&p.login))
                        .filter_map(|p| Some((format!("keys/{}", p.login), tc::wrap(k, &secret, &p.pub_key, &s.repo)?))).collect();
                    wraps.sort();
                    files.extend(wraps);
                    if s.epoch > 0 {
                        files.push(("epoch".into(), s.epoch.to_string()));
                    }
                    if !s.removed.is_empty() {
                        files.push(("removed".into(), s.removed.join("\n")));
                    }
                    let mut inv: Vec<(String, String)> = s.invites.iter().map(|(l, v)| (format!("invites/{l}"), v.clone())).collect();
                    inv.sort();
                    files.extend(inv);
                    let mut d = d.clone();
                    d["outbox"] = Value::from(s.outbox.clone());
                    d["acks"] = json!(s.seen);
                    d["roles"] = json!(s.roles);
                    d["chat"] = Value::from(s.my_chat.clone());
                    d["dms"] = Value::from(s.my_dms.clone());
                    d["groups"] = Value::from(s.my_groups.clone());
                    d["gmsgs"] = Value::from(s.my_gmsgs.clone());
                    d["notes"] = Value::from(s.my_notes.clone());
                    d["claims"] = json!(s.claims);
                    d["uses"] = json!(s.uses);
                    d["promises"] = promises_json(&s.promises);
                    d["sheets"] = s.sheets.iter().map(|(k, v)| (k.clone(), json!({"decided": v.decided, "next": v.next, "ts": v.ts})))
                        .collect::<serde_json::Map<_, _>>().into();
                    d["waiting"] = Value::from(s.waiting.iter().map(|w| json!({"session": w.session, "file": w.file, "func": w.func, "on": w.on, "since": w.since})).collect::<Vec<_>>());
                    if let Some(l) = &line {
                        d["link"] = json!(l.addrs());
                        d["link_ms"] = json!(l.addrs_ms());
                    }
                    files.push(("data".into(), tc::seal(k, &format!("data:{}", s.me), d.to_string().as_bytes())));
                    let mut q = d;
                    if let Some(o) = q.as_object_mut() { o.remove("transcripts"); }
                    quick = Some(q);
                }
            });
            let msg = json!({"v": 2, "state": state_word, "ts": now(), "ms": now_ms(), "qh": crate::update::VERSION});
            files.push(("sign".into(), sign_pub.clone()));
            files.push(("anchor".into(), idc.clone()));
            let sig = tc::sign_folder(&secret, &repo2, &me2, &msg.to_string(), &files);
            files.push(("sig".into(), sig));
            (files, msg, quick)
        };
        while running.load(Relaxed) {
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
                                Ok(list) => { fetched = true; absorb(&repo2, &root, &list, &mut shas, &secret, &tok, &hooks); (hooks.refresh)() }
                                Err(e) => set_error(&repo2, e),
                            }
                        }
                    }
                    Ok((404, _, _)) => { fetched = true; settle_key(&repo2) } // an empty board
                    Ok(_) => {}
                    Err(e) => set_error(&repo2, e),
                }
                stale_check(&repo2, &hooks);
                wake_check(&repo2, &root, &hooks);
            }
            // write my folder: when it changed (throttled), when asked (dirty), or as a keep-alive.
            // Wrapped keys are fresh nonces each time, so compare without them.
            let (files, msg, quick) = build("online");
            if let (Some(l), Some(d)) = (&line, quick) {
                push_live(&repo2, &me2, l, d, &mut live);
            }
            let dirty = with(&repo2, |s| s.dirty).unwrap_or(false);
            let plain = |f: &[(String, String)]| -> Vec<String> { f.iter().map(|(n, c)| if n == "data" || n == "sig" || n.contains('/') { n.clone() } else { format!("{n}={c}") }).collect() };
            let changed = plain(&files) != plain(&last_files) || data_changed(&repo2, &files, &last_files);
            // new line addresses skip the throttle: until they're out, teammates can't reach us directly
            let link_ms = line.as_ref().map_or(0, |l| l.addrs_ms());
            let moved = link_ms != last_link_ms && last_write.elapsed() >= MIN_LINK_WRITE;
            if fetched && (((changed || dirty) && (last_write.elapsed() >= MIN_WRITE || moved)) || last_write.elapsed() >= KEEPALIVE) {
                match publish(&root, &repo2, &me2, &tok, &files, &msg, &idc) {
                    Ok(sha) => {
                        last_files = files;
                        last_link_ms = link_ms;
                        with(&repo2, |s| { s.dirty = false; s.error.clear(); s.my_sha = sha });
                        vouch(&repo2, &tok, &idc, &sign_pub, &my_pub);
                    }
                    Err(e) => set_error(&repo2, e),
                }
                last_write = Instant::now();
            }
            // wait half a second, but take in what comes over the line at once, and go round
            // early when a message, answer or note of ours is waiting to go out
            let until = Instant::now() + Duration::from_millis(500);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match live_rx.recv_timeout(left.min(Duration::from_millis(100))) {
                    Ok((from, bytes)) => {
                        take_live(&repo2, &root, &from, &bytes, &hooks);
                        (hooks.refresh)();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => if line.is_some() && pulse(&repo2) != live.pulse { break },
                    Err(_) => std::thread::sleep(left),
                }
            }
        }
        if let Some(m) = LIVE.lock().unwrap().as_mut() { m.remove(&repo2); }
        let (files, msg, _) = build("offline");
        let _ = publish(&root, &repo2, &me2, &tok, &files, &msg, &idc);
        finished.store(true, Relaxed);
    }).map_err(|e| e.to_string())?;
    remember(&repo, true);
    Ok(format!("team mode on for {repo} as {me} (shared data is encrypted for the team)"))
}

/// Whether the sealed `data` says something different (it's re-sealed with a new nonce every
/// build, so compare what it decrypts to).
fn data_changed(repo: &str, a: &[(String, String)], b: &[(String, String)]) -> bool {
    with(repo, |s| {
        let Some(k) = s.key else { return false };
        let get = |f: &[(String, String)]| f.iter().find(|(n, _)| n == "data").and_then(|(_, c)| tc::open(&k, &format!("data:{}", s.me), c));
        get(a) != get(b)
    }).unwrap_or(false)
}

fn set_error(repo: &str, e: String) {
    with(repo, |s| s.error = e);
}

/// Take in freshly fetched folders: our key if someone wrapped it for us (or left it under our
/// invite code), presence, newcomers, chat, transcripts, and messages to our sessions.
fn absorb(repo: &str, root: &Path, list: &[(String, String)], shas: &mut HashMap<String, String>,
          secret: &x25519_dalek::StaticSecret, tok: &str, hooks: &Hooks) {
    let Some((me, my_sha)) = with(repo, |s| (s.me.clone(), s.my_sha.clone())) else { return };
    // our own folder, rewritten by someone else: put ours back at once
    if let Some((_, sha)) = list.iter().find(|(l, _)| *l == me).filter(|(_, sha)| !my_sha.is_empty() && *sha != my_sha) {
        let ours = read_folder(&folder_files(root, sha), repo, &me, None);
        if !ours.signed || ours.sign != tc::sign_pub(secret) {
            with(repo, |s| s.dirty = true);
            warn_once(repo, Some(hooks), &format!("overwritten {sha}"), "someone overwrote your board folder: writing yours back".into());
        }
    }
    let mut folders = vec![];
    for (login, sha) in list.iter().filter(|(l, _)| *l != me) {
        shas.insert(login.clone(), sha.clone());
        let f = folder_files(root, sha);
        let p = read_folder(&f, repo, login, None);
        if vet(repo, tok, &p, hooks) {
            if crate::update::newer(&p.qh, crate::update::VERSION) {
                warn_once(repo, Some(hooks), &format!("newer qh {}", p.qh), format!(
                    "{login} runs qh {}, newer than yours ({}): run {} so you both work the same way",
                    p.qh, crate::update::VERSION, crate::update::how()));
            }
            folders.push((f, p));
        }
    }
    newer_key(repo, &folders, secret, tok, hooks);
    let Some(mut key) = with(repo, |s| s.key) else { return };
    if key.is_none() {
        let code = std::fs::read_to_string(per_repo("team-codes", repo)).ok()
            .and_then(|c| <[u8; 32]>::try_from(crate::oauth::b64url_decode(c.trim())).ok());
        for (_, p) in &folders {
            let login = &p.login;
            let by_wrap = p.wrapped.get(&me).and_then(|w| tc::unwrap(w, secret, &p.pub_key, repo));
            let by_code = || code.and_then(|c| p.invites.get(&me).and_then(|b| tc::open(&c, &format!("invite:{repo}:{me}"), b)))
                .and_then(|k| <Key>::try_from(k).ok());
            if let Some(k) = by_wrap.or_else(by_code) {
                save_key(repo, &k);
                let _ = std::fs::remove_file(per_repo("team-codes", repo));
                key = Some(k);
                say(repo, hooks, format!("you're in the team: you can see everyone's sessions now (thanks to {login})"));
                break;
            }
        }
    }
    let mut new_chat = false;
    for (f, p) in &folders {
        let p = read_folder(f, repo, &p.login, key.as_ref());
        take_in(repo, &me, key, p, hooks, &mut new_chat);
    }
    if new_chat { save_chat(repo) }
    save_seen(repo);
    settle_key(repo);
}

/// Someone removed a member and made a new team key: switch to it, if GitHub says they're a repo
/// admin and vouched for their member key (a folder signed with keys we only trust on first
/// sight isn't enough: anyone with push access could have written it). If we are the one
/// removed, say so.
fn newer_key(repo: &str, folders: &[(Folder, Peer)], secret: &x25519_dalek::StaticSecret, tok: &str, hooks: &Hooks) {
    let Some((me, epoch, has_key)) = with(repo, |s| (s.me.clone(), s.epoch, s.key.is_some())) else { return };
    if !has_key {
        return; // newcomers take whatever key they're given, the usual way
    }
    for (_, p) in folders {
        let login = &p.login;
        if p.epoch <= epoch {
            continue;
        }
        if !confirmed(repo, login) {
            warn_once(repo, Some(hooks), &format!("rotate {login} {}", p.epoch), format!(
                "ignored a team key change from {login}: GitHub hasn't vouched for their member key"));
            continue;
        }
        if !is_admin(repo, login, tok) {
            log_to(repo, &format!("ignored a team key change from {login}: only repo admins can change it"));
            continue;
        }
        if p.removed.contains(&me) {
            let told = with(repo, |s| std::mem::replace(&mut s.epoch, p.epoch) == p.epoch).unwrap_or(true);
            if !told {
                say(repo, hooks, format!("{login} removed you from the team: you can't read what the team writes from now on"));
            }
            return;
        }
        let Some(k) = p.wrapped.get(&me).and_then(|w| tc::unwrap(w, secret, &p.pub_key, repo)) else { continue };
        save_key(repo, &k);
        let gone = with(repo, |s| {
            // everyone we knew as a member keeps getting the key from us too (their data stays
            // locked to us until they switch, so "joined" alone would drop them)
            let keep: Vec<String> = s.peers.values().filter(|x| x.joined && !p.removed.contains(&x.login)).map(|x| x.login.clone()).collect();
            s.key = Some(k);
            s.epoch = p.epoch;
            let newly: Vec<String> = p.removed.iter().filter(|r| !s.removed.contains(r)).cloned().collect();
            s.removed.extend(newly.iter().cloned());
            s.approved.retain(|a| !s.removed.contains(a));
            s.dirty = true;
            (keep, newly)
        });
        let Some((keep, newly)) = gone else { return };
        persist_membership(repo);
        for l in keep {
            approve(repo, &l);
        }
        forget_lines(repo, &newly);
        let who = if newly.is_empty() { String::new() } else { format!(": {} removed", newly.join(", ")) };
        say(repo, hooks, format!("{login} changed the team key{who}"));
        return;
    }
}

/// Whether GitHub says `login` is an admin of `repo` (asked once per person, then remembered).
fn is_admin(repo: &str, login: &str, tok: &str) -> bool {
    if let Ok(list) = std::env::var("QH_TEAM_ADMINS") {
        return list.split(',').any(|a| a.trim() == login); // testing several people on one machine (with QH_TEAM_LOGIN)
    }
    if let Some(a) = with(repo, |s| s.admins.get(login).copied()).flatten() {
        return a;
    }
    let a = api("GET", &format!("/repos/{repo}/collaborators/{login}/permission"), tok, None, None)
        .map(|(c, _, v)| c == 200 && (v["permission"] == "admin" || v["role_name"] == "admin")).unwrap_or(false);
    with(repo, |s| s.admins.insert(login.to_string(), a));
    a
}

fn persist_membership(repo: &str) {
    with(repo, |s| {
        let e = per_repo("team-epoch", &s.repo);
        let _ = std::fs::create_dir_all(e.parent().unwrap_or(&e));
        let _ = std::fs::write(e, s.epoch.to_string());
        save_json("team-removed", &s.repo, &json!(s.removed));
        save_json("team-approved", &s.repo, &json!(s.approved));
    });
}

/// Drop removed people's direct lines (and our memory of where they were).
fn forget_lines(repo: &str, logins: &[String]) {
    let keys: Vec<String> = with(repo, |s| logins.iter().filter_map(|l| s.peers.get(l).map(|p| p.pub_key.clone())).collect()).unwrap_or_default();
    if let Some(l) = link() {
        for k in keys.iter().filter(|k| !k.is_empty()) {
            l.forget(k);
        }
    }
}

/// Remove `login` from the team: a new team key for everyone else, and no more access to the repo.
/// Only a repo admin can do it (GitHub decides who that is).
pub fn remove(repo: &str, login: &str) -> Result<String, String> {
    let tok = token().ok_or("no GitHub login found: run `gh auth login`")?;
    let me = with(repo, |s| s.me.clone()).ok_or("team mode isn't on for this project: /team on first")?;
    if login == me {
        return Err("you can't remove yourself; leave with /team off".into());
    }
    if !is_admin(repo, &me, &tok) {
        return Err("only a repo admin can remove people (GitHub says you aren't one)".into());
    }
    let known = with(repo, |s| s.peers.contains_key(login) || s.approved.iter().any(|a| a == login)).unwrap_or(false);
    if !known {
        return Err(format!("{login} isn't in this team"));
    }
    with(repo, |s| {
        let keep: Vec<String> = s.peers.values().filter(|p| p.joined && p.login != login).map(|p| p.login.clone()).collect();
        for l in keep {
            if !s.approved.contains(&l) { s.approved.push(l) }
        }
        s.approved.retain(|a| a != login);
        s.asking.retain(|a| a != login);
        s.invites.remove(login);
        if !s.removed.iter().any(|r| r == login) { s.removed.push(login.to_string()) }
        let k = tc::new_team_key();
        save_key(&s.repo, &k);
        s.key = Some(k);
        s.epoch += 1;
        s.dirty = true;
        save_json("team-invites", &s.repo, &json!(s.invites));
    });
    persist_membership(repo);
    forget_lines(repo, &[login.to_string()]);
    // and their access to the repo itself, or they could still read the code and write noise
    let testing = std::env::var("QH_TEAM_ADMINS").is_ok(); // pretend people: never touch real GitHub users
    let access = if testing { format!("(test run: {login}'s repo access left alone)") } else { match api("DELETE", &format!("/repos/{repo}/collaborators/{login}"), &tok, None, None) {
        Ok((204, _, _)) => format!("{login} no longer has access to {repo}"),
        Ok((404, _, _)) => format!("{login} had no direct access to {repo} (check org teams if they still can see it)"),
        Ok((c, _, _)) => format!("GitHub answered {c} when removing {login}'s access: check the repo's settings"),
        Err(e) => format!("couldn't remove {login}'s access to {repo}: {e}"),
    } };
    log_to(repo, &format!("removed {login}: new team key"));
    Ok(format!("{login} is out of the team: everyone else gets a new team key, so nothing written from now on is readable to them.\n{access}"))
}

fn save_chat(repo: &str) {
    with(repo, |s| {
        let keep: Vec<_> = s.chat.iter().rev().take(300).rev().cloned().collect();
        save_json("team-chat", repo, &json!(keep));
        save_private(s);
    });
}

// ---- direct messages and groups ----
// The team key opens everything on the board, so private lines get a second lock inside it:
// a direct message is sealed with the key only its two people share (X25519 between their member
// keys, see teamcrypto::seal_for); a group has its own random key, sealed that way for each
// member by whoever made it. A teammate who isn't in it sees only opaque boxes (and how many).

/// Direct messages and group lines kept on disk (0600): the channels, our published boxes and the
/// group keys, so a restart neither forgets them nor stops offering them to an offline teammate.
fn save_private(s: &State) {
    if cfg!(test) || s.secret.is_none() { return } // tests write nothing to the real state folder
    let keep = s.msgs.len().saturating_sub(1000);
    let msgs: Vec<Value> = s.msgs[keep..].iter().map(|m| json!([m.ts, m.chan, m.from, m.text, m.id])).collect();
    let groups: serde_json::Map<String, Value> = s.groups.iter().map(|(id, g)| (id.clone(), json!({
        "name": g.name, "members": g.members, "owner": g.owner, "key": crate::oauth::b64url(&g.key)}))).collect();
    save_json("team-private", &s.repo, &json!({"msgs": msgs, "groups": groups, "dms": s.my_dms, "my_groups": s.my_groups, "gmsgs": s.my_gmsgs}));
}

fn load_private(s: &mut State) {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(per_repo("team-private", &s.repo)).unwrap_or_default()).unwrap_or_default();
    s.msgs = v["msgs"].as_array().into_iter().flatten().map(|m| Msg {
        ts: m[0].as_u64().unwrap_or(0), chan: m[1].as_str().unwrap_or("").into(), from: m[2].as_str().unwrap_or("").into(),
        text: m[3].as_str().unwrap_or("").into(), id: m[4].as_u64().unwrap_or(0),
    }).collect();
    for (id, g) in v["groups"].as_object().into_iter().flatten() {
        let Some(key) = g["key"].as_str().and_then(|k| crate::oauth::b64url_decode(k).try_into().ok()) else { continue };
        s.groups.insert(id.clone(), Group { name: g["name"].as_str().unwrap_or("").into(), owner: g["owner"].as_str().unwrap_or("").into(),
            members: g["members"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect(), key });
    }
    s.my_dms = v["dms"].as_array().cloned().unwrap_or_default();
    s.my_groups = v["my_groups"].as_array().cloned().unwrap_or_default();
    s.my_gmsgs = v["gmsgs"].as_array().cloned().unwrap_or_default();
}

/// Whether `text` mentions `me`: `@me` or `@team`, as a word of its own (an email address like
/// `x@me.com` or `@me.com` doesn't count). GitHub logins ignore case.
pub fn pings(text: &str, me: &str) -> bool {
    for (i, _) in text.match_indices('@') {
        if text[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || "._-+%".contains(c)) { continue }
        let rest = &text[i + 1..];
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
        let after = &rest[name.len()..];
        // "@bob.example": a domain, not bob
        if after.strip_prefix('.').and_then(|a| a.chars().next()).is_some_and(|c| c.is_alphanumeric()) { continue }
        if !name.is_empty() && (name.eq_ignore_ascii_case(me) || name.eq_ignore_ascii_case("team")) { return true }
    }
    false
}

/// A teammate's text, safe to show on one line: no control characters (newlines become spaces).
fn plain(text: &str) -> String {
    text.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).filter(|c| !c.is_control()).collect()
}

fn clip_to(s: &str, n: usize) -> String {
    let s = plain(s);
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

/// Add a line to a private channel (once); a teammate's line counts as unread, and as a ping when it mentions us.
fn push_msg(s: &mut State, mut m: Msg) -> bool {
    if s.msgs.iter().any(|x| x.from == m.from && x.id == m.id && x.chan == m.chan) { return false }
    m.text = m.text.chars().filter(|c| !c.is_control() || *c == '\n').collect();
    if m.from != s.me {
        *s.chan_unread.entry(m.chan.clone()).or_default() += 1;
        if pings(&m.text, &s.me) && !s.pinged.contains(&m.chan) { s.pinged.push(m.chan.clone()) }
    }
    let at = s.msgs.iter().rposition(|x| x.ts <= m.ts).map_or(0, |i| i + 1);
    s.msgs.insert(at, m);
    let n = s.msgs.len();
    if n > 1000 { s.msgs.drain(..n - 1000); }
    true
}

/// What `p` sealed for us alone: groups they put us in, direct messages, lines in our groups.
/// Returns whether anything was new.
fn take_private(s: &mut State, login: &str, p: &Peer) -> bool {
    let Some(secret) = s.secret.clone() else { return false };
    if p.pub_key.is_empty() || !p.joined { return false }
    let (me, repo) = (s.me.clone(), s.repo.clone());
    let mut new = false;
    for (gid, boxes) in &p.groups {
        if gid.is_empty() || s.groups.contains_key(gid) { continue }
        // one box per member; ours is the one that opens
        let Some(v) = boxes.iter().find_map(|b| tc::open_from(&secret, &p.pub_key, &repo, "group", login, &me, b))
            .and_then(|x| serde_json::from_slice::<Value>(&x).ok()) else { continue };
        let members: Vec<String> = v["members"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect();
        let key: Option<Key> = v["key"].as_str().and_then(|k| crate::oauth::b64url_decode(k).try_into().ok());
        let (Some(key), true) = (key, v["gid"] == gid.as_str() && members.contains(&me) && members.iter().any(|m| m == login)) else { continue };
        let name = clip_to(v["name"].as_str().unwrap_or("group"), 40);
        log_to(&repo, &format!("{login} added you to the group {name} ({})", members.join(", ")));
        alert(format!("qh: {login} added you to the group {name}"));
        s.groups.insert(gid.clone(), Group { name, members, owner: login.into(), key });
        new = true;
    }
    for (id, ts, b) in &p.dms {
        if !s.msg_ids.insert((format!("dm:{login}"), *id)) { continue }
        // most boxes are for someone else and don't open for us
        let Some(text) = tc::open_from(&secret, &p.pub_key, &repo, "dm", login, &me, b).and_then(|x| String::from_utf8(x).ok()) else { continue };
        if push_msg(s, Msg { ts: *ts, chan: format!("@{login}"), from: login.into(), text: text.clone(), id: *id }) {
            new = true;
            alert(format!("qh: {login} (private): {}", clip_to(&text, 80)));
        }
    }
    for (id, ts, gid, b) in &p.gmsgs {
        // a group we don't know yet isn't marked tried: its key may come with its owner's folder
        let Some(g) = s.groups.get(gid).filter(|g| g.members.iter().any(|m| m == login)) else { continue };
        let (gkey, gname) = (g.key, g.name.clone());
        if !s.msg_ids.insert((format!("g:{login}"), *id)) { continue }
        let Some(text) = tc::open(&gkey, &format!("gmsg:{repo}:{gid}:{login}"), b).and_then(|x| String::from_utf8(x).ok()) else { continue };
        if push_msg(s, Msg { ts: *ts, chan: format!("+{gid}"), from: login.into(), text: text.clone(), id: *id }) {
            new = true;
            if pings(&text, &me) { alert(format!("qh: {login} mentioned you in {gname}: {}", clip_to(&text, 80))) }
        }
    }
    new
}

fn msg_id(s: &State) -> u64 {
    let last = s.my_dms.iter().chain(&s.my_gmsgs).filter_map(|m| m["id"].as_u64()).max().unwrap_or(0);
    (now() * 1000).max(last + 1)
}

/// Seal `text` for `to` alone and publish it with our next write.
fn dm_locked(s: &mut State, to: &str, text: &str) -> Result<(), String> {
    let to = to.trim_start_matches('@');
    let secret = s.secret.clone().ok_or("team mode is off")?;
    let p = s.peers.get(to).filter(|p| p.joined && !p.pub_key.is_empty()).ok_or_else(|| format!("no teammate {to}"))?;
    let sealed = tc::seal_for(&secret, &p.pub_key, &s.repo, "dm", &s.me, to, text.as_bytes()).ok_or("couldn't seal the message")?;
    let (id, ts) = (msg_id(s), now());
    s.my_dms.push(json!({"id": id, "ts": ts, "box": sealed}));
    let n = s.my_dms.len();
    if n > 100 { s.my_dms.drain(..n - 100); }
    let me = s.me.clone();
    push_msg(s, Msg { ts, chan: format!("@{to}"), from: me, text: text.into(), id });
    s.dirty = true;
    save_private(s);
    Ok(())
}

/// Make a group of us and `logins`: a fresh key, sealed for each of them alone.
fn group_locked(s: &mut State, name: &str, logins: &[String]) -> Result<String, String> {
    let secret = s.secret.clone().ok_or("team mode is off")?;
    let name = clip_to(name.trim().trim_start_matches(['#', '+']), 40);
    if name.is_empty() || logins.is_empty() { return Err("usage: /team group <name> <login> <login>…".into()) }
    let mut members = vec![s.me.clone()];
    for l in logins {
        let l = l.trim_start_matches('@');
        if !s.peers.get(l).is_some_and(|p| p.joined && !p.pub_key.is_empty()) { return Err(format!("no teammate {l}")) }
        if !members.iter().any(|m| m == l) { members.push(l.to_string()) }
    }
    if members.len() < 2 { return Err("a group needs at least one teammate besides you".into()) }
    if s.groups.values().any(|g| g.name.eq_ignore_ascii_case(&name)) { return Err(format!("there's already a group {name}")) }
    let gid = crate::oauth::b64url(&crate::oauth::random()[..9]);
    let key = tc::new_team_key();
    let def = json!({"gid": gid, "name": name, "members": members, "key": crate::oauth::b64url(&key)}).to_string();
    let mut boxes = vec![];
    for m in members.iter().filter(|m| **m != s.me) {
        let pk = &s.peers[m].pub_key;
        boxes.push(tc::seal_for(&secret, pk, &s.repo, "group", &s.me, m, def.as_bytes()).ok_or("couldn't seal the group key")?);
    }
    s.my_groups.push(json!({"gid": gid, "boxes": boxes}));
    s.groups.insert(gid, Group { name: name.clone(), members: members.clone(), owner: s.me.clone(), key });
    s.dirty = true;
    save_private(s);
    Ok(format!("group {name}: {} · only its members can read it", members.join(", ")))
}

fn group_send_locked(s: &mut State, gid: &str, text: &str) -> Result<(), String> {
    let g = s.groups.get(gid).ok_or("no such group")?;
    if !g.members.contains(&s.me) { return Err("you're not in that group".into()) }
    let sealed = tc::seal(&g.key, &format!("gmsg:{}:{gid}:{}", s.repo, s.me), text.as_bytes());
    let (id, ts) = (msg_id(s), now());
    s.my_gmsgs.push(json!({"id": id, "ts": ts, "g": gid, "box": sealed}));
    let n = s.my_gmsgs.len();
    if n > 100 { s.my_gmsgs.drain(..n - 100); }
    let me = s.me.clone();
    push_msg(s, Msg { ts, chan: format!("+{gid}"), from: me, text: text.into(), id });
    s.dirty = true;
    save_private(s);
    Ok(())
}

/// `/dm <login> <text>`: a private message, readable only by the two of you.
pub fn dm_send(to: &str, text: &str) -> Result<String, String> {
    let to = to.trim_start_matches('@').to_string();
    cur(|s| {
        dm_locked(s, &to, text)?;
        let online = s.peers.get(&to).is_some_and(|p| p.online);
        Ok(format!("→ @{to} (private){}", if online { "" } else { ": they're offline; it reaches them when they're back" }))
    }).ok_or("team mode is off".to_string())?
}

/// `/team group <name> <login>…`
pub fn group_create(name: &str, logins: &[String]) -> Result<String, String> {
    cur(|s| group_locked(s, name, logins)).ok_or("team mode is off".to_string())?
}

/// Our groups: (name, members).
pub fn groups() -> Vec<(String, Vec<String>)> {
    let mut v: Vec<(String, Vec<String>)> = cur(|s| s.groups.values().map(|g| (g.name.clone(), g.members.clone())).collect()).unwrap_or_default();
    v.sort();
    v
}

/// A chat channel in the team room.
#[derive(Clone, Debug)]
pub struct Channel {
    pub id: String,    // "#team", "@login" or "+<group id>"
    pub label: String,
    pub unread: usize,
    pub pinged: bool,  // an unread line mentions you
    pub online: bool,  // a DM: they're online (lines still reach them later if not)
}

fn channels_locked(s: &State) -> Vec<Channel> {
    let mut out = vec![Channel { id: "#team".into(), label: "# team".into(), unread: s.unread, pinged: s.pinged.iter().any(|c| c == "#team"), online: true }];
    let mut peers: Vec<&Peer> = s.peers.values().filter(|p| p.joined && p.login != s.me).collect();
    peers.sort_by_key(|p| (!p.online, p.login.clone()));
    for p in peers {
        let id = format!("@{}", p.login);
        out.push(Channel { label: id.clone(), unread: s.chan_unread.get(&id).copied().unwrap_or(0), pinged: s.pinged.contains(&id), online: p.online, id });
    }
    let mut gs: Vec<(&String, &Group)> = s.groups.iter().collect();
    gs.sort_by_key(|(_, g)| g.name.to_lowercase());
    for (gid, g) in gs {
        let id = format!("+{gid}");
        out.push(Channel { label: format!("+ {}", g.name), unread: s.chan_unread.get(&id).copied().unwrap_or(0), pinged: s.pinged.contains(&id), online: true, id });
    }
    out
}

pub fn channels() -> Vec<Channel> {
    cur(|s| channels_locked(s)).unwrap_or_default()
}

/// A channel's lines, oldest first: (when, who, text, mentions you); "you" for your own.
pub fn channel_msgs(chan: &str) -> Vec<(String, String, String, bool)> {
    cur(|s| {
        let who = |f: &String| if *f == s.me { "you".to_string() } else { f.clone() };
        if chan == "#team" {
            return s.chat.iter().map(|(t, f, x)| (ago(*t), who(f), plain(x), *f != s.me && pings(x, &s.me))).collect();
        }
        s.msgs.iter().filter(|m| m.chan == chan).map(|m| (ago(m.ts), who(&m.from), plain(&m.text), m.from != s.me && pings(&m.text, &s.me))).collect()
    }).unwrap_or_default()
}

/// Who's in a channel, for its header: the group's members, or the DM's two people.
pub fn channel_members(chan: &str) -> Vec<String> {
    cur(|s| match chan.strip_prefix('+') {
        Some(gid) => s.groups.get(gid).map(|g| g.members.clone()).unwrap_or_default(),
        None => chan.strip_prefix('@').map(|l| vec![s.me.clone(), l.to_string()]).unwrap_or_default(),
    }).unwrap_or_default()
}

pub fn channel_read(chan: &str) {
    cur(|s| read_locked(s, chan));
}

fn read_locked(s: &mut State, chan: &str) {
    if chan == "#team" { s.unread = 0 } else { s.chan_unread.remove(chan); }
    s.pinged.retain(|c| c != chan);
}

/// Write in a channel: the team chat, a DM or a group. To people only: nothing runs.
pub fn channel_send(chan: &str, text: &str) -> Result<String, String> {
    if chan == "#team" { return chat_send(text).map(|_| String::new()) }
    if let Some(login) = chan.strip_prefix('@') { return dm_send(login, text) }
    let gid = chan.strip_prefix('+').ok_or("no such channel")?.to_string();
    cur(|s| group_send_locked(s, &gid, text).map(|_| String::new())).ok_or("team mode is off".to_string())?
}

static ALERTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Something worth a desktop notification (the UI decides whether to show it: focus, `notify`).
fn alert(text: String) {
    let mut a = ALERTS.lock().unwrap();
    a.push(text);
    let n = a.len();
    if n > 20 { a.drain(..n - 20); }
}

/// The notifications waiting to be shown, taken.
pub fn take_alerts() -> Vec<String> {
    std::mem::take(&mut *ALERTS.lock().unwrap())
}

/// One teammate's folder, read from git or sent live over the link: presence, newcomers, chat,
/// notes, and messages to our sessions.
fn take_in(repo: &str, me: &String, key: Option<Key>, mut p: Peer, hooks: &Hooks, new_chat: &mut bool) {
    let login = &p.login.clone();
    let mut g = STATES.lock().unwrap();
    let Some(s) = g.get_or_insert_with(HashMap::new).get_mut(repo) else { return };
    s.key = key;
    // a git copy older than what came over the link: keep the live data, take only what git alone carries
    if let Some(old) = s.peers.get(login).filter(|o| o.seq > p.seq) {
        let mut m = old.clone();
        m.ts = m.ts.max(p.ts);
        m.online = m.online || p.online;
        m.pub_key = std::mem::take(&mut p.pub_key);
        m.wrapped = std::mem::take(&mut p.wrapped);
        m.invites = std::mem::take(&mut p.invites);
        if !p.transcripts.is_empty() { m.transcripts = std::mem::take(&mut p.transcripts) }
        p = m;
    }
    let first = !s.peers.contains_key(login);
    let was = s.peers.get(login).is_some_and(|x| x.online);
    // someone who posted a key but has no team data yet: a newcomer. Let them in only if
    // approved (invited, or "let in"); otherwise ask.
    let newcomer = s.key.is_some() && !p.pub_key.is_empty() && !p.joined && !p.locked && !s.removed.contains(login);
    if newcomer {
        if s.approved.contains(login) {
            if first { log_to(repo, &format!("letting {login} in (you invited them)")); s.dirty = true }
        } else if !s.asking.contains(login) {
            s.asking.push(login.clone());
            drop(g);
            say(repo, hooks, format!("{login} wants to join the team: Ctrl+T to let them in"));
            g = STATES.lock().unwrap();
        }
    }
    let Some(s) = g.get_or_insert_with(HashMap::new).get_mut(repo) else { return };
    if p.joined && s.invites.remove(login).is_some() {
        // they're in: their invite code has done its job
        save_json("team-invites", repo, &json!(s.invites));
        s.dirty = true;
    }
    if p.online && !was && p.joined { say_locked(repo, s, hooks, format!("{login} is online")) }
    if !p.online && was { say_locked(repo, s, hooks, format!("{login} went offline")) }
    if first && !p.pub_key.is_empty() { s.dirty = true } // a new member to wrap the key for
    // team chat: lines we haven't taken in yet
    let upto = s.chat_seen.get(login).copied().unwrap_or(0);
    let mut fresh: Vec<&(u64, u64, String)> = p.chat.iter().filter(|(id, _, _)| *id > upto).collect();
    fresh.sort();
    let seen_before = s.chat_seen.contains_key(login);
    for (id, ts, text) in fresh {
        if !s.chat.iter().any(|(t, f, x)| t == ts && f == login && x == text) {
            s.chat.push((*ts, login.clone(), text.clone()));
            if seen_before {
                s.unread += 1;
                *new_chat = true;
                if pings(text, me) {
                    if !s.pinged.iter().any(|c| c == "#team") { s.pinged.push("#team".into()) }
                    alert(format!("qh: {login} mentioned you: {}", clip_to(text, 80)));
                }
            }
        }
        s.chat_seen.insert(login.clone(), *id);
    }
    s.chat_seen.entry(login.clone()).or_insert(0);
    // direct messages and group lines only we (and the group) can open
    if take_private(s, login, &p) { *new_chat = true }
    // change notes: what they changed, for our agents (the first read only catches up)
    let upto = s.notes_seen.get(login).copied();
    for (id, text, file, names) in p.notes.iter().filter(|n| upto.is_none_or(|u| n.0 > u)) {
        if upto.is_some() {
            push_note(repo, text, file, names);
            log_to(repo, &format!("note: {}", text.lines().next().unwrap_or("")));
        }
        s.notes_seen.insert(login.clone(), (*id).max(s.notes_seen.get(login).copied().unwrap_or(0)));
    }
    s.notes_seen.entry(login.clone()).or_insert(0);
    // their acks let us drop what they already handled
    let acked = p.acks.get(me).copied().unwrap_or(0);
    let before = s.outbox.len();
    s.outbox.retain(|m| !(m["to"] == login.as_str() && m["id"].as_u64().unwrap_or(0) <= acked));
    if s.outbox.len() != before { s.dirty = true }
    for m in p.outbox.iter().filter(|m| m["to"] == me.as_str()) {
        let id = m["id"].as_u64().unwrap_or(0);
        if id <= s.seen.get(login).copied().unwrap_or(0) || s.pending.iter().any(|x| x.from == *login && x.id == id) {
            continue;
        }
        let mut inc = Incoming { from: login.clone(), id, session: m["session"].as_str().unwrap_or("main").into(),
                                 text: m["text"].as_str().unwrap_or("").into(), question: None, ts: m["ts"].as_u64().unwrap_or_else(now),
                                 from_session: m["from_session"].as_str().unwrap_or("").into(), replied: false };
        let role = s.roles.get(login).map(String::as_str).unwrap_or("member");
        if m["kind"] == "a" {
            // an answer to one of our questions
            s.seen.insert(login.clone(), id);
            s.dirty = true;
            let re = m["re"].as_u64().unwrap_or(0);
            let Some(i) = s.asked.iter().position(|a| a.id == re) else { continue };
            let q = s.asked.remove(i);
            // what the answer relied on is now something our code counts on: a contract
            let relied: Vec<String> = m["relied"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
            let u = s.uses.entry(q.session.clone()).or_default();
            for n in &relied { u.insert(n.clone(), now()); }
            let from = format!("{login}/{}", m["from_session"].as_str().unwrap_or(""));
            if !relied.is_empty() {
                s.last_answer.insert(q.session.clone(), (from.clone(), relied.clone(), format!("Q: {} A: {}", q.q, inc.text)));
            }
            // the whole answer for you to read (the agent gets it in full either way); a very long one is cut with …
            let head = clip_to(&inc.text, 1500);
            alert(format!("qh: {from} answered: {}", clip_to(&head, 80)));
            if q.session.is_empty() {
                say_locked(repo, s, hooks, format!("{from} answered you: {head}"));
            } else {
                say_locked(repo, s, hooks, format!("{from} answered your {}: {head}", q.session));
                (hooks.answered)(&q.session, &format!("[team answer from {from}] You asked: {}\n\n{}{}", q.q, inc.text,
                    if relied.is_empty() { String::new() } else { format!("\n\n(Their side now knows you rely on: {}. If they change those, they're told first. \
If this answer matters a lot, the user can make it a checked promise with /promise.)", relied.join(", ")) }));
            }
            continue;
        }
        if m["kind"] == "q" {
            let q = Question { repo: repo.into(), from: login.clone(), from_session: m["from_session"].as_str().unwrap_or("").into(),
                               id, session: inc.session.clone(), text: inc.text.clone() };
            let who = if q.from_session.is_empty() { login.clone() } else { format!("{login}/{}", q.from_session) };
            log_to(repo, &format!("{who} asks your {}: {}", q.session, q.text));
            let now = now();
            s.answered.retain(|t| now.saturating_sub(*t) < 3600);
            let refuse = if role == "viewer" { Some("they have you as view only".to_string()) }
                else if s.answers == "off" { Some(format!("{me} has turned off questions from teammates' agents (/answers)")) }
                else if s.answered.len() >= ANSWERS_PER_HOUR { Some(format!("{me}'s agents have answered {ANSWERS_PER_HOUR} questions this hour, the limit")) }
                else { None };
            if let Some(why) = refuse {
                s.seen.insert(login.clone(), id);
                reply_locked(s, &q, &format!("(not answered: {why}. Use their fact sheet, or ask {login} directly.)"), &[]);
                say_locked(repo, s, hooks, format!("didn't answer {who}'s question: {why}"));
                continue;
            }
            if s.answers == "ask" && role != "trusted" {
                inc.text = format!("question: {}", q.text);
                inc.from_session = q.from_session.clone();
                inc.question = Some(q);
                alert(format!("qh: {who} asks your {}: {}", inc.session, clip_to(&inc.text, 80)));
                (hooks.ask)(&inc);
                s.pending.push(inc);
                continue;
            }
            s.seen.insert(login.clone(), id);
            s.answered.push(now);
            s.dirty = true;
            say_locked(repo, s, hooks, format!("{who} asked your {}: \"{}\" (answering from a read-only copy)", q.session, clip_to(&q.text, 300)));
            (hooks.answer)(q);
            continue;
        }
        // work for one of our sessions: it waits in Requests until we accept it, whatever their
        // role and whatever mode the session runs in (a "trusted" teammate's questions skip the
        // queue; their work never does)
        if role == "viewer" {
            say_locked(repo, s, hooks, format!("dropped {login}'s request to your {}: they're view only", inc.session));
            s.seen.insert(login.clone(), id);
            s.dirty = true;
        } else {
            log_to(repo, &format!("{} asks your {}: {}", inc.from, inc.session, inc.text));
            alert(format!("qh: {} asks: {}", inc.from, clip_to(bare(&inc.text), 80)));
            (hooks.ask)(&inc);
            s.pending.push(inc);
        }
    }
    // every member, even before their addresses are out: the beacon, a relay or our memory of them
    // may reach them first (and a hello from someone not registered here is dropped)
    let line = (p.joined && !p.pub_key.is_empty()).then(|| (p.pub_key.clone(), p.link.clone(), p.link_ms));
    s.peers.insert(login.clone(), p);
    drop(g);
    if let (Some((key, addrs, ms)), Some(l)) = (line, link()) {
        l.peer(&key, &addrs, ms);
    }
}

/// Nobody on the board holds a team key yet: the first member creates it. If someone already
/// did (their data is sealed, or they wrap keys), wait for them to wrap a copy for us.
fn settle_key(repo: &str) {
    with(repo, |s| {
        if s.key.is_some() {
            // Two people started on an empty board at the same moment and both made a key, so
            // each sees the other as noise. The later name gives up theirs (only a key made in
            // the last ten minutes that nobody else can read with) and asks to be let in.
            let fresh = std::fs::metadata(per_repo("team-keys", &s.repo)).and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).is_some_and(|e| e < Duration::from_secs(600));
            let alone = !s.peers.values().any(|p| p.joined);
            // after someone was removed, locked folders mean a key change, not a double start
            let rotated = s.epoch > 0 || s.peers.values().any(|p| p.epoch > 0);
            if fresh && alone && !rotated && s.peers.values().any(|p| p.locked && p.login < s.me) {
                let _ = std::fs::remove_file(per_repo("team-keys", &s.repo));
                s.key = None;
                s.dirty = true;
                log_to(&s.repo, "someone else set the team up at the same moment: asking them to let you in");
            }
            return;
        }
        if !s.peers.values().any(|p| !p.wrapped.is_empty() || p.locked) {
            let k = tc::new_team_key();
            save_key(&s.repo, &k);
            s.key = Some(k);
            s.dirty = true;
        }
    });
}

fn stale_check(repo: &str, hooks: &Hooks) {
    let gone: Vec<String> = with(repo, |s| s.peers.values_mut()
        .filter(|p| p.online && now().saturating_sub(p.ts) >= OFFLINE_AFTER)
        .map(|p| { p.online = false; p.login.clone() }).collect()).unwrap_or_default();
    for g in gone { say(repo, hooks, format!("{g} went offline")) }
}

fn save_seen(repo: &str) {
    with(repo, |s| save_json("team-seen", repo, &json!(s.seen)));
}

/// Ask a teammate's agent to do something in one of their sessions; it travels with our next
/// write and runs on their machine once they allow it.
pub fn send(to: &str, session: &str, text: &str) -> Result<String, String> {
    send_from(to, session, text, "")
}

/// `send` from one of our sessions (its agent used tell_team), so they see where it came from.
pub fn send_from(to: &str, session: &str, text: &str, from_session: &str) -> Result<String, String> {
    cur(|s| {
        if s.key.is_none() {
            return Err("you're not in the team yet: waiting to be let in".to_string());
        }
        let p = s.peers.get(to).ok_or_else(|| format!("no teammate {to}"))?;
        if !p.sessions.iter().any(|(n, _, _)| n == session) {
            return Err(format!("{to} has no session {session}"));
        }
        if p.roles.get(&s.me).is_some_and(|r| r == "viewer") {
            return Err(format!("{to} has you as view only: you can read their sessions but not ask their agent"));
        }
        let online = p.online;
        s.next_id += 1;
        let id = s.next_id;
        s.outbox.push(json!({"id": id, "to": to, "session": session, "text": text, "from_session": from_session, "ts": now()}));
        s.dirty = true;
        Ok((format!("you asked {to}'s {session}: {text}"),
            format!("asked {to}'s {session}: it waits in their Requests and runs on their machine only once they accept it{}",
                    if online { "" } else { " (they're offline, so it waits for them)" })))
    }).ok_or("team mode is off".to_string())?.map(|(log_line, msg)| { log(&log_line); msg })
}

/// Say something in the team chat (to people, not agents: nothing runs).
pub fn chat_send(text: &str) -> Result<(), String> {
    let repo = cur(|s| {
        let ts = now();
        let id = ts * 1000 + (s.my_chat.len() as u64 % 1000);
        s.my_chat.push(json!({"id": id, "ts": ts, "text": text}));
        let n = s.my_chat.len();
        if n > 30 { s.my_chat.drain(..n - 30); }
        s.chat.push((ts, s.me.clone(), text.to_string()));
        s.dirty = true;
        s.repo.clone()
    }).ok_or("team mode is off")?;
    with(&repo, |s| {
        let keep: Vec<_> = s.chat.iter().rev().take(300).rev().cloned().collect();
        save_json("team-chat", &repo, &json!(keep));
    });
    Ok(())
}

/// Unread lines in every channel: the team chat, DMs and groups.
pub fn chat_unread() -> usize {
    cur(|s| unread_locked(s)).unwrap_or(0)
}

fn unread_locked(s: &State) -> usize {
    s.unread + s.chan_unread.values().sum::<usize>()
}

/// Newcomers waiting for "let in".
pub fn join_requests() -> Vec<String> {
    cur(|s| s.asking.clone()).unwrap_or_default()
}

pub fn let_in(login: &str) {
    if let Some(repo) = repo() {
        approve(&repo, login);
        log(&format!("you let {login} in"));
    }
}

pub fn not_now(login: &str) {
    cur(|s| s.asking.retain(|x| x != login));
    log(&format!("you didn't let {login} in"));
}

/// A request's text without the "[from bob's agent] " tag tell_team puts on it.
fn bare(text: &str) -> &str {
    text.strip_prefix('[').and_then(|t| t.split_once("] ")).map_or(text, |x| x.1)
}

/// What an accepted request says in our session.
fn accepted_text(inc: &Incoming) -> String {
    format!("[accepted request from {}]: {}", inc.from, inc.text)
}

/// What the requester reads when we decline.
fn declined_text(me: &str, inc: &Incoming) -> String {
    format!("{me} declined: \"{}\"", clip_to(bare(&inc.text), 120))
}

/// What the requester reads when we write back without starting anything.
fn replied_text(me: &str, inc: &Incoming, text: &str) -> String {
    format!("{me} re your request \"{}\": {text}", clip_to(bare(&inc.text), 80))
}

/// Take a waiting request off the list and mark it handled. A declined request for work tells
/// the requester (privately); a declined question gets the usual "not answered".
fn decide_locked(s: &mut State, from: &str, id: u64, answer: &str) -> Option<Incoming> {
    let i = s.pending.iter().position(|x| x.from == from && x.id == id)?;
    let inc = s.pending.remove(i);
    let prev = s.seen.get(from).copied().unwrap_or(0);
    s.seen.insert(from.to_string(), id.max(prev));
    s.dirty = true;
    if answer == "deny" {
        match &inc.question {
            Some(q) => reply_locked(s, q, &format!("(not answered: {} said no. Use their fact sheet, or ask them directly.)", s.me), &[]),
            None => {
                let t = declined_text(&s.me, &inc);
                if let Err(e) = dm_locked(s, from, &t) { log_to(&s.repo.clone(), &format!("couldn't tell {from} you declined: {e}")) }
            }
        }
    }
    Some(inc)
}

/// Our answer to a waiting request: "once" (accept), "deny" (decline) or "always" (accept, and
/// let that person's agents' questions in without asking from now on; their work still waits for
/// you). Returns (session, text) to deliver for accepted work, ("", note) for a question.
pub fn decide(from: &str, id: u64, answer: &str) -> Option<(String, String)> {
    let (inc, repo) = cur(|s| decide_locked(s, from, id, answer).map(|i| (i, s.repo.clone()))).flatten()?;
    log(&format!("{} {}'s {}", match answer { "deny" => "declined", "always" => "always allow", _ => "accepted" }, inc.from,
                 if inc.question.is_some() { "question" } else { "request" }));
    if answer == "always" {
        let _ = set_role(&repo, &inc.from, "trusted");
    }
    save_seen(&repo);
    if answer == "deny" { return None }
    if let Some(q) = inc.question {
        // a question: answered from a read-only copy, nothing runs in the session itself
        let run = with(&repo, |s| { s.answered.push(now()); s.answerer.clone() }).flatten();
        let session = q.session.clone();
        return run.map(|f| { f(q); (String::new(), format!("your {session} is answering {}'s question (read-only)", inc.from)) });
    }
    Some((inc.session.clone(), accepted_text(&inc)))
}

/// Write back to a waiting request without starting any work. A question gets our words as its
/// answer (and is done); a request for work stays waiting, marked replied, until accepted or declined.
pub fn reply_request(from: &str, id: u64, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() { return Err("nothing to send".into()) }
    cur(|s| reply_request_locked(s, from, id, text)).ok_or("team mode is off".to_string())?
}

fn reply_request_locked(s: &mut State, from: &str, id: u64, text: &str) -> Result<String, String> {
    let i = s.pending.iter().position(|x| x.from == from && x.id == id).ok_or("that request isn't waiting any more")?;
    if let Some(q) = s.pending[i].question.clone() {
        decide_locked(s, from, id, "answered");
        let me = s.me.clone();
        reply_locked(s, &q, &format!("({me} answered this in person) {text}"), &[]);
        return Ok(format!("answered {from}'s question"));
    }
    let t = replied_text(&s.me, &s.pending[i], text);
    dm_locked(s, from, &t)?;
    s.pending[i].replied = true;
    Ok(format!("→ @{from} (private); the request still waits: a accept · d decline"))
}

// ---- behaviour contracts: promises about what a function does, checked by a model ----
// Structure contracts (names, fields) are exact and free; behaviour ("rate is a fraction") needs a
// model to judge, so it's opt-in: only promises someone made with /promise are checked.

fn promise_of(v: &Value) -> Promise {
    Promise { session: v["session"].as_str().unwrap_or("").into(), name: v["name"].as_str().unwrap_or("").into(),
              text: v["text"].as_str().unwrap_or("").into(), ts: v["ts"].as_u64().unwrap_or(0) }
}

fn promises_json(p: &[Promise]) -> Value {
    p.iter().map(|x| json!({"session": x.session, "name": x.name, "text": x.text, "ts": x.ts})).collect::<Vec<_>>().into()
}

/// `/promise` (the tab's last teammate answer becomes promises), `/promise <name> <what stays
/// true>`, `/promise drop <name>`, `/promise list`.
pub fn promise(session: &str, arg: &str) -> Result<String, String> {
    let arg = arg.trim();
    let out = cur(|s| -> Result<String, String> {
        let mine = |s: &State| s.promises.iter().filter(|p| p.session == session)
            .map(|p| format!("{}: {}", p.name, p.text)).collect::<Vec<_>>();
        let msg = if arg.is_empty() {
            let (who, names, text) = s.last_answer.remove(session)
                .ok_or("no teammate answer to make a promise from yet: /promise <function> <what must stay true>")?;
            let text: String = text.chars().take(400).collect();
            for n in &names {
                s.promises.retain(|p| !(p.session == session && p.name == *n));
                s.promises.push(Promise { session: session.into(), name: n.clone(), text: text.clone(), ts: now() });
            }
            format!("promise made: whoever changes {} gets their edit checked against {who}'s answer first", names.join(", "))
        } else if arg == "list" {
            let l = mine(s);
            if l.is_empty() { "this tab holds no promises · /promise <function> <what must stay true>".into() } else { l.join("\n") }
        } else if let Some(n) = arg.strip_prefix("drop ") {
            let before = s.promises.len();
            s.promises.retain(|p| !(p.session == session && p.name == n.trim()));
            if s.promises.len() == before { return Err(format!("no promise on {} in this tab", n.trim())) }
            format!("dropped the promise on {}", n.trim())
        } else {
            let (name, text) = arg.split_once(' ').ok_or("usage: /promise <function> <what must stay true>")?;
            s.promises.retain(|p| !(p.session == session && p.name == name));
            s.promises.push(Promise { session: session.into(), name: name.into(), text: text.trim().chars().take(400).collect(), ts: now() });
            format!("promise made: whoever changes {name} gets the edit checked against \"{}\" first", text.trim())
        };
        let n = s.promises.len();
        if n > 50 { s.promises.drain(..n - 50); }
        s.dirty = true;
        save_json("team-promises", &s.repo, &promises_json(&s.promises));
        Ok(msg)
    }).ok_or("team mode is off")??;
    log(&out);
    Ok(out)
}

/// Teammates' promises on the functions an edit changes: (login/session, name, what stays true).
fn promises_on(t: &Touched) -> Vec<(String, String, String)> {
    with(&t.repo, |s| s.peers.values().filter(|p| p.joined).flat_map(|p| p.promises.iter()
        .filter(|x| t.funcs.contains(&x.name)).map(|x| (format!("{}/{}", p.login, x.session), x.name.clone(), x.text.clone()))
        .collect::<Vec<_>>()).collect()).unwrap_or_default()
}

/// Before an edit: the promises it could break, with the function before and after, for the model
/// to judge. Empty when it touches no promised function or the user said /anyway.
pub fn promises_at_risk(session: &str, t: &Touched) -> Vec<(String, String, String, String, String)> {
    if ANYWAY.lock().unwrap().as_ref().and_then(|m| m.get(session)).is_some_and(|until| *until > now()) { return vec![] }
    promises_on(t).into_iter().filter_map(|(who, name, text)| {
        let (_, before, after) = t.bodies.iter().find(|b| b.0 == name)?;
        (before != after).then(|| (who, name, text, before.clone(), after.clone()))
    }).collect()
}

/// A model judged the edit breaks a teammate's promise: hold it, like a structure contract.
pub fn promise_held(t: &Touched, who: &str, name: &str, text: &str, why: &str) -> (String, String) {
    log_to(&t.repo, &format!("an edit would break what {who} counts on: {name} ({why})"));
    (format!("held: this edit changes what {who} counts on in {name}: {why} · keep it or tell them · /anyway goes ahead"),
     format!("error: held, your edit was NOT made. {who} (a teammate) counts on this about {name}: \"{text}\". A check of your change says it \
breaks that: {why}. Either change it so that still holds, or stop and tell the user so they can agree it with {who}. If the user wants \
it anyway, they type /anyway; the teammate's agent is then told to re-check."))
}

/// Promises a session holds (login "" = one of ours), for the map: (function, what stays true).
pub fn promises_of(login: &str, session: &str) -> Vec<(String, String)> {
    cur(|s| {
        let list = if login.is_empty() { &s.promises } else { match s.peers.get(login) { Some(p) => &p.promises, None => return vec![] } };
        list.iter().filter(|p| p.session == session).map(|p| (p.name.clone(), p.text.clone())).collect()
    }).unwrap_or_default()
}

// ---- asking teammates' agents: fact sheet first (the asker pays), then their agent (they pay) ----

/// Put an answer to a teammate's question in our outbox. `relied` = names the answer counts on.
fn reply_locked(s: &mut State, q: &Question, text: &str, relied: &[String]) {
    s.next_id += 1;
    s.outbox.push(json!({"id": s.next_id, "to": q.from, "session": q.from_session, "kind": "a", "re": q.id,
                         "from_session": q.session, "text": text, "relied": relied, "ts": now()}));
    s.dirty = true;
}

/// Our agent's answer to a teammate's question is ready.
pub fn reply(q: &Question, text: &str, relied: &[String]) {
    let who = if q.from_session.is_empty() { q.from.clone() } else { format!("{}/{}", q.from, q.from_session) };
    log_to(&q.repo, &format!("your {} → {who}: {}", q.session, text.chars().take(300).collect::<String>()));
    with(&q.repo, |s| reply_locked(s, q, text, relied));
}

/// Whether teammates' agents may ask ours: auto (answer, within the hourly budget), ask (you
/// allow each one), off.
pub fn set_answers(mode: &str) -> Result<String, String> {
    if !["auto", "ask", "off"].contains(&mode) { return Err("usage: /answers auto|ask|off".into()) }
    let repo = cur(|s| { s.answers = mode.into(); s.repo.clone() }).ok_or("team mode is off")?;
    let _ = std::fs::create_dir_all(crate::config::state_dir().join("team-answers"));
    let _ = std::fs::write(per_repo("team-answers", &repo), mode);
    Ok(match mode {
        "auto" => format!("teammates' agents can ask yours; up to {ANSWERS_PER_HOUR} answers an hour, from a read-only copy"),
        "ask" => "you'll be asked before your agents answer a teammate's question".into(),
        _ => "teammates' agents can't ask yours; they still see your fact sheet".into(),
    })
}

pub fn answers_mode() -> Option<String> {
    cur(|s| s.answers.clone())
}

/// After a session finishes a task: what it decided and what comes next, for its fact sheet.
pub fn set_sheet(session: &str, cwd: &Path, decided: &str, next: &str) {
    let Some((_, repo)) = repo_of(cwd) else { return };
    with(&repo, |s| {
        s.sheets.insert(session.into(), Sheet { decided: decided.chars().take(200).collect(), next: next.chars().take(200).collect(), ts: now() });
        s.dirty = true;
    });
}

/// The decided/next lines of a session's fact sheet; login "" = one of ours.
pub fn sheet_of(login: &str, session: &str) -> Option<Sheet> {
    cur(|s| if login.is_empty() { s.sheets.get(session).cloned() } else { s.peers.get(login).and_then(|p| p.sheets.get(session).cloned()) }).flatten()
}

/// Whether this folder's project is connected (sessions there get team tools and notes).
pub fn running_here(cwd: &Path) -> bool {
    repo_of(cwd).is_some_and(|(_, r)| running_for(&r))
}

/// A teammate session's fact sheet: what it's doing, changing, using, decided and does next.
fn sheet_text(p: &Peer, session: &str) -> String {
    let d = p.details.get(session).cloned().unwrap_or_default();
    let mut out = vec![format!("{}/{session} · {}", p.login, if p.online { "online".to_string() } else { format!("offline, last seen {}", ago(p.ts)) })];
    let role = p.sessions.iter().find(|(n, _, _)| n == session).map(|(_, _, r)| r.clone()).unwrap_or_default();
    let doing = [d.topic.as_str(), role.as_str()].into_iter().find(|x| !x.is_empty()).unwrap_or("").to_string();
    if !doing.is_empty() { out.push(format!("doing: {doing}")) }
    if !d.now.is_empty() { out.push(format!("right now: {}", d.now)) }
    for (f, fns, ts) in claims_view(p.claims.get(session)).into_iter().take(8) {
        out.push(format!("changing: {f}{} ({})", if fns.is_empty() { String::new() } else { format!(" › {}", fns.join(", ")) }, ago(ts)));
    }
    let mut uses = uses_view(p.uses.get(session));
    uses.sort();
    if !uses.is_empty() { out.push(format!("its fresh code uses: {}", uses.iter().take(20).cloned().collect::<Vec<_>>().join(", "))) }
    if let Some(w) = p.waiting.iter().find(|w| w.session == session) { out.push(format!("waiting for {} ({} in {})", w.on, w.func, w.file)) }
    if let Some(sh) = p.sheets.get(session) {
        if !sh.decided.is_empty() { out.push(format!("decided: {} ({})", sh.decided, ago(sh.ts))) }
        if !sh.next.is_empty() { out.push(format!("next: {}", sh.next)) }
    }
    if !d.branch.is_empty() { out.push(format!("branch: {}", d.branch)) }
    for f in d.files.iter().take(6) { out.push(format!("touched: {f}")) }
    out.truncate(30);
    out.join("\n")
}

/// Who should answer a question: (login, session, why), best first. In order: whoever is
/// changing the code it names, then whoever's fresh code uses those names, then whoever last
/// committed the files it names.
fn route(s: &State, root: &Path, text: &str, to: &str) -> Vec<(String, String, String)> {
    static WORD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][\w./-]*[\w]").unwrap());
    let words: Vec<String> = WORD.find_iter(text).map(|w| w.as_str().to_string()).filter(|w| w.len() > 2).collect();
    let paths: Vec<&String> = words.iter().filter(|w| w.contains('.') || w.contains('/')).collect();
    let names: Vec<String> = words.iter().flat_map(|w| w.split(['.', '/'])).filter(|w| w.len() > 2).map(String::from).collect();
    let file_hit = |f: &str| paths.iter().any(|p| f == p.as_str() || f.ends_with(&format!("/{p}")) || p.ends_with(&format!("/{f}")));
    let (want_login, want_sess) = to.split_once('/').unwrap_or((to, ""));
    let now = now();
    let mut scored: Vec<(i64, String, String, String)> = vec![];
    for p in s.peers.values().filter(|p| p.joined && p.login != s.me) {
        if !want_login.is_empty() && p.login != want_login { continue }
        for (sess, _, _) in &p.sessions {
            if !want_sess.is_empty() && sess != want_sess { continue }
            let (mut score, mut why) = (0i64, vec![]);
            for (f, fns) in p.claims.get(sess).into_iter().flatten() {
                let hot: Vec<&String> = fns.keys().filter(|n| !n.is_empty() && names.contains(n)).collect();
                if !hot.is_empty() { score += 8; why.push(format!("changing {}", hot.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", "))) }
                else if file_hit(f) { score += 5; why.push(format!("changing {f}")) }
            }
            let used: Vec<&String> = p.uses.get(sess).into_iter().flatten()
                .filter(|(n, t)| names.contains(n) && now.saturating_sub(**t) < 3600).map(|(n, _)| n).collect();
            if !used.is_empty() { score += 3; why.push(format!("using {}", used.iter().take(3).map(|x| x.as_str()).collect::<Vec<_>>().join(", "))) }
            let topic = p.details.get(sess).map(|d| d.topic.to_lowercase()).unwrap_or_default();
            if names.iter().any(|n| n.len() > 3 && topic.contains(&n.to_lowercase())) { score += 1; why.push("working on it".into()) }
            if score > 0 || !want_login.is_empty() {
                // asked by name: the session busy with the most code first
                let busy = p.claims.get(sess).map(|c| c.len() as i64).unwrap_or(0);
                scored.push((score * 10 + busy + if p.online { 5 } else { 0 }, p.login.clone(), sess.clone(),
                             if why.is_empty() { "named".into() } else { why.join("; ") }));
            }
        }
    }
    if scored.is_empty() && want_login.is_empty() {
        // nobody's on it now: whoever last committed a file the question names
        for p in &paths {
            let Some(who) = git(root, &["log", "-1", "--format=%an%x09%ae", "--", p]).filter(|x| !x.trim().is_empty()) else { continue };
            let who = who.to_lowercase();
            if let Some(peer) = s.peers.values().find(|x| x.joined && x.login != s.me && who.contains(&x.login.to_lowercase())) {
                if let Some((sess, _, _)) = peer.sessions.first() {
                    scored.push((0, peer.login.clone(), sess.clone(), format!("last committed {p}")));
                    break;
                }
            }
        }
    }
    scored.sort_by_key(|x| std::cmp::Reverse(x.0));
    scored.into_iter().map(|(_, l, s, w)| (l, s, w)).collect()
}

/// The ask_team tool. Without `deep`: the fact sheet of whoever owns what the question is about
/// (free for them, a few tokens for us). With `deep`: the question goes to their agent, which
/// answers later from a read-only copy of its session; the answer arrives as a message.
pub fn ask_team(session: &str, cwd: &Path, question: &str, to: &str, deep: bool) -> String {
    let Some((root, repo)) = repo_of(cwd) else { return "error: this folder isn't a team project".into() };
    if !running_for(&repo) { return "error: team mode is off for this project".into() }
    let to = to.trim().trim_start_matches('@');
    with(&repo, |s| {
        if s.key.is_none() { return "error: you're not in the team yet: waiting to be let in".into() }
        let found = route(s, &root, question, to);
        if found.is_empty() {
            let mut team: Vec<String> = s.peers.values().filter(|p| p.joined && p.login != s.me).flat_map(|p| p.sessions.iter().map(|(n, _, r)| {
                let topic = p.details.get(n).map(|d| d.topic.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| r.clone());
                format!("{}/{n}{}{}", p.login, if topic.is_empty() { String::new() } else { format!(": {topic}") }, if p.online { "" } else { " (offline)" })
            }).collect::<Vec<_>>()).collect();
            team.sort();
            return if !to.is_empty() { format!("error: no teammate session {to}. Teammates: {}", team.join("; ")) }
                else if team.is_empty() { "error: nobody else is in this team yet".into() }
                else { format!("error: couldn't tell who owns that: nobody is changing or using what the question names. Name the file or function, or pass to=\"login/session\". Teammates: {}", team.join("; ")) };
        }
        if !deep {
            let mut out = vec![];
            for (login, sess, why) in found.iter().take(2) {
                let Some(p) = s.peers.get(login) else { continue };
                out.push(format!("fact sheet (picked: {why})\n{}", sheet_text(p, sess)));
            }
            log_to(&repo, &format!("your {session} read {}'s fact sheet", found.iter().take(2).map(|(l, s, _)| format!("{l}/{s}")).collect::<Vec<_>>().join(" and ")));
            let (l, ss, _) = &found[0];
            return format!("{}\n\nIf this answers your question, go on. If not, call ask_team again with deep=true and to=\"{l}/{ss}\": their agent answers from its live session (it takes a minute; the answer arrives as a message).",
                           out.join("\n\n"));
        }
        let (login, sess, why) = found[0].clone();
        let online = s.peers.get(&login).is_some_and(|p| p.online);
        if s.peers.get(&login).is_some_and(|p| p.roles.get(&s.me).is_some_and(|r| r == "viewer")) {
            return format!("error: {login} has you as view only: you can read their fact sheet but not ask their agent");
        }
        if s.asked.iter().filter(|a| a.session == session && now().saturating_sub(a.ts) < 600).count() >= 3 {
            return "error: you already have 3 questions out to teammates; wait for those answers first".into();
        }
        s.next_id += 1;
        let id = s.next_id;
        s.outbox.push(json!({"id": id, "to": login, "session": sess, "kind": "q", "from_session": session, "text": question, "ts": now()}));
        s.asked.push(Asked { id, session: session.into(), to: format!("{login}/{sess}"), q: question.chars().take(300).collect(), ts: now() });
        if s.asked.len() > 30 { s.asked.remove(0); }
        s.dirty = true;
        log_to(&repo, &format!("your {session} → {login}/{sess} ({why}): {question}"));
        format!("asked {login}/{sess} (picked: {why}){}. The answer arrives as a [team answer] message, usually within a minute. \
Don't wait: carry on with your best guess, and if the answer says otherwise, fix what you built on it.",
                if online { "" } else { "; they're offline, so it waits until they're back" })
    }).unwrap_or_else(|| "error: team mode is off for this project".into())
}

/// The person (not an agent) asks a teammate's session a question from the map; the answer shows
/// in the activity feed and as a notice.
pub fn ask_person_agent(to: &str, session: &str, question: &str) -> Result<String, String> {
    cur(|s| {
        if s.key.is_none() { return Err("you're not in the team yet".to_string()) }
        if !s.peers.get(to).is_some_and(|p| p.sessions.iter().any(|(n, _, _)| n == session)) { return Err(format!("{to} has no session {session}")) }
        s.next_id += 1;
        let id = s.next_id;
        s.outbox.push(json!({"id": id, "to": to, "session": session, "kind": "q", "from_session": "", "text": question, "ts": now()}));
        s.asked.push(Asked { id, session: String::new(), to: format!("{to}/{session}"), q: question.chars().take(300).collect(), ts: now() });
        s.dirty = true;
        log_to(&s.repo, &format!("you → {to}/{session}: {question}"));
        Ok(format!("asked {to}'s {session}; the answer shows here when it comes"))
    }).ok_or("team mode is off".to_string())?
}

static ANSWERS_IN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new()); // session, text

/// Hand an answer to a busy session: it gets it before its next model call.
pub fn queue_answer(session: &str, text: &str) {
    ANSWERS_IN.lock().unwrap().push((session.into(), text.into()));
}

pub fn answers_for(session: &str) -> Vec<String> {
    let mut a = ANSWERS_IN.lock().unwrap();
    let (mine, rest): (Vec<_>, Vec<_>) = a.drain(..).partition(|(s, _)| s == session);
    *a = rest;
    mine.into_iter().map(|(_, t)| t).collect()
}

/// Our questions still waiting for an answer, for the map: (session, to, question, age).
pub fn open_questions() -> Vec<(String, String, String, String)> {
    cur(|s| s.asked.iter().filter(|a| now().saturating_sub(a.ts) < 3600).map(|a| (a.session.clone(), a.to.clone(), a.q.clone(), ago(a.ts))).collect())
        .unwrap_or_default()
}

pub fn pending() -> Vec<Incoming> {
    cur(|s| s.pending.clone()).unwrap_or_default()
}

/// Things waiting in connected projects other than the one on screen: (repo, count).
pub fn waiting_elsewhere() -> Vec<(String, usize)> {
    let c = CURRENT.lock().unwrap().clone();
    STATES.lock().unwrap().get_or_insert_with(HashMap::new).values()
        .filter(|s| s.repo != c && s.running.load(Relaxed))
        .map(|s| (s.repo.clone(), s.pending.len() + s.asking.len() + unread_locked(s)))
        .filter(|(_, n)| *n > 0).collect()
}

/// Teammates for the /team menu: (login, online, "online" or "2h ago", session names, our role for them).
/// Your GitHub login in the running team.
pub fn me() -> Option<String> {
    cur(|s| s.me.clone())
}

pub fn people() -> Vec<(String, bool, String, Vec<String>, String)> {
    cur(|s| {
        let mut peers: Vec<&Peer> = s.peers.values().filter(|p| p.joined).collect();
        peers.sort_by_key(|p| (!p.online, p.login.clone()));
        peers.iter().map(|p| (
            p.login.clone(), p.online,
            format!("{}{}", if p.online { "online".to_string() } else { ago(p.ts) },
                if s.pins.get(&p.login).is_some_and(|k| k.2) { "" } else { ", key not confirmed by GitHub" }),
            p.sessions.iter().map(|(n, _, _)| n.clone()).collect(),
            s.roles.get(&p.login).cloned().unwrap_or_else(|| "member".into()),
        )).collect()
    }).unwrap_or_default()
}

/// One session in the overview.
#[derive(Clone, Debug, Default)]
pub struct SessionView {
    pub name: String,
    pub status: String,
    pub topic: String,
    pub now: String,
    pub files: Vec<String>,
    pub branch: String,
    pub edits: Vec<(String, Vec<String>, u64)>, // file, functions, latest edit: what it's changing lately
    pub waiting: Option<Wait>,                  // paused on a teammate
    pub uses: Vec<String>,                      // names its fresh code uses (last hour)
}

/// Everything the room shows about one teammate.
pub struct PersonView {
    pub login: String,
    pub online: bool,
    pub seen: String,
    pub sessions: Vec<SessionView>,
    pub view_only: bool,
}

pub fn overview() -> Vec<PersonView> {
    cur(|s| {
        let mut peers: Vec<&Peer> = s.peers.values().filter(|p| p.joined).collect();
        peers.sort_by_key(|p| (!p.online, p.login.clone()));
        peers.iter().map(|p| PersonView {
            login: p.login.clone(), online: p.online,
            seen: if p.online { "online".into() } else { ago(p.ts) },
            sessions: p.sessions.iter().map(|(n, st, role)| {
                let mut v = p.details.get(n).cloned().unwrap_or_default();
                v.name = n.clone();
                v.status = st.clone();
                if v.topic.is_empty() { v.topic = role.clone() }
                v.edits = claims_view(p.claims.get(n));
                v.waiting = p.waiting.iter().find(|w| w.session == *n).cloned();
                v.uses = uses_view(p.uses.get(n));
                v
            }).collect(),
            view_only: s.roles.get(&p.login).is_some_and(|r| r == "viewer"),
        }).collect()
    }).unwrap_or_default()
}

// ---- change notes: a short "fyi, I changed X" to teammates' agents ----
// Agents working in parallel break each other mostly by changing something the other relies on;
// a ~100-token note about the change prevents most of it (arXiv 2609.25396).

type NoteIn = (u64, String, String, String, Vec<String>); // seq, repo, text, file, names it changed
static NOTES_IN: Mutex<Vec<NoteIn>> = Mutex::new(Vec::new());
static NOTES_READ: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);   // session -> last seq given
static NOTE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static MY_NOTE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REPO_OF: Mutex<Option<HashMap<PathBuf, Option<(PathBuf, String)>>>> = Mutex::new(None);

/// github_repo, remembered per folder (it runs git; agents ask on every model call).
fn repo_of(cwd: &Path) -> Option<(PathBuf, String)> {
    let mut c = REPO_OF.lock().unwrap();
    c.get_or_insert_with(HashMap::new).entry(cwd.to_path_buf()).or_insert_with(|| github_repo(cwd)).clone()
}

fn push_note(repo: &str, text: &str, file: &str, names: &[String]) {
    let mut n = NOTES_IN.lock().unwrap();
    n.push((NOTE_SEQ.fetch_add(1, Relaxed), repo.to_string(), text.to_string(), file.to_string(), names.to_vec()));
    let len = n.len();
    if len > 200 { n.drain(..len - 200); }
}

/// Teammates' change notes a session hasn't seen yet, for its next model call. A session
/// starts from now: it isn't handed notes from before it asked the first time. `files` = what
/// the session has read or written (as its tools named them): a note about one of those, or
/// about a name its code uses, comes in full; the rest as one line, so a big team's notes don't
/// fill every context.
pub fn notes_for(session: &str, cwd: &Path, files: &[String]) -> Vec<String> {
    let Some((root, repo)) = repo_of(cwd) else { return vec![] };
    if !running_for(&repo) { return vec![] }
    let mine: Vec<String> = files.iter().map(|f| {
        let p = if Path::new(f).is_absolute() { PathBuf::from(f) } else { cwd.join(f) };
        p.strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| f.clone())
    }).collect();
    let uses: Vec<String> = with(&repo, |s| s.uses.get(session).map(|u| u.keys().cloned().collect()).unwrap_or_default()).unwrap_or_default();
    let n = NOTES_IN.lock().unwrap();
    let last = n.last().map(|x| x.0).unwrap_or(0);
    let mut read = NOTES_READ.lock().unwrap();
    let seen = read.get_or_insert_with(HashMap::new).entry(session.to_string()).or_insert(last);
    // several edits to one file since the last call: only the latest note about it
    let fresh: Vec<&NoteIn> = n.iter().filter(|x| x.0 > *seen && x.1 == repo).collect();
    let out = shape_notes(&fresh, &mine, &uses);
    *seen = last;
    out
}

/// Which notes come in full: all of them when there are only a couple of files; otherwise the
/// ones about files the session touched or names its code uses, and warnings. Only the latest
/// note per file.
fn shape_notes(fresh: &[&NoteIn], mine: &[String], uses: &[String]) -> Vec<String> {
    let mut out: Vec<(String, String)> = vec![];
    let few = fresh.iter().map(|x| &x.3).collect::<std::collections::HashSet<_>>().len() <= 2; // a couple cost little: all in full
    for (_, _, t, file, names) in fresh.iter().copied() {
        let full = few || mine.contains(file) || names.iter().any(|x| uses.contains(x)) || t.starts_with("BREAKING") || t.starts_with("CHECK");
        let text = if full { t.clone() } else {
            format!("{} (a file you haven't touched; read it if your work depends on it)", t.lines().find(|l| l.contains(" changed ")).unwrap_or(t))
        };
        out.retain(|(f, _)| f != file);
        out.push((file.clone(), text));
    }
    out.into_iter().map(|(_, t)| t).collect()
}

// ---- claims: who is changing which function right now ----
// Two agents editing the same function is the one case worth stopping for; anything broader
// (same file, reads) only gets a note. Broad claims serialize most work (arXiv 2608.00947).

/// The name a line defines, if it starts a function, method or type. Language-agnostic on
/// purpose, no parser: fn/def/class/struct/impl/func/function, JS arrow consts, indented methods.
fn def_name(l: &str) -> Option<String> {
    static RES: std::sync::LazyLock<Vec<regex::Regex>> = std::sync::LazyLock::new(|| [
        r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)",
        r"^\s*(?:(?:pub(?:\([^)]*\))?|export|default|async|static|private|public|protected|override|final|abstract|unsafe|const|extern(?:\s+\x22[^\x22]*\x22)?)\s+)*(?:fn|def|class|struct|enum|union|trait|function\*?|interface|module|object|record)\s+([A-Za-z_$][\w$]*)",
        r"^\s*(?:unsafe\s+)?impl(?:<[^>]*>)?\s+(?:[\w:<>, ]+?\s+for\s+)?([A-Za-z_]\w*)",
        r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][\w$]*\s*=>)",
        r"^\s+(?:(?:public|private|protected|static|async|override|virtual|get|set)\s+)*([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*(?::\s*[^{;]+)?\{\s*$",
    ].iter().map(|r| regex::Regex::new(r).unwrap()).collect());
    let n = RES.iter().find_map(|r| r.captures(l)).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())?;
    (!["if", "for", "while", "switch", "catch", "match", "return", "else", "do", "try", "with", "loop"].contains(&n.as_str())).then_some(n)
}

/// For every line of `text`, the function or type it belongs to: the nearest enclosing
/// definition by indentation (None = top-level code).
fn owners(text: &str) -> Vec<Option<String>> {
    let mut stack: Vec<(usize, String)> = vec![];
    text.lines().map(|l| {
        if l.trim().is_empty() { return stack.last().map(|x| x.1.clone()) }
        let k = l.len() - l.trim_start().len();
        if let Some(n) = def_name(l) {
            while stack.last().is_some_and(|x| x.0 >= k) { stack.pop(); }
            stack.push((k, n.clone()));
            return Some(n);
        }
        // deeper definitions are over; one at the same depth too, unless this line closes it
        // (`}` / `end`): Python's top-level code after a def isn't part of the def
        let t = l.trim_start();
        let closes = t.starts_with(['}', ')', ']']) || t == "end" || t.starts_with("end ");
        while stack.last().is_some_and(|x| x.0 > k || (x.0 == k && !closes)) { stack.pop(); }
        stack.last().map(|x| x.1.clone())
    }).collect()
}

/// The lines that differ between two versions: (removed lines of `a`, added lines of `b`).
/// The common start and end are cut first, so a typical edit is exact and cheap; the middle
/// gets a real LCS diff unless it's huge, then it all counts as changed.
fn changed_lines(a: &[&str], b: &[&str]) -> (Vec<usize>, Vec<usize>) {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (am.len(), bm.len());
    if n == 0 || m == 0 || n * m > 1_000_000 {
        return ((pre..pre + n).collect(), (pre..pre + m).collect());
    }
    let w = m + 1;
    let mut t = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * w + j] = if am[i] == bm[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
        }
    }
    let (mut i, mut j, mut del, mut add) = (0, 0, vec![], vec![]);
    while i < n && j < m {
        if am[i] == bm[j] { i += 1; j += 1 }
        else if t[(i + 1) * w + j] >= t[i * w + j + 1] { del.push(pre + i); i += 1 }
        else { add.push(pre + j); j += 1 }
    }
    del.extend((i..n).map(|x| pre + x));
    add.extend((j..m).map(|x| pre + x));
    (del, add)
}

/// What an edit is about to change, in a team project: the file (relative to the project) and
/// the functions in it.
#[derive(Clone, Debug)]
pub struct Touched {
    repo: String,
    root: PathBuf,
    pub file: String,
    pub funcs: Vec<String>,
    removed: Vec<String>, // names it defined that the edit takes away (deleted or renamed)
    uses: Vec<String>,    // names the added lines use
    pub bodies: Vec<(String, String, String)>, // changed function: (name, text before, text after)
}

/// A field declared on this line (`name: String`, `pub name: T`, `name?: string`, `self.name =`).
fn field_name(l: &str) -> Option<String> {
    static F: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(
        r"^\s+(?:(?:pub(?:\([^)]*\))?|readonly|public|private|protected)\s+)*([A-Za-z_]\w*)\??\s*:\s*[A-Za-z_\[({<&'*]|^\s+self\.([A-Za-z_]\w*)\s*(?::[^=]+)?=[^=]").unwrap());
    F.captures(l).and_then(|c| c.get(1).or(c.get(2))).map(|m| m.as_str().to_string())
}

/// Names other code can rely on that these lines define: functions, types, fields.
fn defines(lines: &[&str], which: &[usize]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for &i in which {
        let Some(l) = lines.get(i) else { continue };
        if let Some(n) = def_name(l).or_else(|| field_name(l)) {
            if n.len() >= 3 && !out.contains(&n) { out.push(n) }
        }
    }
    out
}

/// The names a few lines of code use (identifiers outside strings and comments, minus keywords).
fn used_names(lines: &[&str], which: &[usize]) -> Vec<String> {
    static ID: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"[A-Za-z_][A-Za-z0-9_]{2,}").unwrap());
    static STR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`[^`]*`|//.*$|#.*$"#).unwrap());
    const KW: &[&str] = &["def", "class", "return", "import", "from", "self", "None", "True", "False", "for", "while", "and", "not",
        "pub", "let", "mut", "impl", "struct", "enum", "use", "mod", "crate", "Self", "match", "async", "await", "const", "static",
        "function", "var", "new", "this", "null", "undefined", "true", "false", "export", "default", "interface", "type", "else",
        "elif", "pass", "lambda", "yield", "with", "try", "except", "finally", "raise", "catch", "throw", "func", "package", "string",
        "int", "bool", "str", "String", "Vec", "Option", "Some", "Ok", "Err", "Result", "print", "println", "len", "map"];
    let mut out: Vec<String> = vec![];
    for &i in which {
        let Some(l) = lines.get(i) else { continue };
        let code = STR.replace_all(l, " ");
        for m in ID.find_iter(&code) {
            let n = m.as_str().to_string();
            if !KW.contains(&n.as_str()) && !out.contains(&n) { out.push(n) }
        }
        if out.len() >= 40 { break }
    }
    out
}

/// Work out what writing `after` to `full` changes. None outside a connected team project.
pub fn touched(cwd: &Path, full: &Path, after: &str) -> Option<Touched> {
    let (root, repo) = repo_of(cwd)?;
    if !running_for(&repo) { return None }
    let file = full.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
    let before = std::fs::read_to_string(full).unwrap_or_default();
    let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let (del, add) = changed_lines(&a, &b);
    let (oa, ob) = (owners(&before), owners(after));
    let mut funcs: Vec<String> = vec![];
    // blank lines say nothing: one added between two functions isn't a change to the one above
    let real = |lines: &[&str], i: usize| lines.get(i).is_some_and(|l| !l.trim().is_empty());
    for n in del.iter().filter(|&&i| real(&a, i)).filter_map(|&i| oa.get(i).cloned().flatten())
        .chain(add.iter().filter(|&&i| real(&b, i)).filter_map(|&i| ob.get(i).cloned().flatten())) {
        if !funcs.contains(&n) { funcs.push(n) }
    }
    // what other code may rely on: names it defined that are gone afterwards
    let kept = defines(&b, &(0..b.len()).collect::<Vec<_>>());
    let removed = defines(&a, &del).into_iter().filter(|n| !kept.contains(n)).collect();
    let mine = defines(&b, &add);
    let uses = used_names(&b, &add).into_iter().filter(|n| !mine.contains(n)).collect();
    // each changed function's whole text, before and after, for checking behaviour promises
    let body = |lines: &[&str], own: &[Option<String>], f: &str| lines.iter().zip(own)
        .filter(|(_, o)| o.as_deref() == Some(f)).take(120).map(|(l, _)| *l).collect::<Vec<_>>().join("\n");
    let bodies = funcs.iter().map(|f| (f.clone(), body(&a, &oa, f), body(&b, &ob, f))).collect();
    Some(Touched { repo, root, file, funcs, removed, uses, bodies })
}

static ANYWAY: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None); // session -> allowed until

/// The user said "go ahead anyway": this session's edits skip teammates' claims for 10 minutes.
pub fn allow_anyway(session: &str) {
    ANYWAY.lock().unwrap().get_or_insert_with(HashMap::new).insert(session.to_string(), now() + CLAIM_HOT);
}

/// Before an edit: is a teammate changing one of these functions right now? Then the edit
/// doesn't happen, the session is told who has it and why, and it's woken when it's free.
/// None = go ahead.
pub fn check_edit(session: &str, t: &Touched) -> Option<(String, String)> {
    let now = now();
    if ANYWAY.lock().unwrap().as_ref().and_then(|m| m.get(session)).is_some_and(|until| *until > now) { return None }
    if t.funcs.is_empty() { return check_contract(t) }
    let hit = with(&t.repo, |s| {
        for p in s.peers.values().filter(|p| p.joined && p.online) {
            for (sess, files) in &p.claims {
                let Some(fns) = files.get(&t.file) else { continue };
                let Some((func, ts)) = t.funcs.iter().find_map(|f| fns.get(f).filter(|ts| now.saturating_sub(**ts) < CLAIM_HOT).map(|ts| (f.clone(), *ts))) else { continue };
                let why = p.details.get(sess).map(|d| d.topic.clone()).filter(|x| !x.is_empty())
                    .map(|x| format!(" (working on: {x})")).unwrap_or_default();
                if !s.waiting.iter().any(|w| w.session == session && w.file == t.file && w.func == func) {
                    s.waiting.push(Wait { session: session.into(), file: t.file.clone(), func: func.clone(), on: p.login.clone(), since: now });
                    s.dirty = true;
                }
                return Some((p.login.clone(), sess.clone(), func, ts, why));
            }
        }
        None
    }).flatten();
    let Some((who, sess, func, ts, why)) = hit else { return check_contract(t) };
    log_to(&t.repo, &format!("your {session} waits for {who} · {func} in {}", t.file));
    Some((format!("paused: {who}/{sess} is changing {func} in {}. You'll be told when it's free · /anyway goes ahead", t.file),
          format!("error: paused, your edit was NOT made. {who}/{sess} is changing {func} in {} right now (last edit {}){why}. \
Meanwhile work on something that doesn't touch {func}, or stop and tell the user you're waiting for {who}; you'll get a message the \
moment {func} is free. Don't get around this by editing it another way. If the user wants you to go ahead anyway, they type /anyway.",
        t.file, ago(ts))))
}

/// Teammates' fresh code that uses a name, within the last hour: (login, session) pairs.
fn users_of(s: &State, name: &str) -> Vec<(String, String)> {
    let now = now();
    s.peers.values().filter(|p| p.joined).flat_map(|p| p.uses.iter()
        .filter(|(_, u)| u.get(name).is_some_and(|ts| now.saturating_sub(*ts) < 3600))
        .map(|(sess, _)| (p.login.clone(), sess.clone()))).collect()
}

/// Contracts: the edit takes away a name (function, type, field) a teammate's fresh code uses.
/// Their copy doesn't have our change yet, so nothing on our side would catch the break.
fn check_contract(t: &Touched) -> Option<(String, String)> {
    let (name, users) = with(&t.repo, |s| t.removed.iter().find_map(|n| {
        let u = users_of(s, n);
        (!u.is_empty()).then(|| (n.clone(), u))
    })).flatten()?;
    let who: Vec<String> = users.iter().map(|(l, sess)| format!("{l}/{sess}")).collect();
    let who = who.join(", ");
    log_to(&t.repo, &format!("an edit would break {who}: it takes away {name} ({})", t.file));
    Some((format!("held: this edit takes away {name}, which {who} uses. Keep {name} or tell them · /anyway goes ahead"),
          format!("error: held, your edit was NOT made. It removes or renames {name} in {}, and {who} (a teammate) wrote code in the last hour that uses {name}; their copy doesn't have your change, so it would break when the work is merged. Either keep {name} working (for a rename: keep the old name too, delegating to the new one), or stop and tell the user so they can agree it with {who}. If the user wants it anyway, they type /anyway; the teammate's agent then gets told to update.", t.file)))
}

/// After a successful edit: these functions are now this session's for a while, and teammates'
/// agents hear what changed.
pub fn claim(session: &str, topic: &str, t: &Touched) {
    with(&t.repo, |s| {
        let now = now();
        let e = s.claims.entry(session.to_string()).or_default().entry(t.file.clone()).or_default();
        for f in &t.funcs { e.insert(f.clone(), now); }
        if t.funcs.is_empty() { e.insert(String::new(), now); } // outside any function: the file, for the map
        // it went ahead (allowed anyway): no longer waiting on that
        s.waiting.retain(|w| !(w.session == session && w.file == t.file && t.funcs.contains(&w.func)));
        let u = s.uses.entry(session.to_string()).or_default();
        for n in &t.uses { u.insert(n.clone(), now); }
        if u.len() > 200 {
            let mut v: Vec<(String, u64)> = u.drain().collect();
            v.sort_by_key(|x| std::cmp::Reverse(x.1));
            u.extend(v.into_iter().take(150));
        }
        s.dirty = true;
    });
    // went ahead (/anyway) and took away something a teammate uses: say so loudly in the note
    let broken: Vec<(String, Vec<(String, String)>)> = with(&t.repo, |s| t.removed.iter()
        .map(|n| (n.clone(), users_of(s, n))).filter(|(_, u)| !u.is_empty()).collect()).unwrap_or_default();
    let mut warn = broken.iter().map(|(n, u)| format!("BREAKING for {}: {n} was removed or renamed and your recent code uses it. Update it to match.",
        u.iter().map(|(l, sess)| format!("{l}/{sess}")).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>();
    // went ahead on a function someone holds a promise on: tell them to re-check what they count on
    for (who, name, text) in promises_on(t) {
        warn.push(format!("CHECK for {who}: {name} changed and you count on: \"{text}\". Make sure your code still fits."));
    }
    let warn = warn.join("\n");
    note_change(session, topic, t, &warn);
}

/// Tell teammates' agents what changed, with the changed lines (a few) so "name → fullName"
/// comes through. One note per file, refreshed in place while the session keeps editing it.
fn note_change(session: &str, topic: &str, t: &Touched, warn: &str) {
    let diff = git(&t.root, &["diff", "-U0", "--", &t.file]).unwrap_or_default();
    let lines: Vec<String> = diff.lines()
        .filter(|l| (l.starts_with('+') || l.starts_with('-')) && !l.starts_with("+++") && !l.starts_with("---") && l.trim().len() > 1)
        .take(8).map(|l| l.chars().take(100).collect()).collect();
    let file = &t.file;
    with(&t.repo, |s| {
        let what = if t.funcs.is_empty() { file.clone() } else { format!("{file} ({})", t.funcs.join(", ")) };
        let why = if topic.is_empty() { String::new() } else { format!(" — working on: {}", topic.chars().take(80).collect::<String>()) };
        let body = if lines.is_empty() { "(new or untracked file)".to_string() } else { lines.join("\n") };
        let text = format!("{}{}/{session} changed {what}{why}\n{body}", if warn.is_empty() { String::new() } else { format!("{warn}\n") }, s.me);
        let ts = now();
        s.my_notes.retain(|n| !(n["file"] == file.as_str() && n["session"] == session && ts.saturating_sub(n["ts"].as_u64().unwrap_or(0)) < 600));
        let id = MY_NOTE_ID.fetch_add(1, Relaxed).max(ts * 1000);
        MY_NOTE_ID.fetch_max(id + 1, Relaxed);
        let names: Vec<&String> = t.funcs.iter().chain(&t.removed).collect();
        s.my_notes.push(json!({"id": id, "ts": ts, "file": file, "session": session, "text": text, "names": names}));
        let n = s.my_notes.len();
        if n > 20 { s.my_notes.drain(..n - 20); }
        s.dirty = true;
    });
}

/// Every few seconds: let go of claims on what we pushed (or that went stale), and wake our
/// sessions whose function is free again.
fn wake_check(repo: &str, root: &Path, hooks: &Hooks) {
    static LAST_RELEASE: Mutex<Option<Instant>> = Mutex::new(None);
    let due = {
        let mut l = LAST_RELEASE.lock().unwrap();
        let due = l.is_none_or(|t| t.elapsed() >= Duration::from_secs(20));
        if due { *l = Some(Instant::now()) }
        due
    };
    let now = now();
    if due {
        // a file that matches what's upstream again was pushed (or put back): it's free
        let files: Vec<String> = with(repo, |s| s.claims.values().flat_map(|c| c.keys().cloned()).collect()).unwrap_or_default();
        let pushed: Vec<String> = files.into_iter().filter(|f| {
            git(root, &["diff", "--quiet", "@{upstream}", "--", f]).is_some()
                || git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_none() && git(root, &["diff", "--quiet", "HEAD", "--", f]).is_some()
        }).collect();
        with(repo, |s| {
            let before: usize = s.claims.values().map(|c| c.len()).sum();
            for c in s.claims.values_mut() {
                c.retain(|f, fns| { fns.retain(|_, ts| now.saturating_sub(*ts) < 3600); !pushed.contains(f) && !fns.is_empty() });
            }
            s.claims.retain(|_, c| !c.is_empty());
            if s.claims.values().map(|c| c.len()).sum::<usize>() != before { s.dirty = true }
        });
    }
    let woken: Vec<(Wait, &'static str)> = with(repo, |s| {
        let mut out = vec![];
        let peers = &s.peers;
        s.waiting.retain(|w| {
            let p = peers.get(&w.on);
            let held = p.filter(|p| p.online).is_some_and(|p| p.claims.values()
                .any(|c| c.get(&w.file).and_then(|f| f.get(&w.func)).is_some_and(|ts| now.saturating_sub(*ts) < CLAIM_HOT)));
            if held && now.saturating_sub(w.since) < 3600 { return true }
            let why = if !p.is_some_and(|p| p.online) { "went offline" }
                else if p.is_some_and(|p| p.claims.values().all(|c| !c.contains_key(&w.file))) { "pushed it" }
                else { "stopped editing it" };
            out.push((w.clone(), why));
            false
        });
        if !out.is_empty() { s.dirty = true }
        out
    }).unwrap_or_default();
    for (w, why) in woken {
        let pull = if why == "pushed it" { " They pushed it: run `git pull` first so you build on their version." }
                   else { " It isn't pushed yet, so their change is only in the team notes you got: build on what they describe so both fit together." };
        say(repo, hooks, format!("your {} can go on: {} {why}, {} is free", w.session, w.on, w.func));
        (hooks.deliver)(&w.session, &format!("[team] {} in {} is free: {} {why}.{pull} Then carry on with the edit you were paused on.", w.func, w.file, w.on));
    }
}

/// Our own sessions' claims and waits in the project on screen, for the map.
pub fn mine(session: &str) -> (Vec<(String, Vec<String>, u64)>, Option<Wait>, Vec<String>) {
    cur(|s| (claims_view(s.claims.get(session)), s.waiting.iter().find(|w| w.session == session).cloned(), uses_view(s.uses.get(session))))
        .unwrap_or_default()
}

/// Names a session's code used in the last hour.
fn uses_view(u: Option<&HashMap<String, u64>>) -> Vec<String> {
    let now = now();
    u.into_iter().flatten().filter(|(_, t)| now.saturating_sub(**t) < 3600).map(|(n, _)| n.clone()).collect()
}

/// A session's claims as (file, functions, latest edit), most recent first.
fn claims_view(c: Option<&Claims>) -> Vec<(String, Vec<String>, u64)> {
    let mut v: Vec<(String, Vec<String>, u64)> = c.into_iter().flatten().map(|(f, fns)| {
        let mut names: Vec<(&String, &u64)> = fns.iter().filter(|(n, _)| !n.is_empty()).collect();
        names.sort_by_key(|(_, t)| std::cmp::Reverse(**t));
        (f.clone(), names.into_iter().map(|(n, _)| n.clone()).collect(), fns.values().copied().max().unwrap_or(0))
    }).collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.2));
    v
}

static ACTIVITY: Mutex<Vec<(u64, String, String)>> = Mutex::new(Vec::new());

/// Something for a project's Activity tab (last 300 kept across projects).
pub fn log_to(repo: &str, text: &str) {
    let mut a = ACTIVITY.lock().unwrap();
    a.push((now(), repo.to_string(), text.to_string()));
    let n = a.len();
    if n > 300 { a.drain(..n - 300); }
}

pub fn log(text: &str) {
    let repo = CURRENT.lock().unwrap().clone();
    log_to(&repo, text)
}

/// The current project's activity, newest first: (when, text).
pub fn activity() -> Vec<(String, String)> {
    let repo = CURRENT.lock().unwrap().clone();
    ACTIVITY.lock().unwrap().iter().rev().filter(|(_, r, _)| *r == repo).map(|(t, _, x)| (ago(*t), x.clone())).collect()
}

fn say(repo: &str, hooks: &Hooks, text: String) {
    log_to(repo, &text);
    if *CURRENT.lock().unwrap() == repo { (hooks.say)(text) }
}

/// `say` for when the caller already holds the state lock (the current repo is a separate lock).
fn say_locked(repo: &str, _s: &mut State, hooks: &Hooks, text: String) {
    say(repo, hooks, text)
}

/// A teammate's session conversation, oldest first: (role, text).
pub fn transcript_rows(login: &str, session: &str) -> Vec<(String, String)> {
    cur(|s| s.peers.get(login).and_then(|p| p.transcripts.get(session)).cloned()).flatten().unwrap_or_default()
}

/// Teammates' sessions as (login, session, label), online people first.
pub fn sessions() -> Vec<(String, String, String)> {
    cur(|s| {
        let mut peers: Vec<&Peer> = s.peers.values().filter(|p| p.joined).collect();
        peers.sort_by_key(|p| (!p.online, p.login.clone()));
        peers.iter().flat_map(|p| p.sessions.iter().map(|(n, st, role)| (
            p.login.clone(), n.clone(),
            format!("{n}  {st}{}{}", if role.is_empty() { String::new() } else { format!(" · {role}") }, if p.online { "" } else { "  (offline)" }),
        ))).collect()
    }).unwrap_or_default()
}

pub fn transcript(login: &str, session: &str) -> Result<String, String> {
    let t = transcript_rows(login, session);
    if t.is_empty() { return Err(format!("no messages in {login} / {session} yet")) }
    let body: Vec<String> = t.iter().map(|(r, x)| format!("{} {x}", if r == "user" { "▸" } else { "◂" })).collect();
    Ok(format!("{login} / {session} · last {} messages · read-only\n{}", t.len(), body.join("\n")))
}

/// Disconnect the project on screen.
pub fn stop() {
    cur(|s| s.running.store(false, Relaxed));
}

/// Disconnect every project and wait (up to 5 s) for the "offline" writes, at quit.
pub fn stop_and_flush() {
    let flags: Vec<std::sync::Arc<AtomicBool>> = STATES.lock().unwrap().get_or_insert_with(HashMap::new).values()
        .filter(|s| s.running.swap(false, Relaxed)).map(|s| s.finished.clone()).collect();
    let t0 = Instant::now();
    while flags.iter().any(|f| !f.load(Relaxed)) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(Some(l)) = LINK.get() {
        l.stop(); // gives back the router port, if we asked for one
    }
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
    github_repo(cwd).is_some_and(|(_, r)| enabled(&r))
}

pub fn enabled(repo: &str) -> bool {
    std::fs::read_to_string(enabled_path()).unwrap_or_default().lines().any(|l| l == repo)
}

/// (online teammates, waiting items) for a connected project, for the project switcher.
pub fn glance(repo: &str) -> Option<(Vec<String>, usize)> {
    with(repo, |s| {
        if !s.running.load(Relaxed) { return None }
        Some((s.peers.values().filter(|p| p.online && p.joined).map(|p| p.login.clone()).collect(),
              s.pending.len() + s.asking.len() + unread_locked(s)))
    }).flatten()
}

pub fn ago(ts: u64) -> String {
    let d = now().saturating_sub(ts);
    match d { 0..60 => "just now".into(), 60..3600 => format!("{}m ago", d / 60), 3600..86400 => format!("{}h ago", d / 3600), _ => format!("{}d ago", d / 86400) }
}

/// "team: you · rahul ● · anu ○ 2h ago · 2 new messages · 1 waiting (Ctrl+T)" for the status bar, while team mode is on.
pub fn status_line() -> Option<String> {
    let mut out = cur(|s| {
        let mut peers: Vec<&Peer> = s.peers.values().filter(|p| p.joined).collect();
        peers.sort_by_key(|p| (!p.online, std::cmp::Reverse(p.ts)));
        let mut out = format!("team: {}", s.me);
        for p in peers.iter().take(4) {
            out += &if p.online { format!(" · {} ●", p.login) } else { format!(" · {} ○ {}", p.login, ago(p.ts)) };
        }
        if peers.len() > 4 { out += &format!(" · +{}", peers.len() - 4) }
        if s.key.is_none() { out += " · waiting to be let in" }
        let unread = unread_locked(s);
        if unread > 0 { out += &format!(" · {unread} unread{}", if s.pinged.is_empty() { "" } else { " (@you)" }) }
        let n = s.pending.len();
        if n > 0 { out += &format!(" · {n} request{} (Ctrl+T)", if n == 1 { "" } else { "s" }) }
        if !s.asking.is_empty() { out += &format!(" · {} waiting to join (Ctrl+T)", s.asking.len()) }
        if !s.error.is_empty() { out += " · sync problem" }
        out
    })?;
    for (r, n) in waiting_elsewhere() {
        out += &format!(" · {n} waiting in {}", r.rsplit('/').next().unwrap_or(&r));
    }
    Some(out)
}

/// Something in a connected team project waits for you: messages, join requests, asks.
pub fn needs_you() -> bool {
    STATES.lock().unwrap().as_ref().is_some_and(|m| m.values().any(|s| s.running.load(Relaxed)
        && (unread_locked(s) > 0 || !s.pending.is_empty() || !s.asking.is_empty())))
}

/// The `/team status` text.
pub fn report() -> String {
    let Some(r) = cur(|s| {
        let mut out = format!("team {} · you are {}\n", s.repo, s.me);
        if s.key.is_none() { out += "waiting to be let in\n" }
        let mut peers: Vec<&Peer> = s.peers.values().collect();
        peers.sort_by_key(|p| (!p.online, std::cmp::Reverse(p.ts)));
        for p in peers {
            out += &format!("{} {}  {}\n", if p.online { "●" } else { "○" }, p.login,
                            if !p.joined { "waiting to be let in".into() } else if p.online { "online".into() } else { format!("last seen {}", ago(p.ts)) });
            for (n, st, role) in &p.sessions {
                out += &format!("    {n}: {st}{}\n", if role.is_empty() { String::new() } else { format!(" · {role}") });
            }
        }
        if !s.error.is_empty() { out += &format!("last sync problem: {}\n", s.error) }
        out + "Ctrl+T opens the team room"
    }) else {
        return "team mode is off: /team to start, join or connect".into();
    };
    r
}

// ---- the direct line (qh-link): live copies of the same sealed data git carries ----

use crate::link::{Event as LinkEvent, Link};
use std::sync::Arc;

static LINK: std::sync::OnceLock<Option<Arc<Link>>> = std::sync::OnceLock::new();
static LIVE: Mutex<Option<HashMap<String, std::sync::mpsc::Sender<(String, Vec<u8>)>>>> = Mutex::new(None); // repo -> its team thread
static LINES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None); // their member key -> path ("lan …", "via <key>")

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// This machine's line, shared by every connected project, started on first use. QH_LINK=off
/// turns it off, =noupnp skips asking the router for a port, =local also skips STUN (one-machine demos).
fn link() -> Option<Arc<Link>> {
    LINK.get_or_init(|| {
        let mode = std::env::var("QH_LINK").unwrap_or_default();
        if mode == "off" {
            return None;
        }
        let o = crate::link::Options {
            stun: mode != "local", upnp: mode.is_empty(), beacon: mode != "local",
            // teammates met before, and where: after a restart they're found with no git round trip
            remember: Some(crate::config::state_dir().join("link-memory.json")),
            // the user's choice: relay = "default" | "none" | "host:port#secret" (QH_RELAY overrides)
            relays: crate::link::relay_specs(&std::env::var("QH_RELAY").unwrap_or_else(|_| crate::config::load().relay)),
            ..Default::default()
        };
        let (l, rx) = Link::start(tc::identity().0, o).ok()?;
        std::thread::Builder::new().name("qh-link-events".into()).spawn(move || {
            for e in rx {
                on_line(e)
            }
        }).ok()?;
        Some(l)
    }).clone()
}

/// The relay setting changed: the running line (if any) uses it at once.
pub fn set_relays(setting: &str) {
    if let Some(Some(l)) = LINK.get() {
        l.set_relays(crate::link::relay_specs(setting));
    }
}

fn on_line(e: LinkEvent) {
    match e {
        LinkEvent::Msg { from, data } => {
            let repo = serde_json::from_slice::<Value>(&data).ok().and_then(|v| v["repo"].as_str().map(String::from));
            let tx = repo.and_then(|r| LIVE.lock().unwrap().as_ref()?.get(&r).cloned());
            if let Some(tx) = tx {
                let _ = tx.send((from, data));
            }
        }
        LinkEvent::Up { peer, path } => {
            let was = LINES.lock().unwrap().get_or_insert_with(HashMap::new).insert(peer.clone(), path.clone());
            let (now, before) = (describe_line(&path), was.map(|w| describe_line(&w)));
            if before.as_deref() != Some(now.as_str()) {
                for (repo, login) in who_is(&peer) {
                    log_to(&repo, &format!("direct line to {login} ({now})"));
                }
            }
        }
        LinkEvent::Down { peer } => {
            LINES.lock().unwrap().get_or_insert_with(HashMap::new).remove(&peer);
            for (repo, login) in who_is(&peer) {
                log_to(&repo, &format!("line to {login} dropped: their updates come by git again"));
            }
        }
        LinkEvent::Delivered { .. } | LinkEvent::Failed { .. } => {} // git carries everything anyway
    }
}

/// Which teammate (in which project) a member key belongs to.
fn who_is(key: &str) -> Vec<(String, String)> {
    STATES.lock().unwrap().get_or_insert_with(HashMap::new).values()
        .flat_map(|s| s.peers.values().filter(|p| p.pub_key == key).map(|p| (s.repo.clone(), p.login.clone())).collect::<Vec<_>>())
        .collect()
}

/// "lan 192.168.1.4:7001" -> "lan"; "via <key>" -> "via carol".
fn describe_line(path: &str) -> String {
    match path.strip_prefix("via ") {
        Some(k) => format!("via {}", who_is(k).into_iter().next().map_or("a teammate".to_string(), |(_, l)| l)),
        None => path.split(' ').next().unwrap_or("").to_string(),
    }
}

/// How we reach `login` right now ("lan", "ipv6", "public", "via carol"); None = git only.
pub fn line_to(login: &str) -> Option<String> {
    let key = cur(|s| s.peers.get(login).map(|p| p.pub_key.clone())).flatten()?;
    let path = LINES.lock().unwrap().as_ref()?.get(&key)?.clone();
    Some(describe_line(&path))
}

/// What went over the line last, and to whom.
struct Live {
    last: Option<Value>,
    at: Instant,
    pulse: u64,
    told: Vec<String>, // member keys that have our latest
}

/// Changes worth sending at once (messages, answers, acks, notes, claims); the rest (status
/// words) waits up to a second so a busy session doesn't flood the line.
fn pulse(repo: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    with(repo, |s| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.outbox.len().hash(&mut h);
        s.outbox.last().map(|m| m["id"].as_u64()).hash(&mut h);
        let mut seen: Vec<_> = s.seen.iter().collect();
        seen.sort();
        seen.hash(&mut h);
        (s.my_notes.len(), s.my_chat.len(), s.waiting.len(), s.promises.len()).hash(&mut h);
        s.claims.values().map(|c| c.values().map(|f| f.len()).sum::<usize>()).sum::<usize>().hash(&mut h);
        h.finish()
    }).unwrap_or(0)
}

/// Send my data to every teammate on the line: everyone when it changed, and anyone newly on
/// the line (or back on it) whatever happens.
fn push_live(repo: &str, me: &str, l: &Link, d: Value, live: &mut Live) {
    let Some((Some(key), members)) = with(repo, |s| (s.key, s.peers.values().filter(|p| p.joined).map(|p| p.pub_key.clone()).collect::<Vec<_>>())) else { return };
    let lines = LINES.lock().unwrap().clone().unwrap_or_default();
    let on: Vec<String> = members.into_iter().filter(|k| lines.contains_key(k)).collect();
    live.told.retain(|k| on.contains(k));
    let p = pulse(repo);
    let all = live.last.as_ref() != Some(&d) && (p != live.pulse || live.at.elapsed() >= Duration::from_secs(1));
    if all {
        live.last = Some(d.clone());
        live.at = Instant::now();
        live.pulse = p;
        live.told.clear();
    }
    let to: Vec<String> = on.into_iter().filter(|k| !live.told.contains(k)).collect();
    if to.is_empty() {
        return;
    }
    let msg = json!({"repo": repo, "login": me, "seq": now_ms(), "data": tc::seal(&key, &format!("data:{me}"), d.to_string().as_bytes())});
    let bytes = msg.to_string().into_bytes();
    for k in to {
        l.send(&k, &bytes);
        live.told.push(k);
    }
}

/// A teammate's data straight off the line. Only from someone git already showed us as a member,
/// only from their own member key, and only if it's newer than what we have.
fn take_live(repo: &str, root: &Path, from: &str, bytes: &[u8], hooks: &Hooks) {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else { return };
    match v["kind"].as_str() {
        Some("file") => return take_file_req(repo, root, from, &v),
        Some("file_re") => return take_file_reply(repo, from, &v),
        _ => {}
    }
    let login = v["login"].as_str().unwrap_or("").to_string();
    let Some((me, key, old)) = with(repo, |s| (s.me.clone(), s.key, s.peers.get(&login).cloned())) else { return };
    let (Some(k), Some(old)) = (key, old) else { return };
    let seq = v["seq"].as_u64().unwrap_or(0);
    if login == me || old.pub_key != from || !old.joined || seq <= old.seq {
        return;
    }
    let Some(d) = tc::open(&k, &format!("data:{login}"), v["data"].as_str().unwrap_or(""))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { return };
    let mut p = Peer {
        login: login.clone(), online: true, ts: now(), seq, pub_key: old.pub_key, wrapped: old.wrapped,
        invites: old.invites, transcripts: old.transcripts, ..Default::default()
    };
    parse_data(&mut p, &d);
    let mut new_chat = false;
    take_in(repo, &me, key, p, hooks, &mut new_chat);
    if new_chat { save_chat(repo) }
    save_seen(repo);
}

// ---- a teammate's working copy, read over the line ----
//
// Each teammate has their own clone: nothing they haven't pushed is in ours. read_team_file asks
// their qh for one file (uncommitted edits included) and list_team_files for what they changed
// against HEAD. Both go only over the direct line, never into git (file contents don't belong on
// the board), and the serving side answers on its own, within strict rules (`serve_file`).

const FILE_WAIT: Duration = Duration::from_secs(10);
const FILE_MAX_BYTES: usize = 200 * 1024;
const FILE_MAX_LINES: usize = 2000;
const FILE_PART: usize = 24 * 1024; // bytes of text per line message: sealed and escaped it stays under link's 256 KB
const LIST_MAX: usize = 400;

type FileWait = (String, std::sync::mpsc::Sender<Value>); // who must answer, where the parts go
static FILE_WAITS: Mutex<Option<HashMap<u64, FileWait>>> = Mutex::new(None);
static FILE_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A file request or reply as it goes over the line: sealed with the team key, bound to the sender.
fn file_msg(repo: &str, me: &str, key: &Key, kind: &str, body: &Value) -> Vec<u8> {
    json!({"repo": repo, "login": me, "kind": kind, "data": tc::seal(key, &format!("{kind}:{me}"), body.to_string().as_bytes())})
        .to_string().into_bytes()
}

fn open_file_msg(key: &Key, v: &Value) -> Option<Value> {
    let (kind, login) = (v["kind"].as_str()?, v["login"].as_str()?);
    tc::open(key, &format!("{kind}:{login}"), v["data"].as_str()?).and_then(|b| serde_json::from_slice(&b).ok())
}

/// A reply in parts small enough for the line: {re, part, parts, ok, text}.
fn reply_parts(re: u64, r: Result<String, String>) -> Vec<Value> {
    let (ok, text) = match r { Ok(t) => (true, t), Err(e) => (false, e) };
    let mut parts: Vec<String> = vec![];
    let mut rest = text.as_str();
    while !rest.is_empty() || parts.is_empty() {
        let mut cut = rest.len().min(FILE_PART);
        while !rest.is_char_boundary(cut) { cut -= 1 }
        parts.push(rest[..cut].to_string());
        rest = &rest[cut..];
    }
    let n = parts.len();
    parts.into_iter().enumerate().map(|(i, t)| json!({"re": re, "part": i, "parts": n, "ok": ok, "text": t})).collect()
}

/// The whole reply once every part is in; None while some are missing.
fn assemble(parts: &[Value]) -> Option<Result<String, String>> {
    let n = parts.first()?["parts"].as_u64()? as usize;
    if n == 0 || n > 64 { return Some(Err("a malformed reply".into())) }
    let mut slots: Vec<Option<&str>> = vec![None; n];
    for p in parts {
        let i = p["part"].as_u64().unwrap_or(u64::MAX) as usize;
        if i < n { slots[i] = p["text"].as_str() }
    }
    let text: String = slots.iter().map(|s| s.map(str::to_string)).collect::<Option<Vec<_>>>()?.concat();
    Some(if parts[0]["ok"].as_bool() == Some(true) { Ok(text) } else { Err(text) })
}

/// May `login`, on the line as member key `from`, read our working copy? Joined members only,
/// on their own key, not removed, and not someone we have as view only.
fn may_read(s: &State, login: &str, from: &str) -> Result<(), String> {
    let p = s.peers.get(login).ok_or("not a member of this team")?;
    if s.key.is_none() || !p.joined || p.pub_key != from || login == s.me {
        return Err("not a member of this team".into());
    }
    if s.removed.iter().any(|r| r == login) {
        return Err("removed from this team".into());
    }
    if s.roles.get(login).is_some_and(|r| r == "viewer") {
        return Err(format!("{} has you as view only", s.me));
    }
    Ok(())
}

/// Read one file of the team repo at `root` for a teammate, or list what's changed against HEAD.
/// Only plain text files inside the repo (symlinks resolved), never secrets, git's own files,
/// protected files or anything git ignores; at most FILE_MAX_LINES lines / FILE_MAX_BYTES.
pub(crate) fn serve_file(root: &Path, req: &Value) -> Result<String, String> {
    let root = root.canonicalize().map_err(|_| "the project folder is gone".to_string())?;
    if req["op"] == "list" {
        return list_changed(&root);
    }
    let path = req["path"].as_str().unwrap_or("").trim();
    if path.is_empty() || path.contains('\0') {
        return Err("no path given".into());
    }
    let rel = Path::new(path);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
        return Err(format!("{path}: give a path relative to the project root, without .."));
    }
    let full = root.join(rel).canonicalize().map_err(|_| format!("{path}: no such file in their working copy"))?;
    if !crate::sandbox::under(&full, &root) {
        return Err(format!("{path}: outside the project (a link out of it)"));
    }
    let inner = full.strip_prefix(&root).map_err(|_| format!("{path}: outside the project"))?.to_path_buf();
    if inner.as_os_str().is_empty() || !full.is_file() {
        return Err(format!("{path}: not a file"));
    }
    let named = [rel, inner.as_path()];
    if named.iter().any(|p| p.components().any(|c| c.as_os_str() == ".git")) {
        return Err(format!("{path}: git's own files aren't shared"));
    }
    if crate::sandbox::secret(&full) || crate::sandbox::secret(&root.join(rel)) || crate::sandbox::protected(&full) {
        return Err(format!("{path}: a secret or protected file; it's never shared"));
    }
    for p in named {
        let p = p.to_string_lossy();
        if git(&root, &["check-ignore", "-q", "--", &p]).is_some() {
            return Err(format!("{path}: ignored by git, so it isn't shared"));
        }
    }
    let bytes = std::fs::read(&full).map_err(|e| format!("{path}: {e}"))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(format!("{path}: a binary file"));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = req["start"].as_u64().unwrap_or(1).max(1) as usize;
    if start > lines.len().max(1) {
        return Err(format!("{path} has only {} lines", lines.len()));
    }
    let want_end = req["end"].as_u64().filter(|e| *e > 0).map_or(lines.len(), |e| (e as usize).min(lines.len()));
    let (mut out, mut last) = (String::new(), start - 1);
    for i in start..=want_end {
        let row = format!("{i}\t{}\n", lines[i - 1]);
        if i - start >= FILE_MAX_LINES || out.len() + row.len() > FILE_MAX_BYTES {
            break;
        }
        out.push_str(&row);
        last = i;
    }
    let dirty = git(&root, &["status", "--porcelain", "--", &inner.to_string_lossy()]).is_some_and(|s| !s.trim().is_empty());
    let mut head = format!("{} ({} lines{})", inner.to_string_lossy().replace('\\', "/"), lines.len(),
                           if dirty { ", changed since their last commit" } else { "" });
    if last < want_end {
        head += &format!("; showing lines {start}-{last}, ask again with start_line={} for more", last + 1);
    }
    Ok(format!("{head}\n{}", out.trim_end()))
}

/// What the working copy changed against HEAD (git status), secrets left out.
fn list_changed(root: &Path) -> Result<String, String> {
    let st = git_raw(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], None, &[])
        .ok_or("git status failed in their working copy")?;
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let mut rows = vec![];
    let mut it = st.split(|b| *b == 0).filter(|e| e.len() > 3);
    while let Some(e) = it.next() {
        let e = String::from_utf8_lossy(e);
        let (code, path) = (e[..2].to_string(), e[3..].to_string());
        if code.contains('R') || code.contains('C') { it.next(); } // the old name follows a rename
        if crate::sandbox::secret(&root.join(&path)) || path.split('/').any(|c| c == ".git") {
            continue;
        }
        let what = match code.trim() { "??" => "new", c if c.contains('D') => "deleted", c if c.contains('A') => "added",
                                       c if c.contains('R') => "renamed", _ => "modified" };
        rows.push(format!("{what}\t{path}"));
    }
    let n = rows.len();
    rows.truncate(LIST_MAX);
    Ok(format!("branch {branch}; {n} file{} changed against their last commit{}\n{}", if n == 1 { "" } else { "s" },
               if n > LIST_MAX { format!(" (first {LIST_MAX} shown)") } else { String::new() },
               if rows.is_empty() { "(nothing uncommitted)".to_string() } else { rows.join("\n") }))
}

/// A teammate's file request off the line: checked, answered in a thread of its own (git can be slow).
fn take_file_req(repo: &str, root: &Path, from: &str, v: &Value) {
    let login = v["login"].as_str().unwrap_or("").to_string();
    let Some((me, key, ok)) = with(repo, |s| (s.me.clone(), s.key, may_read(s, &login, from))) else { return };
    let Some(key) = key else { return };
    let Some(req) = open_file_msg(&key, v) else { return };
    let Some(id) = req["id"].as_u64() else { return };
    let (repo, root, from) = (repo.to_string(), root.to_path_buf(), from.to_string());
    let _ = std::thread::Builder::new().name("qh-team-file".into()).spawn(move || {
        let what = if req["op"] == "list" { "listed your changed files".to_string() } else { format!("read {}", req["path"].as_str().unwrap_or("")) };
        let r = ok.and_then(|_| if crate::config::load().team_share_files { Ok(()) } else {
            Err(format!("{me} has turned off sharing files with teammates' agents (team_share_files = \"off\")"))
        }).and_then(|_| serve_file(&root, &req));
        match &r {
            Ok(_) => log_to(&repo, &format!("{login}'s agent {what}")),
            Err(e) => log_to(&repo, &format!("refused {login}'s agent ({what}): {e}")),
        }
        if let Some(l) = link() {
            for part in reply_parts(id, r) {
                l.send(&from, &file_msg(&repo, &me, &key, "file_re", &part));
            }
        }
    });
}

/// A part of a reply to one of our file requests: to whoever is waiting, if it's from the one asked.
fn take_file_reply(repo: &str, from: &str, v: &Value) {
    let login = v["login"].as_str().unwrap_or("");
    let Some(Some(key)) = with(repo, |s| s.peers.get(login).filter(|p| p.pub_key == from && p.joined).and(s.key)) else { return };
    let Some(part) = open_file_msg(&key, v) else { return };
    let re = part["re"].as_u64().unwrap_or(0);
    let w = FILE_WAITS.lock().unwrap();
    if let Some((who, tx)) = w.as_ref().and_then(|m| m.get(&re)) {
        if who == login {
            let _ = tx.send(part);
        }
    }
}

/// Ask `to`'s qh for something from their working copy and wait (up to FILE_WAIT) for the reply.
fn file_request(cwd: &Path, to: &str, mut req: Value) -> Result<(String, String), String> {
    let (_, repo) = repo_of(cwd).ok_or("this folder isn't a team project")?;
    if !running_for(&repo) { return Err("team mode is off for this project".into()) }
    let to = to.trim().trim_start_matches('@').split('/').next().unwrap_or("").to_string();
    let (me, key, peer_key) = with(&repo, |s| -> Result<_, String> {
        let key = s.key.ok_or("you're not in the team yet: waiting to be let in")?;
        let mut team: Vec<&str> = s.peers.values().filter(|p| p.joined && p.login != s.me).map(|p| p.login.as_str()).collect();
        team.sort();
        let p = s.peers.get(&to).filter(|p| p.joined && to != s.me)
            .ok_or_else(|| format!("no teammate {to}. Teammates: {}", if team.is_empty() { "nobody yet".into() } else { team.join(", ") }))?;
        if p.roles.get(&s.me).is_some_and(|r| r == "viewer") {
            return Err(format!("{to} has you as view only: their files aren't shared with you"));
        }
        Ok((s.me.clone(), key, p.pub_key.clone()))
    }).ok_or("team mode is off for this project")??;
    if !LINES.lock().unwrap().as_ref().is_some_and(|l| l.contains_key(&peer_key)) {
        return Err(format!("no direct line to {to} right now (offline, or not reachable yet), so their working copy can't be read. \
Ask them to commit and push a branch, then fetch it."));
    }
    let l = link().ok_or("the direct line is off on this machine (QH_LINK=off)")?;
    let id = (now_ms() << 8) | (FILE_IDS.fetch_add(1, Relaxed) & 0xff);
    req["id"] = json!(id);
    let (tx, rx) = std::sync::mpsc::channel();
    FILE_WAITS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, (to.clone(), tx));
    l.send(&peer_key, &file_msg(&repo, &me, &key, "file", &req));
    let until = Instant::now() + FILE_WAIT;
    let mut parts = vec![];
    let got = loop {
        let Some(left) = until.checked_duration_since(Instant::now()) else { break None };
        match rx.recv_timeout(left) {
            Ok(p) => {
                parts.push(p);
                if let Some(r) = assemble(&parts) { break Some(r) }
            }
            Err(_) => break None,
        }
    };
    if let Some(m) = FILE_WAITS.lock().unwrap().as_mut() { m.remove(&id); }
    match got {
        Some(Ok(text)) => Ok((to, text)),
        Some(Err(e)) => Err(format!("{to}'s qh refused: {e}")),
        None => Err(format!("{to}'s qh didn't answer within {}s (busy line, or they just went offline). Try again, or ask them to commit and push a branch.",
                            FILE_WAIT.as_secs())),
    }
}

/// A path as the model gave it, made relative to the repo root (theirs has the same layout):
/// an absolute path in our clone, or one relative to a session working below the root.
fn repo_path(cwd: &Path, path: &str) -> String {
    let path = path.trim().replace('\\', "/");
    let Some((root, _)) = repo_of(cwd) else { return path };
    let root = root.canonicalize().unwrap_or(root);
    if Path::new(&path).is_absolute() {
        return Path::new(&path).strip_prefix(&root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or(path);
    }
    let here = cwd.canonicalize().unwrap_or(cwd.to_path_buf());
    match here.strip_prefix(&root) {
        Ok(sub) if !sub.as_os_str().is_empty() => sub.join(&path).to_string_lossy().replace('\\', "/"),
        _ => path,
    }
}

/// The read_team_file tool.
pub fn read_team_file(cwd: &Path, to: &str, path: &str, start: Option<u64>, end: Option<u64>) -> String {
    match file_request(cwd, to, json!({"op": "read", "path": repo_path(cwd, path), "start": start, "end": end})) {
        Ok((who, text)) => format!("[{who}'s working copy, read live: it may include uncommitted changes that aren't in your clone. \
Read-only: to build on it, wait until they commit and push a branch, then fetch it]\n{text}"),
        Err(e) => format!("error: {e}"),
    }
}

/// The list_team_files tool.
pub fn list_team_files(cwd: &Path, to: &str) -> String {
    match file_request(cwd, to, json!({"op": "list"})) {
        Ok((who, text)) => format!("[{who}'s working copy, live] {text}\nRead one with read_team_file(to=\"{who}\", path=...)."),
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_is_standard_base64() {
        assert_eq!(auth_env("abc")[2].1, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46YWJj");
    }

    fn owned(text: &str) -> Vec<String> {
        owners(text).into_iter().map(|o| o.unwrap_or_default()).collect()
    }

    #[test]
    fn lines_belong_to_their_method_not_the_class() {
        let py = "import os\n\nclass Shop:\n    def total(self):\n        return 1\n\n    def pay(self):\n        x = 2\n        return x\n\ndef main():\n    Shop()\n";
        assert_eq!(owned(py), ["", "", "Shop", "total", "total", "total", "pay", "pay", "pay", "pay", "main", "main"]);
        let rs = "impl Cart {\n    pub fn add(&mut self) {\n        self.n += 1;\n    }\n    fn clear(&mut self) {\n    }\n}\nfn main() {}\n";
        assert_eq!(owned(rs), ["Cart", "add", "add", "add", "clear", "clear", "Cart", "main"]);
        let js = "export const getUser = async (id) => {\n  return db(id)\n}\nclass Api {\n  async fetch(url) {\n    if (x) {\n      go()\n    }\n  }\n}\n";
        assert_eq!(owned(js), ["getUser", "getUser", "getUser", "Api", "fetch", "fetch", "fetch", "fetch", "fetch", "Api"]);
        let top = "def a():\n    x = 1\nprint(1)\n\n@app.get\ndef b():\n    pass\n";
        assert_eq!(owned(top), ["a", "a", "", "", "", "b", "b"]);
        let go = "func (s *Server) Handle(w http.ResponseWriter) {\n\treturn\n}\n";
        assert_eq!(owned(go)[1], "Handle");
    }

    #[test]
    fn a_new_function_next_to_another_touches_only_itself() {
        let before = "def greet(name):\n    return 'hi ' + name\n\nprint('shop')\n";
        let after = "def greet(name):\n    return 'hi ' + name\n\n\ndef bye(name):\n    return 'bye ' + name\n\nprint('shop')\n";
        let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
        let (del, add) = changed_lines(&a, &b);
        let ob = owners(after);
        let real: Vec<String> = del.iter().map(|&i| owners(before)[i].clone()).chain(add.iter().filter(|&&i| !b[i].trim().is_empty()).map(|&i| ob[i].clone())).flatten().collect();
        assert_eq!(real, ["bye", "bye"]);
    }

    #[test]
    fn a_rename_takes_the_old_name_away() {
        let before = "struct UserDto {\n    pub name: String,\n    age: u32,\n}\nfn get_user() {}\n";
        let after = "struct UserDto {\n    pub full_name: String,\n    age: u32,\n}\nfn fetch_user() {}\n";
        let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
        let (del, _) = changed_lines(&a, &b);
        let kept = defines(&b, &(0..b.len()).collect::<Vec<_>>());
        let removed: Vec<String> = defines(&a, &del).into_iter().filter(|n| !kept.contains(n)).collect();
        assert_eq!(removed, ["name", "get_user"]);
        // a moved function isn't taken away
        let moved = "fn get_user() {}\nstruct UserDto {\n    pub name: String,\n    age: u32,\n}\n";
        let m: Vec<&str> = moved.lines().collect();
        let (del, _) = changed_lines(&a, &m);
        let kept = defines(&m, &(0..m.len()).collect::<Vec<_>>());
        assert!(defines(&a, &del).into_iter().all(|n| kept.contains(&n)));
    }

    #[test]
    fn uses_skip_strings_comments_and_keywords() {
        let l = ["    return render(user.fullName, \"getUser\") // avatarUrl", "    x = self.avatar  # old_name"];
        assert_eq!(used_names(&l, &[0, 1]), ["render", "user", "fullName", "avatar"]);
        assert_eq!(field_name("    pub full_name: String,"), Some("full_name".into()));
        assert_eq!(field_name("        self.cache = {}"), Some("cache".into()));
        assert_eq!(field_name("    let x: u32 = 5;"), None);
    }

    #[test]
    fn diff_finds_only_the_changed_lines() {
        let a: Vec<&str> = "a\nb\nc\nd\ne\nf".lines().collect();
        let b: Vec<&str> = "a\nB\nc\nd\ne\nF\ng".lines().collect();
        assert_eq!(changed_lines(&a, &b), (vec![1, 5], vec![1, 5, 6]));
        let same: Vec<&str> = "x\ny".lines().collect();
        assert_eq!(changed_lines(&same, &same), (vec![], vec![]));
        let ins: Vec<&str> = "x\nnew\ny".lines().collect();
        assert_eq!(changed_lines(&same, &ins), (vec![], vec![1]));
    }
    fn state(me: &str) -> State {
        State {
            repo: "o/r".into(), me: me.into(), peers: HashMap::new(), error: String::new(), key: None, outbox: vec![], next_id: 0,
            seen: HashMap::new(), pending: vec![], roles: HashMap::new(), approved: vec![], asking: vec![], invites: HashMap::new(),
            epoch: 0, removed: vec![], admins: HashMap::new(), pins: HashMap::new(), gh_login: me.into(), git_ms: HashMap::new(),
            asked_github: HashMap::new(), warned: vec![], my_sha: String::new(),
            chat: vec![], my_chat: vec![], chat_seen: HashMap::new(), my_notes: vec![], notes_seen: HashMap::new(), claims: HashMap::new(),
            uses: HashMap::new(), waiting: vec![], sheets: HashMap::new(), asked: vec![], answered: vec![], answers: "auto".into(),
            answerer: None, promises: vec![], last_answer: HashMap::new(), unread: 0, dirty: false, running: Default::default(), finished: Default::default(),
            secret: None, my_dms: vec![], my_groups: vec![], my_gmsgs: vec![], groups: HashMap::new(), msgs: vec![],
            msg_ids: Default::default(), chan_unread: HashMap::new(), pinged: vec![],
        }
    }

    fn quiet() -> Hooks {
        Hooks { say: Box::new(|_| {}), deliver: Box::new(|_, _| {}), ask: Box::new(|_| {}), refresh: Box::new(|| {}),
                answer: std::sync::Arc::new(|_| {}), answered: Box::new(|_, _| {}) }
    }

    /// A folder as `who` would push it for `login` (signed with `who`'s keys), read back from git.
    fn folder(root: &Path, repo: &str, login: &str, who: &x25519_dalek::StaticSecret, extra: &[(&str, &str)], ms: u64) -> (Folder, Peer) {
        let mut files = vec![("pub".to_string(), crate::oauth::b64url(x25519_dalek::PublicKey::from(who).as_bytes())),
                             ("sign".to_string(), tc::sign_pub(who))];
        files.extend(extra.iter().map(|(n, c)| (n.to_string(), c.to_string())));
        let msg = json!({"v": 2, "state": "online", "ts": ms / 1000, "ms": ms});
        let sig = tc::sign_folder(who, repo, login, &msg.to_string(), &files);
        files.push(("sig".into(), sig));
        let parent = identity_commit(root, login, &files[0].1, &files[1].1).unwrap();
        let sha = folder_commit(root, &files, &msg, &parent).unwrap();
        let f = folder_files(root, &sha);
        let p = read_folder(&f, repo, login, None);
        (f, p)
    }

    #[test]
    fn folders_written_under_someone_elses_name_are_ignored() {
        let root = std::env::temp_dir().join(format!("qh-vet-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        git(&root, &["init", "-q"]).unwrap();
        let repo = "sec/vet";
        let mut st = state("bob");
        st.repo = repo.into();
        let team = tc::new_team_key();
        st.key = Some(team);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), st);
        let (alice, mallory) = (x25519_dalek::StaticSecret::from(crate::oauth::random()), x25519_dalek::StaticSecret::from(crate::oauth::random()));
        let t = now_ms();

        let (_, p) = folder(&root, repo, "alice", &alice, &[("data", "x")], t);
        assert!(p.signed && vet(repo, "", &p, &quiet()), "alice's own folder is taken (and her keys pinned)");
        let (_, p) = folder(&root, repo, "alice", &mallory, &[("data", "x")], t + 1);
        assert!(p.signed, "mallory's signature is valid for mallory's keys...");
        assert!(!vet(repo, "", &p, &quiet()), "...but they aren't alice's, and GitHub doesn't vouch for them");

        // mallory keeps alice's keys and signature but swaps the sealed data
        let (f, _) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 2);
        let mut files = f.files.clone();
        for (n, c) in files.iter_mut() { if n == "data" { *c = tc::seal(&team, "data:alice", b"{}") } }
        let sha = folder_commit(&root, &files, &serde_json::from_str(&f.msg).unwrap(), &identity_commit(&root, "alice", "a", "b").unwrap()).unwrap();
        let p = read_folder(&folder_files(&root, &sha), repo, "alice", None);
        assert!(!p.signed && !vet(repo, "", &p, &quiet()), "a changed file breaks alice's signature");

        let (_, p) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 5);
        assert!(vet(repo, "", &p, &quiet()), "alice's next folder");
        let (_, p) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 3);
        assert!(!vet(repo, "", &p, &quiet()), "an older folder of alice's, pushed back by someone: a replay");

        // a team key change from a member whose keys GitHub hasn't vouched for is ignored
        let bob = x25519_dalek::StaticSecret::from(crate::oauth::random());
        let bob_pub = crate::oauth::b64url(x25519_dalek::PublicKey::from(&bob).as_bytes());
        let new = tc::new_team_key();
        let wrap = tc::wrap(&new, &alice, &bob_pub, repo).unwrap();
        let (f, p) = folder(&root, repo, "alice", &alice, &[("epoch", "1"), ("keys/bob", &wrap)], t + 6);
        assert!(vet(repo, "", &p, &quiet()));
        newer_key(repo, &[(f, p)], &bob, "", &quiet());
        assert_eq!(with(repo, |s| (s.key, s.epoch)).unwrap(), (Some(team), 0), "the key stays");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn peer(login: &str, sessions: &[&str], online: bool) -> Peer {
        Peer { login: login.into(), online, joined: true, ts: now(),
               sessions: sessions.iter().map(|s| (s.to_string(), "idle".into(), String::new())).collect(), ..Default::default() }
    }

    #[test]
    fn questions_go_to_whoever_is_changing_or_using_the_code() {
        let mut s = state("alice");
        let mut bob = peer("bob", &["main", "ui"], true);
        bob.claims.insert("main".into(), HashMap::from([("api/user.rs".into(), HashMap::from([("get_user".to_string(), now())]))]));
        let mut carol = peer("carol", &["main"], true);
        carol.uses.insert("main".into(), HashMap::from([("get_user".to_string(), now())]));
        s.peers.insert("bob".into(), bob);
        s.peers.insert("carol".into(), carol);
        let root = std::env::temp_dir();
        // the editor first, then the reader
        let r = route(&s, &root, "does get_user still return name?", "");
        assert_eq!((r[0].0.as_str(), r[0].1.as_str()), ("bob", "main"), "{r:?}");
        assert_eq!(r[0].2, "changing get_user", "{r:?}");
        assert_eq!(r[1].0, "carol");
        // a file named in the question finds its editor too
        let r = route(&s, &root, "what's happening in api/user.rs", "");
        assert_eq!(r[0].0, "bob");
        // asked by name: that person, even with nothing matching
        let r = route(&s, &root, "anything new?", "carol");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "carol");
        let r = route(&s, &root, "anything new?", "bob/ui");
        assert_eq!((r[0].0.as_str(), r[0].1.as_str()), ("bob", "ui"));
        // nothing names anyone's code: nobody
        assert!(route(&s, &root, "how are you", "").is_empty());
    }

    #[test]
    fn fact_sheet_says_what_a_session_is_doing() {
        let mut p = peer("bob", &["main"], true);
        p.claims.insert("main".into(), HashMap::from([("api/user.rs".into(), HashMap::from([("get_user".to_string(), now())]))]));
        p.uses.insert("main".into(), HashMap::from([("render".to_string(), now())]));
        p.details.insert("main".into(), SessionView { topic: "rename user fields".into(), ..Default::default() });
        p.sheets.insert("main".into(), Sheet { decided: "name becomes full_name".into(), next: "add avatar".into(), ts: now() });
        let t = sheet_text(&p, "main");
        for want in ["bob/main · online", "doing: rename user fields", "changing: api/user.rs › get_user", "uses: render",
                     "decided: name becomes full_name", "next: add avatar"] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
    }

    #[test]
    fn notes_come_in_full_only_where_they_matter() {
        let note = |f: &str, n: &str| (1, "o/r".to_string(), format!("bob/main changed {f} ({n}) — working on: x\n+ new line"), f.to_string(), vec![n.to_string()]);
        let all = [note("a.py", "fa"), note("b.py", "fb"), note("c.py", "fc"), note("a.py", "fa2")];
        let refs: Vec<&NoteIn> = all.iter().collect();
        let out = shape_notes(&refs, &["a.py".into()], &["fc".into()]);
        assert_eq!(out.len(), 3, "one per file: {out:?}");
        assert!(out.iter().any(|t| t.contains("(fa2)") && t.contains("+ new line")), "touched file in full: {out:?}");
        assert!(out.iter().any(|t| t.contains("(fc)") && t.contains("+ new line")), "used name in full: {out:?}");
        assert!(out.iter().any(|t| t.contains("(fb)") && !t.contains("+ new line") && t.contains("haven't touched")), "{out:?}");
        // just a couple: everything in full
        let two: Vec<&NoteIn> = all[..2].iter().collect();
        assert!(shape_notes(&two, &[], &[]).iter().all(|t| t.contains("+ new line")));
    }

    /// A team repo with the files a teammate might ask for, and one outside it.
    fn share_repo(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("qh-share-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (root, out) = (base.join("proj"), base.join("outside"));
        for d in [root.join("src"), root.join("dist"), root.join("certs"), out.clone()] { std::fs::create_dir_all(d).unwrap() }
        git(&root, &["init", "-q"]).unwrap();
        for (f, body) in [("src/server.js", "const a = 1;\nconst b = 2;\n"), (".gitignore", "dist/\nlocal.txt\n"), ("dist/out.js", "x"),
                          ("local.txt", "mine"), (".env", "TOKEN=1"), ("certs/server.key", "k"), (".env.example", "TOKEN=")] {
            std::fs::write(root.join(f), body).unwrap();
        }
        std::fs::write(out.join("secret.txt"), "outside").unwrap();
        (root, out)
    }

    fn read(root: &Path, path: &str) -> Result<String, String> {
        serve_file(root, &json!({"op": "read", "path": path}))
    }

    #[test]
    fn a_teammate_reads_only_shareable_files_inside_the_repo() {
        let (root, out) = share_repo("rules");
        let ok = read(&root, "src/server.js").unwrap();
        assert!(ok.starts_with("src/server.js (2 lines, changed since their last commit)"), "{ok}");
        assert!(ok.contains("1\tconst a = 1;\n2\tconst b = 2;"), "{ok}");
        assert!(read(&root, "./src/server.js").is_ok());
        assert!(read(&root, ".env.example").is_ok(), "an example env file isn't a secret");
        for (p, why) in [("../outside/secret.txt", "without .."), ("src/../../outside/secret.txt", "without .."),
                         (&out.join("secret.txt").to_string_lossy() as &str, "relative to the project root"),
                         (".env", "secret"), ("certs/server.key", "secret"), (".git/config", "git's own"),
                         ("dist/out.js", "ignored by git"), ("local.txt", "ignored by git"), ("nope.js", "no such file"),
                         ("src", "not a file"), ("", "no path")] {
            let e = read(&root, p).unwrap_err();
            assert!(e.contains(why), "{p}: {e}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(out.join("secret.txt"), root.join("link-out")).unwrap();
            std::os::unix::fs::symlink(root.join("src/server.js"), root.join("link-in")).unwrap();
            std::os::unix::fs::symlink(root.join(".env"), root.join("env-link")).unwrap();
            std::os::unix::fs::symlink(root.join("dist/out.js"), root.join("ignored-link")).unwrap();
            assert!(read(&root, "link-out").unwrap_err().contains("outside the project"));
            assert!(read(&root, "link-in").unwrap().starts_with("src/server.js"), "a link inside is read as its target");
            assert!(read(&root, "env-link").unwrap_err().contains("secret"), "a link to a secret is a secret");
            assert!(read(&root, "ignored-link").unwrap_err().contains("ignored"), "a link to an ignored file is ignored");
        }
        std::fs::write(root.join("bin.dat"), b"ab\0cd").unwrap();
        assert!(read(&root, "bin.dat").unwrap_err().contains("binary"));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn a_read_is_capped_and_says_where_to_go_on() {
        let (root, _) = share_repo("cap");
        let long: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("long.txt"), long).unwrap();
        let r = read(&root, "long.txt").unwrap();
        assert_eq!(r.lines().count(), 1 + FILE_MAX_LINES, "a header and 2000 lines");
        assert!(r.contains("showing lines 1-2000, ask again with start_line=2001"), "{}", r.lines().next().unwrap());
        let r = serve_file(&root, &json!({"op": "read", "path": "long.txt", "start": 4999, "end": 9000})).unwrap();
        assert!(r.ends_with("4999\tline 4999\n5000\tline 5000") && !r.contains("showing"), "{r}");
        assert!(serve_file(&root, &json!({"op": "read", "path": "long.txt", "start": 6000})).unwrap_err().contains("only 5000 lines"));
        let wide: String = (0..400).map(|_| "x".repeat(1000) + "\n").collect();
        std::fs::write(root.join("wide.txt"), wide).unwrap();
        let r = read(&root, "wide.txt").unwrap();
        assert!(r.len() <= FILE_MAX_BYTES + 200 && r.contains("start_line="), "{}", r.len());
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn changed_files_list_leaves_secrets_out() {
        let (root, _) = share_repo("list");
        let r = serve_file(&root, &json!({"op": "list"})).unwrap();
        assert!(r.contains("new\tsrc/server.js") && r.contains("new\t.env.example"), "{r}");
        assert!(!r.contains("\t.env\n") && !r.ends_with("\t.env") && !r.contains("server.key") && !r.contains("dist/"), "{r}");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn only_joined_members_on_their_own_key_may_read() {
        let mut s = state("bob");
        s.key = Some(tc::new_team_key());
        let mut p = peer("alice", &["main"], true);
        p.joined = true;
        p.pub_key = "ka".into();
        s.peers.insert("alice".into(), p.clone());
        p.login = "carol".into();
        p.joined = false;
        s.peers.insert("carol".into(), p);
        assert!(may_read(&s, "alice", "ka").is_ok());
        assert!(may_read(&s, "alice", "kx").is_err(), "someone else's key");
        assert!(may_read(&s, "carol", "ka").is_err(), "not joined");
        assert!(may_read(&s, "mallory", "ka").is_err(), "unknown");
        s.roles.insert("alice".into(), "viewer".into());
        assert!(may_read(&s, "alice", "ka").unwrap_err().contains("view only"));
        s.roles.clear();
        s.removed.push("alice".into());
        assert!(may_read(&s, "alice", "ka").unwrap_err().contains("removed"));
        s.removed.clear();
        s.key = None;
        assert!(may_read(&s, "alice", "ka").is_err(), "no team key: not in the team ourselves");
    }

    #[test]
    fn file_requests_and_replies_survive_the_line() {
        let key = tc::new_team_key();
        let req = json!({"op": "read", "path": "src/server.js", "id": 7, "start": null, "end": null});
        let bytes = file_msg("o/r", "alice", &key, "file", &req);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["kind"], "file");
        assert_eq!(open_file_msg(&key, &v), Some(req));
        let mut forged = v.clone();
        forged["login"] = json!("bob");
        assert_eq!(open_file_msg(&key, &forged), None, "sealed for alice: it can't pass as bob's");
        assert_eq!(open_file_msg(&tc::new_team_key(), &v), None, "another team's key");

        // a full-size reply, with the worst characters for escaping, fits the line in parts
        let text: String = (0..FILE_MAX_BYTES / 8).map(|i| if i % 2 == 0 { "\"\\\"\\\t\n\"\\" } else { "é€😀" }).collect();
        let parts = reply_parts(7, Ok(text.clone()));
        assert!(parts.len() > 1);
        for p in &parts {
            assert!(file_msg("o/r", "bob", &key, "file_re", p).len() < 256 * 1024);
        }
        let mut shuffled = parts.clone();
        shuffled.reverse();
        assert_eq!(assemble(&shuffled[..shuffled.len() - 1]), None, "a part missing");
        assert_eq!(assemble(&shuffled), Some(Ok(text)));
        assert_eq!(assemble(&reply_parts(7, Err("no".into()))), Some(Err("no".into())));
        assert_eq!(assemble(&reply_parts(7, Ok(String::new()))), Some(Ok(String::new())));
    }

    // ---- requests, direct messages, groups, pings ----

    fn member(login: &str) -> (x25519_dalek::StaticSecret, String) {
        let k = x25519_dalek::StaticSecret::from(crate::oauth::random());
        let p = crate::oauth::b64url(x25519_dalek::PublicKey::from(&k).as_bytes());
        let _ = login;
        (k, p)
    }

    /// `login`'s state holding their own key, with `others` (login, public key) as joined teammates.
    fn keyed(login: &str, secret: &x25519_dalek::StaticSecret, others: &[(&str, &str)]) -> State {
        let mut s = state(login);
        s.secret = Some(secret.clone());
        for (l, pk) in others {
            let mut p = peer(l, &["main"], true);
            p.pub_key = pk.to_string();
            s.peers.insert(l.to_string(), p);
        }
        s
    }

    /// What `from`'s state publishes, as a teammate reads it.
    fn published(from: &State, pub_key: &str) -> Peer {
        let mut p = peer(&from.me, &["main"], true);
        p.pub_key = pub_key.into();
        let d = json!({"dms": from.my_dms, "groups": from.my_groups, "gmsgs": from.my_gmsgs});
        parse_data(&mut p, &d);
        p
    }

    #[test]
    fn work_waits_for_you_even_from_a_trusted_teammate_questions_dont() {
        let repo = "req/route";
        let mut st = state("alice");
        st.repo = repo.into();
        st.roles.insert("bob".into(), "trusted".into()); // trusted, and answers on auto: the most permissive setup
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), st);
        let delivered = std::sync::Arc::new(Mutex::new(vec![]));
        let answered = std::sync::Arc::new(Mutex::new(0));
        let (d2, a2) = (delivered.clone(), answered.clone());
        let hooks = Hooks { deliver: Box::new(move |s, t| d2.lock().unwrap().push((s.to_string(), t.to_string()))),
                            answer: std::sync::Arc::new(move |_| *a2.lock().unwrap() += 1), ..quiet() };
        let mut bob = peer("bob", &["main"], true);
        bob.outbox = vec![
            json!({"id": 5, "to": "alice", "session": "main", "text": "[from bob's agent] build the backend", "from_session": "api", "ts": now()}),
            json!({"id": 6, "to": "alice", "session": "main", "kind": "q", "from_session": "api", "text": "what port?", "ts": now()}),
        ];
        let mut nc = false;
        take_in(repo, &"alice".to_string(), None, bob, &hooks, &mut nc);
        let pending = with(repo, |s| s.pending.clone()).unwrap();
        assert_eq!(pending.len(), 1, "only the work waits: {pending:?}");
        assert_eq!((pending[0].id, pending[0].from_session.as_str()), (5, "api"));
        assert!(pending[0].question.is_none());
        assert!(delivered.lock().unwrap().is_empty(), "nothing reached a session without the user");
        assert_eq!(*answered.lock().unwrap(), 1, "the question is answered from a read-only copy, as before");
        let alerts = take_alerts();
        assert!(alerts.iter().any(|a| a == "qh: bob asks: build the backend"), "{alerts:?}");
        // the same folder again: no second request
        let mut bob = peer("bob", &["main"], true);
        bob.outbox = vec![json!({"id": 5, "to": "alice", "session": "main", "text": "build the backend", "ts": now()})];
        take_in(repo, &"alice".to_string(), None, bob, &hooks, &mut nc);
        assert_eq!(with(repo, |s| s.pending.len()).unwrap(), 1);
        // with /answers ask, a question waits too
        with(repo, |s| s.answers = "ask".into());
        with(repo, |s| s.roles.clear());
        let mut bob = peer("bob", &["main"], true);
        bob.outbox = vec![json!({"id": 7, "to": "alice", "session": "main", "kind": "q", "text": "which db?", "ts": now()})];
        take_in(repo, &"alice".to_string(), None, bob, &hooks, &mut nc);
        let pending = with(repo, |s| s.pending.clone()).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending[1].question.is_some());
        assert_eq!(*answered.lock().unwrap(), 1);
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    #[test]
    fn accept_decline_and_reply_say_the_right_thing() {
        let ((a, a_pub), (b, b_pub)) = (member("alice"), member("bob"));
        let mut s = keyed("alice", &a, &[("bob", &b_pub)]);
        let work = |id| Incoming { from: "bob".into(), id, session: "main".into(), text: "[from bob's agent] build the backend".into(), ts: now(), ..Default::default() };
        s.pending = vec![work(1), work(2), work(3)];
        let read = |s: &State| -> Vec<String> {
            let mut bob = keyed("bob", &b, &[("alice", &a_pub)]);
            take_private(&mut bob, "alice", &published(s, &a_pub));
            bob.msgs.iter().map(|m| m.text.clone()).collect()
        };
        // accept: delivered with the tag, nothing sent back
        let inc = decide_locked(&mut s, "bob", 1, "once").unwrap();
        assert_eq!(accepted_text(&inc), "[accepted request from bob]: [from bob's agent] build the backend");
        assert!(s.my_dms.is_empty());
        // reply: a private line back; the request still waits
        let r = reply_request_locked(&mut s, "bob", 2, "which framework?").unwrap();
        assert!(r.contains("still waits"), "{r}");
        assert!(s.pending.iter().any(|p| p.id == 2 && p.replied));
        assert_eq!(read(&s), ["alice re your request \"build the backend\": which framework?"]);
        // decline: gone, and they're told
        assert!(decide_locked(&mut s, "bob", 3, "deny").is_some());
        assert!(!s.pending.iter().any(|p| p.id == 3));
        assert_eq!(read(&s).last().unwrap(), "alice declined: \"build the backend\"");
        assert_eq!(s.seen.get("bob"), Some(&3));
        // a waiting question: a reply is its answer, and it's done
        let q = Question { repo: "o/r".into(), from: "bob".into(), from_session: "api".into(), id: 9, session: "main".into(), text: "port?".into() };
        s.pending.push(Incoming { from: "bob".into(), id: 9, session: "main".into(), text: "question: port?".into(), question: Some(q), ..Default::default() });
        reply_request_locked(&mut s, "bob", 9, "8080").unwrap();
        assert!(!s.pending.iter().any(|p| p.id == 9), "the question is done");
        let a = s.outbox.last().unwrap();
        assert_eq!((a["kind"].as_str(), a["re"].as_u64(), a["session"].as_str()), (Some("a"), Some(9), Some("api")));
        assert!(a["text"].as_str().unwrap().ends_with("8080"));
    }

    #[test]
    fn a_dm_or_group_line_opens_only_for_its_members() {
        let ((a, a_pub), (b, b_pub), (c, c_pub)) = (member("alice"), member("bob"), member("carol"));
        let mut alice = keyed("alice", &a, &[("bob", &b_pub), ("carol", &c_pub)]);
        dm_locked(&mut alice, "bob", "the deploy key is in 1password").unwrap();
        group_locked(&mut alice, "core", &["bob".into()]).unwrap();
        let gid = alice.groups.keys().next().unwrap().clone();
        group_send_locked(&mut alice, &gid, "@bob can you review?").unwrap();
        let folder = published(&alice, &a_pub);
        // nothing in the folder is readable text, even to someone holding the team key
        let raw = json!({"dms": alice.my_dms, "groups": alice.my_groups, "gmsgs": alice.my_gmsgs}).to_string();
        assert!(!raw.contains("1password") && !raw.contains("review") && !raw.contains("core"), "{raw}");

        let mut bob = keyed("bob", &b, &[("alice", &a_pub), ("carol", &c_pub)]);
        assert!(take_private(&mut bob, "alice", &folder));
        let texts: Vec<(&str, &str)> = bob.msgs.iter().map(|m| (m.chan.as_str(), m.text.as_str())).collect();
        assert_eq!(texts, [("@alice", "the deploy key is in 1password"), (format!("+{gid}").as_str(), "@bob can you review?")]);
        assert_eq!(bob.groups[&gid].members, ["alice", "bob"]);

        let mut carol = keyed("carol", &c, &[("alice", &a_pub), ("bob", &b_pub)]);
        assert!(!take_private(&mut carol, "alice", &folder), "carol is a teammate but not in the DM or the group");
        assert!(carol.msgs.is_empty() && carol.groups.is_empty());
        // and the group key itself doesn't open the line under another sender's name
        let line = &alice.my_gmsgs[0]["box"];
        assert!(tc::open(&bob.groups[&gid].key, &format!("gmsg:o/r:{gid}:carol"), line.as_str().unwrap()).is_none());
        // a teammate can't post into a group they're not in, even with a box that opened
        let mut bob2 = keyed("bob", &b, &[("alice", &a_pub), ("carol", &c_pub)]);
        bob2.groups = bob.groups.clone();
        let mut fake = published(&alice, &a_pub);
        fake.login = "carol".into();
        fake.pub_key = c_pub.clone();
        fake.dms.clear();
        fake.groups.clear();
        assert!(!take_private(&mut bob2, "carol", &fake), "carol's name on alice's group line: not a member");
    }

    #[test]
    fn unread_counts_and_pings_per_channel() {
        let ((a, a_pub), (b, b_pub)) = (member("alice"), member("bob"));
        let mut alice = keyed("alice", &a, &[("bob", &b_pub)]);
        dm_locked(&mut alice, "bob", "hi").unwrap();
        dm_locked(&mut alice, "bob", "@bob lunch?").unwrap();
        group_locked(&mut alice, "core", &["bob".into()]).unwrap();
        let gid = alice.groups.keys().next().unwrap().clone();
        group_send_locked(&mut alice, &gid, "plain line").unwrap();
        assert_eq!(unread_locked(&alice), 0, "your own lines aren't unread");
        let mut bob = keyed("bob", &b, &[("alice", &a_pub)]);
        let folder = published(&alice, &a_pub);
        take_private(&mut bob, "alice", &folder);
        take_private(&mut bob, "alice", &folder); // the same folder again: nothing new
        assert_eq!(unread_locked(&bob), 3);
        let ch = channels_locked(&bob);
        let at = ch.iter().find(|c| c.id == "@alice").unwrap();
        assert_eq!((at.unread, at.pinged), (2, true));
        let g = ch.iter().find(|c| c.id == format!("+{gid}")).unwrap();
        assert_eq!((g.unread, g.pinged, g.label.as_str()), (1, false, "+ core"));
        assert_eq!(ch[0].id, "#team");
        read_locked(&mut bob, "@alice");
        assert_eq!(unread_locked(&bob), 1);
        assert!(bob.pinged.is_empty());
    }

    #[test]
    fn pings_are_words_not_email_addresses() {
        assert!(pings("@bob can you look", "bob"));
        assert!(pings("thanks @Bob!", "bob"), "case doesn't matter");
        assert!(pings("over to you, @bob.", "bob"), "a full stop after the name");
        assert!(pings("(@bob)", "bob"));
        assert!(pings("@team standup in 5", "bob"));
        assert!(!pings("mail bob@bob.com", "bob"), "an email address");
        assert!(!pings("see @bob.com", "bob"), "a domain");
        assert!(!pings("@bobby are you there", "bob"), "someone else");
        assert!(!pings("@teams", "bob"));
        assert!(!pings("no mention", "bob"));
        assert!(!pings("@", "bob"));
    }
}
