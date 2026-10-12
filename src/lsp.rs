//! LSP code intelligence.
//!
//! Design:
//! - A minimal hand-written LSP client: JSON-RPC 2.0 over the server's stdio with
//!   `Content-Length:` framing (`encode` / `read_frame`). No LSP crate; serde_json only.
//! - Servers are found on PATH and started lazily, the first time a file of that language is
//!   written, edited or queried: rust-analyzer (Cargo.toml), typescript-language-server --stdio
//!   (package.json / tsconfig.json), pyright-langserver --stdio or pylsp (pyproject.toml,
//!   setup.py, requirements.txt), gopls (go.mod). One per (project root, language). A server that
//!   isn't installed or fails to start is remembered and never retried this session.
//! - Like MCP servers (src/mcp.rs), they are qh's own helper processes, spawned with
//!   `Command` outside the bash sandbox; the model never controls their command line.
//! - One reader thread per server blocks on the pipe and forwards messages over a channel, so
//!   waiting is `recv_timeout` (no polling, no CPU while idle). Server-to-client requests
//!   (workspace/configuration, progress creation…) get a null reply so servers don't stall.
//! - After write_file / edit_file, `after_write` sends didOpen (first time) or a full-text
//!   didChange, then waits up to `lsp_wait_ms` for publishDiagnostics on that file and returns
//!   the errors/warnings that weren't there before the edit (max 15 lines). No server or a
//!   timeout gives an empty string, so the tool result is unchanged.
//! - `goto_definition`, `hover` and `find_references` tools use 1-based line/col; when no server
//!   is available they say so, and the regex find_references / rename_symbol stay as fallback.
//! - `shutdown` kills every server when qh exits.
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One message framed for the wire.
pub fn encode(v: &Value) -> Vec<u8> {
    let body = v.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Read one framed message; None at EOF or on a broken frame.
pub fn read_frame<R: BufRead>(r: &mut R) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 { return None }
        let line = line.trim_end();
        if line.is_empty() {
            if len.is_some() { break } else { continue }
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") { len = v.trim().parse::<usize>().ok() }
        }
    }
    let mut buf = vec![0; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang { Rust, Ts, Py, Go }

pub fn lang_of(path: &Path) -> Option<Lang> {
    Some(match path.extension()?.to_str()? {
        "rs" => Lang::Rust,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Lang::Ts,
        "py" => Lang::Py,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn lang_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        _ => "javascript",
    }
}

fn markers(l: Lang) -> &'static [&'static str] {
    match l {
        Lang::Rust => &["Cargo.toml"],
        Lang::Ts => &["tsconfig.json", "package.json"],
        Lang::Py => &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
        Lang::Go => &["go.mod"],
    }
}

/// Candidate commands, first found on PATH wins.
fn commands(l: Lang) -> &'static [(&'static str, &'static [&'static str])] {
    match l {
        Lang::Rust => &[("rust-analyzer", &[])],
        Lang::Ts => &[("typescript-language-server", &["--stdio"])],
        Lang::Py => &[("pyright-langserver", &["--stdio"]), ("pylsp", &[])],
        Lang::Go => &[("gopls", &[])],
    }
}

/// The outermost-but-nearest folder holding a marker; for Rust the workspace root wins.
fn root_of(file: &Path, l: Lang) -> Option<PathBuf> {
    let mut found = None;
    for d in file.ancestors().skip(1) {
        if markers(l).iter().any(|m| d.join(m).is_file()) {
            found = Some(d.to_path_buf());
            if l != Lang::Rust { break }
        }
    }
    found
}

fn on_path(cmd: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).flat_map(|d| exts.iter().map(move |e| d.join(format!("{cmd}{e}"))))
        .find(|p| p.is_file())
}

pub fn uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s: String = s.chars().map(|c| match c { ' ' => "%20".into(), '#' => "%23".into(), '?' => "%3F".into(), c => c.to_string() }).collect();
    if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") }
}

fn path_of(u: &str) -> PathBuf {
    let s = u.strip_prefix("file://").unwrap_or(u).replace("%20", " ").replace("%23", "#").replace("%3F", "?").replace("%3A", ":");
    // file:///C:/x on Windows
    let s = if s.len() > 3 && s.as_bytes()[2] == b':' && s.starts_with('/') { s[1..].to_string() } else { s };
    PathBuf::from(s)
}

