//! `qh relay`: a small relay server for qh-link, for teammates who can't reach each other directly
//! (two strict routers, phone networks) and have no teammate who reaches both.
//!
//! It never learns who anyone is: two teammates send packets marked with a token only the pair can
//! compute (new every day), and the server passes each packet to the other address that used the
//! same token. Packets are sealed end to end; the server sees IP addresses and sizes, nothing else.
//! A private relay (the default) only serves those who know its secret; `--public` serves anyone.
//!
//! An address is only sent packets after it proved, at that address, that it is one of the pair
//! (see link::CHALLENGE): so nobody can make the relay send traffic to a victim's address, and
//! nobody who merely saw a pair's token can take one of its two places.

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RELAYED: u8 = 0x53;
use crate::link::{CHALLENGE, PROOF};
const CHALLENGES_PER_IP: u8 = 4; // per second: all a spoofed sender can make us send its victim
const CHALLENGES: u32 = 10_000;   // per second, all addresses together (33 bytes each)
// Challenges given out this second, counted in buckets by a keyed hash of the source IP: a fixed
// table, so spoofed sources can't fill it up and starve everyone else of challenges.
const ASKED_BUCKETS: usize = 1 << 16;
const IDLE: Duration = Duration::from_secs(120); // an address that went quiet is forgotten
const BYTES_PER_MIN: usize = 2 * 1024 * 1024; // per token: same budget as a teammate relay
const IP_BYTES_PER_MIN: usize = 16 * 1024 * 1024; // per source IP, all its tokens together
const TOKENS_PER_IP: usize = 32;
const MAX_TOKENS: usize = 200_000;

struct Slot {
    ends: [Option<(End, Instant)>; 2], // where each of the pair proved to be, and when last heard
    window: Instant,
    bytes: usize,
}

/// The cookie for `token` at `from` in 30-second window `w`: only someone at `from` sees it.
fn cookie(key: &ring::hmac::Key, token: &[u8], from: SocketAddr, w: u64) -> [u8; 16] {
    let msg = [token, from.to_string().as_bytes(), &w.to_be_bytes()].concat();
    ring::hmac::sign(key, &msg).as_ref()[..16].try_into().unwrap()
}

fn window() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 30).unwrap_or(0)
}

/// A proof is [PROOF][side][cookie][pair key][signature]: the cookie is this address's (now or
/// the window before), the pair key hashes to the token, and it signed the cookie.
fn proven(key: &ring::hmac::Key, token: &[u8; 16], from: SocketAddr, body: &[u8]) -> Option<usize> {
    if body.len() != 1 + 1 + 16 + 32 + 64 || body[1] > 1 {
        return None;
    }
    let (side, c, pk, sig) = (body[1], &body[2..18], &body[18..50], &body[50..]);
    let w = window();
    if !ct_eq(c, &cookie(key, token, from, w)) && !ct_eq(c, &cookie(key, token, from, w.saturating_sub(1))) {
        return None;
    }
    if crate::link::relay_token(pk) != *token {
        return None;
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk)
        .verify(&crate::link::proof_bytes(token, c, side), sig).ok()?;
    Some(side as usize)
}

/// Equal, in time that doesn't depend on where the first difference is.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y))) == 0
}

/// A line on stderr, at most one a second whatever happens: under attack the log must not become
/// the next thing to run out.
fn warn(msg: impl std::fmt::Display) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if LAST.swap(now, Ordering::Relaxed) != now {
        eprintln!("qh relay: {msg}");
    }
}

/// Lets the relay hold as many sockets as the system allows it (the soft limit, often 1024, up to
/// the hard one), and returns the limit it ends up with.
#[cfg(unix)]
fn raise_nofile() -> Option<u64> {
    // SAFETY: getrlimit/setrlimit only read and write the struct we pass them.
    unsafe {
        let mut l: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) != 0 {
            return None;
        }
        let want = l.rlim_max.min(1 << 20); // an "unlimited" hard limit still has a kernel ceiling
        if l.rlim_cur < want {
            let n = libc::rlimit { rlim_cur: want, rlim_max: l.rlim_max };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &n) == 0 {
                l.rlim_cur = want;
            }
        }
        Some(l.rlim_cur as u64)
    }
}

