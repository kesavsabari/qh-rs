//! ChatGPT plans in qh, with no API key. Two routes:
//! - Codex's (the default; works on every plan, Go included): the Codex CLI's OAuth client over
//!   PKCE on localhost:1455, or the Codex CLI's own saved sign-in (~/.codex/auth.json) when there
//!   is one, and the Codex backend on chatgpt.com. It isn't OpenAI's official route for other apps
//!   (OpenCode and others use it the same way), so OpenAI could close it.
//! - Sign in with ChatGPT (`qh login chatgpt-official`; Plus/Pro only): OpenAI's open-source flow
//!   (developers.openai.com/siwc/token-sharing-open-source), dynamic client registration over
//!   OAuth PKCE on a 127.0.0.1 loopback, and api.openai.com.
//! Either way tokens sit in a 0600 file and are refreshed before they expire, and inference goes
//! through the Responses API (store false, stream true, system prompt as `instructions`).
use crate::backend::{http, Error, Reply};
use crate::oauth::{b64url, b64url_decode, random, sha256};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const AUTH: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";
const ISSUER: &str = "https://auth.openai.com";
pub const API: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const APP_NAME: &str = "qh";
const NAMESPACE: &str = "qh";

const CODEX: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_AUTH: &str = "https://auth.openai.com/oauth/authorize";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_SCOPE: &str = "openid profile email offline_access";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_VERSION: &str = "0.161.0"; // the Codex release whose model list we ask for
const CODEX_PORT: u16 = 1455; // the only redirect Codex's client allows

fn creds_path() -> std::path::PathBuf {
    crate::config::config_dir().join("chatgpt.json")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load() -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(creds_path()).ok()?).ok()
}

fn save(v: &Value) -> Result<(), String> {
    let p = creds_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let tmp = p.with_extension("tmp");
    crate::platform::write_private(&tmp, serde_json::to_string_pretty(v).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &p).map_err(|e| e.to_string())
}

pub fn signed_in() -> bool {
    load().is_some_and(|c| match c["route"].as_str() {
        Some("codex-shared") => shared().is_some(),
        _ => c["refresh_token"].as_str().is_some_and(|t| !t.is_empty()),
    })
}

/// The account's email, and its plan on the Codex route ("user@x.com (go)").
pub fn account() -> Option<String> {
    let c = load()?;
    let email = c["email"].as_str()?.to_string();
    Some(match c["plan"].as_str() {
        Some(p) if !p.is_empty() => format!("{email} ({p})"),
        _ => email,
    })
}

/// Forget qh's sign-in. A shared Codex sign-in stays with the Codex CLI.
pub fn sign_out() {
    let _ = std::fs::remove_file(creds_path());
}

