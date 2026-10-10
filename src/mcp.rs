//! MCP client over stdio: each `[mcp.<name>]` server (cmd, args, env, trusted) is started on
//! first use, its tools listed and offered to the model as `<name>__<tool>`. Newline-delimited
//! JSON-RPC 2.0; a reader thread per server lets every request time out instead of hanging.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone)]
pub struct ServerCfg {
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub trusted: bool, // run its tools without asking
}

struct Server {
    cfg: ServerCfg,
    _child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    id: u64,
    tools: Vec<Value>,
}

impl Server {
    fn start(cfg: &ServerCfg) -> Result<Server, String> {
        let mut child = Command::new(&cfg.cmd).args(&cfg.args).envs(cfg.env.iter().cloned())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            .map_err(|e| format!("mcp {}: can't start {}: {e}", cfg.name, cfg.cmd))?;
        let (tx, lines) = channel();
        let out = child.stdout.take().ok_or("no stdout")?;
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() { break }
            }
        });
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let mut s = Server { cfg: cfg.clone(), _child: child, stdin, lines, id: 0, tools: vec![] };
        s.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "qh", "version": env!("CARGO_PKG_VERSION")}}), 60)?;
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))?;
        let list = s.request("tools/list", json!({}), 30)?;
        s.tools = list["tools"].as_array().cloned().unwrap_or_default();
        Ok(s)
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        writeln!(self.stdin, "{v}").and_then(|_| self.stdin.flush()).map_err(|e| format!("mcp {}: {e}", self.cfg.name))
    }

    fn request(&mut self, method: &str, params: Value, secs: u64) -> Result<Value, String> {
        self.id += 1;
        let id = self.id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self.lines.recv_timeout(left)
                .map_err(|_| format!("mcp {}: no answer to {method} within {secs} s", self.cfg.name))?;
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue }; // logs on stdout
            if v["id"].as_u64() != Some(id) {
                continue; // notifications, or a reply to a request that already timed out
            }
            if let Some(e) = v.get("error") {
                return Err(format!("mcp {}: {}", self.cfg.name, e["message"].as_str().unwrap_or(&e.to_string())));
            }
            return Ok(v["result"].clone());
        }
    }
}

static SERVERS: OnceLock<Vec<Mutex<Option<Server>>>> = OnceLock::new();
static CFGS: OnceLock<Vec<ServerCfg>> = OnceLock::new();
static ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Start every configured server (in parallel, once). Failures are kept for `/mcp`.
pub fn init(cfgs: &[ServerCfg]) {
    let _ = CFGS.set(cfgs.to_vec());
    SERVERS.get_or_init(|| {
        let started: Vec<_> = std::thread::scope(|sc| {
            let hs: Vec<_> = cfgs.iter().map(|c| sc.spawn(move || Server::start(c))).collect();
            hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("panicked".into()))).collect()
        });
        started.into_iter().map(|r| Mutex::new(r.map_err(|e| ERRORS.lock().unwrap().push(e)).ok())).collect()
    });
}

fn servers() -> &'static [Mutex<Option<Server>>] {
    SERVERS.get().map(|v| v.as_slice()).unwrap_or(&[])
}

pub const SEP: &str = "__";

pub fn is_mcp(name: &str) -> bool {
    name.contains(SEP) && servers().iter().any(|s| s.lock().unwrap().as_ref().is_some_and(|s| name.starts_with(&format!("{}{SEP}", s.cfg.name))))
}

pub fn trusted(name: &str) -> bool {
    CFGS.get().into_iter().flatten().any(|c| c.trusted && name.starts_with(&format!("{}{SEP}", c.name)))
}

/// Tool schemas in the function-calling shape.
pub fn schemas() -> Vec<Value> {
    let mut out = vec![];
    for s in servers() {
        let g = s.lock().unwrap();
        let Some(s) = g.as_ref() else { continue };
        for t in &s.tools {
            let mut params = t["inputSchema"].clone();
            if !params.is_object() { params = json!({"type": "object", "properties": {}}) }
            out.push(json!({"type": "function", "function": {
                "name": format!("{}{SEP}{}", s.cfg.name, t["name"].as_str().unwrap_or("")),
                "description": t["description"].as_str().unwrap_or("").chars().take(300).collect::<String>(),
                "parameters": params}}));
        }
    }
    out
}

pub fn call(name: &str, args: &Value) -> String {
    let Some((srv, tool)) = name.split_once(SEP) else { return format!("error: unknown tool {name}") };
    for s in servers() {
        let mut g = s.lock().unwrap();
        let Some(s) = g.as_mut().filter(|s| s.cfg.name == srv) else { continue };
        return match s.request("tools/call", json!({"name": tool, "arguments": args}), 120) {
            Ok(r) => {
                let text: Vec<String> = r["content"].as_array().into_iter().flatten().map(|c| match c["type"].as_str() {
                    Some("text") => c["text"].as_str().unwrap_or("").to_string(),
                    Some(t) => format!("[{t} content not shown]"),
                    None => c.to_string(),
                }).collect();
                let text = text.join("\n");
                if r["isError"].as_bool() == Some(true) { format!("error: {text}") } else { crate::tools::cap(text) }
            }
            Err(e) => format!("error: {e}"),
        };
    }
    format!("error: MCP server {srv} is not running (/mcp shows why)")
}

/// One line per server for `/mcp`.
pub fn status() -> String {
    let mut rows: Vec<String> = servers().iter().filter_map(|s| {
        let g = s.lock().unwrap();
        g.as_ref().map(|s| format!("{}: {} tools{}", s.cfg.name, s.tools.len(), if s.cfg.trusted { " (trusted)" } else { "" }))
    }).collect();
    rows.extend(ERRORS.lock().unwrap().iter().map(|e| format!("failed: {e}")));
    if rows.is_empty() { "no MCP servers: add [mcp.<name>] with cmd and args to config.toml".into() } else { rows.join("\n") }
}