/// The open-file limit as it stands (None where there isn't one to read).
#[cfg(unix)]
fn nofile() -> Option<u64> {
    // SAFETY: as above
    unsafe {
        let mut l: libc::rlimit = std::mem::zeroed();
        (libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) == 0).then_some(l.rlim_cur as u64)
    }
}

#[cfg(not(unix))]
fn nofile() -> Option<u64> {
    None
}

pub fn cli(args: &[String]) -> i32 {
    #[cfg(unix)]
    if let Some(n) = raise_nofile() {
        if n < 4096 {
            println!("note: only {n} open files allowed, so at most {} TCP connections (raise `ulimit -n` for more)", tcp_cap());
        }
    }
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let port: u16 = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(crate::link::RELAY_PORT);
    let public = args.iter().any(|a| a == "--public");
    let secret: Option<[u8; 32]> = if public {
        None
    } else if let Some(k) = arg("--secret") {
        match <[u8; 32]>::try_from(crate::oauth::b64url_decode(&k)) {
            Ok(k) => Some(k),
            Err(_) => {
                eprintln!("--secret must be the 43-character key `qh relay` printed");
                return 2;
            }
        }
    } else {
        // kept, so the line teammates pasted keeps working after a restart
        let file = crate::config::config_dir().join("relay-secret");
        let k = std::fs::read_to_string(&file).ok()
            .and_then(|t| <[u8; 32]>::try_from(crate::oauth::b64url_decode(t.trim())).ok())
            .unwrap_or_else(|| {
                let k = crate::oauth::random();
                let _ = std::fs::create_dir_all(crate::config::config_dir());
                let _ = crate::platform::write_private(&file, crate::oauth::b64url(&k).as_bytes());
                k
            });
        Some(k)
    };
    let sock = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("can't open UDP port {port}: {e}");
            return 1;
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
    let key = secret.map(|k| format!("#{}", crate::oauth::b64url(&k))).unwrap_or_default();
    println!("qh relay on UDP port {port} ({})", if public { "public: anyone may use it" } else { "private" });
    println!("each teammate puts this in ~/.config/qh/config.toml (use this server's public IP or name):");
    println!("  relay = \"<server-ip>:{port}{key}\"");
    println!("open UDP {port} in the server's firewall; packets are sealed, the relay can't read them");
    let tcp_port: u16 = arg("--tcp-port").and_then(|p| p.parse().ok()).unwrap_or(crate::link::RELAY_TCP_PORT);
    let tcp = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, tcp_port)) {
        Ok(l) => {
            println!("and on TCP port {tcp_port}, for networks that block UDP (open it too)");
            Some(l)
        }
        Err(e) => {
            println!("no TCP fallback: can't open TCP port {tcp_port}: {e} (port 443 needs root or CAP_NET_BIND_SERVICE; --tcp-port N)");
            None
        }
    };
    serve_with(&sock, tcp, secret, None);
    0
}

/// Where one of a pair is: a UDP address, or a TCP connection (for networks that block UDP).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum End {
    Udp(SocketAddr),
    Tcp(SocketAddr),
}

impl End {
    fn addr(self) -> SocketAddr {
        match self { End::Udp(a) | End::Tcp(a) => a }
    }
}

const TCP_CONNS: usize = 1024; // TCP connections at once, at most (each: one socket, two threads)
const TCP_PER_IP: usize = 16;   // per IP, counting the ones still in their handshake
const HANDSHAKE: Duration = Duration::from_secs(10); // first byte to TLS done, wall clock
const UNPROVEN: Duration = Duration::from_secs(60);  // a TCP end that hasn't proved itself by then is closed
const OUT_QUEUE: usize = 64; // packets waiting for one slow TCP end; more are dropped

/// How many TCP connections the open-file limit leaves room for (half of it, the rest kept for
/// everything else), within TCP_CONNS.
fn tcp_cap() -> usize {
    nofile().map_or(TCP_CONNS, |n| (n.saturating_sub(64) / 2).clamp(1, TCP_CONNS as u64) as usize)
}

/// An open TCP end: its packets go into `out`, and a writer thread of its own sends them, so a
/// teammate who stops reading holds up nobody but themselves.
struct TcpEnd {
    out: SyncSender<Vec<u8>>,
    wire: Arc<crate::tls::Wire>,
    since: Instant,
}