/// A diagnostic reduced to what we show and compare.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Diag { pub line: u64, pub col: u64, pub severity: u8, pub message: String }

pub fn parse_diags(params: &Value) -> Vec<Diag> {
    params["diagnostics"].as_array().into_iter().flatten().map(|d| Diag {
        line: d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
        col: d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
        severity: d["severity"].as_u64().unwrap_or(1) as u8,
        message: d["message"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string(),
    }).collect()
}

/// Errors and warnings in `now` that aren't in `before` (by severity + message, so lines that
/// merely moved don't count), as "\nlsp: ..." lines; empty when nothing new.
pub fn format_new(path: &str, before: &[Diag], now: &[Diag]) -> String {
    const MAX: usize = 15;
    let old: HashSet<(u8, &str)> = before.iter().map(|d| (d.severity, d.message.as_str())).collect();
    let mut new: Vec<&Diag> = now.iter().filter(|d| d.severity <= 2 && !old.contains(&(d.severity, d.message.as_str()))).collect();
    if new.is_empty() { return String::new() }
    new.sort_by_key(|d| (d.severity, d.line));
    let errs = new.iter().filter(|d| d.severity == 1).count();
    let mut out = format!("\nlsp: {errs} new error(s), {} new warning(s) in {path}:", new.len() - errs);
    for d in new.iter().take(MAX) {
        let kind = if d.severity == 1 { "error" } else { "warning" };
        out.push_str(&format!("\n  {}:{}: {kind}: {}", d.line, d.col, d.message.chars().take(200).collect::<String>()));
    }
    if new.len() > MAX { out.push_str(&format!("\n  … {} more", new.len() - MAX)) }
    out
}

pub struct Client {
    child: Option<Child>,
    stdin: Box<dyn Write + Send>,
    rx: Receiver<Value>,
    id: u64,
    versions: HashMap<String, i64>,
    diags: HashMap<String, Vec<Diag>>,
    name: String,
}

impl Client {
    /// Talk LSP over any pair of streams (a child's pipes, or in-process pipes in tests).
    pub fn over(name: &str, out: Box<dyn Read + Send>, stdin: Box<dyn Write + Send>, child: Option<Child>, root: &Path) -> Result<Client, String> {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            while let Some(v) = read_frame(&mut r) {
                if tx.send(v).is_err() { break }
            }
        });
        let mut c = Client { child, stdin, rx, id: 0, versions: HashMap::new(), diags: HashMap::new(), name: name.into() };
        let ru = uri(root);
        c.request("initialize", json!({"processId": std::process::id(), "rootUri": ru,
            "workspaceFolders": [{"uri": ru, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()}],
            "clientInfo": {"name": "qh", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"textDocument": {
                "synchronization": {"didSave": false},
                "publishDiagnostics": {"versionSupport": true},
                "hover": {"contentFormat": ["plaintext", "markdown"]},
                "definition": {}, "references": {}}}}), Duration::from_secs(60))?;
        c.notify("initialized", json!({}))?;
        Ok(c)
    }

    fn start(l: Lang, root: &Path) -> Result<Client, String> {
        let (cmd, args) = commands(l).iter().find_map(|(c, a)| on_path(c).map(|p| (p, *a)))
            .ok_or_else(|| format!("no language server for {l:?} on PATH"))?;
        let mut child = Command::new(&cmd).args(args).current_dir(root)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            .map_err(|e| format!("lsp: can't start {}: {e}", cmd.display()))?;
        let out = child.stdout.take().ok_or("no stdout")?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let name = cmd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        Client::over(&name, Box::new(out), Box::new(stdin), Some(child), root)
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        self.stdin.write_all(&encode(v)).and_then(|_| self.stdin.flush()).map_err(|e| format!("lsp {}: {e}", self.name))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Handle one incoming message that isn't the reply we're waiting for.
    fn absorb(&mut self, v: Value) {
        if v.get("method").is_some() && v.get("id").is_some() {
            // a server request: answer so it doesn't wait on us
            let _ = self.send(&json!({"jsonrpc": "2.0", "id": v["id"], "result": Value::Null}));
        } else if v["method"] == "textDocument/publishDiagnostics" {
            if let Some(u) = v["params"]["uri"].as_str() {
                self.diags.insert(u.to_string(), parse_diags(&v["params"]));
            }
        }
    }

    fn request(&mut self, method: &str, params: Value, wait: Duration) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self.rx.recv_timeout(left).map_err(|_| format!("lsp {}: no answer to {method}", self.name))?;
            if v.get("method").is_none() && v["id"].as_u64() == Some(id) {
                if let Some(e) = v.get("error") {
                    return Err(format!("lsp {}: {}", self.name, e["message"].as_str().unwrap_or("error")));
                }
                return Ok(v["result"].clone());
            }
            self.absorb(v);
        }
    }

    /// Drain anything already queued (diagnostics that arrived while idle).
    fn drain(&mut self) {
        while let Ok(v) = self.rx.try_recv() { self.absorb(v) }
    }

    /// Make the server see `text` as the file's content (didOpen once, then full didChange).
    fn sync(&mut self, path: &Path, text: &str) -> Result<i64, String> {
        let u = uri(path);
        let v = match self.versions.get(&u) {
            Some(n) => {
                let n = n + 1;
                self.notify("textDocument/didChange", json!({"textDocument": {"uri": u, "version": n}, "contentChanges": [{"text": text}]}))?;
                n
            }
            None => {
                self.notify("textDocument/didOpen", json!({"textDocument": {"uri": u, "languageId": lang_id(path), "version": 1, "text": text}}))?;
                1
            }
        };
        self.versions.insert(u, v);
        Ok(v)
    }

    /// Sync the new text and wait for this file's diagnostics; the new problems as text.
    pub fn diagnose(&mut self, path: &Path, text: &str, wait: Duration, shown: &str) -> String {
        self.drain();
        let u = uri(path);
        let before = self.diags.remove(&u).unwrap_or_default();
        if self.sync(path, text).is_err() { return String::new() }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(v) = self.rx.recv_timeout(left) else { break };
            let ours = v["method"] == "textDocument/publishDiagnostics" && v["params"]["uri"] == u.as_str();
            self.absorb(v);
            if ours {
                // servers often publish twice (quick parse, then semantic); take a short extra look
                let quiet = Instant::now() + Duration::from_millis(250).min(deadline.saturating_duration_since(Instant::now()));
                while let Ok(v) = self.rx.recv_timeout(quiet.saturating_duration_since(Instant::now())) { self.absorb(v) }
                break;
            }
        }
        match self.diags.get(&u) {
            Some(now) => format_new(shown, &before, now),
            None => { self.diags.insert(u, before); String::new() }
        }
    }

    fn position_request(&mut self, method: &str, path: &Path, line: u64, col: u64, extra: Value) -> Result<Value, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        if !self.versions.contains_key(&uri(path)) { self.sync(path, &text)?; }
        let mut p = json!({"textDocument": {"uri": uri(path)}, "position": {"line": line.saturating_sub(1), "character": col.saturating_sub(1)}});
        if let (Some(o), Some(e)) = (p.as_object_mut(), extra.as_object()) { o.extend(e.clone()) }
        self.request(method, p, Duration::from_secs(20))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

