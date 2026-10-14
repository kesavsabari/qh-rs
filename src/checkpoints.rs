//! Snapshots of files before the agent changes them, so /undo rolls back one
//! tool call at a time and /diff shows everything changed this session.
use crate::tools::source_files;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub struct Checkpoints {
    dir: PathBuf,
    stack: Vec<Snap>,
    original: Map<String, Value>,
}

/// One tool call's snapshot, tagged with the user message (0-based) whose turn made it.
struct Snap {
    turn: usize,
    prompt: String,
    files: Map<String, Value>, // abs path -> content, or null if it did not exist
}

impl Snap {
    fn from_json(m: Map<String, Value>) -> Snap {
        match m.get("files").and_then(|f| f.as_object()) {
            Some(f) => Snap { turn: m["turn"].as_u64().unwrap_or(0) as usize,
                prompt: m["prompt"].as_str().unwrap_or("").into(), files: f.clone() },
            None => Snap { turn: 0, prompt: String::new(), files: m }, // before checkpoints had turns
        }
    }

    fn to_json(&self) -> Value {
        serde_json::json!({"turn": self.turn, "prompt": self.prompt, "files": self.files})
    }

    /// Put every file back as it was before this snapshot.
    fn restore(&self) -> Vec<String> {
        let mut names = vec![];
        for (path, content) in &self.files {
            match content.as_str() {
                Some(c) => { let _ = std::fs::write(path, c); }
                None => { let _ = std::fs::remove_file(path); }
            }
            names.push(Path::new(path).file_name().unwrap_or_default().to_string_lossy().to_string());
        }
        names
    }
}

fn read_json(p: &Path) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(&std::fs::read_to_string(p).ok()?).ok()?.as_object().cloned()
}

impl Checkpoints {
    pub fn new(dir: PathBuf) -> Self {
        let mut stack = vec![];
        for i in 1.. {
            match read_json(&dir.join(format!("{i}.json"))) {
                Some(s) => stack.push(Snap::from_json(s)),
                None => break,
            }
        }
        let original = read_json(&dir.join("original.json")).unwrap_or_default();
        Checkpoints { dir, stack, original }
    }

    fn paths_for(name: &str, a: &Value, cwd: &Path) -> Vec<PathBuf> {
        let s = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("");
        match name {
            "write_file" | "edit_file" if !s("path").is_empty() => vec![crate::tools::resolve(cwd, s("path"))],
            "rename_symbol" if !s("old").is_empty() => {
                let Ok(pat) = regex::Regex::new(&format!(r"\b{}\b", regex::escape(s("old")))) else { return vec![] };
                source_files(cwd, s("path")).into_iter().filter(|(_, t)| pat.is_match(t)).map(|(f, _)| f).collect()
            }
            _ => vec![],
        }
    }

