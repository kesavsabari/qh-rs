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

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RELAYED: u8 = 0x53;
use crate::link::{CHALLENGE, PROOF};
const CHALLENGES_PER_IP: u32 = 4; // per second: all a spoofed sender can make us send its victim
const CHALLENGES: u32 = 2000;      // per second, all addresses together
const IDLE: Duration = Duration::from_secs(120); // an address that went quiet is forgotten
const BYTES_PER_MIN: usize = 2 * 1024 * 1024; // per token: same budget as a teammate relay
const TOKENS_PER_IP: usize = 256;
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
    if c != cookie(key, token, from, w) && c != cookie(key, token, from, w.saturating_sub(1)) {
        return None;
    }
    if crate::link::relay_token(pk) != *token {
        return None;
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk)
        .verify(&crate::link::proof_bytes(token, c, side), sig).ok()?;
    Some(side as usize)
}

pub fn cli(args: &[String]) -> i32 {
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

const TCP_CONNS: usize = 4096;
const TCP_PER_IP: usize = 16;

/// What the relay keeps. The UDP loop and every TCP connection feed it packets.
struct Relay {
    secret: Option<[u8; 32]>,
    key: ring::hmac::Key,
    table: HashMap<[u8; 16], Slot>,
    per_ip: HashMap<IpAddr, usize>,
    asked: HashMap<IpAddr, u32>,
    asked_at: Instant,
    asked_all: u32,
    swept: Instant,
    passed: u64,
    dropped: u64,
    tcp: HashMap<SocketAddr, Arc<crate::tls::Wire>>, // open TCP ends, to write their packets to
}

impl Relay {
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
        for s in self.table.values() {
            for (e, _) in s.ends.iter().flatten() {
                *self.per_ip.entry(e.addr().ip()).or_default() += 1;
            }
        }
    }

    /// One packet from `from`; what to send where goes into `out`.
    fn packet(&mut self, buf: &[u8], from: End, out: &mut Vec<(End, Vec<u8>)>) {
        let now = Instant::now();
        let n = buf.len();
        if n < 26 || buf[0] != RELAYED {
            return;
        }
        let token: [u8; 16] = buf[1..17].try_into().unwrap();
        if let Some(k) = &self.secret {
            if crate::link::relay_mac(k, &token) != buf[17..25] {
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
                self.table.insert(token, Slot { ends: [None, None], window: now, bytes: 0 });
            }
            // that side of the pair is here now (it moved, or it's new)
            self.table.get_mut(&token).unwrap().ends[side] = Some((from, now));
            return;
        }
        let Some((slot, side)) = self.table.get_mut(&token).and_then(|s| {
            let side = s.ends.iter().position(|e| e.is_some_and(|(a, _)| a == from))?;
            Some((s, side))
        }) else {
            // an address that hasn't proved itself: ask it to (within the challenge budget)
            if now - self.asked_at >= Duration::from_secs(1) {
                (self.asked_at, self.asked_all) = (now, 0);
                self.asked.clear();
            }
            let n_ip = self.asked.entry(from.addr().ip()).or_default();
            *n_ip += 1;
            self.asked_all += 1;
            if *n_ip <= CHALLENGES_PER_IP && self.asked_all <= CHALLENGES && self.asked.len() < 4096 {
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
        if slot.bytes > BYTES_PER_MIN {
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

    fn send(&mut self, sock: &UdpSocket, out: Vec<(End, Vec<u8>)>) {
        for (to, b) in out {
            match to {
                End::Udp(a) => { let _ = sock.send_to(&b, a); }
                End::Tcp(a) => {
                    let Some(s) = self.tcp.get(&a) else { continue };
                    if s.send(&b).is_err() {
                        // a stalled reader isn't waited for: it loses its connection
                        s.shutdown();
                        self.tcp.remove(&a);
                    }
                }
            }
        }
    }
}

fn lock(m: &Mutex<Relay>) -> std::sync::MutexGuard<'_, Relay> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The relay loop (`until` stops it, for tests), also taking teammates over TCP on `tcp` (same packets, each with a 2-byte length).
pub fn serve_with(sock: &UdpSocket, tcp: Option<TcpListener>, secret: Option<[u8; 32]>, until: Option<Instant>) {
    let now = Instant::now();
    let relay = Arc::new(Mutex::new(Relay {
        secret, key: ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &crate::oauth::random()),
        table: HashMap::new(), per_ip: HashMap::new(), asked: HashMap::new(), asked_at: now, asked_all: 0,
        swept: now, passed: 0, dropped: 0, tcp: HashMap::new(),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(l) = tcp {
        let (relay, stop, sock) = (relay.clone(), stop.clone(), sock.try_clone().expect("udp socket"));
        let _ = l.set_nonblocking(true);
        std::thread::spawn(move || accept(l, relay, sock, stop));
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
        let mut r = lock(&relay);
        r.sweep(now);
        let Ok((n, from)) = got else { continue };
        let mut out = vec![];
        r.packet(&buf[..n], End::Udp(from), &mut out);
        r.send(sock, out);
    }
    stop.store(true, Ordering::Relaxed);
}

fn accept(l: TcpListener, relay: Arc<Mutex<Relay>>, sock: UdpSocket, stop: Arc<AtomicBool>) {
    let sock = Arc::new(sock);
    let handshaking = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        let Ok((s, from)) = l.accept() else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        {
            let r = lock(&relay);
            if r.tcp.len() + handshaking.load(Ordering::Relaxed) >= TCP_CONNS
                || r.tcp.keys().filter(|a| a.ip() == from.ip()).count() >= TCP_PER_IP {
                continue;
            }
        }
        let (relay, stop, sock, handshaking) = (relay.clone(), stop.clone(), sock.clone(), handshaking.clone());
        handshaking.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let _ = s.set_nonblocking(false);
            let _ = s.set_nodelay(true);
            let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
            // the TLS handshake gets 10 s; after it one read waits at most IDLE
            let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
            let got = crate::tls::accept(s.try_clone().expect("tcp"));
            handshaking.fetch_sub(1, Ordering::Relaxed);
            let Ok((w, mut rd)) = got else { return };
            let _ = s.set_read_timeout(Some(IDLE));
            lock(&relay).tcp.insert(from, w);
            while !stop.load(Ordering::Relaxed) {
                let Ok(pkt) = rd.recv() else { break };
                let mut out = vec![];
                let mut r = lock(&relay);
                r.packet(&pkt, End::Tcp(from), &mut out);
                r.send(&sock, out);
            }
            lock(&relay).tcp.remove(&from);
        });
    }
}
