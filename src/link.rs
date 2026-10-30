//! qh-link: a direct, encrypted line between teammates, so questions, notes and claims arrive in
//! well under a second instead of on the next git sync. Standalone: it knows nothing about teams;
//! it is given its own member key and each teammate's public key plus addresses (from the git
//! refs), and hands back messages.
//!
//! Finding each other: every side publishes its addresses: LAN, IPv6, a UPnP-mapped port, and
//! the public address a STUN server sees. Both sides then send hellos to all of them at once:
//! whichever answers first is the path, which also punches through most NATs because both sides
//! send at the same time. Better paths replace worse ones later (LAN < IPv6 < public). When no
//! direct path works, a teammate who reaches both relays the (still end-to-end sealed) packets.
//! When nothing works, `send` reports Failed and the caller uses git.
//!
//! Wire: every UDP packet is a hello or a data packet; nothing on it names anyone.
//!   hello = 0x51 ‖ ephemeral pub(32) ‖ nonce(12) ‖ sealed{time ms, to-id, is-reply} with the two
//!           members' long-term key pair (so only a teammate can make one; the receiver finds
//!           whose by trying each teammate's key, within a budget for unknown senders)
//!   data  = 0x52 ‖ session tag(8) ‖ counter(8) ‖ ChaCha20-Poly1305 under keys from both the
//!           long-term and the ephemeral exchange, one key per direction
//! Inside data: message fragments (acked, resent), acks, pings, relay requests, address news, and
//! path checks (a line moves to a new address only once a check sent there comes back).
use crate::oauth::{b64url, b64url_decode, random};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use x25519_dalek::{PublicKey, StaticSecret};

const HELLO: u8 = 0x51;
const DATA: u8 = 0x52;
const STUN_MAGIC: u32 = 0x2112_A442;
const STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

const FRAG: u8 = 1;
const ACK: u8 = 2;
#[allow(dead_code)] // the keepalive older qh sends (now ECHO); the tests send it
const PING: u8 = 3;
const FWD: u8 = 5; // "please pass this packet to <id>"
const FWDED: u8 = 6; // "this packet was passed to you by me"
const ADDRS: u8 = 7; // "my addresses changed"
const CHECK: u8 = 8; // "is this really you, on this path?" (nonce): the line moves only when it comes back
const CHECKED: u8 = 9; // the answer, sent back the way the check came
const ECHO: u8 = 10; // a keepalive that asks for its 8 bytes back (round-trip time); older qh ignores it
const ECHOED: u8 = 11; // the 8 bytes, back

const PART: usize = 900; // fragment size: a relayed fragment still fits in 1200 bytes
const MAX_MSG: usize = 16 * 1024 * 1024; // diffs, files, handoff context
const MAX_PARTIAL: usize = 16; // messages half-received from one teammate at a time
const MAX_PARTIAL_BYTES: usize = 2 * MAX_MSG; // and what they may hold
const GIVE_UP: Duration = Duration::from_secs(10); // with no line, or with nothing acked for this long
// Parts of one message in flight at once; each ack lets the next one go, so a big message streams
// at the line's pace instead of landing all at once, and a small one is never stuck behind it.
const WINDOW: usize = 128;
const DEAD: Duration = Duration::from_secs(40);
const KEEPALIVE: Duration = Duration::from_secs(15);
const TRY_RELAY_AFTER: Duration = Duration::from_secs(3);
// A teammate who reaches both is preferred over a relay server: the server waits a little longer
// when there's one, so the teammate's path usually comes up first.
const TRY_SERVER_AFTER: Duration = Duration::from_secs(6);
const REDISCOVER: Duration = Duration::from_secs(60); // STUN again: the router may have a new public address
const NET_CHECK: Duration = Duration::from_secs(2); // a new local address means we moved networks
const UPNP_RENEW: Duration = Duration::from_secs(1800); // mappings are asked for an hour
const PUNCH_FOR: Duration = Duration::from_secs(30); // after a move, hello every second while the line is up
// LAN beacon: hellos for teammates we have no line to also go to this multicast group (TTL 1, never
// leaves the network), so people on the same network find each other with no addresses at all.
const BEACON_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 81, 72);
const BEACON_PORT: u16 = 47231;
// Relay servers (`qh relay`): [0x53][token 16][mac 8][packet]. Two teammates meet there under a token
// only the pair can compute (from their shared key, new each day); the server pairs the token's two
// senders and passes packets through untouched. mac: HMAC(server secret, token) for private servers.
// An address only gets packets once it proved it is one of the pair, at that address: the server
// answers a new address with a challenge (a cookie bound to the address), and the pair member
// signs the cookie with the pair's key for the day (whose public half hashes to the token). So a
// relay can't be used to bounce traffic at someone, and no one who merely sees the token can
// take over a pair's place.
const RELAYED: u8 = 0x53;
pub const CHALLENGE: u8 = 0x54; // server -> address: [0x54][token 16][cookie 16]
pub const PROOF: u8 = 0x55;     // address -> server, inside RELAYED: [0x55][side][cookie 16][pair key 32][signature 64]
// A hello costs one trial decryption per teammate before we know it's real, so strangers get a
// budget (per second): per source address, and all addresses we've never seen a teammate at.
const HELLOS_PER_IP: u32 = 50;
const STRANGER_HELLOS: u32 = 500;
pub const RELAY_PORT: u16 = 47232;
/// Where a relay also takes the same packets over TCP, for networks that block UDP: 443, so it
/// passes firewalls that only let web traffic out.
pub const RELAY_TCP_PORT: u16 = 443;
const TCP_AFTER: Duration = Duration::from_secs(5); // a relay silent this long on UDP: try TCP
const TCP_RETRY: Duration = Duration::from_secs(60);
// Birthday punch, for a strict router on one side only: the strict side opens many sockets toward
// the other's public address, the other sprays hellos at random ports on the strict side's IP until
// one lands in an open hole. Rate-limited and rare, as it looks a little like a port scan.
const BDAY_SOCKETS: usize = 256;
const BDAY_FOR: Duration = Duration::from_secs(20);
const BDAY_RATE: u32 = 100; // probes a second from the side that sprays
const BDAY_EVERY: Duration = Duration::from_secs(300);

/// One packet on a TCP stream: a 2-byte length, then the packet.
pub fn write_frame(w: &mut impl Write, pkt: &[u8]) -> std::io::Result<()> {
    let n = u16::try_from(pkt.len()).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    w.write_all(&[&n.to_be_bytes()[..], pkt].concat())
}

/// The relay used when the config says relay = "default". Empty until the project runs one.
pub const DEFAULT_RELAY: Option<&str> = Some("15.235.127.134:47232");
const RELAY_BYTES_PER_MIN: usize = 2 * 1024 * 1024;
// A teammate who left: keep saying hello where they last were, a little more often than an
// unanswered UDP flow is forgotten (30 s on Linux), so our side of the hole stays open for them.
const STANDING_GAP: Duration = Duration::from_secs(25);
const STANDING_FOR: Duration = Duration::from_secs(4 * 3600);
const REMEMBER: usize = 4; // endpoints kept per teammate

type Id = [u8; 8];

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Up { peer: String, path: String },
    Down { peer: String },
    Msg { from: String, data: Vec<u8> },
    Delivered { peer: String, id: u64 },
    Failed { peer: String, id: u64 },
    LanBlocked, // macOS refuses our LAN sends: Local Network access not granted
}

/// One teammate's line as `Link::lines` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct LineInfo {
    pub public: String,
    pub path: Option<String>,   // "lan 192.168.1.4:7001", "via <key>", "relay ovh-ca"; None: no line
    pub rtt_ms: Option<u64>,    // None until the first echo comes back (or they run an older qh)
    pub trying_secs: Option<u64>,
    pub why: Option<String>,    // why there's no line, when there isn't
    pub addrs: Vec<String>,     // where they said they are
    pub relays: Vec<String>,    // relay servers we could meet them on
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Path {
    Direct(SocketAddr),
    Via(Id),            // a teammate passes it on
    Server(SocketAddr), // a relay server passes it on
}

/// A relay server: where, and the secret a private one asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Server {
    addr: SocketAddr,
    secret: Option<[u8; 32]>,
    tcp: u16, // its TCP port, when UDP doesn't get through
}

/// "relay=1.2.3.4:47232/8443#secret" (or without "relay=", "/tcp port" or "#secret"); numeric
/// addresses only.
fn parse_server(s: &str) -> Option<Server> {
    let s = s.strip_prefix("relay=").unwrap_or(s);
    let (a, k) = s.split_once('#').map_or((s, None), |(a, k)| (a, Some(k)));
    let (a, tcp) = match a.split_once('/') {
        Some((a, t)) => (a, t.parse().ok()?),
        None => (a, RELAY_TCP_PORT),
    };
    let secret = match k {
        Some(k) => Some(<[u8; 32]>::try_from(b64url_decode(k)).ok()?),
        None => None,
    };
    Some(Server { addr: a.parse().ok()?, secret, tcp })
}

fn server_str(s: &Server) -> String {
    // the default port is left out: an older qh can't read "/port" and would drop the relay
    let tcp = if s.tcp == RELAY_TCP_PORT { String::new() } else { format!("/{}", s.tcp) };
    format!("relay={}{tcp}{}", s.addr, s.secret.map(|k| format!("#{}", b64url(&k))).unwrap_or_default())
}

/// What a private relay checks: HMAC(secret, token), first 8 bytes. Public relays get zeros.
pub fn relay_mac(secret: &[u8; 32], token: &[u8]) -> [u8; 8] {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::sign(&key, token).as_ref()[..8].try_into().unwrap()
}

/// The token the relay checks a proof against: the first 16 bytes of SHA-256(pair key).
pub fn relay_token(pair_pub: &[u8]) -> [u8; 16] {
    ring::digest::digest(&ring::digest::SHA256, pair_pub).as_ref()[..16].try_into().unwrap()
}

/// What a pair member signs to prove an address at a relay.
pub fn proof_bytes(token: &[u8], cookie: &[u8], side: u8) -> Vec<u8> {
    [&b"qh relay proof v1"[..], token, cookie, &[side]].concat()
}

static RELAY_IDS: Mutex<Option<HashMap<([u8; 32], u64), ([u8; 16], [u8; 32])>>> = Mutex::new(None);

/// The pair's meeting tokens for now (with the seed of the day's pair key): today's, plus the
/// neighbouring day's within 10 minutes of midnight (UTC), so two clocks a little apart still meet.
fn pair_ids(static_dh: &[u8; 32]) -> Vec<([u8; 16], [u8; 32])> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (day, into) = (secs / 86400, secs % 86400);
    let mut days = vec![day];
    if into < 600 {
        days.push(day.saturating_sub(1));
    }
    if into > 86400 - 600 {
        days.push(day + 1);
    }
    let mut g = lock(&RELAY_IDS);
    let cache = g.get_or_insert_with(HashMap::new);
    if cache.len() > 4096 {
        cache.clear();
    }
    days.into_iter().map(|d| *cache.entry((*static_dh, d)).or_insert_with(|| {
        use ring::signature::KeyPair;
        let seed = hkdf(static_dh, &[&b"relay"[..], &d.to_be_bytes()].concat());
        let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).expect("32-byte seed");
        (relay_token(kp.public_key().as_ref()), seed)
    })).collect()
}

fn relay_header(srv: &Server, t: &[u8; 16]) -> Vec<u8> {
    let mut out = vec![RELAYED];
    out.extend(t);
    out.extend(srv.secret.map_or([0; 8], |s| relay_mac(&s, t)));
    out
}

/// Send `pkt` for peer `k` through relay server `srv`.
fn via_server(i: &Inner, k: usize, srv: &Server, pkt: &[u8], sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    for (t, _) in pair_ids(&i.peers[k].static_dh) {
        let mut out = relay_header(srv, &t);
        out.extend(pkt);
        sends.push((srv.addr, out));
    }
}

/// A relay server we use asked this address to prove it's one of a pair: sign its cookie with the
/// pair's key (at most a few times a second per teammate, whoever sends the challenges).
fn on_challenge(i: &mut Inner, pkt: &[u8], from: SocketAddr, sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    let (token, cookie) = (&pkt[1..17], &pkt[17..33]);
    for k in 0..i.peers.len() {
        let Some(srv) = server_at(i, k, from) else { continue };
        let Some((t, seed)) = pair_ids(&i.peers[k].static_dh).into_iter().find(|(t, _)| t == token) else { continue };
        let p = &mut i.peers[k];
        if p.proved.elapsed() < Duration::from_millis(300) {
            return;
        }
        p.proved = Instant::now();
        use ring::signature::KeyPair;
        let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).expect("32-byte seed");
        let side = (i.me > i.peers[k].id) as u8; // the two of a pair always pick different sides
        let mut out = relay_header(&srv, &t);
        out.push(PROOF);
        out.push(side);
        out.extend(cookie);
        out.extend(kp.public_key().as_ref());
        out.extend(kp.sign(&proof_bytes(&t, cookie, side)).as_ref());
        sends.push((from, out));
        return;
    }
}

/// Relay servers we could meet peer `k` at: ours and the ones they published.
fn servers_for(i: &Inner, k: usize) -> Vec<Server> {
    let mut v = i.servers.clone();
    v.extend(i.peers[k].relays.iter().filter(|r| !i.servers.iter().any(|s| s.addr == r.addr)));
    v
}

fn server_at(i: &Inner, k: usize, a: SocketAddr) -> Option<Server> {
    servers_for(i, k).into_iter().find(|s| s.addr == a)
}

/// How good a path is: same network beats IPv6 beats the public internet beats a relay.
fn rank(p: &Path) -> u8 {
    match p {
        Path::Via(_) => 3,
        Path::Server(_) => 4,
        Path::Direct(a) => match a.ip() {
            IpAddr::V4(ip) => if ip.is_private() || ip.is_loopback() || ip.is_link_local() { 0 } else { 2 },
            IpAddr::V6(ip) => {
                if let Some(v4) = ip.to_ipv4_mapped() {
                    return rank(&Path::Direct(SocketAddr::new(IpAddr::V4(v4), a.port())));
                }
                let s = ip.segments()[0];
                if ip.is_loopback() || s & 0xffc0 == 0xfe80 || s & 0xfe00 == 0xfc00 { 0 } else { 1 }
            }
        },
    }
}