static CLIENTS: Mutex<Vec<((PathBuf, Lang), Option<Client>)>> = Mutex::new(Vec::new());

fn enabled() -> bool {
    crate::config::load_cached().lsp != "off"
}

/// Run `f` with the (lazily started) server for this file. None when off, unknown language,
/// no project root, or no server.
fn with<T>(file: &Path, f: impl FnOnce(&mut Client) -> T) -> Option<T> {
    if !enabled() { return None }
    let l = lang_of(file)?;
    let file = std::fs::canonicalize(file).ok()?;
    let root = root_of(&file, l)?;
    let mut all = CLIENTS.lock().unwrap_or_else(|e| e.into_inner());
    let key = (root.clone(), l);
    let i = match all.iter().position(|(k, _)| *k == key) {
        Some(i) => i,
        None => {
            all.push((key, Client::start(l, &root).ok())); // a failure is kept: no retry
            all.len() - 1
        }
    };
    all[i].1.as_mut().map(f)
}

/// After a successful write/edit: new diagnostics for that file, or "".
pub fn after_write(path: &str) -> String {
    let p = Path::new(path);
    let Ok(text) = std::fs::read_to_string(p) else { return String::new() };
    let wait = Duration::from_millis(crate::config::load_cached().lsp_wait_ms);
    let Ok(abs) = std::fs::canonicalize(p) else { return String::new() };
    with(p, |c| c.diagnose(&abs, &text, wait, path)).unwrap_or_default()
}

