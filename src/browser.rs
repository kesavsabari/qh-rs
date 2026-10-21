//! Browser tools over the Chrome DevTools Protocol, spoken directly: a system
//! Chromium with its own profile (never the user's real Chrome), one page,
//! a minimal WebSocket client. One browser per process, calls serialized.
use crate::config::state_dir;
use crate::hub::lock;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const TEXT_PAGE: usize = 3500; // chars of page text per snapshot page
const MAX_ELEMENTS: usize = 60;

const SNAPSHOT_JS: &str = r#"(() => {
  const sel = 'a[href], button, input, textarea, select, summary, [role=button], [role=link], [role=tab], [role=checkbox], [role=menuitem], [onclick], [contenteditable=true]';
  const out = [];
  document.querySelectorAll('[data-qh]').forEach(e => e.removeAttribute('data-qh'));
  let n = 0;
  for (const el of document.querySelectorAll(sel)) {
    const r = el.getBoundingClientRect();
    const st = getComputedStyle(el);
    if (!r.width || !r.height || st.visibility === 'hidden' || st.display === 'none') continue;
    if (el.type === 'hidden') continue;
    n += 1;
    el.setAttribute('data-qh', String(n));
    const tag = el.tagName.toLowerCase();
    const role = el.getAttribute('role') || (tag === 'a' ? 'link' : tag === 'input' ? (el.type || 'text') : tag);
    const field = ['input', 'select', 'textarea'].includes(tag);
    const label = el.labels && el.labels[0] ? el.labels[0].innerText : '';
    let name = field
      ? (el.getAttribute('aria-label') || label || el.placeholder || el.title || '')
      : (el.getAttribute('aria-label') || el.innerText || el.value || el.title || el.alt || '');
    if (!name && el.name) name = el.name;
    name = name.replace(/\s+/g, ' ').trim().slice(0, 80);
    let extra = '';
    if (tag === 'input' || tag === 'textarea') extra = el.value ? ` value="${String(el.value).slice(0, 40)}"` : '';
    if (tag === 'select') extra = ` selected="${el.value}"` + ' options=' + JSON.stringify([...el.options].map(o => o.text).slice(0, 10));
    if (el.type === 'password') extra = ' (password)';
    const editable = el.isContentEditable || tag === 'textarea' || (tag === 'input' &&
      !['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'image', 'range', 'color'].includes(el.type));
    out.push({ref: n, role, name, extra, editable, password: el.type === 'password', href: tag === 'a' ? (el.getAttribute('href') || '') : ''});
  }
  const nav = performance.getEntriesByType('navigation')[0];
  return {status: nav ? nav.responseStatus || 0 : 0, title: document.title, url: location.href,
          text: (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').trim(), elements: out};
})()"#;

struct Ws {
    s: TcpStream,
    next: u64,
    problems: Vec<String>,                          // page errors seen since the last snapshot
    requests: std::collections::HashMap<String, String>, // request id -> url, to name failed loads
}

/// Page problems kept between snapshots, and shown at most this many.
const MAX_PROBLEMS: usize = 12;

impl Ws {
    fn connect(url: &str) -> Result<Ws, String> {
        let rest = url.strip_prefix("ws://").ok_or("bad ws url")?;
        let (host, path) = rest.split_once('/').ok_or("bad ws url")?;
        let mut s = TcpStream::connect(host).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(Duration::from_secs(60))).ok();
        s.set_nodelay(true).ok();
        write!(s, "GET /{path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                   Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut r = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
        let mut line = String::new();
        r.read_line(&mut line).map_err(|e| e.to_string())?;
        if !line.contains(" 101 ") {
            return Err(format!("devtools handshake failed: {}", line.trim()));
        }
        loop {
            line.clear();
            r.read_line(&mut line).map_err(|e| e.to_string())?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        Ok(Ws { s, next: 0, problems: vec![], requests: Default::default() })
    }

    fn send(&mut self, text: &str) -> std::io::Result<()> {
        let b = text.as_bytes();
        let mut f = vec![0x81u8];
        match b.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65536 => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let mask = (Instant::now().elapsed().as_nanos() as u32 ^ 0x5a3c_9e17).to_be_bytes();
        f.extend_from_slice(&mask);
        f.extend(b.iter().enumerate().map(|(i, c)| c ^ mask[i % 4]));
        self.s.write_all(&f)
    }

    /// Next complete text message (answers pings, joins fragments).
    fn recv(&mut self) -> std::io::Result<String> {
        let mut msg = vec![];
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h)?;
            let (fin, op) = (h[0] & 0x80 != 0, h[0] & 0x0f);
            let mut len = (h[1] & 0x7f) as u64;
            if len == 126 {
                let mut l = [0u8; 2];
                self.s.read_exact(&mut l)?;
                len = u16::from_be_bytes(l) as u64;
            } else if len == 127 {
                let mut l = [0u8; 8];
                self.s.read_exact(&mut l)?;
                len = u64::from_be_bytes(l);
            }
            let mut data = vec![0u8; len as usize];
            self.s.read_exact(&mut data)?;
            match op {
                0x8 => return Err(std::io::Error::other("devtools closed the connection")),
                0x9 => {
                    let mut f = vec![0x8a, 0x80 | data.len() as u8, 0, 0, 0, 0];
                    f.extend(&data);
                    self.s.write_all(&f)?;
                }
                0xa => {}
                _ => {
                    msg.extend(data);
                    if fin {
                        return Ok(String::from_utf8_lossy(&msg).into_owned());
                    }
                }
            }
        }
    }

    /// Keep what tells the model a page is broken: console errors, uncaught exceptions, requests
    /// that failed or came back 4xx/5xx. Everything else DevTools sends is dropped.
    fn event(&mut self, m: &Value) {
        let p = &m["params"];
        let short = |s: &str| s.lines().next().unwrap_or("").chars().take(200).collect::<String>();
        let line = match m["method"].as_str().unwrap_or("") {
            "Runtime.consoleAPICalled" if p["type"] == "error" || p["type"] == "assert" => {
                let args: Vec<String> = p["args"].as_array().into_iter().flatten()
                    .map(|a| a["value"].as_str().map(String::from).or_else(|| a["description"].as_str().map(String::from)).unwrap_or_else(|| a["value"].to_string()))
                    .collect();
                Some(format!("console.error: {}", short(&args.join(" "))))
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let what = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("exception");
                let line = d["lineNumber"].as_u64().unwrap_or(0) + 1;
                let at = match d["url"].as_str().map(|u| u.rsplit('/').next().unwrap_or(u)).filter(|f| !f.is_empty()) {
                    Some(file) => format!(" ({file}:{line})"),
                    None => format!(" (inline script, line {line})"),
                };
                Some(format!("uncaught {}{at}", short(what)))
            }
            "Network.requestWillBeSent" => {
                if self.requests.len() > 2000 { self.requests.clear() }
                self.requests.insert(p["requestId"].as_str().unwrap_or("").into(), p["request"]["url"].as_str().unwrap_or("").into());
                None
            }
            // every page asks for /favicon.ico: a missing one isn't the page's problem
            "Network.responseReceived" if p["response"]["status"].as_u64().is_some_and(|s| s >= 400)
                && !p["response"]["url"].as_str().unwrap_or("").ends_with("/favicon.ico") =>
                Some(format!("HTTP {} {}", p["response"]["status"], short(p["response"]["url"].as_str().unwrap_or("")))),
            "Network.loadingFailed" if p["canceled"] != true => {
                let url = self.requests.get(p["requestId"].as_str().unwrap_or("")).cloned().unwrap_or_default();
                Some(format!("request failed ({}): {}", p["errorText"].as_str().unwrap_or("error"), short(&url)))
            }
            _ => None,
        };
        if let Some(l) = line {
            if self.problems.len() < 50 && !self.problems.contains(&l) {
                self.problems.push(l);
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"id": id, "method": method, "params": params}).to_string()).map_err(|e| e.to_string())?;
        loop {
            let m: Value = serde_json::from_str(&self.recv().map_err(|e| e.to_string())?).unwrap_or_default();
            if m["method"].is_string() {
                self.event(&m);
                continue;
            }
            if m["id"].as_u64() == Some(id) {
                if let Some(e) = m.get("error") {
                    return Err(e["message"].as_str().unwrap_or("devtools error").to_string());
                }
                return Ok(m["result"].clone());
            }
        }
    }
}

