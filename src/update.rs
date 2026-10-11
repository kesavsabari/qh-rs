//! Updates. Each release carries `qh-release.json` (the version, and every binary's name, size and
//! SHA-256) and `qh-release.json.sig`, an Ed25519 signature over it made on the maintainer's laptop
//! with a key that never goes to GitHub. qh holds the public key: a release that isn't signed by it
//! is never offered or installed, and nothing older than the running version is.
//!
//! The check runs at most once a day in the background (not in `qh -p`), fetches those two static
//! files from GitHub, and sends nothing about this install. `qh update` / `/update` downloads the
//! binary for this machine, checks size and hash, makes sure it runs and reports the expected
//! version, then swaps it in place.
use crate::oauth::sha256;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES: &str = "https://github.com/kesavsabari/qh-rs/releases/latest/download";
/// The release key's public half (the private half: ~/.config/qh-release/key.pem on the maintainer's laptop).
const PUBLIC_KEY: [u8; 32] = [121, 1, 111, 222, 26, 116, 180, 204, 102, 71, 187, 35, 44, 73, 218, 90,
                              55, 158, 206, 51, 198, 133, 119, 37, 175, 134, 172, 76, 180, 31, 98, 88];
const INSTALL_SH: &str = "curl -fsSL kesavsabari.github.io/qh/sh | sh";
const INSTALL_PS: &str = "irm kesavsabari.github.io/qh | iex";
const DAY: u64 = 86400;
const MAX_BINARY: usize = 64 << 20;

/// A newer version seen by the check, for the UI's notice.
static AVAILABLE: Mutex<Option<String>> = Mutex::new(None);

pub struct Release {
    pub version: String,
    assets: Vec<(String, u64, String)>, // name, size, sha256 hex
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Where releases come from, and the key they must be signed with. Debug builds take overrides
/// (QH_UPDATE_URL, QH_UPDATE_KEY as 64 hex characters) so the updater can be tested against a
/// local fake release; release builds ignore them.
fn source() -> (String, Vec<u8>) {
    #[cfg(debug_assertions)]
    if let (Ok(u), Ok(k)) = (std::env::var("QH_UPDATE_URL"), std::env::var("QH_UPDATE_KEY")) {
        let key = (0..k.len() / 2).filter_map(|i| u8::from_str_radix(k.get(2 * i..2 * i + 2)?, 16).ok()).collect();
        return (u, key);
    }
    (RELEASES.into(), PUBLIC_KEY.to_vec())
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut p = v.trim().trim_start_matches('v').split('.').map(|x| x.parse::<u64>().ok());
    Some((p.next()??, p.next()??, p.next()??))
}

/// `a` is a later version than `b`.
pub fn newer(a: &str, b: &str) -> bool {
    matches!((parse_version(a), parse_version(b)), (Some(x), Some(y)) if x > y)
}

fn fetch(url: &str, cap: usize, timeout: Duration) -> Result<Vec<u8>, String> {
    let r = crate::backend::http().get(url).timeout(timeout).call().map_err(|e| match e {
        ureq::Error::Status(404, _) => "not found".to_string(),
        ureq::Error::Status(c, _) => format!("HTTP {c}"),
        e => e.to_string(),
    })?;
    let mut body = vec![];
    std::io::Read::read_to_end(&mut std::io::Read::take(r.into_reader(), cap as u64 + 1), &mut body).map_err(|e| e.to_string())?;
    if body.len() > cap {
        return Err("download is larger than expected".into());
    }
    Ok(body)
}

/// Check a signed manifest and read it.
fn read_manifest(manifest: &[u8], sig: &[u8], key: &[u8]) -> Result<Release, String> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key).verify(manifest, sig)
        .map_err(|_| "the release's signature doesn't match qh's release key; not installing it")?;
    let v: Value = serde_json::from_slice(manifest).map_err(|_| "the release manifest is damaged")?;
    let version = v["version"].as_str().filter(|s| parse_version(s).is_some()).ok_or("the release manifest has no version")?;
    let assets = v["assets"].as_array().into_iter().flatten().filter_map(|a| {
        Some((a["name"].as_str()?.to_string(), a["size"].as_u64()?, a["sha256"].as_str()?.to_lowercase()))
    }).collect();
    Ok(Release { version: version.to_string(), assets })
}

