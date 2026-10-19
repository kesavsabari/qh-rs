//! The line to `qh serve` for one project (apple/qh/ServeClient.swift): start it (or find the one
//! already running), read its token, connect to its Unix socket, say hello, then hand every event
//! line to the main loop. One reader thread per connection, no async runtime.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

pub enum Msg {
    Event(String, Value),
    Closed(String, String),
}

#[derive(Default)]
struct Inner {
    sock: Option<UnixStream>,
    child: Option<Child>,
    /// kept open, never read again: dropping it would make qh's next stdout write fail
    _out: Option<ChildStdout>,
}

#[derive(Clone)]
pub struct Client(Arc<Mutex<Inner>>);

/// ~/.local/bin/qh, else qh on PATH; QH_BIN wins.
pub fn qh_bin() -> String {
    if let Ok(b) = std::env::var("QH_BIN") { return b }
    let home = std::env::var("HOME").unwrap_or_default();
    let local = PathBuf::from(home).join(".local/bin/qh");
    if local.is_file() { local.to_string_lossy().into() } else { "qh".into() }
}

impl Client {
    pub fn start(path: String, tx: async_channel::Sender<Msg>) -> Client {
        let c = Client(Arc::new(Mutex::new(Inner::default())));
        let me = c.clone();
        std::thread::spawn(move || {
            let why = match me.run(&path, &tx) { Ok(()) => "qh serve stopped".to_string(), Err(e) => e };
            me.0.lock().unwrap().sock = None;
            let _ = tx.send_blocking(Msg::Closed(path, why));
        });
        c
    }

    fn run(&self, path: &str, tx: &async_channel::Sender<Msg>) -> Result<(), String> {
        let log = std::env::temp_dir().join(format!("qh-serve-{:x}.log", hash(path)));
        let err = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let mut child = Command::new(qh_bin()).arg("serve").current_dir(path).env("QH_EDITOR", "desktop")
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(err).spawn()
            .map_err(|e| format!("can't start qh: {e}"))?;
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (sock, token_file, ours) = loop {
            let mut line = String::new();
            if out.read_line(&mut line).unwrap_or(0) == 0 {
                let _ = child.wait();
                let msg = std::fs::read_to_string(&log).unwrap_or_default().trim().to_string();
                return Err(if msg.is_empty() { "qh serve didn't start".into() } else { msg });
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let t = v["type"].as_str().unwrap_or("");
            if t == "listening" || t == "running" {
                break (v["socket"].as_str().unwrap_or("").to_string(), v["token_file"].as_str().unwrap_or("").to_string(), t == "listening");
            }
        };
        let token = std::fs::read_to_string(&token_file).map_err(|e| format!("token: {e}"))?.trim().to_string();
        let s = UnixStream::connect(&sock).map_err(|e| format!("can't connect to qh serve: {e}"))?;
        {
            let mut g = self.0.lock().unwrap();
            g.sock = Some(s.try_clone().map_err(|e| e.to_string())?);
            if ours { g.child = Some(child); g._out = Some(out.into_inner()); }
        }
        self.send(json!({"type": "hello", "token": token}));
        for line in BufReader::new(s).lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send_blocking(Msg::Event(path.to_string(), v)).is_err() { break }
            }
        }
        Ok(())
    }

    pub fn send(&self, v: Value) {
        let mut g = self.0.lock().unwrap();
        if let Some(s) = g.sock.as_mut() {
            if s.write_all(format!("{v}\n").as_bytes()).is_err() { g.sock = None }
        }
    }

    /// Ends the server only if this app started it; one you started yourself keeps running.
    pub fn stop(&self) {
        let mut g = self.0.lock().unwrap();
        if let Some(s) = g.sock.take() { let _ = s.shutdown(std::net::Shutdown::Both); }
        if let Some(mut c) = g.child.take() { let _ = c.kill(); let _ = c.wait(); }
    }
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
