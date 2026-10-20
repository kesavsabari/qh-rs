use crate::config::state_dir;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn dir() -> PathBuf {
    state_dir().join("sessions")
}

pub fn stamp() -> String {
    crate::platform::stamp()
}

/// A fresh session path; a suffix keeps paths unique when several are made in one second.
pub fn new_path() -> PathBuf {
    let _ = std::fs::create_dir_all(dir());
    let base = stamp();
    let mut p = dir().join(format!("{base}.jsonl"));
    let mut i = 2;
    while p.exists() {
        p = dir().join(format!("{base}-{i}.jsonl"));
        i += 1;
    }
    let _ = std::fs::write(&p, "");
    p
}

/// Rewritten whole each turn: compaction edits earlier messages.
pub fn save(path: &Path, messages: &[Value]) {
    let mut s = String::new();
    for m in messages {
        s.push_str(&m.to_string());
        s.push('\n');
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Add one message to the end of the transcript (a whole save follows at turn end).
pub fn append(path: &Path, msg: &Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Replies streaming right now, per transcript: what a kill would otherwise lose.
static PARTIAL: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

pub fn partial_add(path: &Path, text: &str) {
    let mut p = PARTIAL.lock().unwrap_or_else(|e| e.into_inner());
    match p.iter_mut().find(|(q, _)| q == path) {
        Some((_, s)) => s.push_str(text),
        None => p.push((path.to_path_buf(), text.to_string())),
    }
}

/// The reply finished (or failed): the transcript gets it the normal way.
pub fn partial_clear(path: &Path) {
    PARTIAL.lock().unwrap_or_else(|e| e.into_inner()).retain(|(q, _)| q != path);
}

/// On a kill: every reply still streaming goes to the end of its transcript, marked cut off.
pub fn save_partials() {
    let p = std::mem::take(&mut *PARTIAL.lock().unwrap_or_else(|e| e.into_inner()));
    for (path, text) in p.into_iter().filter(|(_, t)| !t.trim().is_empty()) {
        append(&path, &serde_json::json!({"role": "assistant", "content": format!("{text}\n\n[cut off: qh was closed mid-reply]")}));
    }
}

pub fn load(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap_or_default().lines()
        .filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn sorted() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir()).into_iter().flatten().flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0)).collect(); // empty = reserved, never used
    v.sort();
    v
}

pub fn latest() -> Option<PathBuf> {
    sorted().pop()
}

pub fn list(n: usize) -> String {
    let all = sorted();
    let rows: Vec<String> = all[all.len().saturating_sub(n)..].iter().map(|p| {
        let msgs = load(p);
        let first = msgs.iter().find(|m| m["role"] == "user").and_then(|m| m["content"].as_str()).unwrap_or("");
        let first: String = first.chars().take(60).collect();
        format!("{}  {:>3} msgs  {}", p.file_stem().unwrap_or_default().to_string_lossy(), msgs.len(), first)
    }).collect();
    if rows.is_empty() { "none".into() } else { rows.join("\n") }
}

/// Sessions begun in this folder, newest first: the system prompt names the folder.
fn project(dir: &Path, cwd: &Path) -> Vec<(PathBuf, Vec<Value>)> {
    let tag = format!("working in {}.", cwd.display());
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    v.sort();
    v.into_iter().rev().filter_map(|p| {
        let msgs = load(&p);
        msgs.iter().find(|m| m["role"] == "system")?["content"].as_str()?.contains(&tag).then_some((p, msgs))
    }).collect()
}

fn text(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    }
}