/// The Codex CLI's sign-in file.
fn codex_file() -> std::path::PathBuf {
    std::env::var("CODEX_HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| crate::config::home().join(".codex")).join("auth.json")
}

/// The Codex CLI's saved ChatGPT sign-in, if it has one.
fn shared() -> Option<Value> {
    let f: Value = serde_json::from_str(&std::fs::read_to_string(codex_file()).ok()?).ok()?;
    f["tokens"]["refresh_token"].as_str().is_some_and(|t| !t.is_empty()).then_some(f)
}

/// A JWT's claims, unchecked: for tokens that came straight from OpenAI or from our own files.
fn claims(jwt: &str) -> Value {
    jwt.split('.').nth(1).and_then(|p| serde_json::from_slice(&b64url_decode(p)).ok()).unwrap_or(Value::Null)
}

/// Email, plan and account id from a Codex-route ID token.
fn who(id_token: &str) -> (String, String, String) {
    let c = claims(id_token);
    let a = &c["https://api.openai.com/auth"];
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    (s(&c["email"]), s(&a["chatgpt_plan_type"]), s(&a["chatgpt_account_id"]))
}

/// Seconds since 1970 as RFC 3339 UTC, the form Codex keeps `last_refresh` in.
fn rfc3339(t: u64) -> String {
    let (days, secs) = ((t / 86400) as i64, t % 86400);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

/// One per installation, kept across sign-ins as OpenAI asks.
fn host_id() -> String {
    let p = crate::config::config_dir().join("host_id");
    if let Some(id) = std::fs::read_to_string(&p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return id;
    }
    let r = random();
    let hex: String = r[..16].iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("urn:uuid:{}-{}-4{}-a{}-{}", &hex[0..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32]);
    let _ = std::fs::create_dir_all(crate::config::config_dir());
    let _ = std::fs::write(&p, &id);
    id
}

fn enc(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

fn dec(s: &str) -> String {
    let (b, mut out, mut i) = (s.as_bytes(), Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'+' => { out.push(b' '); i += 1 }
            b'%' if i + 2 < b.len() => {
                let v = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(b'?');
                out.push(v);
                i += 3;
            }
            c => { out.push(c); i += 1 }
        }
    }
    String::from_utf8_lossy(&out).into()
}

fn query(path: &str) -> Vec<(String, String)> {
    path.split_once('?').map(|(_, q)| q).unwrap_or("").split('&').filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (dec(k), dec(v))).collect()
}

fn form(url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    let r = http().post(url).timeout(Duration::from_secs(20)).send_form(fields).map_err(|e| match e {
        ureq::Error::Status(c, r) => format!("HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>()),
        e => e.to_string(),
    })?;
    serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// The ID token's payload, after checking its RS256 signature against OpenAI's published keys
/// and the issuer, audience, expiry and (when the sign-in sent one) nonce.
fn verify_id_token(token: &str, client_id: &str, nonce: Option<&str>) -> Result<Value, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("malformed ID token".into());
    }
    let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).map_err(|_| "bad ID token header")?;
    let payload: Value = serde_json::from_slice(&b64url_decode(parts[1])).map_err(|_| "bad ID token payload")?;
    if header["alg"] != "RS256" {
        return Err(format!("unexpected ID token algorithm {}", header["alg"]));
    }
    let jwks: Value = serde_json::from_str(&http().get(JWKS).timeout(Duration::from_secs(10)).call()
        .map_err(|e| format!("couldn't fetch OpenAI's signing keys: {e}"))?.into_string().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let key = jwks["keys"].as_array().into_iter().flatten().find(|k| k["kid"] == header["kid"])
        .ok_or("ID token signed with an unknown key")?;
    let pk = ring::signature::RsaPublicKeyComponents {
        n: b64url_decode(key["n"].as_str().unwrap_or("")),
        e: b64url_decode(key["e"].as_str().unwrap_or("")),
    };
    pk.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64url_decode(parts[2]))
        .map_err(|_| "ID token signature is invalid")?;
    let aud_ok = payload["aud"] == client_id || payload["aud"].as_array().is_some_and(|a| a.iter().any(|x| x == client_id));
    if payload["iss"] != ISSUER || !aud_ok {
        return Err("ID token was issued for someone else".into());
    }
    if payload["exp"].as_u64().unwrap_or(0) + 60 < now() {
        return Err("ID token has expired".into());
    }
    if nonce.is_some_and(|n| payload["nonce"] != n) {
        return Err("ID token nonce doesn't match this sign-in".into());
    }
    Ok(payload)
}

/// The loopback the browser comes back to. `fixed`: only port 1455 will do.
fn listen(fixed: bool) -> Result<(TcpListener, u16), String> {
    // the docs' example port first; for Sign in with ChatGPT any free one works since only the port may vary
    let l = TcpListener::bind("127.0.0.1:1455").or_else(|e| if fixed { Err(e) } else { TcpListener::bind("127.0.0.1:0") })
        .map_err(|e| if fixed {
            format!("port {CODEX_PORT} is busy (another sign-in, maybe `codex login`?); close it and try again ({e})")
        } else {
            format!("can't open a local port: {e}")
        })?;
    let port = l.local_addr().map_err(|e| e.to_string())?.port();
    Ok((l, port))
}