/// What the relay keeps. The UDP loop and every TCP connection feed it packets.
struct Relay {
    secret: Option<[u8; 32]>,
    key: ring::hmac::Key,
    table: HashMap<[u8; 16], Slot>,
    per_ip: HashMap<IpAddr, usize>,
    ip_bytes: HashMap<IpAddr, (Instant, usize)>, // per proven source IP: this minute's start and bytes
    asked: Vec<u8>,
    asked_hash: std::collections::hash_map::RandomState,
    asked_at: Instant,
    asked_all: u32,
    swept: Instant,
    passed: u64,
    dropped: u64,
    tcp: HashMap<SocketAddr, TcpEnd>, // open TCP ends, to write their packets to
}

impl Relay {
    fn new(secret: Option<[u8; 32]>, now: Instant) -> Relay {
        Relay {
            secret, key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &crate::oauth::random()),
            table: HashMap::new(), per_ip: HashMap::new(), ip_bytes: HashMap::new(),
            asked: vec![0; ASKED_BUCKETS], asked_hash: Default::default(), asked_at: now, asked_all: 0,
            swept: now, passed: 0, dropped: 0, tcp: HashMap::new(),
        }
    }

    fn sweep(&mut self, now: Instant) {
        if now - self.swept <= Duration::from_secs(30) {
            return;
        }
        self.swept = now;
        let tcp = &self.tcp;
        self.table.retain(|_, s| {
            for e in s.ends.iter_mut() {
                // a TCP end lasts as long as its connection
                if e.is_some_and(|(a, t)| now - t >= IDLE || matches!(a, End::Tcp(x) if !tcp.contains_key(&x))) {
                    *e = None;
                }
            }
            s.ends.iter().any(|e| e.is_some())
        });
        self.per_ip.clear();
        let mut proven = HashSet::new();
        for s in self.table.values() {
            for (e, _) in s.ends.iter().flatten() {
                *self.per_ip.entry(e.addr().ip()).or_default() += 1;
                if let End::Tcp(a) = e {
                    proven.insert(*a);
                }
            }
        }
        // a connection that never became one of a pair is only holding a place
        self.tcp.retain(|a, t| {
            let keep = proven.contains(a) || now - t.since < UNPROVEN;
            if !keep {
                t.wire.shutdown();
            }
            keep
        });
        self.ip_bytes.retain(|_, (w, _)| now - *w <= Duration::from_secs(60));
    }

    /// One packet from `from`; what to send where goes into `out`.
    fn packet(&mut self, buf: &[u8], from: End, out: &mut Vec<(End, Vec<u8>)>) {
        let now = Instant::now();
        let n = buf.len();
        if n < 26 || buf[0] != RELAYED {
            return;
        }
        let Ok(token) = <[u8; 16]>::try_from(&buf[1..17]) else { return };
        if let Some(k) = &self.secret {
            if !ct_eq(&crate::link::relay_mac(k, &token), &buf[17..25]) {
                self.dropped += 1;
                return;
            }
        }
        let body = &buf[25..n];
        if body[0] == PROOF {
            let Some(side) = proven(&self.key, &token, from.addr(), body) else {
                self.dropped += 1;
                return;
            };
            if !self.table.contains_key(&token) {
                let used = self.per_ip.entry(from.addr().ip()).or_default();
                if *used >= TOKENS_PER_IP || self.table.len() >= MAX_TOKENS {
                    self.dropped += 1;
                    return;
                }
                *used += 1;
            }
            // that side of the pair is here now (it moved, or it's new)
            self.table.entry(token).or_insert(Slot { ends: [None, None], window: now, bytes: 0 }).ends[side] = Some((from, now));
            return;
        }
        let Some((slot, side)) = self.table.get_mut(&token).and_then(|s| {
            let side = s.ends.iter().position(|e| e.is_some_and(|(a, _)| a == from))?;
            Some((s, side))
        }) else {
            // an address that hasn't proved itself: ask it to (within the challenge budget)
            if now - self.asked_at >= Duration::from_secs(1) {
                (self.asked_at, self.asked_all) = (now, 0);
                self.asked.fill(0);
            }
            let b = self.asked_hash.hash_one(from.addr().ip()) as usize % ASKED_BUCKETS;
            self.asked[b] = self.asked[b].saturating_add(1);
            self.asked_all += 1;
            if self.asked[b] <= CHALLENGES_PER_IP && self.asked_all <= CHALLENGES {
                let mut c = vec![CHALLENGE];
                c.extend(token);
                c.extend(cookie(&self.key, &token, from.addr(), window()));
                out.push((from, c));
            }
            self.dropped += 1;
            return;
        };
        slot.ends[side] = Some((from, now));
        if now - slot.window > Duration::from_secs(60) {
            slot.window = now;
            slot.bytes = 0;
        }
        slot.bytes += n;
        let ip = self.ip_bytes.entry(from.addr().ip()).or_insert((now, 0));
        if now - ip.0 > Duration::from_secs(60) {
            *ip = (now, 0);
        }
        ip.1 += n;
        if slot.bytes > BYTES_PER_MIN || ip.1 > IP_BYTES_PER_MIN {
            self.dropped += 1;
            return;
        }
        if let Some((to, seen)) = slot.ends[1 - side] {
            if now - seen < IDLE {
                out.push((to, buf.to_vec()));
                self.passed += 1;
            }
        }
    }

    /// Hands TCP packets to their writers (never waiting) and returns the UDP ones, to be sent
    /// once the lock is let go.
    fn route(&mut self, out: Vec<(End, Vec<u8>)>) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut udp = vec![];
        for (to, b) in out {
            match to {
                End::Udp(a) => udp.push((a, b)),
                End::Tcp(a) => {
                    let Some(t) = self.tcp.get(&a) else { continue };
                    match t.out.try_send(b) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => self.dropped += 1,
                        Err(TrySendError::Disconnected(_)) => {
                            self.tcp.remove(&a);
                        }
                    }
                }
            }
        }
        udp
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn send_udp(sock: &UdpSocket, udp: Vec<(SocketAddr, Vec<u8>)>) {
    for (a, b) in udp {
        let _ = sock.send_to(&b, a);
    }
}