fn cut(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

/// A short title for a session (named by the model), kept beside its transcript.
pub fn set_title(session: &Path, title: &str) {
    if !title.trim().is_empty() { let _ = std::fs::write(session.with_extension("title"), title.trim()); }
}

fn title_of(session: &Path) -> Option<String> {
    std::fs::read_to_string(session.with_extension("title")).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// This folder's sessions with no short title yet and a long first request: (path, that request).
pub fn untitled(cwd: &Path, n: usize) -> Vec<(PathBuf, String)> {
    for_project(cwd, n).into_iter().filter_map(|x| {
        let p = PathBuf::from(x["path"].as_str()?);
        if title_of(&p).is_some() || x["title"].as_str()?.chars().count() <= 40 { return None }
        let first = load(&p).into_iter().filter(|m| m["role"] == "user").map(|m| text(&m))
            .find(|t| !t.trim_start().starts_with('/'))?;
        Some((p, first))
    }).collect()
}

fn id(p: &Path) -> String {
    p.file_stem().unwrap_or_default().to_string_lossy().into()
}

/// This folder's saved sessions for a sidebar, newest first: id, title (the first request),
/// message count, last change (unix seconds), path.
pub fn for_project(cwd: &Path, n: usize) -> Vec<Value> {
    // the sidebar asks after every turn: only files that changed since last time are read again
    type Seen = (std::time::SystemTime, bool, String, usize); // modified, in this folder, title, messages
    static SEEN: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, Seen>>> = std::sync::Mutex::new(None);
    let tag = format!("working in {}.", cwd.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir()).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    files.sort();
    let mut g = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = g.get_or_insert_with(Default::default);
    let mut out = vec![];
    for p in files.into_iter().rev() {
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let fresh = seen.get(&p).is_some_and(|s| s.0 == modified);
        if !fresh {
            let msgs = load(&p);
            let here = msgs.iter().find(|m| m["role"] == "system").and_then(|m| m["content"].as_str()).is_some_and(|c| c.contains(&tag));
            // the title is the first real request, not a /command typed before the app ran them
            let users: Vec<String> = msgs.iter().filter(|m| m["role"] == "user").map(text).collect();
            let first = users.iter().find(|t| !t.trim_start().starts_with('/')).or(users.first()).cloned().unwrap_or_default();
            let title = if users.is_empty() { String::new() } else { cut(&first, 70) };
            seen.insert(p.clone(), (modified, here, title, msgs.len()));
        }
        let (_, here, title, count) = &seen[&p];
        if !*here || title.is_empty() { continue }
        let title = &title_of(&p).unwrap_or_else(|| title.clone());
        let when = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        out.push(serde_json::json!({"id": id(&p), "title": title, "messages": count, "when": when, "path": p}));
        if out.len() >= n { break }
    }
    out
}

/// The past_sessions tool: list (no args), search (`query`) or read (`id`) this folder's sessions.
pub fn past(dir: &Path, cwd: &Path, a: &Value, current: &Path) -> String {
    let mut all = project(dir, cwd);
    all.retain(|(p, _)| p != current);
    let (q, want) = (a["query"].as_str().unwrap_or("").trim().to_lowercase(), a["id"].as_str().unwrap_or("").trim());
    if !want.is_empty() {
        let Some((_, msgs)) = all.iter().find(|(p, _)| id(p) == want) else { return format!("error: no session {want} in this folder") };
        let (mut out, mut steps) = (String::new(), 0);
        for m in msgs {
            let (role, t) = (m["role"].as_str().unwrap_or(""), text(m));
            if role == "tool" || (role == "assistant" && t.trim().is_empty()) { steps += 1; continue }
            if !matches!(role, "user" | "assistant") { continue }
            if steps > 0 { out.push_str(&format!("· {steps} steps\n")); steps = 0 }
            out.push_str(&format!("{role}: {}\n", cut(&t, 1500)));
        }
        // keep the end: that's where the session left off
        let n = out.chars().count();
        if n > 12000 { out = format!("[earlier part cut]\n{}", out.chars().skip(n - 12000).collect::<String>()) }
        return if out.is_empty() { "empty session".into() } else { out };
    }
    if !q.is_empty() {
        let mut hits = vec![];
        's: for (p, msgs) in &all {
            for m in msgs.iter().filter(|m| matches!(m["role"].as_str(), Some("user" | "assistant"))) {
                let t = text(m);
                let Some(i) = t.to_lowercase().find(&q) else { continue };
                // lowercasing can shift byte offsets: count chars instead
                let at = t.to_lowercase()[..i].chars().count();
                let snip: String = t.chars().skip(at.saturating_sub(80)).collect();
                hits.push(format!("{} {}: {}", id(p), m["role"].as_str().unwrap_or(""), cut(&snip, 240)));
                if hits.len() >= 20 { break 's }
            }
        }
        return if hits.is_empty() { format!("no matches for {q:?}") } else { hits.join("\n") };
    }
    let rows: Vec<String> = all.iter().take(20).map(|(p, msgs)| {
        let first = msgs.iter().find(|m| m["role"] == "user").map(text).unwrap_or_default();
        let kb = std::fs::metadata(p).map(|m| m.len() / 1024).unwrap_or(0);
        format!("{}  {} msgs  {kb} KB  {}", id(p), msgs.len(), cut(&first, 80))
    }).collect();
    if rows.is_empty() { "no earlier sessions in this folder".into() } else { rows.join("\n") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_kill_keeps_the_reply_streaming() {
        let d = std::env::temp_dir().join(format!("qh-partial-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (a, b) = (d.join("a.jsonl"), d.join("b.jsonl"));
        save(&a, &[json!({"role": "user", "content": "explain"})]);
        save(&b, &[json!({"role": "user", "content": "done one"})]);
        partial_add(&a, "Half of ");
        partial_add(&a, "an answer");
        partial_add(&b, "finished");
        partial_clear(&b); // completed normally: saved by the turn, not here
        save_partials();
        let msgs = load(&a);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[1]["content"].as_str().unwrap().starts_with("Half of an answer\n\n[cut off"), "{msgs:?}");
        assert_eq!(load(&b).len(), 1);
        save_partials(); // nothing left: a second signal adds nothing
        assert_eq!(load(&a).len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn past_lists_searches_and_reads_only_this_folder() {
        let d = std::env::temp_dir().join(format!("qh-past-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let sys = |f: &str| json!({"role": "system", "content": format!("You are a terse coding agent working in {f}.\nUse tools.")});
        save(&d.join("20261010-100000.jsonl"), &[sys("/p"), json!({"role": "user", "content": "fix the login bug"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{}]}), json!({"role": "tool", "content": "ok"}),
            json!({"role": "assistant", "content": "Fixed the token refresh."})]);
        save(&d.join("20261011-100000.jsonl"), &[sys("/p"), json!({"role": "user", "content": "add dark mode"})]);
        save(&d.join("20261012-100000.jsonl"), &[sys("/other"), json!({"role": "user", "content": "login elsewhere"})]);
        let cwd = Path::new("/p");
        let l = past(&d, cwd, &json!({}), Path::new(""));
        assert!(l.starts_with("20261011-100000") && l.contains("fix the login bug") && !l.contains("elsewhere"), "{l}");
        let s = past(&d, cwd, &json!({"query": "LOGIN"}), Path::new(""));
        assert!(s.contains("20261010-100000 user: fix the login bug") && !s.contains("20261012"), "{s}");
        let r = past(&d, cwd, &json!({"id": "20261010-100000"}), Path::new(""));
        assert_eq!(r, "user: fix the login bug\n· 2 steps\nassistant: Fixed the token refresh.\n");
        assert!(past(&d, cwd, &json!({"id": "20261012-100000"}), Path::new("")).starts_with("error"));
        // the session asking isn't one of its own past sessions
        let me = d.join("20261011-100000.jsonl");
        let mine = past(&d, cwd, &json!({}), &me);
        assert!(!mine.contains("20261011-100000") && mine.contains("20261010-100000"), "{mine}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