fn reply(stream: &mut std::net::TcpStream, msg: &str) {
    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n\
<html><body style=\"font-family:sans-serif;padding:3em\"><h2>{msg}</h2></body></html>");
}

/// Wait for the browser to come back to the loopback with this sign-in's `state`; its query.
fn wait(listener: &TcpListener, state: &str) -> Result<Vec<(String, String)>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    loop {
        if t0.elapsed() > Duration::from_secs(600) {
            return Err("no answer from the browser within 10 minutes".into());
        }
        let Ok((mut stream, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(100)); continue };
        let _ = stream.set_nonblocking(false);
        let mut line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut line);
        let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
        if !path.starts_with("/auth/callback") {
            reply(&mut stream, "Waiting for the ChatGPT sign-in…");
            continue;
        }
        let q = query(&path);
        let get = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        if get("state").as_deref() != Some(state) {
            reply(&mut stream, "This sign-in didn't come from qh; try again from qh.");
            continue;
        }
        if let Some(e) = get("error") {
            reply(&mut stream, "Sign-in was cancelled. You can close this tab.");
            return Err(if e == "access_denied" { "you declined; nothing was saved".into() } else { format!("sign-in failed: {e}") });
        }
        reply(&mut stream, "Signed in. You can close this tab and go back to qh.");
        return Ok(q);
    }
}