fn kind(a: &SocketAddr) -> &'static str {
    match rank(&Path::Direct(*a)) {
        0 => "lan",
        1 => "ipv6",
        _ => "public",
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn id_of(public: &[u8; 32]) -> Id {
    ring::digest::digest(&ring::digest::SHA256, public).as_ref()[..8].try_into().unwrap()
}

fn hkdf(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"qh-link v1");
    let mut out = [0u8; 32];
    let info = [info];
    salt.extract(ikm).expand(&info, ring::hkdf::HKDF_SHA256).expect("hkdf").fill(&mut out).expect("hkdf");
    out
}

fn aead(k: &[u8; 32]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, k).expect("32-byte key"))
}

fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(n)
}

struct Session {
    their_eph: [u8; 32],
    send: LessSafeKey,
    recv: LessSafeKey,
    tx_tag: Id, // put on our DATA packets so they find this session; derived from the session keys,
    rx_tag: Id, // so nothing on the wire stays the same from one session to the next
    counter: u64,
    rx_top: u64,
    rx_seen: u64, // bit i = rx_top - i arrived (replay window of 64)
    path: Path,
    path_rx: Instant,
    last_rx: Instant,
    last_tx: Instant,
    announced: bool,
    heard_addrs: bool, // they sent their addresses this session (we answer the first with ours)
    probe: Option<(Path, [u8; 8], Instant)>, // a path we'd move to, once our check comes back on it
    rtt: Option<Duration>, // smoothed round trip, from ECHO
}

impl Session {
    /// A counter not seen before and not too old; marks it seen.
    fn fresh(&mut self, c: u64) -> bool {
        if c > self.rx_top {
            let shift = c - self.rx_top;
            self.rx_seen = if shift >= 64 { 1 } else { self.rx_seen << shift | 1 };
            self.rx_top = c;
            return true;
        }
        let back = self.rx_top - c;
        if back >= 64 || self.rx_seen >> back & 1 == 1 {
            return false;
        }
        self.rx_seen |= 1 << back;
        true
    }
}

struct Out {
    id: u64,
    part: u16,
    count: u16,
    bytes: Vec<u8>,
    next: Instant,
    wait: Duration,
    since: Instant, // queued, then (once sent) the last time this message made progress
    sent: bool,
}

/// The FRAG frame for a part.
fn frag_of(o: &Out) -> Vec<u8> {
    let mut f = vec![FRAG];
    f.extend(o.id.to_be_bytes());
    f.extend(o.part.to_be_bytes());
    f.extend(o.count.to_be_bytes());
    f.extend(&o.bytes);
    f
}

struct Peer {
    public: String,
    id: Id,
    static_dh: [u8; 32],
    hello_key: LessSafeKey,
    addrs: Vec<SocketAddr>,
    known: Vec<SocketAddr>, // where a line to them last worked (remembered across restarts), newest first
    relays: Vec<Server>,    // relay servers they use (published with their addresses)
    sess: Option<Session>,
    trying_since: Option<Instant>,
    next_hello: Instant,
    last_upgrade: Instant,
    punch: Option<Instant>, // since when we re-punch an up line (someone's address changed)
    addrs_ms: u64,          // when the peer's own clock said `addrs` were current (0: unknown)
    hello_ts: u64,
    out: VecDeque<Out>,
    parts: HashMap<u64, (Vec<Option<Vec<u8>>>, Instant, usize)>, // parts, last one in, bytes held
    done: VecDeque<u64>,
    resync_at: Instant,
    relayed: (Instant, usize),
    proved: Instant, // when we last answered a relay server's challenge for this pair
    strict: bool,    // they said their router changes ports per destination
    bday_at: Option<Instant>, // our last birthday punch toward them
}

struct Inner {
    secret: StaticSecret,
    me: Id,
    eph: StaticSecret,
    eph_pub: [u8; 32],
    peers: Vec<Peer>,
    next_id: u64,
    mine: Vec<SocketAddr>,
    mine_ms: u64, // when `mine` last changed (our clock); sent along so peers keep the newest
    stun_tx: HashMap<[u8; 12], Instant>,
    stun_seen: Vec<SocketAddr>,
    relay: bool,
    beacon: bool, // our hellos also go to the LAN beacon group
    servers: Vec<Server>, // relay servers we use (resolved from relay_specs)
    relay_specs: Vec<String>, // the setting they come from ("host:port#secret"), changeable while running
    rediscover: bool,         // look again now (the relay setting changed)
    blocked: Vec<Id>, // tests only: pretend these can't be reached directly
    memory: HashMap<String, Vec<SocketAddr>>, // public key -> known endpoints, as saved
    memory_dirty: bool,
    hello_keys: Arc<Vec<(Id, [u8; 32])>>, // each teammate's hello key: tried outside the lock
    over_tcp: Vec<SocketAddr>, // relay servers we reach over TCP right now
}

/// The hello budget (see HELLOS_PER_IP).
struct Gate {
    window: Instant,
    per_ip: HashMap<IpAddr, u32>,
    strangers: u32,
}

impl Gate {
    fn allow(&mut self, ip: IpAddr, known: bool) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.per_ip.clear();
            self.strangers = 0;
        }
        if !known {
            self.strangers += 1;
            if self.strangers > STRANGER_HELLOS || self.per_ip.len() >= 4096 {
                return false;
            }
        }
        let n = self.per_ip.entry(ip).or_default();
        *n += 1;
        *n <= HELLOS_PER_IP
    }
}

pub struct Link {
    inner: Arc<Mutex<Inner>>,
    v4: Arc<UdpSocket>,
    v6: Option<Arc<UdpSocket>>,
    stop: Arc<AtomicBool>,
    events: Sender<Event>,
    mapping: Mutex<Option<Upnp>>, // the router port we asked for, given back at stop
    lanes: Mutex<HashMap<SocketAddr, Lane>>, // per relay server: is UDP getting through, or TCP
    bday: Mutex<Bday>,
    lan_refused: AtomicU32, // LAN sends refused in a row ("no route to host" with the host right there)
    #[cfg(test)]
    udp_off: Mutex<Vec<SocketAddr>>, // tests: this relay can't be reached over UDP
}

/// The strict side of a birthday punch: the sockets held open, and which one reaches whom once
/// a hole worked (their packets only come back through the socket that opened it).
#[derive(Default)]
struct Bday {
    socks: Vec<Arc<UdpSocket>>,
    until: Option<Instant>,
    route: HashMap<SocketAddr, Arc<UdpSocket>>,
    reading: bool, // a thread reads the sockets
}

/// How we reach one relay server. UDP until it stays silent, then TCP.
#[derive(Default)]
struct Lane {
    tcp: Option<Arc<crate::tls::Wire>>,
    sent: Option<Instant>,  // first UDP packet to it that hasn't been answered yet
    heard: Option<Instant>, // its last packet to us
    failed: Option<Instant>, // the last TCP attempt that failed or dropped
    connecting: bool,
}

/// A UPnP port mapping on the home router: where to ask, and which port.
struct Upnp {
    url: String,
    service: String,
    port: u16,
}

