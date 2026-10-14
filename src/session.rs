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

fn id(p: &Path) -> String {
    p.file_stem().unwrap_or_default().to_string_lossy().into()
}

/// This folder's saved sessions for a sidebar, newest first: id, title (the first request),
/// message count, last change (unix seconds), path.
pub fn for_project(cwd: &Path, n: usize) -> Vec<Value> {
    project(&dir(), cwd).into_iter().filter(|(_, m)| m.iter().any(|x| x["role"] == "user")).take(n).map(|(p, msgs)| {
        // the title is the first real request, not a /command typed before the app ran them
        let first = msgs.iter().filter(|m| m["role"] == "user").map(text).find(|t| !t.trim_start().starts_with('/'))
            .or_else(|| msgs.iter().find(|m| m["role"] == "user").map(text)).unwrap_or_default();
        let when = std::fs::metadata(&p).and_then(|m| m.modified()).ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
        serde_json::json!({"id": id(&p), "title": cut(&first, 70), "messages": msgs.len(), "when": when, "path": p})
    }).collect()
}

/// The past_sessions tool: list (no args), search (`query`) or read (`id`) this folder's sessions.
pub fn past(dir: &Path, cwd: &Path, a: &Value) -> String {
    let all = project(dir, cwd);
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
        let l = past(&d, cwd, &json!({}));
        assert!(l.starts_with("20261011-100000") && l.contains("fix the login bug") && !l.contains("elsewhere"), "{l}");
        let s = past(&d, cwd, &json!({"query": "LOGIN"}));
        assert!(s.contains("20261010-100000 user: fix the login bug") && !s.contains("20261012"), "{s}");
        let r = past(&d, cwd, &json!({"id": "20261010-100000"}));
        assert_eq!(r, "user: fix the login bug\n· 2 steps\nassistant: Fixed the token refresh.\n");
        assert!(past(&d, cwd, &json!({"id": "20261012-100000"})).starts_with("error"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
