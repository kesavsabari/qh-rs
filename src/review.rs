//! Review requests: before a push, ask the agent of whoever owns the changed code to look at it.
//!
//! Who owns a changed line: a `CODEOWNERS` rule when one matches the file (GitHub syntax, last
//! matching rule wins), otherwise the last author of that line on the base (`git blame`), mapped
//! from their git name / email to a team member's GitHub login. We never ask ourselves. Each owner
//! gets the diff hunks in their area as a question to their agent (team.rs carries it: the direct
//! line when they're on it, the board otherwise); the answer shows in the team room, as a notice
//! and as a `review` event for `qh serve`. The push waits at most `review_wait` seconds, then goes
//! ahead with a notice; an answer that comes later still shows up. `review = "off"` turns it off.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HUNKS_MAX: usize = 12 * 1024; // diff text per owner: their agent reads it all, so keep it a few pages

// ---- CODEOWNERS ----

/// The rules of a CODEOWNERS file, in order: (pattern, owners). A pattern with no owners is
/// legal and means "nobody owns this" (it undoes an earlier rule).
pub fn parse_codeowners(text: &str) -> Vec<(String, Vec<String>)> {
    text.lines().filter_map(|l| {
        let l = l.split_once(" #").map_or(l, |(a, _)| a).trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with('[') { return None } // [Section] lines are GitLab's
        let mut w = l.split_whitespace();
        let pat = w.next()?.replace("\\#", "#");
        Some((pat, w.map(String::from).collect()))
    }).collect()
}

/// Whether a CODEOWNERS pattern covers `path` (relative to the repo, `/`-separated): gitignore
/// style. `/x` is anchored at the root, a pattern without an inner `/` matches at any depth, `*`
/// stays within one folder, `**` crosses folders, and a folder pattern covers everything in it.
pub fn pattern_matches(pat: &str, path: &str) -> bool {
    let dir_only = pat.ends_with('/');
    let p = pat.trim_end_matches('/');
    if p.is_empty() { return false }
    let anchored = p.starts_with('/') || p.trim_start_matches('/').contains('/');
    let p = p.trim_start_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    // try the pattern against every prefix of the path (it matches a file or a folder above it),
    // starting at the root only when anchored
    let starts: Vec<usize> = if anchored { vec![0] } else { (0..parts.len()).collect() };
    for s in starts {
        for e in s + 1..=parts.len() {
            if dir_only && e == parts.len() { continue } // "docs/" means the folder, not a file named docs
            if glob(p, &parts[s..e].join("/")) { return true }
        }
    }
    false
}

/// `*` (not `/`), `?`, and `**` (anything, `/` included).
fn glob(p: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = p[2..].strip_prefix(b"/").unwrap_or(&p[2..]);
                (0..=s.len()).any(|i| go(rest, &s[i..])) || (0..=s.len()).any(|i| go(&p[2..], &s[i..]))
            }
            Some(b'*') => (0..=s.len()).take_while(|&i| i == 0 || s[i - 1] != b'/').any(|i| go(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && s[0] != b'/' && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(p.as_bytes(), s.as_bytes())
}

/// The owners CODEOWNERS gives `path`: the last matching rule's. None when no rule matches.
pub fn codeowners_for<'a>(rules: &'a [(String, Vec<String>)], path: &str) -> Option<&'a [String]> {
    rules.iter().rev().find(|(p, _)| pattern_matches(p, path)).map(|(_, o)| o.as_slice())
}

/// The project's CODEOWNERS, where GitHub looks for it (.github/, the root, docs/).
fn load_codeowners(root: &Path) -> Vec<(String, Vec<String>)> {
    [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"].iter()
        .find_map(|f| std::fs::read_to_string(root.join(f)).ok())
        .map(|t| parse_codeowners(&t)).unwrap_or_default()
}

// ---- who is who ----

/// The team member a git author is, from their name and email: a GitHub noreply address
/// (`123+login@users.noreply.github.com`), or a login equal to the email's local part or the name.
pub fn member_of(name: &str, email: &str, members: &[String]) -> Option<String> {
    let email = email.trim().trim_matches(['<', '>']).to_lowercase();
    let (local, domain) = email.split_once('@').unwrap_or((&email, ""));
    let cand: Vec<String> = if domain == "users.noreply.github.com" {
        vec![local.rsplit('+').next().unwrap_or(local).to_string()]
    } else {
        vec![local.to_string(), name.trim().to_lowercase(), name.trim().to_lowercase().replace(' ', "")]
    };
    cand.iter().filter(|c| !c.is_empty())
        .find_map(|c| members.iter().find(|m| m.eq_ignore_ascii_case(c)).cloned())
}

/// A CODEOWNERS owner as a member: `@login`, or an email. `@org/team` names a GitHub team,
/// which we can't expand here: skipped.
pub fn owner_member(owner: &str, members: &[String]) -> Option<String> {
    match owner.strip_prefix('@') {
        Some(l) if !l.contains('/') => members.iter().find(|m| m.eq_ignore_ascii_case(l)).cloned(),
        Some(_) => None,
        None if owner.contains('@') => member_of("", owner, members),
        None => None,
    }
}

// ---- the diff and its owners ----

/// One hunk of the diff being pushed: its file, the base lines it changes (1-based; for a pure
/// addition, the base lines around it), and its text.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    pub file: String,
    pub old: Vec<usize>,
    pub text: String,
}

