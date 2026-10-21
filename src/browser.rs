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

// `__ROOT__` is replaced with a JSON selector or null: every element still gets a ref (so refs stay
// page-wide), but only those inside the region are marked `inside`, and `region` is its text.
const SNAPSHOT_JS: &str = r#"((rootSel) => {
  const root = rootSel ? document.querySelector(rootSel) : null;
  if (rootSel && !root) return {missing: true};
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
    // the value goes back whole; Rust shortens it with an explicit marker
    const value = (tag === 'input' || tag === 'textarea') && el.type !== 'password' ? String(el.value || '') : '';
    if (tag === 'select') extra = ` selected="${el.value}"` + ' options=' + JSON.stringify([...el.options].map(o => o.text).slice(0, 10));
    if (el.type === 'password') extra = ' (password)';
    const editable = el.isContentEditable || tag === 'textarea' || (tag === 'input' &&
      !['button', 'submit', 'reset', 'checkbox', 'radio', 'file', 'image', 'range', 'color'].includes(el.type));
    out.push({ref: n, role, name, extra, value, inside: !root || root.contains(el), editable, password: el.type === 'password', href: tag === 'a' ? (el.getAttribute('href') || '') : ''});
  }
  const nav = performance.getEntriesByType('navigation')[0];
  return {status: nav ? nav.responseStatus || 0 : 0, title: document.title, url: location.href,
          text: (document.body ? document.body.innerText : '').replace(/\n{3,}/g, '\n\n').trim(),
          region: root ? root.innerText.replace(/\n{3,}/g, '\n\n').trim() : null, elements: out};
})(__ROOT__)"#;

/// Longest input value shown in full; longer ones are cut with an explicit marker.
const MAX_VALUE: usize = 80;
/// Changed text lines shown in a click/type diff.
const MAX_DIFF_LINES: usize = 30;

/// An input's value for a snapshot line, never cut silently.
fn show_value(v: &str) -> String {
    let n = v.chars().count();
    if v.is_empty() { String::new() }
    else if n <= MAX_VALUE { format!(" value=\"{v}\"") }
    else { format!(" value=\"{}…({n} chars)\"", v.chars().take(MAX_VALUE).collect::<String>()) }
}

struct El {
    r: i64,
    key: String,  // role + name + href: what the element is, for matching across snapshots
    body: String, // the line without its [ref]
    inside: bool, // within the snapshot's selector region
}

/// One read of the page, as the model sees it.
struct Page {
    title: String,
    url: String,
    status: u64,
    els: Vec<El>,
    text: String,
    problems: Vec<String>,
}

impl Page {
    fn from_json(d: &Value, problems: Vec<String>) -> Page {
        let str_ = |v: &Value| v.as_str().unwrap_or("").to_string();
        let els = d["elements"].as_array().into_iter().flatten().map(|e| {
            let href = e["href"].as_str().unwrap_or("");
            let to = if href.is_empty() || href.starts_with("javascript:") { String::new() } else {
                format!(" → {}", href.chars().take(80).collect::<String>())
            };
            let (role, name) = (str_(&e["role"]), str_(&e["name"]));
            El { r: e["ref"].as_i64().unwrap_or(0), key: format!("{role}\0{name}\0{href}"),
                 body: format!("{role} \"{name}\"{}{}{to}", str_(&e["extra"]), show_value(e["value"].as_str().unwrap_or(""))),
                 inside: e["inside"].as_bool() != Some(false) }
        }).collect();
        let text = d["region"].as_str().or(d["text"].as_str()).unwrap_or("").to_string();
        Page { title: str_(&d["title"]), url: str_(&d["url"]), status: d["status"].as_u64().unwrap_or(0), els, text, problems }
    }

    fn warn(&self) -> String {
        if self.status >= 400 {
            format!("HTTP {}: this page does not exist or failed. Don't guess URLs; go back and click a [ref] \
                     link, or use a URL shown after → in a snapshot.\n", self.status)
        } else {
            String::new()
        }
    }