fn loc_line(v: &Value) -> Option<String> {
    let u = v["uri"].as_str().or(v["targetUri"].as_str())?;
    let r = if v.get("targetSelectionRange").is_some() { &v["targetSelectionRange"] } else { &v["range"] };
    let line = r["start"]["line"].as_u64()? + 1;
    let col = r["start"]["character"].as_u64().unwrap_or(0) + 1;
    let p = path_of(u);
    let src = std::fs::read_to_string(&p).ok().and_then(|t| t.lines().nth(line as usize - 1).map(|l| l.trim().chars().take(160).collect::<String>())).unwrap_or_default();
    Some(format!("{}:{line}:{col}: {src}", p.display()))
}

fn locations(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(loc_line).collect(),
        Value::Object(_) => loc_line(v).into_iter().collect(),
        _ => vec![],
    }
}

pub fn hover_text(v: &Value) -> String {
    let c = &v["contents"];
    let part = |x: &Value| x.as_str().map(String::from).or_else(|| x["value"].as_str().map(String::from)).unwrap_or_default();
    match c {
        Value::Array(a) => a.iter().map(part).collect::<Vec<_>>().join("\n"),
        _ => part(c),
    }
}

const NO_SERVER: &str = "no language server for this file (lsp off, not installed, or no project root); use grep / find_references instead";

/// The goto_definition / hover / lsp find_references tools.
pub fn tool(name: &str, cwd: &Path, a: &Value) -> Result<String, String> {
    let path = crate::tools::resolve(cwd, a["path"].as_str().unwrap_or(""));
    let n = |k: &str| a[k].as_u64().or_else(|| a[k].as_str().and_then(|s| s.parse().ok())).unwrap_or(1);
    let (line, col) = (n("line"), n("col"));
    let (method, extra) = match name {
        "goto_definition" => ("textDocument/definition", json!({})),
        "hover" => ("textDocument/hover", json!({})),
        _ => ("textDocument/references", json!({"context": {"includeDeclaration": true}})),
    };
    let r = with(&path, |c| c.position_request(method, &path, line, col, extra)).ok_or(NO_SERVER)??;
    Ok(match name {
        "hover" => {
            let t = hover_text(&r);
            if t.trim().is_empty() { "nothing to show at that position".into() } else { crate::tools::cap(t) }
        }
        _ => {
            let l = locations(&r);
            if l.is_empty() { "no results at that position (check line/col are 1-based and on the name)".into() } else { crate::tools::cap(l.join("\n")) }
        }
    })
}

