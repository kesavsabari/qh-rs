//! Snapshots of files before the agent changes them, so /undo rolls back one
//! tool call at a time and /diff shows everything changed this session.
use crate::tools::source_files;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub struct Checkpoints {
    dir: PathBuf,
    stack: Vec<Map<String, Value>>, // abs path -> content, or null if it did not exist
    original: Map<String, Value>,
}

fn read_json(p: &Path) -> Option<Map<String, Value>> {
    serde_json::from_str::<Value>(&std::fs::read_to_string(p).ok()?).ok()?.as_object().cloned()
}

impl Checkpoints {
    pub fn new(dir: PathBuf) -> Self {
        let mut stack = vec![];
        for i in 1.. {
            match read_json(&dir.join(format!("{i}.json"))) {
                Some(s) => stack.push(s),
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

    pub fn snapshot(&mut self, name: &str, a: &Value, cwd: &Path) {
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
        self.stack.push(snap.clone());
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join(format!("{}.json", self.stack.len())), Value::Object(snap).to_string());
        let _ = std::fs::write(self.dir.join("original.json"), Value::Object(self.original.clone()).to_string());
    }

    pub fn undo(&mut self) -> String {
        let Some(snap) = self.stack.pop() else { return "nothing to undo".into() };
        let _ = std::fs::remove_file(self.dir.join(format!("{}.json", self.stack.len() + 1)));
        let mut names = vec![];
        for (path, content) in &snap {
            match content.as_str() {
                Some(c) => { let _ = std::fs::write(path, c); }
                None => { let _ = std::fs::remove_file(path); }
            }
            names.push(Path::new(path).file_name().unwrap_or_default().to_string_lossy().to_string());
        }
        format!("restored {}", names.join(", "))
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