    fn problems(&self) -> String {
        // said even when clean, so the model knows the check ran
        if self.problems.is_empty() { return "\n\nNo console errors or failed requests.".to_string() }
        let more = self.problems.len().saturating_sub(MAX_PROBLEMS);
        format!("\n\nPage problems since the last action (console errors, failed requests):\n{}{}",
                self.problems.iter().take(MAX_PROBLEMS).map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"),
                if more > 0 { format!("\n- … {more} more") } else { String::new() })
    }

    fn render(&self, page: usize) -> String {
        let shown: Vec<&El> = self.els.iter().filter(|e| e.inside).collect();
        let mut lines: Vec<String> = shown.iter().take(MAX_ELEMENTS).map(|e| format!("[{}] {}", e.r, e.body)).collect();
        if shown.len() > MAX_ELEMENTS {
            lines.push(format!("… {} more elements (scroll to see others)", shown.len() - MAX_ELEMENTS));
        }
        let text: Vec<char> = self.text.chars().collect();
        let pages = text.len().div_ceil(TEXT_PAGE).max(1);
        let page = page.clamp(1, pages);
        let chunk: String = text[((page - 1) * TEXT_PAGE).min(text.len())..(page * TEXT_PAGE).min(text.len())].iter().collect();
        let more = if page < pages { format!("\n(text page {page}/{pages}; snapshot(page={}) for more)", page + 1) } else { String::new() };
        format!("{}Title: {}\nURL: {}\n\nElements:\n{}\n\nText:\n{chunk}{more}{}", self.warn(), self.title, self.url,
                lines.join("\n"), self.problems())
    }
}

/// What changed between two reads of the page: URL/title, elements added (+), removed (-) or
/// changed (~, e.g. a value), and text lines added or removed. Problems are always summarised.
fn diff(before: &Page, after: &Page) -> String {
    let mut out = vec![];
    if before.url != after.url {
        out.push(format!("URL: {} → {}", before.url, after.url));
    }
    if before.title != after.title {
        out.push(format!("Title: \"{}\" → \"{}\"", before.title, after.title));
    }
    // match elements by identity, in order, so repeated ones ("Delete" per row) pair up one to one
    let mut old: std::collections::HashMap<&str, std::collections::VecDeque<&El>> = std::collections::HashMap::new();
    for e in &before.els {
        old.entry(&e.key).or_default().push_back(e);
    }
    let (mut els, mut shifted) = (vec![], false);
    for e in &after.els {
        match old.get_mut(e.key.as_str()).and_then(|q| q.pop_front()) {
            None => els.push(format!("+ [{}] {}", e.r, e.body)),
            Some(o) => {
                shifted |= o.r != e.r;
                if o.body != e.body {
                    els.push(format!("~ [{}] {}  (was: {})", e.r, e.body, o.body));
                }
            }
        }
    }
    let mut gone: Vec<&El> = old.into_values().flatten().collect();
    gone.sort_by_key(|e| e.r);
    els.extend(gone.iter().map(|e| format!("- {}", e.body)));
    if !els.is_empty() {
        let more = els.len().saturating_sub(MAX_ELEMENTS);
        els.truncate(MAX_ELEMENTS);
        if more > 0 {
            els.push(format!("… {more} more element changes; snapshot() for the whole page"));
        }
        out.push(format!("Elements:\n{}", els.join("\n")));
    }
    if shifted {
        out.push("Refs were renumbered: use the refs shown here, or snapshot() before clicking others.".into());
    }
    let (added, removed) = line_diff(&before.text, &after.text);
    let cap = |v: Vec<&str>, sign: &str| {
        let more = v.len().saturating_sub(MAX_DIFF_LINES);
        let mut l: Vec<String> = v.iter().take(MAX_DIFF_LINES).map(|s| format!("{sign} {}", s.chars().take(200).collect::<String>())).collect();
        if more > 0 {
            l.push(format!("{sign} … {more} more lines"));
        }
        l
    };
    if !added.is_empty() || !removed.is_empty() {
        let mut l = cap(added, "+");
        l.extend(cap(removed, "-"));
        out.push(format!("Text:\n{}", l.join("\n")));
    }
    let body = if out.is_empty() { "No visible change on the page.".to_string() } else { out.join("\n\n") };
    format!("{}Changes since the last snapshot (full=true or snapshot() for the whole page):\n{body}{}", after.warn(), after.problems())
}