/// The newest signed release.
pub fn latest(timeout: Duration) -> Result<Release, String> {
    let (base, key) = source();
    let manifest = fetch(&format!("{base}/qh-release.json"), 64 << 10, timeout)
        .map_err(|e| format!("couldn't read the latest release ({e})"))?;
    let sig = fetch(&format!("{base}/qh-release.json.sig"), 1024, timeout)
        .map_err(|e| format!("the latest release isn't signed yet ({e})"))?;
    read_manifest(&manifest, &sig, &key)
}

/// This machine's binary in a release.
fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "qh-linux-x86_64",
        ("macos", "aarch64") => "qh-macos-arm64",
        ("macos", "x86_64") => "qh-macos-x86_64",
        ("windows", "x86_64") => "qh-windows-x86_64.exe",
        _ => return None,
    })
}

/// The running binary, unless it was built from source (inside a cargo `target` folder).
fn installed_exe() -> Result<PathBuf, String> {
    #[cfg(debug_assertions)]
    if let Ok(p) = std::env::var("QH_UPDATE_EXE") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|e| format!("can't find the running qh: {e}"))?;
    let from_source = exe.parent().and_then(|d| d.parent()).and_then(|d| d.file_name()).is_some_and(|n| n == "target")
        || exe.parent().and_then(|d| d.parent()).and_then(|d| d.parent()).and_then(|d| d.file_name()).is_some_and(|n| n == "target");
    if from_source {
        return Err(format!("this qh was built from source ({}); update it with `git pull && cargo build --release`", exe.display()));
    }
    Ok(exe)
}

/// What the notice tells the user to run.
pub fn how() -> &'static str {
    static HOW: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    HOW.get_or_init(|| if installed_exe().is_ok() { "qh update" } else { "git pull && cargo build --release" })
}

fn reinstall() -> &'static str {
    if cfg!(windows) { INSTALL_PS } else { INSTALL_SH }
}

fn state_path() -> PathBuf {
    crate::config::state_dir().join("update-check.json")
}

fn remember(latest: &str) {
    let p = state_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
    let _ = std::fs::write(p, json!({"checked": now(), "latest": latest}).to_string());
}

/// A newer qh, if the last check found one.
pub fn available() -> Option<String> {
    crate::hub::lock(&AVAILABLE).clone()
}

fn set_available(v: &str) {
    *crate::hub::lock(&AVAILABLE) = newer(v, VERSION).then(|| v.to_string());
}

/// At start: show what the last check found at once, and check again if that was over a day ago.
/// Off when the config says so or QH_NO_UPDATE_CHECK is set.
pub fn check_in_background(enabled: bool) {
    cleanup();
    if !enabled || std::env::var_os("QH_NO_UPDATE_CHECK").is_some() {
        return;
    }
    let last: Value = std::fs::read_to_string(state_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if let Some(v) = last["latest"].as_str() {
        set_available(v);
    }
    if last["checked"].as_u64().unwrap_or(0) + DAY > now() {
        return;
    }
    std::thread::spawn(|| {
        // a failure (offline, not signed yet) stays quiet and is tried again next start
        if let Ok(r) = latest(Duration::from_secs(5)) {
            remember(&r.version);
            set_available(&r.version);
        }
    });
}

/// The swap leaves the old Windows binary next to the new one; it goes on the next start.
fn cleanup() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old.exe"));
    }
}

/// Put `new` where `exe` is. Unix: one rename (a new file, not an overwrite: macOS kills a binary
/// changed in place). Windows can't replace a running .exe but can rename it, so it steps aside first.
fn swap(new: &Path, exe: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let old = exe.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
        if let Err(e) = std::fs::rename(new, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    std::fs::rename(new, exe)
}

/// Run the downloaded binary's `--version` (10 s at most): it must start on this machine and be
/// the release it claims to be.
fn runs_as(bin: &Path, version: &str) -> Result<(), String> {
    let mut child = std::process::Command::new(bin).arg("--version").stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()
        .map_err(|e| format!("the new qh won't start on this machine ({e})"))?;
    let t0 = Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if t0.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            return Err("the new qh didn't answer `--version`".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut o, &mut out);
    }
    if out.trim() != format!("qh {version}") {
        return Err(format!("the new qh reports `{}`, not qh {version}", out.trim()));
    }
    Ok(())
}

