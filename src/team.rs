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
//!
//! Two threads: one holds the line, the other does everything that waits on git or GitHub, so a
//! stalled network can't hold up the line. Folders are kept in a bare repository of their own in
//! the state folder, not in the project's .git. While a teammate has one of our sessions open,
//! its transcript goes to them over the line too, only what changed.
use crate::backend::http;
use crate::teamcrypto::{self as tc, Key};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API: &str = "https://api.github.com";
const MIN_WRITE: Duration = Duration::from_secs(10);
const QUIET_WRITE: Duration = Duration::from_secs(30); // everyone who's online has it over the line: git is only the fallback
const MIN_LINK_WRITE: Duration = Duration::from_secs(1); // floor for the address-change exemption
const KEEPALIVE: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(3);
const GIT_NET_LIMIT: Duration = Duration::from_secs(30); // a push or fetch stuck longer is stopped and tried again
const LIVE_TRANSCRIPTS: &str = "0.2.8"; // the first version that asks for transcripts over the line
const WATCH_EVERY: Duration = Duration::from_secs(5); // while a teammate's session is open, say so this often
const WATCH_LIFE: Duration = Duration::from_secs(20); // and stop sending when they've gone quiet this long
const TR_EVERY: Duration = Duration::from_secs(1); // a watched transcript goes out at most this often
const TR_LOST_AFTER: Duration = Duration::from_secs(15); // an update they haven't got by now isn't coming
const GC_LIMIT: Duration = Duration::from_secs(60);
const LOOSE_TIDY: u64 = 2000; // loose objects in a store at startup that make it worth tidying
const BOARD_TIDY: u32 = 200; // writes between clearing out old folders from the board store
const OFFLINE_AFTER: u64 = 300; // seconds without a write before someone counts as offline
const ORPHAN_AFTER: u64 = 1800; // seconds every key holder must be gone before an admin starts a new key
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
    pub unmerged: Claims,                    // what their copy changes that isn't on main yet, however it got changed
    pub uses: HashMap<String, HashMap<String, u64>>, // session -> names its fresh code uses -> when
    pub waiting: Vec<Wait>,                 // their sessions paused on someone
    sheets: HashMap<String, Sheet>,         // session -> what it decided and does next
    promises: Vec<Promise>,                 // what their sessions count on staying true
    link: Vec<String>,                      // where their qh-link listens ("lan=1.2.3.4:5"), sealed like the rest
    link_ms: u64,                           // when those addresses were current (their clock, ms)
    focus: u64,                             // focusing till then (0: not)
    seq: u64,                               // when this copy was made (ms): an older git copy can't undo a live one
    sign: String,                           // their signing key (Ed25519), named in the folder
    anchor: String,                         // the commit GitHub vouches for their keys on
    signed: bool,                           // the folder's signature checks out against `sign`
    qh: String,                             // the qh version they run (empty before 0.1.4)
    team_notes: String,                     // their copy of the team notes (see team_notes)
    tr_seq: HashMap<String, u64>,           // session -> when its transcript last came over the line (their ms): an older git copy can't undo it
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
    pub kind: String,         // handover | question
    pub history: Vec<(Status, u64)>, // delivered when it reached us, then seen, replied…
    pub snoozed: bool,        // "later": off the card, still counted
    pub until: u64,           // ...till this time, then back on the card (0: till you pick it)
    pub packet: Value,        // a handover's packet from the sender's agent: done, left, files, watch, branch
    pub draft: Option<(String, Vec<String>)>, // a question our agent answered with a promise, held for you: (answer, names)
}

impl Incoming {
    /// When it reached us (its send time when older data has no history).
    pub fn arrived(&self) -> u64 {
        self.history.first().map_or(self.ts, |h| h.1)
    }
    pub fn seen(&self) -> bool {
        self.history.iter().any(|h| h.0 == Status::Seen)
    }
}

/// Where a request is, the same words on both sides. The sender learns each step from the
/// receiver's status messages; "queued" is the sender's own until the first one comes back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status { #[default] Queued, Delivered, Seen, Replied, Accepted, Declined, Withdrawn, Expired }

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued", Status::Delivered => "delivered", Status::Seen => "seen", Status::Replied => "replied",
            Status::Accepted => "accepted", Status::Declined => "declined", Status::Withdrawn => "withdrawn", Status::Expired => "expired",
        }
    }
    pub fn parse(s: &str) -> Option<Status> {
        [Status::Queued, Status::Delivered, Status::Seen, Status::Replied, Status::Accepted, Status::Declined, Status::Withdrawn, Status::Expired]
            .into_iter().find(|x| x.as_str() == s)
    }
    /// Nothing more happens to it.
    pub fn done(self) -> bool {
        matches!(self, Status::Accepted | Status::Declined | Status::Withdrawn | Status::Expired)
    }
}

/// A request we sent, and what became of it.
#[derive(Clone, Debug, Default)]
pub struct Outgoing {
    pub id: u64,
    pub to: String,
    pub session: String,
    pub text: String,
    pub kind: String,
    pub from_session: String,
    pub ts: u64,
    pub history: Vec<(Status, u64)>,
    pub reply: String, // their latest words back (a reply, or why they declined)
    pub packet: Value, // the handover packet it went with (Null if none), so a resend keeps it
    pub was: Vec<u64>, // ids it went under before a resend, so their news about those still lands here
}

impl Outgoing {
    pub fn status(&self) -> Status {
        self.history.last().map_or(Status::Queued, |h| h.0)
    }
}

/// Answers, reviews and mentions: worth knowing, nothing to decide.
#[derive(Clone, Debug)]
pub struct Fyi {
    pub ts: u64,
    pub kind: String, // answer | review | mention | reply
    pub text: String,
}

/// What came in since qh was last open in this project.
#[derive(Clone, Debug, Default)]
pub struct Away {
    pub since_ts: u64,
    pub needs_you: Vec<Incoming>,
    pub fyi: Vec<Fyi>,
}

/// What the UI is told without asking.
#[derive(Clone, Debug)]
pub enum TeamEvent {
    Arrived { from: String, id: u64 },
    Gone { from: String, id: u64 },     // withdrawn or expired on their side
    Outgoing { id: u64, status: Status },
}

/// What we do with a waiting request. Later's time: when the card comes back (0: when you pick it).
#[derive(Clone, Debug)]
pub enum Action { Accept, Always, Reply(String), Decline(Option<String>), Later(u64) }

/// An answer our agent sent a teammate without asking us.
#[derive(Clone, Debug, Default)]
pub struct Answered {
    pub ts: u64,
    pub to: String,
    pub question: String,
    pub answer: String,
    pub promise: bool, // it committed us to something (PROMISE): only trusted teammates get those alone
    pub taken_back: bool,
    pub session: String,      // their session that asked: a correction goes to its agent
    pub relied: Vec<String>,  // names the answer counted on: taking it back ends those contracts
}

/// Steps 6-8: handovers you took, what your agent answered alone, trust topics, focus.
#[derive(Default)]
struct Flow {
    taken: Vec<Incoming>,                 // handovers you accepted, newest last (the last 30)
    approved: Vec<(String, u64)>,         // questions you said yes to: their answers go out whatever they promise
    log: Vec<Answered>,                   // the last 100 answers sent without you
    topics: HashMap<String, Vec<String>>, // trusted login -> topics their promises may cover (none: any)
    focus: u64,                           // holding cards and notifications till then
    focus_told: bool,                     // the "focus over" note went out
}

const REQUEST_LIFE: u64 = 14 * 86400; // a request nobody acted on expires after two weeks

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

const OUTBOX_MAX: usize = 300; // a teammate gone for good never acks: this rides in every folder and line message
const CHAT_MAX: usize = 1000; // in memory; the file keeps the last 300
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
    sent: Vec<Outgoing>,        // our requests and their statuses (the last 100)
    fyi: Vec<Fyi>,              // answers, reviews, mentions (the last 50)
    last_open: u64,             // when qh last had this project open before this run
    away_until: u64,            // when the first read of the board finished; 0 until it has
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
    unmerged: Claims,                 // what our copy changes that isn't on main yet (from git), published
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
    cleared: HashMap<String, u64>,  // DM / group channel -> lines up to this time hidden (`x` in the chat)
    pinged: Vec<String>,            // channels with an unread @mention of us
    dirty: bool,                    // something to publish now (sent message, decision)
    flow: Flow,
    running: std::sync::Arc<AtomicBool>,
    finished: std::sync::Arc<AtomicBool>,
    watching: Option<(String, String, Instant)>, // the teammate's session on screen (login, session), and when it was last drawn
    watch_now: bool,                  // what we hold of it doesn't match theirs: ask again without waiting
}

/// Bumped whenever team state a client shows may have changed (board read, live line, chat,
/// error), so `qh serve` can push the team room instead of being polled.
static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn bump() { GEN.fetch_add(1, Relaxed); }
pub fn generation() -> u64 { GEN.load(Relaxed) }

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

/// `d` ago, or just now if the clock doesn't go back that far: on Windows it starts at boot, and
/// `Instant::now() - d` would panic.
fn instant_ago(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_sub(d).unwrap_or(now)
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn git_in(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, String)]) -> Option<String> {
    git_raw(root, args, input, env).map(|o| String::from_utf8_lossy(&o).trim_end().to_string())
}