/// The relay loop (`until` stops it, for tests), also taking teammates over TCP on `tcp` (same packets, each with a 2-byte length).
pub fn serve_with(sock: &UdpSocket, tcp: Option<TcpListener>, secret: Option<[u8; 32]>, until: Option<Instant>) {
    let relay = Arc::new(Mutex::new(Relay::new(secret, Instant::now())));
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(l) = tcp {
        let (relay, stop) = (relay.clone(), stop.clone());
        let started = sock.try_clone().and_then(|sock| {
            l.set_nonblocking(true)?;
            std::thread::Builder::new().name("relay-accept".into()).spawn(move || accept(l, relay, sock, stop))
        });
        if let Err(e) = started {
            eprintln!("qh relay: no TCP fallback: {e}");
        }
    }
    let mut stats = Instant::now();
    let mut buf = [0u8; 2048];
    while until.is_none_or(|u| Instant::now() < u) {
        let now = Instant::now();
        if until.is_none() && now - stats > Duration::from_secs(300) {
            stats = now;
            let mut r = lock(&relay);
            println!("{} pairs meeting ({} over tcp), {} packets passed, {} dropped (last 5 min)", r.table.len(), r.tcp.len(), r.passed, r.dropped);
            (r.passed, r.dropped) = (0, 0);
        }
        let got = sock.recv_from(&mut buf);
        let udp = {
            let mut r = lock(&relay);
            r.sweep(now);
            let Ok((n, from)) = got else { continue };
            let mut out = vec![];
            r.packet(&buf[..n], End::Udp(from), &mut out);
            r.route(out)
        };
        send_udp(sock, udp);
    }
    stop.store(true, Ordering::Relaxed);
}

