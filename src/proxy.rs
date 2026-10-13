//! The sandbox's way out: a local HTTP proxy that every sandboxed command's traffic is forced
//! through, so each new host is asked about (allowlist, then the user) before a byte leaves.
//! CONNECT tunnels (HTTPS, git, cargo, npm, pip) and plain `GET http://…` requests; one upstream
//! connection per client connection. Listens on 127.0.0.1 and, on Unix, a private socket for
//! commands in their own network namespace (a helper inside bridges to it).
//!
//! It also serves package mirrors in plain HTTP (`/qh-mirror/…`) for commands that can't do TLS
//! themselves: on Windows, Schannel refuses the sandbox's write-restricted token, and cargo has no
//! other TLS. qh fetches upstream over its own TLS, after the same allowlist check.
use std::io::{self, Read, Write};
use std::net::{Ipv6Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_HEAD: usize = 64 * 1024;
const HEAD_TIME: Duration = Duration::from_secs(30); // a client gets this long to send its request
const CONNECT_TIME: Duration = Duration::from_secs(10);
const BUF: usize = 16 * 1024;

/// `decide(host, port)`: true = forward, false = refuse.
pub type Decide = Arc<dyn Fn(&str, u16) -> bool + Send + Sync>;

/// A running proxy. Dropping it does not stop it; it lives as long as the process (one per session).
pub struct Proxy {
    /// Listening on 127.0.0.1:port.
    pub port: u16,
    /// The same proxy on a Unix socket in a private 0700 dir; None if that couldn't be made.
    #[cfg(unix)]
    pub sock: Option<std::path::PathBuf>,
}

/// Start a proxy. `decide(host, port)` is called for every new connection/request before anything is
/// forwarded, on that connection's thread; it may block for a long time. true = forward, false = refuse.
pub fn start(decide: Decide) -> io::Result<Proxy> {
    // Windows' online sandbox user may reach only these ports (its firewall filters)
    #[cfg(windows)]
    let l = match crate::sandbox_windows::PROXY_PORTS.filter(|_| crate::sandbox_windows::ready()).find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok()) {
        Some(l) => l,
        None => TcpListener::bind("127.0.0.1:0")?,
    };
    #[cfg(not(windows))]
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    serve(move || l.accept().map(|(s, _)| Box::new(s) as Box<dyn Stream>), decide.clone());
    #[cfg(unix)]
    let sock = unix_listener().map(|(path, l)| {
        serve(move || l.accept().map(|(s, _)| Box::new(s) as Box<dyn Stream>), decide);
        path
    });
    Ok(Proxy { port, #[cfg(unix)] sock })
}

/// qh-proxy-<pid>-<random>/p.sock under the temp dir, 0700. Dirs left by dead qh processes are
/// swept first (Linux, where /proc says who is alive).
#[cfg(unix)]
fn unix_listener() -> Option<(std::path::PathBuf, std::os::unix::net::UnixListener)> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let tmp = std::env::temp_dir();
    let tag: String = crate::oauth::random()[..6].iter().map(|b| format!("{b:02x}")).collect();
    let dir = tmp.join(format!("qh-proxy-{}-{tag}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    let uid = std::fs::symlink_metadata(&dir).ok()?.uid();
    if std::path::Path::new("/proc/self").exists() {
        for e in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(pid) = name.strip_prefix("qh-proxy-").and_then(|r| r.split('-').next()) else { continue };
            let mine = e.metadata().is_ok_and(|m| m.is_dir() && m.uid() == uid); // DirEntry::metadata doesn't follow links
            if mine && pid.parse::<u32>().is_ok() && !std::path::Path::new("/proc").join(pid).exists() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    let path = dir.join("p.sock");
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => Some((path, l)),
        Err(_) => { let _ = std::fs::remove_dir(&dir); None }
    }
}

/// What a client connection needs: TCP or a Unix socket.
trait Stream: Read + Write + Send + 'static {
    fn dup(&self) -> io::Result<Box<dyn Stream>>;
    fn shut(&self, how: Shutdown);
    fn timeout(&self, t: Option<Duration>);
}

impl Stream for TcpStream {
    fn dup(&self) -> io::Result<Box<dyn Stream>> { Ok(Box::new(self.try_clone()?)) }
    fn shut(&self, how: Shutdown) { let _ = self.shutdown(how); }
    fn timeout(&self, t: Option<Duration>) { let _ = self.set_read_timeout(t); }
}

#[cfg(unix)]
impl Stream for std::os::unix::net::UnixStream {
    fn dup(&self) -> io::Result<Box<dyn Stream>> { Ok(Box::new(self.try_clone()?)) }
    fn shut(&self, how: Shutdown) { let _ = self.shutdown(how); }
    fn timeout(&self, t: Option<Duration>) { let _ = self.set_read_timeout(t); }
}

fn serve(mut accept: impl FnMut() -> io::Result<Box<dyn Stream>> + Send + 'static, decide: Decide) {
    let _ = std::thread::Builder::new().name("qh-proxy".into()).spawn(move || loop {
        match accept() {
            Ok(c) => {
                let d = decide.clone();
                let _ = std::thread::Builder::new().spawn(move || handle(c, &*d));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)), // out of fds: don't spin
        }
    });
}

fn handle(mut c: Box<dyn Stream>, decide: &(dyn Fn(&str, u16) -> bool + Send + Sync)) {
    c.timeout(Some(HEAD_TIME));
    let Some((buf, end)) = read_head(&mut *c) else { return };
    if let Some(m) = (end <= MAX_HEAD).then(|| mirror_request(&buf[..end])).flatten() {
        return mirror(&mut *c, m, decide);
    }
    let Some((host, port, fwd)) = (end <= MAX_HEAD).then(|| parse(&buf[..end])).flatten() else {
        return reply(&mut *c, "400 Bad Request", "qh sandbox: malformed proxy request");
    };
    if !decide(&host, port) {
        let why = format!("qh sandbox: {host} is not on the network allowlist (the user declined or wasn't asked)");
        return reply(&mut *c, "403 Forbidden", &why);
    }
    let mut u = match connect(&host, port) {
        Ok(u) => u,
        Err(e) => return reply(&mut *c, "502 Bad Gateway", &format!("qh sandbox: can't reach {host}:{port}: {e}")),
    };
    let http = fwd.is_some();
    let sent = match fwd {
        Some(head) => u.write_all(&head),
        None => c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n"),
    };
    if sent.is_err() || u.write_all(&buf[end..]).is_err() { return }
    c.timeout(None);
    let _ = u.set_nodelay(true);
    splice(c, Box::new(u), http);
}

/// Reads until the blank line ending the head. None if the client went away or stalled (after a 400).
fn read_head(c: &mut dyn Stream) -> Option<(Vec<u8>, usize)> {
    let deadline = Instant::now() + HEAD_TIME;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(e) = head_end(&buf) { return Some((buf, e)) }
        if buf.len() > MAX_HEAD || Instant::now() > deadline { break }
        match c.read(&mut chunk) {
            Ok(0) if buf.is_empty() => return None,
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    reply(c, "400 Bad Request", "qh sandbox: incomplete or oversized proxy request");
    // read what the client is still sending before closing: closing on unread data sends a reset,
    // which (macOS especially) throws the 400 away before the client reads it
    c.shut(Shutdown::Write);
    c.timeout(Some(Duration::from_secs(1)));
    let mut left: usize = 1 << 20;
    while left > 0 {
        match c.read(&mut chunk) {
            Ok(n) if n > 0 => left = left.saturating_sub(n),
            _ => break,
        }
    }
    None
}

fn head_end(b: &[u8]) -> Option<usize> {
    (0..b.len()).filter(|&i| b[i] == b'\n').find_map(|i| match &b[i + 1..] {
        [b'\n', ..] => Some(i + 2),
        [b'\r', b'\n', ..] => Some(i + 3),
        _ => None,
    })
}

/// The target and, for plain HTTP, the head to send upstream (origin-form, Connection: close).
fn parse(head: &[u8]) -> Option<(String, u16, Option<Vec<u8>>)> {
    let mut lines = head.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let mut p = std::str::from_utf8(lines.next()?).ok()?.split(' ');
    let (m, target, v) = (p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || m.is_empty() || !v.starts_with("HTTP/1.") { return None }
    if m == "CONNECT" {
        let (h, port) = authority(target, None)?;
        return Some((h, port, None));
    }
    let rest = &target[target.get(..7).filter(|s| s.eq_ignore_ascii_case("http://"))?.len()..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let auth = rest[..end].rsplit('@').next()?; // never userinfo
    let (host, port) = authority(auth, Some(80))?;
    let path = rest[end..].split('#').next()?;
    let slash = if path.starts_with('/') { "" } else { "/" };
    // A proxy must replace Host with the URI's authority: the host decide() approved.
    let mut out = format!("{m} {slash}{path} {v}\r\nHost: {auth}\r\n").into_bytes();
    for l in lines.take_while(|l| !l.is_empty()) {
        let colon = l.iter().position(|&b| b == b':')?;
        if matches!(l[0], b' ' | b'\t') { return None } // obsolete line folding
        let name = String::from_utf8_lossy(&l[..colon]).trim().to_ascii_lowercase();
        if !matches!(name.as_str(), "host" | "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive") {
            out.extend_from_slice(l);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Some((host, port, Some(out)))
}

/// Path prefix → upstream: the crates.io sparse index and its crate downloads.
const MIRRORS: [(&str, &str); 2] = [("/qh-mirror/crates-io/", "https://index.crates.io/"),
                                     ("/qh-mirror/crates-dl/", "https://static.crates.io/crates/")];

/// A request for one of the mirrors, in origin-form (sent straight to the proxy, not through it).
struct MirrorReq { method: String, url: String, host: String, local: String, headers: Vec<(String, String)> }

fn mirror_request(head: &[u8]) -> Option<MirrorReq> {
    let mut lines = head.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let mut p = std::str::from_utf8(lines.next()?).ok()?.split(' ');
    let (m, target, v) = (p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || !matches!(m, "GET" | "HEAD") || !v.starts_with("HTTP/1.") { return None }
    let (prefix, base) = MIRRORS.iter().find(|(pre, _)| target.starts_with(pre))?;
    let rest = &target[prefix.len()..];
    if rest.is_empty() || rest.contains("..") || !rest.bytes().all(|b| b.is_ascii_graphic()) { return None }
    let (mut local, mut headers) = (String::new(), vec![]);
    for l in lines.take_while(|l| !l.is_empty()) {
        let l = std::str::from_utf8(l).ok()?;
        let (k, val) = l.split_once(':')?;
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => local = val.trim().into(),
            k @ ("if-none-match" | "if-modified-since" | "accept" | "user-agent") => headers.push((k.into(), val.trim().into())),
            _ => {}
        }
    }
    // the download links handed out point back here: only ever at loopback
    if !matches!(authority(&local, Some(80)), Some((h, _)) if h == "127.0.0.1" || h == "localhost") { return None }
    let url = format!("{base}{rest}");
    let host = url[8..].split('/').next()?.to_string();
    Some(MirrorReq { method: m.into(), url, host, local, headers })
}

/// Fetch a mirror request upstream (no redirects: they could lead off the allowlist) and pass the
/// response back, closing the connection after it. The index's config.json is rewritten so crate
/// downloads come back through the mirror too.
fn mirror(c: &mut dyn Stream, m: MirrorReq, decide: &(dyn Fn(&str, u16) -> bool + Send + Sync)) {
    if !decide(&m.host, 443) {
        return reply(c, "403 Forbidden", &format!("qh sandbox: {} is not on the network allowlist (the user declined or wasn't asked)", m.host));
    }
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    let agent = AGENT.get_or_init(|| ureq::AgentBuilder::new().redirects(0).timeout_connect(CONNECT_TIME)
        .timeout_read(Duration::from_secs(60)).build());
    let mut req = agent.request(&m.method, &m.url);
    for (k, v) in &m.headers { req = req.set(k, v) }
    let r = match req.call() {
        Ok(r) | Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return reply(c, "502 Bad Gateway", &format!("qh sandbox: can't fetch {}: {e}", m.url)),
    };
    let mut head = format!("HTTP/1.1 {} {}\r\n", r.status(), r.status_text());
    for k in ["content-type", "etag", "last-modified", "cache-control", "location"] {
        if let Some(v) = r.header(k) { head += &format!("{k}: {v}\r\n") }
    }
    let config = m.method == "GET" && m.url == "https://index.crates.io/config.json" && r.status() == 200;
    if config {
        let body = r.into_string().map(|b| config_json(&b, &m.local)).unwrap_or_default();
        let _ = write!(c, "{head}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    } else {
        // no content-length: the body may have been decompressed on the way; the close ends it
        if write!(c, "{head}connection: close\r\n\r\n").is_ok() && m.method == "GET" {
            let _ = io::copy(&mut r.into_reader(), c);
        }
    }
    let _ = c.flush();
    c.shut(Shutdown::Both);
}

/// The crates.io index config with `dl` pointed at the mirror on `local` (host:port).
fn config_json(body: &str, local: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| serde_json::json!({}));
    v["dl"] = format!("http://{local}/qh-mirror/crates-dl").into();
    v.to_string()
}

/// host[:port] or [v6][:port] → (lowercase host without brackets or trailing dot, port).
fn authority(s: &str, default: Option<u16>) -> Option<(String, u16)> {
    let (host, port, v6) = match s.strip_prefix('[') {
        Some(r) => { let (h, p) = r.split_once(']')?; (h, p, true) }
        None => match s.find(':') { Some(i) => (&s[..i], &s[i..], false), None => (s, "", false) },
    };
    let port = match port {
        "" => default?,
        p => p.strip_prefix(':')?.parse().ok().filter(|&p| p != 0)?,
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let ok = if v6 { host.parse::<Ipv6Addr>().is_ok() } else {
        !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
    };
    ok.then_some((host, port))
}

fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address for the name");
    for a in (host, port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&a, CONNECT_TIME) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn reply(c: &mut dyn Stream, status: &str, body: &str) {
    let _ = write!(c, "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n", body.len() + 1);
    let _ = c.flush();
}

/// Copies both ways until both sides are done. A clean EOF is passed on as a half-close; an error
/// on either side tears both down, which also wakes the other direction. For plain HTTP the end of
/// the response ends the connection (one request per connection), which also stops the other pump.
fn splice(mut c: Box<dyn Stream>, mut u: Box<dyn Stream>, http: bool) {
    let (Ok(mut c2), Ok(mut u2)) = (c.dup(), u.dup()) else { return };
    let up = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || pump(&mut *c2, &mut *u2));
    if up.is_err() { return }
    pump(&mut *u, &mut *c);
    if http { c.shut(Shutdown::Both) }
}

/// from → to until EOF (then `to` gets a write shutdown) or an error (then both are torn down).
fn pump(from: &mut dyn Stream, to: &mut dyn Stream) {
    let mut buf = vec![0u8; BUF];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return to.shut(Shutdown::Write),
            Ok(n) => if to.write_all(&buf[..n]).is_err() { break },
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    from.shut(Shutdown::Both);
    to.shut(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Mutex;

    type Seen = Arc<Mutex<Vec<(String, u16)>>>;

    /// A proxy that allows only 127.0.0.1 and records every decide() call.
    fn proxy() -> (Proxy, Seen) {
        let seen: Seen = Arc::default();
        let s = seen.clone();
        let p = start(Arc::new(move |h: &str, port| { s.lock().unwrap().push((h.into(), port)); h == "127.0.0.1" })).unwrap();
        (p, seen)
    }

    fn echo_server() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || for c in l.incoming().flatten() {
            std::thread::spawn(move || { let mut r = c.try_clone().unwrap(); let mut w = c; let _ = io::copy(&mut r, &mut w); });
        });
        port
    }

    /// Serves one fake HTTP response and hands back the request head it got.
    fn http_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let (buf, end) = read_head(&mut c).unwrap();
            tx.send(String::from_utf8_lossy(&buf[..end]).into_owned()).unwrap();
            c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
        });
        (port, rx)
    }

    fn head_of(c: &mut dyn Read) -> String {
        let mut h = Vec::new();
        let mut b = [0u8; 1];
        while !h.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap() == 1 { h.push(b[0]) }
        String::from_utf8(h).unwrap()
    }

    fn tunnel_echo<S: Read + Write>(mut c: S, up: u16) -> S {
        write!(c, "CONNECT 127.0.0.1:{up} HTTP/1.1\r\nHost: 127.0.0.1:{up}\r\n\r\n").unwrap();
        assert!(head_of(&mut c).starts_with("HTTP/1.1 200 "));
        for msg in [&b"hello"[..], &[7u8; 40_000][..]] {
            c.write_all(msg).unwrap();
            let mut got = vec![0u8; msg.len()];
            c.read_exact(&mut got).unwrap();
            assert_eq!(got, msg);
        }
        c
    }

    fn ask(p: &Proxy, req: &[u8]) -> String {
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).unwrap();
        c.write_all(req).unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    }

    #[test]
    fn connect_tunnels_both_ways_and_half_closes() {
        let (p, seen) = proxy();
        let up = echo_server();
        let c = tunnel_echo(TcpStream::connect(("127.0.0.1", p.port)).unwrap(), up);
        c.shutdown(Shutdown::Write).unwrap(); // the echo server sees EOF, closes, and we see it too
        let mut rest = Vec::new();
        (&c).read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        assert_eq!(*seen.lock().unwrap(), vec![("127.0.0.1".to_string(), up)]);
    }

    #[test]
    fn refused_hosts_get_403() {
        let (p, seen) = proxy();
        let r = ask(&p, b"CONNECT LocalHost.:8443 HTTP/1.1\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 403 ") && r.contains("localhost is not on the network allowlist"), "{r}");
        let r = ask(&p, b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 403 "), "{r}");
        let r = ask(&p, b"GET http://user:pw@Example.COM/x HTTP/1.1\r\nHost: example.com\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 403 "), "{r}");
        assert_eq!(*seen.lock().unwrap(), vec![("localhost".into(), 8443), ("::1".into(), 443), ("example.com".into(), 80)]);
    }

    #[test]
    fn plain_http_is_rewritten_and_forwarded() {
        let (p, seen) = proxy();
        let (up, rx) = http_server();
        let r = ask(&p, format!("GET http://127.0.0.1:{up}/a/b?c=1 HTTP/1.1\r\nHost: evil.example\r\nUser-Agent: t\r\n\
            Proxy-Authorization: Basic eDp5\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n").as_bytes());
        assert!(r.starts_with("HTTP/1.1 200 OK") && r.ends_with("\r\n\r\nok"), "{r}");
        let h = rx.recv().unwrap();
        assert_eq!(h, format!("GET /a/b?c=1 HTTP/1.1\r\nHost: 127.0.0.1:{up}\r\nUser-Agent: t\r\nAccept: */*\r\nConnection: close\r\n\r\n"));
        assert_eq!(*seen.lock().unwrap(), vec![("127.0.0.1".to_string(), up)]);
    }

    #[test]
    fn malformed_requests_get_400() {
        let (p, seen) = proxy();
        for req in [&b"garbage\r\n\r\n"[..], b"GET /local HTTP/1.1\r\n\r\n", b"CONNECT example.com HTTP/1.1\r\n\r\n",
            b"CONNECT ::1:443 HTTP/1.1\r\n\r\n", b"CONNECT a b:443 HTTP/1.1\r\n\r\n", b"GET https://x/ HTTP/1.1\r\n\r\n",
            b"GET http://x/ HTTP/1.1\r\nno colon\r\n\r\n", b"CONNECT x:0 HTTP/1.1\r\n\r\n", b"CONNECT x:1"] {
            let mut c = TcpStream::connect(("127.0.0.1", p.port)).unwrap();
            c.write_all(req).unwrap();
            c.shutdown(Shutdown::Write).unwrap();
            let mut out = String::new();
            c.read_to_string(&mut out).unwrap();
            assert!(out.starts_with("HTTP/1.1 400 "), "{:?}: {out}", String::from_utf8_lossy(req));
        }
        let r = ask(&p, &vec![b'a'; MAX_HEAD + 10_000]);
        assert!(r.starts_with("HTTP/1.1 400 "));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_tunnels_too() {
        use std::os::unix::fs::PermissionsExt;
        let (p, _) = proxy();
        let sock = p.sock.clone().unwrap();
        let dir = sock.parent().unwrap();
        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        drop(tunnel_echo(std::os::unix::net::UnixStream::connect(&sock).unwrap(), echo_server()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn curl_through_the_proxy() {
        let (p, _) = proxy();
        let (up, rx) = http_server();
        let Ok(out) = std::process::Command::new("curl").args(["-s", "-x", &format!("http://127.0.0.1:{}", p.port)])
            .arg(format!("http://127.0.0.1:{up}/x")).output() else { return };
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
        assert!(rx.recv().unwrap().starts_with("GET /x HTTP/1.1\r\nHost: 127.0.0.1:"));
    }

    #[test]
    fn mirror_requests_are_only_index_and_downloads_for_loopback() {
        let m = mirror_request(b"GET /qh-mirror/crates-io/se/rd/serde HTTP/1.1\r\nHost: 127.0.0.1:47610\r\nIf-None-Match: \"x\"\r\nCookie: a\r\n\r\n").unwrap();
        assert_eq!((m.url.as_str(), m.host.as_str(), m.local.as_str()), ("https://index.crates.io/se/rd/serde", "index.crates.io", "127.0.0.1:47610"));
        assert_eq!(m.headers, [("if-none-match".to_string(), "\"x\"".to_string())]);
        let m = mirror_request(b"HEAD /qh-mirror/crates-dl/serde/1.0.0/download HTTP/1.1\r\nHost: localhost:9\r\n\r\n").unwrap();
        assert_eq!((m.method.as_str(), m.url.as_str()), ("HEAD", "https://static.crates.io/crates/serde/1.0.0/download"));
        for bad in [&b"GET /qh-mirror/crates-io/../x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n"[..],
                    b"POST /qh-mirror/crates-io/x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
                    b"GET /qh-mirror/crates-io/x HTTP/1.1\r\nHost: evil.example:80\r\n\r\n",
                    b"GET /qh-mirror/crates-io/x HTTP/1.1\r\n\r\n",
                    b"GET /qh-mirror/other/x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
                    b"GET /qh-mirror/crates-io/ HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n"] {
            assert!(mirror_request(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
        let c = config_json(r#"{"dl":"https://static.crates.io/crates","api":"https://crates.io"}"#, "127.0.0.1:47610");
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!((v["dl"].as_str(), v["api"].as_str()), (Some("http://127.0.0.1:47610/qh-mirror/crates-dl"), Some("https://crates.io")));
    }

    #[test]
    fn mirror_asks_the_allowlist_first() {
        let (p, seen) = proxy();
        let r = ask(&p, format!("GET /qh-mirror/crates-io/config.json HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", p.port).as_bytes());
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        assert_eq!(*seen.lock().unwrap(), [("index.crates.io".to_string(), 443)]);
    }

    /// Real cargo, real crates.io, through the mirror only (QH_REALWORK=1: needs internet).
    #[test]
    fn cargo_builds_through_the_mirror() {
        if std::env::var("QH_REALWORK").is_err() || Command::new("cargo").arg("--version").output().is_err() { return }
        let p = start(Arc::new(|h: &str, _| h.ends_with("crates.io"))).unwrap();
        let d = std::env::temp_dir().join(format!("qh-mirror-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let home = d.join("cargo-home");
        std::fs::create_dir_all(&home).unwrap();
        // as the Windows sandbox sets it up: a fixed config, the mirror's address per command in the env
        std::fs::write(home.join("config.toml"), "[source.crates-io]\nreplace-with = \"qh-mirror\"\n").unwrap();
        let ok = |c: &mut Command| { let o = c.output().unwrap(); assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr)); };
        ok(Command::new("cargo").args(["new", "-q", "--vcs", "none", "m"]).current_dir(&d));
        let proj = d.join("m");
        let mut t = std::fs::read_to_string(proj.join("Cargo.toml")).unwrap();
        t.push_str("itoa = \"1\"\n");
        std::fs::write(proj.join("Cargo.toml"), t).unwrap();
        // no proxy variables and no other way out: only the mirror can have served this
        ok(Command::new("cargo").args(["build", "-q"]).current_dir(&proj).env("CARGO_HOME", &home)
            .env("CARGO_REGISTRIES_QH_MIRROR_INDEX", format!("sparse+http://127.0.0.1:{}/qh-mirror/crates-io/", p.port))
            .env("CARGO_TARGET_DIR", d.join("t")).env_remove("HTTPS_PROXY").env_remove("https_proxy"));
        // cargo names its cache after the registry it used: the mirror on loopback
        let cache: Vec<String> = std::fs::read_dir(home.join("registry/cache")).unwrap().flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(!cache.is_empty() && cache.iter().all(|n| n.starts_with("127.0.0.1-")), "{cache:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