/// Non-blank lines of `b` not in `a`, and of `a` not in `b`, counting repeats.
fn line_diff<'a>(a: &'a str, b: &'a str) -> (Vec<&'a str>, Vec<&'a str>) {
    let lines = |s: &'a str| s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>();
    let (a, b) = (lines(a), lines(b));
    let minus = |x: &[&'a str], y: &[&'a str]| {
        let mut left: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for l in y {
            *left.entry(l).or_default() += 1;
        }
        x.iter().filter(|l| match left.get_mut(**l) { Some(n) if *n > 0 => { *n -= 1; false } _ => true }).copied().collect::<Vec<_>>()
    };
    (minus(&b, &a), minus(&a, &b))
}

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
    last: Option<Page>, // what the model last saw, for click/type diffs
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
    Ok(Browser { child, ws, refs: vec![], last: None })
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

    /// Read the page; `region` limits the shown elements and text to a CSS selector.
    fn read(&mut self, region: Option<&str>) -> Result<Page, String> {
        self.wait_ready();
        let d = self.eval(&SNAPSHOT_JS.replace("__ROOT__", &region.map_or("null".into(), |r| json!(r).to_string())))?;
        if d["missing"] == true {
            return Err(format!("no element matches selector {}", region.unwrap_or("")));
        }
        self.refs = d["elements"].as_array().cloned().unwrap_or_default();
        let problems = std::mem::take(&mut self.ws.problems);
        Ok(Page::from_json(&d, problems))
    }

    fn snapshot(&mut self, page: usize, region: Option<&str>) -> Result<String, String> {
        let p = self.read(region)?;
        let out = p.render(page);
        // a region view is partial; diffing the next action against it would report the rest as new
        self.last = if region.is_none() { Some(p) } else { None };
        Ok(out)
    }

    /// After click/type: what changed since the last snapshot, or the whole page when asked
    /// for (full=true), when there is nothing to compare with, or when the diff isn't shorter.
    fn changed(&mut self, full: bool) -> Result<String, String> {
        let p = self.read(None)?;
        let whole = p.render(1);
        let out = match (&self.last, full) {
            (Some(before), false) => {
                let d = diff(before, &p);
                if d.len() < whole.len() { d } else { whole }
            }
            _ => whole,
        };
        self.last = Some(p);
        Ok(out)
    }

    fn el(&self, r: i64) -> Result<&Value, String> {
        self.refs.iter().find(|e| e["ref"] == r).ok_or_else(|| format!("no element [{r}] on this page; call snapshot() for fresh refs"))
    }

    /// Scroll element `r` into view and return its centre.
    fn centre(&mut self, r: i64) -> Result<(f64, f64), String> {
        self.el(r)?;
        let v = self.eval(&format!("(() => {{ const e = document.querySelector('[data-qh=\"{r}\"]'); if (!e) return null; \
            e.scrollIntoView({{block: 'center'}}); const b = e.getBoundingClientRect(); return [b.x + b.width / 2, b.y + b.height / 2]; }})()"))?;
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
                b.snapshot(1, None)
            }
            "snapshot" => {
                let region = s("selector");
                b.snapshot(a["page"].as_u64().unwrap_or(1) as usize, Some(region.as_str()).filter(|r| !r.is_empty()))
            }
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
                b.changed(a["full"].as_bool() == Some(true))
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
                b.changed(a["full"].as_bool() == Some(true))
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
                b.snapshot(1, None)
            }
            "scroll" => {
                b.eval(&format!("window.scrollBy(0, {})", if s("direction") == "up" { -600 } else { 600 }))?;
                sleep(300);
                b.snapshot(1, None)
            }
            "back" => {
                b.eval("history.back()")?;
                sleep(500);
                b.snapshot(1, None)
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
        schema("snapshot", "Re-read the whole current page. page=2.. for more text; selector=CSS selector shows only that region's elements and text. Console errors and failed requests since the last action are listed at the end.", &[("page", "integer"), ("selector", "string")], &[]),
        schema("click", "Click element [ref] from the latest snapshot. Returns only what changed on the page (+ added, - removed, ~ changed elements and text, URL/title, console errors); full=true returns the whole page instead.", &[("ref", "integer"), ("full", "boolean")], &["ref"]),
        schema("type", "Fill element [ref] with text; submit=true presses Enter. Returns only what changed on the page; full=true returns the whole page instead.", &[("ref", "integer"), ("text", "string"), ("submit", "boolean"), ("full", "boolean")], &["ref", "text"]),
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

#[cfg(test)]
mod diff_tests {
    use super::{diff, show_value, Page};
    use serde_json::{json, Value};

    fn page(url: &str, title: &str, els: Value, text: &str) -> Page {
        Page::from_json(&json!({"url": url, "title": title, "status": 200, "elements": els, "text": text}), vec![])
    }

    #[test]
    fn long_values_carry_a_marker() {
        let v = "Submitted by the agent while testing the form, end to end, ok?";
        assert_eq!(show_value("hi"), " value=\"hi\"");
        assert_eq!(show_value(""), "");
        let s = show_value(&v.repeat(2));
        assert!(s.ends_with(&format!("…({} chars)\"", v.len() * 2)), "{s}");
        assert_eq!(show_value(v), format!(" value=\"{v}\""), "a {}-char value is shown whole", v.len());
    }

    #[test]
    fn diff_reports_only_changes() {
        let a = page("http://x/", "Form", json!([
            {"ref": 1, "role": "text", "name": "Name", "extra": "", "value": ""},
            {"ref": 2, "role": "button", "name": "Save", "extra": ""},
            {"ref": 3, "role": "link", "name": "Help", "extra": "", "href": "/help"}]), "Welcome\nFill the form");
        let b = page("http://x/", "Form", json!([
            {"ref": 1, "role": "text", "name": "Name", "extra": "", "value": "Ann"},
            {"ref": 2, "role": "button", "name": "Save", "extra": ""},
            {"ref": 3, "role": "link", "name": "Help", "extra": "", "href": "/help"}]), "Welcome\nFill the form");
        let d = diff(&a, &b);
        assert!(d.contains("~ [1] text \"Name\" value=\"Ann\"  (was: text \"Name\")"), "{d}");
        assert!(!d.contains("Save\"") && !d.contains("Help") && !d.contains("URL:") && !d.contains("renumbered"), "{d}");
        assert!(d.contains("No console errors"), "{d}");
        assert!(diff(&a, &a).contains("No visible change"));
    }

    #[test]
    fn diff_shows_navigation_additions_removals_and_shifts() {
        let a = page("http://x/", "Form", json!([
            {"ref": 1, "role": "button", "name": "Save", "extra": ""},
            {"ref": 2, "role": "button", "name": "Delete", "extra": ""},
            {"ref": 3, "role": "button", "name": "Delete", "extra": ""}]), "Rows\nrow a\nrow b");
        let mut b = page("http://x/done", "Saved", json!([
            {"ref": 1, "role": "status", "name": "Saved!", "extra": ""},
            {"ref": 2, "role": "button", "name": "Save", "extra": ""},
            {"ref": 3, "role": "button", "name": "Delete", "extra": ""}]), "Rows\nrow a\nSaved!");
        b.problems = vec!["console error: boom".into()];
        let d = diff(&a, &b);
        assert!(d.contains("URL: http://x/ → http://x/done") && d.contains("Title: \"Form\" → \"Saved\""), "{d}");
        assert!(d.contains("+ [1] status \"Saved!\""), "{d}");
        assert_eq!(d.matches("- button \"Delete\"").count(), 1, "one of two Deletes went: {d}");
        assert!(d.contains("renumbered"), "{d}");
        assert!(d.contains("+ Saved!") && d.contains("- row b") && !d.contains("row a"), "{d}");
        assert!(d.contains("console error: boom"), "{d}");
    }

    #[test]
    fn region_render_keeps_only_inside_elements_and_text() {
        let p = Page::from_json(&json!({"url": "u", "title": "t", "elements": [
            {"ref": 1, "role": "button", "name": "Out", "extra": "", "inside": false},
            {"ref": 2, "role": "button", "name": "In", "extra": "", "inside": true}],
            "text": "whole page", "region": "just the form"}), vec![]);
        let r = p.render(1);
        assert!(r.contains("[2] button \"In\"") && !r.contains("Out") && r.contains("just the form") && !r.contains("whole page"), "{r}");
    }
}