pub struct Options {
    pub port: u16,
    pub stun: bool,
    pub upnp: bool,
    pub relay: bool,
    pub beacon: bool,
    /// Where to keep the teammates met and the endpoints that worked. With it, the port is stable
    /// (derived from the member key) and teammates met before are found again with no rendezvous.
    pub remember: Option<std::path::PathBuf>,
    /// Relay servers to use when nothing else reaches a teammate: "host:port" or "host:port#secret".
    pub relays: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options { port: 0, stun: true, upnp: true, relay: true, beacon: true, remember: None, relays: vec![] }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Link {
    /// Open the sockets and start the background threads. Events come out of the receiver.
    pub fn start(secret: StaticSecret, o: Options) -> std::io::Result<(Arc<Link>, Receiver<Event>)> {
        let me = id_of(PublicKey::from(&secret).as_bytes());
        // The same port every time on this network: a router that keeps ports then gives us the
        // same public address after a restart, so remembered endpoints stay right on both sides.
        // It comes from our secret and the LAN address, so it differs from network to network and
        // nobody (teammates included) can predict it: a fixed port would follow us around.
        let stable = o.remember.as_ref().filter(|_| o.port == 0).map(|_| {
            let lan = local_ip(false).map(|ip| ip.to_string()).unwrap_or_default();
            let h = hkdf(&secret.to_bytes(), &[&b"port"[..], lan.as_bytes()].concat());
            20000 + u16::from_be_bytes([h[0], h[1]]) % 40000
        });
        let v4 = Arc::new(match stable.and_then(|p| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, p)).ok()) {
            Some(s) => s,
            None => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, o.port))?,
        });
        let v6 = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok().map(Arc::new);
        let memory: HashMap<String, Vec<SocketAddr>> = o.remember.as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| serde_json::from_str::<HashMap<String, Vec<String>>>(&t).ok())
            .map(|m| m.into_iter().map(|(k, v)| (k, v.iter().filter_map(|a| a.parse().ok()).collect())).collect())
            .unwrap_or_default();
        let eph = StaticSecret::from(random());
        let eph_pub = *PublicKey::from(&eph).as_bytes();
        let mut next = [0u8; 8];
        next.copy_from_slice(&random()[..8]);
        let inner = Inner {
            secret, me, eph, eph_pub, peers: vec![], hello_keys: Arc::new(vec![]), next_id: u64::from_be_bytes(next) >> 2,
            mine: vec![], mine_ms: 0, stun_tx: HashMap::new(), stun_seen: vec![], relay: o.relay, beacon: false, servers: vec![], relay_specs: o.relays.clone(), rediscover: false, blocked: vec![],
            memory, memory_dirty: false, over_tcp: vec![],
        };
        let (tx, rx) = channel();
        let link = Arc::new(Link { inner: Arc::new(Mutex::new(inner)), v4, v6, stop: Arc::new(AtomicBool::new(false)), events: tx, mapping: Mutex::new(None),
            lanes: Mutex::new(HashMap::new()), bday: Mutex::default(), lan_refused: AtomicU32::new(0), #[cfg(test)] udp_off: Mutex::new(vec![]) });
        link.v4.set_read_timeout(Some(Duration::from_millis(200)))?;
        // The beacon listens on a fixed port; a second qh on this machine finds it taken and goes
        // without (teammates on one machine reach each other by the LAN address anyway).
        let beacon = o.beacon.then(|| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, BEACON_PORT)).ok()).flatten()
            .filter(|b| b.join_multicast_v4(&BEACON_GROUP, &Ipv4Addr::UNSPECIFIED).is_ok()).map(Arc::new);
        if o.beacon {
            let _ = link.v4.set_multicast_ttl_v4(1);
            lock(&link.inner).beacon = true;
        }
        for s in [Some(link.v4.clone()), link.v6.clone(), beacon].into_iter().flatten() {
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            let l = link.clone();
            std::thread::spawn(move || l.receive(&s));
        }
        let l = link.clone();
        let file = o.remember.clone();
        std::thread::spawn(move || {
            while !l.stop.load(Ordering::Relaxed) {
                l.tick();
                l.check_lanes();
                if let Some(f) = &file {
                    l.save_memory(f);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let l = link.clone();
        std::thread::spawn(move || l.discover(o.stun, o.upnp));
        Ok((link, rx))
    }

    /// Stop the threads and give the router port back (the lease would expire in an hour anyway).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(m) = lock(&self.mapping).take() {
            let _ = soap(&upnp_agent(), &m.url, &m.service, "DeletePortMapping", &format!(
                "<NewRemoteHost></NewRemoteHost><NewExternalPort>{}</NewExternalPort><NewProtocol>UDP</NewProtocol>", m.port));
        }
    }

    /// Use these relay servers from now on (the user changed the setting); teammates are told.
    pub fn set_relays(&self, specs: Vec<String>) {
        let mut i = lock(&self.inner);
        i.relay_specs = specs;
        i.rediscover = true;
    }

    /// Drop a teammate: their line, their pending messages, and our memory of where they were.
    pub fn forget(&self, public: &str) {
        let mut i = lock(&self.inner);
        i.peers.retain(|p| p.public != public);
        rekey(&mut i);
        if i.memory.remove(public).is_some() {
            i.memory_dirty = true;
        }
    }

    /// Teammates met before (their public keys), from the memory file.
    pub fn remembered(&self) -> Vec<String> {
        lock(&self.inner).memory.keys().cloned().collect()
    }

    fn save_memory(&self, file: &std::path::Path) {
        let mut i = lock(&self.inner);
        if !i.memory_dirty {
            return;
        }
        i.memory_dirty = false;
        let m: HashMap<&String, Vec<String>> = i.memory.iter().map(|(k, v)| (k, v.iter().map(|a| a.to_string()).collect())).collect();
        let text = serde_json::to_string(&m).unwrap_or_default();
        drop(i);
        let _ = std::fs::create_dir_all(file.parent().unwrap_or(file));
        let _ = crate::platform::write_private(file, text.as_bytes());
    }

    /// This side's public key (what teammates know us by).
    pub fn me(&self) -> String {
        b64url(PublicKey::from(&lock(&self.inner).secret).as_bytes())
    }

    pub fn port(&self) -> u16 {
        self.v4.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// Addresses to publish, best first, as "kind=addr" (kind: lan, ipv6, upnp, public).
    pub fn addrs(&self) -> Vec<String> {
        let mut v: Vec<(u8, String)> = lock(&self.inner).mine.iter()
            .map(|a| (rank(&Path::Direct(*a)), format!("{}={a}", kind(a)))).collect();
        v.sort();
        v.dedup();
        let mut out: Vec<String> = v.into_iter().map(|(_, s)| s).collect();
        if strict(&lock(&self.inner)) {
            out.push("nat=strict".into()); // older qh skips what isn't an address
        }
        out.extend(lock(&self.inner).servers.iter().map(server_str));
        out
    }

    /// When our published addresses last changed (ms, our clock): publish it with them.
    pub fn addrs_ms(&self) -> u64 {
        lock(&self.inner).mine_ms
    }

    /// Whether this side looks to be behind a NAT that changes ports per destination; hole
    /// punching rarely works then, so a relay is the likely path.
    pub fn strict_nat(&self) -> bool {
        strict(&lock(&self.inner))
    }

    /// Learn (or update) a teammate: their public key and published addresses, and when (their
    /// clock, ms) those addresses were current. A list older than the one we hold is ignored, so
    /// a slow git copy can't undo a fresh one from the line; 0 means "unknown, take it".
    pub fn peer(&self, public: &str, addrs: &[String], ms: u64) {
        let Ok(pk) = <[u8; 32]>::try_from(b64url_decode(public)) else { return };
        let relays: Vec<Server> = addrs.iter().filter(|a| a.starts_with("relay=")).filter_map(|a| parse_server(a)).collect();
        let strict = addrs.iter().any(|a| a == "nat=strict");
        let addrs: Vec<SocketAddr> = addrs.iter()
            .filter_map(|a| a.rsplit_once('=').map_or(a.as_str(), |(_, x)| x).parse().ok()).collect();
        let mut i = lock(&self.inner);
        let id = id_of(&pk);
        if id == i.me {
            return;
        }
        if let Some(p) = i.peers.iter_mut().find(|p| p.id == id) {
            if ms != 0 && ms < p.addrs_ms {
                return;
            }
            p.addrs_ms = p.addrs_ms.max(ms);
            if !relays.is_empty() {
                p.relays = relays;
            }
            if !addrs.is_empty() {
                p.strict = strict;
            }
            if !addrs.is_empty() && p.addrs != addrs {
                p.addrs = addrs;
                p.next_hello = Instant::now();
                if p.sess.is_some() {
                    p.punch = Some(Instant::now());
                }
            }
            return;
        }
        let static_dh = *i.secret.diffie_hellman(&PublicKey::from(pk)).as_bytes();
        let now = Instant::now();
        let known = i.memory.get(public).cloned().unwrap_or_default();
        i.peers.push(Peer {
            public: public.to_string(), id, static_dh, hello_key: aead(&hkdf(&static_dh, b"hello")), addrs, addrs_ms: ms, known, relays,
            sess: None, trying_since: None, next_hello: now, last_upgrade: now, punch: None, hello_ts: 0,
            out: VecDeque::new(), parts: HashMap::new(), done: VecDeque::new(), resync_at: now, relayed: (now, 0),
            proved: now - Duration::from_secs(1), strict, bday_at: None,
        });
        rekey(&mut i);
    }

    /// Queue a message for a teammate. The id comes back in Delivered or Failed. Messages wait up
    /// to 10 s for a line, so sending right after `peer` is fine.
    pub fn send(&self, to: &str, data: &[u8]) -> Option<u64> {
        if data.len() > MAX_MSG {
            return None;
        }
        let mut i = lock(&self.inner);
        i.next_id += 1;
        let id = i.next_id;
        let p = i.peers.iter_mut().find(|p| p.public == to)?;
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(PART).collect() };
        let now = Instant::now();
        for (n, c) in chunks.iter().enumerate() {
            p.out.push_back(Out {
                id, part: n as u16, count: chunks.len() as u16, bytes: c.to_vec(), next: now,
                wait: Duration::from_millis(300), since: now, sent: false,
            });
        }
        Some(id)
    }

    /// Who is reachable and how ("lan 192.168.1.4:7001", "via <key>"), for the status line.
    pub fn status(&self) -> Vec<(String, Option<String>)> {
        let i = lock(&self.inner);
        i.peers.iter().map(|p| (p.public.clone(), p.sess.as_ref().map(|s| describe(&i, &s.path)))).collect()
    }

    /// Every teammate's line in detail, for `/team link`.
    pub fn lines(&self) -> Vec<LineInfo> {
        let i = lock(&self.inner);
        let strict = strict(&i);
        i.peers.iter().enumerate().map(|(k, p)| {
            let s = p.sess.as_ref();
            let why = match s {
                Some(_) => None,
                None if p.addrs.is_empty() && p.known.is_empty() && servers_for(&i, k).is_empty() =>
                    Some("no address for them yet and no relay: waiting for their card by git".into()),
                None if servers_for(&i, k).is_empty() && strict =>
                    Some("your router changes ports per destination (strict NAT) and no relay is set: /relay".into()),
                None if strict => Some("your router changes ports per destination (strict NAT): trying the relay".into()),
                None => Some("no answer on any address yet (offline, or their network blocks it)".into()),
            };
            LineInfo {
                public: p.public.clone(),
                path: s.map(|s| describe(&i, &s.path)),
                rtt_ms: s.and_then(|s| s.rtt).map(|r| r.as_millis() as u64),
                trying_secs: p.trying_since.map(|t| t.elapsed().as_secs()),
                why,
                addrs: p.addrs.iter().map(|a| a.to_string()).collect(),
                relays: servers_for(&i, k).iter().map(|r| relay_name(&r.addr)).collect(),
            }
        }).collect()
    }

    /// Tests: act as if `public` can't be reached directly (so the relay gets used).
    #[cfg(test)]
    fn block(&self, public: &str) {
        let pk: [u8; 32] = b64url_decode(public).try_into().unwrap();
        lock(&self.inner).blocked.push(id_of(&pk));
    }

    fn raw_send(&self, to: SocketAddr, bytes: &[u8]) {
        let to = match to {
            SocketAddr::V6(a) => a.ip().to_ipv4_mapped().map_or(to, |v4| SocketAddr::new(IpAddr::V4(v4), a.port())),
            v4 => v4,
        };
        if bytes.first() == Some(&RELAYED) {
            let mut lanes = lock(&self.lanes);
            let lane = lanes.entry(to).or_default();
            if let Some(s) = lane.tcp.as_ref() {
                if s.send(bytes).is_ok() {
                    return;
                }
                lane.tcp = None;
                lane.failed = Some(Instant::now());
                drop(lanes);
                lock(&self.inner).over_tcp.retain(|a| *a != to);
            } else {
                lane.sent.get_or_insert_with(Instant::now);
            }
            #[cfg(test)]
            if lock(&self.udp_off).contains(&to) { return }
        }
        if let Some(s) = lock(&self.bday).route.get(&to).cloned() {
            let _ = s.send_to(bytes, to);
            return;
        }
        let sock = if to.is_ipv4() { &self.v4 } else if let Some(s) = &self.v6 { s } else { return };
        let sent = sock.send_to(bytes, to);
        if cfg!(target_os = "macos") && (to.ip().is_multicast() || kind(&to) == "lan") {
            self.lan_check(to, sent.err().map(|e| e.kind()));
        }
    }

    /// macOS answers "no route to host" for every LAN send when qh lacks Local Network access
    /// (System Settings > Privacy & Security > Local Network), often after a network change or
    /// when qh was started over SSH, where there's no one to ask. We then sit on the relay with no
    /// clue why, so say it once. Multicast never needs the host to be up, so one refusal there is
    /// enough; a LAN teammate might just be gone, so wait for a run of them.
    fn lan_check(&self, to: SocketAddr, err: Option<std::io::ErrorKind>) {
        if err != Some(std::io::ErrorKind::HostUnreachable) {
            self.lan_refused.store(0, Ordering::Relaxed);
            return;
        }
        let before = self.lan_refused.load(Ordering::Relaxed);
        let n = if to.ip().is_multicast() { before.max(10) } else { before + 1 };
        self.lan_refused.store(n, Ordering::Relaxed);
        if before < 10 && n >= 10 {
            let _ = self.events.send(Event::LanBlocked);
        }
    }

    /// A relay we keep sending to over UDP that never answers: UDP is blocked on this network (or on
    /// the way), so open the same lane over TCP. The TCP lane stays until it breaks.
    fn check_lanes(self: &Arc<Self>) {
        let now = Instant::now();
        let servers: Vec<Server> = {
            let i = lock(&self.inner);
            let mut v = i.servers.clone();
            v.extend(i.peers.iter().flat_map(|p| p.relays.iter().copied()));
            v
        };
        let mut lanes = lock(&self.lanes);
        for srv in servers {
            let Some(lane) = lanes.get_mut(&srv.addr) else { continue };
            let silent = lane.sent.is_some_and(|t| now - t > TCP_AFTER) && lane.heard.is_none_or(|h| lane.sent.is_some_and(|t| h < t));
            if lane.tcp.is_some() || lane.connecting || !silent || lane.failed.is_some_and(|f| now - f < TCP_RETRY) {
                continue;
            }
            lane.connecting = true;
            let l = self.clone();
            std::thread::spawn(move || l.open_lane(srv));
        }
    }

    fn open_lane(self: Arc<Self>, srv: Server) {
        let at = SocketAddr::new(srv.addr.ip(), srv.tcp);
        let s = TcpStream::connect_timeout(&at, Duration::from_secs(5)).ok().filter(|s| {
            // keepalives come every 15 s, so 45 s of nothing means the connection is gone
            s.set_nodelay(true).is_ok() && s.set_read_timeout(Some(Duration::from_secs(45))).is_ok()
                && s.set_write_timeout(Some(Duration::from_secs(2))).is_ok()
        });
        let s = s.and_then(|s| crate::tls::connect(s).ok());
        let r = {
            let mut lanes = lock(&self.lanes);
            let lane = lanes.entry(srv.addr).or_default();
            lane.connecting = false;
            match s {
                Some((w, r)) => { lane.tcp = Some(w); lane.sent = None; r }
                None => { lane.failed = Some(Instant::now()); return }
            }
        };
        lock(&self.inner).over_tcp.push(srv.addr);
        let mut r = r;
        let mut gate = Gate { window: Instant::now(), per_ip: HashMap::new(), strangers: 0 };
        while !self.stop.load(Ordering::Relaxed) {
            let Ok(pkt) = r.recv() else { break };
            self.packet(&pkt, srv.addr, &mut gate);
        }
        let mut lanes = lock(&self.lanes);
        if let Some(l) = lanes.get_mut(&srv.addr) {
            l.tcp = None;
            l.failed = Some(Instant::now());
        }
        drop(lanes);
        lock(&self.inner).over_tcp.retain(|a| *a != srv.addr);
    }

    fn receive(&self, sock: &UdpSocket) {
        let mut buf = [0u8; 2048];
        let mut gate = Gate { window: Instant::now(), per_ip: HashMap::new(), strangers: 0 };
        while !self.stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            self.packet(&buf[..n], from, &mut gate);
        }
    }

    /// One packet from `from`, whichever way it came (UDP, or a relay's TCP lane).
    fn packet(&self, pkt: &[u8], from: SocketAddr, gate: &mut Gate) {
        if pkt.first().is_some_and(|b| *b == RELAYED || *b == CHALLENGE) {
            if let Some(l) = lock(&self.lanes).get_mut(&from) {
                l.heard = Some(Instant::now());
                l.sent = None;
            }
        }
        let mut i = lock(&self.inner);
        if pkt.len() >= 20 && pkt[0] == 0x01 && pkt[1] == 0x01 {
            stun_reply(&mut i, pkt);
            return;
        }
        let mut sends = vec![];
        let mut events = vec![];
        if pkt.len() == 33 && pkt[0] == CHALLENGE {
            on_challenge(&mut i, pkt, from, &mut sends);
            drop(i);
            self.flush(sends, vec![]);
            return;
        }
        let from_server = pkt.len() > 25 && pkt[0] == RELAYED
            && (i.servers.iter().any(|s| s.addr == from) || i.peers.iter().any(|p| p.relays.iter().any(|r| r.addr == from)));
        let (body, path) = if from_server { (&pkt[25..], Path::Server(from)) } else { (pkt, Path::Direct(from)) };
        if body.first() == Some(&HELLO) {
            // the trial decryption happens outside the lock, and only within the budget
            let known = from_server || i.peers.iter().any(|p| p.addrs.contains(&from) || p.known.contains(&from)
                || p.sess.as_ref().is_some_and(|s| s.path == path));
            if !gate.allow(from.ip(), known) {
                return;
            }
            let (keys, me) = (i.hello_keys.clone(), i.me);
            drop(i);
            let Some((id, plain)) = open_with(&keys, &me, body) else { return };
            i = lock(&self.inner);
            let Some(k) = i.peers.iter().position(|p| p.id == id) else { return };
            take(&mut i, k, plain, body, path, &mut sends, &mut events, 0);
        } else {
            handle(&mut i, body, path, &mut sends, &mut events, 0);
        }
        drop(i);
        self.flush(sends, events);
    }

    fn flush(&self, sends: Vec<(SocketAddr, Vec<u8>)>, events: Vec<Event>) {
        for (a, b) in sends {
            self.raw_send(a, &b);
        }
        for e in events {
            let _ = self.events.send(e);
        }
    }

    fn tick(self: &Arc<Self>) {
        let mut i = lock(&self.inner);
        let mut sends = vec![];
        let mut events = vec![];
        let now = Instant::now();
        let mut bdays = vec![];
        let me_strict = strict(&i);
        for k in 0..i.peers.len() {
            // One side strict, the other not, and no direct line: try a birthday punch.
            let p = &mut i.peers[k];
            let direct = p.sess.as_ref().is_some_and(|s| matches!(s.path, Path::Direct(_)));
            let waiting = p.sess.is_some() || p.trying_since.is_some_and(|t| now - t >= TRY_RELAY_AFTER);
            let public: Vec<SocketAddr> = p.addrs.iter().filter(|a| kind(a) == "public").copied().collect();
            if !direct && waiting && p.strict != me_strict && !public.is_empty() && p.bday_at.is_none_or(|t| now - t > BDAY_EVERY) {
                p.bday_at = Some(now);
                bdays.push((p.public.clone(), public, me_strict));
            }
            // A line that went quiet is down; start looking again.
            if i.peers[k].sess.as_ref().is_some_and(|s| now - s.last_rx > DEAD) {
                let p = &mut i.peers[k];
                p.sess = None;
                p.trying_since = None;
                p.next_hello = now;
                events.push(Event::Down { peer: p.public.clone() });
            }
            let up = i.peers[k].sess.is_some();
            if !up {
                if i.peers[k].addrs.is_empty() && i.peers[k].known.is_empty() && relays(&i, k).is_empty() && !i.beacon
                    && servers_for(&i, k).is_empty() {
                    fail_all(&mut i.peers[k], now, &mut events, false);
                    continue;
                }
                if now >= i.peers[k].next_hello {
                    let since = *i.peers[k].trying_since.get_or_insert(now);
                    let tried = now - since;
                    let servers = tried >= TRY_SERVER_AFTER || tried >= TRY_RELAY_AFTER && relays(&i, k).is_empty();
                    hello_everywhere(&mut i, k, tried >= TRY_RELAY_AFTER, servers, &mut sends);
                    // Fast while both sides are likely punching at once, then back off.
                    let gap = if tried < Duration::from_secs(15) { Duration::from_secs(1) }
                        else if tried < Duration::from_secs(60) { Duration::from_secs(5) }
                        else if tried < STANDING_FOR { STANDING_GAP } else { Duration::from_secs(300) };
                    i.peers[k].next_hello = now + gap;
                }
                fail_all(&mut i.peers[k], now, &mut events, true);
                continue;
            }
            // Up: resend what wasn't acked, keep the NAT mapping alive, look for a better path.
            let mut frames = vec![];
            let p = &mut i.peers[k];
            let mut gone = vec![];
            let mut flying: HashMap<u64, usize> = HashMap::new();
            for o in p.out.iter_mut() {
                let n = flying.entry(o.id).or_default();
                if *n >= WINDOW { continue }
                *n += 1;
                if o.sent && now - o.since > GIVE_UP {
                    gone.push(o.id);
                } else if now >= o.next {
                    if !o.sent { o.sent = true; o.since = now }
                    frames.push(frag_of(o));
                    o.next = now + o.wait;
                    o.wait = (o.wait * 3 / 2).min(Duration::from_secs(2));
                }
            }
            gone.dedup();
            for id in gone {
                p.out.retain(|o| o.id != id);
                events.push(Event::Failed { peer: p.public.clone(), id });
            }
            let s = p.sess.as_ref().unwrap();
            if frames.is_empty() && now - s.last_tx > KEEPALIVE || s.rtt.is_none() && now - s.last_tx > Duration::from_secs(3) {
                let mut f = vec![ECHO];
                f.extend(now_ms().to_be_bytes());
                frames.push(f);
            }
            // Re-punch after a move until the line hears from them again on its path.
            if p.punch.is_some_and(|t| (matches!(s.path, Path::Direct(_)) && s.path_rx > t) || now - t > PUNCH_FOR) {
                p.punch = None;
            }
            let punching = p.punch.is_some() && now >= p.next_hello;
            if punching {
                p.next_hello = now + Duration::from_secs(1);
            }
            let better = punching || rank(&s.path) > 0 && now - p.last_upgrade > Duration::from_secs(30);
            for f in frames {
                seal_to(&mut i, k, &f, &mut sends);
            }
            if better {
                i.peers[k].last_upgrade = now;
                // on a relay server: a teammate who reaches both of us now is a better path
                let on_server = matches!(i.peers[k].sess.as_ref().map(|s| &s.path), Some(Path::Server(_)));
                hello_everywhere(&mut i, k, on_server, false, &mut sends);
            }
        }
        drop(i);
        self.flush(sends, events);
        for (peer, to, me_strict) in bdays {
            if me_strict { self.bday_open(&peer, to) } else { self.bday_spray(&peer, to) }
        }
        self.bday_tidy();
    }

    /// A fresh hello for `peer` (an older one would be taken as a replay).
    fn hello_for(&self, peer: &str) -> Option<Vec<u8>> {
        let i = lock(&self.inner);
        let k = i.peers.iter().position(|p| p.public == peer)?;
        (!i.blocked.contains(&i.peers[k].id)).then(|| hello(&i, k, false))
    }

    /// Strict side: many sockets, each saying hello to their public address every second, so
    /// our router keeps many holes toward them open while they guess at one.
    fn bday_open(self: &Arc<Self>, peer: &str, to: Vec<SocketAddr>) {
        let mut b = lock(&self.bday);
        let start = !std::mem::replace(&mut b.reading, true);
        while b.socks.len() < BDAY_SOCKETS {
            let Ok(s) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else { break };
            let _ = s.set_nonblocking(true);
            b.socks.push(Arc::new(s));
        }
        b.until = Some(Instant::now() + BDAY_FOR);
        drop(b);
        let (l, peer) = (self.clone(), peer.to_string());
        std::thread::spawn(move || {
            let end = Instant::now() + BDAY_FOR;
            while Instant::now() < end && !l.stop.load(Ordering::Relaxed) {
                let Some(h) = l.hello_for(&peer) else { return };
                let socks = lock(&l.bday).socks.clone();
                for s in &socks {
                    for a in &to {
                        let _ = s.send_to(&h, a);
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        if start {
            let l = self.clone();
            std::thread::spawn(move || l.bday_receive());
        }
    }

    /// Reads the birthday sockets (one thread, not one each: this runs on weak machines too). The
    /// first socket to hear from an address becomes the way to it.
    fn bday_receive(&self) {
        let mut buf = [0u8; 2048];
        let mut gate = Gate { window: Instant::now(), per_ip: HashMap::new(), strangers: 0 };
        while !self.stop.load(Ordering::Relaxed) {
            let socks = {
                let mut b = lock(&self.bday);
                if b.socks.is_empty() {
                    b.reading = false;
                    return;
                }
                b.socks.clone()
            };
            let mut got = false;
            for s in &socks {
                while let Ok((n, from)) = s.recv_from(&mut buf) {
                    got = true;
                    lock(&self.bday).route.entry(from).or_insert_with(|| s.clone());
                    self.packet(&buf[..n], from, &mut gate);
                }
            }
            if !got {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// Cone side: hellos from our one socket to random ports on their IP, for a while.
    fn bday_spray(self: &Arc<Self>, peer: &str, to: Vec<SocketAddr>) {
        let (l, peer) = (self.clone(), peer.to_string());
        std::thread::spawn(move || {
            let end = Instant::now() + BDAY_FOR;
            while Instant::now() < end && !l.stop.load(Ordering::Relaxed) {
                if l.hello_for(&peer).is_none() || l.direct_to(&peer) {
                    return;
                }
                // ten a tick, a fresh hello each tick
                let Some(h) = l.hello_for(&peer) else { return };
                for _ in 0..BDAY_RATE / 10 {
                    let r = random();
                    let port = 1024 + u16::from_be_bytes([r[0], r[1]]) % (65535 - 1024);
                    for a in &to {
                        l.raw_send(SocketAddr::new(a.ip(), port), &h);
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    fn direct_to(&self, peer: &str) -> bool {
        lock(&self.inner).peers.iter().any(|p| p.public == peer && p.sess.as_ref().is_some_and(|s| matches!(s.path, Path::Direct(_))))
    }

    /// After a punch: keep only the sockets that lines use, and close the rest.
    fn bday_tidy(&self) {
        let mut b = lock(&self.bday);
        if b.until.is_none_or(|t| Instant::now() < t) {
            return;
        }
        let used: Vec<SocketAddr> = lock(&self.inner).peers.iter()
            .filter_map(|p| match p.sess.as_ref()?.path { Path::Direct(a) => Some(a), _ => None }).collect();
        b.route.retain(|a, _| used.contains(a));
        let keep: Vec<Arc<UdpSocket>> = b.route.values().cloned().collect();
        b.socks.retain(|s| keep.iter().any(|k| Arc::ptr_eq(k, s)));
        if b.route.is_empty() {
            b.until = None;
        }
    }

    /// Find our own addresses (LAN and IPv6 from the routing table, public from STUN, a port
    /// mapping from UPnP) and keep them current: again every minute, and at once when the local
    /// address changes (the laptop moved networks). Teammates are told whenever the set changes.
    fn discover(&self, stun: bool, upnp: bool) {
        // QH_STUN="host:port,host:port" replaces the default servers (own STUN, blocked networks, tests).
        let custom: Vec<String> = std::env::var("QH_STUN").unwrap_or_default()
            .split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        let hosts: Vec<String> = if custom.is_empty() { STUN_SERVERS.iter().map(|s| s.to_string()).collect() } else { custom };
        let port = self.port();
        let v6_port = self.v6.as_ref().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port());
        let mut net = None;
        let mut mapped: Option<SocketAddr> = None;
        let (mut next_full, mut upnp_at) = (Instant::now(), None::<Instant>);
        while !self.stop.load(Ordering::Relaxed) {
            let here = (local_ip(false), local_ip(true));
            let moved = net.is_some_and(|n| n != here);
            let asked = std::mem::take(&mut lock(&self.inner).rediscover);
            if net.is_none() || moved || asked || Instant::now() >= next_full {
                let relays = lock(&self.inner).relay_specs.clone();
                let servers: Vec<Server> = relays.iter().filter_map(|spec| {
                    let (host, key) = spec.split_once('#').map_or((spec.as_str(), None), |(h, k)| (h, Some(k)));
                    let (host, tcp) = host.split_once('/').map_or((host, String::new()), |(h, t)| (h, format!("/{t}")));
                    let host = if host.contains(':') { host.to_string() } else { format!("{host}:{RELAY_PORT}") };
                    let addr = host.to_socket_addrs().ok()?.find(|a| a.is_ipv4())?;
                    parse_server(&match key { Some(k) => format!("{addr}{tcp}#{k}"), None => format!("{addr}{tcp}") })
                }).collect();
                let servers_moved = {
                    let mut i = lock(&self.inner);
                    let changed = i.servers != servers;
                    i.servers = servers;
                    changed
                };
                let mut found: Vec<SocketAddr> = here.0.map(|ip| SocketAddr::new(ip, port)).into_iter()
                    .chain(here.1.filter(|_| self.v6.is_some()).map(|ip| SocketAddr::new(ip, v6_port))).collect();
                if net.is_none() {
                    self.set_mine(found.clone(), false, false); // the LAN address right away; STUN takes a moment
                }
                if stun {
                    found.extend(self.stun_round(&hosts));
                }
                if upnp && (moved || upnp_at.is_none_or(|t| t.elapsed() > UPNP_RENEW)) {
                    mapped = here.0.and_then(|ip| upnp_map(ip, port)).map(|(a, m)| {
                        *lock(&self.mapping) = Some(m);
                        a
                    });
                    upnp_at = Some(Instant::now());
                }
                found.extend(mapped);
                self.set_mine(found, moved, servers_moved);
                net = Some(here);
                next_full = Instant::now() + REDISCOVER;
            }
            let t = Instant::now();
            while t.elapsed() < NET_CHECK && !self.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    /// Ask every STUN server where it sees us from; waits up to 1.5 s for the answers.
    fn stun_round(&self, hosts: &[String]) -> Vec<SocketAddr> {
        let mut asked = 0;
        lock(&self.inner).stun_seen.clear();
        for host in hosts {
            let Some(to) = host.to_socket_addrs().ok().and_then(|mut a| a.find(|a| a.is_ipv4())) else { continue };
            let tx: [u8; 12] = random()[..12].try_into().unwrap();
            let mut req = vec![0, 1, 0, 0];
            req.extend(STUN_MAGIC.to_be_bytes());
            req.extend(tx);
            lock(&self.inner).stun_tx.insert(tx, Instant::now());
            self.raw_send(to, &req);
            asked += 1;
        }
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1500) && lock(&self.inner).stun_seen.len() < asked {
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut i = lock(&self.inner);
        i.stun_tx.retain(|_, at| at.elapsed() < Duration::from_secs(5));
        i.stun_seen.clone()
    }

    /// Replace our published addresses. On a change, connected teammates are told; after a move
    /// (`moved`) we also punch again, since their routers only let in packets from where we were.
    fn set_mine(&self, mut found: Vec<SocketAddr>, moved: bool, servers_moved: bool) {
        found.dedup();
        let mut i = lock(&self.inner);
        if found.is_empty() || (i.mine == found && !moved && !servers_moved) {
            return;
        }
        if i.mine != found || servers_moved {
            i.mine = found;
            i.mine_ms = now_ms();
        }
        let msg = addrs_msg(&i);
        let mut sends = vec![];
        let now = Instant::now();
        for k in 0..i.peers.len() {
            if i.peers[k].sess.is_some() {
                seal_to(&mut i, k, &msg, &mut sends);
                if moved {
                    i.peers[k].punch = Some(now);
                    i.peers[k].next_hello = now;
                }
            }
        }
        drop(i);
        self.flush(sends, vec![]);
    }
}

/// "My addresses": a stamp (our clock, ms) and the list, e.g. "ms=1760000000000 1.2.3.4:5 [::1]:6".
/// Older builds ignore the stamp token (it doesn't parse as an address).
fn addrs_msg(i: &Inner) -> Vec<u8> {
    let mut msg = vec![ADDRS];
    msg.extend(format!("ms={}", i.mine_ms).bytes());
    for a in &i.mine {
        msg.extend(format!(" {a}").bytes());
    }
    for r in &i.servers {
        msg.extend(format!(" {}", server_str(r)).bytes());
    }
    if strict(i) {
        msg.extend(b" nat=strict");
    }
    msg
}

fn strict(i: &Inner) -> bool {
    i.stun_seen.windows(2).any(|w| w[0].ip() == w[1].ip() && w[0].port() != w[1].port())
}

fn describe(i: &Inner, p: &Path) -> String {
    match p {
        Path::Direct(a) => format!("{} {a}", kind(a)),
        Path::Via(r) => format!("via {}", i.peers.iter().find(|p| p.id == *r).map_or("?", |p| p.public.as_str())),
        Path::Server(a) if i.over_tcp.contains(a) => format!("relay {} over tcp", relay_name(a)),
        Path::Server(a) => format!("relay {}", relay_name(a)),
    }
}

/// "ovh-ca" for qh's default relay, else its address.
fn relay_name(a: &SocketAddr) -> String {
    if DEFAULT_RELAY.is_some_and(|d| d.parse() == Ok(*a)) { "ovh-ca (qh's default)".into() } else { a.to_string() }
}

/// Teammates with a direct line that could pass packets on to peer `k`.
fn relays(i: &Inner, k: usize) -> Vec<Id> {
    i.peers.iter().enumerate()
        .filter(|(j, p)| *j != k && p.sess.as_ref().is_some_and(|s| matches!(s.path, Path::Direct(_))))
        .map(|(_, p)| p.id).collect()
}

/// Messages that have waited too long with no line at all fail, so the caller can use git.
fn fail_all(p: &mut Peer, now: Instant, events: &mut Vec<Event>, only_old: bool) {
    let mut gone: Vec<u64> = p.out.iter().filter(|o| !only_old || now - o.since > GIVE_UP).map(|o| o.id).collect();
    gone.dedup();
    for id in gone {
        p.out.retain(|o| o.id != id);
        events.push(Event::Failed { peer: p.public.clone(), id });
    }
}

fn hello(i: &Inner, k: usize, reply: bool) -> Vec<u8> {
    let p = &i.peers[k];
    // No sender id on the wire (it would name us to everyone on the network); the receiver finds
    // out who it's from by which teammate's key opens it.
    let mut out = vec![HELLO];
    out.extend(i.eph_pub);
    let mut body = now_ms().to_be_bytes().to_vec();
    body.extend(p.id);
    body.push(reply as u8);
    let n: [u8; 12] = random()[..12].try_into().unwrap();
    let aad = [&[HELLO][..], &i.eph_pub].concat();
    p.hello_key.seal_in_place_append_tag(Nonce::assume_unique_for_key(n), Aad::from(&aad), &mut body).expect("seal");
    out.extend(n);
    out.extend(body);
    out
}

/// Say hello on every published address (both sides doing this at once is the hole punch), and,
/// when asked, through every teammate who could relay and through the relay servers.
fn hello_everywhere(i: &mut Inner, k: usize, via_relays: bool, via_servers: bool, sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    let h = hello(i, k, false);
    if !i.blocked.contains(&i.peers[k].id) {
        let p = &i.peers[k];
        let mut to = p.addrs.clone();
        to.extend(p.known.iter().filter(|a| !p.addrs.contains(a)));
        for a in to {
            sends.push((a, h.clone()));
        }
        if i.beacon && i.peers[k].sess.is_none() {
            sends.push((SocketAddr::new(IpAddr::V4(BEACON_GROUP), BEACON_PORT), h.clone()));
        }
    }
    if via_servers {
        for srv in servers_for(i, k) {
            via_server(i, k, &srv, &h, sends);
        }
    }
    if via_relays {
        let target = i.peers[k].id;
        for r in relays(i, k) {
            let mut f = vec![FWD];
            f.extend(target);
            f.extend(&h);
            let j = i.peers.iter().position(|p| p.id == r).unwrap();
            seal_to(i, j, &f, sends);
        }
    }
}

/// Seal a frame for peer `k` and send it down its current path (through the relay if need be).
fn seal_to(i: &mut Inner, k: usize, frame: &[u8], sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    seal_on(i, k, None, frame, sends)
}

/// Seal a frame for peer `k` and send it on `on` (their line's path when None).
fn seal_on(i: &mut Inner, k: usize, on: Option<Path>, frame: &[u8], sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    let Some(s) = i.peers[k].sess.as_mut() else { return };
    s.counter += 1;
    s.last_tx = Instant::now();
    let mut buf = frame.to_vec();
    let mut aad = vec![DATA];
    aad.extend(s.tx_tag);
    aad.extend(s.counter.to_be_bytes());
    s.send.seal_in_place_append_tag(nonce(s.counter), Aad::from(&aad), &mut buf).expect("seal");
    let mut pkt = aad;
    pkt.extend(buf);
    match on.unwrap_or(s.path) {
        Path::Direct(a) => sends.push((a, pkt)),
        Path::Via(r) => {
            let mut f = vec![FWD];
            f.extend(i.peers[k].id);
            f.extend(pkt);
            if let Some(j) = i.peers.iter().position(|p| p.id == r && p.sess.as_ref().is_some_and(|s| matches!(s.path, Path::Direct(_)))) {
                seal_to(i, j, &f, sends);
            }
        }
        Path::Server(a) => {
            if let Some(srv) = server_at(i, k, a) {
                via_server(i, k, &srv, &pkt, sends);
            }
        }
    }
}

/// One packet from the wire (or passed on by a relay: `depth` 1, never further).
fn handle(i: &mut Inner, pkt: &[u8], from: Path, sends: &mut Vec<(SocketAddr, Vec<u8>)>, events: &mut Vec<Event>, depth: u8) {
    if pkt.is_empty() || depth > 1 {
        return;
    }
    let k = match pkt[0] {
        HELLO if pkt.len() >= 1 + 32 + 12 + 17 + 16 => open_hello(i, pkt),
        DATA if pkt.len() >= 17 + 16 => {
            let tag: Id = pkt[1..9].try_into().unwrap();
            i.peers.iter().position(|p| p.sess.as_ref().is_some_and(|s| s.rx_tag == tag)).map(|k| (k, [0; 17]))
        }
        _ => None,
    };
    let Some((k, plain)) = k else { return };
    take(i, k, plain, pkt, from, sends, events, depth);
}

/// A packet known to be from teammate `k` (a hello that opened, or data under their session tag).
#[allow(clippy::too_many_arguments)]
fn take(i: &mut Inner, k: usize, plain: [u8; 17], pkt: &[u8], from: Path, sends: &mut Vec<(SocketAddr, Vec<u8>)>, events: &mut Vec<Event>, depth: u8) {
    if matches!(from, Path::Direct(_)) && i.blocked.contains(&i.peers[k].id) {
        return;
    }
    if pkt[0] == HELLO {
        on_hello(i, k, pkt, &plain, from, sends, events);
    } else {
        on_data(i, k, pkt, from, sends, events, depth);
    }
}

/// The hello keys tried on incoming hellos, after teammates came or went.
fn rekey(i: &mut Inner) {
    i.hello_keys = Arc::new(i.peers.iter().map(|p| (p.id, hkdf(&p.static_dh, b"hello"))).collect());
}

fn open_hello(i: &Inner, pkt: &[u8]) -> Option<(usize, [u8; 17])> {
    let (id, plain) = open_with(&i.hello_keys, &i.me, pkt)?;
    Some((i.peers.iter().position(|p| p.id == id)?, plain))
}

/// Which teammate a hello is from: the one whose pair key opens it. It must also be addressed to
/// us; our own hellos coming back (the LAN beacon loops) open with the same pair key but name them.
fn open_with(keys: &[(Id, [u8; 32])], me: &Id, pkt: &[u8]) -> Option<(Id, [u8; 17])> {
    if pkt.len() < 1 + 32 + 12 + 17 + 16 || pkt[0] != HELLO {
        return None;
    }
    let n: [u8; 12] = pkt[33..45].try_into().unwrap();
    let aad = &pkt[..33];
    keys.iter().find_map(|(id, key)| {
        let mut body = pkt[45..].to_vec();
        let plain = aead(key).open_in_place(Nonce::assume_unique_for_key(n), Aad::from(aad), &mut body).ok()?;
        (plain.len() == 17 && plain[8..16] == *me).then(|| (*id, plain.try_into().unwrap()))
    })
}

fn on_hello(i: &mut Inner, k: usize, pkt: &[u8], plain: &[u8; 17], from: Path, sends: &mut Vec<(SocketAddr, Vec<u8>)>, events: &mut Vec<Event>) {
    let eph: [u8; 32] = pkt[1..33].try_into().unwrap();
    let ts = u64::from_be_bytes(plain[..8].try_into().unwrap());
    let reply = plain[16] == 1;
    let now = Instant::now();
    let p = &mut i.peers[k];
    let fresh_eph = p.sess.as_ref().is_none_or(|s| s.their_eph != eph);
    // Ten minutes of clock difference allowed; an old hello can't roll a newer line back, and one
    // seen before (a replay) does nothing at all.
    if now_ms().abs_diff(ts) > 600_000 || ts < p.hello_ts || (!fresh_eph && ts == p.hello_ts) {
        return;
    }
    p.hello_ts = p.hello_ts.max(ts);
    if fresh_eph {
        let ee = *i.eph.diffie_hellman(&PublicKey::from(eph)).as_bytes();
        let ikm = [&i.peers[k].static_dh[..], &ee].concat();
        let them = i.peers[k].id;
        let dir = |a: &Id, b: &Id, ea: &[u8; 32], eb: &[u8; 32]| [&a[..], b, ea, eb].concat();
        let (out, inn) = (dir(&i.me, &them, &i.eph_pub, &eph), dir(&them, &i.me, &eph, &i.eph_pub));
        let send = aead(&hkdf(&ikm, &out));
        let recv = aead(&hkdf(&ikm, &inn));
        let tag = |d: &[u8]| -> Id { hkdf(&ikm, &[b"tag", d].concat())[..8].try_into().unwrap() };
        let (tx_tag, rx_tag) = (tag(&out), tag(&inn));
        let p = &mut i.peers[k];
        if p.sess.is_some() {
            p.done.clear(); // they restarted: their message ids start over
            p.parts.clear();
        }
        p.sess = Some(Session {
            their_eph: eph, send, recv, tx_tag, rx_tag, counter: 0, rx_top: 0, rx_seen: 0, path: from, path_rx: now,
            last_rx: now, last_tx: now, announced: false, heard_addrs: false, probe: None, rtt: None,
        });
    }
    let p = &mut i.peers[k];
    let s = p.sess.as_mut().unwrap();
    s.last_rx = now;
    let check = if fresh_eph { None } else { note_path(s, from, now) };
    p.trying_since = None;
    check_path(i, k, from, check, sends);
    if !reply {
        let h = hello(i, k, true);
        send_on(i, k, from, h, sends);
    }
    // A new session says where we are (after the hello, so they can open it). If it gets lost,
    // their own ADDRS makes us say it again.
    if fresh_eph && !i.mine.is_empty() {
        let msg = addrs_msg(i);
        seal_to(i, k, &msg, sends);
    }
    announce(i, k, events);
}

/// Send an outer packet for peer `k` straight down `path` (hellos answer the way they came).
fn send_on(i: &mut Inner, k: usize, path: Path, pkt: Vec<u8>, sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    match path {
        Path::Direct(a) => sends.push((a, pkt)),
        Path::Via(r) => {
            let Some(j) = i.peers.iter().position(|p| p.id == r) else { return };
            let mut f = vec![FWD];
            f.extend(i.peers[k].id);
            f.extend(pkt);
            seal_to(i, j, &f, sends);
        }
        Path::Server(a) => {
            if let Some(srv) = server_at(i, k, a) {
                via_server(i, k, &srv, &pkt, sends);
            }
        }
    }
}

/// A packet arrived on `from`. Move to it when it is a better kind of path, the same kind (they
/// send from one place, so a new one means they moved or their router remapped them), or when the
/// current one has gone quiet for longer than a keepalive; but only once a check sent there comes
/// back (someone who saw a packet on the way and sent it on from elsewhere can't move the line).
/// Returns the check to send on `from`.
fn note_path(s: &mut Session, from: Path, now: Instant) -> Option<[u8; 8]> {
    if from == s.path {
        s.path_rx = now;
        return None;
    }
    if !(rank(&from) <= rank(&s.path) || now - s.path_rx > KEEPALIVE + Duration::from_secs(5)) {
        return None;
    }
    if s.probe.is_some_and(|(p, _, t)| p == from && now - t < Duration::from_secs(1)) {
        return None; // checking already
    }
    let nonce: [u8; 8] = random()[..8].try_into().unwrap();
    s.probe = Some((from, nonce, now));
    Some(nonce)
}

/// Send the check `note_path` asked for.
fn check_path(i: &mut Inner, k: usize, from: Path, nonce: Option<[u8; 8]>, sends: &mut Vec<(SocketAddr, Vec<u8>)>) {
    if let Some(n) = nonce {
        let mut f = vec![CHECK];
        f.extend(n);
        seal_on(i, k, Some(from), &f, sends);
    }
}

fn announce(i: &mut Inner, k: usize, events: &mut Vec<Event>) {
    let d = i.peers[k].sess.as_ref().map(|s| describe(i, &s.path));
    let p = &mut i.peers[k];
    if let (Some(s), Some(d)) = (p.sess.as_mut(), d) {
        if !s.announced {
            s.announced = true;
            events.push(Event::Up { peer: p.public.clone(), path: d });
            // Remember them (and, for a direct line, where): after a restart they're tried at once,
            // directly or through whichever teammate relays.
            if let Path::Direct(a) = s.path {
                p.known.retain(|x| *x != a);
                p.known.insert(0, a);
                p.known.truncate(REMEMBER);
            }
            let (key, known) = (p.public.clone(), p.known.clone());
            if i.memory.get(&key) != Some(&known) {
                i.memory.insert(key, known);
                i.memory_dirty = true;
            }
        }
    }
}

fn on_data(i: &mut Inner, k: usize, pkt: &[u8], from: Path, sends: &mut Vec<(SocketAddr, Vec<u8>)>, events: &mut Vec<Event>, depth: u8) {
    let counter = u64::from_be_bytes(pkt[9..17].try_into().unwrap());
    let now = Instant::now();
    let p = &mut i.peers[k];
    let opened = p.sess.as_mut().and_then(|s| {
        let mut buf = pkt[17..].to_vec();
        let n = s.recv.open_in_place(nonce(counter), Aad::from(&pkt[..17]), &mut buf).ok()?.len();
        buf.truncate(n);
        Some(s.fresh(counter).then_some(buf))
    });
    let f = match opened {
        Some(Some(f)) => f,
        Some(None) => return, // a replay
        None => {
            // Can't open it: they probably restarted with a new key. Say hello again (not too often).
            if now >= p.resync_at {
                p.resync_at = now + Duration::from_secs(2);
                let h = hello(i, k, false);
                send_on(i, k, from, h, sends);
            }
            return;
        }
    };
    let s = p.sess.as_mut().unwrap();
    s.last_rx = now;
    if f.first() == Some(&CHECKED) {
        // our check came back on the path it went out on: that's where they are
        if f.len() == 9 && s.probe.is_some_and(|(p, n, _)| p == from && n[..] == f[1..9]) {
            s.probe = None;
            s.path = from;
            s.path_rx = now;
            s.rtt = None; // measured again on the new path
            s.announced = false;
            announce(i, k, events);
        }
        return;
    }
    let check = note_path(s, from, now);
    check_path(i, k, from, check, sends);
    announce(i, k, events);
    let Some(&kind) = f.first() else { return };
    let body = &f[1..];
    match kind {
        ECHO if body.len() == 8 => {
            let mut back = vec![ECHOED];
            back.extend(body);
            seal_on(i, k, Some(from), &back, sends);
        }
        ECHOED if body.len() == 8 => {
            let sent = u64::from_be_bytes(body.try_into().unwrap());
            let took = Duration::from_millis(now_ms().saturating_sub(sent).min(60_000));
            if let Some(s) = i.peers[k].sess.as_mut().filter(|s| s.path == from) {
                s.rtt = Some(s.rtt.map_or(took, |r| (r * 7 + took) / 8));
            }
        }
        CHECK if body.len() == 8 => {
            let mut back = vec![CHECKED];
            back.extend(body);
            seal_on(i, k, Some(from), &back, sends);
        }
        FRAG if body.len() >= 12 => {
            let id = u64::from_be_bytes(body[..8].try_into().unwrap());
            let part = u16::from_be_bytes(body[8..10].try_into().unwrap());
            let count = u16::from_be_bytes(body[10..12].try_into().unwrap());
            let p = &i.peers[k];
            let held: usize = p.parts.values().map(|e| e.2).sum();
            if !p.parts.contains_key(&id) && !p.done.contains(&id) && (p.parts.len() >= MAX_PARTIAL || held >= MAX_PARTIAL_BYTES) {
                return; // too much half-received from them: no ack, so it comes again later (or by git)
            }
            let mut ack = vec![ACK];
            ack.extend(&body[..10]);
            seal_to(i, k, &ack, sends);
            let p = &mut i.peers[k];
            if count == 0 || part >= count || count as usize * PART > MAX_MSG + PART || p.done.contains(&id) {
                return;
            }
            let entry = p.parts.entry(id).or_insert_with(|| (vec![None; count as usize], now, 0));
            if entry.0.len() != count as usize {
                return;
            }
            entry.1 = now;
            if entry.0[part as usize].is_none() {
                entry.2 += body.len() - 12;
                entry.0[part as usize] = Some(body[12..].to_vec());
            }
            if entry.0.iter().all(Option::is_some) {
                let (parts, _, _) = p.parts.remove(&id).unwrap();
                p.done.push_back(id);
                if p.done.len() > 512 {
                    p.done.pop_front();
                }
                events.push(Event::Msg { from: p.public.clone(), data: parts.into_iter().flatten().flatten().collect() });
            }
            p.parts.retain(|_, (_, t, _)| now - *t < Duration::from_secs(60));
        }
        ACK if body.len() >= 10 => {
            let id = u64::from_be_bytes(body[..8].try_into().unwrap());
            let part = u16::from_be_bytes(body[8..10].try_into().unwrap());
            let p = &mut i.peers[k];
            let had = p.out.iter().any(|o| o.id == id);
            let before = p.out.len();
            p.out.retain(|o| !(o.id == id && o.part == part));
            if had && !p.out.iter().any(|o| o.id == id) {
                events.push(Event::Delivered { peer: p.public.clone(), id });
            } else if p.out.len() < before {
                // progress: the message isn't stuck, and the next part waiting outside the window goes now
                let mut n = 0;
                let mut next = None;
                for o in p.out.iter_mut().filter(|o| o.id == id) {
                    if o.sent { o.since = now } else if next.is_none() && n < WINDOW { o.sent = true; o.since = now; o.next = now + o.wait; next = Some(frag_of(o)) }
                    n += 1;
                    if n >= WINDOW { break }
                }
                if let Some(f) = next { seal_to(i, k, &f, sends) }
            }
        }
        FWD if body.len() > 8 && depth == 0 => {
            // A teammate asks us to pass a packet on. Only to someone we reach directly, only
            // within a budget, and we can't read it (it's sealed for them).
            let to: Id = body[..8].try_into().unwrap();
            let src = i.peers[k].id;
            let ok = i.relay && matches!(from, Path::Direct(_));
            let Some(j) = i.peers.iter().position(|p| p.id == to && p.sess.as_ref().is_some_and(|s| matches!(s.path, Path::Direct(_)))) else { return };
            let r = &mut i.peers[k].relayed;
            if now - r.0 > Duration::from_secs(60) {
                *r = (now, 0);
            }
            r.1 += body.len();
            if !ok || r.1 > RELAY_BYTES_PER_MIN {
                return;
            }
            let mut f = vec![FWDED];
            f.extend(src);
            f.extend(&body[8..]);
            seal_to(i, j, &f, sends);
        }
        FWDED if body.len() > 8 && depth == 0 => {
            let relay = i.peers[k].id;
            handle(i, &body[8..], Path::Via(relay), sends, events, 1);
        }
        ADDRS => {
            let text = String::from_utf8_lossy(body);
            let ms = text.split(' ').find_map(|t| t.strip_prefix("ms=")?.parse::<u64>().ok()).unwrap_or(0);
            let addrs: Vec<SocketAddr> = text.split(' ').filter_map(|a| a.parse().ok()).collect();
            let relays: Vec<Server> = text.split(' ').filter(|t| t.starts_with("relay=")).filter_map(parse_server).collect();
            let p = &mut i.peers[k];
            if !relays.is_empty() {
                p.relays = relays;
            }
            let first = p.sess.as_mut().is_some_and(|s| !std::mem::replace(&mut s.heard_addrs, true));
            if !addrs.is_empty() && (ms == 0 || ms >= p.addrs_ms) {
                p.addrs_ms = p.addrs_ms.max(ms);
                p.strict = text.split(' ').any(|t| t == "nat=strict");
                if p.addrs != addrs {
                    p.addrs = addrs;
                    p.punch = Some(Instant::now());
                    p.next_hello = Instant::now();
                }
            }
            if first && !i.mine.is_empty() {
                let msg = addrs_msg(i);
                seal_to(i, k, &msg, sends);
            }
        }
        _ => {} // PING: being received was the point
    }
}

fn stun_reply(i: &mut Inner, pkt: &[u8]) {
    let tx: [u8; 12] = pkt[8..20].try_into().unwrap();
    if u32::from_be_bytes(pkt[4..8].try_into().unwrap()) != STUN_MAGIC || i.stun_tx.remove(&tx).is_none() {
        return;
    }
    let mut at = 20;
    while at + 4 <= pkt.len() {
        let t = u16::from_be_bytes([pkt[at], pkt[at + 1]]);
        let len = u16::from_be_bytes([pkt[at + 2], pkt[at + 3]]) as usize;
        let v = &pkt[(at + 4).min(pkt.len())..(at + 4 + len).min(pkt.len())];
        if t == 0x0020 && v.len() >= 8 && v[1] == 1 {
            let port = u16::from_be_bytes([v[2], v[3]]) ^ (STUN_MAGIC >> 16) as u16;
            let ip = u32::from_be_bytes(v[4..8].try_into().unwrap()) ^ STUN_MAGIC;
            let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port);
            i.stun_seen.push(a);
            return;
        }
        at += 4 + len.div_ceil(4) * 4;
    }
}

/// The address the system would use to reach the internet (no packet is sent).
fn local_ip(v6: bool) -> Option<IpAddr> {
    let (bind, to): (SocketAddr, SocketAddr) = if v6 {
        ("[::]:0".parse().ok()?, "[2001:4860:4860::8888]:53".parse().ok()?)
    } else {
        ("0.0.0.0:0".parse().ok()?, "1.1.1.1:53".parse().ok()?)
    };
    let s = UdpSocket::bind(bind).ok()?;
    s.connect(to).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

fn upnp_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(3)).build()
}

fn soap(agent: &ureq::Agent, url: &str, service: &str, action: &str, args: &str) -> Option<String> {
    let body = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">{args}</u:{action}></s:Body></s:Envelope>");
    agent.post(url).set("Content-Type", "text/xml; charset=\"utf-8\"").set("SOAPAction", &format!("\"{service}#{action}\""))
        .send_string(&body).ok()?.into_string().ok()
}

/// Ask the home router (UPnP IGD) to forward our UDP port; returns the outside address.
fn upnp_map(lan: IpAddr, port: u16) -> Option<(SocketAddr, Upnp)> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let search = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
                  ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";
    s.send_to(search.as_bytes(), "239.255.255.250:1900").ok()?;
    let mut buf = [0u8; 2048];
    let (n, _) = s.recv_from(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let location = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("location").then(|| v.trim().to_string())
    })?;
    let agent = upnp_agent();
    let desc = agent.get(&location).call().ok()?.into_string().ok()?;
    let (service, control) = ["WANIPConnection:2", "WANIPConnection:1", "WANPPPConnection:1"].iter().find_map(|svc| {
        let at = desc.find(&format!("urn:schemas-upnp-org:service:{svc}"))?;
        let rest = &desc[at..];
        let c = rest.split("<controlURL>").nth(1)?.split("</controlURL>").next()?.trim().to_string();
        Some((format!("urn:schemas-upnp-org:service:{svc}"), c))
    })?;
    let base = location.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    let url = if control.starts_with("http") { control } else { format!("{base}/{}", control.trim_start_matches('/')) };
    soap(&agent, &url, &service, "AddPortMapping", &format!(
        "<NewRemoteHost></NewRemoteHost><NewExternalPort>{port}</NewExternalPort><NewProtocol>UDP</NewProtocol>\
         <NewInternalPort>{port}</NewInternalPort><NewInternalClient>{lan}</NewInternalClient><NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>qh-link</NewPortMappingDescription><NewLeaseDuration>3600</NewLeaseDuration>"))?;
    let r = soap(&agent, &url, &service, "GetExternalIPAddress", "")?;
    let ip: IpAddr = r.split("<NewExternalIPAddress>").nth(1)?.split('<').next()?.trim().parse().ok()?;
    Some((SocketAddr::new(ip, port), Upnp { url, service, port }))
}

/// The relay setting ("default", "none", or "host:port#secret,...") as server specs.
pub fn relay_specs(setting: &str) -> Vec<String> {
    match setting.trim() {
        "" | "default" => DEFAULT_RELAY.map(String::from).into_iter().collect(),
        "none" | "off" => vec![],
        list => list.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
    }
}

/// `qh link`: try the line by hand. Prints a one-line card for the other side; paste theirs.
/// Lines typed after that go to every connected teammate.
pub fn cli(args: &[String]) -> i32 {
    let port = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)?.parse().ok()).unwrap_or(0);
    let local = args.iter().any(|a| a == "--local");
    let secret = match args.iter().position(|a| a == "--key").and_then(|i| args.get(i + 1)) {
        Some(k) => match <[u8; 32]>::try_from(b64url_decode(k)) { Ok(b) => StaticSecret::from(b), Err(_) => return 2 },
        None => crate::teamcrypto::identity().0,
    };
    let o = Options {
        port, stun: !local, upnp: !local && !args.iter().any(|a| a == "--no-upnp"), relay: true,
        beacon: !args.iter().any(|a| a == "--no-beacon"),
        remember: args.iter().position(|a| a == "--remember").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from),
        relays: args.iter().position(|a| a == "--relay").and_then(|i| args.get(i + 1)).cloned()
            .or_else(|| std::env::var("QH_RELAY").ok()).map(|r| relay_specs(&r)).unwrap_or_default(),
    };
    let (link, rx) = match Link::start(secret, o) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("can't open a UDP port: {e}");
            return 1;
        }
    };
    std::thread::spawn(move || {
        for e in rx {
            match e {
                Event::Msg { from, data } => println!("[{}] {}", &from[..6], String::from_utf8_lossy(&data)),
                Event::Up { peer, path } => println!("up {} ({path})", &peer[..6]),
                Event::Down { peer } => println!("down {}", &peer[..6]),
                Event::Delivered { peer, id } => println!("delivered #{} to {}", id % 1000, &peer[..6]),
                Event::Failed { peer, id } => println!("FAILED #{} to {} (would go by git)", id % 1000, &peer[..6]),
                Event::LanBlocked => println!("macOS blocks our LAN sends: grant Local Network access"),
            }
        }
    });
    // Teammates met before: try where they were at once, before STUN has even answered.
    for k in link.remembered() {
        link.peer(&k, &[], 0);
        println!("looking for {} where we last met", &k[..6.min(k.len())]);
    }
    std::thread::sleep(Duration::from_millis(if local { 50 } else { 2500 }));
    let card = |link: &Link| {
        let addrs = if local { vec![format!("lan=127.0.0.1:{}", link.port())] } else { link.addrs() };
        println!("card: qhlink:{}@{}", link.me(), addrs.join(","));
    };
    card(&link);
    if link.strict_nat() {
        println!("note: this network changes ports per destination; expect a relay");
    }
    println!("paste a teammate's card (or type a message once connected); /card, /status, /quit");
    let mut line = String::new();
    while std::io::stdin().read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
        let t = line.trim();
        if let Some(card) = t.strip_prefix("qhlink:") {
            if let Some((key, addrs)) = card.split_once('@') {
                link.peer(key, &addrs.split(',').map(str::to_string).collect::<Vec<_>>(), 0);
                println!("connecting to {}", &key[..6.min(key.len())]);
            }
        } else if t == "/quit" {
            break;
        } else if t == "/card" {
            card(&link);
        } else if t == "/status" {
            for x in link.lines() {
                let rtt = x.rtt_ms.map(|ms| format!(" · {ms}ms")).unwrap_or_default();
                println!("{} {}{rtt}", &x.public[..6], x.path.or(x.why).unwrap_or_else(|| "no line".into()));
            }
        } else if !t.is_empty() {
            for (p, _) in link.status() {
                link.send(&p, t.as_bytes());
            }
        }
        line.clear();
    }
    link.stop();
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> (Arc<Link>, Receiver<Event>) {
        Link::start(StaticSecret::from(random()), Options { port: 0, stun: false, upnp: false, relay: true, beacon: false, remember: None, relays: vec![] }).unwrap()
    }

    fn card(l: &Link) -> (String, Vec<String>) {
        (l.me(), vec![format!("lan=127.0.0.1:{}", l.port())])
    }

    fn meet(a: &Link, b: &Link) {
        let (k, ad) = card(b);
        a.peer(&k, &ad, 0);
        let (k, ad) = card(a);
        b.peer(&k, &ad, 0);
    }

    /// A data packet from `from` to its first teammate, as it would send it (but handed to us).
    /// Waits for `from`'s own half of the line: callers only wait for the other side's Up, and on a
    /// slow machine `from` may not have its session yet (nothing to seal with).
    fn sealed(from: &Link, frame: &[u8]) -> Vec<u8> {
        let t0 = Instant::now();
        loop {
            let mut sends = vec![];
            seal_on(&mut lock(&from.inner), 0, Some(Path::Direct("127.0.0.1:9".parse().unwrap())), frame, &mut sends);
            if !sends.is_empty() || t0.elapsed() > Duration::from_secs(5) {
                return sends.remove(0).1;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_packet_sent_on_from_elsewhere_doesnt_move_the_line_until_a_check_comes_back() {
        let ((a, ra), (b, _rb)) = (node(), node());
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("up");
        let at_a: SocketAddr = format!("127.0.0.1:{}", a.port()).parse().unwrap();
        // someone on the way takes b's next packet and sends it on from their own address first
        let m = UdpSocket::bind("127.0.0.1:0").unwrap();
        m.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        m.send_to(&sealed(&b, &[PING]), at_a).unwrap();
        let mut buf = [0u8; 2048];
        let (n, _) = m.recv_from(&mut buf).expect("a checks the new path");
        assert_eq!(buf[0], DATA, "a sealed check, {n} bytes: they can't answer it");
        std::thread::sleep(Duration::from_millis(300));
        // (the line may still upgrade to another of b's own addresses meanwhile, e.g. its LAN one)
        let mid = m.local_addr().unwrap();
        let path = || lock(&a.inner).peers[0].sess.as_ref().unwrap().path;
        assert_ne!(path(), Path::Direct(mid), "the line stays where b really is");
        // a replayed hello does nothing at all
        let h = hello(&lock(&b.inner), 0, false);
        m.send_to(&h, at_a).unwrap();
        m.send_to(&h, at_a).unwrap();
        while m.recv_from(&mut buf).is_ok() {}
        assert_ne!(path(), Path::Direct(mid));
        // but when the check does come back on the new path (b really moved there), the line moves
        std::thread::sleep(Duration::from_secs(1)); // (one check per path a second)
        let pkt = sealed(&b, &[PING]);
        m.send_to(&pkt, at_a).unwrap();
        let (n, _) = m.recv_from(&mut buf).expect("the check");
        let at_b: SocketAddr = format!("127.0.0.1:{}", b.port()).parse().unwrap();
        let fwd = UdpSocket::bind("127.0.0.1:0").unwrap();
        fwd.send_to(&buf[..n], at_b).unwrap(); // b answers back the way the check came: to fwd
        fwd.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        while let Ok((n, _)) = fwd.recv_from(&mut buf) {
            m.send_to(&buf[..n], at_a).unwrap(); // b's answer (and b's own check of this new path)
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lock(&a.inner).peers[0].sess.as_ref().unwrap().path, Path::Direct(m.local_addr().unwrap()));
        a.stop();
        b.stop();
    }

    #[test]
    fn strangers_get_a_hello_budget_teammates_addresses_get_more() {
        let mut g = Gate { window: Instant::now(), per_ip: HashMap::new(), strangers: 0 };
        let ip = |n: u32| IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n));
        assert_eq!((0..100).filter(|_| g.allow(ip(1), false)).count(), HELLOS_PER_IP as usize, "one address");
        let spread = (2..2000).filter(|n| g.allow(ip(*n), false)).count();
        assert!(spread <= STRANGER_HELLOS as usize, "{spread} from many addresses");
        assert!(g.allow(ip(9999), true), "a teammate's known address still gets through");
    }

    #[test]
    fn a_teammate_cant_make_us_hold_endless_half_messages() {
        let ((a, ra), (b, _rb)) = (node(), node());
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("up");
        let from = lock(&a.inner).peers[0].sess.as_ref().unwrap().path;
        for id in 0..100u64 {
            let mut f = vec![FRAG];
            f.extend(id.to_be_bytes());
            f.extend(0u16.to_be_bytes());
            f.extend(290u16.to_be_bytes()); // a 260 KB message, of which only the first part ever comes
            f.extend([7u8; PART]);
            let pkt = sealed(&b, &f);
            let (mut sends, mut events) = (vec![], vec![]);
            handle(&mut lock(&a.inner), &pkt, from, &mut sends, &mut events, 0);
        }
        assert!(lock(&a.inner).peers[0].parts.len() <= MAX_PARTIAL);
        a.stop();
        b.stop();
    }

    fn wait(rx: &Receiver<Event>, secs: u64, want: impl Fn(&Event) -> bool) -> Option<Event> {
        let end = Instant::now() + Duration::from_secs(secs);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(e) if want(&e) => return Some(e),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    #[test]
    fn two_teammates_talk_directly_and_big_messages_arrive_whole() {
        let ((a, ra), (b, rb)) = (node(), node());
        meet(&a, &b);
        let up = wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("line comes up");
        assert!(matches!(up, Event::Up { ref path, .. } if path.starts_with("lan 127.0.0.1")), "{up:?}");
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let id = a.send(&b.me(), &big).unwrap();
        let got = wait(&rb, 5, |e| matches!(e, Event::Msg { .. })).expect("message arrives");
        assert_eq!(got, Event::Msg { from: a.me(), data: big });
        assert!(wait(&ra, 5, |e| *e == Event::Delivered { peer: b.me(), id }).is_some(), "sender hears it arrived");
        b.send(&a.me(), b"hi back").unwrap();
        assert!(wait(&ra, 5, |e| matches!(e, Event::Msg { data, .. } if data == b"hi back")).is_some());
        a.stop();
        b.stop();
    }

    #[test]
    fn a_big_message_streams_and_a_small_one_isnt_stuck_behind_it() {
        let ((a, ra), (b, rb)) = (node(), node());
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("line comes up");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        let id = a.send(&b.me(), &big).unwrap();
        let small = a.send(&b.me(), b"meanwhile").unwrap();
        // the small one arrives first: it has its own window
        let first = wait(&rb, 10, |e| matches!(e, Event::Msg { .. })).expect("a message arrives");
        assert_eq!(first, Event::Msg { from: a.me(), data: b"meanwhile".to_vec() });
        let got = wait(&rb, 60, |e| matches!(e, Event::Msg { .. })).expect("the big one arrives");
        assert_eq!(got, Event::Msg { from: a.me(), data: big });
        let mut done = vec![];
        while done.len() < 2 {
            match wait(&ra, 10, |e| matches!(e, Event::Delivered { .. })) {
                Some(Event::Delivered { id, .. }) => done.push(id),
                _ => panic!("not delivered: {done:?}"),
            }
        }
        done.sort();
        assert_eq!(done, vec![id.min(small), id.max(small)]);
        assert!(a.send(&b.me(), &vec![0u8; MAX_MSG + 1]).is_none(), "past the cap: the caller uses git");
        a.stop();
        b.stop();
    }

    #[test]
    fn a_line_reports_its_round_trip_and_no_line_says_why() {
        let ((a, ra), (b, _rb)) = (node(), node());
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("line comes up");
        let end = Instant::now() + Duration::from_secs(8);
        let x = loop {
            let x = a.lines().into_iter().find(|x| x.public == b.me()).unwrap();
            let settled = x.rtt_ms.is_some() && x.path.as_deref().is_some_and(|p| p.starts_with("lan "));
            if settled || Instant::now() > end { break x }
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(x.path.as_deref().is_some_and(|p| p.starts_with("lan ")), "{x:?}"); // loopback, or the machine's own LAN address
        assert!(x.rtt_ms.is_some_and(|ms| ms < 1000), "an echo came back: {x:?}");
        assert_eq!(x.why, None);
        let ((c, _), (d, _)) = (node(), node());
        let (k, _) = card(&d);
        d.stop();
        c.peer(&k, &["lan=127.0.0.1:9".into()], 0);
        let x = c.lines().into_iter().find(|x| x.public == k).unwrap();
        assert_eq!((x.path.as_deref(), x.rtt_ms), (None, None));
        assert!(x.why.is_some(), "{x:?}");
        for l in [a, b, c] { l.stop() }
    }

    /// A relay server on localhost for `secs`; returns its port.
    fn relay_server(secret: Option<[u8; 32]>, secs: u64) -> u16 {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let port = sock.local_addr().unwrap().port();
        std::thread::spawn(move || crate::relay::serve_with(&sock, None, secret, Some(Instant::now() + Duration::from_secs(secs))));
        port
    }

    /// A relay server on localhost taking UDP and TCP (on the same port number) for `secs`.
    fn relay_server_tcp(secret: Option<[u8; 32]>, secs: u64) -> u16 {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        let sock = UdpSocket::bind(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        std::thread::spawn(move || crate::relay::serve_with(&sock, Some(tcp), secret, Some(Instant::now() + Duration::from_secs(secs))));
        port
    }

    #[test]
    fn where_udp_is_blocked_the_relay_is_reached_over_tcp() {
        let key: [u8; 32] = random();
        let port = relay_server_tcp(Some(key), 30);
        let spec = format!("127.0.0.1:{port}/{port}#{}", b64url(&key));
        let ((a, ra), (b, rb)) = (with_relay(spec.clone()), with_relay(spec));
        let relay: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        lock(&a.udp_off).push(relay); // a's network drops UDP to the relay; b's doesn't
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &b);
        let up = wait(&ra, 20, |e| matches!(e, Event::Up { peer, .. } if *peer == b.me())).expect("through the relay over tcp");
        assert!(matches!(&up, Event::Up { path, .. } if path.starts_with("relay 127.0.0.1")), "{up:?}");
        assert!(lock(&a.lanes).get(&relay).is_some_and(|l| l.tcp.is_some()), "a's lane is tcp");
        assert!(a.lines()[0].path.as_deref().is_some_and(|p| p.ends_with("over tcp")), "{:?}", a.lines());
        a.send(&b.me(), &vec![7u8; 5000]).unwrap();
        assert!(wait(&rb, 10, |e| matches!(e, Event::Msg { data, .. } if data.len() == 5000)).is_some(), "a → b");
        b.send(&a.me(), b"back over udp, then tcp").unwrap();
        assert!(wait(&ra, 10, |e| matches!(e, Event::Msg { data, .. } if data == b"back over udp, then tcp")).is_some(), "b → a");
        a.stop();
        b.stop();
    }

    #[test]
    fn a_relay_spec_keeps_its_tcp_port_and_leaves_out_the_default() {
        let s = parse_server("relay=1.2.3.4:47232/8443").unwrap();
        assert_eq!((s.addr.port(), s.tcp), (47232, 8443));
        assert_eq!(server_str(&s), "relay=1.2.3.4:47232/8443");
        let d = parse_server("1.2.3.4:47232").unwrap();
        assert_eq!((d.tcp, server_str(&d)), (RELAY_TCP_PORT, "relay=1.2.3.4:47232".to_string()));
    }

    fn with_relay(spec: String) -> (Arc<Link>, Receiver<Event>) {
        let o = Options { port: 0, stun: false, upnp: false, relay: true, beacon: false, remember: None, relays: vec![spec] };
        Link::start(StaticSecret::from(random()), o).unwrap()
    }

    #[test]
    fn a_relay_server_carries_teammates_nothing_else_reaches() {
        let key: [u8; 32] = random();
        let port = relay_server(Some(key), 20);
        let spec = format!("127.0.0.1:{port}#{}", b64url(&key));
        let ((a, ra), (b, rb)) = (with_relay(spec.clone()), with_relay(spec));
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300)); // the relay setting is resolved
        meet(&a, &b);
        let up = wait(&ra, 10, |e| matches!(e, Event::Up { peer, .. } if *peer == b.me())).expect("through the relay");
        assert_eq!(up, Event::Up { peer: b.me(), path: format!("relay 127.0.0.1:{port}") });
        a.send(&b.me(), b"via the server").unwrap();
        assert!(wait(&rb, 5, |e| matches!(e, Event::Msg { data, .. } if data == b"via the server")).is_some());
        a.stop();
        b.stop();
    }

    /// Talks to the real default relay: `cargo test -- --ignored the_default_relay`.
    #[test]
    #[ignore]
    fn the_default_relay_carries_a_pair() {
        let spec = DEFAULT_RELAY.unwrap().to_string();
        let ((a, ra), (b, rb)) = (with_relay(spec.clone()), with_relay(spec.clone()));
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &b);
        let up = wait(&ra, 15, |e| matches!(e, Event::Up { peer, .. } if *peer == b.me())).expect("through the relay");
        assert_eq!(up, Event::Up { peer: b.me(), path: "relay ovh-ca (qh's default)".into() });
        a.send(&b.me(), b"via the default relay").unwrap();
        assert!(wait(&rb, 10, |e| matches!(e, Event::Msg { data, .. } if data == b"via the default relay")).is_some());
        a.stop();
        b.stop();
    }

    #[test]
    #[ignore] // live: reaches the real relay over tcp/tls 443
    fn the_default_relay_carries_a_pair_over_tcp() {
        let spec = DEFAULT_RELAY.unwrap().to_string();
        let ((a, ra), (b, rb)) = (with_relay(spec.clone()), with_relay(spec.clone()));
        let relay: SocketAddr = spec.parse().unwrap();
        lock(&a.udp_off).push(relay);
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &b);
        wait(&ra, 30, |e| matches!(e, Event::Up { peer, .. } if *peer == b.me())).expect("through the relay over tcp");
        assert!(a.lines()[0].path.as_deref().is_some_and(|p| p.ends_with("over tcp")), "{:?}", a.lines());
        a.send(&b.me(), b"over 443").unwrap();
        assert!(wait(&rb, 10, |e| matches!(e, Event::Msg { data, .. } if data == b"over 443")).is_some());
        b.send(&a.me(), b"and back").unwrap();
        assert!(wait(&ra, 10, |e| matches!(e, Event::Msg { data, .. } if data == b"and back")).is_some());
        a.stop();
        b.stop();
    }

    #[test]
    fn a_public_relay_wont_bounce_traffic_or_hand_a_pairs_place_to_an_onlooker() {
        let port = relay_server(None, 25);
        let spec = format!("127.0.0.1:{port}");
        let ((a, ra), (b, rb)) = (with_relay(spec.clone()), with_relay(spec));
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &b);
        wait(&ra, 10, |e| matches!(e, Event::Up { .. })).expect("a and b meet at the relay");
        let token = pair_ids(&lock(&a.inner).peers[0].static_dh)[0].0;
        let relay: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let m = UdpSocket::bind("127.0.0.1:0").unwrap();
        m.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let send = |body: &[u8], t: &[u8; 16]| { let mut out = vec![RELAYED]; out.extend(t); out.extend([0u8; 8]); out.extend(body); m.send_to(&out, relay).unwrap(); };
        // someone who saw the pair's token sends with it: nothing goes through, they're only asked to prove themselves
        send(&[DATA; 60], &token);
        let mut buf = [0u8; 2048];
        let (n, _) = m.recv_from(&mut buf).expect("a challenge");
        assert_eq!((n, buf[0], &buf[1..17]), (33, CHALLENGE, &token[..]));
        // a proof with their own key (it doesn't hash to the pair's token) is refused
        let kp = ring::signature::Ed25519KeyPair::from_seed_unchecked(&random()).unwrap();
        let cookie = buf[17..33].to_vec();
        let mut proof = vec![PROOF, 0];
        proof.extend(&cookie);
        { use ring::signature::KeyPair; proof.extend(kp.public_key().as_ref()); }
        proof.extend(kp.sign(&proof_bytes(&token, &cookie, 0)).as_ref());
        send(&proof, &token);
        send(&[DATA; 60], &token);
        let (n, _) = m.recv_from(&mut buf).expect("still just a challenge");
        assert_eq!((n, buf[0]), (33, CHALLENGE));
        // and the pair still talks through the relay
        a.send(&b.me(), b"still ours").unwrap();
        assert!(wait(&rb, 5, |e| matches!(e, Event::Msg { data, .. } if data == b"still ours")).is_some());
        // a flood of packets from one address gets a handful of small challenges back, nothing more
        std::thread::sleep(Duration::from_millis(1100));
        for _ in 0..200 {
            send(&[DATA; 1000], &random()[..16].try_into().unwrap());
        }
        let mut back = 0;
        while let Ok((n, _)) = m.recv_from(&mut buf) {
            assert_eq!(n, 33);
            back += 1;
        }
        assert!(back <= 8, "{back} challenges for 200 packets");
        a.stop();
        b.stop();
    }

    #[test]
    fn a_private_relay_ignores_the_wrong_secret() {
        let port = relay_server(Some(random()), 12);
        let spec = format!("127.0.0.1:{port}#{}", b64url(&random()));
        let ((a, ra), (b, _rb)) = (with_relay(spec.clone()), with_relay(spec));
        a.block(&b.me());
        b.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &b);
        assert!(wait(&ra, 7, |e| matches!(e, Event::Up { .. })).is_none(), "no line without the secret");
        a.stop();
        b.stop();
    }

    #[test]
    fn a_teammate_relays_when_two_cant_reach_each_other() {
        let ((a, ra), (b, _rb), (c, rc)) = (node(), node(), node());
        a.block(&c.me());
        c.block(&a.me());
        meet(&a, &b);
        meet(&b, &c);
        meet(&a, &c);
        let up = wait(&ra, 10, |e| matches!(e, Event::Up { peer, .. } if *peer == c.me())).expect("a reaches c");
        assert_eq!(up, Event::Up { peer: c.me(), path: format!("via {}", b.me()) });
        let id = a.send(&c.me(), b"through b").unwrap();
        assert!(wait(&rc, 5, |e| matches!(e, Event::Msg { data, .. } if data == b"through b")).is_some());
        assert!(wait(&ra, 5, |e| *e == Event::Delivered { peer: c.me(), id }).is_some());
        // b can't read what it passed on: it was sealed between a and c.
        a.stop();
        b.stop();
        c.stop();
    }

    #[test]
    fn a_teammate_who_reaches_both_is_used_before_the_relay_server() {
        let key: [u8; 32] = random();
        let port = relay_server(Some(key), 60);
        let spec = format!("127.0.0.1:{port}#{}", b64url(&key));
        let ((a, ra), (b, _rb), (c, _rc)) = (with_relay(spec.clone()), node(), with_relay(spec));
        a.block(&c.me());
        c.block(&a.me());
        std::thread::sleep(Duration::from_millis(300)); // the relay setting is resolved
        meet(&a, &b);
        meet(&b, &c);
        meet(&a, &c);
        let up = wait(&ra, 10, |e| matches!(e, Event::Up { peer, .. } if *peer == c.me())).expect("a reaches c");
        assert_eq!(up, Event::Up { peer: c.me(), path: format!("via {}", b.me()) }, "the teammate, not the server");
        for l in [a, b, c] { l.stop() }
    }

    #[test]
    fn a_line_on_the_relay_server_moves_to_a_teammate_who_can_pass_it() {
        let key: [u8; 32] = random();
        let port = relay_server(Some(key), 90);
        let spec = format!("127.0.0.1:{port}#{}", b64url(&key));
        let ((a, ra), (c, _rc)) = (with_relay(spec.clone()), with_relay(spec));
        a.block(&c.me());
        c.block(&a.me());
        std::thread::sleep(Duration::from_millis(300));
        meet(&a, &c);
        let up = wait(&ra, 15, |e| matches!(e, Event::Up { peer, .. } if *peer == c.me())).expect("through the server");
        assert_eq!(up, Event::Up { peer: c.me(), path: format!("relay 127.0.0.1:{port}") });
        // a teammate who reaches both turns up later: the line moves to them
        let (b, _rb) = node();
        meet(&a, &b);
        meet(&b, &c);
        let end = Instant::now() + Duration::from_secs(50);
        let via = format!("via {}", b.me());
        loop {
            let p = a.lines().into_iter().find(|x| x.public == c.me()).and_then(|x| x.path);
            if p.as_deref() == Some(via.as_str()) { break }
            assert!(Instant::now() < end, "still {p:?}");
            std::thread::sleep(Duration::from_millis(200));
        }
        a.send(&c.me(), b"now through b").unwrap();
        for l in [a, b, c] { l.stop() }
    }

    #[test]
    fn with_no_line_the_message_fails_so_git_can_take_it() {
        let ((a, ra), (b, _rb)) = (node(), node());
        let (k, _) = card(&b);
        b.stop();
        a.peer(&k, &["lan=127.0.0.1:9".into()], 0);
        let id = a.send(&k, b"anyone?").unwrap();
        let e = wait(&ra, 15, |e| matches!(e, Event::Failed { .. })).expect("gives up");
        assert_eq!(e, Event::Failed { peer: k, id });
        a.stop();
    }

    #[test]
    fn teammates_met_before_find_each_other_again_with_no_rendezvous() {
        let dir = std::env::temp_dir().join(format!("qh-link-mem-{}", u64::from_be_bytes(random()[..8].try_into().unwrap())));
        let (ka, kb): ([u8; 32], [u8; 32]) = (random(), random());
        let o = |f: &str| Options { port: 0, stun: false, upnp: false, relay: true, beacon: false, remember: Some(dir.join(f)), relays: vec![] };
        let (a, ra) = Link::start(StaticSecret::from(ka), o("a.json")).unwrap();
        let (b, _rb) = Link::start(StaticSecret::from(kb), o("b.json")).unwrap();
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("first meeting");
        std::thread::sleep(Duration::from_millis(300)); // memory written
        let (pa, pb) = (a.port(), b.port());
        a.stop();
        b.stop();
        std::thread::sleep(Duration::from_millis(400));
        drop((a, b));
        // Both come back: no cards, no addresses, only what each remembers.
        let (a2, ra2) = Link::start(StaticSecret::from(ka), o("a.json")).unwrap();
        let (b2, _rb2) = Link::start(StaticSecret::from(kb), o("b.json")).unwrap();
        if (a2.port(), b2.port()) != (pa, pb) {
            // another test took a freed port in between (they run in parallel): nothing to test this time
            eprintln!("skipped: a remembered port was taken meanwhile");
            return;
        }
        for l in [&a2, &b2] {
            for k in l.remembered() {
                l.peer(&k, &[], 0);
            }
        }
        assert!(wait(&ra2, 5, |e| matches!(e, Event::Up { peer, .. } if *peer == b2.me())).is_some(), "found again");
        a2.stop();
        b2.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restarted_teammate_is_picked_up_again() {
        let (a, ra) = node();
        let key = random();
        let (b, rb) = Link::start(StaticSecret::from(key), Options { port: 0, stun: false, upnp: false, relay: true, beacon: false, remember: None, relays: vec![] }).unwrap();
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("up");
        let port = b.port();
        b.stop();
        std::thread::sleep(Duration::from_millis(400)); // its threads let go of the socket
        drop(b);
        drop(rb);
        // Same member key, same port, new process: a's old keys no longer open its packets.
        // its threads may hold the socket a moment longer on a busy machine
        let mut tries = 0;
        let (b2, rb2) = loop {
            match Link::start(StaticSecret::from(key), Options { port, stun: false, upnp: false, relay: true, beacon: false, remember: None, relays: vec![] }) {
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 30 => { tries += 1; std::thread::sleep(Duration::from_millis(100)) }
                r => break r.unwrap(),
            }
        };
        let (k, ad) = card(&a);
        b2.peer(&k, &ad, 0);
        b2.send(&a.me(), b"back").unwrap();
        assert!(wait(&ra, 8, |e| matches!(e, Event::Msg { data, .. } if data == b"back")).is_some());
        a.send(&b2.me(), b"welcome").unwrap();
        assert!(wait(&rb2, 8, |e| matches!(e, Event::Msg { data, .. } if data == b"welcome")).is_some());
        a.stop();
        b2.stop();
    }

    #[test]
    fn strangers_and_replays_are_ignored() {
        let ((a, ra), (b, _rb)) = (node(), node());
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("up");
        // Someone who isn't a teammate sends a hello claiming to be b.
        let (stranger, _) = node();
        let fake = {
            let i = lock(&stranger.inner);
            let mut h = vec![HELLO];
            h.extend(id_of(PublicKey::from(&lock(&b.inner).secret).as_bytes()));
            h.extend(i.eph_pub);
            h.extend([0u8; 12 + 17 + 16]);
            h
        };
        stranger.raw_send(format!("127.0.0.1:{}", a.port()).parse().unwrap(), &fake);
        std::thread::sleep(Duration::from_millis(300));
        let s = a.status();
        assert!(s[0].1.as_deref().is_some_and(|p| p.starts_with("lan")), "line to b untouched: {s:?}");
        let mut sess = Session {
            their_eph: [0; 32], send: aead(&[1; 32]), recv: aead(&[1; 32]), tx_tag: [0; 8], rx_tag: [0; 8], counter: 0, rx_top: 0, rx_seen: 0,
            path: Path::Via([0; 8]), path_rx: Instant::now(), last_rx: Instant::now(), last_tx: Instant::now(), announced: true, heard_addrs: false, probe: None, rtt: None,
        };
        assert!(sess.fresh(5) && sess.fresh(3) && !sess.fresh(5) && !sess.fresh(3) && sess.fresh(4));
        assert!(sess.fresh(200) && !sess.fresh(100), "too old");
        a.stop();
        b.stop();
        stranger.stop();
    }

    #[test]
    fn nothing_on_the_wire_names_a_member() {
        let (a, ra) = node();
        let key = random();
        let o = |port| Options { port, stun: false, upnp: false, relay: true, beacon: false, remember: None, relays: vec![] };
        let (b, rb) = Link::start(StaticSecret::from(key), o(0)).unwrap();
        meet(&a, &b);
        wait(&ra, 5, |e| matches!(e, Event::Up { .. })).expect("up");
        let (me, tx1, rx1, h) = {
            let i = lock(&a.inner);
            let s = i.peers[0].sess.as_ref().unwrap();
            (i.me, s.tx_tag, s.rx_tag, hello(&i, 0, false))
        };
        assert!(!h.windows(8).any(|w| w == me), "a hello must not carry the sender's id");
        assert!(tx1 != me && tx1 != rx1, "data tags are per session and per direction");
        // b comes back as a new process: a new session, so new tags
        let port = b.port();
        b.stop();
        std::thread::sleep(Duration::from_millis(400));
        drop(b);
        drop(rb);
        let (b2, _rb2) = Link::start(StaticSecret::from(key), o(port)).unwrap();
        let (k, ad) = card(&a);
        b2.peer(&k, &ad, 0);
        b2.send(&a.me(), b"again").unwrap();
        assert!(wait(&ra, 8, |e| matches!(e, Event::Msg { data, .. } if data == b"again")).is_some());
        let tx2 = lock(&a.inner).peers[0].sess.as_ref().unwrap().tx_tag;
        assert_ne!(tx1, tx2, "tags change with every session");
        a.stop();
        b2.stop();
    }

    #[test]
    fn an_older_address_list_never_replaces_a_newer_one() {
        let (a, _ra) = node();
        let (b, _rb) = node();
        let k = b.me();
        let addrs = |a: &Link| lock(&a.inner).peers[0].addrs.clone();
        a.peer(&k, &["public=203.0.113.9:1000".into()], 200); // fresh, from the line
        a.peer(&k, &["public=203.0.113.9:2000".into()], 100); // a slow git copy
        assert_eq!(addrs(&a), vec!["203.0.113.9:1000".parse::<SocketAddr>().unwrap()]);
        a.peer(&k, &["public=203.0.113.9:3000".into()], 300);
        assert_eq!(addrs(&a), vec!["203.0.113.9:3000".parse::<SocketAddr>().unwrap()]);
        a.peer(&k, &["public=203.0.113.9:4000".into()], 0); // unknown age (a pasted card): taken
        assert_eq!(addrs(&a), vec!["203.0.113.9:4000".parse::<SocketAddr>().unwrap()]);
        a.stop();
        b.stop();
    }

    #[test]
    fn paths_rank_lan_then_ipv6_then_public_then_relay() {
        let d = |s: &str| rank(&Path::Direct(s.parse().unwrap()));
        assert_eq!(d("192.168.1.4:1"), 0);
        assert_eq!(d("[fe80::1]:1"), 0);
        assert_eq!(d("[2a02:8109::1]:1"), 1);
        assert_eq!(d("84.12.3.4:1"), 2);
        assert_eq!(d("[::ffff:10.0.0.2]:1"), 0);
        assert_eq!(rank(&Path::Via([0; 8])), 3);
    }
}