/// Sign in for the Codex route: the Codex CLI's saved sign-in when there is one, else the browser.
/// Returns the account ("email (plan)").
pub fn login(say: &dyn Fn(&str)) -> Result<String, String> {
    if let Some(f) = shared() {
        let (email, plan, _) = who(f["tokens"]["id_token"].as_str().unwrap_or(""));
        save(&json!({"route": "codex-shared", "email": email, "plan": plan, "saved_at": now()}))?;
        say(&format!("Using the Codex CLI's ChatGPT sign-in ({}); `codex logout` or `qh logout chatgpt` stops it.", codex_file().display()));
        return Ok(account().unwrap_or(email));
    }
    let (listener, port) = listen(true)?;
    let redirect = format!("http://localhost:{port}/auth/callback");
    let (state, verifier) = (b64url(&random()), b64url(&random()));
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let params = [
        ("response_type", "code"), ("client_id", CODEX_CLIENT), ("redirect_uri", &redirect), ("scope", CODEX_SCOPE),
        ("code_challenge", &challenge), ("code_challenge_method", "S256"), ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"), ("state", &state), ("originator", CODEX_ORIGINATOR),
    ];
    let url = format!("{CODEX_AUTH}?{}", params.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"));
    say(&format!("Opening your browser to sign in with ChatGPT. If it doesn't open, visit:\n  {url}"));
    crate::platform::open_url(&url);
    let q = wait(&listener, &state)?;
    let code = q.iter().find(|(a, _)| a == "code").map(|(_, v)| v.clone()).ok_or("the browser came back without a code")?;
    let t = form(CODEX_TOKEN, &[("grant_type", "authorization_code"), ("code", &code), ("redirect_uri", &redirect),
        ("client_id", CODEX_CLIENT), ("code_verifier", &verifier)])?;
    let id_token = t["id_token"].as_str().ok_or("no ID token in the answer")?;
    verify_id_token(id_token, CODEX_CLIENT, None)?;
    let (email, plan, account_id) = who(id_token);
    if account_id.is_empty() {
        return Err("signed in, but OpenAI sent no ChatGPT account (is this a ChatGPT login?)".into());
    }
    let access = t["access_token"].as_str().unwrap_or("");
    save(&json!({
        "route": "codex", "email": email, "plan": plan, "account_id": account_id, "id_token": id_token,
        "access_token": access, "refresh_token": t["refresh_token"], "expires_at": expiry(access, &t), "saved_at": now(),
    }))?;
    Ok(account().unwrap_or(email))
}

/// When an access token runs out: its `exp`, else the answer's `expires_in`.
fn expiry(access: &str, answer: &Value) -> u64 {
    claims(access)["exp"].as_u64().unwrap_or_else(|| now() + answer["expires_in"].as_u64().unwrap_or(3600))
}

/// Sign in with ChatGPT, OpenAI's official route for other apps (Plus/Pro plans). Returns the email.
pub fn login_official(say: &dyn Fn(&str)) -> Result<String, String> {
    let saved = load().filter(|c| matches!(c["route"].as_str(), None | Some("official")));
    let (listener, port) = listen(false)?;
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let (state, nonce, verifier) = (b64url(&random()), b64url(&random()), b64url(&random()));
    let challenge = b64url(&sha256(verifier.as_bytes()));
    let known_client = saved.as_ref().and_then(|c| c["client_id"].as_str().map(String::from));
    let mut params = vec![
        ("client_id", known_client.clone().unwrap_or_else(|| "dynamic_agent_client".into())),
        ("ext_agent_host_id", host_id()),
        ("response_type", "code".into()),
        ("redirect_uri", redirect.clone()),
        ("scope", SCOPE.into()),
        ("resource", API.into()),
        ("state", state.clone()),
        ("nonce", nonce.clone()),
        ("code_challenge_method", "S256".into()),
        ("code_challenge", challenge),
    ];
    match &saved {
        Some(c) => {
            if let Some(t) = c["id_token"].as_str() { params.push(("id_token_hint", t.into())) }
            if let Some(e) = c["email"].as_str() { params.push(("login_hint", e.into())) }
        }
        None => params.push(("agent_name_hint", APP_NAME.into())),
    }
    let url = format!("{AUTH}?{}", params.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&"));
    say(&format!("Opening your browser to sign in with ChatGPT. If it doesn't open, visit:\n  {url}"));
    crate::platform::open_url(&url);

    let q = wait(&listener, &state)?;
    let get = |k: &str| q.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let code = get("code").ok_or("the browser came back without a code")?;
    let client_id = match (&known_client, get("client_id")) {
        (Some(k), Some(c)) if *k != c => return Err("the browser returned a different registration; sign out and try again".into()),
        (Some(k), _) => k.clone(),
        (None, Some(c)) if c != "dynamic_agent_client" => c,
        (None, _) => return Err("registration didn't finish (no client id came back)".into()),
    };
    let t = form(TOKEN, &[("grant_type", "authorization_code"), ("code", &code), ("client_id", &client_id),
        ("redirect_uri", &redirect), ("code_verifier", &verifier), ("resource", API)])?;
    let id_token = t["id_token"].as_str().ok_or("no ID token in the answer")?;
    let claims = verify_id_token(id_token, &client_id, Some(&nonce))?;
    if let Some(prev) = saved.as_ref().and_then(|c| c["subject"].as_str()) {
        if claims["sub"].as_str() != Some(prev) {
            return Err("that's a different ChatGPT account than the saved one; run `qh logout chatgpt` first".into());
        }
    }
    let scopes: Vec<&str> = t["scope"].as_str().unwrap_or("").split_whitespace().collect();
    if !scopes.contains(&"chatgpt.tokens.use.direct") {
        return Err("signed in, but ChatGPT plan use wasn't allowed for qh. Sign in again and allow it.".into());
    }
    let email = claims["email"].as_str().unwrap_or("").to_string();
    save(&json!({
        "route": "official", "email": email, "issuer": ISSUER, "subject": claims["sub"], "client_id": client_id,
        "ext_agent_host_id": host_id(), "id_token": id_token, "access_token": t["access_token"],
        "refresh_token": t["refresh_token"], "token_type": "Bearer", "expires_in": t["expires_in"],
        "expires_at": now() + t["expires_in"].as_u64().unwrap_or(3600), "scopes": scopes, "saved_at": now(),
    }))?;
    Ok(email)
}

/// What a request needs: a valid access token, and on the Codex route the ChatGPT account.
struct Auth {
    token: String,
    account: Option<String>, // Some: the Codex route
}

impl Auth {
    fn base(&self) -> &'static str {
        if self.account.is_some() { CODEX } else { API }
    }

    fn sign(&self, r: ureq::Request) -> ureq::Request {
        let r = r.set("Authorization", &format!("Bearer {}", self.token));
        match &self.account {
            Some(a) => r.set("chatgpt-account-id", a).set("originator", CODEX_ORIGINATOR).set("OpenAI-Beta", "responses=experimental"),
            None => r,
        }
    }
}

/// The Codex route's token refresh. Refresh tokens are single-use: the answer carries the next one.
fn codex_refresh(refresh_token: &str) -> Result<Value, String> {
    let body = json!({"client_id": CODEX_CLIENT, "grant_type": "refresh_token", "refresh_token": refresh_token, "scope": "openid profile email"});
    let r = http().post(CODEX_TOKEN).timeout(Duration::from_secs(20)).set("Content-Type", "application/json")
        .send_string(&body.to_string()).map_err(|e| match e {
            ureq::Error::Status(c, r) => format!("HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(300).collect::<String>()),
            e => e.to_string(),
        })?;
    serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

/// Valid credentials, the access token refreshed when it has under five minutes left.
fn auth() -> Result<Auth, String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(()); // one refresh at a time across sessions
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut c = load().ok_or("not signed in to ChatGPT: run `qh login chatgpt`")?;
    match c["route"].as_str() {
        Some("codex-shared") => {
            // the Codex CLI's file is the one copy of these tokens: a refresh is written back to it,
            // the way Codex does, so neither side ends up holding a used-up refresh token
            let path = codex_file();
            let mut f = shared().ok_or("the Codex CLI is signed out; run `codex login` or `qh login chatgpt`")?;
            let t = &f["tokens"];
            let account = t["account_id"].as_str().map(String::from)
                .unwrap_or_else(|| who(t["id_token"].as_str().unwrap_or("")).2);
            let access = t["access_token"].as_str().unwrap_or("").to_string();
            if claims(&access)["exp"].as_u64().unwrap_or(0) > now() + 300 {
                return Ok(Auth { token: access, account: Some(account) });
            }
            let n = codex_refresh(t["refresh_token"].as_str().unwrap_or(""))
                .map_err(|e| format!("the Codex CLI's ChatGPT sign-in has expired ({e}); run `codex login` or `qh login chatgpt`"))?;
            for k in ["access_token", "refresh_token", "id_token"] {
                if n[k].is_string() { f["tokens"][k] = n[k].clone() }
            }
            f["last_refresh"] = rfc3339(now()).into();
            let tmp = path.with_extension("qh-tmp");
            crate::platform::write_private(&tmp, serde_json::to_string_pretty(&f).unwrap_or_default().as_bytes()).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
            let token = f["tokens"]["access_token"].as_str().unwrap_or("").to_string();
            Ok(Auth { token, account: Some(account) })
        }
        Some("codex") => {
            let account = Some(c["account_id"].as_str().unwrap_or("").to_string());
            if c["expires_at"].as_u64().unwrap_or(0) > now() + 300 {
                let token = c["access_token"].as_str().ok_or("saved ChatGPT sign-in is damaged; run `qh login chatgpt`")?.to_string();
                return Ok(Auth { token, account });
            }
            let n = codex_refresh(c["refresh_token"].as_str().unwrap_or(""))
                .map_err(|e| format!("ChatGPT sign-in has expired ({e}); run `qh login chatgpt`"))?;
            for k in ["access_token", "refresh_token", "id_token"] {
                if n[k].is_string() { c[k] = n[k].clone() }
            }
            c["expires_at"] = expiry(c["access_token"].as_str().unwrap_or(""), &n).into();
            c["saved_at"] = now().into();
            save(&c)?;
            let token = c["access_token"].as_str().ok_or("refresh gave no access token")?.to_string();
            Ok(Auth { token, account })
        }
        _ => {
            if c["expires_at"].as_u64().unwrap_or(0) > now() + 300 {
                let token = c["access_token"].as_str().ok_or("saved ChatGPT sign-in is damaged; run `qh login chatgpt`")?.to_string();
                return Ok(Auth { token, account: None });
            }
            let t = form(TOKEN, &[("grant_type", "refresh_token"), ("refresh_token", c["refresh_token"].as_str().unwrap_or("")),
                ("client_id", c["client_id"].as_str().unwrap_or("")), ("resource", API)])
                .map_err(|e| format!("ChatGPT sign-in has expired ({e}); run `qh login chatgpt`"))?;
            for k in ["access_token", "refresh_token", "expires_in"] {
                if !t[k].is_null() { c[k] = t[k].clone() }
            }
            if let Some(s) = t["scope"].as_str() { c["scopes"] = s.split_whitespace().collect::<Vec<_>>().into() }
            c["expires_at"] = (now() + t["expires_in"].as_u64().unwrap_or(3600)).into();
            c["saved_at"] = now().into();
            save(&c)?;
            let token = c["access_token"].as_str().ok_or("refresh gave no access token")?.to_string();
            Ok(Auth { token, account: None })
        }
    }
}

/// The signed-in account's models, as slugs (display names only differ in case/punctuation),
/// with the context window each one can use when the list says.
pub fn models() -> Result<Vec<(String, Option<usize>)>, String> {
    let a = auth()?;
    let url = match a.account {
        Some(_) => format!("{CODEX}/models?client_version={CODEX_VERSION}"),
        None => format!("{API}/models"),
    };
    let r = a.sign(http().get(&url).timeout(Duration::from_secs(10))).call().map_err(|e| match e {
            ureq::Error::Status(c, r) => format!("HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
            e => e.to_string(),
        })?;
    let v: Value = serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    Ok(v["models"].as_array().into_iter().flatten().filter(|m| m["visibility"] == "list").filter_map(|m| {
        let ctx = m["context_window"].as_u64()
            .map(|w| (w * m["effective_context_window_percent"].as_u64().unwrap_or(100).min(100) / 100) as usize);
        Some((m["slug"].as_str()?.to_string(), ctx))
    }).collect())
}

fn text(m: &Value) -> &str {
    m["content"].as_str().unwrap_or("")
}

/// qh's Ollama-shaped history → Responses `instructions` + `input` items.
fn to_input(messages: &[Value]) -> (String, Vec<Value>) {
    let (mut instructions, mut input) = (String::new(), vec![]);
    let mut ids: std::collections::VecDeque<String> = Default::default();
    for (n, m) in messages.iter().enumerate() {
        let images: Vec<Value> = m["images"].as_array().into_iter().flatten().filter_map(|i| i.as_str())
            .map(|i| json!({"type": "input_image", "image_url": format!("data:image/png;base64,{i}")})).collect();
        match m["role"].as_str().unwrap_or("") {
            "system" => { if !instructions.is_empty() { instructions.push('\n') } instructions.push_str(text(m)) }
            "user" if !images.is_empty() => {
                let mut parts = vec![json!({"type": "input_text", "text": text(m)})];
                parts.extend(images);
                input.push(json!({"role": "user", "content": parts}));
            }
            "user" => input.push(json!({"role": "user", "content": text(m)})),
            "assistant" => {
                if !text(m).is_empty() {
                    input.push(json!({"role": "assistant", "content": text(m)}));
                }
                for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
                    let id = c["id"].as_str().map(String::from).unwrap_or_else(|| format!("call_{n}_{i}"));
                    ids.push_back(id.clone());
                    let args = match &c["function"]["arguments"] { Value::String(a) => a.clone(), Value::Null => "{}".into(), a => a.to_string() };
                    let mut item = json!({"type": "function_call", "call_id": id, "name": c["function"]["name"], "arguments": args});
                    if let Some(ns) = c["namespace"].as_str() { item["namespace"] = ns.into() }
                    input.push(item);
                }
            }
            "tool" => {
                let id = ids.pop_front().unwrap_or_else(|| format!("call_orphan_{n}"));
                input.push(json!({"type": "function_call_output", "call_id": id, "output": text(m)}));
                if !images.is_empty() {
                    let mut parts = vec![json!({"type": "input_text", "text": "(screenshot from the tool above)"})];
                    parts.extend(images);
                    input.push(json!({"role": "user", "content": parts}));
                }
            }
            _ => {}
        }
    }
    (instructions, input)
}

/// One streamed Responses call on the user's ChatGPT plan.
pub fn chat(model: &str, messages: &[Value], tools: &[Value], think: &str,
            on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    let a = auth().map_err(Error::Other)?;
    let codex = a.account.is_some();
    let (instructions, mut input) = to_input(messages);
    if codex {
        // the Codex backend takes plain function tools, no namespaces
        input.iter_mut().filter_map(Value::as_object_mut).for_each(|i| { i.remove("namespace"); });
    }
    let mut body = json!({"model": model, "instructions": instructions, "input": input, "store": false, "stream": true});
    if !tools.is_empty() {
        let fns: Vec<Value> = tools.iter().map(|t| {
            let f = &t["function"];
            json!({"type": "function", "name": f["name"], "description": f["description"], "parameters": f["parameters"]})
        }).collect();
        body["tools"] = if codex { fns.into() } else { json!([{"type": "namespace", "name": NAMESPACE, "description": "qh agent tools", "tools": fns}]) };
    }
    if !think.is_empty() {
        body["reasoning"] = json!({"summary": "auto"});
        if matches!(think, "low" | "medium" | "high") {
            body["reasoning"]["effort"] = json!(think);
        }
    }
    let r = a.sign(http().post(&format!("{}/responses", a.base()))).set("Content-Type", "application/json")
        .set("Accept", "text/event-stream").send_string(&body.to_string()).map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let text = r.into_string().unwrap_or_default();
                let err: Value = serde_json::from_str(&text).unwrap_or_default();
                let e = &err["error"];
                if e["type"] == "usage_limit_reached" || e["code"] == "usage_limit_reached" {
                    let wait = e["resets_in_seconds"].as_u64().map(|s| format!(" (resets in {}h {}m)", s / 3600, s / 60 % 60)).unwrap_or_default();
                    return Error::Other(format!("you've hit your ChatGPT plan's limit for {model}{wait}; switch model with /model"));
                }
                if code == 429 {
                    let why = e["message"].as_str().unwrap_or("too many requests").to_string();
                    return Error::Limited(why, None);
                }
                let msg = e["message"].as_str().or(err["detail"].as_str()).map(String::from).unwrap_or(text);
                Error::Other(format!("HTTP {code}: {msg}"))
            }
            ureq::Error::Transport(t) => Error::Connect(t.to_string()),
        })?;
    // the Codex backend reports the plan's 5-hour window use in headers (as the Codex CLI reads them)
    let plan_pct = r.header("x-codex-primary-used-percent").and_then(|s| s.trim().parse::<f64>().ok());
    let mut usage = crate::usage::Calls::default();
    let (mut content, mut calls, mut prompt, mut total, mut done) = (String::new(), vec![], 0, 0, false);
    for line in BufReader::new(r.into_reader()).lines() {
        let line = line.map_err(|e| Error::Other(e.to_string()))?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
        match ev["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                let d = ev["delta"].as_str().unwrap_or("");
                content.push_str(d);
                if !on(false, d) { return Err(Error::Cancelled) }
            }
            // reasoning starts long before (or without) any summary text: say so right away
            "response.output_item.added" if ev["item"]["type"] == "reasoning" => {
                if !on(true, "") { return Err(Error::Cancelled) }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if !on(true, ev["delta"].as_str().unwrap_or("")) { return Err(Error::Cancelled) }
            }
            "response.output_item.done" if ev["item"]["type"] == "function_call" => {
                let it = &ev["item"];
                let mut c = json!({"id": it["call_id"], "function": {"name": it["name"], "arguments": it["arguments"]}});
                if let Some(ns) = it["namespace"].as_str() { c["namespace"] = ns.into() }
                calls.push(c);
            }
            "response.completed" => {
                let u = &ev["response"]["usage"];
                prompt = u["input_tokens"].as_u64().unwrap_or(0);
                total = u["total_tokens"].as_u64().unwrap_or(prompt + u["output_tokens"].as_u64().unwrap_or(0));
                usage = crate::usage::responses(u);
                done = true;
            }
            "response.failed" | "response.incomplete" | "error" => {
                let e = if ev["error"].is_object() { &ev["error"] } else { &ev["response"]["error"] };
                let code = e["code"].as_str().unwrap_or("");
                let msg = match code {
                    "subscription_sharing_usage_limit_exceeded" =>
                        "your ChatGPT plan's allowance for qh is used up for now; raise qh's cap in ChatGPT settings, wait for the reset, or switch model with /model".into(),
                    "subscription_sharing_usage_unavailable" =>
                        "ChatGPT plan use isn't available for this account right now; check ChatGPT settings → connected apps".into(),
                    _ => format!("ChatGPT request failed: {} {}", code, e["message"].as_str().unwrap_or(ev["type"].as_str().unwrap_or(""))),
                };
                return Err(Error::Other(msg));
            }
            _ => {}
        }
    }
    if !done {
        return Err(Error::Other("the ChatGPT stream ended before the reply was complete".into()));
    }
    let mut msg = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        msg["tool_calls"] = calls.into();
    }
    Ok(Reply { msg, prompt, total, usage, plan_pct })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_maps_to_responses_items() {
        let msgs = vec![
            json!({"role": "system", "content": "be terse"}),
            json!({"role": "user", "content": "read a"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_x", "namespace": "qh", "function": {"name": "read_file", "arguments": {"path": "a"}}}]}),
            json!({"role": "tool", "tool_name": "read_file", "content": "hello"}),
        ];
        let (ins, input) = to_input(&msgs);
        assert_eq!(ins, "be terse");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_x");
        assert_eq!(input[1]["namespace"], "qh");
        assert_eq!(input[1]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(input[2], json!({"type": "function_call_output", "call_id": "call_x", "output": "hello"}));
        assert!(input.iter().all(|i| i["role"] != "system"));
    }

    #[test]
    fn codex_claims_and_times() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1760200123), "2025-10-11T16:28:43Z");
        assert_eq!(rfc3339(951782400), "2000-02-29T00:00:00Z");
        let body = b64url(br#"{"email":"a@b.c","https://api.openai.com/auth":{"chatgpt_plan_type":"go","chatgpt_account_id":"acc"}}"#);
        assert_eq!(who(&format!("x.{body}.y")), ("a@b.c".into(), "go".into(), "acc".into()));
        assert_eq!(who("garbage"), (String::new(), String::new(), String::new()));
    }

    #[test]
    fn url_coding_round_trips() {
        assert_eq!(enc("a b/c:d"), "a%20b%2Fc%3Ad");
        let q = query("/auth/callback?code=x%2By&scope=a+b&state=s");
        assert_eq!(q[0], ("code".into(), "x+y".into()));
        assert_eq!(q[1].1, "a b");
    }
}
