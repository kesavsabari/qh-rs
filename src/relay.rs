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
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
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
    ends: [Option<(SocketAddr, Instant)>; 2], // where each of the pair proved to be, and when last heard
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
    serve(&sock, secret, None);
    0
}

/// The relay loop. `until` stops it (tests).
pub fn serve(sock: &UdpSocket, secret: Option<[u8; 32]>, until: Option<Instant>) {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &crate::oauth::random());
    let mut table: HashMap<[u8; 16], Slot> = HashMap::new();
    let mut per_ip: HashMap<IpAddr, usize> = HashMap::new();
    let (mut asked, mut asked_at, mut asked_all) = (HashMap::<IpAddr, u32>::new(), Instant::now(), 0u32);
    let (mut swept, mut stats, mut passed, mut dropped) = (Instant::now(), Instant::now(), 0u64, 0u64);
    let mut buf = [0u8; 2048];
    while until.is_none_or(|u| Instant::now() < u) {
        let now = Instant::now();
        if now - swept > Duration::from_secs(30) {
            swept = now;
            table.retain(|_, s| {
                for e in s.ends.iter_mut() {
                    if e.is_some_and(|(_, t)| now - t >= IDLE) {
                        *e = None;
                    }
                }
                s.ends.iter().any(|e| e.is_some())
            });
            per_ip.clear();
            for s in table.values() {
                for (e, _) in s.ends.iter().flatten() {
                    *per_ip.entry(e.ip()).or_default() += 1;
                }
            }
        }
        if until.is_none() && now - stats > Duration::from_secs(300) {
            stats = now;
            println!("{} pairs meeting, {passed} packets passed, {dropped} dropped (last 5 min)", table.len());
            (passed, dropped) = (0, 0);
        }
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        if n < 26 || buf[0] != RELAYED {
            continue;
        }
        let token: [u8; 16] = buf[1..17].try_into().unwrap();
        if let Some(k) = &secret {
            if crate::link::relay_mac(k, &token) != buf[17..25] {
                dropped += 1;
                continue;
            }
        }
        let body = &buf[25..n];
        if body[0] == PROOF {
            let Some(side) = proven(&key, &token, from, body) else {
                dropped += 1;
                continue;
            };
            if !table.contains_key(&token) {
                let used = per_ip.entry(from.ip()).or_default();
                if *used >= TOKENS_PER_IP || table.len() >= MAX_TOKENS {
                    dropped += 1;
                    continue;
                }
                *used += 1;
                table.insert(token, Slot { ends: [None, None], window: now, bytes: 0 });
            }
            // that side of the pair is here now (it moved, or it's new)
            table.get_mut(&token).unwrap().ends[side] = Some((from, now));
            continue;
        }
        let Some((slot, side)) = table.get_mut(&token).and_then(|s| {
            let side = s.ends.iter().position(|e| e.is_some_and(|(a, _)| a == from))?;
            Some((s, side))
        }) else {
            // an address that hasn't proved itself: ask it to (within the challenge budget)
            if now - asked_at >= Duration::from_secs(1) {
                (asked_at, asked_all) = (now, 0);
                asked.clear();
            }
            let n_ip = asked.entry(from.ip()).or_default();
            *n_ip += 1;
            asked_all += 1;
            if *n_ip <= CHALLENGES_PER_IP && asked_all <= CHALLENGES && asked.len() < 4096 {
                let mut c = vec![CHALLENGE];
                c.extend(token);
                c.extend(cookie(&key, &token, from, window()));
                let _ = sock.send_to(&c, from);
            }
            dropped += 1;
            continue;
        };
        slot.ends[side] = Some((from, now));
        if now - slot.window > Duration::from_secs(60) {
            slot.window = now;
            slot.bytes = 0;
        }
        slot.bytes += n;
        if slot.bytes > BYTES_PER_MIN {
            dropped += 1;
            continue;
        }
        if let Some((to, seen)) = slot.ends[1 - side] {
            if now - seen < IDLE {
                let _ = sock.send_to(&buf[..n], to);
                passed += 1;
            }
        }
    }
}