/// TCP connections open now, all and per IP (the ones still in their handshake included).
#[derive(Default)]
struct Conns {
    all: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// One connection's place in `Conns`, given back when it's dropped, however the connection ends.
struct Held {
    ip: IpAddr,
    conns: Arc<Mutex<Conns>>,
}

impl Held {
    fn take(conns: &Arc<Mutex<Conns>>, ip: IpAddr, cap: usize) -> Option<Held> {
        let mut c = lock(conns);
        let n = c.per_ip.get(&ip).copied().unwrap_or(0);
        if c.all >= cap || n >= TCP_PER_IP {
            return None;
        }
        c.all += 1;
        c.per_ip.insert(ip, n + 1);
        Some(Held { ip, conns: conns.clone() })
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let mut c = lock(&self.conns);
        c.all = c.all.saturating_sub(1);
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

fn accept(l: TcpListener, relay: Arc<Mutex<Relay>>, sock: UdpSocket, stop: Arc<AtomicBool>) {
    let sock = Arc::new(sock);
    let conns = Arc::new(Mutex::new(Conns::default()));
    let cap = tcp_cap();
    while !stop.load(Ordering::Relaxed) {
        let (s, from) = match l.accept() {
            Ok(x) => x,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    warn(format_args!("can't take a TCP connection: {e}")); // out of files, most likely
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        // over the cap: `s` is dropped, and with it the connection
        let Some(held) = Held::take(&conns, from.ip(), cap) else { continue };
        let (relay, stop, sock) = (relay.clone(), stop.clone(), sock.clone());
        // if no thread can be made, the closure (with the socket and its place) is dropped
        if let Err(e) = std::thread::Builder::new().name("relay-tcp".into()).spawn(move || conn(s, from, held, relay, sock, stop)) {
            warn(format_args!("can't start a TCP connection's thread: {e}"));
        }
    }
}

/// One TCP connection, from the handshake to its end. `_held` keeps its place in the count.
fn conn(s: TcpStream, from: SocketAddr, _held: Held, relay: Arc<Mutex<Relay>>, sock: Arc<UdpSocket>, stop: Arc<AtomicBool>) {
    let _ = s.set_nodelay(true);
    // a write waits at most 500 ms (in the writer thread); a read, IDLE (the handshake, less)
    if s.set_nonblocking(false).is_err() || s.set_write_timeout(Some(Duration::from_millis(500))).is_err()
        || s.set_read_timeout(Some(IDLE)).is_err() {
        return;
    }
    let Ok((w, mut rd)) = crate::tls::accept(s, Instant::now() + HANDSHAKE) else { return };
    let (tx, rx) = sync_channel::<Vec<u8>>(OUT_QUEUE);
    let writer = w.clone();
    let started = std::thread::Builder::new().name("relay-tcp-out".into()).stack_size(256 * 1024).spawn(move || {
        for pkt in rx {
            if writer.send(&pkt).is_err() {
                break; // a stalled reader isn't waited for: it loses its connection
            }
        }
        writer.shutdown();
    });
    if let Err(e) = started {
        warn(format_args!("can't start a TCP connection's writer: {e}"));
        w.shutdown();
        return;
    }
    lock(&relay).tcp.insert(from, TcpEnd { out: tx, wire: w.clone(), since: Instant::now() });
    while !stop.load(Ordering::Relaxed) {
        let Ok(pkt) = rd.recv() else { break };
        let udp = {
            let mut r = lock(&relay);
            let mut out = vec![];
            r.packet(&pkt, End::Tcp(from), &mut out);
            r.route(out)
        };
        send_udp(&sock, udp);
    }
    w.shutdown();
    let mut r = lock(&relay);
    if r.tcp.get(&from).is_some_and(|t| Arc::ptr_eq(&t.wire, &w)) {
        r.tcp.remove(&from); // and with it the writer's queue, so the writer ends too
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use std::io::{Read, Write};

    fn udp(ip: [u8; 4], port: u16) -> End {
        End::Udp(SocketAddr::from((ip, port)))
    }

    /// A pair's key and the token it hashes to.
    fn pair() -> (Ed25519KeyPair, [u8; 16]) {
        let kp = Ed25519KeyPair::from_seed_unchecked(&crate::oauth::random()).unwrap();
        let t = crate::link::relay_token(kp.public_key().as_ref());
        (kp, t)
    }

    fn proof(r: &Relay, kp: &Ed25519KeyPair, t: &[u8; 16], at: End, side: u8) -> Vec<u8> {
        let c = cookie(&r.key, t, at.addr(), window());
        let mut p = vec![RELAYED];
        p.extend(t);
        p.extend([0u8; 8]);
        p.extend([PROOF, side]);
        p.extend(c);
        p.extend(kp.public_key().as_ref());
        p.extend(kp.sign(&crate::link::proof_bytes(t, &c, side)).as_ref());
        p
    }

    fn data(t: &[u8; 16], len: usize) -> Vec<u8> {
        let mut p = vec![RELAYED];
        p.extend(t);
        p.extend([0u8; 8]);
        p.resize(len, 0x02);
        p
    }

    /// Both sides of a fresh pair proved at `a` and `b`; returns its token.
    fn meet(r: &mut Relay, a: End, b: End) -> [u8; 16] {
        let (kp, t) = pair();
        let mut out = vec![];
        r.packet(&proof(r, &kp, &t, a, 0), a, &mut out);
        r.packet(&proof(r, &kp, &t, b, 1), b, &mut out);
        assert!(out.is_empty());
        t
    }

    #[test]
    fn constant_time_compare_still_compares() {
        assert!(ct_eq(b"abcdefgh", b"abcdefgh"));
        assert!(!ct_eq(b"abcdefgh", b"abcdefgX"));
        assert!(!ct_eq(b"Xbcdefgh", b"abcdefgh"));
        assert!(!ct_eq(b"abc", b"abcd"));
    }

    #[test]
    fn a_proven_pair_is_relayed_and_a_stranger_is_only_challenged() {
        let mut r = Relay::new(None, Instant::now());
        let (a, b) = (udp([10, 0, 0, 1], 1000), udp([10, 0, 0, 2], 2000));
        let t = meet(&mut r, a, b);
        let mut out = vec![];
        r.packet(&data(&t, 100), a, &mut out);
        assert_eq!(out, vec![(b, data(&t, 100))]);
        out.clear();
        r.packet(&data(&t, 100), udp([10, 0, 0, 3], 3000), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].1.len(), out[0].1[0]), (33, CHALLENGE));
    }

    #[test]
    fn a_private_relay_checks_its_mac() {
        let key = crate::oauth::random();
        let mut r = Relay::new(Some(key), Instant::now());
        let (kp, t) = pair();
        let a = udp([10, 0, 0, 1], 1000);
        let mut p = proof(&r, &kp, &t, a, 0);
        let mut out = vec![];
        r.packet(&p, a, &mut out);
        assert!(r.table.is_empty(), "no mac, no place");
        p[17..25].copy_from_slice(&crate::link::relay_mac(&key, &t));
        r.packet(&p, a, &mut out);
        assert_eq!(r.table.len(), 1);
    }

    #[test]
    fn one_ip_holds_at_most_tokens_per_ip() {
        let mut r = Relay::new(None, Instant::now());
        let mut out = vec![];
        for i in 0..TOKENS_PER_IP + 8 {
            let (kp, t) = pair();
            let at = udp([10, 0, 0, 1], 1000 + i as u16);
            r.packet(&proof(&r, &kp, &t, at, 0), at, &mut out);
        }
        assert_eq!(r.table.len(), TOKENS_PER_IP);
    }

    #[test]
    fn one_ip_gets_a_byte_budget_across_all_its_tokens() {
        let mut r = Relay::new(None, Instant::now());
        let tokens: Vec<_> = (0..12).map(|i| meet(&mut r, udp([10, 0, 0, 1], 1000 + i), udp([10, 0, 0, 2], 2000 + i))).collect();
        // 1.5 MB on each token is within its own 2 MB, but 18 MB from one IP is over 16 MB
        let mut passed = 0;
        for (i, t) in tokens.iter().enumerate() {
            for _ in 0..1500 {
                let mut out = vec![];
                r.packet(&data(t, 1024), udp([10, 0, 0, 1], 1000 + i as u16), &mut out);
                passed += out.len() * 1024;
            }
        }
        assert!(passed <= IP_BYTES_PER_MIN && passed > IP_BYTES_PER_MIN - 64 * 1024, "{passed}");
        // another IP isn't charged for it
        let t = meet(&mut r, udp([10, 0, 0, 3], 1), udp([10, 0, 0, 4], 1));
        let mut out = vec![];
        r.packet(&data(&t, 1024), udp([10, 0, 0, 3], 1), &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn spoofed_sources_cant_use_up_everyone_elses_challenges() {
        let mut r = Relay::new(None, Instant::now());
        let t = [7u8; 16];
        let mut out = vec![];
        // the old table stopped answering anyone new once 4096 addresses had asked in a second
        for i in 0..6000u32 {
            let b = i.to_be_bytes();
            r.packet(&data(&t, 40), udp([11, b[1], b[2], b[3]], 9), &mut out);
        }
        out.clear();
        r.packet(&data(&t, 40), udp([10, 9, 9, 9], 9), &mut out);
        assert_eq!(out.len(), 1, "a newcomer still gets its challenge");
        // and one address still gets only a few
        out.clear();
        for _ in 0..50 {
            r.packet(&data(&t, 40), udp([10, 9, 9, 8], 9), &mut out);
        }
        assert!(out.len() <= CHALLENGES_PER_IP as usize, "{}", out.len());
    }

    #[test]
    fn a_tcp_end_that_stops_reading_doesnt_hold_anyone_up() {
        let mut r = Relay::new(None, Instant::now());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        c.write_all(&[0, 0]).unwrap(); // plain framing
        let (s, from) = l.accept().unwrap();
        let (w, _rd) = crate::tls::accept(s, Instant::now() + Duration::from_secs(2)).unwrap();
        let (tx, _rx) = sync_channel(OUT_QUEUE); // nobody drains it: a writer stuck on a full socket
        r.tcp.insert(from, TcpEnd { out: tx, wire: w, since: Instant::now() });
        let t0 = Instant::now();
        for _ in 0..10_000 {
            let udp = r.route(vec![(End::Tcp(from), vec![0u8; 1000])]);
            assert!(udp.is_empty());
        }
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
        assert_eq!(r.dropped, 10_000 - OUT_QUEUE as u64);
        drop(c);
    }

    #[test]
    fn a_connection_counts_against_its_ip_from_the_start_and_gives_its_place_back() {
        let conns = Arc::new(Mutex::new(Conns::default()));
        let ip: IpAddr = [10, 0, 0, 1].into();
        let held: Vec<_> = (0..TCP_PER_IP).map(|_| Held::take(&conns, ip, 100).unwrap()).collect();
        assert!(Held::take(&conns, ip, 100).is_none(), "per IP");
        let other = Held::take(&conns, [10, 0, 0, 2].into(), 100).unwrap();
        assert!(Held::take(&conns, [10, 0, 0, 3].into(), TCP_PER_IP + 1).is_none(), "all together");
        drop(held);
        drop(other);
        let c = lock(&conns);
        assert_eq!((c.all, c.per_ip.len()), (0, 0));
    }

    /// A relay on localhost (UDP and TCP), for `secs`; returns the TCP address.
    fn local_relay(secs: u64) -> SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        std::thread::spawn(move || serve_with(&sock, Some(l), None, Some(Instant::now() + Duration::from_secs(secs))));
        at
    }

    /// Whether the relay has closed `s` (it reads end-of-file or a reset within `wait`).
    fn closed(mut s: &TcpStream, wait: Duration) -> bool {
        s.set_read_timeout(Some(wait)).unwrap();
        let mut b = [0u8; 64];
        match s.read(&mut b) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
        }
    }

    #[test]
    fn silent_connections_from_one_ip_get_only_their_share() {
        let at = local_relay(8);
        let v: Vec<TcpStream> = (0..TCP_PER_IP + 8).map(|_| TcpStream::connect(at).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(500));
        let open = v.iter().filter(|s| !closed(s, Duration::from_millis(100))).count();
        assert_eq!(open, TCP_PER_IP, "connections still in their handshake count too");
    }

    #[test]
    fn a_handshake_trickled_a_byte_at_a_time_is_cut_off() {
        let at = local_relay(20);
        let mut s = TcpStream::connect(at).unwrap();
        let t0 = Instant::now();
        // a TLS record header, then one byte every 400 ms: each read is quick, the whole never ends
        s.write_all(&[0x16]).unwrap();
        let gone = loop {
            std::thread::sleep(Duration::from_millis(400));
            if s.write_all(&[0x03]).is_err() || closed(&s, Duration::from_millis(1)) {
                break t0.elapsed();
            }
            if t0.elapsed() > Duration::from_secs(16) {
                panic!("still open after {:?}", t0.elapsed());
            }
        };
        assert!(gone >= HANDSHAKE - Duration::from_secs(1), "{gone:?}");
    }
}