struct Browser {
    /// None when qh attached to a browser an earlier qh left running
    child: Option<Child>,
    ws: Ws,
    refs: Vec<Value>,
}

static B: LazyLock<Mutex<Option<Browser>>> = LazyLock::new(|| Mutex::new(None));
static SHOT: Mutex<Option<String>> = Mutex::new(None);

fn launch(headless: bool) -> Result<Browser, String> {
    let profile = state_dir().join("browser");
    let _ = std::fs::create_dir_all(&profile);
    let port_file = profile.join("DevToolsActivePort");
    // A browser still holding this profile (a window qh lost track of, or one left by an earlier
    // qh) makes a new one hand over to it and quit, so it would never start: use that one instead.
    if let Some(port) = live_port(&port_file) {
        return connect(port, None);
    }
    let _ = std::fs::remove_file(&port_file);
    let mut args = vec![format!("--user-data-dir={}", profile.display()), "--remote-debugging-port=0".into(),
                        "--no-first-run".into(), "--no-default-browser-check".into(), "--window-size=1100,760".into()];
    if headless {
        args.push("--headless=new".into());
    }
    args.push("about:blank".into());
    let child = crate::platform::browsers().iter().find_map(|b| {
        let mut c = Command::new(b);
        c.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // its own process group: the terminal's title and Ctrl+C stay with qh
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c.spawn().ok()
    }).ok_or("no Chrome, Chromium, Edge or Brave found")?;
    let t0 = Instant::now();
    let port = loop {
        if let Some(p) = std::fs::read_to_string(&port_file).ok().and_then(|s| s.lines().next()?.trim().parse::<u16>().ok()) {
            break p;
        }
        if t0.elapsed() > Duration::from_secs(15) {
            return Err(format!("the browser did not start. If a qh browser window is open, quit it (all of its \
                                windows) and try again. Its profile: {}", profile.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    connect(port, Some(child))
}

/// The DevTools port qh's browser profile last wrote, without checking it answers: the sandbox
/// keeps commands away from it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn devtools_port() -> Option<u16> {
    std::fs::read_to_string(state_dir().join("browser").join("DevToolsActivePort")).ok()?.lines().next()?.trim().parse().ok()
}

/// The DevTools port of a browser already running on qh's profile, if it still answers.
fn live_port(port_file: &std::path::Path) -> Option<u16> {
    let port = std::fs::read_to_string(port_file).ok()?.lines().next()?.trim().parse::<u16>().ok()?;
    let ok = ureq::AgentBuilder::new().timeout(Duration::from_secs(2)).build()
        .get(&format!("http://127.0.0.1:{port}/json/version")).call().is_ok();
    ok.then_some(port)
}

fn connect(port: u16, child: Option<Child>) -> Result<Browser, String> {
    let list: Value = crate::backend::http().get(&format!("http://127.0.0.1:{port}/json/list")).call()
        .map_err(|e| e.to_string())?.into_string().ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let mut page = list.as_array().into_iter().flatten().find(|t| t["type"] == "page").cloned();
    if page.is_none() {
        // every window was closed but the browser kept running, as it does on macOS
        page = crate::backend::http().request("PUT", &format!("http://127.0.0.1:{port}/json/new?about:blank")).call()
            .ok().and_then(|r| r.into_string().ok()).and_then(|s| serde_json::from_str(&s).ok());
    }
    let url = page.as_ref().and_then(|t| t["webSocketDebuggerUrl"].as_str()).map(String::from).ok_or("no page target")?;
    let mut ws = Ws::connect(&url)?;
    ws.call("Page.enable", json!({}))?;
    // console errors, exceptions and failed requests come back with each snapshot
    ws.call("Runtime.enable", json!({}))?;
    ws.call("Network.enable", json!({}))?;
    Ok(Browser { child, ws, refs: vec![] })
}

pub fn shutdown() {
    if let Some(mut b) = lock(&B).take() {
        let _ = b.ws.call("Browser.close", json!({}));
        if let Some(c) = b.child.as_mut() {
            let _ = c.wait();
        }
    }
}

/// The screenshot taken by the last `screenshot` call, for the engine to attach.
pub fn take_image() -> Option<String> {
    lock(&SHOT).take()
}

/// `/allow <address>`: the browser rules for a web address the user lets this session open, or
/// None when it isn't one (then it's an ordinary rule). localhost and 127.0.0.1 go together.
pub fn allow_rules(arg: &str) -> Option<Vec<String>> {
    let a = arg.trim();
    let rest = a.strip_prefix("http://").or_else(|| a.strip_prefix("https://"));
    let auth = rest.unwrap_or(a).split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = match auth.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        _ => (auth, None),
    };
    let named = matches!(host, "localhost" | "127.0.0.1") || host.ends_with(".local") || host.ends_with(".lan")
        || host.parse::<std::net::Ipv4Addr>().is_ok();
    // a bare word ("git*", "pytest") stays a command rule; an address is a URL, has a port, or is a known local name
    if host.is_empty() || host.contains(['*', ' ']) || !(rest.is_some() || port.is_some() || named) {
        return None;
    }
    let hosts: Vec<&str> = if matches!(host, "localhost" | "127.0.0.1") { vec!["localhost", "127.0.0.1"] } else { vec![host] };
    Some(hosts.iter().map(|h| match port { Some(p) => format!("browse:{h}:{p}"), None => format!("browse:{h}") }).collect())
}

fn private(host: &str) -> bool {
    if host == "localhost" || [".local", ".internal", ".lan"].iter().any(|s| host.ends_with(s)) {
        return true;
    }
    (host, 80).to_socket_addrs().ok().and_then(|mut a| a.next()).is_some_and(|a| match a.ip() {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
    })
}

fn check_url(url: &str, allow: &[String]) -> Option<String> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    if scheme != "http" && scheme != "https" {
        return Some(format!("error: only http(s) URLs are allowed, got {}", if scheme.is_empty() { "none" } else { scheme }));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let port = if host.starts_with('[') { host.split_once("]:").map(|(_, p)| p) } else { host.split_once(':').map(|(_, p)| p) }
        .unwrap_or(if scheme == "https" { "443" } else { "80" }).to_string();
    let host = if host.starts_with('[') { host.trim_start_matches('[').split(']').next().unwrap_or("") } else { host.split(':').next().unwrap_or("") };
    // an entry is a host, or host:port for one port only (the tab's own dev server)
    if !allow.iter().any(|h| h == host || *h == format!("{host}:{port}")) && private(host) {
        // say why and what to do: the model can't change config, and "add it to browser_allow_hosts"
        // left both it and the user stuck (seen live on a Mac: port 3000 held by another program)
        let cfg = crate::platform::config_base().join("config.toml");
        if matches!(host, "localhost" | "127.0.0.1" | "::1") {
            return Some(format!("error: {host}:{port} isn't a server this tab started; the browser opens only those. \
Start the app's dev server with bash in the background (pick a free port if {port} is taken) and browse that port. \
To open a server that was already running, the user types `/allow localhost:{port}` (this session) or adds \"localhost:{port}\" to browser_allow_hosts in {}.", cfg.display()));
        }
        return Some(format!("error: {host} is on the local network, which the browser doesn't open unless the user \
types `/allow {host}` (this session) or adds \"{host}\" to browser_allow_hosts in {}.", cfg.display()));
    }
    None
}

impl Browser {
    fn eval(&mut self, js: &str) -> Result<Value, String> {
        let r = self.ws.call("Runtime.evaluate", json!({"expression": js, "returnByValue": true, "awaitPromise": true}))?;
        if let Some(e) = r.get("exceptionDetails") {
            return Err(e["exception"]["description"].as_str().or(e["text"].as_str()).unwrap_or("script error")
                .lines().next().unwrap_or("").to_string());
        }
        Ok(r["result"]["value"].clone())
    }

    fn wait_ready(&mut self) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            match self.eval("document.readyState") {
                Ok(v) if v != "loading" => return,
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    fn snapshot(&mut self, page: usize) -> Result<String, String> {
        self.wait_ready();
        let d = self.eval(SNAPSHOT_JS)?;
        self.refs = d["elements"].as_array().cloned().unwrap_or_default();
        let mut lines: Vec<String> = self.refs.iter().take(MAX_ELEMENTS).map(|e| {
            let href = e["href"].as_str().unwrap_or("");
            let to = if href.is_empty() || href.starts_with("javascript:") { String::new() } else {
                format!(" → {}", href.chars().take(80).collect::<String>())
            };
            format!("[{}] {} \"{}\"{}{to}", e["ref"], e["role"].as_str().unwrap_or(""), e["name"].as_str().unwrap_or(""),
                    e["extra"].as_str().unwrap_or(""))
        }).collect();
        if self.refs.len() > MAX_ELEMENTS {
            lines.push(format!("… {} more elements (scroll to see others)", self.refs.len() - MAX_ELEMENTS));
        }
        let text: Vec<char> = d["text"].as_str().unwrap_or("").chars().collect();
        let pages = text.len().div_ceil(TEXT_PAGE).max(1);
        let page = page.clamp(1, pages);
        let chunk: String = text[((page - 1) * TEXT_PAGE).min(text.len())..(page * TEXT_PAGE).min(text.len())].iter().collect();
        let more = if page < pages { format!("\n(text page {page}/{pages}; snapshot(page={}) for more)", page + 1) } else { String::new() };
        let status = d["status"].as_u64().unwrap_or(0);
        let warn = if status >= 400 {
            format!("HTTP {status}: this page does not exist or failed. Don't guess URLs; go back and click a [ref] \
                     link, or use a URL shown after → in a snapshot.\n")
        } else {
            String::new()
        };
        let problems = std::mem::take(&mut self.ws.problems);
        // said even when clean, so the model knows the check ran
        let problems = if problems.is_empty() { "\n\nNo console errors or failed requests.".to_string() } else {
            let more = problems.len().saturating_sub(MAX_PROBLEMS);
            format!("\n\nPage problems since the last action (console errors, failed requests):\n{}{}",
                    problems.iter().take(MAX_PROBLEMS).map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"),
                    if more > 0 { format!("\n- … {more} more") } else { String::new() })
        };
        Ok(format!("{warn}Title: {}\nURL: {}\n\nElements:\n{}\n\nText:\n{chunk}{more}{problems}", d["title"].as_str().unwrap_or(""),
                   d["url"].as_str().unwrap_or(""), lines.join("\n")))
    }

    fn el(&self, r: i64) -> Result<&Value, String> {
        self.refs.iter().find(|e| e["ref"] == r).ok_or_else(|| format!("no element [{r}] on this page; call snapshot() for fresh refs"))
    }

    /// Scroll element `r` into view and return its centre.
    fn centre(&mut self, r: i64) -> Result<(f64, f64), String> {
        self.el(r)?;
        let v = self.eval(&format!("(() => {{ const e = document.querySelector('[data-qh=\"{r}\"]'); if (!e) return null; \
            e.scrollIntoView({{block: 'center', behavior: 'instant'}}); const b = e.getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; }})()"))?;
        match (v[0].as_f64(), v[1].as_f64()) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err("page changed since the last snapshot; call snapshot()".into()),
        }
    }

    fn url(&mut self) -> String {
        self.eval("location.href").ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
    }

    fn key(&mut self, key: &str, code: i64, text: &str) -> Result<(), String> {
        for t in ["keyDown", "keyUp"] {
            self.ws.call("Input.dispatchKeyEvent", json!({"type": t, "key": key, "code": key,
                "windowsVirtualKeyCode": code, "text": if t == "keyDown" { text } else { "" }}))?;
        }
        Ok(())
    }
}

fn sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

pub fn label(a: &Value) -> String {
    let r = a["ref"].as_i64().unwrap_or(-1);
    lock(&B).as_ref().and_then(|b| b.refs.iter().find(|e| e["ref"] == r)).and_then(|e| e["name"].as_str())
        .unwrap_or("").to_string()
}

pub fn call(name: &str, a: &Value, headless: bool, allow: &[String]) -> String {
    let mut g = lock(&B);
    if g.as_mut().is_some_and(|b| b.child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some())
                               || b.ws.call("Runtime.evaluate", json!({"expression": "1"})).is_err()) {
        // the window was closed or the browser stopped answering: don't leave it running holding the
        // profile, or every later start hands over to it and quits
        if let Some(mut b) = g.take() {
            if let Some(c) = b.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
    if g.is_none() {
        match launch(headless) {
            Ok(b) => *g = Some(b),
            Err(e) => return format!("error: {e}"),
        }
    }
    let b = g.as_mut().unwrap();
    let r = a["ref"].as_i64().unwrap_or(-1);
    let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
    let res = (|| -> Result<String, String> {
        match name {
            "browse" => {
                let mut url = s("url");
                if !url.contains("://") {
                    url = format!("https://{url}");
                }
                if let Some(e) = check_url(&url, allow) {
                    return Ok(e);
                }
                let nav = b.ws.call("Page.navigate", json!({"url": url}))?;
                if let Some(e) = nav["errorText"].as_str() {
                    return Err(format!("{e} loading {url}"));
                }
                sleep(300);
                b.snapshot(1)
            }
            "snapshot" => b.snapshot(a["page"].as_u64().unwrap_or(1) as usize),
            "click" => {
                let (x, y) = b.centre(r)?;
                for t in ["mousePressed", "mouseReleased"] {
                    b.ws.call("Input.dispatchMouseEvent", json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": 1}))?;
                }
                sleep(400);
                let url = b.url();
                if let Some(e) = check_url(&url, allow) {
                    b.eval("history.back()")?;
                    return Ok(e);
                }
                b.snapshot(1)
            }
            "type" => {
                let el = b.el(r)?;
                if el["password"].as_bool() == Some(true) {
                    return Ok("error: typing into password fields is not allowed; ask the user to do it".into());
                }
                // typing into a button or link puts the text wherever focus was left, corrupting another field
                let role = el["role"].as_str().unwrap_or("").to_string();
                if el["editable"].as_bool() != Some(true) {
                    let how = if role == "select" { "select" } else { "click" };
                    return Ok(format!("error: [{r}] is a {role}, not a text field; use {how} for it"));
                }
                b.centre(r)?;
                b.eval(&format!("(() => {{ const e = document.querySelector('[data-qh=\"{r}\"]'); e.focus(); \
                    if (e.isContentEditable) {{ document.execCommand('selectAll'); }} else if (e.select) {{ e.select(); }} }})()"))?;
                b.ws.call("Input.insertText", json!({"text": s("text")}))?;
                if a["submit"].as_bool() == Some(true) {
                    b.key("Enter", 13, "\r")?;
                    sleep(600);
                }
                b.snapshot(1)
            }
            "select" => {
                b.el(r)?;
                let ok = b.eval(&format!("(() => {{ const e = document.querySelector('[data-qh=\"{r}\"]'); \
                    const o = [...e.options].find(o => o.text.trim() === {0}.trim()); if (!o) return false; \
                    e.value = o.value; e.dispatchEvent(new Event('input', {{bubbles: true}})); \
                    e.dispatchEvent(new Event('change', {{bubbles: true}})); return true; }})()", json!(s("option"))))?;
                if ok != true {
                    return Ok(format!("error: no option \"{}\" in [{r}]", s("option")));
                }
                b.snapshot(1)
            }
            "scroll" => {
                b.eval(&format!("window.scrollBy(0, {})", if s("direction") == "up" { -600 } else { 600 }))?;
                sleep(300);
                b.snapshot(1)
            }
            "back" => {
                b.eval("history.back()")?;
                sleep(500);
                b.snapshot(1)
            }
            "screenshot" => {
                let shot = b.ws.call("Page.captureScreenshot", json!({"format": "jpeg", "quality": 60}))?;
                *lock(&SHOT) = shot["data"].as_str().map(String::from);
                Ok(format!("screenshot of {} attached", b.url()))
            }
            _ => Err(format!("unknown browser tool {name}")),
        }
    })();
    res.unwrap_or_else(|e| format!("error: {}", e.chars().take(200).collect::<String>()))
}

pub const TOOLS: [&str; 8] = ["browse", "snapshot", "click", "type", "select", "scroll", "back", "screenshot"];

pub fn schemas() -> Vec<Value> {
    use crate::tools::schema;
    vec![
        schema("browse", "Open a URL in a real browser (click, type, scroll tools follow).", &[("url", "string")], &["url"]),
        schema("snapshot", "Re-read the current page. page=2.. for more text. Console errors and failed requests since the last action are listed at the end.", &[("page", "integer")], &[]),
        schema("click", "Click element [ref] from the latest snapshot.", &[("ref", "integer")], &["ref"]),
        schema("type", "Fill element [ref] with text; submit=true presses Enter.", &[("ref", "integer"), ("text", "string"), ("submit", "boolean")], &["ref", "text"]),
        schema("select", "Choose an option (by visible label) in select [ref].", &[("ref", "integer"), ("option", "string")], &["ref", "option"]),
        schema("scroll", "Scroll the page up or down.", &[("direction", "string")], &[]),
        schema("back", "Go back one page.", &[], &[]),
        schema("screenshot", "See the page as an image.", &[], &[]),
    ]
}

#[cfg(test)]
mod url_tests {
    use super::{allow_rules, check_url};

    #[test]
    fn a_port_entry_opens_that_port_only() {
        let allow = ["localhost:3001".to_string()];
        assert!(check_url("http://localhost:3001/", &allow).is_none());
        assert!(check_url("http://localhost:3002/", &allow).is_some(), "another local port stays blocked");
        assert!(check_url("http://localhost/", &allow).is_some(), "port 80 is not 3001");
        let e = check_url("http://localhost:3001/", &[]).unwrap();
        assert!(e.contains("this tab started") && e.contains("/allow localhost:3001") && e.contains("\"localhost:3001\" to browser_allow_hosts"), "{e}");
        let r = |s: &str| allow_rules(s);
        assert_eq!(r("localhost:57961"), Some(vec!["browse:localhost:57961".into(), "browse:127.0.0.1:57961".into()]));
        assert_eq!(r("http://127.0.0.1:8080/app"), Some(vec!["browse:localhost:8080".into(), "browse:127.0.0.1:8080".into()]));
        assert_eq!(r("printer.local"), Some(vec!["browse:printer.local".into()]));
        assert_eq!(r("git push*"), None, "a command rule stays one");
        assert_eq!(r("pytest"), None);
        assert_eq!(r("net:pypi.org"), None);
        // what /allow added reaches check_url as browser_allow_hosts
        let extra: Vec<String> = r("localhost:57961").unwrap().iter().map(|x| x[7..].to_string()).collect();
        assert!(check_url("http://localhost:57961/", &extra).is_none() && check_url("http://localhost:57962/", &extra).is_some());
        assert!(check_url("http://localhost:9/", &["localhost".to_string()]).is_none(), "a bare host allows every port");
    }
}