/// The hunks of a `git diff` (unified, any context). New files have no base lines.
pub fn parse_diff(diff: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = vec![];
    let (mut file, mut old_file) = (String::new(), String::new());
    let mut line = 0usize;
    let mut replacing = false; // added lines right after removed ones replace them: the removed lines say whose
    for l in diff.lines() {
        if l.starts_with("diff --git ") { file.clear(); old_file.clear(); continue }
        if let Some(f) = l.strip_prefix("--- ") { old_file = f.strip_prefix("a/").unwrap_or(f).to_string(); continue }
        if let Some(f) = l.strip_prefix("+++ ") {
            file = f.strip_prefix("b/").unwrap_or(f).to_string();
            if file == "/dev/null" { file = old_file.clone() } // a deleted file: it's still in their area
            continue;
        }
        if let Some(h) = l.strip_prefix("@@ -") {
            line = h.split([',', ' ']).next().and_then(|n| n.parse().ok()).unwrap_or(0);
            replacing = false;
            out.push(Hunk { file: file.clone(), old: vec![], text: format!("{l}\n") });
            continue;
        }
        let Some(h) = out.last_mut().filter(|h| h.file == file && !file.is_empty()) else { continue };
        h.text.push_str(l);
        h.text.push('\n');
        match l.as_bytes().first() {
            Some(b'-') => { h.old.push(line); line += 1; replacing = true }
            Some(b'+') => if old_file != "/dev/null" && !replacing {
                // an added line belongs with the base lines on either side of it
                for n in [line.saturating_sub(1), line] { if n > 0 && !h.old.contains(&n) { h.old.push(n) } }
            },
            Some(b'\\') => {}
            _ => { line += 1; replacing = false }
        }
    }
    out.retain(|h| !h.file.is_empty());
    out
}

/// `git blame --line-porcelain`: base line number -> (commit, author name, author email).
pub fn parse_blame(text: &str) -> HashMap<usize, (String, String, String)> {
    let mut out = HashMap::new();
    let (mut sha, mut at, mut name) = (String::new(), 0usize, String::new());
    for l in text.lines() {
        if let Some(n) = l.strip_prefix("author ") { name = n.to_string(); continue }
        if let Some(m) = l.strip_prefix("author-mail ") {
            out.insert(at, (sha.clone(), name.clone(), m.trim_matches(['<', '>']).to_string()));
            continue;
        }
        let w: Vec<&str> = l.split(' ').collect();
        if w.len() >= 3 && w[0].len() >= 40 && w[0].bytes().all(|b| b.is_ascii_hexdigit()) {
            sha = w[0].to_string();
            at = w[2].parse().unwrap_or(0);
        }
    }
    out
}

/// Who owns each hunk, grouped by owner: CODEOWNERS when a rule matches its file, else whoever
/// last wrote most of the base lines it changes. `who(name, email, commit)` maps a git author to
/// a member login. We (`me`) are never an owner; neither is anyone outside the team.
pub fn owners(hunks: &[Hunk], rules: &[(String, Vec<String>)], blame: &dyn Fn(&str) -> HashMap<usize, (String, String, String)>,
              who: &dyn Fn(&str, &str, &str) -> Option<String>, members: &[String], me: &str) -> Vec<(String, Vec<Hunk>)> {
    let mut by: Vec<(String, Vec<Hunk>)> = vec![];
    let mut blames: HashMap<String, HashMap<usize, (String, String, String)>> = HashMap::new();
    for h in hunks {
        let named: Vec<String> = match codeowners_for(rules, &h.file) {
            Some(o) => o.iter().filter_map(|x| owner_member(x, members)).collect(),
            None => {
                let b = blames.entry(h.file.clone()).or_insert_with(|| blame(&h.file));
                let mut count: Vec<(String, usize)> = vec![];
                for n in &h.old {
                    let Some((sha, name, email)) = b.get(n) else { continue };
                    let Some(l) = who(name, email, sha) else { continue };
                    match count.iter_mut().find(|(x, _)| *x == l) { Some(c) => c.1 += 1, None => count.push((l, 1)) }
                }
                count.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
                count.into_iter().take(1).map(|(l, _)| l).collect()
            }
        };
        for o in named.into_iter().filter(|o| !o.eq_ignore_ascii_case(me)) {
            match by.iter_mut().find(|(x, _)| *x == o) {
                Some((_, v)) => if !v.contains(h) { v.push(h.clone()) },
                None => by.push((o, vec![h.clone()])),
            }
        }
    }
    by
}

/// The question an owner's agent gets: the hunks in their area, clipped to a few pages.
pub fn request_text(me: &str, hunks: &[Hunk]) -> String {
    let mut files: Vec<&str> = hunks.iter().map(|h| h.file.as_str()).collect();
    files.dedup();
    let mut body = String::new();
    let mut last = "";
    let mut cut = 0;
    for h in hunks {
        if body.len() + h.text.len() > HUNKS_MAX { cut += h.text.lines().count(); continue }
        if h.file != last { body.push_str(&format!("--- {}\n", h.file)); last = &h.file }
        body.push_str(&h.text);
    }
    if cut > 0 { body.push_str(&format!("({cut} more diff lines left out: ask for them with read_team_file)\n")) }
    format!("[review request] {me} is about to push changes to code you own ({}). Review the diff below. \
Start your reply with APPROVE (fine to push), CHANGES (what must change first), or HANDOFF when it needs \
your person to decide: say why. Prefer HANDOFF over CHANGES when the diff is a product, pricing, policy or design \
decision that isn't wrong as code (only a person can say whether it's intended), or when it's too big or unclear \
to judge from the diff. Then \
short, concrete comments (file:line, what and why). Only judge what's in the diff against what you know of this code.\n\n```diff\n{body}```",
            files.join(", "))
}

/// approve | changes | comments, from the start of an answer.
pub fn verdict(answer: &str) -> &'static str {
    let a = answer.trim_start().trim_start_matches(['*', '#', '>', ' ']).to_uppercase();
    if a.starts_with("(NOT ANSWERED") || a.starts_with("(COULDN'T") { "none" }
    else if a.starts_with("APPROVE") || a.starts_with("LGTM") { "approve" }
    else if a.starts_with("CHANGES") || a.starts_with("REQUEST CHANGES") { "changes" }
    else if a.starts_with("HANDOFF") || a.starts_with("HAND OFF") { "handoff" }
    else { "comments" }
}

// ---- requests in flight ----

#[derive(Clone, Debug)]
pub struct Review {
    pub id: u64,       // the question's id in team.rs
    pub repo: String,
    pub session: String,
    pub to: String,    // login/session
    pub files: Vec<String>,
    pub head: String,  // the commit asked about: a retried push of it reuses this review
    pub answer: Option<String>,
    pub ts: u64,
    pub gave_up: bool, // the push stopped waiting: a later answer goes to the session as a message
}

static REVIEWS: Mutex<Vec<Review>> = Mutex::new(Vec::new());
static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn track(r: Review) {
    let mut v = REVIEWS.lock().unwrap();
    v.push(r);
    let n = v.len();
    if n > 50 { v.drain(..n - 50); }
}

