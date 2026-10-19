//! What qh did in each session, one JSON line per action, in `<session>.activity.jsonl` next to
//! the session file: commands run, files changed, network asks through the proxy, approvals.
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX: usize = 500;

pub fn path_for(session: &Path) -> PathBuf {
    let stem = session.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    session.with_file_name(format!("{stem}.activity.jsonl"))
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Appends one action; `extra` fields (cmd, exit, host…) ride along with t/kind/text/ok.
pub fn record(session: &Path, kind: &str, text: &str, ok: bool, extra: Value) {
    if session.as_os_str().is_empty() { return }
    let mut v = json!({"t": now(), "kind": kind, "text": text, "ok": ok});
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) { o.extend(e) }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path_for(session)) {
        let _ = writeln!(f, "{v}");
    }
}

/// The last `n` actions, newest last.
pub fn read(session: &Path, n: usize) -> Vec<Value> {
    let s = std::fs::read_to_string(path_for(session)).unwrap_or_default();
    let all: Vec<Value> = s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    all[all.len().saturating_sub(n)..].to_vec()
}

/// Lines added and removed between two texts (a cheap line-set count, not a real diff).
pub fn plus_minus(before: &str, after: &str) -> (usize, usize) {
    use std::collections::HashMap;
    let mut c: HashMap<&str, i64> = HashMap::new();
    for l in before.lines() { *c.entry(l).or_default() -= 1 }
    for l in after.lines() { *c.entry(l).or_default() += 1 }
    c.values().fold((0, 0), |(a, r), &v| if v > 0 { (a + v as usize, r) } else { (a, r + (-v) as usize) })
}

/// The exit code a bash tool result starts with ("exit 1\n…").
pub fn exit_code(out: &str) -> Option<i64> {
    out.strip_prefix("exit ")?.split(|c: char| c.is_whitespace()).next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_newest_last_capped() {
        let d = std::env::temp_dir().join(format!("qh-act-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let s = d.join("20260101-000000.jsonl");
        for i in 0..5 { record(&s, "cmd", &format!("c{i}"), i % 2 == 0, json!({"exit": i})) }
        assert!(d.join("20260101-000000.activity.jsonl").exists());
        let r = read(&s, 3);
        assert_eq!(r.len(), 3);
        assert_eq!((r[2]["text"].as_str(), r[2]["exit"].as_i64(), r[2]["ok"].as_bool()), (Some("c4"), Some(4), Some(true)));
        assert_eq!(r[0]["text"], "c2");
        assert!(read(&d.join("none.jsonl"), 9).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn counts_and_exit() {
        assert_eq!(plus_minus("a\nb\nc", "a\nB\nc\nd"), (2, 1));
        assert_eq!(exit_code("exit 2\nboom"), Some(2));
        assert_eq!(exit_code("exit -1"), Some(-1));
        assert_eq!(exit_code("error: x"), None);
    }
}