/// Kill every server (qh is exiting).
pub fn shutdown() {
    CLIENTS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "result": {"x": "é"}});
        let b = json!({"jsonrpc": "2.0", "method": "m"});
        let mut wire = encode(&a);
        wire.extend(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n");
        wire.extend(encode(&b));
        let mut r = std::io::Cursor::new(wire);
        assert_eq!(read_frame(&mut r), Some(a));
        assert_eq!(read_frame(&mut r), Some(b));
        assert_eq!(read_frame(&mut r), None);
        assert!(String::from_utf8(encode(&json!("é"))).unwrap().starts_with("Content-Length: 4\r\n\r\n"));
    }

    fn d(line: u64, sev: u8, m: &str) -> Diag { Diag { line, col: 1, severity: sev, message: m.into() } }

    #[test]
    fn only_new_errors_and_warnings_are_shown() {
        let before = vec![d(3, 1, "old error")];
        let now = vec![d(9, 1, "old error"), d(5, 2, "unused x"), d(2, 1, "type mismatch"), d(1, 3, "info")];
        let s = format_new("a.rs", &before, &now);
        assert_eq!(s, "\nlsp: 1 new error(s), 1 new warning(s) in a.rs:\n  2:1: error: type mismatch\n  5:1: warning: unused x");
        assert_eq!(format_new("a.rs", &now, &now), "");
        let many: Vec<Diag> = (0..20).map(|i| d(i, 1, &format!("e{i}"))).collect();
        let s = format_new("a.rs", &[], &many);
        assert_eq!(s.trim_start().lines().count(), 1 + 15 + 1);
        assert!(s.ends_with("… 5 more"));
    }

    #[test]
    fn parses_diagnostics_and_hover() {
        let p = json!({"uri": "file:///a.rs", "diagnostics": [{"range": {"start": {"line": 4, "character": 2}}, "severity": 1, "message": "bad\nmore"}]});
        assert_eq!(parse_diags(&p), vec![Diag { line: 5, col: 3, severity: 1, message: "bad".into() }]);
        assert_eq!(hover_text(&json!({"contents": {"kind": "markdown", "value": "fn f()"}})), "fn f()");
        assert_eq!(hover_text(&json!({"contents": ["a", {"language": "rust", "value": "b"}]})), "a\nb");
        assert_eq!(path_of(&uri(Path::new("/tmp/a b.rs"))), PathBuf::from("/tmp/a b.rs"));
    }

    /// A fake server on in-process pipes: answers initialize, asks the client a question,
    /// publishes diagnostics for each change, and answers definition.
    #[test]
    fn talks_to_a_fake_server() {
        let (c_read, s_write) = std::io::pipe().unwrap();
        let (s_read, c_write) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let mut r = BufReader::new(s_read);
            let mut w = s_write;
            let mut changes = 0;
            while let Some(m) = read_frame(&mut r) {
                let method = m["method"].as_str().unwrap_or("").to_string();
                let out = match method.as_str() {
                    "initialize" => vec![json!({"jsonrpc": "2.0", "id": m["id"], "result": {"capabilities": {}}})],
                    "textDocument/didOpen" | "textDocument/didChange" => {
                        changes += 1;
                        let u = m["params"]["textDocument"]["uri"].clone();
                        let diags = if changes == 1 { json!([]) } else {
                            json!([{"range": {"start": {"line": 1, "character": 4}}, "severity": 1, "message": "mismatched types"}]) };
                        vec![json!({"jsonrpc": "2.0", "id": 99, "method": "workspace/configuration", "params": {}}),
                             json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": u, "diagnostics": diags}})]
                    }
                    "textDocument/definition" => vec![json!({"jsonrpc": "2.0", "id": m["id"], "result": [
                        {"uri": m["params"]["textDocument"]["uri"], "range": {"start": {"line": 0, "character": 3}}}]})],
                    _ => vec![],
                };
                for o in out { w.write_all(&encode(&o)).unwrap() }
            }
        });
        let dir = std::env::temp_dir().join(format!("qh-lsp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "fn main() {}\n").unwrap();
        let mut c = Client::over("fake", Box::new(c_read), Box::new(c_write), None, &dir).unwrap();
        let w = Duration::from_secs(3);
        assert_eq!(c.diagnose(&f, "fn main() {}\n", w, "a.rs"), "");
        let s = c.diagnose(&f, "fn main() {\n    let x: u8 = \"\";\n}\n", w, "a.rs");
        assert_eq!(s, "\nlsp: 1 new error(s), 0 new warning(s) in a.rs:\n  2:5: error: mismatched types");
        let r = c.position_request("textDocument/definition", &f, 1, 4, json!({})).unwrap();
        assert_eq!(locations(&r), vec![format!("{}:1:4: fn main() {{}}", f.display())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "needs rust-analyzer on PATH"]
    fn live_rust_analyzer() {
        let dir = std::env::temp_dir().join(format!("qh-lsp-live-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let f = dir.join("src/main.rs");
        std::fs::write(&f, "fn main() {}\n").unwrap();
        let mut c = Client::start(Lang::Rust, &dir).unwrap();
        let _ = c.diagnose(&f, "fn main() {}\n", Duration::from_secs(20), "main.rs");
        let s = c.diagnose(&f, "fn main() { let x: u8 = \"s\"; }\n", Duration::from_secs(20), "main.rs");
        assert!(s.contains("error"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