fn json_of(r: &Review) -> Value {
    json!({"id": r.id, "repo": r.repo, "tab": r.session, "to": r.to, "files": r.files, "ts": r.ts,
           "status": r.answer.as_deref().map_or(if r.gave_up { "waiting (push went ahead)" } else { "waiting" }, verdict),
           "answer": r.answer})
}

fn event(r: &Review) {
    let mut v = json_of(r);
    v["type"] = json!("review");
    let mut e = EVENTS.lock().unwrap();
    e.push(v);
    let n = e.len();
    if n > 50 { e.drain(..n - 50); }
    crate::team::bump();
}

/// An answer to question `re` came from `from` (team.rs calls this for every answer). None when
/// it isn't a review; Some(late) when it is: late = the push already went ahead, so the session
/// should get it as a message.
pub fn answered(re: u64, from: &str, text: &str) -> Option<bool> {
    let mut v = REVIEWS.lock().unwrap();
    let r = v.iter_mut().find(|r| r.id == re && r.answer.is_none())?;
    r.answer = Some(text.to_string());
    r.to = from.to_string();
    let (late, snap) = (r.gave_up, r.clone());
    drop(v);
    event(&snap);
    Some(late)
}

/// Recent review requests, newest first (for the team room in `qh serve`).
pub fn list() -> Vec<Value> {
    let repo = crate::team::repo().unwrap_or_default();
    REVIEWS.lock().unwrap().iter().rev().filter(|r| r.repo == repo).take(20).map(json_of).collect()
}

/// `review` events not yet sent to `qh serve` clients.
pub fn take_events() -> Vec<Value> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

/// Wait until every review in `ids` is answered, `wait` passes, or `stop()`; then mark the
/// unanswered ones as given up. Returns the (to, answer) pairs that came in.
pub fn wait_for(ids: &[u64], wait: Duration, stop: &dyn Fn() -> bool) -> Vec<(String, Option<String>)> {
    until(ids, wait, stop, &|| false);
    collect(ids)
}

