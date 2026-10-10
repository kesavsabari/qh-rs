//! OpenRouter's browser login (OAuth PKCE): the user approves qh on openrouter.ai, the browser
//! comes back to a one-shot localhost listener with a code, and the code is exchanged for an
//! API key. No key copying. https://openrouter.ai/docs/use-cases/oauth-pkce
use crate::backend::http;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// SHA-256 (FIPS 180-4), just for the PKCE challenge; not worth a dependency.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 { msg.push(0) }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 { w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap()) }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for i in 0..8 { h[i] = h[i].wrapping_add(v[i]) }
    }
    let mut out = [0u8; 32];
    for i in 0..8 { out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes()) }
    out
}

fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() { s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char) }
    }
    s
}

/// 32 unpredictable bytes without a rand crate: the OS's randomness where there is a file for
/// it, else hashed clocks, addresses and the process id.
fn random() -> [u8; 32] {
    let mut seed = Vec::new();
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut b = [0u8; 32];
        if std::io::Read::read_exact(&mut f, &mut b).is_ok() { return b }
    }
    for _ in 0..64 {
        seed.extend_from_slice(&std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos().to_le_bytes());
        seed.extend_from_slice(&(Instant::now().elapsed().as_nanos() as u64).to_le_bytes());
        let probe = Box::new(0u8);
        seed.extend_from_slice(&(&*probe as *const u8 as usize).to_le_bytes());
    }
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    sha256(&seed)
}

/// Run the login; returns an OpenRouter API key.
#[allow(dead_code)] // behind setup::OPENROUTER_LOGIN until tested
pub fn openrouter() -> Result<String, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("can't open a local port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let verifier = b64url(&random());
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let callback = format!("http://localhost:{port}/callback");
    let url = format!("https://openrouter.ai/auth?callback_url={}&code_challenge={challenge}&code_challenge_method=S256",
                      callback.replace(':', "%3A").replace('/', "%2F"));
    println!("Opening your browser to log in to OpenRouter. If it doesn't open, visit:\n  {url}");
    crate::platform::open_url(&url);
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let code = loop {
        if t0.elapsed() > Duration::from_secs(300) {
            return Err("no answer from the browser within 5 minutes".into());
        }
        let Ok((mut stream, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(100)); continue };
        let _ = stream.set_nonblocking(false);
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        // "GET /callback?code=abc HTTP/1.1"
        let code = line.split_whitespace().nth(1).and_then(|p| p.split_once("code=")).map(|(_, c)| c.split('&').next().unwrap_or("").to_string());
        let body = if code.is_some() { "Logged in. You can close this tab and go back to qh." } else { "No code in this request; try again from qh." };
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
<html><body style=\"font-family:sans-serif;padding:3em\"><h2>{body}</h2></body></html>");
        if let Some(c) = code.filter(|c| !c.is_empty()) { break c }
    };
    let r = http().post("https://openrouter.ai/api/v1/auth/keys").timeout(Duration::from_secs(20))
        .set("Content-Type", "application/json")
        .send_string(&json!({"code": code, "code_verifier": verifier, "code_challenge_method": "S256"}).to_string())
        .map_err(|e| format!("OpenRouter didn't hand over a key: {e}"))?;
    let v: Value = serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    v["key"].as_str().map(String::from).ok_or_else(|| "OpenRouter's answer had no key".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_and_b64url_match_known_vectors() {
        let hex: String = sha256(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // RFC 7636 appendix B
        assert_eq!(b64url(&sha256(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