/// `qh update`: install the newest signed release over this binary. Returns what happened.
pub fn run(say: &dyn Fn(&str)) -> Result<String, String> {
    let r = latest(Duration::from_secs(20))?;
    remember(&r.version);
    set_available(&r.version);
    if !newer(&r.version, VERSION) {
        return Ok(format!("qh {VERSION} is the latest"));
    }
    let name = asset_name().ok_or_else(|| format!("there's no prebuilt qh for this machine; build {} from source", r.version))?;
    let (_, size, hash) = r.assets.iter().find(|a| a.0 == name)
        .ok_or_else(|| format!("qh {} has no build for this machine yet", r.version))?.clone();
    let exe = installed_exe()?;
    let dir = exe.parent().ok_or("can't tell which folder qh is in")?;
    let tmp = dir.join(format!(".qh-update-{}{}", std::process::id(), if cfg!(windows) { ".exe" } else { "" }));
    say(&format!("downloading qh {} ({:.1} MB)…", r.version, size as f64 / 1e6));
    let (base, _) = source();
    let bin = fetch(&format!("{base}/{name}"), MAX_BINARY, Duration::from_secs(300)).map_err(|e| format!("download failed: {e}"))?;
    let got: String = sha256(&bin).iter().map(|b| format!("{b:02x}")).collect();
    if bin.len() as u64 != size || got != hash {
        return Err("the download doesn't match the signed release (size or hash); not installing it".into());
    }
    let result = (|| {
        write_exec(&tmp, &bin).map_err(|e| if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("qh can't write to {} (installed by an admin or a package manager?); reinstall with: {}", dir.display(), reinstall())
        } else {
            format!("couldn't save the download next to qh: {e}")
        })?;
        runs_as(&tmp, &r.version)?;
        swap(&tmp, &exe).map_err(|e| format!("couldn't replace {}: {e}; reinstall with: {}", exe.display(), reinstall()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    set_available(VERSION);
    Ok(format!("updated qh {VERSION} → {}; restart qh to use it", r.version))
}

fn write_exec(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o755);
    }
    std::io::Write::write_all(&mut o.open(path)?, body)
}

/// `qh update` from the shell.
pub fn cli() -> i32 {
    match run(&|s| println!("{s}")) {
        Ok(m) => { println!("{m}"); 0 }
        Err(e) => { eprintln!("✗ {e}"); 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("v1.0.0", "0.9.9"));
        assert!(!newer("0.1.4", "0.1.4"));
        assert!(!newer("0.1.3", "0.1.4"));
        assert!(!newer("junk", "0.1.0"));
    }

    #[test]
    fn manifest_needs_the_release_key() {
        // RFC 8032 test 1: key 9d61b19d…, public d75a9801…, signature over the empty message
        let hex = |s: &str| (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<u8>>();
        let public = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let sig = hex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b");
        assert!(read_manifest(b"", &sig, &public).is_err_and(|e| e.contains("damaged")), "signature checks, then the empty manifest is refused");
        assert!(read_manifest(b"{}", &sig, &public).is_err_and(|e| e.contains("signature")), "a changed manifest fails the signature");
        assert!(read_manifest(b"", &sig, &PUBLIC_KEY).is_err_and(|e| e.contains("signature")), "another key's signature fails");
    }

    #[test]
    fn swap_replaces_the_file() {
        let d = std::env::temp_dir().join(format!("qh-swap-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (exe, new) = (d.join(if cfg!(windows) { "qh.exe" } else { "qh" }), d.join(".qh-update"));
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        swap(&new, &exe).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert!(!new.exists());
        let _ = std::fs::remove_dir_all(&d);
    }
}