/// Wait until every review in `ids` is answered or `stop()`; past `wait`, only while `more()` says
/// one is still being worked on, and never past REVIEW_CAP.
fn until(ids: &[u64], wait: Duration, stop: &dyn Fn() -> bool, more: &dyn Fn() -> bool) {
    let t0 = Instant::now();
    loop {
        let done = { let v = REVIEWS.lock().unwrap(); ids.iter().all(|id| v.iter().any(|r| r.id == *id && r.answer.is_some())) };
        let el = t0.elapsed();
        if done || stop() || el >= REVIEW_CAP || (el >= wait && !more()) { break }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The (to, answer) pairs so far; the unanswered ones are marked as given up.
fn collect(ids: &[u64]) -> Vec<(String, Option<String>)> {
    let mut v = REVIEWS.lock().unwrap();
    v.iter_mut().filter(|r| ids.contains(&r.id)).map(|r| {
        if r.answer.is_none() { r.gave_up = true }
        (r.to.clone(), r.answer.clone())
    }).collect()
}

/// However long a reviewer who has the request and is online takes, a push waits no longer.
const REVIEW_CAP: Duration = Duration::from_secs(300);

// ---- the whole request ----

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
}

/// What the push is measured against: where our upstream is, else the remote's default branch.
fn base(root: &Path) -> Option<String> {
    ["@{upstream}", "origin/HEAD", "origin/main", "origin/master"].iter()
        .find_map(|r| git(root, &["merge-base", "HEAD", r]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
}

/// Whether a bash command pushes with git (`git push`, `git -C dir push …`, in a chain too).
pub fn is_push(cmd: &str) -> bool {
    is_git(cmd, "push")
}

/// Whether a shell command runs `git commit`.
pub fn is_commit(cmd: &str) -> bool {
    is_git(cmd, "commit")
}

fn is_git(cmd: &str, sub: &str) -> bool {
    cmd.split(['&', ';', '|', '\n']).any(|seg| {
        let w: Vec<&str> = seg.split_whitespace().collect();
        let Some(g) = w.iter().position(|x| *x == "git" || x.ends_with("/git")) else { return false };
        // skip git's own options (and -C/-c's argument) to find the subcommand
        let mut i = g + 1;
        while i < w.len() && w[i].starts_with('-') { i += if matches!(w[i], "-C" | "-c") { 2 } else { 1 } }
        w.get(i) == Some(&sub)
    })
}

/// GitHub's login for a commit's author (cached; None without a token or in tests).
fn github_author(repo: &str, sha: &str, email: &str) -> Option<String> {
    static CACHE: Mutex<Option<HashMap<String, Option<String>>>> = Mutex::new(None);
    if cfg!(test) || sha.is_empty() || sha.bytes().all(|b| b == b'0') { return None } // all zeros: not committed yet
    if let Some(hit) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(email) { return hit.clone() }
    let tok = crate::team::token()?;
    let login = crate::team::api("GET", &format!("/repos/{repo}/commits/{sha}"), &tok, None, None).ok()
        .and_then(|(code, _, v)| (code == 200).then(|| v["author"]["login"].as_str().map(String::from)).flatten());
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(email.to_string(), login.clone());
    login
}

/// Ask the owners of what we're about to push to review it. Returns the request ids and a line
/// for the user; Err when there's nothing to ask (with why).
pub fn request(session: &str, cwd: &Path) -> Result<(Vec<u64>, String), String> {
    let (root, repo) = crate::team::github_repo(cwd).ok_or("this folder isn't a team project")?;
    if !crate::team::running_for(&repo) { return Err("team mode is off for this project".into()) }
    let me = crate::team::me().unwrap_or_default();
    let members: Vec<String> = crate::team::people().into_iter().map(|p| p.0).collect();
    if members.is_empty() { return Err("nobody else is in the team yet".into()) }
    let base = base(&root).ok_or("couldn't tell what this push is measured against (no upstream or origin/HEAD)")?;
    let diff = git(&root, &["diff", "--no-color", "--no-ext-diff", "-U3", &base, "HEAD"]).unwrap_or_default();
    let hunks = parse_diff(&diff);
    if hunks.is_empty() { return Err("nothing to review: no committed changes since the base".into()) }
    let rules = load_codeowners(&root);
    let blame = |f: &str| git(&root, &["blame", "--line-porcelain", &base, "--", f]).map(|t| parse_blame(&t)).unwrap_or_default();
    let who = |name: &str, email: &str, sha: &str| member_of(name, email, &members)
        .or_else(|| github_author(&repo, sha, email).and_then(|l| members.iter().find(|m| m.eq_ignore_ascii_case(&l)).cloned()));
    let by = owners(&hunks, &rules, &blame, &who, &members, &me);
    if by.is_empty() { return Err("nothing to review: the changed code is yours, or nobody in the team owns it".into()) }
    let head = git(&root, &["rev-parse", "HEAD"]).unwrap_or_default().trim().to_string();
    let (mut ids, mut asked) = (vec![], vec![]);
    for (owner, hs) in &by {
        let mut files: Vec<String> = hs.iter().map(|h| h.file.clone()).collect();
        files.dedup();
        // a push retried after a failure (or a second /review-request) asks nobody twice
        let seen = REVIEWS.lock().unwrap().iter().rev().find(|r| r.repo == repo && r.head == head && !head.is_empty()
            && r.to.split('/').next() == Some(owner.as_str()) && now().saturating_sub(r.ts) < 3600).map(|r| (r.id, r.to.clone()));
        if let Some((id, to)) = seen {
            ids.push(id);
            asked.push(format!("{to} ({}, already asked about this commit)", files.join(", ")));
            continue;
        }
        match crate::team::ask_review(&repo, session, owner, &request_text(&me, hs)) {
            Ok((id, to, online)) => {
                track(Review { id, repo: repo.clone(), session: session.into(), to: to.clone(), files: files.clone(), head: head.clone(), answer: None, ts: now(), gave_up: false });
                event(REVIEWS.lock().unwrap().last().unwrap());
                ids.push(id);
                asked.push(format!("{to} ({}){}", files.join(", "), if online { "" } else { ", offline" }));
            }
            Err(e) => asked.push(format!("{owner}: not asked ({e})")),
        }
    }
    if ids.is_empty() { return Err(asked.join("; ")) }
    Ok((ids, format!("asked for review: {}", asked.join("; "))))
}

/// What came back before a push: the text for the agent, and who held it.
pub struct Outcome {
    pub text: String,
    /// (login/session, answer) of reviewers who asked for changes
    pub changes: Vec<(String, String)>,
    /// (login/session, answer) of reviewers who handed it to their person
    pub handoff: Vec<(String, String)>,
    pub files: Vec<String>,
    /// the commit reviewed
    pub head: String,
    /// the change itself, cut short, for the owner's decision card
    pub diff: String,
    /// the checkout and its owner/name, so a hold can be kept on disk for that project
    pub root: std::path::PathBuf,
    pub repo: String,
}

/// Commits the user chose to push anyway: a retried push of one doesn't ask again.
static PUSH_ANYWAY: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn push_anyway(head: &str) {
    if !head.is_empty() { PUSH_ANYWAY.lock().unwrap().push(head.to_string()) }
}

pub fn pushed_anyway(head: &str) -> bool {
    !head.is_empty() && PUSH_ANYWAY.lock().unwrap().iter().any(|h| h == head)
}

/// Before the agent's `git push`: ask, wait up to `review_wait`, and say what came back. None
/// when reviews are off or there's nothing to ask. CHANGES or HANDOFF holds the push (engine.rs).
pub fn before_push(session: &str, cwd: &Path, cfg: &crate::config::Config, notice: &dyn Fn(String), stop: &dyn Fn() -> bool) -> Option<Outcome> {
    if !cfg.review { return None }
    let (ids, line) = request(session, cwd).ok()?;
    notice(format!("{line} · waiting up to {}s before pushing", cfg.review_wait));
    // past review_wait, keep holding while an owner who has the request is online: their agent is on it
    let told = std::cell::Cell::new(false);
    let more = || {
        let on = { let v = REVIEWS.lock().unwrap(); v.iter().filter(|r| ids.contains(&r.id) && r.answer.is_none()).any(|r| crate::team::reviewing(r.id, &r.to)) };
        if on && !told.replace(true) { notice("still reviewing: their agent has it and they're online, so the push keeps waiting (5 min at most)".into()) }
        on
    };
    until(&ids, Duration::from_secs(cfg.review_wait), stop, &more);
    let got = collect(&ids);
    let text = summary(&got);
    notice(text.lines().next().unwrap_or("").to_string());
    let pick = |v: &str| got.iter().filter_map(|(t, a)| a.as_ref().filter(|a| verdict(a) == v).map(|a| (t.clone(), a.clone()))).collect();
    let (files, head) = {
        let v = REVIEWS.lock().unwrap();
        let mut f: Vec<String> = v.iter().filter(|r| ids.contains(&r.id)).flat_map(|r| r.files.clone()).collect();
        f.sort();
        f.dedup();
        (f, v.iter().find(|r| ids.contains(&r.id)).map(|r| r.head.clone()).unwrap_or_default())
    };
    let (root, repo) = crate::team::github_repo(cwd).unwrap_or_default();
    let diff = (!repo.is_empty()).then(|| base(&root)).flatten().and_then(|b| {
        let mut a = vec!["diff", "--no-color", "--no-ext-diff", "-U1", b.as_str(), "HEAD", "--"];
        a.extend(files.iter().map(|f| f.as_str()));
        git(&root, &a)
    }).map(|d| clip_diff(&d)).unwrap_or_default();
    Some(Outcome { text, changes: pick("changes"), handoff: pick("handoff"), files, head, diff, root, repo })
}

/// The private message that hands a held push to the owner themselves, in team chat. Its last
/// line tags it as a hold, so the owner's qh shows it as a card they can approve or send back.
pub fn handoff_text(me: &str, files: &[String], answer: &str, diff: &str, head: &str, session: &str) -> String {
    let a: String = answer.trim().chars().take(600).collect();
    let d = if diff.is_empty() { String::new() } else { format!("\n\n{diff}") };
    format!("[review] {me} wants to push changes to {} and needs you to decide. Your agent said: {a}{d}\n{HOLD} {head} {session}",
            if files.is_empty() { "your code".to_string() } else { files.join(", ") })
}

/// The diff for a decision card: headers and changed lines only, at most 30 lines.
fn clip_diff(d: &str) -> String {
    let keep: Vec<&str> = d.lines().filter(|l| l.starts_with("@@") || l.starts_with("+++ ") ||
        ((l.starts_with('+') || l.starts_with('-')) && !l.starts_with("--- ") && !l.starts_with("+++ "))).collect();
    let mut out: Vec<String> = keep.iter().take(30).map(|l| l.chars().take(160).collect()).collect();
    if keep.len() > 30 { out.push(format!("… {} more lines", keep.len() - 30)) }
    out.join("\n")
}

// ---- the owner decides a handed-off push ----
//
// The hold goes out as a private message tagged `#qh-hold <commit> <session>`; the owner answers
// with `#qh-decide <commit> approve|changes` (and a note). The pusher's qh only takes a decision
// from someone the push was handed to, for a commit it's holding: DMs are sealed between the two
// of them, so the sender is who they say.

const HOLD: &str = "#qh-hold";
const DECIDE: &str = "#qh-decide";

#[derive(Debug, PartialEq)]
pub enum Tag { Hold { head: String, session: String }, Decide { head: String, approve: bool, note: String } }

pub fn tag(text: &str) -> Option<Tag> {
    let last = text.trim_end().lines().last()?;
    if let Some(r) = last.strip_prefix(HOLD) {
        let mut w = r.split_whitespace();
        return Some(Tag::Hold { head: w.next()?.into(), session: w.next().unwrap_or("").into() });
    }
    let first = text.lines().next()?;
    let mut w = first.strip_prefix(DECIDE)?.split_whitespace();
    let head = w.next()?.to_string();
    let approve = match w.next()? { "approve" => true, "changes" => false, _ => return None };
    Some(Tag::Decide { head, approve, note: text.lines().skip(1).collect::<Vec<_>>().join("\n").trim().to_string() })
}

/// The owner's answer to a hold.
pub fn decision_text(head: &str, approve: bool, note: &str) -> String {
    format!("{DECIDE} {head} {}\n{}", if approve { "approve" } else { "changes" }, note.trim())
}

/// A tagged message as people read it: the hold without its tag, a decision in words.
pub fn shown(text: &str) -> String {
    match tag(text) {
        Some(Tag::Hold { .. }) => text.trim_end().rsplit_once('\n').map_or(text, |(a, _)| a).to_string(),
        Some(Tag::Decide { head, approve, note }) => {
            let c: String = head.chars().take(7).collect();
            let n = if note.is_empty() { String::new() } else { format!(": {note}") };
            if approve { format!("✓ approved the push ({c}){n}") } else { format!("↩ sent the push back for changes ({c}){n}") }
        }
        None => text.to_string(),
    }
}

/// A push of ours waiting on its owner's decision. Kept on disk per project (team-held/), so a
/// restart neither forgets the hold nor drops the decision that comes for it.
#[derive(Clone)]
struct Held {
    repo: String,
    root: String, // the checkout it was pushed from: on load, whether it still stands
    head: String,
    session: String,
    owners: Vec<String>,
    ts: u64,
    decided: Option<bool>, // the owner approved (true) or sent it back (false)
    news: Option<String>,  // their decision, not yet handed to the session
}

static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

/// A hold older than this is dropped on load: nobody's coming back to it.
const HELD_DAYS: u64 = 14;

fn held_file(repo: &str) -> std::path::PathBuf {
    crate::config::state_dir().join("team-held").join(repo.replace('/', "__"))
}

/// Write `repo`'s holds out, whole (callers pass the list they just changed, still locked).
fn save_held(v: &[Held], repo: &str) {
    // tests write only to a QH_STATE_DIR of their own
    if repo.is_empty() || (cfg!(test) && std::env::var_os("QH_STATE_DIR").is_none()) { return }
    let list: Vec<Value> = v.iter().filter(|h| h.repo == repo).map(|h| json!({"root": h.root, "head": h.head, "session": h.session,
        "owners": h.owners, "ts": h.ts, "decided": h.decided, "news": h.news})).collect();
    let p = held_file(repo);
    if list.is_empty() { let _ = std::fs::remove_file(&p); return }
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let tmp = p.with_extension("tmp");
    if crate::platform::write_private(&tmp, Value::Array(list).to_string().as_bytes()).is_ok() { let _ = std::fs::rename(&tmp, &p); }
}

/// team.rs connected `repo`: take back the holds saved for it, minus the stale ones: too old,
/// the commit gone or already on a remote branch, or no longer in its checkout's HEAD (rewritten,
/// or the branch moved elsewhere).
pub fn load_held(repo: &str) {
    let saved: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(held_file(repo)).unwrap_or_default()).unwrap_or_default();
    let s = |x: &Value| x.as_str().unwrap_or("").to_string();
    let stands = |h: &Held| {
        let root = Path::new(&h.root);
        now().saturating_sub(h.ts) < HELD_DAYS * 86400
            && git(root, &["merge-base", "--is-ancestor", &h.head, "HEAD"]).is_some()
            && git(root, &["branch", "-r", "--contains", &h.head]).is_some_and(|o| o.trim().is_empty())
    };
    let got: Vec<Held> = saved.iter().map(|x| Held {
        repo: repo.into(), root: s(&x["root"]), head: s(&x["head"]), session: s(&x["session"]),
        owners: x["owners"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(String::from)).collect(),
        ts: x["ts"].as_u64().unwrap_or(0), decided: x["decided"].as_bool(), news: x["news"].as_str().map(String::from),
    }).filter(|h| !h.head.is_empty() && !h.root.is_empty() && stands(h)).collect();
    for h in got.iter().filter(|h| h.decided == Some(true)) { push_anyway(&h.head) }
    let news = got.iter().any(|h| h.news.is_some());
    let mut v = HELD.lock().unwrap();
    v.retain(|h| h.repo != repo);
    v.extend(got);
    save_held(&v, repo);
    drop(v);
    if news { crate::team::bump() }
}

/// engine.rs handed `head` to `owners`: keep it until one of them decides.
pub fn held(repo: &str, root: &Path, head: &str, session: &str, owners: &[String]) {
    if head.is_empty() { return }
    let mut v = HELD.lock().unwrap();
    v.retain(|h| h.head != head);
    v.push(Held { repo: repo.into(), root: root.to_string_lossy().into(), head: head.into(), session: session.into(),
        owners: owners.to_vec(), ts: now(), decided: None, news: None });
    save_held(&v, repo);
}

/// A private message came in from `from` (team.rs calls this for each). A decision on a push we
/// hold goes to the session that pushed, and an approval lets the retried push through.
pub fn on_dm(from: &str, text: &str) {
    let Some(Tag::Decide { head, approve, note }) = tag(text) else { return };
    let mut v = HELD.lock().unwrap();
    let Some(h) = v.iter_mut().find(|h| h.head == head && h.decided.is_none() && h.owners.iter().any(|o| o.eq_ignore_ascii_case(from))) else { return };
    let n = if note.is_empty() { String::new() } else { format!("\n\nTheir note: {note}") };
    h.news = Some(if approve {
        push_anyway(&head);
        format!("[owner decision] {from} approved the held push.{n}\n\nPush it now (the same command); it won't be held again.")
    } else {
        format!("[owner decision] {from} sent the push back for changes.{n}\n\nMake the changes, commit, and push again.")
    });
    h.decided = Some(approve);
    let repo = h.repo.clone();
    save_held(&v, &repo);
    drop(v);
    crate::team::bump();
}

/// Decisions not yet handed to their sessions (team.rs's loop delivers them). A push sent back
/// is done with once told; an approved one stays until it's pushed, so its retry goes through.
pub fn take_decisions() -> Vec<(String, String)> {
    let mut v = HELD.lock().unwrap();
    let (mut out, mut repos) = (vec![], vec![]);
    for h in v.iter_mut() {
        let Some(m) = h.news.take() else { continue };
        out.push((h.session.clone(), m));
        if !repos.contains(&h.repo) { repos.push(h.repo.clone()) }
    }
    v.retain(|h| h.decided != Some(false) || h.news.is_some());
    for r in &repos { save_held(&v, r) }
    out
}

/// What the reviews said, for the agent and the user.
pub fn summary(got: &[(String, Option<String>)]) -> String {
    let mut out = vec![];
    for (to, a) in got {
        match a {
            Some(a) => out.push(format!("review from {to}: {}\n{}", verdict(a), a.trim())),
            None => out.push(format!("review from {to}: no answer yet; the push went ahead (their answer will still come)")),
        }
    }
    format!("[review] {}", out.join("\n\n"))
}

/// For team.rs's tests: a review in flight, as `request` tracks it.
#[cfg(test)]
pub fn tests_track(id: u64, repo: &str) {
    track(Review { id, repo: repo.into(), session: "main".into(), to: String::new(), files: vec![], head: String::new(), answer: None, ts: now(), gave_up: false });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_keeps_waiting_while_the_reviewer_is_on_it() {
        track(Review { id: 900_101, repo: "t/w".into(), session: "main".into(), to: "bob/main".into(), files: vec![], head: String::new(), answer: None, ts: now(), gave_up: false });
        let t0 = Instant::now();
        until(&[900_101], Duration::ZERO, &|| false, &|| t0.elapsed() < Duration::from_millis(400));
        let e = t0.elapsed();
        assert!(e >= Duration::from_millis(400) && e < Duration::from_secs(2), "{e:?}: held while they work, then went ahead");
        assert_eq!(collect(&[900_101]), vec![("bob/main".to_string(), None)]);
    }

    #[test]
    fn codeowners_parse_and_match_like_github() {
        let rules = parse_codeowners("# owners\n*       @alice\n/src/team*.rs @bob @org/core  # the team code\n\
docs/   carol@example.com\n**/tests/** @dave\n/vendor/\nsrc/link.rs @erin\n");
        assert_eq!(rules.len(), 6);
        assert_eq!(rules[1], ("/src/team*.rs".to_string(), vec!["@bob".to_string(), "@org/core".to_string()]));
        let o = |p: &str| codeowners_for(&rules, p).map(|o| o.to_vec());
        assert_eq!(o("README.md"), Some(vec!["@alice".into()]), "* matches everything");
        assert_eq!(o("src/team.rs").unwrap()[0], "@bob");
        assert_eq!(o("src/teamview.rs").unwrap()[0], "@bob");
        assert_eq!(o("src/sub/team.rs"), Some(vec!["@alice".into()]), "anchored: not deeper");
        assert_eq!(o("docs/a/b.md"), Some(vec!["carol@example.com".into()]), "a folder covers what's in it");
        assert_eq!(o("x/docs/a.md"), Some(vec!["carol@example.com".into()]), "only a trailing slash: docs/ at any depth");
        assert_eq!(o("a/b/tests/c/d.rs"), Some(vec!["@dave".into()]));
        assert_eq!(o("vendor/lib.rs"), Some(vec![]), "no owners: unowned, the last match wins");
        assert_eq!(o("src/link.rs"), Some(vec!["@erin".into()]), "anchored by its inner slash");
        assert!(!pattern_matches("docs/", "docs"), "a folder pattern isn't a file");
        assert!(pattern_matches("*.js", "web/app/main.js"));
        assert!(!pattern_matches("/*.js", "web/main.js"));
        assert!(pattern_matches("apps/**/x.rs", "apps/x.rs"));
        assert!(codeowners_for(&parse_codeowners("/a @x"), "b").is_none());
    }

    #[test]
    fn git_authors_map_to_team_logins() {
        let m: Vec<String> = vec!["kesavsabari".into(), "Bob-Smith".into(), "carol".into()];
        assert_eq!(member_of("Kesav S", "12345+kesavsabari@users.noreply.github.com", &m), Some("kesavsabari".into()));
        assert_eq!(member_of("x", "bob-smith@users.noreply.github.com", &m), Some("Bob-Smith".into()));
        assert_eq!(member_of("Carol", "c.jones@example.com", &m), Some("carol".into()), "by name");
        assert_eq!(member_of("Someone", "carol@corp.example", &m), Some("carol".into()), "by the email's local part");
        assert_eq!(member_of("Dave", "dave@example.com", &m), None);
        assert_eq!(owner_member("@CAROL", &m), Some("carol".into()));
        assert_eq!(owner_member("@org/core", &m), None);
        assert_eq!(owner_member("kesavsabari@example.com", &m), Some("kesavsabari".into()));
    }

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -2,3 +2,3 @@ fn x() {
 keep
-old b
+new b
 keep
@@ -10,2 +10,3 @@
 ten
+added
 eleven
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
";

    #[test]
    fn diff_hunks_know_their_base_lines() {
        let h = parse_diff(DIFF);
        assert_eq!(h.len(), 3);
        assert_eq!((h[0].file.as_str(), h[0].old.clone()), ("src/a.rs", vec![3]));
        assert_eq!(h[1].old, vec![10, 11], "an addition belongs to the lines around it");
        assert!(h[1].text.contains("+added"));
        assert_eq!((h[2].file.as_str(), h[2].old.is_empty()), ("new.txt", true), "a new file has no base lines");
    }

    #[test]
    fn blame_picks_the_owner_and_skips_us() {
        let sha = "a".repeat(40);
        let sha2 = "b".repeat(40);
        let porcelain = format!("{sha} 3 3 1\nauthor Bob\nauthor-mail <bob@example.com>\nsummary x\n\tline\n\
{sha2} 10 10 1\nauthor Me\nauthor-mail <me@example.com>\n\tline\n{sha2} 11 11\nauthor Me\nauthor-mail <me@example.com>\n\tline\n");
        let b = parse_blame(&porcelain);
        assert_eq!(b[&3].1, "Bob");
        assert_eq!(b[&11].2, "me@example.com");
        let members: Vec<String> = vec!["bob".into(), "me".into(), "carol".into()];
        let hunks = parse_diff(DIFF);
        let blame = |f: &str| if f == "src/a.rs" { parse_blame(&porcelain) } else { HashMap::new() };
        let who = |n: &str, e: &str, _: &str| member_of(n, e, &members);
        let by = owners(&hunks, &[], &blame, &who, &members, "me");
        assert_eq!(by.len(), 1, "the second hunk is our own code, the new file nobody's: {by:?}");
        assert_eq!((by[0].0.as_str(), by[0].1.len()), ("bob", 1));
        // CODEOWNERS beats blame, and still never names us
        let rules = parse_codeowners("src/ @carol @me\nnew.txt @bob");
        let by = owners(&hunks, &rules, &blame, &who, &members, "me");
        let names: Vec<(&str, usize)> = by.iter().map(|(o, h)| (o.as_str(), h.len())).collect();
        assert_eq!(names, [("carol", 2), ("bob", 1)]);
        let text = request_text("me", &by[0].1);
        assert!(text.starts_with("[review request] me is about to push") && text.contains("src/a.rs") && text.contains("+new b"), "{text}");
    }

    #[test]
    fn real_git_blame_finds_the_owner_on_the_base() {
        let root = std::env::temp_dir().join(format!("qh-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str], who: &str| {
            let ok = std::process::Command::new("git").arg("-C").arg(&root).args(args)
                .env("GIT_AUTHOR_NAME", who).env("GIT_AUTHOR_EMAIL", format!("{who}@example.com"))
                .env("GIT_COMMITTER_NAME", who).env("GIT_COMMITTER_EMAIL", format!("{who}@example.com"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1")
                .output().unwrap().status.success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"], "x");
        std::fs::write(root.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        run(&["add", "."], "bob");
        run(&["commit", "-qm", "bob's"], "bob");
        std::fs::write(root.join("a.rs"), "fn a() {}\nfn b() { 1 }\nfn c() {}\n").unwrap();
        run(&["commit", "-qam", "mine"], "me");
        let diff = git(&root, &["diff", "--no-color", "-U3", "HEAD~1", "HEAD"]).unwrap();
        let hunks = parse_diff(&diff);
        let members: Vec<String> = vec!["bob".into(), "me".into()];
        let blame = |f: &str| git(&root, &["blame", "--line-porcelain", "HEAD~1", "--", f]).map(|t| parse_blame(&t)).unwrap_or_default();
        let who = |n: &str, e: &str, _: &str| member_of(n, e, &members);
        let by = owners(&hunks, &[], &blame, &who, &members, "me");
        assert_eq!(by.iter().map(|(o, _)| o.as_str()).collect::<Vec<_>>(), ["bob"]);
        assert!(by[0].1[0].text.contains("+fn b() { 1 }"));
        // bob pushing his own change back: nobody else owns it
        let by = owners(&hunks, &[], &blame, &who, &members, "bob");
        assert!(by.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pushes_are_spotted() {
        assert!(is_push("git push"));
        assert!(is_push("cargo test && git push origin main"));
        assert!(is_push("git -C ../x push --force-with-lease"));
        assert!(is_push("/usr/bin/git push"));
        assert!(!is_push("git status; echo push"));
        assert!(!is_push("git log --grep push"));
        assert!(!is_push("git stash push"));
    }

    #[test]
    fn verdicts_and_waiting() {
        assert_eq!(verdict("APPROVE. Looks right."), "approve");
        assert_eq!(verdict("**CHANGES**: the lock"), "changes");
        assert_eq!(verdict("Two notes: …"), "comments");
        assert_eq!(verdict("HANDOFF: this changes pricing policy, dave should decide"), "handoff");
        let h = handoff_text("erin", &["shop.py".into()], "HANDOFF: pricing call", "@@ -1 +1 @@\n-a\n+b", "abc123", "main");
        assert!(h.contains("erin wants to push changes to shop.py") && h.contains("pricing call") && h.contains("+b"));
        assert_eq!(tag(&h), Some(Tag::Hold { head: "abc123".into(), session: "main".into() }));
        assert!(!shown(&h).contains("#qh-hold") && shown(&h).contains("+b"));
        assert_eq!(verdict("(not answered: they have you as view only.)"), "none");
        // one answered in time, one not: the wait ends at the timeout and marks it given up
        let (a, b) = (900_001, 900_002);
        for id in [a, b] {
            track(Review { id, repo: "t/rv".into(), session: "main".into(), to: format!("x/{id}"), files: vec![], head: String::new(), answer: None, ts: now(), gave_up: false });
        }
        let t = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(100)); answered(a, "bob/main", "APPROVE") });
        let t0 = Instant::now();
        let got = wait_for(&[a, b], Duration::from_millis(600), &|| false);
        assert_eq!(t.join().unwrap(), Some(false), "answered while the push waited");
        assert!(t0.elapsed() >= Duration::from_millis(550), "waited for the second");
        assert_eq!(got[0], ("bob/main".to_string(), Some("APPROVE".to_string())));
        assert_eq!(got[1].1, None);
        assert_eq!(answered(b, "carol/main", "CHANGES: x"), Some(true), "late: the session gets it as a message");
        assert_eq!(answered(b, "carol/main", "again"), None, "answered once");
        assert_eq!(answered(123, "z", "not a review"), None);
        let s = summary(&got);
        assert!(s.contains("review from bob/main: approve") && s.contains("no answer yet; the push went ahead"), "{s}");
        // all answered: no waiting at all; stop() ends it early too
        let t0 = Instant::now();
        wait_for(&[a], Duration::from_secs(5), &|| false);
        track(Review { id: 900_003, repo: "t/rv".into(), session: "main".into(), to: "y".into(), files: vec![], head: String::new(), answer: None, ts: now(), gave_up: false });
        wait_for(&[900_003], Duration::from_secs(5), &|| true);
        assert!(t0.elapsed() < Duration::from_secs(1));
        let ev = take_events();
        assert!(ev.iter().any(|e| e["type"] == "review" && e["status"] == "approve"), "{ev:?}");
    }

    #[test]
    fn an_owner_decision_reaches_only_a_push_we_hold() {
        let _env = crate::hub::tests::ENV.lock().unwrap_or_else(|e| e.into_inner()); // the held list is shared with the restart test
        let d = decision_text("feed01", true, "fine, ship it");
        assert_eq!(tag(&d), Some(Tag::Decide { head: "feed01".into(), approve: true, note: "fine, ship it".into() }));
        assert!(shown(&d).starts_with("✓ approved the push (feed01)"));
        assert!(shown(&decision_text("feed01", false, "")).starts_with("↩ sent the push back"));
        assert_eq!(tag("just chatting about #qh-decide"), None);
        held("t/hold", Path::new("/nowhere"), "feed01", "s1", &["dave".into()]);
        on_dm("mallory", &d); // not who it was handed to
        on_dm("dave", &decision_text("other", true, ""));
        assert!(!pushed_anyway("feed01") && take_decisions().iter().all(|(s, _)| s != "s1"));
        on_dm("Dave", &d);
        let got = take_decisions();
        assert!(pushed_anyway("feed01") && got.iter().any(|(s, m)| s == "s1" && m.contains("approved") && m.contains("ship it")));
        on_dm("dave", &d); // once only
        assert!(take_decisions().iter().all(|(s, _)| s != "s1"));
        held("t/hold", Path::new("/nowhere"), "feed02", "s2", &["dave".into()]);
        on_dm("dave", &decision_text("feed02", false, "use / 100"));
        assert!(!pushed_anyway("feed02") && take_decisions().iter().any(|(s, m)| s == "s2" && m.contains("use / 100")));
        let c = clip_diff(&format!("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n ctx\n{}", "+l\n".repeat(40)));
        assert!(!c.contains("ctx") && !c.contains("--- a/x") && c.contains("@@") && c.ends_with("more lines"));
    }

    #[test]
    fn a_held_push_and_its_decision_survive_a_restart() {
        let _env = crate::hub::tests::ENV.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("qh-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("r");
        std::fs::create_dir_all(&repo).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        let g = |a: &[&str]| git(&repo, a).unwrap_or_default().trim().to_string();
        let commit = |m: &str, amend: bool| {
            let mut a = vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", m];
            if amend { a.push("--amend") }
            g(&a);
            g(&["rev-parse", "HEAD"])
        };
        g(&["init", "-q"]);
        let head = commit("a", false);
        assert!(!head.is_empty());
        // a restart: what's in memory is gone, the file brings it back
        let restart = || {
            HELD.lock().unwrap().retain(|h| h.repo != "t/held");
            PUSH_ANYWAY.lock().unwrap().retain(|h| h.len() != 40 && h.len() != 64); // this test's heads are real commits (sha1 or sha256)
            load_held("t/held");
        };
        held("t/held", &repo, &head, "s1", &["erin".into()]);
        restart();
        assert!(HELD.lock().unwrap().iter().any(|h| h.repo == "t/held" && h.head == head && h.session == "s1" && h.decided.is_none()));
        // the owner's decision comes in after it; a restart before it's delivered keeps it too
        on_dm("erin", &decision_text(&head, true, "ok"));
        restart();
        assert!(pushed_anyway(&head), "an approval outlives a restart");
        assert!(take_decisions().iter().any(|(s, m)| s == "s1" && m.contains("approved")));
        restart();
        assert!(pushed_anyway(&head) && take_decisions().iter().all(|(s, _)| s != "s1"), "delivered once");
        // pushed since: it doesn't come back
        g(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        restart();
        assert!(!pushed_anyway(&head) && !HELD.lock().unwrap().iter().any(|h| h.repo == "t/held"));
        assert!(!held_file("t/held").exists(), "nothing left, no file");
        // the branch was rewritten: the held commit isn't in HEAD any more
        g(&["update-ref", "-d", "refs/remotes/origin/main"]);
        held("t/held", &repo, &head, "s1", &["erin".into()]);
        let h2 = commit("b", true);
        restart();
        assert!(!HELD.lock().unwrap().iter().any(|h| h.repo == "t/held"));
        // sent back: told once, then let go
        held("t/held", &repo, &h2, "s1", &["erin".into()]);
        on_dm("erin", &decision_text(&h2, false, "no"));
        restart();
        assert!(take_decisions().iter().any(|(s, m)| s == "s1" && m.contains("sent the push back")));
        restart();
        assert!(!HELD.lock().unwrap().iter().any(|h| h.repo == "t/held") && !pushed_anyway(&h2));
        let _ = std::fs::remove_dir_all(&root);
    }
}