    pub fn snapshot(&mut self, name: &str, a: &Value, cwd: &Path, turn: usize, prompt: &str) {
        let paths = Self::paths_for(name, a, cwd);
        if paths.is_empty() {
            return;
        }
        let mut snap = Map::new();
        for p in paths {
            let p = std::path::absolute(&p).unwrap_or(p);
            let k = p.to_string_lossy().to_string();
            let v = std::fs::read_to_string(&p).map(Value::from).unwrap_or(Value::Null);
            self.original.entry(k.clone()).or_insert_with(|| v.clone());
            snap.insert(k, v);
        }
        let snap = Snap { turn, prompt: prompt.chars().take(200).collect(), files: snap };
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join(format!("{}.json", self.stack.len() + 1)), snap.to_json().to_string());
        self.stack.push(snap);
        let _ = std::fs::write(self.dir.join("original.json"), Value::Object(self.original.clone()).to_string());
    }

    pub fn undo(&mut self) -> String {
        let Some(snap) = self.stack.pop() else { return "nothing to undo".into() };
        let _ = std::fs::remove_file(self.dir.join(format!("{}.json", self.stack.len() + 1)));
        format!("restored {}", snap.restore().join(", "))
    }

    /// The turns that changed files, oldest first: (turn, its prompt, absolute paths it touched).
    pub fn turns(&self) -> Vec<(usize, String, Vec<String>)> {
        let mut out: Vec<(usize, String, Vec<String>)> = vec![];
        for s in &self.stack {
            if out.last().map(|l| l.0) != Some(s.turn) { out.push((s.turn, s.prompt.clone(), vec![])) }
            let files = &mut out.last_mut().unwrap().2;
            for p in s.files.keys() { if !files.contains(p) { files.push(p.clone()) } }
        }
        out
    }

    /// Put files back as they were before user message `turn` was sent, newest change first.
    pub fn rewind(&mut self, turn: usize) -> Vec<String> {
        let mut names = vec![];
        while self.stack.last().is_some_and(|s| s.turn >= turn) {
            let snap = self.stack.pop().unwrap();
            let _ = std::fs::remove_file(self.dir.join(format!("{}.json", self.stack.len() + 1)));
            for n in snap.restore() { if !names.contains(&n) { names.push(n) } }
        }
        names
    }

    /// Every file changed this session: (absolute path, before or None if it was new, now or None if deleted).
    pub fn changes(&self) -> Vec<(String, Option<String>, Option<String>)> {
        self.original.iter().map(|(p, b)| (p.clone(), b.as_str().map(String::from), std::fs::read_to_string(p).ok()))
            .filter(|(_, b, a)| b != a).collect()
    }

    /// Keep one file as it is now: it leaves the review list (and /diff), /undo still steps back.
    pub fn accept(&mut self, path: &str) {
        self.original.remove(path);
        let _ = std::fs::write(self.dir.join("original.json"), Value::Object(self.original.clone()).to_string());
    }

    /// Put one file back the way it was before the agent touched it this session.
    pub fn reject(&mut self, path: &str) -> Result<(), String> {
        let before = self.original.get(path).ok_or("not a file changed this session")?.clone();
        match before.as_str() {
            Some(c) => std::fs::write(path, c).map_err(|e| e.to_string())?,
            None => { let _ = std::fs::remove_file(path); }
        }
        self.accept(path);
        Ok(())
    }

    /// Unified diff of every touched file against its first snapshot, via `diff -u`.
    pub fn diff(&self) -> String {
        let mut out = String::new();
        for (path, before) in &self.original {
            let after = std::fs::read_to_string(path).ok();
            out.push_str(&crate::platform::diff(path, before.as_str(), after.as_deref()));
        }
        if out.is_empty() { "no changes this session".into() } else { out }
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn reject_restores_and_accept_keeps() {
        let d = std::env::temp_dir().join(format!("qh-ckpt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let (a, b) = (d.join("a.txt"), d.join("b.txt"));
        std::fs::write(&a, "old a").unwrap();
        std::fs::write(&b, "old b").unwrap();
        let mut c = Checkpoints::new(d.join("ck"));
        for f in [&a, &b] {
            c.snapshot("write_file", &serde_json::json!({"path": f.to_string_lossy()}), &d, 0, "");
        }
        std::fs::write(&a, "new a").unwrap();
        std::fs::write(&b, "new b").unwrap();
        assert_eq!(c.changes().len(), 2);
        let ka = std::path::absolute(&a).unwrap().to_string_lossy().to_string();
        let kb = std::path::absolute(&b).unwrap().to_string_lossy().to_string();
        c.reject(&ka).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "old a");
        c.accept(&kb);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new b");
        assert!(c.changes().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod rewind_tests {
    use super::*;

    #[test]
    fn rewind_goes_back_before_a_turn_and_survives_reload() {
        let d = std::env::temp_dir().join(format!("qh-rewind-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let (a, b) = (d.join("a.txt"), d.join("b.txt"));
        std::fs::write(&a, "a0").unwrap();
        let w = |c: &mut Checkpoints, f: &Path, text: &str, turn| {
            c.snapshot("write_file", &serde_json::json!({"path": f.to_string_lossy()}), &d, turn, &format!("turn {turn}"));
            std::fs::write(f, text).unwrap();
        };
        let mut c = Checkpoints::new(d.join("ck"));
        w(&mut c, &a, "a1", 0);
        w(&mut c, &a, "a2", 2);
        w(&mut c, &b, "b2", 2); // b is new in turn 2
        w(&mut c, &a, "a3", 3);
        let mut c = Checkpoints::new(d.join("ck")); // reloaded from disk
        let t = c.turns();
        assert_eq!(t.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 2, 3]);
        assert_eq!(t[1].1, "turn 2");
        assert_eq!(t[1].2.len(), 2);
        c.rewind(2);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a1");
        assert!(!b.exists());
        assert_eq!(Checkpoints::new(d.join("ck")).turns().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
