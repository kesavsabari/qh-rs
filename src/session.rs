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