fn git_raw(root: &Path, args: &[&str], input: Option<&[u8]>, env: &[(&str, String)]) -> Option<Vec<u8>> {
    let mut c = git_cmd(root);
    c.args(args).env("GIT_TERMINAL_PROMPT", "0")
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

/// The team stores in use on this machine (see `board_store_in`). A list, not a look at the
/// folder: these wrappers also run in folders we didn't make, and a bare repository that came
/// with a clone must stay refused where the user's git refuses it (`safe.bareRepository`).
static STORES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn is_store(dir: &Path) -> bool {
    STORES.lock().unwrap().iter().any(|d| d == dir)
}

/// git in `root`. A store is named outright, and whatever the environment says about some other
/// repository doesn't apply to it. `--git-dir` alone wouldn't do that: it overrides `GIT_DIR` but
/// not, say, `GIT_OBJECT_DIRECTORY`.
fn git_cmd(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(root);
    if is_store(root) {
        c.arg("--git-dir=.");
        for v in GIT_LOCAL_ENV { c.env_remove(v); }
    }
    c
}

/// Run a command that may stall for as long as the network does, and stop it (helpers too) after
/// `limit`, or as soon as `stop` is set. If it didn't work, says how in words fit to show: never
/// what it ran, or its output (which goes nowhere: `http.extraHeader` carries the token).
fn run_limited(mut c: Command, limit: Duration, stop: Option<&AtomicBool>) -> Result<(), String> {
    let stopped = || stop.is_some_and(|f| f.load(Relaxed));
    if stopped() {
        return Err("stopped".into()); // the board's thread can get this far after the team was told to go
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    { use std::os::unix::process::CommandExt; c.process_group(0); }
    let Ok(mut child) = c.spawn() else { return Err("couldn't start it".into()) };
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(st.code().map_or("ended by a signal".into(), |c| format!("exit code {c}"))),
            Err(e) => return Err(format!("lost track of it: {e}")),
            Ok(None) if stopped() => {
                stop_tree(&mut child, Duration::from_millis(300));
                return Err("stopped".into());
            }
            Ok(None) if t0.elapsed() >= limit => {
                // a short limit means someone is waiting on us: the call at quit has seconds at most
                stop_tree(&mut child, if limit < Duration::from_secs(5) { Duration::from_millis(300) } else { Duration::from_secs(2) });
                return Err(format!("timed out after {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// End a command and everything it started. git removes its lockfiles when asked (seen: SIGTERM
/// clears a held ref lock, SIGKILL leaves it), so ask, give it `grace`, then make sure of the rest.
#[cfg(unix)]
fn stop_tree(child: &mut std::process::Child, grace: Duration) {
    let group = -(child.id() as i32);
    unsafe { libc::kill(group, libc::SIGTERM); }
    let t0 = Instant::now();
    while matches!(child.try_wait(), Ok(None)) && t0.elapsed() < grace {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(50)); // helpers got the same request: their turn
    unsafe { libc::kill(group, libc::SIGKILL); }
    let _ = child.kill();
    let _ = child.wait();
}

/// End a command and everything it started. There's no asking on Windows: `/F` leaves no cleanup,
/// which is what `sweep_store` is for.
#[cfg(windows)]
fn stop_tree(child: &mut std::process::Child, _grace: Duration) {
    kill_tree_bounded(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// `taskkill /T` for at most 3 s: it's what we're using to get unstuck, so it mustn't stick too
/// (hence no `.output()`), and no console may flash up.
#[cfg(windows)]
fn kill_tree_bounded(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let Ok(mut k) = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn() else { return };
    let t0 = Instant::now();
    while matches!(k.try_wait(), Ok(None)) {
        if t0.elapsed() >= Duration::from_secs(3) {
            let _ = k.kill();
            let _ = k.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A push or fetch of the board.
fn git_net(root: &Path, args: &[&str], env: &[(&str, String)], limit: Duration, stop: Option<&AtomicBool>) -> Result<(), String> {
    let mut c = git_cmd(root);
    c.args(args).env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in env { c.env(k, v); }
    run_limited(c, limit, stop)
}

/// What `git rev-parse --local-env-vars` lists: settings that belong to one repository.
const GIT_LOCAL_ENV: [&str; 15] = ["GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_OBJECT_DIRECTORY",
    "GIT_DIR", "GIT_WORK_TREE", "GIT_IMPLICIT_WORK_TREE", "GIT_GRAFT_FILE", "GIT_INDEX_FILE", "GIT_NO_REPLACE_OBJECTS", "GIT_REPLACE_REF_BASE", "GIT_PREFIX",
    "GIT_SHALLOW_FILE", "GIT_COMMON_DIR"];

/// Clear out old folders from a store.
fn tidy_store(store: &Path, stop: Option<&AtomicBool>) -> bool {
    // not --prune=now: the board's thread may still be writing (see `leave_board`), and git warns
    // that risks corruption with a writer about
    is_store(store) && git_net(store, &["gc", "-q", "--prune=1.hour.ago"], &[], GC_LIMIT, stop).is_ok()
}

const SWEEP_AGE: Duration = Duration::from_secs(120);

/// Clear what a killed git left in a store. One stale ref lock makes every fetch fail, which holds
/// up everyone's folders, and our own if it comes before the first read. `/F` on Windows allows no
/// cleanup at all, and SIGTERM doesn't clear temp packs. The age is there because the team lock
/// means one qh, not one git: a git left over from before can be alive and holding these, and
/// then (on Windows) removing them fails, which is fine: the next failed fetch tries again.
fn sweep_store(store: &Path, age: Duration) {
    if !is_store(store) {
        return; // never a project's .git
    }
    let mut left = vec![store.join("packed-refs.lock")];
    let mut dirs = vec![store.join("refs/qh/board")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                dirs.push(e.path());
            } else if e.file_name().to_string_lossy().ends_with(".lock") {
                left.push(e.path());
            }
        }
    }
    for e in std::fs::read_dir(store.join("objects/pack")).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("tmp_pack_") {
            left.push(e.path());
        }
    }
    // a store made while git defaulted to reftable stays that way: it works, with its locks here
    for e in std::fs::read_dir(store.join("reftable")).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().ends_with(".lock") {
            left.push(e.path());
        }
    }
    for f in left {
        let old = std::fs::metadata(&f).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|e| e >= age);
        if old { let _ = std::fs::remove_file(&f); }
    }
}

/// The loose objects in what `git count-objects -v` says.
fn loose_objects(count_objects: &str) -> Option<u64> {
    count_objects.lines().find_map(|l| l.strip_prefix("count: "))?.trim().parse().ok()
}

/// Tidy a store with more than `most` loose objects. Says whether it did. For stores that never
/// reach `BOARD_TIDY` writes in one run, which is most of them.
fn tidy_if_big(store: &Path, most: u64, stop: Option<&AtomicBool>) -> bool {
    is_store(store) && git(store, &["count-objects", "-v"]).as_deref().and_then(loose_objects).is_some_and(|n| n > most) && tidy_store(store, stop)
}

/// After this many writes, tidy a store with more loose objects than the answer. The first look
/// comes after the first write: tidying can take a minute, and our address must be out by then.
fn tidy_limit(writes: u32) -> Option<u64> {
    if writes == 1 { Some(LOOSE_TIDY) } else { writes.is_multiple_of(BOARD_TIDY).then_some(0) }
}

/// Where the team's folders live on this machine: a bare repository of their own, so a team's
/// writes never touch the project's .git (no objects piling up, no cleanup set off there). Only
/// the folders travel, so it needs none of the project's history.
fn board_store(repo: &str, root: &Path) -> PathBuf {
    board_store_in(&per_repo("team-board", repo), root)
}

/// A store made while git defaulted to sha256 can't push to GitHub, or fetch any folder from it
/// ("mismatched algorithms"; with no folders to fetch, a fetch exits 0 and brings nothing). So
/// nothing in it ever went anywhere and nothing in it is the only copy: it holds folders we build
/// from our state, and the identity commit, which comes out the same each time.
/// We hold the team lock here, so no qh is using it. Moved aside first, since a rename either
/// happens or doesn't: a delete cut short by a file in use on Windows would leave half a store.
/// Aside means `team-board-old/<slug>`, next to `team-board`: same disk, and no repo's name can
/// reach it, as `<slug>.something` would (that's the store of a repo called that).
/// False if it's still there.
fn replace_sha256_store(dir: &Path) -> bool {
    if !dir.join("HEAD").exists() || git(dir, &["rev-parse", "--show-object-format"]).as_deref() != Some("sha256") {
        return true;
    }
    let (Some(boards), Some(name)) = (dir.parent(), dir.file_name()) else { return false };
    let mut old = boards.as_os_str().to_owned();
    old.push("-old");
    let old = PathBuf::from(old);
    let aside = old.join(name);
    let _ = std::fs::remove_dir_all(&aside);
    let moved = std::fs::create_dir_all(&old).is_ok() && std::fs::rename(dir, &aside).is_ok();
    let _ = std::fs::remove_dir_all(&aside);
    let _ = std::fs::remove_dir(&old); // only if nothing else is in it
    moved && std::fs::create_dir_all(dir).is_ok()
}

fn board_store_in(dir: &Path, root: &Path) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    { let mut all = STORES.lock().unwrap(); if !all.iter().any(|d| d == dir) { all.push(dir.to_path_buf()) } }
    // Files because `sweep_store` knows those, sha1 because GitHub is. These variables, not
    // `-c init.default...`: they beat that
    let formats = [("GIT_DEFAULT_REF_FORMAT", "files".to_string()), ("GIT_DEFAULT_HASH", "sha1".to_string())];
    let made = replace_sha256_store(dir)
        && (dir.join("HEAD").exists() || git_in(dir, &["init", "-q", "--bare"], None, &formats).is_some())
        && git(dir, &["rev-parse", "--git-dir"]).is_some();
    if made {
        // worth having, not worth leaving the store for: a stale config.lock only stops the writing,
        // and everything else works without it. The next start tries again
        for (k, v) in [("gc.auto", "0"), ("maintenance.auto", "false")] {
            if git(dir, &["config", k]).as_deref() != Some(v) {
                let _ = git(dir, &["config", k, v]);
            }
        }
        return dir.to_path_buf();
    }
    STORES.lock().unwrap().retain(|d| d != dir);
    root.to_path_buf()
}

/// Why a project's team data isn't in its own store, if it isn't. The `say` hook only reaches the
/// screen in `qh team`, and this has to reach everyone: see the caller of `start` in ui.rs.
pub fn store_warning(repo: &str) -> Option<String> {
    let dir = per_repo("team-board", repo);
    (!is_store(&dir)).then(|| format!("git can't use the team's own store ({}): team data goes in this project's .git until that's sorted out", dir.display()))
}

/// The repo's root and its GitHub owner/name, if `origin` is on GitHub.
pub fn github_repo(cwd: &Path) -> Option<(PathBuf, String)> {
    let root = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"])?);
    let url = git(&root, &["remote", "get-url", "origin"])?;
    Some((root, github_slug(&url)?))
}

fn github_slug(url: &str) -> Option<String> {
    // a URL, or git's short form for ssh: [user@]host:path
    let (authority, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            if !["https", "http", "ssh", "git"].iter().any(|k| scheme.eq_ignore_ascii_case(k)) { return None }
            rest.split_once('/')?
        }
        None => url.split_once(':').filter(|(a, _)| !a.contains('/'))?,
    };
    // these end the host before any @ does, whatever a parser that stops at the last @ would think
    if authority.contains(['?', '#', '\\']) { return None }
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match (host.split_once(':'), url.contains("://")) {
        (Some((h, port)), true) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        (None, _) => host,
        _ => return None,
    };
    // exactly these. Not an ssh alias like github.com-work: its name says nothing about where it
    // leads, and anyone can name a repo's remote that
    if !host.eq_ignore_ascii_case("github.com") && !host.eq_ignore_ascii_case("www.github.com") { return None }
    let slug = path.trim_start_matches('/').trim_end_matches('/').trim_end_matches(".git");
    valid_slug(slug).then(|| slug.to_string())
}

/// owner/name, each as GitHub allows it. These become folder names (`per_repo`), so nothing that
/// could lead out of the state folder gets through: on Windows `..\` works as well as `../`.
pub fn valid_slug(slug: &str) -> bool {
    let parts: Vec<&str> = slug.split('/').collect();
    parts.len() == 2 && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != ".."
        && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
}

/// `github_repo` for what runs on every frame: two git processes each time would be up to 360 a
/// second while a reply streams. A few seconds out of date is fine for showing things.
pub fn github_repo_cached(cwd: &Path) -> Option<(PathBuf, String)> {
    static SEEN: Mutex<Option<HashMap<PathBuf, (Instant, Option<(PathBuf, String)>)>>> = Mutex::new(None);
    let mut g = SEEN.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    if let Some((at, r)) = m.get(cwd) {
        if at.elapsed() < Duration::from_secs(5) { return r.clone() }
    }
    let r = github_repo(cwd);
    m.insert(cwd.to_path_buf(), (Instant::now(), r.clone()));
    r
}

/// A GitHub token: GH_TOKEN/GITHUB_TOKEN, the gh CLI's login, or git's stored credential.
pub fn token() -> Option<String> {
    for v in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(v) { if !t.is_empty() { return Some(t) } }
    }
    if cfg!(test) { return None } // a test has no business with the real login
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
fn publish(root: &Path, repo: &str, me: &str, tok: &str, folder: (&[(String, String)], &Value, &str), limit: Duration, stop: Option<&AtomicBool>) -> Result<String, String> {
    publish_to(root, &format!("https://github.com/{repo}.git"), me, &auth_env(tok), folder, limit, stop)
}

/// `publish` to any remote. `folder` is the files, the commit message and the parent.
fn publish_to(root: &Path, url: &str, me: &str, env: &[(&str, String)], (files, message, parent): (&[(String, String)], &Value, &str), limit: Duration, stop: Option<&AtomicBool>) -> Result<String, String> {
    push_commit(root, url, me, env, folder_commit(root, files, message, parent)?, limit, stop)
}

fn push_commit(root: &Path, url: &str, me: &str, env: &[(&str, String)], commit: String, limit: Duration, stop: Option<&AtomicBool>) -> Result<String, String> {
    git_net(root, &["push", "-q", "-f", url, &format!("{commit}:refs/qh/peers/{me}")], env, limit, stop)
        .map(|_| commit).map_err(|why| format!("push to the board failed: {why}"))
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
/// `stop` is looked at after each question to GitHub (15 s): by then quitting may have gone ahead,
/// and what's on disk may be the next holder's.
fn vet(repo: &str, tok: &str, p: &Peer, hooks: &Hooks, stop: &AtomicBool) -> bool {
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
            if !confirmed && vouched(repo, tok, p) && !stop.load(Relaxed) {
                pin_keys(repo, p, true);
            }
            true
        }
        Some((old, _, _)) => {
            let vouches = vouched(repo, tok, p);
            if stop.load(Relaxed) { return false }
            if vouches {
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
            if stop.load(Relaxed) { return false }
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
#[derive(Clone)]
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
fn fetch_board(root: &Path, repo: &str, tok: &str, limit: Duration, stop: Option<&AtomicBool>) -> Result<Vec<(String, String)>, String> {
    fetch_refs(root, &format!("https://github.com/{repo}.git"), &fetch_env(tok), limit, stop)
}

/// `fetch_board` from any remote.
fn fetch_refs(root: &Path, url: &str, env: &[(&str, String)], limit: Duration, stop: Option<&AtomicBool>) -> Result<Vec<(String, String)>, String> {
    if let Err(why) = git_net(root, &["fetch", "-q", "--no-tags", "--prune", url, "+refs/qh/peers/*:refs/qh/board/*"], env, limit, stop) {
        sweep_store(root, SWEEP_AGE);
        return Err(format!("fetching the board failed: {why}"));
    }
    list_board(root)
}

fn list_board(root: &Path) -> Result<Vec<(String, String)>, String> {
    // no answer isn't an empty board: being first on an empty board makes the team key
    let list = git(root, &["for-each-ref", "--format=%(refname:strip=3) %(objectname)", "refs/qh/board/"]).ok_or("git couldn't list the board")?;
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
    let claims_of = |files: &Value| -> Claims { files.as_object().into_iter().flatten().map(|(f, fns)| (f.clone(),
        fns.as_object().into_iter().flatten().map(|(n, t)| (n.clone(), t.as_u64().unwrap_or(0))).collect())).collect() };
    if let Some(c) = d["claims"].as_object() {
        p.claims = c.iter().map(|(sess, files)| (sess.clone(), claims_of(files))).collect();
    }
    p.unmerged = claims_of(&d["unmerged"]);
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
    p.focus = d["focus"].as_u64().unwrap_or(0);
    p.team_notes = d["team_notes"].as_str().unwrap_or("").to_string();
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

/// Drop a teammate's topics where they're kept: this process's copy when the team is open here,
/// and the file an earlier run left, so a later trust can't bring them back.
fn forget_topics(repo: &str, login: &str) {
    with(repo, |s| { if s.flow.topics.remove(login).is_some() { s.dirty = true } });
    let Ok(body) = std::fs::read_to_string(per_repo("team-requests", repo)) else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&body) else { return };
    let Some(topics) = v.get_mut("topics").and_then(Value::as_object_mut) else { return };
    if topics.remove(login).is_none() { return }
    if let Some(g) = SAVED.lock().unwrap().as_mut() { g.remove(repo); } // what it last wrote is older than this
    save_json("team-requests", repo, &v);
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
    with(repo, |s| {
        s.roles = roles;
        s.dirty = true;
    });
    // topics narrow a trust: with the trust gone they mean nothing, and must not come back with the next one
    if role != "trusted" { forget_topics(repo, login) }
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

fn history_json(h: &[(Status, u64)]) -> Value {
    h.iter().map(|(st, ts)| json!([st.as_str(), ts])).collect::<Vec<_>>().into()
}

fn history_of(v: &Value) -> Vec<(Status, u64)> {
    v.as_array().into_iter().flatten()
        .filter_map(|x| Some((Status::parse(x[0].as_str()?)?, x[1].as_u64()?))).collect()
}

fn incoming_json(i: &Incoming) -> Value {
    json!({"from": i.from, "id": i.id, "session": i.session, "text": i.text, "ts": i.ts, "from_session": i.from_session,
           "replied": i.replied, "kind": i.kind, "snoozed": i.snoozed, "until": i.until, "packet": i.packet,
           "draft": i.draft.as_ref().map(|d| json!([d.0, d.1])), "history": history_json(&i.history),
           "question": i.question.as_ref().map(|q| json!({"repo": q.repo, "from": q.from, "from_session": q.from_session,
                                                           "id": q.id, "session": q.session, "text": q.text}))})
}

fn incoming_of(v: &Value) -> Incoming {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let q = &v["question"];
    Incoming {
        from: s("from"), id: v["id"].as_u64().unwrap_or(0), session: s("session"), text: s("text"), ts: v["ts"].as_u64().unwrap_or(0),
        from_session: s("from_session"), replied: v["replied"].as_bool().unwrap_or(false), kind: s("kind"),
        snoozed: v["snoozed"].as_bool().unwrap_or(false), history: history_of(&v["history"]),
        until: v["until"].as_u64().unwrap_or(0), packet: v["packet"].clone(),
        draft: v["draft"][0].as_str().map(|a| (a.to_string(), v["draft"][1].as_array().into_iter().flatten()
            .filter_map(|x| x.as_str().map(String::from)).collect())),
        question: q.is_object().then(|| Question {
            repo: q["repo"].as_str().unwrap_or("").into(), from: q["from"].as_str().unwrap_or("").into(),
            from_session: q["from_session"].as_str().unwrap_or("").into(), id: q["id"].as_u64().unwrap_or(0),
            session: q["session"].as_str().unwrap_or("").into(), text: q["text"].as_str().unwrap_or("").into() }),
    }
}

fn outgoing_json(o: &Outgoing) -> Value {
    json!({"id": o.id, "to": o.to, "session": o.session, "text": o.text, "kind": o.kind, "from_session": o.from_session,
           "ts": o.ts, "history": history_json(&o.history), "reply": o.reply, "packet": o.packet, "was": o.was})
}

fn outgoing_of(v: &Value) -> Outgoing {
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    Outgoing { id: v["id"].as_u64().unwrap_or(0), to: s("to"), session: s("session"), text: s("text"), kind: s("kind"),
               from_session: s("from_session"), ts: v["ts"].as_u64().unwrap_or(0), history: history_of(&v["history"]), reply: s("reply"),
               packet: if v["packet"].is_object() { v["packet"].clone() } else { Value::Null },
               was: v["was"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect() }
}

/// What's in flight survives a restart: our outbox (else a request to someone offline is gone the
/// moment we restart before they read it), what waits for us, what we sent, and when we were last open.
struct Saved { outbox: Vec<Value>, pending: Vec<Incoming>, sent: Vec<Outgoing>, fyi: Vec<Fyi>, ts: u64, flow: Flow }

fn answered_json(a: &Answered) -> Value {
    json!({"ts": a.ts, "to": a.to, "q": a.question, "a": a.answer, "promise": a.promise, "back": a.taken_back, "session": a.session, "relied": a.relied})
}

fn answered_of(v: &Value) -> Answered {
    Answered { ts: v["ts"].as_u64().unwrap_or(0), to: v["to"].as_str().unwrap_or("").into(), question: v["q"].as_str().unwrap_or("").into(),
               answer: v["a"].as_str().unwrap_or("").into(), promise: v["promise"].as_bool().unwrap_or(false),
               taken_back: v["back"].as_bool().unwrap_or(false), session: v["session"].as_str().unwrap_or("").into(),
               relied: v["relied"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect() }
}

fn load_requests(repo: &str) -> Saved {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(per_repo("team-requests", repo)).unwrap_or_default()).unwrap_or_default();
    let arr = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    Saved {
        outbox: arr("outbox"),
        pending: arr("pending").iter().map(incoming_of).collect(),
        sent: arr("sent").iter().map(outgoing_of).collect(),
        fyi: arr("fyi").iter().map(|f| Fyi { ts: f["ts"].as_u64().unwrap_or(0), kind: f["kind"].as_str().unwrap_or("").into(),
                                             text: f["text"].as_str().unwrap_or("").into() }).collect(),
        ts: v["ts"].as_u64().unwrap_or(0),
        flow: Flow {
            taken: arr("taken").iter().map(incoming_of).collect(),
            log: arr("answered").iter().map(answered_of).collect(),
            topics: serde_json::from_value(v["topics"].clone()).unwrap_or_default(),
            focus: v["focus"].as_u64().unwrap_or(0),
            focus_told: v["focus_told"].as_bool().unwrap_or(false),
            approved: vec![],
        },
    }
}

static SAVED: Mutex<Option<HashMap<String, (String, u64)>>> = Mutex::new(None);

/// Written when something changed, and once a minute anyway (the time says when we were last open).
fn save_requests(s: &State) {
    let mut v = json!({
        "outbox": s.outbox,
        "pending": s.pending.iter().map(incoming_json).collect::<Vec<_>>(),
        "sent": s.sent.iter().map(outgoing_json).collect::<Vec<_>>(),
        "fyi": s.fyi.iter().map(|f| json!({"ts": f.ts, "kind": f.kind, "text": f.text})).collect::<Vec<_>>(),
        "taken": s.flow.taken.iter().map(incoming_json).collect::<Vec<_>>(),
        "answered": s.flow.log.iter().map(answered_json).collect::<Vec<_>>(),
        "topics": s.flow.topics, "focus": s.flow.focus, "focus_told": s.flow.focus_told,
    });
    let body = v.to_string();
    let mut g = SAVED.lock().unwrap();
    let last = g.get_or_insert_with(HashMap::new).entry(s.repo.clone()).or_default();
    if last.0 == body && now().saturating_sub(last.1) < 60 { return }
    *last = (body, now());
    v["ts"] = json!(now());
    save_json("team-requests", &s.repo, &v);
}

/// Tell whoever sent request `re` where it is now (rides our next write, like any message).
/// A qh before 0.2.6 reads kind "s" as a new request (an empty card), so it gets the old private
/// line for a reply or a decline and nothing for the rest.
fn status_out(s: &mut State, to: &str, re: u64, st: Status, text: &str) {
    let what = s.pending.iter().chain(&s.flow.taken).find(|i| i.from == to && i.id == re).map(|i| i.text.clone());
    status_about(s, to, re, st, text, what.as_deref().unwrap_or(""))
}

/// `status_out` for request `re`, whose words were `what` (for an older qh's line).
fn status_about(s: &mut State, to: &str, re: u64, st: Status, text: &str, what: &str) {
    if !knows_status(s, to) {
        let what = if what.is_empty() { "your request".to_string() } else { clip_to(bare(what), 80) };
        let me = s.me.clone();
        let line = match st {
            Status::Replied => format!("{me} re your request \"{what}\": {text}"),
            Status::Declined if text.is_empty() => format!("{me} declined: \"{what}\""),
            Status::Declined => format!("{me} declined: \"{what}\": {text}"),
            _ => return,
        };
        if let Err(e) = dm_locked(s, to, &line) { log_to(&s.repo.clone(), &format!("couldn't tell {to}: {e}")) }
        return;
    }
    s.next_id += 1;
    s.outbox.push(json!({"id": s.next_id, "to": to, "kind": "s", "re": re, "status": st.as_str(), "text": text, "ts": now()}));
    s.dirty = true;
}

/// Whether their qh reads request status and withdraw messages (kinds "s", "w"): 0.2.6 on.
fn knows_status(s: &State, to: &str) -> bool {
    s.peers.get(to).is_none_or(|p| !p.qh.is_empty() && !crate::update::newer("0.2.6", &p.qh))
}

fn push_fyi(s: &mut State, kind: &str, text: String) {
    s.fyi.push(Fyi { ts: now(), kind: kind.into(), text });
    let n = s.fyi.len();
    if n > 50 { s.fyi.drain(..n - 50); }
}

/// Requests nobody acted on for two weeks: gone, and the sender is told.
/// Over the cap: news about requests goes first, then the oldest of what's left.
fn trim_outbox(s: &mut State) {
    let mut over = s.outbox.len().saturating_sub(OUTBOX_MAX);
    if over == 0 { return }
    s.outbox.retain(|m| { let drop = over > 0 && m["kind"] == "s"; if drop { over -= 1 } !drop });
    s.outbox.drain(..over);
    s.dirty = true;
}

fn expire_locked(s: &mut State) -> Vec<(String, u64)> {
    let old: Vec<(String, u64)> = s.pending.iter().filter(|i| now().saturating_sub(i.arrived()) > REQUEST_LIFE)
        .map(|i| (i.from.clone(), i.id)).collect();
    for (from, id) in &old {
        decide_locked(s, from, *id, "expired");
    }
    trim_outbox(s);
    old
}

/// Cards put off till a time come back at that time; focus ends at its time, with one note of what waited.
fn wake_locked(s: &mut State) {
    let t = now();
    for i in s.pending.iter_mut().filter(|i| i.snoozed && i.until > 0 && i.until <= t) {
        i.snoozed = false;
        i.until = 0;
        alert(format!("qh: back to {}'s request: {}", i.from, clip_to(bare(&i.text), 80)));
        s.dirty = true;
        bump();
    }
    if s.flow.focus > 0 && s.flow.focus <= t && !s.flow.focus_told {
        s.flow.focus_told = true;
        let n = s.pending.iter().filter(|i| !i.snoozed).count();
        if n > 0 { ALERTS.lock().unwrap().push(format!("qh: focus over · {n} waiting for you")) }
        s.dirty = true;
        bump(); // an idle screen still says "focusing" otherwise
    }
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
/// A hook that takes a session and a text.
pub type PairHook = Box<dyn Fn(&str, &str) + Send + Sync>;

pub struct Hooks {
    pub say: Box<dyn Fn(String) + Send + Sync>,
    pub deliver: PairHook,
    pub ask: Box<dyn Fn(&Incoming) + Send + Sync>,
    pub refresh: Box<dyn Fn() + Send + Sync>, // redraw: the status line changed
    /// Answer a teammate's question from a read-only copy of the asked session (returns at once).
    pub answer: std::sync::Arc<dyn Fn(Question) + Send + Sync>,
    /// An answer came back for one of our sessions: hand it over (now, or before its next call).
    pub answered: PairHook,
    /// A request arrived or left, or one of ours moved on: redraw what shows it.
    pub changed: Box<dyn Fn(TeamEvent) + Send + Sync>,
}

/// Connect this folder's project (or just show it, if it's already connected).
pub fn start(cwd: &Path, summary: Summary, hooks: Hooks, background: bool) -> Result<String, String> {
    let (root, repo) = github_repo(cwd).ok_or("this folder's git remote isn't on GitHub (other hosts come later)")?;
    crate::teamsetup::not_a_project(&root)?; // never a team on your home folder, however it got a remote
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
    // first the connection: waiting for it can take 15 s, and what we load below must include what
    // the qh we waited for saved on its way out. A qh that loses doesn't touch the store either.
    hold_connection(&repo, background)?;
    let board = board_store(&repo, &root);
    let Some(idc) = identity_commit(&board, &me, &my_pub, &sign_pub) else {
        release(&repo);
        return Err("git couldn't create your identity commit".into());
    };
    if board != root {
        git(&board, &["update-ref", "refs/qh/me", &idc]); // every folder's parent: tidying keeps it
    }
    let seen: HashMap<String, u64> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-seen", &repo)).unwrap_or_default()).unwrap_or_default();
    let invites: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-invites", &repo)).unwrap_or_default()).unwrap_or_default();
    let chat: Vec<(u64, String, String)> = serde_json::from_str(&std::fs::read_to_string(per_repo("team-chat", &repo)).unwrap_or_default()).unwrap_or_default();
    let mine: Vec<&(u64, String, String)> = chat.iter().filter(|(_, f, _)| *f == me).collect();
    let my_chat: Vec<Value> = mine[mine.len().saturating_sub(30)..].iter().copied()
        .enumerate().map(|(i, (ts, _, t))| json!({"id": ts * 1000 + i as u64, "ts": ts, "text": t})).collect();
    let saved = load_requests(&repo);
    // our ids keep rising across runs, past anything still in flight
    let next_id = saved.outbox.iter().filter_map(|m| m["id"].as_u64()).chain(saved.sent.iter().map(|o| o.id))
        .max().unwrap_or(0).max(now() * 1000);
    let (running, finished) = (std::sync::Arc::new(AtomicBool::new(true)), std::sync::Arc::new(AtomicBool::new(false)));
    STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), State {
        repo: repo.clone(), me: me.clone(), peers: HashMap::new(), error: String::new(), key: load_key(&repo),
        outbox: saved.outbox, next_id, seen, pending: saved.pending, sent: saved.sent, fyi: saved.fyi, last_open: saved.ts, away_until: 0, flow: saved.flow,
        roles: load_roles(&repo),
        approved: load_list("team-approved", &repo), asking: vec![], invites, chat,
        epoch: std::fs::read_to_string(per_repo("team-epoch", &repo)).ok().and_then(|e| e.trim().parse().ok()).unwrap_or(0),
        removed: load_list("team-removed", &repo), admins: HashMap::new(),
        pins: serde_json::from_str(&std::fs::read_to_string(per_repo("team-pins", &repo)).unwrap_or_default()).unwrap_or_default(),
        gh_login: gh_login.clone(), git_ms: HashMap::new(), asked_github: HashMap::new(), warned: vec![], my_sha: String::new(), my_chat, chat_seen: HashMap::new(), unread: 0,
        my_notes: vec![], notes_seen: HashMap::new(), claims: HashMap::new(), unmerged: HashMap::new(), waiting: vec![], uses: HashMap::new(),
        sheets: HashMap::new(), asked: vec![], answered: vec![], answerer: Some(hooks.answer.clone()),
        promises: serde_json::from_str::<Vec<Value>>(&std::fs::read_to_string(per_repo("team-promises", &repo)).unwrap_or_default())
            .unwrap_or_default().iter().map(promise_of).collect(),
        last_answer: HashMap::new(),
        answers: std::fs::read_to_string(per_repo("team-answers", &repo)).ok().map(|s| s.trim().to_string())
            .filter(|s| ["auto", "ask", "off"].contains(&s.as_str())).unwrap_or_else(|| "auto".into()),
        secret: Some(secret.clone()), my_dms: vec![], my_groups: vec![], my_gmsgs: vec![], groups: HashMap::new(), msgs: vec![],
        msg_ids: Default::default(), chan_unread: HashMap::new(), cleared: HashMap::new(), pinged: vec![],
        dirty: true, running: running.clone(), finished: finished.clone(), watching: None, watch_now: false,
    });
    with(&repo, load_private);
    with(&repo, |s| FOCUS.store(s.flow.focus, Relaxed));
    crate::review::load_held(&repo); // pushes of ours still waiting on an owner, from before a restart
    set_current(&repo);
    let (repo2, me2) = (repo.clone(), me.clone());
    let (live_tx, live_rx) = std::sync::mpsc::channel();
    LIVE.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), live_tx);
    let line = link();
    let hooks = std::sync::Arc::new(hooks);
    if let Some(w) = store_warning(&repo) { log_to(&repo, &w) }
    let latest: std::sync::Arc<Mutex<(Option<Snapshot>, bool)>> = Default::default(); // the folder to write next, and whether something asked for a write
    let stop = std::sync::Arc::new(AtomicBool::new(false)); // the board's git calls end now, not at their limit
    let fetched = std::sync::Arc::new(AtomicBool::new(false)); // we've read the board at least once
    // The board's thread: everything that waits on git or GitHub. It can stall for as long as the
    // network does, and the line must not stall with it.
    let reader = {
        let (repo2, me2, root, board, tok, secret, sign_pub, my_pub, idc, hooks, running, latest, line_up, stop, fetched) =
            (repo.clone(), me.clone(), root.clone(), board.clone(), tok.clone(), secret.clone(), sign_pub.clone(), my_pub.clone(), idc.clone(), hooks.clone(), running.clone(), latest.clone(), line.is_some(), stop.clone(), fetched.clone());
        std::thread::Builder::new().name(format!("qh-board-{repo}")).spawn(move || {
            let mut etag: Option<String> = None;
            let (mut last_files, mut last_write, mut last_poll): (Vec<(String, String)>, _, _) = (vec![], instant_ago(KEEPALIVE), instant_ago(POLL));
            let mut last_link_ms = 0u64; // the address stamp in the last folder we wrote
            let (mut retry, mut push) = (FetchRetry::new(FETCH_FIRST, Instant::now()), FetchRetry::new(PUSH_FIRST, Instant::now()));
            let mut folders: HashMap<String, (String, Folder)> = HashMap::new(); // login -> sha, its files: a commit never changes, so read each once
            let (mut snap, mut wanted, mut last_data): (Option<Snapshot>, bool, Option<Value>) = (None, false, None);
            let (mut failed, mut failed_at, mut writes) = (LINE_FAILED.load(Relaxed), None::<Instant>, 0u32);
            while running.load(Relaxed) {
                // read the board: one conditional request, a git fetch only when something changed
                if last_poll.elapsed() >= POLL {
                    last_poll = Instant::now();
                    let reply = api("GET", &format!("/repos/{repo2}/git/matching-refs/qh/peers/"), &tok, None, etag.as_deref());
                    if stop.load(Relaxed) { break }
                    if take_listing(&repo2, &me2, reply, &folders, &mut etag, &mut retry) && retry.due(Instant::now()) {
                        match fetch_board(&board, &repo2, &tok, fetch_limit(retry.fails, fetched.load(Relaxed)), Some(&stop)) {
                            Ok(_) | Err(_) if stop.load(Relaxed) => break,
                            Ok(list) => {
                                retry.worked(Instant::now());
                                if push.fails == 0 { with(&repo2, |s| s.error.clear()); }
                                let was = fetched.load(Relaxed);
                                take_in_board(&fetched, &stop, || absorb(&repo2, &board, &list, &mut folders, &secret, &tok, &hooks, &stop));
                                caught_up(&repo2, was, &fetched);
                                bump();
                                (hooks.refresh)();
                            }
                            Err(e) => { retry.failed(Instant::now()); set_error(&repo2, e) }
                        }
                    }
                    if stop.load(Relaxed) { break } // quitting went ahead without us: everything below is someone else's now
                    stale_check(&repo2, &hooks);
                    wake_check(&repo2, &root, &hooks);
                }
                // write my folder: when it changed (throttled), when asked (dirty), or as a keep-alive.
                // Each write is a push (GitHub asks for at most 6 a minute per repository), so with
                // everyone on the line data changes go out less often, and a change to a
                // transcript alone waits for the next one: whoever's watching has it over the
                // line. Who's in the team and addresses never wait.
                {
                    let mut l = latest.lock().unwrap();
                    if let Some(n) = l.0.take() { snap = Some(n) }
                    wanted |= std::mem::take(&mut l.1);
                }
                if stop.load(Relaxed) { break }
                if LINE_FAILED.load(Relaxed) != failed {
                    failed = LINE_FAILED.load(Relaxed);
                    failed_at = Some(Instant::now());
                }
                if let Some(n) = &snap {
                    let plain = |f: &[(String, String)]| -> Vec<String> { f.iter().filter(|(n, _)| n != "sig").map(|(n, c)| if n == "data" || n.contains('/') { n.clone() } else { format!("{n}={c}") }).collect() };
                    let data = |f: &[(String, String)]| f.iter().find(|(n, _)| n == "data").map(|(_, c)| c.clone());
                    let same = plain(&n.files) == plain(&last_files);
                    let by_line = line_up && transcripts_by_line(&repo2);
                    let changed = !same || if by_line && n.quick.is_some() { n.quick != last_data } else { data(&n.files) != data(&last_files) };
                    let gap = if same && failed_at.is_none_or(|t| t.elapsed() >= QUIET_WRITE) && line_up && everyone_on_line(&repo2) { QUIET_WRITE } else { MIN_WRITE };
                    // new line addresses skip the throttle: until they're out, teammates can't reach us directly
                    let link_ms = link().map_or(0, |l| l.addrs_ms());
                    let moved = link_ms != last_link_ms && last_write.elapsed() >= MIN_LINK_WRITE;
                    if fetched.load(Relaxed) && push.ready(Instant::now()) && (((changed || wanted) && (last_write.elapsed() >= gap || moved)) || last_write.elapsed() >= KEEPALIVE) {
                        let files = signed_files(&secret, &repo2, &me2, n.files.clone(), &n.msg);
                        match publish(&board, &repo2, &me2, &tok, (&files, &n.msg, &idc), GIT_NET_LIMIT, Some(&stop)) {
                            Ok(sha) => {
                                last_files = files;
                                last_data = n.quick.clone();
                                last_link_ms = link_ms;
                                wanted = false;
                                push.worked(Instant::now());
                                // a fetch still failing keeps its message: it would only come back
                                with(&repo2, |s| { if retry.fails == 0 { s.error.clear() } s.my_sha = sha });
                                vouch(&repo2, &tok, &idc, &sign_pub, &my_pub);
                                writes += 1;
                                if let Some(most) = tidy_limit(writes) {
                                    tidy_if_big(&board, most, Some(&stop));
                                }
                            }
                            Err(_) if stop.load(Relaxed) => break,
                            Err(e) => { push.failed(Instant::now()); set_error(&repo2, e) }
                        }
                        last_write = Instant::now();
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }).map_err(|e| { release(&repo); e.to_string() })?
    };
    std::thread::Builder::new().name(format!("qh-team-{repo}")).spawn(move || {
        let mut live = Live { last: None, at: Instant::now(), pulse: 0, told: vec![], bytes: 0, failed: 0, watchers: vec![], asked: None };
        let mut sealed = Sealed::default();
        // my folder: the files for git, and the commit message (presence); plus the data without
        // transcripts, for the direct line
        let mut build = |state_word: &str| -> (Vec<(String, String)>, Value, Option<Value>, Value) {
            let mut quick = None;
            let mut shown = Value::Null;
            let mut files = vec![("pub".to_string(), my_pub.clone())];
            let d = summary(); // outside the lock: it reads the hub
            with(&repo2, |s| {
                expire_locked(s);
                wake_locked(s);
                save_requests(s);
                if let Some(k) = &s.key {
                    // the key goes to members, and to newcomers we approved; nobody else
                    let mut who: Vec<(String, String)> = s.peers.values()
                        .filter(|p| !p.pub_key.is_empty() && (p.joined || s.approved.contains(&p.login)) && !s.removed.contains(&p.login))
                        .map(|p| (p.login.clone(), p.pub_key.clone())).collect();
                    who.sort();
                    if sealed.key != Some(*k) { sealed = Sealed { key: Some(*k), ..Default::default() } }
                    if sealed.who != who {
                        sealed.wraps = who.iter().filter_map(|(l, public)| Some((format!("keys/{l}"), tc::wrap(k, &secret, public, &s.repo)?))).collect();
                        sealed.who = who;
                    }
                    files.extend(sealed.wraps.iter().cloned());
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
                    d["focus"] = json!(s.flow.focus);
                    d["chat"] = Value::from(s.my_chat.clone());
                    d["dms"] = Value::from(s.my_dms.clone());
                    d["groups"] = Value::from(s.my_groups.clone());
                    d["gmsgs"] = Value::from(s.my_gmsgs.clone());
                    d["notes"] = Value::from(s.my_notes.clone());
                    d["team_notes"] = json!(std::fs::read_to_string(team_notes_file(&s.repo)).unwrap_or_default());
                    d["claims"] = json!(s.claims);
                    d["unmerged"] = json!(s.unmerged);
                    d["uses"] = json!(s.uses);
                    d["promises"] = promises_json(&s.promises);
                    d["sheets"] = s.sheets.iter().map(|(k, v)| (k.clone(), json!({"decided": v.decided, "next": v.next, "ts": v.ts})))
                        .collect::<serde_json::Map<_, _>>().into();
                    d["waiting"] = Value::from(s.waiting.iter().map(|w| json!({"session": w.session, "file": w.file, "func": w.func, "on": w.on, "since": w.since})).collect::<Vec<_>>());
                    if let Some(l) = &line {
                        d["link"] = json!(l.addrs());
                        d["link_ms"] = json!(l.addrs_ms());
                    }
                    let plain = d.to_string();
                    if sealed.plain != plain {
                        sealed.data = tc::seal(k, &format!("data:{}", s.me), plain.as_bytes());
                        sealed.plain = plain;
                    }
                    files.push(("data".into(), sealed.data.clone()));
                    let mut q = d;
                    if let Some(o) = q.as_object_mut() { shown = o.remove("transcripts").unwrap_or_default(); }
                    quick = Some(q);
                }
            });
            let msg = json!({"v": 2, "state": state_word, "ts": now(), "ms": now_ms(), "qh": crate::update::VERSION});
            files.push(("sign".into(), sign_pub.clone()));
            files.push(("anchor".into(), idc.clone()));
            (files, msg, quick, shown)
        };
        let mut handed = false; // plain qh gave the team to `qh team`: it takes it back after
        while running.load(Relaxed) {
            // here and not on the board's thread: that one can sit in a fetch for as long as the
            // network does, and whoever asked gives up after 15 s
            if wanted_elsewhere(&repo2) {
                handed = held_as(&repo2) == "solo";
                log_to(&repo2, if handed {
                    "`qh team` took over the team connection; holds and claims here pause until it closes"
                } else { "the qh opened in this project took over the team connection" });
                running.store(false, Relaxed);
                break;
            }
            // what asked for a write before this build is in it
            let want = with(&repo2, |s| std::mem::take(&mut s.dirty)).unwrap_or(false);
            let (files, msg, quick, shown) = build("online");
            if let (Some(l), Some(d)) = (&line, &quick) {
                push_live(&repo2, &me2, l, d, &mut live);
                watch_round(&repo2, &me2, l, &shown, &mut live);
            }
            // the screen only redraws when something happens: drawing is how a session counts as still open
            if with(&repo2, |s| s.watching.as_ref().is_some_and(|w| (WATCH_EVERY..WATCH_LIFE).contains(&w.2.elapsed()))).unwrap_or(false) {
                (hooks.refresh)();
            }
            {
                let mut l = latest.lock().unwrap();
                l.0 = Some(Snapshot { files, msg, quick });
                l.1 |= want;
            }
            // wait half a second, but take in what comes over the line at once, and go round
            // early when a message, answer or note of ours is waiting to go out
            let until = Instant::now() + Duration::from_millis(500);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match live_rx.recv_timeout(left.min(Duration::from_millis(100))) {
                    Ok((from, bytes)) => {
                        take_live(&repo2, &root, &from, &bytes, &hooks, &mut live);
                        bump();
                        (hooks.refresh)();
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => if !running.load(Relaxed) || (line.is_some() && pulse(&repo2) != live.pulse) { break },
                    Err(_) => std::thread::sleep(left),
                }
            }
            // an owner decided a push we handed them: tell the session that pushed
            for (session, text) in crate::review::take_decisions() { (hooks.answered)(&session, &text) }
        }
        if let Some(m) = LIVE.lock().unwrap().as_mut() { m.remove(&repo2); }
        stop.store(true, Relaxed);
        let since = quit_began().unwrap_or_else(Instant::now);
        leave_board(reader, &fetched, || build("offline"), |(files, msg, _, _)| {
            let files = signed_files(&secret, &repo2, &me2, files, &msg);
            let _ = last_write(&board, &format!("https://github.com/{repo2}.git"), &me2, &auth_env(&tok), (&files, &msg, &idc), since);
        });
        release(&repo2);
        finished.store(true, Relaxed);
        if handed {
            // holds and claims work only while connected: once `qh team` closes, connect again.
            // First let it take the lock (it's free for a moment after we let go, seen live)
            let t0 = Instant::now();
            while held_as(&repo2) == "solo" && t0.elapsed() < Duration::from_secs(30) { std::thread::sleep(Duration::from_millis(500)) }
            while !connection_free(&repo2) { std::thread::sleep(Duration::from_secs(20)) }
            RETAKE.lock().unwrap().push(repo2.clone());
            (hooks.refresh)();
        }
    }).map_err(|e| { release(&repo); e.to_string() })?;
    remember(&repo, true);
    Ok(format!("team mode on for {repo} as {me} (shared data is encrypted for the team)"))
}

/// The folder as of the line's last round, for the board's thread to write.
struct Snapshot {
    files: Vec<(String, String)>,
    msg: Value,
    quick: Option<Value>, // the data without transcripts
}

fn signed_files(secret: &x25519_dalek::StaticSecret, repo: &str, me: &str, mut files: Vec<(String, String)>, msg: &Value) -> Vec<(String, String)> {
    let sig = tc::sign_folder(secret, repo, me, &msg.to_string(), &files);
    files.push(("sig".into(), sig));
    files
}

/// When quitting began, if it has. Set once and never cleared: `stop_and_flush` is only for when
/// the process is on its way out, not for disconnecting (that's `stop`).
static QUIT_AT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn quit_began() -> Option<Instant> {
    QUIT_AT.get().copied()
}

/// What the last write ("offline") has left, or None if that's too little to get anywhere. Quitting
/// waits 5 s from `since`: stopping the push must have begun well before that (on Windows it takes
/// starting taskkill), or we exit first and leave the push running with nobody to end it.
fn last_write_limit(since: Instant) -> Option<Duration> {
    let left = Duration::from_millis(4500).saturating_sub(since.elapsed());
    (left >= Duration::from_millis(500)).then_some(left)
}

/// The last write: the commit first, since that takes time too, then a push for what's left.
/// Doesn't look at `stop`: this is the write we stopped everything for.
fn last_write(root: &Path, url: &str, me: &str, env: &[(&str, String)], (files, message, parent): (&[(String, String)], &Value, &str), since: Instant) -> Result<String, String> {
    let commit = folder_commit(root, files, message, parent)?;
    let limit = last_write_limit(since).ok_or("no time left to tell teammates we left")?;
    push_commit(root, url, me, env, commit, limit, None)
}

/// Quitting, once the board's thread was told to stop: give it a moment, then build the last
/// folder and write it, whether that thread has ended or not. It may be inside a GitHub call, up
/// to 15 s, and nothing can stop those. Nothing goes out if we never read the board: the folder's
/// wrapped keys come from what we read, so it would take them away from everyone.
fn leave_board<T>(reader: std::thread::JoinHandle<()>, fetched: &AtomicBool, make: impl FnOnce() -> T, push: impl FnOnce(T)) {
    let t0 = Instant::now();
    while !reader.is_finished() && t0.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let folder = make();
    if fetched.load(Relaxed) {
        push(folder);
    }
}

/// Take in a fetch, and only then count the board as read: `absorb` returns early when stopped,
/// knowing nobody yet, and a folder written from that has no keys/* for anyone.
fn take_in_board(fetched: &AtomicBool, stop: &AtomicBool, absorb: impl FnOnce()) {
    absorb();
    if !stop.load(Relaxed) {
        fetched.store(true, Relaxed);
    }
}

/// What we sealed last, so an unchanged folder costs no crypto (it's built twice a second).
#[derive(Default)]
struct Sealed {
    key: Option<Key>,
    who: Vec<(String, String)>, // login, member key: who the wraps are for
    wraps: Vec<(String, String)>,
    plain: String,
    data: String,
}

/// Whether whoever could watch our transcripts right now gets them over the line.
fn transcripts_by_line(repo: &str) -> bool {
    everyone_on_line(repo) && with(repo, |s| s.peers.values().filter(|p| p.online)
        .all(|p| !p.qh.is_empty() && !crate::update::newer(LIVE_TRANSCRIPTS, &p.qh))).unwrap_or(false)
}

/// Whether everyone reading the board right now gets our data over the line anyway.
fn everyone_on_line(repo: &str) -> bool {
    let lines = LINES.lock().unwrap().clone().unwrap_or_default();
    with(repo, |s| s.peers.values().filter(|p| p.online).all(|p| lines.contains_key(&p.pub_key))).unwrap_or(false)
}

fn set_error(repo: &str, e: String) {
    with(repo, |s| s.error = e);
    bump();
}

const FETCH_FIRST: Duration = Duration::from_secs(6);
const PUSH_FIRST: Duration = Duration::from_secs(10);

/// Up to a quarter more on each wait, from the pid: two teammates stuck together don't retry together.
fn jitter_of(pid: u32) -> u32 {
    pid % 26
}

/// When to try a fetch or a push again. A retry means processes and, for a push, a commit each
/// time, and the listing's etag no longer says when: once it has been seen, it answers 304.
struct FetchRetry {
    need: bool, // there's something to fetch; starts true, because we haven't read the board yet
    fails: u32,
    next: Instant,
    first: Duration, // the first wait; each one after is twice as long, up to 2 minutes
    jitter: u32,     // percent
}

impl FetchRetry {
    fn new(first: Duration, now: Instant) -> Self {
        FetchRetry { need: true, fails: 0, next: now, first, jitter: jitter_of(std::process::id()) }
    }
    fn ready(&self, now: Instant) -> bool {
        now >= self.next
    }
    fn due(&self, now: Instant) -> bool {
        self.need && self.ready(now)
    }
    fn failed(&mut self, now: Instant) {
        self.fails += 1;
        let wait = self.first.saturating_mul(1 << (self.fails - 1).min(16)).min(Duration::from_secs(120));
        self.next = now + wait + wait * self.jitter / 100;
    }
    fn worked(&mut self, now: Instant) {
        self.need = false;
        self.fails = 0;
        self.next = now;
    }
}

/// How long a fetch may take. Before the first one works, longer each try: a first fetch has the
/// whole board to bring, on what may be a slow line. After that the wait is short: this thread
/// also writes our address, which can't wait on a fetch.
fn fetch_limit(fails: u32, fetched: bool) -> Duration {
    if fetched { return GIT_NET_LIMIT }
    GIT_NET_LIMIT.saturating_mul(1 << fails.min(2))
}

/// Our login, and to give up on a connection that has gone slow without waiting out the limit.
/// That likely doesn't cover connecting, so the limit stays too.
fn fetch_env(tok: &str) -> Vec<(&'static str, String)> {
    let mut env = auth_env(tok);
    env.push(("GIT_HTTP_LOW_SPEED_LIMIT", "1024".into()));
    env.push(("GIT_HTTP_LOW_SPEED_TIME", "20".into()));
    env
}

/// What the listing of everyone's folders says. Whether it's worth a fetch, if one is due.
fn take_listing(repo: &str, me: &str, reply: Result<(u16, Option<String>, Value), String>, folders: &HashMap<String, (String, Folder)>,
                etag: &mut Option<String>, retry: &mut FetchRetry) -> bool {
    match reply {
        Ok((200, tag, v)) => {
            let changed = v.as_array().into_iter().flatten().any(|r| {
                let name = r["ref"].as_str().unwrap_or("").trim_start_matches("refs/qh/peers/");
                name != me && folders.get(name).map(|(s, _)| s.as_str()) != r["object"]["sha"].as_str()
            });
            *etag = tag;
            retry.need |= changed;
            true
        }
        Ok((304, _, _)) => true,
        // GitHub lists no folders as 200 and an empty list. This is a repo we can't see: no team
        // key, or we'd keep a wrong one after the access problem is gone
        Ok((404, _, _)) => { set_error(repo, format!("GitHub doesn't show you {repo}: check it exists and that this login has access")); false }
        Ok(_) => false,
        Err(e) => { set_error(repo, e); false }
    }
}

/// Take in freshly fetched folders: our key if someone wrapped it for us (or left it under our
/// invite code), presence, newcomers, chat, transcripts, and messages to our sessions.
#[allow(clippy::too_many_arguments)]
fn absorb(repo: &str, root: &Path, list: &[(String, String)], read: &mut HashMap<String, (String, Folder)>,
          secret: &x25519_dalek::StaticSecret, tok: &str, hooks: &Hooks, stop: &AtomicBool) {
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
        let f = match read.get(login) { Some((s, f)) if s == sha => f.clone(), _ => folder_files(root, sha) };
        read.insert(login.clone(), (sha.clone(), f.clone()));
        let p = read_folder(&f, repo, login, None);
        if vet(repo, tok, &p, hooks, stop) {
            if crate::update::newer(&p.qh, crate::update::VERSION) {
                warn_once(repo, Some(hooks), &format!("newer qh {}", p.qh), format!(
                    "{login} runs qh {}, newer than yours ({}): run {} so you both work the same way",
                    p.qh, crate::update::VERSION, crate::update::how()));
            }
            folders.push((f, p));
        }
        // each of those can ask GitHub (15 s). If quitting went ahead without us, the lock may
        // already be someone else's: what's on disk is theirs to write now
        if stop.load(Relaxed) { return }
    }
    newer_key(repo, &folders, secret, tok, hooks, stop);
    if stop.load(Relaxed) { return }
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
    let _one = APPLY.lock().unwrap();
    let mut new_chat = false;
    for (f, p) in &folders {
        let p = read_folder(f, repo, &p.login, key.as_ref());
        take_in(repo, &me, key, p, hooks, &mut new_chat);
    }
    if new_chat { save_chat(repo) }
    save_seen(repo);
    settle_key(repo, stop);
}

/// Someone removed a member and made a new team key: switch to it, if GitHub says they're a repo
/// admin and vouched for their member key (a folder signed with keys we only trust on first
/// sight isn't enough: anyone with push access could have written it). If we are the one
/// removed, say so.
fn newer_key(repo: &str, folders: &[(Folder, Peer)], secret: &x25519_dalek::StaticSecret, tok: &str, hooks: &Hooks, stop: &AtomicBool) {
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
        let admin = is_admin(repo, login, tok);
        if stop.load(Relaxed) { return } // that took a while: see `vet`
        if !admin {
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
    forget_topics(repo, login); // their trust ended with their access
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
    save_json("team-private", &s.repo, &json!({"msgs": msgs, "groups": groups, "dms": s.my_dms, "my_groups": s.my_groups, "gmsgs": s.my_gmsgs,
        "cleared": s.cleared}));
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
    s.cleared = v["cleared"].as_object().into_iter().flatten().filter_map(|(k, t)| Some((k.clone(), t.as_u64()?))).collect();
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
    // a thread you cleared: what was in it doesn't come back from their folder
    if s.cleared.get(&m.chan).is_some_and(|&t| m.ts <= t) { return false }
    m.text = m.text.chars().filter(|c| !c.is_control() || *c == '\n').collect();
    if m.from != s.me {
        *s.chan_unread.entry(m.chan.clone()).or_default() += 1;
        if pings(&m.text, &s.me) && !s.pinged.contains(&m.chan) { s.pinged.push(m.chan.clone()) }
    }
    let at = s.msgs.iter().rposition(|x| x.ts <= m.ts).map_or(0, |i| i + 1);
    s.msgs.insert(at, m);
    let n = s.msgs.len();
    if n > 1000 { s.msgs.drain(..n - 1000); }
    bump();
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
            crate::review::on_dm(login, &text);
            alert(format!("qh: {login} (private): {}", clip_to(&crate::review::shown(&text), 80)));
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
/// Returns the line as it goes over the direct line (`quick_send`).
fn dm_locked(s: &mut State, to: &str, text: &str) -> Result<Value, String> {
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
    Ok(json!({"chan": "dm", "to": [to], "id": id, "ts": ts, "box": sealed}))
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

fn group_send_locked(s: &mut State, gid: &str, text: &str) -> Result<Value, String> {
    let g = s.groups.get(gid).ok_or("no such group")?;
    if !g.members.contains(&s.me) { return Err("you're not in that group".into()) }
    let sealed = tc::seal(&g.key, &format!("gmsg:{}:{gid}:{}", s.repo, s.me), text.as_bytes());
    let (id, ts) = (msg_id(s), now());
    s.my_gmsgs.push(json!({"id": id, "ts": ts, "g": gid, "box": sealed}));
    let n = s.my_gmsgs.len();
    if n > 100 { s.my_gmsgs.drain(..n - 100); }
    let me = s.me.clone();
    let to: Vec<String> = s.groups.get(gid).map(|g| g.members.iter().filter(|m| **m != me).cloned().collect()).unwrap_or_default();
    push_msg(s, Msg { ts, chan: format!("+{gid}"), from: me, text: text.into(), id });
    s.dirty = true;
    save_private(s);
    Ok(json!({"chan": "g", "to": to, "g": gid, "id": id, "ts": ts, "box": sealed}))
}

/// `/dm <login> <text>`: a private message, readable only by the two of you.
pub fn dm_send(to: &str, text: &str) -> Result<String, String> {
    let to = to.trim_start_matches('@').to_string();
    let (repo, quick, msg) = cur(|s| {
        let quick = dm_locked(s, &to, text)?;
        let online = s.peers.get(&to).is_some_and(|p| p.online);
        Ok::<_, String>((s.repo.clone(), quick, format!("→ @{to} (private){}", if online { "" } else { ": they're offline; it reaches them when they're back" })))
    }).ok_or("team mode is off".to_string())??;
    quick_send(&repo, &quick);
    Ok(msg)
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
        s.msgs.iter().filter(|m| m.chan == chan).map(|m| (ago(m.ts), who(&m.from), plain(&crate::review::shown(&m.text)), m.from != s.me && pings(&m.text, &s.me))).collect()
    }).unwrap_or_default()
}

/// Who's in a channel, for its header: the group's members, or the DM's two people.
pub fn channel_members(chan: &str) -> Vec<String> {
    cur(|s| match chan.strip_prefix('+') {
        Some(gid) => s.groups.get(gid).map(|g| g.members.clone()).unwrap_or_default(),
        None => chan.strip_prefix('@').map(|l| vec![s.me.clone(), l.to_string()]).unwrap_or_default(),
    }).unwrap_or_default()
}

/// `x` in a private thread: hide everything in it so far, for you only (they keep their copy).
pub fn channel_clear(chan: &str) {
    if chan == "#team" { return }
    cur(|s| {
        let t = s.msgs.iter().filter(|m| m.chan == chan).map(|m| m.ts).max().unwrap_or(0).max(now());
        s.cleared.insert(chan.to_string(), t);
        s.msgs.retain(|m| m.chan != chan);
        read_locked(s, chan);
        save_private(s);
        bump();
    });
}

/// A teammate's push handed to you to decide (a `#qh-hold` private message).
#[derive(Clone, Debug)]
pub struct Hold {
    pub from: String,
    pub head: String,
    pub text: String,          // the message without its tag: reason and diff
    pub ago: String,
    pub decided: Option<bool>, // you approved (true) or sent it back (false)
}

/// Pushes handed to you, newest first; a decided one stays (with what you said) for a while.
pub fn holds() -> Vec<Hold> {
    cur(|s| {
        let mut out: Vec<Hold> = vec![];
        for m in s.msgs.iter().filter(|m| m.from != s.me && m.chan.starts_with('@')) {
            let Some(crate::review::Tag::Hold { head, .. }) = crate::review::tag(&m.text) else { continue };
            let decided = s.msgs.iter().filter(|x| x.from == s.me && x.chan == m.chan && x.ts >= m.ts).find_map(|x| match crate::review::tag(&x.text) {
                Some(crate::review::Tag::Decide { head: h, approve, .. }) if h == head => Some(approve),
                _ => None,
            });
            out.retain(|h| h.head != head || h.from != m.from);
            out.push(Hold { from: m.from.clone(), head, text: crate::review::shown(&m.text), ago: ago(m.ts), decided });
        }
        out.reverse();
        out
    }).unwrap_or_default()
}

/// Approve a held push of `from`'s, or send it back with a note: it goes to them privately, and
/// their qh lets the push through or hands the note to their agent.
pub fn decide_hold(from: &str, head: &str, approve: bool, note: &str) -> Result<String, String> {
    if !holds().iter().any(|h| h.from == from && h.head == head) { return Err("no such held push".into()) }
    dm_send(from, &crate::review::decision_text(head, approve, note))?;
    Ok(if approve { format!("approved {from}'s push") } else { format!("sent {from}'s push back") })
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
    let (repo, quick) = cur(|s| group_send_locked(s, &gid, text).map(|q| (s.repo.clone(), q))).ok_or("team mode is off".to_string())??;
    quick_send(&repo, &quick);
    Ok(String::new())
}

static ALERTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static FOCUS: AtomicU64 = AtomicU64::new(0); // the current team's focus end, readable under the state lock

/// Something worth a desktop notification (the UI decides whether to show it: focus, `notify`).
fn alert(text: String) {
    if focusing().is_some() { return } // held: the count still shows, and focus ending says what waited
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
    // transcripts that came over the line after this copy was made stay
    if let Some(old) = s.peers.get(login) {
        p.tr_seq = old.tr_seq.clone();
        for (session, ms) in &old.tr_seq {
            if let Some(rows) = old.transcripts.get(session).filter(|_| *ms > p.seq) {
                p.transcripts.insert(session.clone(), rows.clone());
            }
        }
    }
    // a git copy older than what came over the link: keep the live data, take only what git alone carries
    if let Some(old) = s.peers.get(login).filter(|o| o.seq > p.seq) {
        let mut m = old.clone();
        m.ts = m.ts.max(p.ts);
        m.online = m.online || p.online;
        m.pub_key = std::mem::take(&mut p.pub_key);
        m.wrapped = std::mem::take(&mut p.wrapped);
        m.invites = std::mem::take(&mut p.invites);
        m.qh = std::mem::take(&mut p.qh);
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
        // already here over the direct line (the same id), or the same line from an older copy
        let quick = !s.msg_ids.insert((format!("chat:{login}"), *id));
        if !quick && !s.chat.iter().any(|(t, f, x)| t == ts && f == login && x == text) {
            insert_chat(s, *ts, login, text);
            if seen_before {
                s.unread += 1;
                *new_chat = true;
                if pings(text, me) {
                    if !s.pinged.iter().any(|c| c == "#team") { s.pinged.push("#team".into()) }
                    alert(format!("qh: {login} mentioned you: {}", clip_to(text, 80)));
                    push_fyi(s, "mention", format!("{login} mentioned you: {}", clip_to(text, 300)));
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
        // a resend of one we still hold: same card, under its new id (their news about it uses that)
        let was: Vec<u64> = m["was"].as_array().into_iter().flatten().filter_map(Value::as_u64).collect();
        if let Some(x) = s.pending.iter_mut().find(|x| x.from == *login && was.contains(&x.id)) {
            x.id = id;
            s.dirty = true;
            continue;
        }
        let mut inc = Incoming { from: login.clone(), id, session: m["session"].as_str().unwrap_or("main").into(),
                                 text: m["text"].as_str().unwrap_or("").into(), question: None, ts: m["ts"].as_u64().unwrap_or_else(now),
                                 from_session: m["from_session"].as_str().unwrap_or("").into(), replied: false,
                                 packet: if m["packet"].is_object() { m["packet"].clone() } else { Value::Null }, ..Default::default() };
        let role = s.roles.get(login).map(String::as_str).unwrap_or("member");
        if m["kind"] == "s" {
            // where one of our requests is now
            s.seen.insert(login.clone(), id);
            s.dirty = true;
            let (re, text) = (m["re"].as_u64().unwrap_or(0), inc.text.clone());
            let Some(st) = m["status"].as_str().and_then(Status::parse) else { continue };
            let Some(o) = s.sent.iter_mut().find(|o| o.to == *login && (o.id == re || o.was.contains(&re))) else { continue };
            let id = o.id; // the id it goes under now, after any resend
            if o.history.iter().any(|h| h.0 == st) && st != Status::Replied {
                if st == Status::Seen && !text.is_empty() { o.reply = text.clone() } // "seen, back to it 14:00"
                continue;
            }
            o.history.push((st, inc.ts));
            if !text.is_empty() { o.reply = text.clone() }
            else if o.reply.starts_with("seen, ") { o.reply.clear() } // that news is over
            let what = clip_to(bare(&o.text), 80);
            let line = match st {
                Status::Replied => Some(format!("{login} replied to \"{what}\": {text}")),
                Status::Accepted => Some(format!("{login} accepted \"{what}\"")),
                Status::Declined => Some(format!("{login} declined \"{what}\"{}", if text.is_empty() { String::new() } else { format!(": {text}") })),
                Status::Expired => Some(format!("\"{what}\" expired: {login} didn't act on it in two weeks")),
                _ => None,
            };
            if let Some(line) = line {
                alert(format!("qh: {}", clip_to(&line, 100)));
                push_fyi(s, "reply", line.clone());
                say_locked(repo, s, hooks, line);
            }
            (hooks.changed)(TeamEvent::Outgoing { id, status: st });
            continue;
        }
        if m["kind"] == "w" {
            // they took a request back
            s.seen.insert(login.clone(), id);
            s.dirty = true;
            let re = m["re"].as_u64().unwrap_or(0);
            if let Some(i) = s.pending.iter().position(|x| x.from == *login && x.id == re) {
                let gone = s.pending.remove(i);
                say_locked(repo, s, hooks, format!("{login} withdrew their request: \"{}\"", clip_to(bare(&gone.text), 80)));
                (hooks.changed)(TeamEvent::Gone { from: login.clone(), id: re });
            }
            continue;
        }
        if m["kind"] == "a" {
            // an answer to one of our questions
            s.seen.insert(login.clone(), id);
            s.dirty = true;
            let re = m["re"].as_u64().unwrap_or(0);
            let Some(i) = s.asked.iter().position(|a| a.id == re) else { continue };
            let q = s.asked.remove(i);
            let from = format!("{login}/{}", m["from_session"].as_str().unwrap_or(""));
            if let Some(late) = crate::review::answered(re, &from, &inc.text) {
                // a review of what we pushed (or are about to): the team room and the pusher see it
                let head = clip_to(&inc.text, 1500);
                alert(format!("qh: review from {from}: {}", clip_to(&head, 80)));
                push_fyi(s, "review", format!("review from {from}: {}", clip_to(&head, 300)));
                say_locked(repo, s, hooks, format!("review from {from} ({}): {head}", crate::review::verdict(&inc.text)));
                if late && !q.session.is_empty() {
                    (hooks.answered)(&q.session, &format!("[team review from {from}] (it came after the push)\n\n{}", inc.text));
                }
                continue;
            }
            // what the answer relied on is now something our code counts on: a contract
            let relied: Vec<String> = m["relied"].as_array().into_iter().flatten().filter_map(|n| n.as_str().map(String::from)).collect();
            let u = s.uses.entry(q.session.clone()).or_default();
            for n in &relied { u.insert(n.clone(), now()); }
            if !relied.is_empty() {
                s.last_answer.insert(q.session.clone(), (from.clone(), relied.clone(), format!("Q: {} A: {}", q.q, inc.text)));
            }
            // the whole answer for you to read (the agent gets it in full either way); a very long one is cut with …
            let head = clip_to(&inc.text, 1500);
            alert(format!("qh: {from} answered: {}", clip_to(&head, 80)));
            push_fyi(s, "answer", format!("{from} answered: {}", clip_to(&head, 300)));
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
        if m["kind"] == "c" {
            // they took back what their agent told ours: its contracts end, and the agent is told
            s.seen.insert(login.clone(), id);
            s.dirty = true;
            let (q, a) = (m["q"].as_str().unwrap_or(""), m["a"].as_str().unwrap_or(""));
            if let Some(u) = s.uses.get_mut(&inc.session) {
                for n in m["relied"].as_array().into_iter().flatten().filter_map(Value::as_str) { u.remove(n); }
            }
            if s.last_answer.get(&inc.session).is_some_and(|l| l.2 == format!("Q: {q} A: {a}")) { s.last_answer.remove(&inc.session); }
            let line = format!("{login} took back their agent's answer to \"{}\"", clip_to(q, 80));
            alert(format!("qh: {}", clip_to(&line, 100)));
            push_fyi(s, "answer", line.clone());
            say_locked(repo, s, hooks, line);
            if !inc.session.is_empty() {
                (hooks.answered)(&inc.session, &format!("[team correction from {login}] {login} took back the answer you were given to: {q}\n\nThat answer was: {a}\n\n\
Don't rely on it any more, and if you saved it anywhere, remove it. If you already acted on it, tell the user what depends on it. Don't ask their agent \
the same thing again: it could only repeat itself. If this matters, tell the user to ask {login} in person."));
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
                inc.kind = "question".into();
                inc.history = vec![(Status::Delivered, now)];
                alert(format!("qh: {who} asks your {}: {}", inc.session, clip_to(&inc.text, 80)));
                (hooks.ask)(&inc);
                (hooks.changed)(TeamEvent::Arrived { from: login.clone(), id });
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
            inc.kind = "handover".into();
            inc.history = vec![(Status::Delivered, now())];
            status_out(s, login, id, Status::Delivered, "");
            (hooks.ask)(&inc);
            (hooks.changed)(TeamEvent::Arrived { from: login.clone(), id });
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
fn settle_key(repo: &str, stop: &AtomicBool) {
    if stop.load(Relaxed) { return } // see `vet`
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
    recover_key(repo, stop);
}

/// Every folder we can't read was last written long ago: whoever held the team key lost it with
/// their machine, so nobody is left to let anyone in. A repo admin starts the next key
/// generation, as `/team remove` does, and the others switch to it when they come back.
fn recover_key(repo: &str, stop: &AtomicBool) {
    let Some((me, epoch)) = with(repo, |s| {
        let t = now();
        let mut holders = s.peers.values().filter(|p| !p.wrapped.is_empty() || p.locked).peekable();
        let orphaned = s.key.is_none() && holders.peek().is_some()
            && holders.all(|p| !p.online && t.saturating_sub(p.ts) >= ORPHAN_AFTER);
        orphaned.then(|| (s.me.clone(), s.peers.values().map(|p| p.epoch).max().unwrap_or(0)))
    }).flatten() else { return };
    let admin = is_admin(repo, &me, &token().unwrap_or_default());
    if !admin || stop.load(Relaxed) { // that took a while: see `vet`. A new key now could undo what the next holder published
        return;
    }
    with(repo, |s| {
        let left: Vec<String> = s.peers.values().map(|p| p.login.clone()).collect();
        for l in left {
            if !s.approved.contains(&l) { s.approved.push(l) }
        }
        let k = tc::new_team_key();
        save_key(&s.repo, &k);
        s.key = Some(k);
        s.epoch = s.epoch.max(epoch) + 1;
        s.dirty = true;
    });
    persist_membership(repo);
    log_to(repo, "nobody who holds the team key has been seen in a long time: you're an admin, so you started a new one");
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
    send_packet(to, session, text, from_session, Value::Null)
}

/// `send_from` with a handover packet the sending agent wrote: what's done, what's left, the
/// files, what to watch out for, the branch. The receiver sees it when they accept.
pub fn send_packet(to: &str, session: &str, text: &str, from_session: &str, packet: Value) -> Result<String, String> {
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
        let mut m = json!({"id": id, "to": to, "session": session, "text": text, "from_session": from_session, "ts": now()});
        let packet = if packet.is_object() { m["packet"] = packet.clone(); packet } else { Value::Null };
        s.outbox.push(m);
        s.sent.push(Outgoing { id, to: to.into(), session: session.into(), text: text.into(), kind: "handover".into(),
                               from_session: from_session.into(), ts: now(), history: vec![(Status::Queued, now())],
                               reply: String::new(), packet, was: vec![] });
        let n = s.sent.len();
        if n > 100 { s.sent.drain(..n - 100); }
        s.dirty = true;
        Ok((format!("you asked {to}'s {session}: {text}"),
            format!("asked {to}'s {session}: it waits in their Requests and runs on their machine only once they accept it{}",
                    if online { "" } else { " (they're offline, so it waits for them)" })))
    }).ok_or("team mode is off".to_string())?.map(|(log_line, msg)| { log(&log_line); msg })
}

/// Say something in the team chat (to people, not agents: nothing runs).
pub fn chat_send(text: &str) -> Result<(), String> {
    let (repo, quick) = cur(|s| {
        let quick = chat_locked(s, text);
        (s.repo.clone(), quick)
    }).ok_or("team mode is off")?;
    quick_send(&repo, &quick);
    with(&repo, |s| {
        let keep: Vec<_> = s.chat.iter().rev().take(300).rev().cloned().collect();
        save_json("team-chat", &repo, &json!(keep));
    });
    Ok(())
}

/// Add our line to the team chat and publish it; returns it as it goes over the direct line.
/// Ids only grow (also within one second), so a teammate can tell every line apart.
fn chat_locked(s: &mut State, text: &str) -> Value {
    let ts = now();
    let last = s.my_chat.iter().filter_map(|m| m["id"].as_u64()).max().unwrap_or(0);
    let id = (ts * 1000).max(last + 1);
    s.my_chat.push(json!({"id": id, "ts": ts, "text": text}));
    let n = s.my_chat.len();
    if n > 30 { s.my_chat.drain(..n - 30); }
    let me = s.me.clone();
    insert_chat(s, ts, &me, text);
    s.dirty = true;
    json!({"chan": "#team", "id": id, "ts": ts, "text": text})
}

/// A team chat line in time order (a late line from the board slots in where it belongs).
fn insert_chat(s: &mut State, ts: u64, from: &str, text: &str) {
    let at = s.chat.iter().rposition(|(t, _, _)| *t <= ts).map_or(0, |i| i + 1);
    s.chat.insert(at, (ts, from.to_string(), text.to_string()));
    if s.chat.len() > CHAT_MAX { s.chat.remove(0); }
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
pub fn bare(text: &str) -> &str {
    text.strip_prefix('[').and_then(|t| t.split_once("] ")).map_or(text, |x| x.1)
}

/// What an accepted request says in our session: the request, then its handover packet if it has one.
fn accepted_text(inc: &Incoming) -> String {
    let brief = packet_brief(inc);
    let head = format!("[accepted request from {}]: {}", inc.from, inc.text);
    if brief.is_empty() { head } else { format!("{head}\n\n[handover packet]\n{brief}") }
}

/// A handover's packet in words for the agent, beginning with where to start: "" if it has none.
pub fn packet_brief(i: &Incoming) -> String {
    let p = &i.packet;
    let list = |k: &str| p[k].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| format!("- {x}")).collect::<Vec<_>>().join("\n");
    let mut t = String::new();
    if p.is_object() {
        if let Some(b) = p["branch"].as_str().filter(|b| !b.is_empty()) { t += &format!("\nTheir commits are on branch {b} (switch to it first if you're not on it; ask me if the working tree isn't clean).\n") }
        for (head, k) in [("Done", "done"), ("Left", "left")] {
            let l = list(k);
            if !l.is_empty() { t += &format!("\n{head}:\n{l}\n") }
        }
        if let Some(x) = p["watch"].as_str().filter(|x| !x.is_empty()) { t += &format!("\nWatch out: {x}\n") }
        let files = packet_files(i);
        if !files.is_empty() { t += &format!("\nFiles: {}\n", files.join(", ")) }
        if let Some(first) = p["left"].as_array().and_then(|a| a.first()).and_then(|x| x.as_str()) { t += &format!("\nStart with: {first}") }
    }
    for (who, both) in clashes(i) { t += &format!("\nCareful: {who} also handed me {}; check their changes there first.", both.join(", ")) }
    t.trim_start().to_string()
}

/// What the requester reads when the work we accepted is over: the agent's last words, or that it stopped.
fn done_text(text: &str, fin: &str) -> String {
    let what = clip_to(bare(text), 80);
    if fin == "interrupted" || fin.starts_with("error") { format!("stopped your request \"{what}\" before it finished") }
    else { format!("finished your request \"{what}\": {}", clip_to(fin.trim(), 400)) }
}

/// A DM sent now over the direct line; a failure only goes in the team log.
fn dm_now(repo: &str, to: &str, text: &str) {
    match with(repo, |s| dm_locked(s, to, text)) {
        Some(Ok(q)) => quick_send(repo, &q),
        Some(Err(e)) => log_to(repo, &format!("couldn't tell {to}: {e}")),
        None => {}
    }
}

/// A session's turn on an accepted request ended: the requester hears how it went.
pub fn request_done(cwd: &Path, request: &str, fin: &str) {
    let Some((from, text)) = request.strip_prefix("[accepted request from ").and_then(|r| r.split_once("]: ")) else { return };
    let Some((_, repo)) = repo_of(cwd) else { return };
    if !running_for(&repo) { return }
    dm_now(&repo, from, &done_text(text.split("\n\n[").next().unwrap_or(text), fin));
}

/// Take a waiting request off the list and mark it handled; the requester learns which way it
/// went. "once"/"always" accept, "deny" declines (`why` goes with it), "expired" times out.
/// A declined question gets the usual "not answered".
fn decide_locked(s: &mut State, from: &str, id: u64, answer: &str) -> Option<Incoming> {
    decide_why_locked(s, from, id, answer, "")
}

fn decide_why_locked(s: &mut State, from: &str, id: u64, answer: &str, why: &str) -> Option<Incoming> {
    let i = s.pending.iter().position(|x| x.from == from && x.id == id)?;
    let inc = s.pending.remove(i);
    let prev = s.seen.get(from).copied().unwrap_or(0);
    s.seen.insert(from.to_string(), id.max(prev));
    s.dirty = true;
    match (&inc.question, answer) {
        (Some(q), "deny" | "expired") => {
            let q = q.clone();
            reply_locked(s, &q, &format!("(not answered: {} said no. Use their fact sheet, or ask them directly.)", s.me), &[]);
        }
        (Some(_), _) => {}
        (None, "deny") => status_about(s, from, id, Status::Declined, why, &inc.text),
        (None, "expired") => status_out(s, from, id, Status::Expired, ""),
        (None, _) => {
            status_out(s, from, id, Status::Accepted, "");
            let mut t = inc.clone();
            t.history.push((Status::Accepted, now()));
            s.flow.taken.push(t);
            let n = s.flow.taken.len();
            if n > 30 { s.flow.taken.drain(..n - 30); }
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
        if let Some((text, relied)) = inc.draft {
            // held because it promised something: you looked and said yes, so it goes as written
            with(&repo, |s| { reply_locked(s, &q, &text, &relied); log_answer(s, &q, &text, &relied, true) });
            return Some((String::new(), format!("sent your agent's answer to {}", inc.from)));
        }
        with(&repo, |s| s.flow.approved.push((q.from.clone(), q.id)));
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
    s.pending[i].replied = true;
    s.pending[i].history.push((Status::Replied, now()));
    status_out(s, from, id, Status::Replied, text);
    Ok(format!("→ {from} (on the request); it still waits: a accept · d decline"))
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

/// Our agent's answer to a teammate's question is ready. Facts go out alone to anyone; an answer
/// that commits us to something (`promise`) goes alone only to someone trusted on that topic, else
/// it's held in Asked you with the answer ready. Whatever goes out without you is logged.
pub fn reply(q: &Question, text: &str, relied: &[String], promise: bool) {
    let what = if relied.is_empty() { "something".to_string() } else { relied.join(", ") };
    let who = if q.from_session.is_empty() { q.from.clone() } else { format!("{}/{}", q.from, q.from_session) };
    let held = with(&q.repo, |s| {
        let pos = s.flow.approved.iter().position(|a| *a == (q.from.clone(), q.id));
        if let Some(i) = pos { s.flow.approved.remove(i); }
        let alone = pos.is_none() && !text.starts_with("(not answered");
        if alone && promise && !trusted_for(s, &q.from, &q.text) {
            let mut inc = Incoming { from: q.from.clone(), id: q.id, session: q.session.clone(), text: format!("question: {}", q.text),
                                     question: Some(q.clone()), ts: now(), from_session: q.from_session.clone(), kind: "question".into(),
                                     history: vec![(Status::Delivered, now())], draft: Some((text.to_string(), relied.to_vec())), ..Default::default() };
            inc.packet = json!({"held": format!("held for you: it promises {what}, and {} isn't trusted on this topic", q.from)});
            s.pending.push(inc);
            s.dirty = true;
            return true;
        }
        reply_locked(s, q, text, relied);
        if alone { log_answer(s, q, text, relied, promise) }
        false
    }).unwrap_or(false);
    if held {
        alert(format!("qh: {who}'s question waits for you: the answer promises something"));
        log_to(&q.repo, &format!("held your {}'s answer to {who}: it promises {what}", q.session));
    } else {
        log_to(&q.repo, &format!("your {} → {who}: {}", q.session, text.chars().take(300).collect::<String>()));
    }
}

fn log_answer(s: &mut State, q: &Question, text: &str, relied: &[String], promise: bool) {
    s.flow.log.push(Answered { ts: now(), to: q.from.clone(), question: q.text.clone(), answer: text.to_string(), promise, taken_back: false,
                               session: q.from_session.clone(), relied: relied.to_vec() });
    let n = s.flow.log.len();
    if n > 100 { s.flow.log.drain(..n - 100); }
    push_fyi(s, "answered", format!("your agent answered {}: {}", q.from, clip_to(&q.text, 80)));
    s.dirty = true;
}

/// May our agent promise `login` something about this question without us? Trusted, and the
/// question names one of their topics (no topics picked: any).
fn trusted_for(s: &State, login: &str, question: &str) -> bool {
    if s.roles.get(login).map(String::as_str) != Some("trusted") { return false }
    let topics = s.flow.topics.get(login).map(Vec::as_slice).unwrap_or(&[]);
    let q = question.to_lowercase();
    topics.is_empty() || topics.iter().any(|t| q.contains(&t.to_lowercase()))
}

/// Local clock time of `ts`: "14:00", with the day when it isn't today ("tomorrow 09:00").
pub fn clock(ts: u64) -> String {
    let off = crate::platform::utc_offset();
    let local = |t: u64| t as i64 + off;
    let (day, today) = (local(ts).div_euclid(86400), local(now()).div_euclid(86400));
    let m = local(ts).rem_euclid(86400) / 60;
    let hm = format!("{:02}:{:02}", m / 60, m % 60);
    match day - today {
        0 => hm,
        1 => format!("tomorrow {hm}"),
        -1 => format!("yesterday {hm}"),
        d if d < 0 => format!("{}d ago {hm}", -d),
        d => format!("in {d}d {hm}"),
    }
}

/// The next local `hh:mm` from now (today if it's still ahead, else tomorrow).
fn next_at(hour: i64, min: i64) -> u64 {
    let off = crate::platform::utc_offset();
    let t = now() as i64 + off;
    let mut at = t.div_euclid(86400) * 86400 + hour * 3600 + min * 60;
    if at <= t { at += 86400 }
    (at - off).max(0) as u64
}

/// Later's choices: (words, when the card comes back; 0 = when you pick it from Asked you).
pub fn later_choices() -> Vec<(String, u64)> {
    let mut v = vec![("in 1 hour".to_string(), now() + 3600)];
    let lunch = next_at(14, 0);
    if lunch - now() < 12 * 3600 && lunch > now() + 3600 { v.push((format!("after lunch {}", clock(lunch)), lunch)) }
    let morning = next_at(9, 0);
    v.push((format!("tomorrow morning {}", clock(morning).trim_start_matches("tomorrow ")), morning + if morning - now() < 6 * 3600 { 86400 } else { 0 }));
    v.push(("when I'm done here".to_string(), 0));
    v
}

/// Focusing: cards and notifications hold till then. None when not focusing.
pub fn focusing() -> Option<u64> {
    let f = FOCUS.load(Relaxed);
    (f > now()).then_some(f)
}

/// Start focusing till `until` (0 stops). Teammates see it on your folder.
pub fn set_focus(until: u64) -> Result<String, String> {
    cur(|s| { s.flow.focus = until; s.flow.focus_told = until == 0; s.dirty = true; }).ok_or("team mode is off")?;
    FOCUS.store(until, Relaxed);
    bump();
    Ok(if until == 0 { "focus off: cards and notifications are back".into() }
       else { format!("focusing till {}: cards and notifications hold, teammates see it; your agent still answers facts", clock(until)) })
}

/// The default focus end: an hour from now, up to the next half hour.
pub fn focus_default() -> u64 {
    let t = now() + 3600;
    t.div_ceil(1800) * 1800
}

/// Till when a teammate is focusing (0: they aren't).
pub fn focus_of(login: &str) -> u64 {
    cur(|s| s.peers.get(login).map(|p| p.focus).unwrap_or(0)).unwrap_or(0)
}

/// Handovers you accepted, newest first.
pub fn taken() -> Vec<Incoming> {
    cur(|s| s.flow.taken.iter().rev().cloned().collect()).unwrap_or_default()
}

/// Drop a handover from Taken over (it's finished).
pub fn taken_done(from: &str, id: u64) {
    cur(|s| { s.flow.taken.retain(|t| !(t.from == from && t.id == id)); s.dirty = true; });
}

/// Files a handover touches: its packet's list.
pub fn packet_files(i: &Incoming) -> Vec<String> {
    i.packet["files"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect()
}

/// Other handovers, waiting or taken, that touch a file this one does: (from, shared files).
pub fn clashes(i: &Incoming) -> Vec<(String, Vec<String>)> {
    let mine = packet_files(i);
    if mine.is_empty() { return vec![] }
    cur(|s| clashes_in(s, i, &mine)).unwrap_or_default()
}

/// One line per person, however many handovers of theirs touch the same files.
fn clashes_in(s: &State, i: &Incoming, mine: &[String]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = vec![];
    for x in s.pending.iter().filter(|x| x.question.is_none()).chain(s.flow.taken.iter()).filter(|x| !(x.from == i.from && x.id == i.id)) {
        for f in packet_files(x).into_iter().filter(|f| mine.contains(f)) {
            match out.iter_mut().find(|o| o.0 == x.from) {
                Some(o) => if !o.1.contains(&f) { o.1.push(f) },
                None => out.push((x.from.clone(), vec![f])),
            }
        }
    }
    out
}

/// What our agent told teammates without us, newest first.
pub fn answer_log() -> Vec<Answered> {
    cur(|s| s.flow.log.iter().rev().cloned().collect()).unwrap_or_default()
}

/// Take back an answer from the log (newest-first index): the teammate gets a correction.
pub fn take_back(n: usize) -> Result<String, String> {
    let a = cur(|s| {
        let i = s.flow.log.len().checked_sub(n + 1)?;
        let a = s.flow.log.get_mut(i)?;
        if a.taken_back { return None }
        a.taken_back = true;
        s.dirty = true;
        Some(a.clone())
    }).flatten().ok_or("already taken back, or gone")?;
    // 0.2.7 and later: to the agent that asked, which is the one building on it
    if cur(|s| correct_out(s, &a)) == Some(true) {
        return Ok(format!("took it back: {} and their agent get a correction", a.to));
    }
    channel_send(&format!("@{}", a.to), &format!("Correction: please don't rely on my agent's answer to \"{}\" (\"{}\"). Ask me again.",
                                                  clip_to(&a.question, 120), clip_to(&a.answer, 120)))?;
    Ok(format!("took it back: {} gets a correction", a.to))
}

/// Put a correction in our outbox, if they run a qh that understands it (an older one would show
/// an unknown kind as a request for work).
fn correct_out(s: &mut State, a: &Answered) -> bool {
    if !s.peers.get(&a.to).is_some_and(|p| !p.qh.is_empty() && !crate::update::newer("0.2.7", &p.qh)) { return false }
    s.next_id += 1;
    s.outbox.push(json!({"id": s.next_id, "to": a.to, "session": a.session, "kind": "c", "q": a.question, "a": a.answer, "relied": a.relied, "ts": now()}));
    s.dirty = true;
    true
}

/// The topics a trusted teammate's promises may cover (empty: any).
pub fn topics(login: &str) -> Vec<String> {
    cur(|s| s.flow.topics.get(login).cloned().unwrap_or_default()).unwrap_or_default()
}

/// Add (or with `remove`, drop) a topic for a teammate.
pub fn set_topic(login: &str, topic: &str, remove: bool) -> Result<String, String> {
    let t = topic.trim().to_lowercase();
    if t.is_empty() { return Err("no topic given".into()) }
    let did = cur(|s| {
        if remove {
            let (had, empty) = match s.flow.topics.get_mut(login) {
                Some(v) => {
                    let had = v.contains(&t);
                    v.retain(|x| *x != t);
                    (had, v.is_empty())
                }
                None => (false, false),
            };
            if !had { return false }
            if empty { s.flow.topics.remove(login); } // an empty list is no list
        } else {
            let v = s.flow.topics.entry(login.to_string()).or_default();
            v.retain(|x| *x != t);
            v.push(t.clone());
        }
        s.dirty = true;
        true
    });
    match did {
        None => Err("team mode is off".into()),
        Some(false) => Err(format!("{login} has no topic {t}")),
        Some(true) => Ok(if remove { format!("{login}: dropped topic {t}") } else { format!("{login}: your agent may promise them things about {t}") }),
    }
}

/// Our role for a teammate: viewer, member or trusted.
pub fn role_of(login: &str) -> String {
    cur(|s| s.roles.get(login).cloned().unwrap_or_else(|| "member".into())).unwrap_or_default()
}

/// Forget an expired or finished request of ours ("let it go").
pub fn let_go(id: u64) {
    cur(|s| { s.sent.retain(|o| o.id != id); s.dirty = true; });
}

/// Send an expired request of ours again, as a new one.
pub fn send_again(id: u64) -> Result<String, String> {
    let o = cur(|s| s.sent.iter().find(|o| o.id == id).cloned()).flatten().ok_or("no such request")?;
    let r = send_packet(&o.to, &o.session, &o.text, &o.from_session, o.packet.clone())?;
    let_go(id);
    Ok(r)
}

/// Days until a request of ours expires, from day 12 of 14 while it's still open (None before that).
pub fn expires_in(o: &Outgoing) -> Option<u64> {
    if o.status().done() || o.status() == Status::Replied { return None }
    let age = now().saturating_sub(o.ts);
    (age >= REQUEST_LIFE - 2 * 86400).then(|| (REQUEST_LIFE.saturating_sub(age)).div_ceil(86400))
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

/// Our review request `id` to `to` ("login/session") reached them, and they're still online.
pub fn reviewing(id: u64, to: &str) -> bool {
    let login = to.split('/').next().unwrap_or("");
    cur(|s| s.peers.get(login).is_some_and(|p| p.online && p.acks.get(&s.me).copied().unwrap_or(0) >= id)).unwrap_or(false)
}

/// Ask `owner`'s agent to review a diff (review.rs builds the text): a question like ask_team's,
/// to the session of theirs busiest with the code it names. Returns (id, "login/session", online).
pub fn ask_review(repo: &str, session: &str, owner: &str, text: &str) -> Result<(u64, String, bool), String> {
    with(repo, |s| {
        if s.key.is_none() { return Err("you're not in the team yet".to_string()) }
        let p = s.peers.get(owner).filter(|p| p.joined).ok_or_else(|| format!("{owner} isn't in the team"))?;
        if p.roles.get(&s.me).is_some_and(|r| r == "viewer") { return Err(format!("{owner} has you as view only")) }
        let online = p.online;
        let mut sessions: Vec<(usize, String)> = p.sessions.iter().map(|(n, _, _)| (p.claims.get(n).map_or(0, |c| c.len()), n.clone())).collect();
        sessions.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
        let sess = sessions.into_iter().next().map_or_else(|| "main".to_string(), |(_, n)| n);
        s.next_id += 1;
        let id = s.next_id;
        s.outbox.push(json!({"id": id, "to": owner, "session": sess, "kind": "q", "from_session": session, "text": text, "ts": now()}));
        s.asked.push(Asked { id, session: session.into(), to: format!("{owner}/{sess}"), q: format!("review: {}", clip_to(text, 200)), ts: now() });
        if s.asked.len() > 30 { s.asked.remove(0); }
        s.dirty = true;
        log_to(repo, &format!("your {session} asked {owner}/{sess} to review the changes in their area{}", if online { "" } else { " (offline: it waits on the board)" }));
        Ok((id, format!("{owner}/{sess}"), online))
    }).ok_or("team mode is off for this project".to_string())?
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

/// Requests waiting for us, oldest first (snoozed ones included: they still count).
pub fn pending() -> Vec<Incoming> {
    let mut v = cur(|s| s.pending.clone()).unwrap_or_default();
    v.sort_by_key(|i| i.arrived());
    v
}

/// (waiting, when the oldest arrived): cheap enough for every frame.
pub fn pending_summary() -> (usize, Option<u64>) {
    cur(|s| (s.pending.len(), s.pending.iter().map(|i| i.arrived()).min())).unwrap_or((0, None))
}

/// What came in since qh last had this project open.
pub fn away() -> Away {
    cur(|s| away_of(s)).unwrap_or_default()
}

/// What came between `last_open` and the end of the first read of the board. Not "before this run
/// started": what was sent while we were away is stamped when it arrives, during that read. What
/// comes after it, we were here for.
fn away_of(s: &State) -> Away {
    let since = s.last_open;
    let within = |t: u64| since > 0 && t > since && (s.away_until == 0 || t <= s.away_until);
    let mut needs_you: Vec<Incoming> = s.pending.iter().filter(|i| within(i.arrived())).cloned().collect();
    needs_you.sort_by_key(|i| i.arrived());
    Away { since_ts: since, needs_you, fyi: s.fyi.iter().filter(|f| within(f.ts)).cloned().collect() }
}

/// The first read of the board just finished, if it did.
fn caught_up(repo: &str, was_fetched: bool, fetched: &AtomicBool) {
    if !was_fetched && fetched.load(Relaxed) {
        with(repo, |s| s.away_until = now());
    }
}

/// Our requests, newest first.
pub fn outgoing() -> Vec<Outgoing> {
    cur(|s| s.sent.iter().rev().cloned().collect()).unwrap_or_default()
}

/// The UI drew this request where the person can see it: the sender learns it was seen.
pub fn mark_seen(from: &str, id: u64) {
    cur(|s| mark_seen_locked(s, from, id));
}

fn mark_seen_locked(s: &mut State, from: &str, id: u64) {
    let Some(i) = s.pending.iter_mut().find(|i| i.from == from && i.id == id) else { return };
    if i.seen() { return }
    i.history.push((Status::Seen, now()));
    if i.question.is_none() { status_out(s, from, id, Status::Seen, "") }
    s.dirty = true;
}

/// Do something with a waiting request. Accepting returns (session, text) to deliver, as `decide`.
pub fn act(from: &str, id: u64, action: Action) -> Result<Option<(String, String)>, String> {
    match action {
        Action::Accept => decide(from, id, "once").map(Some).ok_or_else(|| "that request isn't waiting any more".to_string()),
        Action::Always => decide(from, id, "always").map(Some).ok_or_else(|| "that request isn't waiting any more".to_string()),
        Action::Reply(t) => reply_request(from, id, &t).map(|_| None),
        Action::Decline(why) => {
            let why = why.unwrap_or_default();
            let done = cur(|s| decide_why_locked(s, from, id, "deny", why.trim()).is_some()).unwrap_or(false);
            if !done { return Err("that request isn't waiting any more".into()) }
            log(&format!("declined {from}'s request"));
            if let Some(r) = repo() { save_seen(&r) }
            Ok(None)
        }
        Action::Later(until) => cur(|s| {
            let i = s.pending.iter_mut().find(|i| i.from == from && i.id == id)?;
            i.snoozed = true;
            i.until = until;
            let tell = until > 0 && i.question.is_none();
            if tell { status_out(s, from, id, Status::Seen, &format!("seen, back to it {}", clock(until))) }
            s.dirty = true;
            Some(())
        }).flatten().map(|_| None).ok_or_else(|| "that request isn't waiting any more".to_string()),
    }
}

/// Put a request of ours back on the line if it isn't there (not once it's been answered).
/// It leaves the outbox only once they've acked past its id, and they drop any id at or below
/// that, so it goes under a new id; `was` tells them, so one they still hold isn't doubled.
pub fn resend(id: u64) -> Result<(), String> {
    cur(|s| resend_locked(s, id)).unwrap_or_else(|| Err("team mode is off".into()))
}

fn resend_locked(s: &mut State, id: u64) -> Result<(), String> {
    let i = s.sent.iter().position(|o| o.id == id).ok_or("no such request")?;
    let o = s.sent[i].clone();
    if o.status().done() { return Err(format!("it was already {}", o.status().as_str())) }
    if s.outbox.iter().any(|m| m["id"].as_u64() == Some(id)) { return Ok(()) }
    s.next_id += 1;
    let new = s.next_id;
    let mut was = o.was.clone();
    was.push(id);
    let mut m = json!({"id": new, "to": o.to, "session": o.session, "text": o.text, "from_session": o.from_session, "ts": o.ts, "was": was});
    if o.packet.is_object() { m["packet"] = o.packet.clone() }
    s.outbox.push(m);
    s.sent[i].id = new;
    s.sent[i].was = was;
    s.dirty = true;
    Ok(())
}

/// Take a request of ours back: it leaves their list (or never reaches it).
pub fn withdraw(id: u64) -> Result<(), String> {
    cur(|s| withdraw_locked(s, id)).unwrap_or_else(|| Err("team mode is off".into()))
}

fn withdraw_locked(s: &mut State, id: u64) -> Result<(), String> {
    {
        let o = s.sent.iter_mut().find(|o| o.id == id).ok_or("no such request")?;
        if o.status().done() { return Err(format!("it was already {}", o.status().as_str())) }
        o.history.push((Status::Withdrawn, now()));
        let to = o.to.clone();
        // still in our outbox doesn't mean they lack it (a waiting request stays unacked so a
        // restart can't lose it), so always tell them; it's a no-op if it never reached them
        s.outbox.retain(|m| m["id"].as_u64() != Some(id));
        if knows_status(s, &to) { // an older qh would show the notice as a new, empty request
            s.next_id += 1;
            s.outbox.push(json!({"id": s.next_id, "to": to, "kind": "w", "re": id, "ts": now()}));
        }
        s.dirty = true;
        Ok(())
    }
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
    pub unmerged: Vec<(String, Vec<String>)>, // what their copy changes that isn't on main yet: file, functions
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
            unmerged: unmerged_view(&p.unmerged),
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

/// The team's notes (`.qh/team-notes.md` on the board): every member publishes the copy they
/// hold, sealed with the rest of their folder; the notes are those copies merged line by line.
/// Additions only, each approved by the person whose agent proposed it.
fn team_notes_file(repo: &str) -> PathBuf {
    per_repo("team-notes", repo).join("team-notes.md")
}

/// Copies merged: every line once, in the order first seen.
pub fn merge_notes(copies: &[&str]) -> String {
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<&str> = copies.iter().flat_map(|c| c.lines()).map(str::trim_end).filter(|l| !l.trim().is_empty() && seen.insert(*l)).collect();
    if lines.is_empty() { String::new() } else { lines.join("\n") + "\n" }
}

/// This team project's notes, for a session's prompt; None when it isn't connected.
pub fn team_notes(cwd: &Path) -> Option<String> {
    let (_, repo) = repo_of(cwd)?;
    let mine = std::fs::read_to_string(team_notes_file(&repo)).unwrap_or_default();
    let theirs = with(&repo, |s| s.peers.values().filter(|p| p.joined && !s.removed.contains(&p.login))
        .map(|p| p.team_notes.clone()).collect::<Vec<_>>())?;
    let mut all = vec![mine.as_str()];
    all.extend(theirs.iter().map(String::as_str));
    Some(merge_notes(&all))
}

/// Add an approved note: merged into our copy, published with our next folder.
pub fn add_team_note(cwd: &Path, text: &str) -> Result<PathBuf, String> {
    let (_, repo) = repo_of(cwd).ok_or("not a team project")?;
    let who = me().unwrap_or_default();
    let line = format!("- {} {who}: {}", crate::memory::today(), text.replace('\n', " "));
    let now = team_notes(cwd).unwrap_or_default();
    let f = team_notes_file(&repo);
    std::fs::create_dir_all(f.parent().unwrap_or(Path::new("."))).map_err(|e| e.to_string())?;
    std::fs::write(&f, merge_notes(&[&now, &line])).map_err(|e| e.to_string())?;
    with(&repo, |s| s.dirty = true);
    Ok(f)
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
/// With `claims = "warn"` (`lock` false) the edit goes ahead and the session is told once instead.
/// None = go ahead; Some((notice, message, held)): held = the edit is not made and the message is
/// the tool's answer, otherwise it's a note added to the tool's answer.
pub fn check_edit(session: &str, t: &Touched, lock: bool) -> Option<(String, String, bool)> {
    let now = now();
    if ANYWAY.lock().unwrap().as_ref().and_then(|m| m.get(session)).is_some_and(|until| *until > now) { return None }
    let hold = |x: Option<(String, String)>| x.map(|(a, b)| (a, b, true));
    if t.funcs.is_empty() { return hold(check_contract(t)) }
    // warn softens the claim pause only: an edit that breaks a teammate's code is still held
    if !lock { return hold(check_contract(t)).or_else(|| check_warn(t)) }
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
    let Some((who, sess, func, ts, why)) = hit else { return hold(check_unmerged(t).or_else(|| check_contract(t))) };
    log_to(&t.repo, &format!("your {session} waits for {who} · {func} in {}", t.file));
    hold(Some((format!("paused: {who}/{sess} is changing {func} in {}. You'll be told when it's free · /anyway goes ahead", t.file),
          format!("error: paused, your edit was NOT made. {who}/{sess} is changing {func} in {} right now (last edit {}){why}. \
Meanwhile work on something that doesn't touch {func}, or stop and tell the user you're waiting for {who}; you'll get a message the \
moment {func} is free. Don't get around this by editing it another way. If the user wants you to go ahead anyway, they type /anyway.",
        t.file, ago(ts)))))
}

/// `claims = "warn"`: a teammate is changing this function now, or changed it and it isn't on main
/// yet. The edit is made; the session hears it once per function and teammate.
fn check_warn(t: &Touched) -> Option<(String, String, bool)> {
    let now = now();
    // someone on it right now comes before someone whose change just sits unmerged
    let (who, func, live, ts) = with(&t.repo, |s| {
        let peers = || s.peers.values().filter(|p| p.joined);
        peers().filter(|p| p.online).find_map(|p| p.claims.values().find_map(|files| {
            let fns = files.get(&t.file)?;
            t.funcs.iter().find_map(|f| fns.get(f).filter(|ts| now.saturating_sub(**ts) < CLAIM_HOT).map(|ts| (p.login.clone(), f.clone(), true, *ts)))
        })).or_else(|| peers().find_map(|p| {
            let fns = p.unmerged.get(&t.file)?;
            t.funcs.iter().find_map(|f| fns.get(f).map(|ts| (p.login.clone(), f.clone(), false, *ts)))
        }))
    }).flatten()?;
    let key = format!("warn:{}:{}:{who}", t.file, func);
    if !with(&t.repo, |s| if s.warned.contains(&key) { false } else { s.warned.push(key); true }).unwrap_or(false) { return None }
    let what = if live { format!("is changing {func} in {} right now (last edit {})", t.file, ago(ts)) }
        else { format!("also changed {func} in {}, not on main yet (first seen {})", t.file, ago(ts)) };
    log_to(&t.repo, &format!("{who} {what} · went ahead (claims = warn)"));
    Some((format!("heads-up: {who} {what} · edit made (claims = warn)"),
          format!("note: the edit was made, but {who} (a teammate) {what}, so the two changes may conflict. \
Tell the user so they can agree it with {who}; this is said once."), false))
}

/// A teammate's copy changes this function too and it isn't on main yet (online or not, however
/// they changed it): both changes would collide at merge. Held once per function, not paused:
/// nothing wakes it but their merge, so the agent hears it and the user decides.
fn check_unmerged(t: &Touched) -> Option<(String, String)> {
    let (who, func, since) = with(&t.repo, |s| s.peers.values().filter(|p| p.joined).find_map(|p| {
        let fns = p.unmerged.get(&t.file)?;
        t.funcs.iter().find_map(|f| fns.get(f).map(|ts| (p.login.clone(), f.clone(), *ts)))
    })).flatten()?;
    let key = format!("{}:{}:{who}", t.file, func);
    if !with(&t.repo, |s| if s.warned.contains(&key) { false } else { s.warned.push(key); true }).unwrap_or(false) { return None }
    log_to(&t.repo, &format!("{who} also changed {func} in {} (not on main yet)", t.file));
    Some((format!("held: {who} also changed {func} in {}, not on main yet. Talk to them first · /anyway goes ahead", t.file),
          format!("error: held, your edit was NOT made. {who} (a teammate) has changed {func} in {} too (first seen {}) and it isn't on main yet, \
so both changes would conflict when merged. Stop and tell the user, so they can agree it with {who}, or work on something else. \
If the user wants it anyway, they type /anyway (or retry: this is said once).", t.file, ago(since))))
}

/// Teammates' fresh code that uses a name, within the last hour: (login, session) pairs.
fn users_of(s: &State, name: &str) -> Vec<(String, String)> {
    users_within(s, name, 3600)
}

fn users_within(s: &State, name: &str, secs: u64) -> Vec<(String, String)> {
    let now = now();
    s.peers.values().filter(|p| p.joined).flat_map(|p| p.uses.iter()
        .filter(|(_, u)| u.get(name).is_some_and(|ts| now.saturating_sub(*ts) < secs))
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

/// Each file this copy changes against the merge base of main (commits, uncommitted edits and
/// untracked files; at most 200, none over 1 MB): (file, text before, text now).
fn changes_vs_main(root: &Path) -> Vec<(String, String, String)> {
    changes_since(root, false)
}

/// `changes_vs_main`; `mine` starts after the newest commit someone else wrote, so on a shared
/// branch (or one pulled from a teammate) their commits aren't counted as this copy's work.
fn changes_since(root: &Path, mine: bool) -> Vec<(String, String, String)> {
    let Some(mut base) = ["origin/HEAD", "origin/main", "origin/master", "@{upstream}"].iter()
        .find_map(|r| git(root, &["merge-base", "HEAD", r]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())) else { return vec![] };
    let me = git(root, &["var", "GIT_AUTHOR_IDENT"]).and_then(|i| i.split_once('<').and_then(|(_, r)| r.split_once('>')).map(|(e, _)| e.to_string()));
    if let (true, Some(me)) = (mine, me.filter(|e| !e.is_empty())) {
        let theirs = git(root, &["log", "--format=%H %ae", &format!("{base}..HEAD")]).unwrap_or_default().lines()
            .find_map(|l| l.split_once(' ').filter(|(_, e)| !e.eq_ignore_ascii_case(&me)).map(|(h, _)| h.to_string()));
        if let Some(h) = theirs { base = h }
    }
    let mut files: Vec<String> = git(root, &["diff", "--name-only", "--no-renames", &base]).unwrap_or_default().lines().map(String::from).collect();
    // untracked: new source files count; build output nobody ignored (.pyc and the like) isn't text, so it doesn't
    files.extend(git(root, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default().lines()
        .filter(|f| std::fs::read_to_string(root.join(f)).is_ok()).map(String::from));
    files.into_iter().filter(|f| !f.is_empty()).take(200).filter_map(|f| {
        let before = git(root, &["show", &format!("{base}:{f}")]).unwrap_or_default();
        let after = std::fs::read_to_string(root.join(&f)).unwrap_or_default();
        (before.len().max(after.len()) <= 1 << 20).then_some((f, before, after))
    }).collect()
}

/// What this copy changes that isn't on main yet, however it was changed (qh's agents, a shell,
/// an editor, a merge): committed or not, pushed to a branch or not. file -> function -> since
/// when ("" = the file, outside any function). Compared with the merge base of main, so a branch
/// that sits unmerged for days still shows; a teammate's commits we pulled aren't ours. `old` keeps each function's first-seen time.
fn scan_unmerged(root: &Path, old: &Claims) -> Claims {
    let now = now();
    let mut out: Claims = HashMap::new();
    for (f, before, after) in changes_since(root, true) {
        let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
        let (del, add) = changed_lines(&a, &b);
        let (oa, ob) = (owners(&before), owners(&after));
        let real = |lines: &[&str], i: usize| lines.get(i).is_some_and(|l| !l.trim().is_empty());
        let mut fns: Vec<String> = del.iter().filter(|&&i| real(&a, i)).map(|&i| oa.get(i).cloned().flatten().unwrap_or_default())
            .chain(add.iter().filter(|&&i| real(&b, i)).map(|&i| ob.get(i).cloned().flatten().unwrap_or_default())).collect();
        if fns.is_empty() { fns.push(String::new()) } // whitespace-only or binary: still the file
        let was = old.get(&f);
        let e = out.entry(f.clone()).or_default();
        for n in fns { let since = was.and_then(|w| w.get(&n)).copied().unwrap_or(now); e.insert(n, since); }
    }
    out
}

/// Where our unmerged work and a teammate's change the same function (or both change a file
/// outside any function): (login, file, function), sorted. Both would conflict at merge.
fn overlaps(s: &State) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = vec![];
    for p in s.peers.values().filter(|p| p.joined) {
        for (f, fns) in &s.unmerged {
            let Some(theirs) = p.unmerged.get(f) else { continue };
            v.extend(fns.keys().filter(|n| theirs.contains_key(*n)).map(|n| (p.login.clone(), f.clone(), n.clone())));
        }
    }
    v.sort();
    v
}

fn overlap_text(v: &[(String, String, String)]) -> String {
    v.iter().map(|(who, f, n)| if n.is_empty() { format!("{who}: {f}") } else { format!("{who}: {n} in {f}") }).collect::<Vec<_>>().join("; ")
}

/// Before an agent's `git push`: does what we push collide with a teammate's work that isn't on
/// main yet? Rescans first, so a commit made a second ago counts. Held once per set of overlaps.
pub fn push_overlap(cwd: &Path) -> Option<(String, String)> {
    let (root, repo) = repo_of(cwd)?;
    if !running_for(&repo) { return None }
    let old = with(&repo, |s| s.unmerged.clone())?;
    let fresh = scan_unmerged(&root, &old);
    let v = with(&repo, |s| { if s.unmerged != fresh { s.unmerged = fresh; s.dirty = true } overlaps(s) })?;
    if v.is_empty() { return None }
    let list = overlap_text(&v);
    let key = format!("push:{list}");
    if !with(&repo, |s| if s.warned.contains(&key) { false } else { s.warned.push(key); true }).unwrap_or(false) { return None }
    log_to(&repo, &format!("push held: it overlaps work not on main yet ({list})"));
    Some((format!("push held: teammates changed the same code, not on main yet ({list}) · push again to go ahead"),
          format!("error: push NOT run. Teammates changed the same functions and it isn't on main yet: {list}. Whoever merges \
second gets a conflict. Stop and tell the user so they can agree the order with them (or message them). If the user wants to push \
anyway, run the same push again: this is said once.")))
}

/// Across the change against main: names defined before that are gone now, and functions changed.
fn gone_and_changed(root: &Path) -> (Vec<String>, Vec<String>) {
    let (mut gone, mut changed): (Vec<String>, Vec<String>) = (vec![], vec![]);
    for (_, before, after) in changes_vs_main(root) {
        let (a, b): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
        let (del, add) = changed_lines(&a, &b);
        let kept = defines(&b, &(0..b.len()).collect::<Vec<_>>());
        gone.extend(defines(&a, &del).into_iter().filter(|n| !kept.contains(n)));
        let (oa, ob) = (owners(&before), owners(&after));
        changed.extend(del.iter().filter_map(|&i| oa.get(i).cloned().flatten())
            .chain(add.iter().filter_map(|&i| ob.get(i).cloned().flatten())));
    }
    (gone, changed)
}

/// What those break for teammates: (login, what, the same said to them), sorted. A gone name their
/// code from the last day uses, or a changed function they hold a promise on.
fn contract_breaks(s: &State, gone: &[String], changed: &[String]) -> Vec<(String, String, String)> {
    let mut v: Vec<(String, String, String)> = vec![];
    for n in gone {
        v.extend(users_within(s, n, 86400).into_iter().map(|(l, _)| (l.clone(), format!("{n} is removed or renamed and {l}'s code uses it"),
            format!("{n} is removed or renamed and your code uses it"))));
    }
    for p in s.peers.values().filter(|p| p.joined) {
        v.extend(p.promises.iter().filter(|x| changed.contains(&x.name))
            .map(|x| (p.login.clone(), format!("{} changed and {} counts on: \"{}\"", x.name, p.login, x.text),
                      format!("{} changed and you count on: \"{}\"", x.name, x.text))));
    }
    v.sort();
    v.dedup();
    v
}

/// Before an agent's `git commit` or `git push`: the contracts, checked on the whole change however
/// it was made (an editor or a shell edit never went through the per-edit check). A name that's
/// gone which a teammate's code from the last day uses, or a changed function a teammate holds a
/// promise on, holds it once, and each of those teammates is sent a request to agree it, so the
/// break reaches them before it lands rather than at merge.
pub fn commit_contracts(cwd: &Path) -> Option<(String, String)> {
    let (root, repo) = repo_of(cwd)?;
    if !running_for(&repo) { return None }
    let (gone, changed) = gone_and_changed(&root);
    // each break is said once: one already let through (and committed) stays off main until merge,
    // so without this every later commit would hold on it again and re-ask the teammate
    let breaks = with(&repo, |s| contract_breaks(s, &gone, &changed).into_iter().filter(|b| {
        let k = format!("contract:{}:{}", b.0, b.1);
        !s.warned.contains(&k) && { s.warned.push(k); true }
    }).collect::<Vec<_>>())?;
    if breaks.is_empty() { return None }
    let list = breaks.iter().map(|b| b.1.as_str()).collect::<Vec<_>>().join("; ");
    // the request: each teammate whose code it breaks hears it now, from us
    let sent: Vec<String> = with(&repo, |s| {
        let mut who: Vec<String> = breaks.iter().map(|b| b.0.clone()).collect();
        who.sort();
        who.dedup();
        who.into_iter().filter(|l| {
            let mine: Vec<&str> = breaks.iter().filter(|b| b.0 == *l).map(|b| b.2.as_str()).collect();
            dm_locked(s, l, &format!("I'm about to commit a change that breaks what your code counts on: {}. Reply if it has to \
stay as it was; otherwise update your side to match.", mine.join("; "))).is_ok()
        }).collect()
    }).unwrap_or_default();
    let told = if sent.is_empty() { String::new() } else { format!(" {} {} been sent a request about it.", sent.join(", "), if sent.len() == 1 { "has" } else { "have" }) };
    log_to(&repo, &format!("commit held: it breaks what teammates count on ({list})"));
    Some((format!("held: this breaks what teammates count on ({list}) · run it again to go ahead"),
          format!("error: NOT run. The change breaks what teammates' code counts on: {list}.{told} Either keep what they use \
(for a rename: keep the old name too, delegating to the new one), or stop and tell the user so they can agree it with them. If \
the user wants it anyway, run the same command again: this is said once.")))
}

/// Every few seconds: let go of claims on what we pushed (or that went stale), and wake our
/// sessions whose function is free again.
fn wake_check(repo: &str, root: &Path, hooks: &Hooks) {
    // per repo: with two team projects open, one's check mustn't push back the other's
    static LAST_RELEASE: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    static LAST_SCAN: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    static LAST_TIPS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
    let every = |m: &Mutex<Option<HashMap<String, Instant>>>, secs: u64, now: bool| {
        let mut l = m.lock().unwrap();
        let l = l.get_or_insert_with(HashMap::new);
        let due = now || l.get(repo).is_none_or(|t| t.elapsed() >= Duration::from_secs(secs));
        if due { l.insert(repo.to_string(), Instant::now()); }
        due
    };
    let due = every(&LAST_RELEASE, 20, false);
    let now = now();
    // a commit, push or pull moves these: scan now, or a teammate who pulls our push straight
    // away is held for up to a minute on work that is already on main (seen live)
    // one process every few seconds, not one per name; %(HEAD) catches switching branch
    let tips = git(root, &["for-each-ref", "--format=%(HEAD)%(objectname)", "refs/heads", "refs/remotes/origin"]).unwrap_or_default();
    let moved = LAST_TIPS.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), tips.clone()) != Some(tips);
    let scan = every(&LAST_SCAN, 60, moved);
    if scan {
        let old = with(repo, |s| s.unmerged.clone()).unwrap_or_default();
        let p = scan_unmerged(root, &old);
        // however the change was made (by hand in an editor too), say once when it overlaps a teammate's
        let v = with(repo, |s| { if s.unmerged != p { s.unmerged = p; s.dirty = true } overlaps(s) }).unwrap_or_default();
        let fresh: Vec<_> = with(repo, |s| v.iter().filter(|o| {
            let k = format!("overlap:{}:{}:{}", o.0, o.1, o.2);
            !s.warned.contains(&k) && { s.warned.push(k); true }
        }).cloned().collect()).unwrap_or_default();
        if !fresh.is_empty() {
            say(repo, hooks, format!("heads-up: you and a teammate both changed code that isn't on main yet ({}); agree who merges first", overlap_text(&fresh)));
        }
    }
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

/// Unmerged work as the map lists it: files in order, the functions in each (empty for a whole file).
fn unmerged_view(c: &Claims) -> Vec<(String, Vec<String>)> {
    let mut v: Vec<(String, Vec<String>)> = c.iter().map(|(f, fns)| {
        let mut n: Vec<String> = fns.keys().filter(|k| !k.is_empty()).cloned().collect();
        n.sort();
        (f.clone(), n)
    }).collect();
    v.sort();
    v
}

/// What our own copy changes that isn't on main yet.
pub fn my_unmerged() -> Vec<(String, Vec<String>)> {
    cur(|s| unmerged_view(&s.unmerged)).unwrap_or_default()
}

/// Names a session's code used in the last hour.
fn uses_view(u: Option<&HashMap<String, u64>>) -> Vec<String> {
    let now = now();
    u.into_iter().flatten().filter(|(_, t)| now.saturating_sub(**t) < 3600).map(|(n, _)| n.clone()).collect()
}

/// What online teammates are editing right now (the state behind the conflict warnings):
/// "rahul/api: server.js (handle, route)", most recent first.
pub fn editing() -> Vec<String> {
    cur(|s| editing_in(&s.peers)).unwrap_or_default()
}

fn editing_in(peers: &HashMap<String, Peer>) -> Vec<String> {
    let now = now();
    let mut v: Vec<(u64, String)> = vec![];
    for p in peers.values().filter(|p| p.joined) {
        for (f, fns) in &p.unmerged {
            // what's also a hot claim is listed below with its session; show the rest
            let hot = |n: Option<&String>| p.online && p.claims.values().filter_map(|c| c.get(f))
                .any(|x| x.iter().any(|(k, ts)| n.is_none_or(|n| n == k) && now.saturating_sub(*ts) < CLAIM_HOT));
            let named: Vec<&String> = fns.keys().filter(|n| !n.is_empty()).collect();
            let mut names: Vec<&String> = named.iter().copied().filter(|n| !hot(Some(n))).collect();
            if if named.is_empty() { hot(None) } else { names.is_empty() } { continue }
            names.sort();
            let list = if names.is_empty() { String::new() } else { format!(" ({})", names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")) };
            v.push((fns.values().copied().min().unwrap_or(0), format!("{}: {f}{list} · not on main yet", p.login)));
        }
    }
    for p in peers.values().filter(|p| p.online) {
        for (session, c) in &p.claims {
            for (f, fns, ts) in claims_view(Some(c)).into_iter().filter(|x| now.saturating_sub(x.2) < CLAIM_HOT) {
                let fns = if fns.is_empty() { String::new() } else { format!(" ({})", fns.join(", ")) };
                v.push((ts, format!("{}/{session}: {f}{fns}", p.login)));
            }
        }
    }
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    v.into_iter().map(|x| x.1).collect()
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
    cur(|s| {
        // on screen (or just read): keep it current over the line
        match &mut s.watching {
            Some((l, n, at)) if l == login && n == session => *at = Instant::now(),
            w => *w = Some((login.to_string(), session.to_string(), Instant::now())),
        }
        s.peers.get(login).and_then(|p| p.transcripts.get(session)).cloned()
    }).flatten().unwrap_or_default()
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
/// One qh process per team repo holds its connection: two (a project's own window and another
/// one keeping it in the background) would both take in and ack the same messages, each
/// dropping answers to questions only the other asked, and keep replacing each other's line.
static LOCKS: Mutex<Option<HashMap<String, std::fs::File>>> = Mutex::new(None);

fn hold_connection(repo: &str, background: bool) -> Result<(), String> {
    hold_connection_as(repo, background, team_window())
}

/// `hold_connection`, saying whether we are the `qh team` window.
fn hold_connection_as(repo: &str, background: bool, team: bool) -> Result<(), String> {
    let path = per_repo("team-lock", repo);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).map_err(|e| e.to_string())?;
    let want = per_repo("team-want", repo);
    let _ = std::fs::create_dir_all(want.parent().unwrap_or(&want));
    let t0 = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                let held = std::fs::read_to_string(per_repo("team-owner", repo)).unwrap_or_default();
                let (pid, mode) = held.split_once(' ').unwrap_or((held.trim(), ""));
                // a window opened in the project itself takes the team from one keeping it in the background
                // one that is quitting still holds it while it tells teammates (up to 5 s): wait that out,
                // or a quick restart comes up without the team (seen live)
                // plain `qh` gives way to `qh team` too: its notice sends you there to answer a request
                let gives = mode.trim() == "background" || (mode.trim() == "solo" && !background && team);
                let wait = if gives { 15 } else { 7 };
                if background || t0.elapsed() > Duration::from_secs(wait) {
                    let _ = std::fs::remove_file(&want);
                    return Err(format!("team {repo} is connected in another qh (pid {pid}); close that one first"));
                }
                let _ = std::fs::write(&want, format!("{}{}", std::process::id(), if team { " team" } else { "" }));
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.to_string()),
        }
    }
    let _ = std::fs::remove_file(&want);
    // who holds it goes in a file of its own: Windows locks are mandatory, so nobody else can read the locked one
    let owner = per_repo("team-owner", repo);
    let _ = std::fs::create_dir_all(owner.parent().unwrap_or(&owner));
    let mode = if background { "background" } else if team { "project" } else { "solo" };
    let _ = std::fs::write(&owner, format!("{} {mode}", std::process::id()));
    LOCKS.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), f);
    Ok(())
}

/// Nobody holds `repo`'s team connection (checked by taking the lock and letting it go).
fn connection_free(repo: &str) -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open(per_repo("team-lock", repo)).is_ok_and(|f| f.try_lock().is_ok())
}

static RETAKE: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Projects plain qh handed to `qh team` that are free again: connect them.
pub fn take_retake() -> Vec<String> {
    std::mem::take(&mut *RETAKE.lock().unwrap())
}

fn release(repo: &str) {
    if let Some(m) = LOCKS.lock().unwrap().as_mut() { m.remove(repo); }
}

/// This process is the `qh team` window (plain `qh` keeps holds and claims working underneath).
fn team_window() -> bool {
    crate::ui::TEAM_UI.load(Relaxed)
}

/// How we hold `repo`: background, solo (plain `qh`), project (`qh team`); "" if we don't.
fn held_as(repo: &str) -> String {
    let mine = std::fs::read_to_string(per_repo("team-owner", repo)).unwrap_or_default();
    mine.strip_prefix(&format!("{} ", std::process::id())).unwrap_or("").trim().to_string()
}

/// We hold `repo` in the background and the qh opened in that project asked for it, or we're
/// plain `qh` and `qh team` asked.
fn wanted_elsewhere(repo: &str) -> bool {
    let want = std::fs::read_to_string(per_repo("team-want", repo));
    match held_as(repo).as_str() {
        "background" => want.is_ok(),
        "solo" => want.is_ok_and(|w| w.trim_end().ends_with(" team")),
        _ => false,
    }
}

pub fn stop() {
    cur(|s| s.running.store(false, Relaxed));
}

/// Disconnect every project and wait (up to 5 s) for the "offline" writes, at quit. Only as the
/// process exits: a second call shares the first one's 5 s (`QUIT_AT`).
pub fn stop_and_flush() {
    let t0 = *QUIT_AT.get_or_init(Instant::now); // one wait, however many of us run it
    let flags: Vec<std::sync::Arc<AtomicBool>> = STATES.lock().unwrap().get_or_insert_with(HashMap::new).values()
        // every one not yet finished, not just those this call stopped: a closed window runs this
        // twice at once (the kill handler and the main loop), and the one that returns early exits
        .filter(|s| { s.running.store(false, Relaxed); !s.finished.load(Relaxed) }).map(|s| s.finished.clone()).collect();
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
    github_repo_cached(cwd).is_some_and(|(_, r)| enabled(&r))
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
    let on_screen = cur(|s| {
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
    });
    // qh started outside the team's folder: still connected, one key away
    let mut out = match on_screen {
        Some(o) => o,
        None => {
            let c = CURRENT.lock().unwrap().clone();
            let bg: Vec<String> = STATES.lock().unwrap().get_or_insert_with(HashMap::new).values()
                .filter(|s| s.running.load(Relaxed)).map(|s| s.repo.clone()).collect();
            background_hint(&c, &bg)?
        }
    };
    for (r, n) in waiting_elsewhere() {
        out += &format!(" · {n} waiting in {}", r.rsplit('/').next().unwrap_or(&r));
    }
    Some(out)
}

/// "team <name> · Ctrl+] to open" for a team connected while another project is on screen.
fn background_hint(current: &str, running: &[String]) -> Option<String> {
    let mut v: Vec<&String> = running.iter().filter(|r| *r != current).collect();
    v.sort();
    let r = v.first()?;
    Some(format!("team {} · Ctrl+] to open", r.rsplit('/').next().unwrap_or(r)))
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
static APPLY: Mutex<()> = Mutex::new(()); // one teammate's folder taken in at a time: from git, or off the line
static LINE_FAILED: AtomicU64 = AtomicU64::new(0); // messages the line gave up on
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
        LinkEvent::LanBlocked => {
            for repo in LIVE.lock().unwrap().as_ref().map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                log_to(&repo, "macOS is blocking qh from your local network, so teammates here go by relay: \
                    allow it in System Settings > Privacy & Security > Local Network, then restart qh");
            }
        }
        LinkEvent::Failed { .. } => { LINE_FAILED.fetch_add(1, Relaxed); }
        LinkEvent::Delivered { .. } => {}
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

/// The line to `login` for the team screen: ("lan · 3ms", None) when up, (None, why) when not.
pub fn line_detail(login: &str) -> (Option<String>, Option<String>) {
    let Some(key) = cur(|s| s.peers.get(login).map(|p| p.pub_key.clone())).flatten() else { return (None, None) };
    let Some(Some(l)) = LINK.get() else { return (None, Some("the direct line is off on this machine".into())) };
    match l.lines().into_iter().find(|x| x.public == key) {
        Some(x) => (x.path.as_deref().map(|p| with_rtt(describe_line(p), x.rtt_ms)), x.why),
        None => (None, None),
    }
}

fn with_rtt(how: String, rtt: Option<u64>) -> String {
    match rtt { Some(ms) => format!("{how} · {ms}ms"), None => how }
}

/// `/team link`: how each teammate is reached, in full: path, round trip, addresses, relays, and
/// why not when there's no line.
pub fn link_report() -> String {
    let Some(Some(l)) = LINK.get() else { return "the direct line isn't running (QH_LINK=off, or team mode hasn't started it yet): updates go by git".into() };
    let mut out = format!("this machine: {}{}\n", if l.addrs().is_empty() { "no addresses found yet".into() } else { l.addrs().join(", ") },
        if l.strict_nat() { " · strict NAT (a relay is the likely path)" } else { "" });
    let lines = l.lines();
    if lines.is_empty() { return out + "no teammates on the line yet" }
    for x in lines {
        let who = who_is(&x.public).into_iter().next().map_or_else(|| x.public[..8.min(x.public.len())].to_string(), |(_, l)| l);
        match &x.path {
            Some(p) => out += &format!("\n{who}: {}\n  path {p}\n", with_rtt(describe_line(p), x.rtt_ms)),
            None => out += &format!("\n{who}: by git{}\n  {}\n", x.trying_secs.map(|t| format!(" (trying for {t}s)")).unwrap_or_default(),
                x.why.as_deref().unwrap_or("")),
        }
        out += &format!("  their addresses: {}\n  relays: {}\n", if x.addrs.is_empty() { "none known".into() } else { x.addrs.join(", ") },
            if x.relays.is_empty() { "none".into() } else { x.relays.join(", ") });
    }
    out.trim_end().to_string()
}

/// What went over the line last, and to whom.
struct Live {
    last: Option<Value>,
    at: Instant,
    pulse: u64,
    told: Vec<String>, // member keys that have our latest
    bytes: usize,      // size of the last message
    failed: u64,       // LINE_FAILED when we last looked
    watchers: Vec<Watcher>,
    asked: Option<(String, String, Instant)>, // the session we last said we're watching, and when
}

/// A teammate with one of our sessions open. They get only what changed, about once a second, for
/// as long as they keep saying so; each time they also say what they hold, so a lost update mends itself.
struct Watcher {
    login: String,
    key: String, // their member key: where the line sends
    session: String,
    until: Instant,
    sent: Vec<(String, String)>, // the rows they should hold
    before: u64,                 // the hash of what they held before the last send (it may still be on its way)
    have: Option<u64>,           // the hash they last said they hold, not yet looked at
    at: Instant,
}

/// A stable hash of transcript rows (both machines compute it, whatever their Rust version).
fn rows_hash(rows: &[(String, String)]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for (role, text) in rows {
        for b in role.bytes().chain([0]).chain(text.bytes()).chain([0]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn rows_of(v: &Value) -> Vec<(String, String)> {
    v.as_array().into_iter().flatten().map(|m| (m[0].as_str().unwrap_or("").to_string(), m[1].as_str().unwrap_or("").to_string())).collect()
}

/// How `new` follows from `old`: drop this many rows off the front, keep this many of the rest,
/// and add everything after them. Rows only leave from the front (the window moves on) or change at the end.
fn rows_diff(old: &[(String, String)], new: &[(String, String)]) -> (usize, usize) {
    (0..=old.len()).map(|drop| (drop, old[drop..].iter().zip(new).take_while(|(a, b)| a == b).count()))
        .fold((0, 0), |best, c| if c.1 > best.1 { c } else { best })
}

/// One round of the live transcripts: tell the teammate we're watching that we are, and send those
/// watching us whatever changed.
fn watch_round(repo: &str, me: &str, l: &Link, shown: &Value, live: &mut Live) {
    let Some(Some(key)) = with(repo, |s| s.key) else { return };
    let lines = LINES.lock().unwrap().clone().unwrap_or_default();
    let ask = with(repo, |s| {
        let (login, session, drawn) = s.watching.clone()?;
        if drawn.elapsed() >= WATCH_LIFE { return None }
        let p = s.peers.get(&login)?;
        let now = std::mem::take(&mut s.watch_now);
        let p_key = p.pub_key.clone();
        let have = rows_hash(p.transcripts.get(&session).map_or(&[][..], |r| &r[..]));
        Some((login, session, p_key, have, now))
    }).flatten();
    if let Some((login, session, peer, have, now)) = ask {
        let same = live.asked.as_ref().is_some_and(|(l, s, t)| *l == login && *s == session && t.elapsed() < WATCH_EVERY);
        if (now || !same) && lines.contains_key(&peer) {
            l.send(&peer, &file_msg(repo, me, &key, "watch", &json!({"session": session, "have": have})));
            live.asked = Some((login, session, Instant::now()));
        }
    }
    let now = Instant::now();
    live.watchers.retain(|w| w.until > now && lines.contains_key(&w.key));
    for w in &mut live.watchers {
        let rows = rows_of(&shown[&w.session]);
        let hash = rows_hash(&rows);
        if let Some(have) = w.have.take() {
            reconcile(w, have, hash, &rows);
        }
        if rows == w.sent || w.at.elapsed() < TR_EVERY {
            continue;
        }
        let (drop, keep) = rows_diff(&w.sent, &rows);
        let body = json!({"session": w.session, "seq": now_ms(), "drop": drop, "keep": keep, "rows": rows[keep..].iter().map(|(r, t)| json!([r, t])).collect::<Vec<_>>(), "hash": hash});
        l.send(&w.key, &file_msg(repo, me, &key, "tr", &body));
        w.before = rows_hash(&w.sent);
        w.sent = rows;
        w.at = now;
    }
}

/// What a watcher says it holds, against what we think it holds. If it's what they held before
/// our last send, that send may still be on its way, but after a while it's lost. Nothing else
/// would mend that: with no new rows, nothing goes out again.
fn reconcile(w: &mut Watcher, have: u64, hash: u64, rows: &[(String, String)]) {
    if have == hash {
        w.sent = rows.to_vec();
    } else if have != rows_hash(&w.sent) && (have != w.before || w.at.elapsed() >= TR_LOST_AFTER) {
        w.sent.clear(); // they lost something: send it whole
    }
}

/// A teammate opened one of our sessions, or still has it open.
fn take_watch(repo: &str, from: &str, v: &Value, live: &mut Live) {
    let login = v["login"].as_str().unwrap_or("").to_string();
    let Some(Some(key)) = with(repo, |s| s.peers.get(&login).filter(|p| p.pub_key == from && p.joined && login != s.me).and(s.key)) else { return };
    let Some(body) = open_file_msg(&key, v) else { return };
    let session = body["session"].as_str().unwrap_or("").to_string();
    let now = Instant::now();
    let i = live.watchers.iter().position(|w| w.login == login).unwrap_or_else(|| {
        live.watchers.push(Watcher { login: login.clone(), key: from.to_string(), session: session.clone(), until: now, sent: vec![], before: 0, have: None, at: instant_ago(TR_EVERY) });
        live.watchers.len() - 1
    });
    let w = &mut live.watchers[i];
    if w.session != session {
        w.session = session;
        w.sent.clear();
    }
    w.until = now + WATCH_LIFE;
    w.have = body["have"].as_u64();
}

/// What changed in a session we're watching. Applied to what we hold; if that isn't what they
/// think we hold, ask again and they send it whole.
fn take_rows(s: &mut State, login: &str, body: &Value) -> bool {
    let (session, seq) = (body["session"].as_str().unwrap_or(""), body["seq"].as_u64().unwrap_or(0));
    let (drop, keep) = (body["drop"].as_u64().unwrap_or(0) as usize, body["keep"].as_u64().unwrap_or(0) as usize);
    let Some(p) = s.peers.get_mut(login) else { return false };
    if seq <= p.tr_seq.get(session).copied().unwrap_or(0) {
        return false;
    }
    let held = p.transcripts.get(session).map_or(&[][..], |r| &r[..]);
    let rows = held.get(drop..drop.saturating_add(keep)).map(|k| [k, &rows_of(&body["rows"])[..]].concat());
    match rows.filter(|r| r.len() <= 200 && Some(rows_hash(r)) == body["hash"].as_u64()) {
        Some(r) => {
            p.transcripts.insert(session.to_string(), r);
            p.tr_seq.insert(session.to_string(), seq);
            true
        }
        None => { s.watch_now = true; false }
    }
}

fn take_rows_msg(repo: &str, from: &str, v: &Value, hooks: &Hooks) {
    let login = v["login"].as_str().unwrap_or("").to_string();
    let Some(Some(key)) = with(repo, |s| s.peers.get(&login).filter(|p| p.pub_key == from && p.joined && login != s.me).and(s.key)) else { return };
    let Some(body) = open_file_msg(&key, v) else { return };
    if with(repo, |s| take_rows(s, &login, &body)) == Some(true) {
        bump();
        (hooks.refresh)();
    }
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
        // the lists are capped, so their lengths stop moving: the newest ids do
        [&s.my_chat, &s.my_dms, &s.my_gmsgs].map(|l| l.last().and_then(|m| m["id"].as_u64())).hash(&mut h);
        s.claims.values().map(|c| c.values().map(|f| f.len()).sum::<usize>()).sum::<usize>().hash(&mut h);
        h.finish()
    }).unwrap_or(0)
}

/// Send my data to every teammate on the line: everyone when it changed, and anyone newly on
/// the line (or back on it) whatever happens.
fn push_live(repo: &str, me: &str, l: &Link, d: &Value, live: &mut Live) {
    let Some((Some(key), members)) = with(repo, |s| (s.key, s.peers.values().filter(|p| p.joined).map(|p| p.pub_key.clone()).collect::<Vec<_>>())) else { return };
    let lines = LINES.lock().unwrap().clone().unwrap_or_default();
    let on: Vec<String> = members.into_iter().filter(|k| lines.contains_key(k)).collect();
    live.told.retain(|k| on.contains(k));
    // a message the line gave up on: nobody can be counted as having our latest
    let failed = LINE_FAILED.load(Relaxed);
    if failed != live.failed {
        live.failed = failed;
        live.told.clear();
    }
    let p = pulse(repo);
    // the whole state goes each time, so what isn't urgent is paced by its size: 1 s for a small
    // team, 4 s at 33 KB. A relayed line has 2 MB a minute and drops the rest.
    let every = Duration::from_millis((live.bytes as u64 / 8).max(1000));
    let all = live.last.as_ref() != Some(d) && (p != live.pulse || live.at.elapsed() >= every);
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
    live.bytes = bytes.len();
    for k in to {
        l.send(&k, &bytes);
        live.told.push(k);
    }
}

/// A teammate's data straight off the line. Only from someone git already showed us as a member,
/// only from their own member key, and only if it's newer than what we have.
fn take_live(repo: &str, root: &Path, from: &str, bytes: &[u8], hooks: &Hooks, live: &mut Live) {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else { return };
    match v["kind"].as_str() {
        Some("file") => return take_file_req(repo, root, from, &v),
        Some("file_re") => return take_file_reply(repo, from, &v),
        Some("msg") => return take_quick_msg(repo, from, &v),
        Some("watch") => return take_watch(repo, from, &v, live),
        Some("tr") => return take_rows_msg(repo, from, &v, hooks),
        _ => {}
    }
    let _one = APPLY.lock().unwrap();
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
        invites: old.invites, transcripts: old.transcripts, qh: old.qh, ..Default::default()
    };
    parse_data(&mut p, &d);
    let mut new_chat = false;
    take_in(repo, &me, key, p, hooks, &mut new_chat);
    if new_chat { save_chat(repo) }
    save_seen(repo);
}

// ---- messages over the direct line first ----
//
// A #team line, DM or group line also goes straight to every teammate on the line (~1s instead of
// a board write every 10s and a poll every 3s). The board still carries it, as the fallback for
// whoever isn't on the line and as history; the receiving side tells copies apart by the sender's
// message id, so a line shows once whichever way it came first.

/// Send one of our new lines to the teammates it's for who are on the line now. Only on a line
/// that's already running: sending never starts one.
fn quick_send(repo: &str, body: &Value) {
    let Some(Some(l)) = LINK.get() else { return };
    let to: Option<Vec<String>> = body["to"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
    let Some((me, Some(key), keys)) = with(repo, |s| (s.me.clone(), s.key, s.peers.values()
        .filter(|p| p.joined && !p.pub_key.is_empty() && to.as_ref().is_none_or(|t| t.contains(&p.login)))
        .map(|p| p.pub_key.clone()).collect::<Vec<_>>())) else { return };
    let lines = LINES.lock().unwrap().clone().unwrap_or_default();
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() { o.remove("to"); }
    let bytes = file_msg(repo, &me, &key, "msg", &body);
    for k in keys.iter().filter(|k| lines.contains_key(*k)) {
        l.send(k, &bytes);
    }
}

/// A line straight off the direct line: only from a member, on their own member key.
fn take_quick_msg(repo: &str, from: &str, v: &Value) {
    let login = v["login"].as_str().unwrap_or("").to_string();
    let Some(Some(key)) = with(repo, |s| s.peers.get(&login).filter(|p| p.pub_key == from && p.joined && login != s.me).and(s.key)) else { return };
    let Some(body) = open_file_msg(&key, v) else { return };
    let chat = body["chan"] == "#team";
    if with(repo, |s| take_quick(s, &login, &body)) == Some(true) {
        bump();
        if chat { save_chat(repo) }
    }
}

/// Take in one line that came over the direct line; false when we already have it (or it's bad).
/// A team line marks its id as had, so the same line in their next folder isn't shown again; it
/// doesn't move the "taken up to" mark, so an older line that went by git only still comes in.
fn take_quick(s: &mut State, login: &str, body: &Value) -> bool {
    let (Some(id), ts) = (body["id"].as_u64(), body["ts"].as_u64().unwrap_or_else(now)) else { return false };
    match body["chan"].as_str() {
        Some("#team") => {
            let text = body["text"].as_str().unwrap_or("").to_string();
            if id <= s.chat_seen.get(login).copied().unwrap_or(0) || !s.msg_ids.insert((format!("chat:{login}"), id)) { return false }
            insert_chat(s, ts, login, &text);
            s.unread += 1;
            if pings(&text, &s.me.clone()) {
                if !s.pinged.iter().any(|c| c == "#team") { s.pinged.push("#team".into()) }
                alert(format!("qh: {login} mentioned you: {}", clip_to(&text, 80)));
            }
            true
        }
        Some(c @ ("dm" | "g")) => {
            // the same path as their folder's boxes: a peer copy with just this one line
            let Some(old) = s.peers.get(login) else { return false };
            let mut p = Peer { login: login.into(), pub_key: old.pub_key.clone(), joined: old.joined, ..Default::default() };
            let b = body["box"].as_str().unwrap_or("").to_string();
            if c == "dm" { p.dms = vec![(id, ts, b)] } else { p.gmsgs = vec![(id, ts, body["g"].as_str().unwrap_or("").into(), b)] }
            take_private(s, login, &p)
        }
        _ => false,
    }
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
const FILE_PART: usize = 24 * 1024; // bytes of text per line message: small parts keep a relayed line responsive
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
pub(crate) mod tests {
    /// A newcomer waiting to be let in, for other modules' tests.
    pub(crate) fn ask_to_join(login: &str) { super::cur(|s| s.asking.push(login.into())); }

    #[test]
    fn editing_lists_only_hot_claims_of_online_teammates() {
        use std::collections::HashMap;
        let t = super::now();
        let claim = |f: &str, func: &str, ts: u64| -> super::Claims { HashMap::from([(f.to_string(), HashMap::from([(func.to_string(), ts)]))]) };
        let peer = |login: &str, online: bool, c: super::Claims| super::Peer {
            login: login.into(), online, claims: HashMap::from([("api".to_string(), c)]), ..Default::default() };
        let peers = HashMap::from([
            ("rahul".to_string(), peer("rahul", true, claim("server.js", "route", t))),
            ("anu".to_string(), peer("anu", false, claim("db.js", "save", t))),
            ("old".to_string(), peer("old", true, claim("x.js", "", t - super::CLAIM_HOT - 5))),
        ]);
        assert_eq!(super::editing_in(&peers), vec!["rahul/api: server.js (route)".to_string()]);
    }

    #[test]
    fn unmerged_work_shows_even_offline_without_repeating_hot_claims() {
        use std::collections::HashMap;
        let t = super::now();
        let mut anu = super::Peer { login: "anu".into(), joined: true, online: false, ..Default::default() };
        anu.unmerged = HashMap::from([("db.js".to_string(), HashMap::from([("save".to_string(), t - 86400)]))]);
        let mut rahul = super::Peer { login: "rahul".into(), joined: true, online: true, ..Default::default() };
        rahul.claims = HashMap::from([("api".to_string(), HashMap::from([("server.js".to_string(), HashMap::from([("route".to_string(), t)]))]))]);
        rahul.unmerged = HashMap::from([("server.js".to_string(), HashMap::from([("route".to_string(), t), ("listen".to_string(), t)]))]);
        let peers = HashMap::from([("anu".to_string(), anu), ("rahul".to_string(), rahul)]);
        let mut got = super::editing_in(&peers);
        got.sort();
        assert_eq!(got, vec!["anu: db.js (save) · not on main yet", "rahul/api: server.js (route)", "rahul: server.js (listen) · not on main yet"]);
    }

    #[test]
    fn unmerged_comes_from_git_whatever_made_the_change() {
        let d = std::env::temp_dir().join(format!("qh-pending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let run = |dir: &std::path::Path, a: &[&str]| assert!(std::process::Command::new("git").current_dir(dir).args(a)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output().unwrap().status.success(), "{a:?}");
        let (up, w) = (d.join("up.git"), d.join("w"));
        std::fs::create_dir_all(&w).unwrap();
        run(&d, &["init", "-q", "--bare", "-b", "main", up.to_str().unwrap()]);
        run(&w, &["init", "-q", "-b", "main"]);
        std::fs::write(w.join("a.py"), "def one():\n    return 1\n\ndef two():\n    return 2\n").unwrap();
        run(&w, &["add", "."]);
        run(&w, &["commit", "-qm", "a"]);
        run(&w, &["remote", "add", "origin", up.to_str().unwrap()]);
        run(&w, &["push", "-q", "origin", "main"]);
        run(&w, &["remote", "set-head", "origin", "main"]);
        run(&w, &["config", "user.email", "t@t"]); // who "we" are when qh asks git
        run(&w, &["config", "user.name", "t"]);
        assert!(super::scan_unmerged(&w, &Default::default()).is_empty(), "nothing off main");
        // a branch with a committed change (as a merge or another tool would leave it), plus a shell edit and a new file
        run(&w, &["checkout", "-qb", "side"]);
        std::fs::write(w.join("a.py"), "def one():\n    return 11\n\ndef two():\n    return 2\n").unwrap();
        run(&w, &["commit", "-qam", "b"]);
        std::fs::write(w.join("a.py"), "def one():\n    return 11\n\ndef two():\n    return 22\n").unwrap();
        std::fs::write(w.join("new.txt"), "hi\n").unwrap();
        std::fs::write(w.join("cache.pyc"), [0xa7u8, 0x0d, 0xff, 0x00]).unwrap();
        let old = std::collections::HashMap::from([("a.py".to_string(), std::collections::HashMap::from([("one".to_string(), 5u64)]))]);
        let p = super::scan_unmerged(&w, &old);
        assert_eq!(p["a.py"].len(), 2);
        assert_eq!(p["a.py"]["one"], 5, "first-seen time kept");
        assert!(p["a.py"].contains_key("two"));
        assert!(p["new.txt"].contains_key(""));
        assert!(!p.contains_key("cache.pyc"), "untracked build output is not work");
        // a teammate's commit on the shared branch (pulled): only what's ours after it counts
        let _ = std::fs::remove_file(w.join("new.txt"));
        run(&w, &["checkout", "-q", "--", "a.py"]);
        std::fs::write(w.join("a.py"), "def one():\n    return 11\n\ndef two():\n    return 222\n").unwrap();
        assert!(std::process::Command::new("git").current_dir(&w).args(["commit", "-qam", "theirs"])
            .env("GIT_AUTHOR_NAME", "bob").env("GIT_AUTHOR_EMAIL", "bob@b").env("GIT_COMMITTER_NAME", "bob").env("GIT_COMMITTER_EMAIL", "bob@b")
            .output().unwrap().status.success());
        assert!(super::scan_unmerged(&w, &Default::default()).is_empty(), "their commit, pulled, isn't our work");
        std::fs::write(w.join("a.py"), "def one():\n    return 111\n\ndef two():\n    return 222\n").unwrap();
        let p = super::scan_unmerged(&w, &Default::default());
        assert_eq!(p["a.py"].keys().collect::<Vec<_>>(), vec!["one"], "only ours after it");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_commit_that_breaks_what_a_teammate_counts_on_is_found_however_the_edit_was_made() {
        let d = std::env::temp_dir().join(format!("qh-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let run = |dir: &std::path::Path, a: &[&str]| assert!(std::process::Command::new("git").current_dir(dir).args(a)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .output().unwrap().status.success(), "{a:?}");
        let (up, w) = (d.join("up.git"), d.join("w"));
        std::fs::create_dir_all(&w).unwrap();
        run(&d, &["init", "-q", "--bare", "-b", "main", up.to_str().unwrap()]);
        run(&w, &["init", "-q", "-b", "main"]);
        std::fs::write(w.join("a.py"), "def get_user(i):\n    return i\n\ndef rate():\n    return 0.5\n").unwrap();
        run(&w, &["add", "."]);
        run(&w, &["commit", "-qm", "a"]);
        run(&w, &["remote", "add", "origin", up.to_str().unwrap()]);
        run(&w, &["push", "-q", "origin", "main"]);
        run(&w, &["remote", "set-head", "origin", "main"]);
        // by hand: get_user renamed, rate changed
        std::fs::write(w.join("a.py"), "def fetch_user(i):\n    return i\n\ndef rate():\n    return 50\n").unwrap();
        let (gone, changed) = super::gone_and_changed(&w);
        assert_eq!(gone, vec!["get_user".to_string()]);
        assert!(changed.contains(&"rate".to_string()));
        let mut st = state("kesav");
        let mut bob = Peer { login: "bob".into(), joined: true, ..Default::default() };
        bob.uses.insert("w1".into(), HashMap::from([("get_user".to_string(), now() - 7200)])); // two hours ago: still counts here
        bob.promises.push(Promise { session: "w2".into(), name: "rate".into(), text: "a fraction".into(), ts: 0 });
        st.peers.insert("bob".into(), bob);
        let b = super::contract_breaks(&st, &gone, &changed);
        assert_eq!(b.len(), 2, "{b:?}");
        assert!(b.iter().all(|x| x.0 == "bob"));
        assert!(b.iter().any(|x| x.1.contains("get_user is removed") && x.1.contains("bob's code") && x.2.contains("your code")));
        assert!(b.iter().any(|x| x.1.contains("rate changed") && x.1.contains("a fraction")));
        assert!(super::contract_breaks(&st, &[], &["other".into()]).is_empty());
        assert!(crate::review::is_commit("git add . && git -C x commit -m hi") && !crate::review::is_commit("git log"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_team_in_the_background_shows_how_to_open_it() {
        assert_eq!(super::background_hint("", &["kesav/shop".into()]).as_deref(), Some("team shop · Ctrl+] to open"));
        assert_eq!(super::background_hint("kesav/shop", &["kesav/shop".into()]), None, "on screen: the full line shows instead");
        assert_eq!(super::background_hint("", &[]), None);
    }

    use super::*;

    #[test]
    fn an_admin_restarts_a_team_whose_key_holders_are_long_gone() {
        let repo = "rec/orphan";
        let mut st = state("kesav");
        st.repo = repo.into();
        let old = now() - ORPHAN_AFTER - 60;
        st.peers.insert("dave".into(), Peer { login: "dave".into(), locked: true, ts: old, epoch: 2, ..Default::default() });
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), st);
        with(repo, |s| s.admins.insert("kesav".into(), false));
        settle_key(repo, &AtomicBool::new(false));
        assert!(with(repo, |s| s.key.is_none()).unwrap(), "only an admin may start over");
        with(repo, |s| { s.admins.insert("kesav".into(), true); s.peers.get_mut("dave").unwrap().ts = now() });
        settle_key(repo, &AtomicBool::new(false));
        assert!(with(repo, |s| s.key.is_none()).unwrap(), "dave wrote lately: wait for him to let us in");
        with(repo, |s| s.peers.get_mut("dave").unwrap().ts = old);
        settle_key(repo, &AtomicBool::new(false));
        let (key, epoch, approved) = with(repo, |s| (s.key.is_some(), s.epoch, s.approved.clone())).unwrap();
        assert!(key && epoch == 3 && approved.contains(&"dave".to_string()), "a new generation, and dave gets it when he's back");
    }

    #[test]
    fn requests_in_flight_survive_a_restart() {
        // the bug: the outbox lived in memory only, so restarting before an offline teammate read a
        // request dropped it; and accepting a later request acked earlier ones that still waited
        let repo = format!("req/persist{}", std::process::id());
        let mut s = state("alice");
        s.repo = repo.clone();
        s.outbox = vec![json!({"id": 7, "to": "bob", "session": "main", "text": "build it", "ts": 100})];
        s.pending = vec![Incoming { from: "bob".into(), id: 1, session: "main".into(), text: "T1".into(), ts: 50, kind: "handover".into(),
                                    history: vec![(Status::Delivered, 60), (Status::Seen, 70)], snoozed: true, ..Default::default() }];
        s.seen.insert("bob".into(), 2); // T2 was accepted: T1 is below the ack line and must not be lost
        s.sent = vec![Outgoing { id: 7, to: "bob".into(), session: "main".into(), text: "build it".into(), kind: "handover".into(),
                                 history: vec![(Status::Queued, 100), (Status::Delivered, 110)], reply: "on it".into(), ..Default::default() }];
        push_fyi(&mut s, "answer", "bob answered: 8080".into());
        save_requests(&s);
        let back = load_requests(&repo);
        let _ = std::fs::remove_file(per_repo("team-requests", &repo));
        assert_eq!(back.outbox, s.outbox);
        assert_eq!(back.pending.len(), 1);
        let p = &back.pending[0];
        assert_eq!((p.id, p.text.as_str(), p.arrived(), p.seen(), p.snoozed, p.kind.as_str()), (1, "T1", 60, true, true, "handover"));
        assert_eq!(back.sent[0].status(), Status::Delivered);
        assert_eq!(back.sent[0].reply, "on it");
        assert_eq!(back.fyi[0].text, "bob answered: 8080");
        assert!(back.ts > 0, "when we were last open");
    }

    #[test]
    fn the_sender_hears_each_step_of_a_request() {
        let (ra, rb) = (format!("req/steps-a{}", std::process::id()), format!("req/steps-b{}", std::process::id()));
        let mut alice = state("alice");
        alice.repo = ra.clone();
        let mut bob = state("bob");
        bob.repo = rb.clone();
        bob.next_id = 4;
        bob.outbox.push(json!({"id": 5, "to": "alice", "session": "main", "text": "build the backend", "ts": now()}));
        bob.sent.push(Outgoing { id: 5, to: "alice".into(), session: "main".into(), text: "build the backend".into(),
                                 history: vec![(Status::Queued, now())], ..Default::default() });
        let bob_out = bob.outbox.clone();
        { let mut g = STATES.lock().unwrap(); let m = g.get_or_insert_with(HashMap::new); m.insert(ra.clone(), alice); m.insert(rb.clone(), bob); }
        let events = std::sync::Arc::new(Mutex::new(vec![]));
        let ev = events.clone();
        let hooks = Hooks { changed: Box::new(move |e| ev.lock().unwrap().push(format!("{e:?}"))), ..quiet() };
        let mut nc = false;
        // alice's qh takes it in: it waits, and bob will hear it was delivered
        let mut p = peer("bob", &["main"], true);
        p.outbox = bob_out;
        take_in(&ra, &"alice".to_string(), None, p, &hooks, &mut nc);
        with(&ra, |s| {
            assert_eq!(s.pending.len(), 1);
            mark_seen_locked(s, "bob", 5);
            mark_seen_locked(s, "bob", 5); // drawn again: still one "seen"
            reply_request_locked(s, "bob", 5, "which framework?").unwrap();
            decide_locked(s, "bob", 5, "once").unwrap();
        });
        let alice_out = with(&ra, |s| s.outbox.clone()).unwrap();
        // bob's qh reads alice's folder: every step lands on his request, in order
        let mut p = peer("alice", &["main"], true);
        p.outbox = alice_out.clone();
        take_in(&rb, &"bob".to_string(), None, p, &hooks, &mut nc);
        let sent = with(&rb, |s| s.sent[0].clone()).unwrap();
        let steps: Vec<&str> = sent.history.iter().map(|h| h.0.as_str()).collect();
        assert_eq!(steps, ["queued", "delivered", "seen", "replied", "accepted"]);
        assert_eq!(sent.reply, "which framework?");
        // the same folder again changes nothing
        let mut p = peer("alice", &["main"], true);
        p.outbox = alice_out;
        take_in(&rb, &"bob".to_string(), None, p, &hooks, &mut nc);
        assert_eq!(with(&rb, |s| s.sent[0].history.len()).unwrap(), 5);
        let ev = events.lock().unwrap().clone();
        assert!(ev[0].starts_with("Arrived"), "{ev:?}");
        assert_eq!(ev.iter().filter(|e| e.starts_with("Outgoing")).count(), 4, "{ev:?}");
        let alerts = take_alerts();
        assert!(alerts.iter().any(|a| a == "qh: alice replied to \"build the backend\": which framework?"), "{alerts:?}");
        assert!(alerts.iter().any(|a| a == "qh: alice accepted \"build the backend\""), "{alerts:?}");
        let mut g = STATES.lock().unwrap();
        let m = g.as_mut().unwrap();
        m.remove(&ra);
        m.remove(&rb);
    }

    #[test]
    fn clashes_name_each_person_once() {
        let h = |from: &str, id, files: &[&str]| Incoming { from: from.into(), id, packet: json!({"files": files}), ..Default::default() };
        let mut s = state("alice");
        s.pending = vec![h("bob", 1, &["web.py"]), h("carol", 2, &["other.py"])];
        s.flow.taken = vec![h("bob", 3, &["web.py", "api.py"])];
        let mine = ["web.py".to_string(), "api.py".to_string()];
        assert_eq!(clashes_in(&s, &h("dan", 9, &[]), &mine), [("bob".to_string(), vec!["web.py".to_string(), "api.py".to_string()])]);
    }

    #[test]
    fn a_handover_packet_arrives_is_kept_on_accept_and_clashes_are_found() {
        let (ra, rb) = (format!("req/pk-a{}", std::process::id()), format!("req/pk-b{}", std::process::id()));
        let mut alice = state("alice");
        alice.repo = ra.clone();
        let pk = |files: &[&str]| json!({"done": ["button"], "left": ["partial amount", "tests"], "files": files, "watch": "pence", "branch": "sam/refund"});
        let mut bob = state("bob");
        bob.repo = rb.clone();
        bob.outbox.push(json!({"id": 5, "to": "alice", "session": "main", "text": "refund button", "ts": now(), "packet": pk(&["checkout.tsx", "api.ts"])}));
        let mut carol = state("carol");
        carol.outbox.push(json!({"id": 7, "to": "alice", "session": "main", "text": "error banner", "ts": now(), "packet": pk(&["checkout.tsx"])}));
        let (bob_out, carol_out) = (bob.outbox.clone(), carol.outbox.clone());
        { let mut g = STATES.lock().unwrap(); let m = g.get_or_insert_with(HashMap::new); m.insert(ra.clone(), alice); m.insert(rb.clone(), bob); }
        let hooks = quiet();
        let mut nc = false;
        for (who, out) in [("bob", bob_out), ("carol", carol_out)] {
            let mut p = peer(who, &["main"], true);
            p.outbox = out;
            take_in(&ra, &"alice".to_string(), None, p, &hooks, &mut nc);
        }
        with(&ra, |s| {
            assert_eq!(s.pending.len(), 2);
            assert_eq!(s.pending[0].packet["left"][0], "partial amount");
            assert_eq!(s.pending[0].packet["branch"], "sam/refund");
            decide_locked(s, "bob", 5, "once").unwrap();
            assert_eq!(s.flow.taken.len(), 1, "an accepted handover stays in Taken over");
            assert!(s.flow.taken[0].history.iter().any(|h| h.0 == Status::Accepted));
        });
        // carol's waiting handover touches checkout.tsx, as bob's taken one does
        let carol_inc = with(&ra, |s| s.pending[0].clone()).unwrap();
        let mine = packet_files(&carol_inc);
        let others: Vec<(String, Vec<String>)> = with(&ra, |s| s.flow.taken.iter().filter_map(|x| {
            let both: Vec<String> = packet_files(x).into_iter().filter(|f| mine.contains(f)).collect();
            (!both.is_empty()).then(|| (x.from.clone(), both))
        }).collect()).unwrap();
        assert_eq!(others, [("bob".to_string(), vec!["checkout.tsx".to_string()])]);
        let mut g = STATES.lock().unwrap();
        let m = g.as_mut().unwrap();
        m.remove(&ra);
        m.remove(&rb);
    }

    #[test]
    fn later_with_a_time_tells_the_sender_and_comes_back_by_itself() {
        let mut s = state("alice");
        s.pending = vec![Incoming { from: "bob".into(), id: 3, text: "job".into(), kind: "handover".into(),
                                    history: vec![(Status::Delivered, now())], ..Default::default() }];
        let i = s.pending.first_mut().unwrap();
        i.snoozed = true;
        i.until = now() + 3600;
        status_out(&mut s, "bob", 3, Status::Seen, &format!("seen, back to it {}", clock(now() + 3600)));
        wake_locked(&mut s);
        assert!(s.pending[0].snoozed, "not yet");
        s.pending[0].until = now() - 1;
        wake_locked(&mut s);
        assert!(!s.pending[0].snoozed && s.pending[0].until == 0, "back on the card");
        let m = s.outbox.last().unwrap();
        assert_eq!((m["status"].as_str(), m["text"].as_str().map(|t| t.starts_with("seen, back to it "))), (Some("seen"), Some(true)));
        // the sender's line shows it even though "seen" came before
        let mut bob = state("bob");
        bob.sent.push(Outgoing { id: 3, to: "alice".into(), text: "job".into(), history: vec![(Status::Queued, now()), (Status::Seen, now())], ..Default::default() });
        let repo = format!("req/later{}", std::process::id());
        bob.repo = repo.clone();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), bob);
        let mut p = peer("alice", &["main"], true);
        p.outbox = vec![m.clone()];
        take_in(&repo, &"bob".to_string(), None, p, &quiet(), &mut false);
        assert!(with(&repo, |s| s.sent[0].reply.starts_with("seen, back to it")).unwrap());
        // once they accept, the "back to it" news is over
        let mut p = peer("alice", &["main"], true);
        p.outbox = vec![json!({"id": m["id"].as_u64().unwrap() + 1, "to": "bob", "kind": "s", "re": 3, "status": "accepted", "text": "", "ts": now()})];
        take_in(&repo, &"bob".to_string(), None, p, &quiet(), &mut false);
        assert!(with(&repo, |s| s.sent[0].status() == Status::Accepted && s.sent[0].reply.is_empty()).unwrap());
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
    }

    #[test]
    fn promises_go_alone_only_to_the_trusted_on_their_topics() {
        let repo = format!("req/trust{}", std::process::id());
        let mut s = state("alice");
        s.repo = repo.clone();
        s.roles.insert("sam".into(), "trusted".into());
        s.flow.topics.insert("sam".into(), vec!["refund".into()]);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), s);
        let q = |id, from: &str, text: &str| Question { repo: repo.clone(), from: from.into(), from_session: String::new(), id, session: "main".into(), text: text.into() };
        // a fact: goes to anyone, logged
        reply(&q(1, "ravi", "which field?"), "currency", &[], false);
        // a promise to the trusted, on their topic: goes, logged as a promise
        reply(&q(2, "sam", "are refunds idempotent?"), "yes, by refund id", &["refund_id".into()], true);
        // a promise off their topic, and one to someone untrusted: held with the answer ready
        reply(&q(3, "sam", "payout schedule for EU?"), "weekly", &["payout_day".into()], true);
        reply(&q(4, "ravi", "is total in cents?"), "yes", &["total".into()], true);
        // a fact that names something, to someone untrusted: goes, and stays a fact
        reply(&q(5, "ravi", "what does greet return?"), "a string", &["greet".into()], false);
        // a promise that names nothing still waits for you
        reply(&q(6, "ravi", "when is it done?"), "by Friday", &[], true);
        with(&repo, |s| {
            let sent: Vec<u64> = s.outbox.iter().filter(|m| m["kind"] == "a").filter_map(|m| m["re"].as_u64()).collect();
            assert_eq!(sent, [1, 2, 5]);
            assert_eq!(s.flow.log.iter().map(|a| (a.to.as_str(), a.promise)).collect::<Vec<_>>(), [("ravi", false), ("sam", true), ("ravi", false)]);
            assert_eq!(s.pending.iter().map(|i| i.id).collect::<Vec<_>>(), [3, 4, 6]);
            assert!(s.pending[2].packet["held"].as_str().unwrap().contains("it promises something"));
            assert_eq!(s.pending[0].draft.as_ref().unwrap().0, "weekly");
            assert!(s.pending[0].packet["held"].as_str().unwrap().contains("isn't trusted on this topic"));
        });
        // a question you said yes to goes whatever it promises, and isn't logged as alone
        with(&repo, |s| s.flow.approved.push(("ravi".into(), 9)));
        reply(&q(9, "ravi", "port?"), "8080", &["PORT".into()], true);
        with(&repo, |s| {
            assert!(s.outbox.iter().any(|m| m["kind"] == "a" && m["re"] == 9));
            assert_eq!(s.flow.log.len(), 3);
        });
        // stop trusting them and their topics go too: trusted again, it's on anything until you pick
        set_role(&repo, "sam", "member").unwrap();
        assert!(with(&repo, |s| !s.flow.topics.contains_key("sam")).unwrap(), "topics belong to the trust");
        set_role(&repo, "sam", "trusted").unwrap();
        reply(&q(7, "sam", "payout schedule for EU?"), "weekly", &["payout_day".into()], true);
        assert!(with(&repo, |s| s.outbox.iter().any(|m| m["kind"] == "a" && m["re"] == 7)).unwrap(), "no topics: on anything");
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
    }

    /// A direct-line update carries no version: losing it made a 0.2.6 teammate look old, and
    /// their status news was dropped.
    /// Taking back an answer: an older qh gets a DM (it would show the new kind as work), a 0.2.7
    /// asker's agent is told and the contract the answer made ends.
    #[test]
    fn a_take_back_reaches_the_agent_that_asked() {
        let a = Answered { ts: 1, to: "bob".into(), question: "is it stable?".into(), answer: "yes".into(), promise: true, taken_back: true,
                           session: "main".into(), relied: vec!["greet".into()] };
        let mut s = state("alice");
        let mut old = peer("bob", &["main"], true);
        old.qh = "0.2.6".into();
        s.peers.insert("bob".into(), old);
        assert!(!correct_out(&mut s, &a), "0.2.6 doesn't know it");
        assert!(s.outbox.is_empty());
        s.peers.get_mut("bob").unwrap().qh = "0.2.7".into();
        assert!(correct_out(&mut s, &a));
        assert_eq!(s.outbox[0]["kind"], "c");
        assert_eq!(answered_of(&answered_json(&a)).relied, a.relied, "kept over a restart");

        let repo = format!("req/back{}", std::process::id());
        let mut b = state("bob");
        b.repo = repo.clone();
        b.uses.entry("main".into()).or_default().extend([("greet".to_string(), 1), ("other".to_string(), 1)]);
        b.last_answer.insert("main".into(), ("alice/x".into(), vec!["greet".into()], "Q: is it stable? A: yes".into()));
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), b);
        let told = std::sync::Arc::new(Mutex::new(vec![]));
        let t2 = told.clone();
        let hooks = Hooks { answered: Box::new(move |s, t| t2.lock().unwrap().push((s.to_string(), t.to_string()))), ..quiet() };
        let mut alice = peer("alice", &["x"], true);
        alice.outbox = s.outbox.clone();
        take_in(&repo, &"bob".to_string(), None, alice, &hooks, &mut false);
        let told = told.lock().unwrap();
        assert_eq!(told[0].0, "main");
        assert!(told[0].1.contains("took back") && told[0].1.contains("is it stable?") && told[0].1.contains("yes"), "{}", told[0].1);
        with(&repo, |b| {
            assert!(!b.uses["main"].contains_key("greet"), "the contract ended");
            assert!(b.uses["main"].contains_key("other"), "others stay");
            assert!(!b.last_answer.contains_key("main"));
            assert!(b.pending.is_empty(), "not shown as work");
        });
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
    }

    #[test]
    fn the_outbox_and_chat_stay_bounded() {
        let mut s = state("alice");
        s.outbox.push(json!({"id": 1, "to": "bob", "kind": "q", "text": "oldest question"}));
        for id in 2..=(OUTBOX_MAX as u64 + 10) { s.outbox.push(json!({"id": id, "to": "bob", "kind": "s"})) }
        trim_outbox(&mut s);
        assert_eq!(s.outbox.len(), OUTBOX_MAX);
        assert_eq!(s.outbox[0]["kind"], "q", "news goes before a request");
        s.outbox = (0..OUTBOX_MAX as u64 + 5).map(|id| json!({"id": id, "kind": "q"})).collect();
        trim_outbox(&mut s);
        assert_eq!((s.outbox.len(), s.outbox[0]["id"].as_u64()), (OUTBOX_MAX, Some(5)));
        for i in 0..CHAT_MAX as u64 + 3 { insert_chat(&mut s, i, "bob", "x") }
        assert_eq!((s.chat.len(), s.chat[0].0), (CHAT_MAX, 3));
    }

    #[test]
    fn a_live_update_keeps_their_version() {
        let repo = format!("req/ver{}", std::process::id());
        let k: Key = [7; 32];
        let mut s = state("alice");
        s.repo = repo.clone();
        s.key = Some(k);
        let mut bob = peer("bob", &["main"], true);
        (bob.pub_key, bob.qh, bob.seq) = ("bobkey".into(), "0.2.6".into(), 100);
        s.peers.insert("bob".into(), bob);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), s);
        let live = json!({"login": "bob", "seq": 200, "data": tc::seal(&k, "data:bob", b"{}")});
        take_live(&repo, Path::new("."), "bobkey", live.to_string().as_bytes(), &quiet(), &mut Live { last: None, at: Instant::now(), pulse: 0, told: vec![], bytes: 0, failed: 0, watchers: vec![], asked: None });
        let told = |repo: &str| with(repo, |s| {
            assert_eq!(s.peers["bob"].seq, 200, "the live copy landed");
            assert!(knows_status(s, "bob"), "still 0.2.6");
            status_about(s, "bob", 1, Status::Delivered, "", "job");
            s.outbox.iter().any(|m| m["kind"] == "s" && m["status"] == "delivered")
        }).unwrap();
        assert!(told(&repo));
        // an older git copy keeps the live data but brings the version along
        let mut git = peer("bob", &["main"], true);
        (git.pub_key, git.qh, git.seq) = ("bobkey".into(), "0.2.7".into(), 150);
        take_in(&repo, &"alice".to_string(), Some(k), git, &quiet(), &mut false);
        with(&repo, |s| assert_eq!((s.peers["bob"].seq, s.peers["bob"].qh.as_str()), (200, "0.2.7")));
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
    }

    #[test]
    fn focus_ending_moves_the_generation_so_an_idle_screen_redraws() {
        let mut st = state("alice");
        st.flow.focus = now() - 1;
        let before = generation();
        wake_locked(&mut st);
        assert!(generation() > before);
        assert!(st.flow.focus_told);
    }

    #[test]
    fn expiry_shows_from_day_twelve() {
        let o = |age: u64| Outgoing { id: 1, ts: now() - age, history: vec![(Status::Delivered, now() - age)], ..Default::default() };
        assert_eq!(expires_in(&o(11 * 86400)), None);
        assert_eq!(expires_in(&o(12 * 86400)), Some(2));
        assert_eq!(expires_in(&o(13 * 86400 + 100)), Some(1));
        let mut done = o(13 * 86400);
        done.history.push((Status::Accepted, now()));
        assert_eq!(expires_in(&done), None);
        // past two weeks, still open: nothing here ends it, so don't count down to it
        let old = o(14 * 86400 + 100);
        assert_eq!(expires_in(&old), Some(0));
        assert_eq!(crate::teamview::outgoing_side(&old), "two weeks old · still open on their side");
        assert!(crate::teamview::outgoing_side(&o(12 * 86400)).starts_with("expires in 2d"));
    }

    #[test]
    fn clock_says_when_in_words_whichever_way() {
        let at = |d: i64| clock((now() as i64 + d * 86400) as u64);
        assert!(at(0).len() == 5 && at(0).as_bytes()[2] == b':', "{}", at(0));
        assert!(at(1).starts_with("tomorrow "), "{}", at(1));
        assert!(at(-1).starts_with("yesterday "), "{}", at(-1));
        assert!(at(-3).starts_with("3d ago "), "{}", at(-3));
        assert!(at(5).starts_with("in 5d "), "{}", at(5));
    }

    #[test]
    fn clock_says_today_or_tomorrow() {
        let t = next_at(9, 0);
        assert!(clock(t).ends_with("09:00"), "{}", clock(t));
        assert!(t > now() && t <= now() + 86400);
        assert!(later_choices().last().is_some_and(|c| c.1 == 0));
    }

    #[test]
    fn a_withdrawn_request_leaves_their_list_or_never_reaches_it() {
        let (ra, rb) = (format!("req/wd-a{}", std::process::id()), format!("req/wd-b{}", std::process::id()));
        let mut bob = state("bob");
        bob.repo = rb.clone();
        bob.next_id = 10;
        for id in [5, 6] {
            bob.outbox.push(json!({"id": id, "to": "alice", "session": "main", "text": format!("job {id}"), "ts": now()}));
            bob.sent.push(Outgoing { id, to: "alice".into(), history: vec![(Status::Queued, now())], ..Default::default() });
        }
        let mut alice = state("alice");
        alice.repo = ra.clone();
        { let mut g = STATES.lock().unwrap(); let m = g.get_or_insert_with(HashMap::new); m.insert(ra.clone(), alice); m.insert(rb.clone(), bob); }
        let mut nc = false;
        // alice has seen job 5 only
        let mut p = peer("bob", &["main"], true);
        p.outbox = with(&rb, |s| s.outbox[..1].to_vec()).unwrap();
        take_in(&ra, &"alice".to_string(), None, p, &quiet(), &mut nc);
        with(&rb, |s| {
            withdraw_locked(s, 6).unwrap();    // never reached her: dropped (the notice is a no-op)
            assert!(!s.outbox.iter().any(|m| m["id"] == 6));
            withdraw_locked(s, 5).unwrap();    // she has it, unacked while it waits: she's still told
            assert!(!s.outbox.iter().any(|m| m["id"] == 5));
            assert!(s.outbox.iter().any(|m| m["kind"] == "w" && m["re"] == 5));
            assert!(withdraw_locked(s, 5).unwrap_err().contains("already withdrawn"));
        });
        let mut p = peer("bob", &["main"], true);
        p.outbox = with(&rb, |s| s.outbox.clone()).unwrap();
        take_in(&ra, &"alice".to_string(), None, p, &quiet(), &mut nc);
        assert!(with(&ra, |s| s.pending.is_empty()).unwrap(), "job 5 left her list");
        let mut g = STATES.lock().unwrap();
        let m = g.as_mut().unwrap();
        m.remove(&ra);
        m.remove(&rb);
    }

    #[test]
    fn a_request_nobody_acts_on_expires_and_the_sender_is_told() {
        let mut s = state("alice");
        s.pending = vec![Incoming { from: "bob".into(), id: 3, text: "old".into(), history: vec![(Status::Delivered, now() - REQUEST_LIFE - 10)], ..Default::default() },
                         Incoming { from: "bob".into(), id: 4, text: "new".into(), history: vec![(Status::Delivered, now())], ..Default::default() }];
        assert_eq!(expire_locked(&mut s), [("bob".to_string(), 3)]);
        assert_eq!(s.pending.len(), 1);
        let m = s.outbox.last().unwrap();
        assert_eq!((m["re"].as_u64(), m["status"].as_str()), (Some(3), Some("expired")));
    }

    #[test]
    fn one_qh_holds_a_team_and_the_projects_own_window_takes_it_from_a_background_one() {
        let _env = crate::hub::tests::ENV.lock().unwrap_or_else(|e| e.into_inner()); // the lock files live in the state dir
        let repo = format!("lock/t{}", std::process::id());
        hold_connection(&repo, true).unwrap();
        assert!(hold_connection(&repo, true).unwrap_err().contains("connected in another qh"), "a second background one stays out");
        let r2 = repo.clone();
        let fg = std::thread::spawn(move || hold_connection_as(&r2, false, false));
        let t0 = Instant::now();
        while !wanted_elsewhere(&repo) && t0.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(20)) }
        assert!(wanted_elsewhere(&repo), "the background holder is asked to give it up");
        release(&repo); // what its sync loop does when it sees that
        fg.join().unwrap().unwrap();
        assert!(!wanted_elsewhere(&repo) && std::fs::read_to_string(per_repo("team-owner", &repo)).unwrap().ends_with(" solo"));
        // plain qh holds it now; its notice says open `qh team`, and that window takes it over
        let r3 = repo.clone();
        let tw = std::thread::spawn(move || hold_connection_as(&r3, false, true));
        let t0 = Instant::now();
        while !wanted_elsewhere(&repo) && t0.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(20)) }
        assert!(wanted_elsewhere(&repo), "plain qh is asked to give it to `qh team`");
        release(&repo);
        tw.join().unwrap().unwrap();
        assert!(std::fs::read_to_string(per_repo("team-owner", &repo)).unwrap().ends_with(" project"));
        assert!(!connection_free(&repo), "plain qh waits while `qh team` is open");
        release(&repo);
        // a git child another test forks can hold the lock until it execs: give it a moment
        let t0 = Instant::now();
        while !connection_free(&repo) && t0.elapsed() < Duration::from_secs(3) { std::thread::sleep(Duration::from_millis(50)) }
        assert!(connection_free(&repo), "and connects again once it closes");
    }

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
/// A team for the screen tests: two handovers that clash on checkout.tsx, a question held
    /// because it promises something, one handover taken, answers sent alone, requests about to
    /// expire, a trusted teammate on one topic who is focusing.
    pub(crate) fn install_flow_demo(repo: &str) {
        let mut s = state("kesav");
        s.repo = repo.into();
        s.key = Some(tc::new_team_key());
        s.running = std::sync::Arc::new(AtomicBool::new(true));
        let t = now();
        for (login, online) in [("sam", true), ("priya", true), ("ravi", false)] {
            let mut p = Peer { login: login.into(), online, joined: true, ts: t, ..Default::default() };
            p.sessions = vec![("ui".into(), "working".into(), String::new())];
            if login == "sam" { p.focus = t + 3600 }
            s.peers.insert(login.into(), p);
        }
        s.roles.insert("sam".into(), "trusted".into());
        s.flow.topics.insert("sam".into(), vec!["refunds".into()]);
        let pk = |files: &[&str], left: &[&str]| json!({"done": ["button + confirm dialog", "calls POST /refunds (full only)"], "left": left,
            "files": files, "watch": "amount is in minor units (pence)", "branch": "sam/refund-btn"});
        let inc = |from: &str, id, text: &str, packet: Value| Incoming { from: from.into(), id, session: "main".into(), text: text.into(),
            ts: t - 600, kind: "handover".into(), history: vec![(Status::Delivered, t - 600)], packet, ..Default::default() };
        s.pending.push(inc("priya", 7, "[from priya's agent] api error banner on checkout", pk(&["checkout.tsx"], &["banner copy"])));
        let q = Question { repo: repo.into(), from: "ravi".into(), from_session: String::new(), id: 9, session: "main".into(), text: "is total in cents?".into() };
        s.pending.push(Incoming { from: "ravi".into(), id: 9, session: "main".into(), text: "question: is total in cents?".into(), question: Some(q),
            kind: "question".into(), ts: t - 60, history: vec![(Status::Delivered, t - 60)], draft: Some(("yes, always cents".into(), vec!["total".into()])),
            packet: json!({"held": "held for you: it promises total, and ravi isn't trusted on this topic"}), ..Default::default() });
        let mut took = inc("sam", 5, "[from sam's agent] refund button wiring", pk(&["checkout.tsx", "RefundButton.tsx", "api/refunds.ts"],
                                                                                   &["partial amount field", "error state when refund fails", "tests in checkout.test.tsx"]));
        took.history.push((Status::Accepted, t - 300));
        s.flow.taken.push(took);
        s.flow.log = vec![
            Answered { ts: t - 7200, to: "sam".into(), question: "which currency field does refund.created use?".into(), answer: "currency, ISO 4217".into(), ..Default::default() },
            Answered { ts: t - 3600, to: "sam".into(), question: "can I rely on refunds being idempotent?".into(), answer: "yes, by refund id".into(), promise: true, ..Default::default() },
        ];
        s.sent = vec![
            Outgoing { id: 1, to: "sam".into(), session: "ui".into(), text: "handover: checkout tests".into(), kind: "handover".into(), ts: t - 12 * 86400,
                       history: vec![(Status::Queued, t - 12 * 86400), (Status::Delivered, t - 12 * 86400)], ..Default::default() },
            Outgoing { id: 2, to: "ravi".into(), session: "main".into(), text: "question: payout cutoff?".into(), kind: "question".into(), ts: t - 14 * 86400,
                       history: vec![(Status::Delivered, t - 14 * 86400), (Status::Expired, t - 3600)], ..Default::default() },
        ];
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), s);
        set_current(repo);
    }

    pub(crate) fn add_outgoing(repo: &str, o: Outgoing) {
        with(repo, |s| s.sent.push(o));
    }

    pub(crate) fn drop_demo(repo: &str) {
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    fn state(me: &str) -> State {
        State {
            repo: "o/r".into(), me: me.into(), peers: HashMap::new(), error: String::new(), key: None, outbox: vec![], next_id: 0, sent: vec![], fyi: vec![], last_open: 0, away_until: 0,
            seen: HashMap::new(), pending: vec![], roles: HashMap::new(), approved: vec![], asking: vec![], invites: HashMap::new(),
            epoch: 0, removed: vec![], admins: HashMap::new(), pins: HashMap::new(), gh_login: me.into(), git_ms: HashMap::new(),
            asked_github: HashMap::new(), warned: vec![], my_sha: String::new(),
            chat: vec![], my_chat: vec![], chat_seen: HashMap::new(), my_notes: vec![], notes_seen: HashMap::new(), claims: HashMap::new(), unmerged: HashMap::new(),
            uses: HashMap::new(), waiting: vec![], sheets: HashMap::new(), asked: vec![], answered: vec![], answers: "auto".into(),
            answerer: None, promises: vec![], last_answer: HashMap::new(), unread: 0, dirty: false, flow: Flow::default(), running: Default::default(), finished: Default::default(), watching: None, watch_now: false,
            secret: None, my_dms: vec![], my_groups: vec![], my_gmsgs: vec![], groups: HashMap::new(), msgs: vec![],
            msg_ids: Default::default(), chan_unread: HashMap::new(), cleared: HashMap::new(), pinged: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_git_call_is_stopped_with_its_helpers() {
        let mark = std::env::temp_dir().join(format!("qh-limit-{}", std::process::id()));
        let _ = std::fs::remove_file(&mark);
        let mut c = Command::new("sh");
        c.args(["-c", &format!("(sleep 2; touch {}) & wait", mark.display())]);
        let t0 = Instant::now();
        assert!(run_limited(c, Duration::from_millis(200), None).is_err(), "stopped counts as failed");
        assert!(t0.elapsed() < Duration::from_secs(1), "it took {:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!mark.exists(), "the helper it started was stopped too");
        let mut ok = Command::new("sh");
        ok.args(["-c", "exit 0"]);
        assert!(run_limited(ok, Duration::from_secs(5), None).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn a_stalled_git_call_is_stopped_with_its_helpers_on_windows() {
        let base = std::env::temp_dir().join(format!("qh-limit-win-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (mark, inner) = (base.join("mark"), base.join("inner.cmd"));
        std::fs::write(&inner, format!("ping -n 4 127.0.0.1 >NUL\r\necho x> \"{}\"\r\n", mark.display())).unwrap();
        let mut c = Command::new("cmd");
        c.args(["/C", "cmd", "/C"]).arg(&inner);
        let t0 = Instant::now();
        assert!(run_limited(c, Duration::from_millis(200), None).is_err(), "stopped counts as failed");
        assert!(t0.elapsed() < Duration::from_secs(5), "it took {:?}", t0.elapsed());
        std::thread::sleep(Duration::from_secs(4));
        assert!(!mark.exists(), "the inner cmd was stopped too");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_flag_ends_a_stalled_call_at_once() {
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let f2 = flag.clone();
        std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(200)); f2.store(true, Relaxed) });
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30"]);
        let t0 = Instant::now();
        assert!(run_limited(c, Duration::from_secs(60), Some(&flag)).is_err(), "stopped counts as failed");
        assert!(t0.elapsed() < Duration::from_secs(1), "it took {:?}", t0.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_flag_already_set_starts_nothing() {
        let mark = std::env::temp_dir().join(format!("qh-preset-{}", std::process::id()));
        let _ = std::fs::remove_file(&mark);
        let mut c = Command::new("sh");
        c.args(["-c", &format!("touch {}", mark.display())]);
        assert!(run_limited(c, Duration::from_secs(5), Some(&AtomicBool::new(true))).is_err());
        std::thread::sleep(Duration::from_millis(200));
        assert!(!mark.exists(), "it never ran");
        // with no flag to look at, the same call goes ahead
        let mut c = Command::new("sh");
        c.args(["-c", &format!("touch {}", mark.display())]);
        assert!(run_limited(c, Duration::from_secs(5), None).is_ok());
        assert!(mark.exists());
        let _ = std::fs::remove_file(&mark);
    }

    /// git removes its lockfiles when told to stop politely (seen: SIGTERM clears a held ref lock,
    /// SIGKILL leaves it), and whatever ignores that is stopped anyway.
    #[cfg(unix)]
    #[test]
    fn a_stalled_call_is_asked_first_and_then_made_to_stop() {
        let base = std::env::temp_dir().join(format!("qh-term-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let (cleaned, late) = (base.join("cleaned"), base.join("late"));
        let mut c = Command::new("sh");
        c.args(["-c", &format!("trap 'touch {}; exit 0' TERM; (trap '' TERM; sleep 2; touch {}) & wait", cleaned.display(), late.display())]);
        let t0 = Instant::now();
        assert!(run_limited(c, Duration::from_millis(200), None).is_err());
        assert!(t0.elapsed() < Duration::from_secs(2), "it took {:?}", t0.elapsed());
        assert!(cleaned.exists(), "it had the chance to clean up");
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!late.exists(), "the helper that ignores being asked was stopped too");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A remote that never answers, and how to tell whether what it started is still alive.
    #[cfg(unix)]
    fn stalled_remote(base: &Path, name: &str) -> (String, PathBuf) {
        let (script, pid) = (base.join(format!("{name}.sh")), base.join(format!("{name}.pid")));
        std::fs::write(&script, format!("#!/bin/sh\necho $$ > {}\nexec sleep 300\n", pid.display())).unwrap();
        (format!("ext::sh {}", script.display()), pid)
    }

    #[cfg(unix)]
    fn gone(pid_file: &Path) {
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap();
        let t0 = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0 && t0.elapsed() < Duration::from_secs(2) { std::thread::sleep(Duration::from_millis(20)) }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "{} is still running", pid_file.display());
    }

    fn file_url(p: &Path) -> String {
        let path = p.display().to_string().replace('\\', "/");
        if path.starts_with('/') { format!("file://{path}") } else { format!("file:///{path}") }
    }

    /// A bare repository to fetch from and push to. Registered like a store, so it works whatever
    /// the machine's git says about bare repositories.
    fn scratch_remote(base: &Path) -> PathBuf {
        let remote = base.join("remote.git");
        assert_eq!(board_store_in(&remote, base), remote);
        remote
    }

    fn scratch_store(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("qh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let store = base.join("store");
        assert_eq!(board_store_in(&store, &base.join("project")), store);
        (base, store)
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_remote_cant_hold_up_quitting() {
        let (base, store) = scratch_store("quit");
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let (stalled_fetch, fetch_pid) = stalled_remote(&base, "fetch");
        let (stalled_push, push_pid) = stalled_remote(&base, "push");
        let env = [("GIT_ALLOW_PROTOCOL", "ext".to_string())];
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (stop2, store2, env2) = (stop.clone(), store.clone(), env.clone());
        let reader = std::thread::spawn(move || {
            let _ = fetch_refs(&store2, &stalled_fetch, &env2, Duration::from_secs(30), Some(&*stop2));
        });
        let t0 = Instant::now();
        while !fetch_pid.exists() && t0.elapsed() < Duration::from_secs(5) { std::thread::sleep(Duration::from_millis(20)) }
        assert!(fetch_pid.exists(), "the stalled fetch started");
        let fetched = AtomicBool::new(true);
        let msg = json!({"state": "offline"});
        let t0 = Instant::now();
        stop.store(true, Relaxed);
        leave_board(reader, &fetched, || vec![("pub".to_string(), "a".to_string())], |files| {
            let r = last_write(&store, &stalled_push, "alice", &env, (&files, &msg, &idc), t0);
            assert!(r.is_err(), "nothing can have landed");
        });
        assert!(t0.elapsed() < Duration::from_secs(5), "quitting took {:?}", t0.elapsed());
        gone(&fetch_pid);
        gone(&push_pid);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_last_write_goes_out_though_the_board_thread_is_stuck() {
        let (base, store) = scratch_store("last");
        let remote = scratch_remote(&base);
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let url = file_url(&remote);
        // like a thread inside a GitHub call, which nothing can stop
        let reader = std::thread::spawn(|| std::thread::sleep(Duration::from_secs(4)));
        let (fetched, stop, msg) = (AtomicBool::new(true), AtomicBool::new(true), json!({"state": "offline"}));
        let t0 = Instant::now();
        let mut pushed = None;
        leave_board(reader, &fetched, || vec![("pub".to_string(), "a".to_string())], |files| {
            assert!(stop.load(Relaxed), "the stop flag is set, and the last write doesn't look at it");
            pushed = last_write(&store, &url, "alice", &[], (&files, &msg, &idc), t0).ok();
        });
        assert!(t0.elapsed() < Duration::from_millis(3000), "it took {:?}", t0.elapsed());
        assert_eq!(git(&remote, &["rev-parse", "refs/qh/peers/alice"]), pushed);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn quitting_before_the_first_fetch_writes_no_folder() {
        let reader = std::thread::spawn(|| {});
        let (mut made, mut pushed) = (false, false);
        leave_board(reader, &AtomicBool::new(false), || made = true, |_| pushed = true);
        assert!(made, "the folder is still built: that also saves our state");
        assert!(!pushed, "a folder from before the first fetch has no wrapped keys, and would take them from everyone");
    }

    #[test]
    fn the_time_left_for_the_last_write() {
        let long_ago = |ms| Instant::now() - Duration::from_millis(ms);
        let left = |ms| last_write_limit(long_ago(ms)).unwrap();
        assert!(left(0) <= Duration::from_millis(4500) && left(0) > Duration::from_millis(4400));
        assert!(left(1000) <= Duration::from_millis(3500));
        assert_eq!(last_write_limit(long_ago(4400)), None, "too short to get anywhere, so nothing is started to outlive us");
        assert_eq!(last_write_limit(long_ago(9000)), None);
    }

    #[test]
    fn tidying_leaves_what_a_writer_just_made() {
        let (base, store) = scratch_store("tidy");
        let sha = blob(&store, "being written right now, nothing points at it yet").unwrap();
        assert!(tidy_store(&store, None));
        assert_eq!(git(&store, &["cat-file", "-t", &sha]).as_deref(), Some("blob"), "another git may be about to use it");
        assert!(!tidy_store(&base.join("project"), None), "only a store of ours is ever tidied");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A store that has read one teammate's folder from a local remote, and that teammate's next
    /// folder waiting there: a fetch now has to update the ref.
    fn store_behind_a_remote(name: &str) -> (PathBuf, PathBuf, String) {
        let (base, store) = scratch_store(name);
        let remote = scratch_remote(&base);
        let url = file_url(&remote);
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let folder = |text: &str| -> String {
            let files = [("pub".to_string(), text.to_string())];
            publish_to(&store, &url, "alice", &[], (&files, &json!({"state": "online"}), &idc), GIT_NET_LIMIT, None).unwrap()
        };
        folder("one");
        fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).unwrap();
        folder("two");
        (base, store, url)
    }

    /// Plant a file some killed git could have left, last written `age` ago.
    fn plant(path: &Path, age: Duration) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let f = std::fs::File::create(path).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    const LONG_AGO: Duration = Duration::from_secs(600);

    #[test]
    fn a_stale_lock_costs_one_failed_fetch_and_no_more() {
        let (base, store, url) = store_behind_a_remote("sweep-old");
        plant(&store.join("refs/qh/board/alice.lock"), LONG_AGO);
        assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err(), "a stale lock fails the whole fetch");
        let list = fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).expect("the failed fetch cleared it");
        assert_eq!(list.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_fresh_lock_is_left_alone() {
        let (base, store, url) = store_behind_a_remote("sweep-new");
        let lock = store.join("refs/qh/board/alice.lock");
        plant(&lock, Duration::from_secs(0));
        for _ in 0..2 {
            assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err());
            assert!(lock.exists(), "another git may be holding it: there's a holder lock for qh, not for an orphan git");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_sweep_takes_only_old_leftovers_in_a_store() {
        let (base, store) = scratch_store("sweep-what");
        let old = [store.join("refs/qh/board/alice.lock"), store.join("refs/qh/board/deep/er.lock"), store.join("packed-refs.lock"), store.join("objects/pack/tmp_pack_abc")];
        let kept = [store.join("refs/qh/board/bob.lock"), store.join("objects/pack/tmp_pack_new"), store.join("objects/pack/pack-1.pack")];
        let elsewhere = store.join("refs/qh/me.lock");
        for f in &old { plant(f, LONG_AGO) }
        plant(&kept[0], Duration::from_secs(5));
        plant(&kept[1], Duration::from_secs(5));
        plant(&kept[2], LONG_AGO);
        plant(&elsewhere, LONG_AGO);
        sweep_store(&store, SWEEP_AGE);
        for f in &old { assert!(!f.exists(), "{}", f.display()) }
        for f in &kept { assert!(f.exists(), "{}", f.display()) }
        assert!(elsewhere.exists(), "only the refs a fetch writes");
        // a project's .git is never ours to clear, whatever it holds
        let project = base.join("project");
        let lock = project.join("packed-refs.lock");
        plant(&lock, LONG_AGO);
        sweep_store(&project, SWEEP_AGE);
        assert!(lock.exists());
        // and a store missing its folders is nothing to fail over
        std::fs::remove_dir_all(store.join("objects/pack")).unwrap();
        sweep_store(&store, SWEEP_AGE);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_failing_fetch_backs_off_and_a_new_listing_doesnt_undo_that() {
        let t = Instant::now();
        let mut r = FetchRetry::new(FETCH_FIRST, t);
        r.jitter = 0;
        assert!(r.due(t), "the first read of the board goes ahead");
        let mut at = t;
        for want in [6, 12, 24, 48, 96, 120, 120, 120] {
            r.failed(at);
            assert!(!r.due(at + Duration::from_secs(want) - Duration::from_millis(1)), "before {want} s");
            r.need = true; // a new 200 changes nothing about that
            assert!(!r.due(at + Duration::from_secs(want) - Duration::from_millis(1)), "before {want} s, with a new listing");
            at += Duration::from_secs(want);
            assert!(r.due(at), "at {want} s");
        }
        r.worked(at);
        assert!(!r.due(at), "read: nothing more to fetch");
        r.need = true;
        assert!(r.due(at), "something new, and nothing to wait out");
        assert_eq!(r.fails, 0);
        r.failed(at);
        assert!(r.due(at + FETCH_FIRST), "it starts over from the shortest wait");
    }

    #[test]
    fn two_stuck_teammates_dont_retry_in_step() {
        let t = Instant::now();
        assert!(FetchRetry::new(FETCH_FIRST, t).jitter <= 25);
        let waits = |pid: u32| { let mut r = FetchRetry::new(FETCH_FIRST, t); r.jitter = jitter_of(pid); r.failed(t); r.next.duration_since(t) };
        assert!((0..500).all(|pid| waits(pid) >= FETCH_FIRST && waits(pid) <= FETCH_FIRST * 5 / 4));
        assert!((0..500).any(|pid| waits(pid) != waits(0)));
    }

    #[test]
    fn a_failing_push_backs_off_whatever_asks_for_it() {
        let t = Instant::now();
        let mut r = FetchRetry::new(PUSH_FIRST, t);
        r.jitter = 0;
        assert!(r.ready(t));
        let mut at = t;
        for want in [10, 20, 40, 80, 120, 120] {
            r.failed(at);
            assert!(!r.ready(at + Duration::from_secs(want) - Duration::from_millis(1)), "before {want} s");
            at += Duration::from_secs(want);
            assert!(r.ready(at), "at {want} s");
        }
        r.worked(at);
        assert!(r.ready(at) && r.fails == 0);
    }

    #[test]
    fn a_first_fetch_gets_longer_each_time_and_later_ones_dont() {
        let secs = |fails, fetched| fetch_limit(fails, fetched).as_secs();
        assert_eq!((secs(0, false), secs(1, false), secs(2, false), secs(9, false)), (30, 60, 120, 120));
        assert_eq!((secs(0, true), secs(5, true)), (30, 30), "the board's thread also writes our address: it can't sit in a fetch");
    }

    #[test]
    fn a_fetch_gives_up_on_a_connection_gone_slow() {
        let env = fetch_env("tok");
        let get = |k: &str| env.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!((get("GIT_HTTP_LOW_SPEED_LIMIT"), get("GIT_HTTP_LOW_SPEED_TIME")), (Some("1024"), Some("20")));
        assert!(get("GIT_CONFIG_VALUE_0").is_some_and(|v| v.starts_with("Authorization: Basic ")));
    }

    #[test]
    fn a_fetch_fails_then_works_when_the_remote_comes_back() {
        let (base, store) = scratch_store("retry");
        let remote = base.join("remote.git");
        let url = file_url(&remote);
        let t = Instant::now();
        let mut r = FetchRetry::new(FETCH_FIRST, t);
        assert!(r.due(t));
        assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err());
        r.failed(t);
        assert!(!r.due(t + Duration::from_secs(5)) && r.need && r.fails == 1);
        assert_eq!(scratch_remote(&base), remote);
        let later = t + FETCH_FIRST * 2;
        assert!(r.due(later));
        assert_eq!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).unwrap().len(), 0, "an empty board is a good read");
        r.worked(later);
        assert!(!r.due(later) && r.fails == 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// N3: GitHub says 404 for a repo we can't see, and 200 with an empty list for one with no
    /// folders yet. Only the second means we're first and should make the team key.
    #[test]
    fn no_access_makes_no_team_key_and_an_empty_board_does() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-listing-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("state")).unwrap();
            rerun("no_access_makes_no_team_key_and_an_empty_board_does",
                  &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.join("state").to_string_lossy())]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let base = Path::new(&base);
        let repo = "o/first-one";
        let mut s = state("alice");
        s.repo = repo.into();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), s);
        let t = Instant::now();
        let (mut retry, mut folders, mut etag) = (FetchRetry::new(FETCH_FIRST, t), HashMap::new(), None);
        // no access: an error to read, no key, and nothing to fetch
        assert!(!take_listing(repo, "alice", Ok((404, None, Value::Null)), &folders, &mut etag, &mut retry));
        assert!(with(repo, |s| !s.error.is_empty() && s.key.is_none()).unwrap(), "no key made, and the reason shows");
        assert!(!per_repo("team-keys", repo).exists(), "no key saved to outlive the problem");
        assert!(retry.need, "the board is still to be read");
        // other failures say what they say
        assert!(!take_listing(repo, "alice", Err("boom".into()), &folders, &mut etag, &mut retry));
        assert_eq!(with(repo, |s| s.error.clone()).unwrap(), "boom");
        // the listing comes back empty: fetch, and being first, make the key
        assert!(take_listing(repo, "alice", Ok((200, Some("e1".into()), json!([]))), &folders, &mut etag, &mut retry));
        assert_eq!(etag.as_deref(), Some("e1"));
        assert!(take_listing(repo, "alice", Ok((304, None, Value::Null)), &folders, &mut etag, &mut retry), "304 can still retry a fetch");
        assert!(retry.due(t));
        let store = base.join("store");
        assert_eq!(board_store_in(&store, base), store);
        let remote = scratch_remote(base);
        let list = fetch_refs(&store, &file_url(&remote), &[], GIT_NET_LIMIT, None).unwrap();
        retry.worked(t);
        let bytes: [u8; 32] = crate::oauth::random();
        let secret = x25519_dalek::StaticSecret::from(bytes);
        absorb(repo, &store, &list, &mut folders, &secret, "tok", &quiet(), &AtomicBool::new(false));
        assert!(with(repo, |s| s.key.is_some()).unwrap(), "the first on an empty board makes the key");
        // a listing that names a folder we haven't read: fetch again
        let found = json!([{"ref": "refs/qh/peers/bob", "object": {"sha": "abc"}}, {"ref": "refs/qh/peers/alice", "object": {"sha": "mine"}}]);
        assert!(!retry.need);
        assert!(take_listing(repo, "alice", Ok((200, Some("e2".into()), found.clone())), &folders, &mut etag, &mut retry));
        assert!(retry.need);
        retry.worked(t);
        let mine = json!([{"ref": "refs/qh/peers/alice", "object": {"sha": "mine2"}}]);
        assert!(take_listing(repo, "alice", Ok((200, None, mine)), &folders, &mut etag, &mut retry));
        assert!(!retry.need, "our own folder moving is no reason to fetch");
    }

    fn watcher(sent: &[&str], before: &[&str], since_send: Duration) -> Watcher {
        Watcher { login: "bob".into(), key: "k".into(), session: "main".into(), until: Instant::now() + WATCH_LIFE, sent: rows(sent),
                  before: rows_hash(&rows(before)), have: None, at: Instant::now() - since_send }
    }

    #[test]
    fn a_lost_transcript_update_mends_itself() {
        let now = rows(&["a", "b", "c"]);
        let (hash, recent, long) = (rows_hash(&now), Duration::from_secs(1), Duration::from_secs(16));
        let held_before = rows_hash(&rows(&["a", "b"]));
        // they still say what they held before our last send: it may be on its way
        let mut w = watcher(&["a", "b", "c"], &["a", "b"], recent);
        reconcile(&mut w, held_before, hash, &now);
        assert_eq!(w.sent, now, "not yet: no reason to send it all again");
        // ...but not for ever: it was lost, and nothing else would ever send it
        let mut w = watcher(&["a", "b", "c"], &["a", "b"], long);
        reconcile(&mut w, held_before, hash, &now);
        assert!(w.sent.is_empty(), "sent whole");
        // they hold what's current, whatever we thought
        let mut w = watcher(&["x"], &[], long);
        reconcile(&mut w, hash, hash, &now);
        assert_eq!(w.sent, now);
        // they hold what we last sent, and something newer is yet to go: only that goes
        let mut w = watcher(&["a", "b"], &["a"], long);
        reconcile(&mut w, rows_hash(&rows(&["a", "b"])), hash, &now);
        assert_eq!(w.sent, rows(&["a", "b"]));
        // they hold something else altogether
        let mut w = watcher(&["a", "b"], &["a"], recent);
        reconcile(&mut w, 12345, hash, &now);
        assert!(w.sent.is_empty());
    }

    #[test]
    fn loose_objects_are_read_from_count_objects() {
        assert_eq!(loose_objects("count: 2345\nsize: 10\nin-pack: 7\npacks: 1\nsize-pack: 9\nprune-packable: 0\ngarbage: 0\nsize-garbage: 0"), Some(2345));
        assert_eq!(loose_objects("in-pack: 7"), None);
        assert_eq!(loose_objects("count: lots"), None);
    }

    #[test]
    fn a_store_with_too_many_loose_objects_is_tidied_and_keeps_our_commit() {
        let (base, store) = scratch_store("gc");
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        assert!(git(&store, &["update-ref", "refs/qh/me", &idc]).is_some());
        let loose = || loose_objects(&git(&store, &["count-objects", "-v"]).unwrap()).unwrap();
        let before = loose();
        assert!(before >= 4, "commit, tree, two blobs");
        // temp packs from fetches killed long ago go, and any from a fetch running now stay
        let (old, new) = (store.join("objects/pack/tmp_pack_old"), store.join("objects/pack/tmp_pack_new"));
        plant(&old, Duration::from_secs(2 * 3600));
        plant(&new, Duration::from_secs(5));
        assert!(!tidy_if_big(&store, before, None), "at the limit: left alone");
        assert_eq!(loose(), before);
        assert!(old.exists());
        assert!(tidy_if_big(&store, before - 1, None), "over it");
        assert!(loose() < before, "the commit we keep went into a pack");
        assert_eq!(git(&store, &["rev-parse", "refs/qh/me"]).as_deref(), Some(idc.as_str()));
        assert_eq!(git(&store, &["cat-file", "-t", &idc]).as_deref(), Some("commit"));
        assert!(!old.exists() && new.exists(), "old temp pack cleared, a fresh one kept");
        assert!(!tidy_if_big(&base.join("project"), 0, None), "only a store of ours");
        // told to stop: it doesn't start
        assert!(!tidy_if_big(&store, 0, Some(&AtomicBool::new(true))));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_call_says_why_without_saying_what_it_ran() {
        let run = |script: &str, limit, stop: Option<&AtomicBool>| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            run_limited(c, limit, stop)
        };
        let long = Duration::from_secs(5);
        assert_eq!(run("exit 0", long, None), Ok(()));
        assert_eq!(run("exit 128", long, None), Err("exit code 128".to_string()));
        assert_eq!(run("sleep 30 # secret", Duration::from_millis(100), None), Err("timed out after 0 s".to_string()));
        assert_eq!(run("exit 0", long, Some(&AtomicBool::new(true))), Err("stopped".to_string()));
        assert_eq!(run("kill -9 $$", long, None), Err("ended by a signal".to_string()));
        let mut none = Command::new("/no/such/program");
        none.arg("secret");
        assert!(run_limited(none, long, None).is_err_and(|e| !e.contains("secret")));
    }

    #[test]
    fn a_failed_fetch_or_push_says_why() {
        let (base, store) = scratch_store("why");
        let url = file_url(&base.join("no-such-remote"));
        let e = fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).unwrap_err();
        assert!(e.starts_with("fetching the board failed") && e.contains("exit code"), "{e}");
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let e = publish_to(&store, &url, "alice", &[], (&[], &json!({}), &idc), GIT_NET_LIMIT, Some(&AtomicBool::new(true))).unwrap_err();
        assert!(e.starts_with("push to the board failed") && e.contains("stopped"), "{e}");
        let _ = std::fs::remove_dir_all(&base);
    }

    // item 1
    #[test]
    fn quitting_during_the_first_take_in_doesnt_count_as_having_read_the_board() {
        // quit comes while we're taking in the folders
        let (fetched, stop) = (AtomicBool::new(false), AtomicBool::new(false));
        take_in_board(&fetched, &stop, || {
            assert!(!fetched.load(Relaxed), "nothing is taken in yet");
            stop.store(true, Relaxed);
        });
        assert!(!fetched.load(Relaxed));
        let reader = std::thread::spawn(|| {});
        let mut pushed = false;
        leave_board(reader, &fetched, || (), |_| pushed = true);
        assert!(!pushed, "we know nobody yet: a folder now would have no keys/* for anyone");
        // the real thing, stopped: it returns before knowing anyone or making a key
        let repo = format!("o/stopped-{}", std::process::id());
        let mut st = state("alice");
        st.repo = repo.clone();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), st);
        let bytes: [u8; 32] = crate::oauth::random();
        let secret = x25519_dalek::StaticSecret::from(bytes);
        let fetched = AtomicBool::new(false);
        take_in_board(&fetched, &stop, || absorb(&repo, Path::new("."), &[], &mut HashMap::new(), &secret, "", &quiet(), &stop));
        assert!(!fetched.load(Relaxed));
        assert!(with(&repo, |s| s.key.is_none()).unwrap());
        // not stopped: a finished take-in is a read, and that's when writing may begin
        let (fetched, stop) = (AtomicBool::new(false), AtomicBool::new(false));
        take_in_board(&fetched, &stop, || assert!(!fetched.load(Relaxed)));
        assert!(fetched.load(Relaxed));
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).remove(&repo);
    }

    // item 2
    #[test]
    fn a_stale_config_lock_keeps_the_store() {
        let (base, store) = scratch_store("cfglock");
        let project = base.join("project");
        let config = store.join("config");
        plant(&store.join("config.lock"), LONG_AGO);
        assert_eq!(board_store_in(&store, &project), store, "values already right: nothing to write");
        assert!(is_store(&store));
        // a value that needs writing can't be, this time: still the store, and it works
        let text = std::fs::read_to_string(&config).unwrap().replace("auto = 0", "auto = 5");
        std::fs::write(&config, text).unwrap();
        assert_eq!(board_store_in(&store, &project), store, "settings are worth having, not worth leaving the store for");
        assert!(is_store(&store));
        assert!(identity_commit(&store, "alice", "a", "b").is_some());
        let remote = scratch_remote(&base);
        assert!(fetch_refs(&store, &file_url(&remote), &[], GIT_NET_LIMIT, None).is_ok(), "a fetch doesn't need that lock");
        assert!(tidy_store(&store, None), "nor does tidying");
        // the lock gone, the value is put right
        std::fs::remove_file(store.join("config.lock")).unwrap();
        assert_eq!(board_store_in(&store, &project), store);
        assert_eq!(git(&store, &["config", "gc.auto"]).as_deref(), Some("0"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_store_with_the_right_settings_isnt_written_to() {
        let (base, store) = scratch_store("cfgsame");
        let f = std::fs::File::options().write(true).open(store.join("config")).unwrap();
        let old = SystemTime::now() - LONG_AGO;
        f.set_modified(old).unwrap();
        drop(f);
        assert_eq!(board_store_in(&store, &base.join("project")), store);
        assert_eq!(std::fs::metadata(store.join("config")).unwrap().modified().unwrap(), old);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_store_git_cant_read_falls_back_and_is_no_store() {
        let (base, store) = scratch_store("broken");
        std::fs::write(store.join("HEAD"), "not a ref\n").unwrap();
        let project = base.join("project");
        assert_eq!(board_store_in(&store, &project), project);
        assert!(!is_store(&store));
        let _ = std::fs::remove_dir_all(&base);
    }

    // item 3
    #[cfg(unix)]
    #[test]
    fn a_slow_commit_doesnt_push_the_end_of_the_last_write_past_the_quit_wait() {
        let (base, store) = scratch_store("slowcommit");
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let (stalled, pid) = stalled_remote(&base, "push");
        let env = [("GIT_ALLOW_PROTOCOL", "ext".to_string())];
        // out of time before the push: none is started, so none can outlive us
        let since = Instant::now() - Duration::from_millis(4200);
        let t0 = Instant::now();
        assert!(last_write(&store, &stalled, "alice", &env, (&[("pub".to_string(), "a".to_string())], &json!({"try": 2}), &idc), since).is_err());
        assert!(t0.elapsed() < Duration::from_millis(1000), "it took {:?}", t0.elapsed());
        // Each file in a commit is a git process. Enough of them to take about 800 ms on this machine:
        // past 500 ms, a limit worked out before the commit would run over the 5 s
        let t = Instant::now();
        for i in 0..5 { blob(&store, &format!("timing {i}")).unwrap(); }
        let spawn = (t.elapsed() / 5).max(Duration::from_micros(100));
        let mut count = (800_000 / spawn.as_micros().max(1)).clamp(20, 4000) as usize;
        let files = |n: usize| -> Vec<(String, String)> { (0..n).map(|i| (format!("f{i}"), format!("{i}"))).collect() };
        let mut commit_takes;
        loop {
            let t = Instant::now();
            folder_commit(&store, &files(count), &json!({"try": 0}), &idc).unwrap();
            commit_takes = t.elapsed();
            if commit_takes >= Duration::from_millis(500) || count >= 4000 { break }
            count *= 2;
        }
        if commit_takes > Duration::from_secs(3) || commit_takes < Duration::from_millis(500) {
            println!("skipped: a commit of {count} files takes {commit_takes:?} here, which leaves nothing to show");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        // the commit ends with just enough left for a push: a limit worked out before it would run 'commit_takes' over
        let since = Instant::now() - Duration::from_millis(3900).saturating_sub(commit_takes);
        assert!(last_write(&store, &stalled, "alice", &env, (&files(count), &json!({"try": 1}), &idc), since).is_err());
        assert!(since.elapsed() < Duration::from_secs(5), "ended {:?} after quitting began; the commit takes {commit_takes:?}", since.elapsed());
        // no pid: this commit ran long enough that no push was started, which is right too
        if pid.exists() { gone(&pid) }
        let _ = std::fs::remove_dir_all(&base);
    }

    // item 4
    #[test]
    fn a_stop_that_comes_during_a_github_call_saves_no_key() {
        let Ok(_) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-latekey-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            rerun("a_stop_that_comes_during_a_github_call_saves_no_key",
                  &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.to_string_lossy()), ("QH_TEAM_ADMINS", "alice"), ("GH_TOKEN", "x")]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let repo = "o/late-key";
        let mut st = state("alice");
        st.repo = repo.into();
        let mut gone = peer("bob", &[], false);
        gone.locked = true;
        gone.ts = 1;
        st.peers.insert("bob".into(), gone);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), st);
        let stopped = AtomicBool::new(true);
        recover_key(repo, &stopped);
        assert!(with(repo, |s| s.key.is_none() && s.epoch == 0).unwrap(), "the lock may be someone else's by now");
        assert!(!per_repo("team-keys", repo).exists());
        // and the same with nothing stopping it does make one: the test above means something
        recover_key(repo, &AtomicBool::new(false));
        assert!(with(repo, |s| s.key.is_some() && s.epoch == 1).unwrap());
    }

    #[test]
    fn a_stop_that_comes_during_a_github_call_pins_no_keys() {
        let Ok(_) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-latepin-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            rerun("a_stop_that_comes_during_a_github_call_pins_no_keys", &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.to_string_lossy())]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let repo = "o/late-pin";
        let mut st = state("alice");
        st.repo = repo.into();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), st);
        // no anchor, so `vouched` answers without asking anyone
        let p = Peer { login: "bob".into(), pub_key: "k".into(), sign: "s".into(), signed: true, seq: 5, ..Default::default() };
        let stopped = AtomicBool::new(true);
        assert!(!vet(repo, "", &p, &quiet(), &stopped), "not taken in");
        assert!(with(repo, |s| s.pins.is_empty()).unwrap());
        assert!(!per_repo("team-pins", repo).exists());
        assert!(vet(repo, "", &p, &quiet(), &AtomicBool::new(false)));
        assert!(with(repo, |s| s.pins.contains_key("bob")).unwrap());
    }

    // item 5
    #[test]
    fn the_big_look_at_a_store_waits_for_the_first_push() {
        assert_eq!(tidy_limit(1), Some(LOOSE_TIDY), "after the first write: our address is out by then");
        assert_eq!(tidy_limit(2), None);
        assert_eq!(tidy_limit(199), None);
        assert_eq!(tidy_limit(BOARD_TIDY), Some(0), "and every so often, whatever the count");
        assert_eq!(tidy_limit(2 * BOARD_TIDY), Some(0));
    }

    // item 6
    #[test]
    fn a_list_git_couldnt_read_is_no_empty_board() {
        let (base, store) = scratch_store("list");
        assert_eq!(list_board(&store).unwrap().len(), 0, "really empty");
        let not_a_repo = base.join("project");
        assert!(list_board(&not_a_repo).is_err(), "couldn't say: a key made on this would be a wrong one");
        let _ = std::fs::remove_dir_all(&base);
    }

    // item 7
    #[test]
    fn a_new_store_uses_ref_files_whatever_git_defaults_to() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-reffmt-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("project")).unwrap();
            let config = base.join("gitconfig");
            std::fs::write(&config, "[init]\n\tdefaultRefFormat = reftable\n").unwrap();
            rerun("a_new_store_uses_ref_files_whatever_git_defaults_to", &[("QH_TEST_BASE", &base.to_string_lossy()), ("GIT_CONFIG_GLOBAL", &config.to_string_lossy()), ("GIT_DEFAULT_REF_FORMAT", "reftable")]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let (store, root) = (Path::new(&base).join("store"), Path::new(&base).join("project"));
        assert_eq!(board_store_in(&store, &root), store);
        assert!(!store.join("reftable").exists(), "the sweep and everyone's tools know these");
        assert!(identity_commit(&store, "alice", "a", "b").is_some());
    }

    #[test]
    fn a_reftable_store_made_earlier_keeps_working_and_is_swept() {
        let base = std::env::temp_dir().join(format!("qh-reftable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let store = base.join("store");
        std::fs::create_dir_all(&store).unwrap();
        assert!(git_in(&store, &["init", "-q", "--bare"], None, &[("GIT_DEFAULT_REF_FORMAT", "reftable".to_string()), ("GIT_DEFAULT_HASH", "sha1".to_string())]).is_some());
        if !store.join("reftable").exists() {
            let _ = std::fs::remove_dir_all(&base);
            return; // a git too old to make one has none to keep working
        }
        assert_eq!(board_store_in(&store, &base.join("project")), store);
        assert!(store.join("reftable").exists(), "left as it is");
        let url = file_url(&scratch_remote(&base));
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        publish_to(&store, &url, "alice", &[], (&[("pub".to_string(), "x".to_string())], &json!({}), &idc), GIT_NET_LIMIT, None).unwrap();
        let lock = store.join("reftable/tables.list.lock");
        plant(&lock, Duration::from_secs(0));
        assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err());
        assert!(lock.exists(), "a fresh one may be in use");
        plant(&lock, LONG_AGO);
        assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err());
        assert_eq!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).expect("the failed fetch cleared it").len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    // item 9
    #[test]
    #[should_panic(expected = "ran no test")]
    fn rerun_notices_a_test_that_isnt_there() {
        rerun("no_test_has_this_name", &[]);
    }

    #[test]
    fn a_new_store_is_sha1_whatever_git_defaults_to() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-hash-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("project")).unwrap();
            let config = base.join("gitconfig");
            std::fs::write(&config, "[init]\n\tdefaultObjectFormat = sha256\n").unwrap();
            rerun("a_new_store_is_sha1_whatever_git_defaults_to", &[("QH_TEST_BASE", &base.to_string_lossy()), ("GIT_CONFIG_GLOBAL", &config.to_string_lossy()), ("GIT_DEFAULT_HASH", "sha256")]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let (store, root) = (Path::new(&base).join("store"), Path::new(&base).join("project"));
        assert_eq!(board_store_in(&store, &root), store);
        assert_eq!(identity_commit(&store, "alice", "a", "b").unwrap().len(), 40, "GitHub is sha1, and the identity commit goes there");
    }

    /// A sha256 store made earlier: a store is sha256 or it talks to GitHub, so it holds nothing
    /// that ever went anywhere, and everything in it is built again from our state.
    #[test]
    fn a_sha256_store_made_earlier_is_replaced() {
        let base = std::env::temp_dir().join(format!("qh-sha256-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let store = base.join("store");
        std::fs::create_dir_all(&store).unwrap();
        assert!(git_in(&store, &["init", "-q", "--bare"], None, &[("GIT_DEFAULT_HASH", "sha256".to_string())]).is_some());
        if !git_raw(&store, &["--git-dir=.", "rev-parse", "--show-object-format"], None, &[]).is_some_and(|o| o.starts_with(b"sha256")) {
            let _ = std::fs::remove_dir_all(&base);
            return; // a git too old to make one has none to replace
        }
        let url = file_url(&scratch_remote(&base));
        // why replacing it loses nothing: it can't push, even to an empty remote, or fetch a folder
        STORES.lock().unwrap().push(store.clone());
        let old = identity_commit(&store, "alice", "a", "b").unwrap();
        assert_eq!(old.len(), 64);
        let one = |root: &Path, idc: &str| publish_to(root, &url, "alice", &[], (&[("pub".to_string(), "x".to_string())], &json!({}), idc), GIT_NET_LIMIT, None);
        assert!(one(&store, &old).is_err(), "nothing it holds ever went anywhere");
        let side = base.join("side");
        assert_eq!(board_store_in(&side, &base.join("project")), side);
        let idc = identity_commit(&side, "alice", "a", "b").unwrap();
        one(&side, &idc).unwrap();
        assert!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).is_err(), "and it can't read the board");
        // made fresh, it does both
        assert_eq!(board_store_in(&store, &base.join("project")), store);
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        assert_eq!(idc.len(), 40);
        one(&store, &idc).unwrap();
        assert_eq!(fetch_refs(&store, &url, &[], GIT_NET_LIMIT, None).unwrap().len(), 1);
        let names: Vec<_> = std::fs::read_dir(&base).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert!(!names.iter().any(|n| n.contains("sha256")), "nothing left over: {names:?}");
        // it stays what it now is: the next start finds nothing to do
        assert_eq!(board_store_in(&store, &base.join("project")), store);
        assert_eq!(git(&store, &["cat-file", "-t", &idc]).as_deref(), Some("commit"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_sha256_store_that_cant_be_replaced_is_no_store() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("qh-sha256-stuck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (parent, project) = (base.join("parent"), base.join("project"));
        let store = parent.join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        assert!(git_in(&store, &["init", "-q", "--bare"], None, &[("GIT_DEFAULT_HASH", "sha256".to_string())]).is_some());
        if !git_raw(&store, &["--git-dir=.", "rev-parse", "--show-object-format"], None, &[]).is_some_and(|o| o.starts_with(b"sha256")) {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        let got = board_store_in(&store, &project);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(got, project, "the same way out as any store git can't use, which says so");
        assert!(!is_store(&store));
        assert!(store.join("HEAD").exists(), "left as it was");
        let _ = std::fs::remove_dir_all(&base);
    }

    // second review, item 2
    #[test]
    fn replacing_a_sha256_store_leaves_a_team_called_after_its_old_copy_alone() {
        let base = std::env::temp_dir().join(format!("qh-sha256-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let boards = base.join("team-board");
        // the repo o/r.old-sha256 keeps its store here: a real team's
        let (ours, theirs) = (boards.join("o__r"), boards.join("o__r.old-sha256"));
        assert_eq!(board_store_in(&theirs, &base.join("project")), theirs);
        let sha = identity_commit(&theirs, "bob", "a", "b").unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        assert!(git_in(&ours, &["init", "-q", "--bare"], None, &[("GIT_DEFAULT_HASH", "sha256".to_string())]).is_some());
        if !git_raw(&ours, &["--git-dir=.", "rev-parse", "--show-object-format"], None, &[]).is_some_and(|o| o.starts_with(b"sha256")) {
            let _ = std::fs::remove_dir_all(&base);
            return; // a git too old to make one
        }
        assert_eq!(board_store_in(&ours, &base.join("project")), ours);
        assert_eq!(git(&ours, &["rev-parse", "--show-object-format"]).as_deref(), Some("sha1"));
        assert_eq!(git(&theirs, &["cat-file", "-t", &sha]).as_deref(), Some("commit"), "the other team's store is still there");
        assert!(!base.join("team-board-old").exists(), "and ours left nothing behind");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn only_github_itself_counts_as_github() {
        for url in ["https://github.com/o/r", "https://github.com/o/r.git", "https://github.com/o/r/", "https://github.com/o/r.git/", "http://github.com/o/r",
                    "https://user@github.com/o/r", "https://user:tok@github.com/o/r", "git@github.com:o/r", "git@github.com:o/r.git", "git@github.com:/o/r", "github.com:o/r",
                    "ssh://git@github.com/o/r", "ssh://git@github.com:22/o/r", "git://github.com/o/r", "https://github.com:443/o/r",
                    "https://www.github.com/o/r", "git@www.github.com:o/r", "https://GitHub.COM/o/r", "git@GITHUB.com:o/r", "SSH://git@github.com/o/r"] {
            assert_eq!(github_slug(url).as_deref(), Some("o/r"), "{url}");
        }
        for url in ["https://evil.com/github.com/o/r", "notgithub.com:o/r", "git@notgithub.com:o/r", "https://notgithub.com/o/r", "https://github.com.evil.com/o/r", "git@github.com.evil.com:o/r",
                    "https://evil.com/?x=github.com/o/r", "https://github.com@evil.com/o/r", "https://github.com:x@evil.com/o/r", "git@evil.com:github.com/o/r",
                    // what's before the @ is only a login if nothing before it ends the host first
                    "https://evil.com?x=@github.com/o/r", "https://evil.com#@github.com/o/r", "https://evil.com\\@github.com/o/r", "https://evil.com/@github.com/o/r",
                    "https://github.com:x/o/r", "file:///tmp/github.com/o/r", "/tmp/github.com:o/r", "./github.com:o/r", "ext::sh github.com:o/r", "ftp://github.com/o/r",
                    // an ssh alias says nothing about where it leads
                    "git@github.com-work:o/r", "github.com-work:o/r", "ssh://git@github.com-work/o/r"] {
            assert_eq!(github_slug(url), None, "{url}");
        }
    }

    // second review, item 3
    #[test]
    fn only_a_plain_owner_and_name_make_a_team_folder() {
        let slug = |u: &str| github_slug(u);
        for (url, want) in [("https://github.com/o/r.git", "o/r"), ("git@github.com:o/r.git", "o/r"), ("https://github.com/o/r/", "o/r"),
                            ("ssh://git@github.com/my-org/re_po.v2", "my-org/re_po.v2"), ("https://x@github.com/O/R", "O/R")] {
            assert_eq!(slug(url).as_deref(), Some(want), "{url}");
        }
        for url in ["https://github.com/o/..\\..\\x", "https://github.com/..\\x/r", "https://github.com/../r", "https://github.com/o/..", "https://github.com/./r",
                    "https://github.com/o/.", "https://github.com/o/r x", "https://github.com/o/r\nz", "https://github.com//r", "https://github.com/o/a/b",
                    "https://github.com/o/r:x", "https://github.com/o", "https://example.com/o/r"] {
            assert_eq!(slug(url), None, "{url}");
        }
        // the other way a name gets here; refused before it asks anyone anything
        assert!(crate::teamsetup::join_inner("o/..\\..\\x", None).is_err_and(|e| e.contains("owner/repo")));
    }

    // second review, item 4
    #[test]
    fn a_store_we_cant_use_is_said_to_whoever_started_the_team() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-warnstore-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("project")).unwrap();
            rerun("a_store_we_cant_use_is_said_to_whoever_started_the_team", &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.to_string_lossy())]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let root = Path::new(&base).join("project");
        // git can't read it: we're told, naming the folder and where the data goes instead
        let dir = per_repo("team-board", "o/broken");
        assert_eq!(board_store_in(&dir, &root), dir);
        std::fs::write(dir.join("HEAD"), "not a ref\n").unwrap();
        assert_eq!(board_store(&"o/broken".to_string(), &root), root);
        let w = store_warning("o/broken").expect("said");
        assert!(w.contains("team-board") && w.contains(".git"), "{w}");
        // all's well: not a word
        assert_eq!(board_store(&"o/fine".to_string(), &root), per_repo("team-board", "o/fine"));
        assert_eq!(store_warning("o/fine"), None);
    }

    // second review, item 6
    #[test]
    fn settling_the_key_does_nothing_once_stopped() {
        let Ok(_) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-settlestop-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            rerun("settling_the_key_does_nothing_once_stopped", &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.to_string_lossy())]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let repo = "o/settle-stop";
        let mut st = state("alice");
        st.repo = repo.into();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), st);
        settle_key(repo, &AtomicBool::new(true));
        assert!(with(repo, |s| s.key.is_none()).unwrap() && !per_repo("team-keys", repo).exists(), "the lock may be someone else's by now");
        settle_key(repo, &AtomicBool::new(false));
        assert!(with(repo, |s| s.key.is_some()).unwrap());
    }

    #[test]
    fn while_you_were_away_ends_with_the_first_read_of_the_board() {
        let inc = |ts: u64| Incoming { id: ts, ts, ..Default::default() };
        let fyi = |ts: u64| Fyi { ts, kind: "reply".into(), text: "x".into() };
        let mut st = state("alice");
        st.last_open = 100;
        st.pending = vec![inc(50), inc(200), inc(400)];
        st.fyi = vec![fyi(50), fyi(200), fyi(400)];
        let got = |s: &State| { let a = away_of(s); (a.needs_you.iter().map(|i| i.ts).collect::<Vec<_>>(), a.fyi.iter().map(|f| f.ts).collect::<Vec<_>>()) };
        // still catching up: what arrives now was sent while we were away, whatever its stamp
        assert_eq!(got(&st), (vec![200, 400], vec![200, 400]));
        // caught up at 300: what came after that came with us here
        st.away_until = 300;
        assert_eq!(got(&st), (vec![200], vec![200]));
        // the first read that finishes sets it, and only the first
        let repo = format!("req/caught-{}", std::process::id());
        let mut st = state("alice");
        st.repo = repo.clone();
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), st);
        let until = || with(&repo, |s| s.away_until).unwrap();
        caught_up(&repo, false, &AtomicBool::new(false));
        assert_eq!(until(), 0, "quit during it: still catching up");
        caught_up(&repo, false, &AtomicBool::new(true));
        let first = until();
        assert!(first >= now() - 2, "{first}");
        with(&repo, |s| s.away_until = 1);
        caught_up(&repo, true, &AtomicBool::new(true));
        assert_eq!(until(), 1, "later reads leave it");
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).remove(&repo);
    }

    #[test]
    fn the_board_stays_out_of_the_project() {
        let base = std::env::temp_dir().join(format!("qh-store-{}", std::process::id()));
        let (project, remote, store) = (base.join("project"), base.join("remote.git"), base.join("store"));
        std::fs::create_dir_all(&project).unwrap();
        assert!(git(&project, &["init", "-q"]).is_some());
        assert_eq!(scratch_remote(&base), remote);
        assert_eq!(board_store_in(&store, &project), store);
        let count = |d: &Path| git(d, &["count-objects"]).unwrap_or_default();
        let before = count(&project);
        // a folder written the way the board's thread writes one: a commit on the identity commit
        let idc = identity_commit(&store, "alice", "a", "b").unwrap();
        let msg = json!({"state": "online"});
        let commit = folder_commit(&store, &[("pub".to_string(), "a".to_string())], &msg, &idc).unwrap();
        let url = remote.to_string_lossy().to_string();
        assert!(git_net(&store, &["push", "-q", "-f", &url, &format!("{commit}:refs/qh/peers/alice")], &[], GIT_NET_LIMIT, None).is_ok(), "no project history is needed to push");
        assert_eq!(count(&project), before, "nothing landed in the project's .git");
        git(&store, &["update-ref", "refs/qh/me", &idc]);
        assert!(tidy_store(&store, None));
        assert!(folder_commit(&store, &[("pub".to_string(), "c".to_string())], &msg, &idc).is_ok(), "tidying keeps what the next folder needs");
        let _ = std::fs::remove_dir_all(&base);
    }

    // second review, defect 2
    #[test]
    fn a_role_change_clears_topics_even_with_no_team_here() {
        let repo = format!("req/cleartopics-{}", std::process::id());
        // a copy from an earlier run: sam may promise refunds; this process has no team open for it
        save_json("team-requests", &repo, &json!({"topics": {"sam": ["refunds"], "ravi": ["payouts"]}, "ts": 1}));
        assert!(with(&repo, |_| ()).is_none(), "no state here");
        set_role(&repo, "sam", "member").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(per_repo("team-requests", &repo)).unwrap()).unwrap();
        assert!(v["topics"].get("sam").is_none(), "the copy on disk must not bring the trust's topics back: {v}");
        assert!(v["topics"].get("ravi").is_some(), "someone else's topics stay: {v}");
        // with the team open, both copies go
        let mut st = state("alice");
        st.repo = repo.clone();
        st.flow.topics.insert("sam".into(), vec!["refunds".into()]);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), st);
        save_json("team-requests", &repo, &json!({"topics": {"sam": ["refunds"]}, "ts": 2}));
        set_role(&repo, "sam", "viewer").unwrap();
        assert!(with(&repo, |s| !s.flow.topics.contains_key("sam")).unwrap(), "in memory too");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(per_repo("team-requests", &repo)).unwrap()).unwrap();
        assert!(v["topics"].get("sam").is_none(), "and on disk: {v}");
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
    }

    // second review, defect 2
    #[test]
    fn removing_a_member_clears_their_topics() {
        let Ok(_) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-remove-topics-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            rerun("removing_a_member_clears_their_topics", &[("QH_TEST_BASE", &base.to_string_lossy()), ("QH_STATE_DIR", &base.to_string_lossy()),
                                                             ("QH_TEAM_ADMINS", "alice"), ("GH_TOKEN", "x")]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let repo = "req/remove-topics";
        let mut st = state("alice");
        st.repo = repo.into();
        st.approved = vec!["sam".into()];
        st.flow.topics.insert("sam".into(), vec!["refunds".into()]);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.to_string(), st);
        save_json("team-requests", repo, &json!({"topics": {"sam": ["refunds"]}, "ts": 1}));
        remove(repo, "sam").unwrap();
        assert!(with(repo, |s| !s.flow.topics.contains_key("sam")).unwrap(), "topics go with their trust");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(per_repo("team-requests", repo)).unwrap()).unwrap();
        assert!(v["topics"].get("sam").is_none(), "and off the copy on disk: {v}");
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    // second review, defect 3
    #[test]
    fn dropping_a_topic_that_isnt_there_says_so_and_leaves_no_entry() {
        let repo = format!("req/topicdrop-{}", std::process::id());
        let mut st = state("alice");
        st.repo = repo.clone();
        st.running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)); // else `cur` says team mode is off
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), st);
        set_current(&repo);
        let e = set_topic("sam", "refunds", true).unwrap_err();
        assert!(e.contains("no topic"), "{e}");
        assert!(with(&repo, |s| !s.flow.topics.contains_key("sam")).unwrap(), "no empty entry");
        set_topic("sam", "refunds", false).unwrap();
        set_topic("sam", "refunds", true).unwrap();
        assert!(with(&repo, |s| !s.flow.topics.contains_key("sam")).unwrap(), "an empty list is no list");
        STATES.lock().unwrap().as_mut().unwrap().remove(&repo);
        set_current("");
    }

    /// Run one test again in a process of its own, with `env` set (tests run in parallel, so no set_var).
    fn rerun(test: &str, env: &[(&str, &str)]) {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", &format!("team::tests::{test}"), "--nocapture"]).envs(env.iter().copied());
        let o = c.output().unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        assert!(o.status.success(), "{out}\n{}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains(" 1 passed;"), "ran no test called {test}:\n{out}");
    }

    /// A folder for one test, with a git config that refuses bare repositories found by looking
    /// (what Git 3.0 does by default).
    fn explicit_only(name: &str) -> (PathBuf, String) {
        let base = std::env::temp_dir().join(format!("qh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("project")).unwrap();
        let config = base.join("gitconfig");
        std::fs::write(&config, "[safe]\n\tbareRepository = explicit\n").unwrap();
        (base, config.to_string_lossy().to_string())
    }

    #[test]
    fn the_store_works_when_git_only_takes_explicit_bare_repositories() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let (base, config) = explicit_only("explicit");
            rerun("the_store_works_when_git_only_takes_explicit_bare_repositories",
                  &[("QH_TEST_BASE", &base.to_string_lossy()), ("GIT_CONFIG_GLOBAL", &config)]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let (store, root) = (Path::new(&base).join("store"), Path::new(&base).join("project"));
        for round in 0..2 {
            assert_eq!(board_store_in(&store, &root), store, "round {round}: the store, not the project");
            let idc = identity_commit(&store, "alice", "a", "b");
            assert!(idc.is_some(), "round {round}: the identity commit is written");
            assert!(git(&store, &["update-ref", "refs/qh/me", &idc.unwrap()]).is_some(), "round {round}: the ref is written");
        }
        // a store made before this setting, or by an older qh, gets its settings back every start
        std::fs::write(store.join("config"), "[core]\n\tbare = true\n\trepositoryformatversion = 0\n").unwrap();
        assert_eq!(board_store_in(&store, &root), store);
        assert_eq!(git(&store, &["config", "gc.auto"]).as_deref(), Some("0"));
        assert_eq!(git(&store, &["config", "maintenance.auto"]).as_deref(), Some("false"));
    }

    #[test]
    fn only_a_registered_store_skips_the_bare_repository_protection() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let (base, config) = explicit_only("embedded");
            rerun("only_a_registered_store_skips_the_bare_repository_protection",
                  &[("QH_TEST_BASE", &base.to_string_lossy()), ("GIT_CONFIG_GLOBAL", &config)]);
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        // a bare repository that came with a clone: git must go on refusing it
        let embedded = Path::new(&base).join("embedded");
        std::fs::create_dir_all(&embedded).unwrap();
        assert!(git(&embedded, &["init", "-q", "--bare"]).is_some());
        assert!(git(&embedded, &["rev-parse", "--git-dir"]).is_none(), "found by looking, so refused");
        // and when a store can't be set up, we fall back to the project and stop treating it as one
        let (file, root) = (Path::new(&base).join("a-file"), Path::new(&base).join("project"));
        std::fs::write(&file, "x").unwrap();
        assert_eq!(board_store_in(&file.join("store"), &root), root);
        assert!(git(&file.join("store"), &["rev-parse", "--git-dir"]).is_none());
    }

    #[test]
    fn the_store_ignores_git_variables_meant_for_another_repository() {
        let Ok(base) = std::env::var("QH_TEST_BASE") else {
            let base = std::env::temp_dir().join(format!("qh-localenv-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let elsewhere = base.join("elsewhere");
            std::fs::create_dir_all(&elsewhere).unwrap();
            let e = elsewhere.to_string_lossy().to_string();
            rerun("the_store_ignores_git_variables_meant_for_another_repository", &[
                ("QH_TEST_BASE", &base.to_string_lossy()), ("GIT_DIR", &e), ("GIT_OBJECT_DIRECTORY", &e), ("GIT_ALTERNATE_OBJECT_DIRECTORIES", &e),
                ("GIT_CONFIG_COUNT", "1"), ("GIT_CONFIG_KEY_0", "qh.test"), ("GIT_CONFIG_VALUE_0", "leaked"), ("GIT_INDEX_FILE", &e),
            ]);
            assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing landed where the variables pointed");
            let _ = std::fs::remove_dir_all(&base);
            return;
        };
        let (store, root) = (Path::new(&base).join("store"), Path::new(&base).join("project"));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(board_store_in(&store, &root), store);
        let sha = identity_commit(&store, "alice", "a", "b").unwrap();
        assert_eq!(git(&store, &["cat-file", "-t", &sha]).as_deref(), Some("commit"));
        assert!(store.join("objects").join(&sha[..2]).join(&sha[2..]).exists(), "the object is in the store");
        assert!(git(&store, &["config", "qh.test"]).is_none(), "the caller's GIT_CONFIG_COUNT is dropped");
        // what we pass ourselves (the login, on a push or fetch) still arrives
        let ours = [("GIT_CONFIG_COUNT", "1".to_string()), ("GIT_CONFIG_KEY_0", "qh.test".to_string()), ("GIT_CONFIG_VALUE_0", "ours".to_string())];
        assert_eq!(git_in(&store, &["config", "qh.test"], None, &ours).as_deref(), Some("ours"));
    }

    #[test]
    fn the_list_of_local_git_variables_is_complete() {
        let all = Command::new("git").args(["rev-parse", "--local-env-vars"]).output().unwrap();
        for v in String::from_utf8_lossy(&all.stdout).lines() {
            assert!(GIT_LOCAL_ENV.contains(&v), "{v} is missing from GIT_LOCAL_ENV");
        }
    }

    fn rows(list: &[&str]) -> Vec<(String, String)> {
        list.iter().map(|t| ("user".to_string(), t.to_string())).collect()
    }

    /// What a sender would put in an update, from the rows it believes the watcher holds.
    fn update(old: &[(String, String)], new: &[(String, String)], seq: u64) -> Value {
        let (drop, keep) = rows_diff(old, new);
        json!({"session": "main", "seq": seq, "drop": drop, "keep": keep, "hash": rows_hash(new),
               "rows": new[keep..].iter().map(|(r, t)| json!([r, t])).collect::<Vec<_>>()})
    }

    #[test]
    fn a_watched_transcript_sends_only_what_changed() {
        assert_eq!(rows_diff(&rows(&["a", "b"]), &rows(&["a", "b", "c"])), (0, 2), "one more");
        assert_eq!(rows_diff(&rows(&["a", "b", "c"]), &rows(&["b", "c", "d"])), (1, 2), "the window moved on");
        assert_eq!(rows_diff(&rows(&["a", "b"]), &rows(&["a", "b2"])), (0, 1), "the last one changed");
        assert_eq!(rows_diff(&[], &rows(&["a"])), (0, 0), "first time, whole");
        assert_eq!(rows_diff(&rows(&["a"]), &rows(&["x", "y"])), (0, 0), "nothing in common");
        let mut s = state("bob");
        s.peers.insert("alice".into(), peer("alice", &["main"], true));
        let (one, two, three) = (rows(&["a", "b"]), rows(&["a", "b", "c"]), rows(&["b", "c", "d"]));
        assert!(take_rows(&mut s, "alice", &update(&[], &one, 10)));
        assert!(take_rows(&mut s, "alice", &update(&one, &two, 20)));
        assert_eq!(s.peers["alice"].transcripts["main"], two);
        assert!(!take_rows(&mut s, "alice", &update(&one, &two, 15)), "an older update, late");
        assert!(!s.watch_now);
        // one went missing: they don't guess, they ask again, and what they hold stays
        assert!(!take_rows(&mut s, "alice", &update(&rows(&["a", "b", "c", "z"]), &rows(&["a", "b", "c", "z", "y"]), 30)));
        assert!(s.watch_now);
        assert_eq!(s.peers["alice"].transcripts["main"], two);
        assert!(take_rows(&mut s, "alice", &update(&two, &three, 40)));
        assert_eq!(s.peers["alice"].transcripts["main"], three);
    }

    #[test]
    fn an_older_git_copy_doesnt_undo_a_live_transcript() {
        let repo = format!("req/live-tr{}", std::process::id());
        let mut s = state("bob");
        s.repo = repo.clone();
        let mut alice = peer("alice", &["main"], true);
        alice.seq = 100;
        s.peers.insert("alice".into(), alice);
        assert!(take_rows(&mut s, "alice", &update(&[], &rows(&["new"]), 500)));
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.clone(), s);
        let from_git = |seq: u64, text: &str| {
            let mut p = peer("alice", &["main"], true);
            p.seq = seq;
            p.transcripts.insert("main".into(), rows(&[text]));
            p
        };
        take_in(&repo, &"bob".to_string(), None, from_git(200, "old"), &quiet(), &mut false);
        assert_eq!(with(&repo, |s| s.peers["alice"].transcripts["main"].clone()).unwrap(), rows(&["new"]), "made before the live one");
        take_in(&repo, &"bob".to_string(), None, from_git(900, "newer"), &quiet(), &mut false);
        assert_eq!(with(&repo, |s| s.peers["alice"].transcripts["main"].clone()).unwrap(), rows(&["newer"]), "made after it");
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).remove(&repo);
    }

    fn quiet() -> Hooks {
        Hooks { say: Box::new(|_| {}), deliver: Box::new(|_, _| {}), ask: Box::new(|_| {}), refresh: Box::new(|| {}),
                answer: std::sync::Arc::new(|_| {}), answered: Box::new(|_, _| {}), changed: Box::new(|_| {}) }
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
        assert!(p.signed && vet(repo, "", &p, &quiet(), &AtomicBool::new(false)), "alice's own folder is taken (and her keys pinned)");
        let (_, p) = folder(&root, repo, "alice", &mallory, &[("data", "x")], t + 1);
        assert!(p.signed, "mallory's signature is valid for mallory's keys...");
        assert!(!vet(repo, "", &p, &quiet(), &AtomicBool::new(false)), "...but they aren't alice's, and GitHub doesn't vouch for them");

        // mallory keeps alice's keys and signature but swaps the sealed data
        let (f, _) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 2);
        let mut files = f.files.clone();
        for (n, c) in files.iter_mut() { if n == "data" { *c = tc::seal(&team, "data:alice", b"{}") } }
        let sha = folder_commit(&root, &files, &serde_json::from_str(&f.msg).unwrap(), &identity_commit(&root, "alice", "a", "b").unwrap()).unwrap();
        let p = read_folder(&folder_files(&root, &sha), repo, "alice", None);
        assert!(!p.signed && !vet(repo, "", &p, &quiet(), &AtomicBool::new(false)), "a changed file breaks alice's signature");

        let (_, p) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 5);
        assert!(vet(repo, "", &p, &quiet(), &AtomicBool::new(false)), "alice's next folder");
        let (_, p) = folder(&root, repo, "alice", &alice, &[("data", "x")], t + 3);
        assert!(!vet(repo, "", &p, &quiet(), &AtomicBool::new(false)), "an older folder of alice's, pushed back by someone: a replay");

        // a team key change from a member whose keys GitHub hasn't vouched for is ignored
        let bob = x25519_dalek::StaticSecret::from(crate::oauth::random());
        let bob_pub = crate::oauth::b64url(x25519_dalek::PublicKey::from(&bob).as_bytes());
        let new = tc::new_team_key();
        let wrap = tc::wrap(&new, &alice, &bob_pub, repo).unwrap();
        let (f, p) = folder(&root, repo, "alice", &alice, &[("epoch", "1"), ("keys/bob", &wrap)], t + 6);
        assert!(vet(repo, "", &p, &quiet(), &AtomicBool::new(false)));
        newer_key(repo, &[(f, p)], &bob, "", &quiet(), &AtomicBool::new(false));
        assert_eq!(with(repo, |s| (s.key, s.epoch)).unwrap(), (Some(team), 0), "the key stays");
        let _ = std::fs::remove_dir_all(&root);
    }

    fn peer(login: &str, sessions: &[&str], online: bool) -> Peer {
        Peer { login: login.into(), online, joined: true, ts: now(), qh: crate::update::VERSION.into(),
               sessions: sessions.iter().map(|s| (s.to_string(), "idle".into(), String::new())).collect(), ..Default::default() }
    }

    #[test]
    fn claims_warn_lets_the_edit_through_and_says_so_once() {
        let repo = "o/claims-warn";
        let mut s = state("alice");
        s.repo = repo.into();
        let mut bob = peer("bob", &["api"], true);
        bob.claims = HashMap::from([("api".to_string(), HashMap::from([("api/user.rs".to_string(), HashMap::from([("get_user".to_string(), now() - 30)]))]))]);
        s.peers.insert("bob".into(), bob);
        let mut ann = peer("ann", &[], false); // an old unmerged change: the person on it now is named first
        ann.unmerged = HashMap::from([("api/user.rs".into(), HashMap::from([("get_user".to_string(), now() - 86400)]))]);
        s.peers.insert("ann".into(), ann);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), s);
        let t = |f: &str| Touched { repo: repo.into(), root: std::env::temp_dir(), file: "api/user.rs".into(), funcs: vec![f.into()],
            removed: vec![], uses: vec![], bodies: vec![] };
        let (line, note, held) = check_edit("main", &t("get_user"), false).expect("told");
        assert!(!held && line.contains("bob is changing get_user") && note.contains("edit was made"), "{line} / {note}");
        assert!(check_edit("main", &t("get_user"), false).is_none(), "said once");
        assert!(with(repo, |s| s.waiting.is_empty()).unwrap(), "warn doesn't queue a wait");
        let (_, _, held) = check_edit("main", &t("get_user"), true).expect("lock still holds");
        assert!(held);
        // warn softens the pause, not the contract: a rename that breaks bob's fresh code is held
        with(repo, |s| s.peers.get_mut("bob").unwrap().uses = HashMap::from([("api".to_string(), HashMap::from([("get_user".to_string(), now())]))]));
        let mut gone = t("get_user");
        gone.removed = vec!["get_user".into()];
        let (_, err, held) = check_edit("main", &gone, false).expect("contract checked in warn mode");
        assert!(held && err.contains("removes or renames get_user"), "{err}");
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    #[test]
    fn an_edit_is_held_once_when_an_offline_teammate_changed_the_same_function() {
        let repo = "o/held-unmerged";
        let mut s = state("alice");
        s.repo = repo.into();
        let mut bob = peer("bob", &[], false);
        bob.unmerged = HashMap::from([("api/user.rs".into(), HashMap::from([("get_user".to_string(), now() - 3 * 86400)]))]);
        s.peers.insert("bob".into(), bob);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), s);
        let t = |f: &str| Touched { repo: repo.into(), root: std::env::temp_dir(), file: "api/user.rs".into(), funcs: vec![f.into()],
            removed: vec![], uses: vec![], bodies: vec![] };
        assert!(check_edit("main", &t("list_users"), true).is_none(), "another function: fine");
        let (line, err, held) = check_edit("main", &t("get_user"), true).expect("held");
        assert!(held);
        assert!(line.contains("bob also changed get_user") && err.contains("NOT made") && err.contains("first seen 3d ago"), "{line} / {err}");
        assert!(check_edit("main", &t("get_user"), true).is_none(), "said once; a retry goes ahead");
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    #[test]
    fn overlaps_are_the_same_function_in_both_unmerged_copies() {
        let mut s = state("alice");
        s.unmerged = HashMap::from([("a.rs".into(), HashMap::from([("one".to_string(), 1), ("two".to_string(), 1)])),
                                    ("new.txt".into(), HashMap::from([(String::new(), 1)]))]);
        let mut bob = peer("bob", &[], false);
        bob.unmerged = HashMap::from([("a.rs".into(), HashMap::from([("two".to_string(), 1), ("three".to_string(), 1)])),
                                      ("new.txt".into(), HashMap::from([(String::new(), 1)]))]);
        s.peers.insert("bob".into(), bob);
        let v = overlaps(&s);
        assert_eq!(overlap_text(&v), "bob: two in a.rs; bob: new.txt");
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
    fn a_teammate_on_an_older_qh_gets_plain_lines_not_status_cards() {
        let ((a, a_pub), (b, b_pub)) = (member("alice"), member("bob"));
        let mut s = keyed("alice", &a, &[("bob", &b_pub)]);
        s.peers.get_mut("bob").unwrap().qh = "0.2.4".into();
        let work = |id| Incoming { from: "bob".into(), id, session: "main".into(), text: "build the backend".into(), ts: now(), ..Default::default() };
        s.pending = vec![work(1), work(2), work(3)];
        decide_locked(&mut s, "bob", 1, "once").unwrap();
        reply_request_locked(&mut s, "bob", 2, "which framework?").unwrap();
        decide_why_locked(&mut s, "bob", 3, "deny", "no time");
        assert!(!s.outbox.iter().any(|m| m["kind"] == "s"), "0.2.4 would show these as empty requests: {:?}", s.outbox);
        let mut bob = keyed("bob", &b, &[("alice", &a_pub)]);
        take_private(&mut bob, "alice", &published(&s, &a_pub));
        let got: Vec<String> = bob.msgs.iter().map(|m| m.text.clone()).collect();
        assert_eq!(got, vec!["alice re your request \"build the backend\": which framework?".to_string(),
                             "alice declined: \"build the backend\": no time".to_string()]);
        // nor a withdraw notice
        s.sent.push(Outgoing { id: 9, to: "bob".into(), history: vec![(Status::Queued, now())], ..Default::default() });
        withdraw_locked(&mut s, 9).unwrap();
        assert!(!s.outbox.iter().any(|m| m["kind"] == "w"));
    }

    #[test]
    fn a_resend_goes_under_a_new_id_with_its_packet_and_isnt_doubled() {
        let mut alice = state("alice");
        alice.next_id = 10;
        let packet = json!({"done": "parser", "left": "tests", "files": ["src/p.rs"]});
        alice.sent.push(Outgoing { id: 4, to: "bob".into(), session: "main".into(), text: "finish it".into(),
                                   history: vec![(Status::Delivered, now())], packet: packet.clone(), ..Default::default() });
        resend_locked(&mut alice, 4).unwrap();
        let m = alice.outbox.last().unwrap().clone();
        assert_eq!((m["id"].as_u64(), m["packet"].clone(), m["was"].clone()), (Some(11), packet, json!([4])));
        assert_eq!((alice.sent[0].id, alice.sent[0].was.clone()), (11, vec![4]));
        let o = outgoing_of(&outgoing_json(&alice.sent[0]));
        assert!(o.packet.is_object() && o.was == vec![4], "kept across a restart");
        resend_locked(&mut alice, 11).unwrap();
        assert_eq!(alice.outbox.len(), 1, "still on the line: not sent twice");
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
        // a handover: the agent gets its packet, and the requester's "finished" line still quotes only the request
        let mut h = inc.clone();
        h.packet = json!({"done": ["routes"], "left": ["add the test"], "watch": "single constant", "files": ["web.py"], "branch": "wip"});
        let said = accepted_text(&h);
        for part in ["branch wip", "- routes", "- add the test", "single constant", "Files: web.py", "Start with: add the test"] {
            assert!(said.contains(part), "{part} missing from {said}");
        }
        assert_eq!(said.split("\n\n[").next().unwrap(), accepted_text(&inc));
        assert_eq!(done_text(&inc.text, " built it "), "finished your request \"build the backend\": built it");
        assert!(done_text(&inc.text, "interrupted").contains("stopped your request"));
        assert!(s.my_dms.is_empty());
        let last = |s: &State| { let m = s.outbox.last().unwrap(); (m["kind"].as_str().unwrap().to_string(), m["re"].as_u64().unwrap(),
                                                                  m["status"].as_str().unwrap().to_string(), m["text"].as_str().unwrap().to_string()) };
        assert_eq!(last(&s), ("s".into(), 1, "accepted".into(), "".into()), "the sender learns it was accepted");
        // reply: on the request itself, not a DM; the request still waits
        let r = reply_request_locked(&mut s, "bob", 2, "which framework?").unwrap();
        assert!(r.contains("still waits"), "{r}");
        assert!(s.pending.iter().any(|p| p.id == 2 && p.replied));
        assert_eq!(last(&s), ("s".into(), 2, "replied".into(), "which framework?".into()));
        // decline: gone, and they're told
        assert!(decide_locked(&mut s, "bob", 3, "deny").is_some());
        assert!(!s.pending.iter().any(|p| p.id == 3));
        assert_eq!(last(&s), ("s".into(), 3, "declined".into(), "".into()));
        assert!(read(&s).is_empty(), "no DMs: {:?}", read(&s));
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
    fn a_cleared_thread_stays_cleared_when_their_folder_comes_again() {
        let ((a, a_pub), (b, b_pub)) = (member("alice"), member("bob"));
        let mut alice = keyed("alice", &a, &[("bob", &b_pub)]);
        dm_locked(&mut alice, "bob", "ping-1").unwrap();
        let folder = published(&alice, &a_pub);
        let mut bob = keyed("bob", &b, &[("alice", &a_pub)]);
        assert!(take_private(&mut bob, "alice", &folder));
        let t = bob.msgs.iter().map(|m| m.ts).max().unwrap();
        bob.cleared.insert("@alice".into(), t);
        bob.msgs.retain(|m| m.chan != "@alice");
        bob.msg_ids.clear(); // as after a restart
        assert!(!take_private(&mut bob, "alice", &folder) && bob.msgs.is_empty(), "the old line doesn't come back");
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

    #[test]
    fn a_line_over_the_direct_line_and_then_the_board_shows_once_in_order() {
        let repo = "t/quick-chat";
        let mut st = state("alice");
        st.repo = repo.into();
        st.chat_seen.insert("bob".into(), 0); // bob's folder was read before: new lines count as unread
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), st);
        // bob's third line comes over the line first, twice (a resend)
        let third = json!({"chan": "#team", "id": 3000, "ts": 30, "text": "third"});
        assert_eq!(with(repo, |s| take_quick(s, "bob", &third)), Some(true));
        assert_eq!(with(repo, |s| take_quick(s, "bob", &third)), Some(false), "the same id again: dropped");
        // then his folder from the board, with all three
        let mut bob = peer("bob", &["main"], true);
        bob.chat = vec![(1000, 10, "first".into()), (2000, 20, "second".into()), (3000, 30, "third".into())];
        let mut nc = false;
        take_in(repo, &"alice".to_string(), None, bob.clone(), &quiet(), &mut nc);
        let (chat, unread) = with(repo, |s| (s.chat.iter().map(|(_, _, t)| t.clone()).collect::<Vec<_>>(), s.unread)).unwrap();
        assert_eq!(chat, ["first", "second", "third"], "the late board lines slot in before the quick one");
        assert_eq!(unread, 3);
        // the same folder again, and the quick copy once more: nothing new
        take_in(repo, &"alice".to_string(), None, bob, &quiet(), &mut nc);
        assert_eq!(with(repo, |s| take_quick(s, "bob", &third)), Some(false), "taken from the board already");
        let older = json!({"chan": "#team", "id": 2000, "ts": 20, "text": "second"});
        assert_eq!(with(repo, |s| take_quick(s, "bob", &older)), Some(false));
        assert_eq!(with(repo, |s| (s.chat.len(), s.unread)).unwrap(), (3, 3));
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }

    #[test]
    fn our_chat_ids_only_grow() {
        let mut s = state("alice");
        let ids: Vec<u64> = (0..5).map(|i| chat_locked(&mut s, &format!("line {i}"))["id"].as_u64().unwrap()).collect();
        assert!(ids.windows(2).all(|w| w[1] > w[0]), "{ids:?}");
        assert_eq!(s.chat.len(), 5);
        assert!(s.dirty, "the board still gets it");
    }

    #[test]
    fn a_dm_over_the_direct_line_is_not_taken_twice_from_the_board() {
        let ((a, a_pub), (b, b_pub), (c, c_pub)) = (member("alice"), member("bob"), member("carol"));
        let mut alice = keyed("alice", &a, &[("bob", &b_pub), ("carol", &c_pub)]);
        let quick = dm_locked(&mut alice, "bob", "the key is in the vault").unwrap();
        assert_eq!(quick["to"], json!(["bob"]), "a DM goes over the line to bob alone");
        assert!(!quick.to_string().contains("vault"), "sealed for bob, also on the line");
        let mut bob = keyed("bob", &b, &[("alice", &a_pub), ("carol", &c_pub)]);
        assert!(take_quick(&mut bob, "alice", &quick));
        assert!(!take_quick(&mut bob, "alice", &quick));
        assert!(!take_private(&mut bob, "alice", &published(&alice, &a_pub)), "the board copy is the same line");
        assert_eq!(bob.msgs.len(), 1);
        assert_eq!(bob.chan_unread.get("@alice"), Some(&1));
        // carol can't open it, whichever way it came
        let mut carol = keyed("carol", &c, &[("alice", &a_pub), ("bob", &b_pub)]);
        assert!(!take_quick(&mut carol, "alice", &quick));
        // a group line: to its members, once
        group_locked(&mut alice, "core", &["bob".into()]).unwrap();
        let gid = alice.groups.keys().next().unwrap().clone();
        assert!(take_private(&mut bob, "alice", &published(&alice, &a_pub)), "bob learns the group from the board");
        let g = group_send_locked(&mut alice, &gid, "standup?").unwrap();
        assert_eq!(g["to"], json!(["bob"]));
        assert!(take_quick(&mut bob, "alice", &g));
        assert!(!take_private(&mut bob, "alice", &published(&alice, &a_pub)));
        assert_eq!(bob.msgs.iter().filter(|m| m.text == "standup?").count(), 1);
    }

    #[test]
    fn a_review_request_goes_to_the_owners_busiest_session_and_its_answer_comes_back() {
        let repo = "t/review";
        let mut st = state("alice");
        st.repo = repo.into();
        st.key = Some([7u8; 32]);
        let mut bob = peer("bob", &["main", "api"], false);
        bob.claims.insert("api".into(), HashMap::from([("src/a.rs".to_string(), HashMap::new())]));
        st.peers.insert("bob".into(), bob);
        STATES.lock().unwrap().get_or_insert_with(HashMap::new).insert(repo.into(), st);
        let (id, to, online) = ask_review(repo, "main", "bob", "[review request] …").unwrap();
        assert_eq!((to.as_str(), online), ("bob/api", false), "offline: it waits on the board");
        assert!(with(repo, |s| s.dirty && s.outbox.iter().any(|m| m["id"] == id && m["kind"] == "q")).unwrap());
        assert!(ask_review(repo, "main", "nobody", "x").is_err());
        // track it the way review::request does, then bob's answer arrives in his folder
        let hooks = quiet();
        crate::review::tests_track(id, repo);
        let mut bob = peer("bob", &["main", "api"], true);
        bob.outbox = vec![json!({"id": 50, "to": "alice", "session": "main", "kind": "a", "re": id, "from_session": "api", "text": "APPROVE: fine", "ts": now()})];
        let mut nc = false;
        take_in(repo, &"alice".to_string(), Some([7u8; 32]), bob, &hooks, &mut nc);
        // the team room (activity) shows it
        let said: Vec<String> = ACTIVITY.lock().unwrap().iter().filter(|(_, r, _)| r == repo).map(|(_, _, t)| t.clone()).collect();
        assert!(said.iter().any(|t| t.starts_with("review from bob/api (approve): APPROVE: fine")), "{said:?}");
        assert!(with(repo, |s| s.asked.is_empty()).unwrap());
        STATES.lock().unwrap().as_mut().unwrap().remove(repo);
    }
}

