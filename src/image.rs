//! Images the user attaches to a message: from the clipboard (Ctrl+V), a pasted file path, or
//! `/image <path>`. An image travels as a base64 string in the message's `images` array (the same
//! field browser screenshots use), so it is saved with the session and `--resume` resends it.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};

/// Larger images are refused; downscaling is not done.
pub const MAX: usize = 5 * 1024 * 1024;
const EXTS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// Images waiting for a session's next user message, by session name.
static PENDING: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Default::default);

pub fn stage(session: &str, imgs: Vec<String>) {
    if !imgs.is_empty() {
        crate::hub::lock(&PENDING).entry(session.to_string()).or_default().extend(imgs);
    }
}

pub fn take(session: &str) -> Vec<String> {
    crate::hub::lock(&PENDING).remove(session).unwrap_or_default()
}

/// Standard base64 with padding.
pub fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// The media type of base64 image data, from its first bytes; PNG when unknown (screenshots).
pub fn mime(b64: &str) -> &'static str {
    if b64.starts_with("/9j/") { "image/jpeg" }
    else if b64.starts_with("R0lGOD") { "image/gif" }
    else if b64.starts_with("UklGR") { "image/webp" }
    else { "image/png" }
}

fn sniff(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG") || b.starts_with(b"\xff\xd8\xff") || b.starts_with(b"GIF8")
        || (b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

/// Checks and encodes raw image bytes.
pub fn encode(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX {
        return Err(format!("image is {}, over the 5 MB limit; shrink it first", size(bytes.len())));
    }
    if !sniff(bytes) {
        return Err("not a PNG, JPEG, WebP or GIF image".into());
    }
    Ok(b64(bytes))
}

pub fn load(p: &Path) -> Result<String, String> {
    let len = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?.len() as usize;
    if len > MAX {
        return Err(format!("{} is {}, over the 5 MB limit; shrink it first", p.display(), size(len)));
    }
    encode(&std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?)
}

pub fn size(n: usize) -> String {
    if n >= 1024 * 1024 { format!("{:.1} MB", n as f64 / 1048576.0) } else { format!("{} KB", n.div_ceil(1024)) }
}

/// Decoded size of base64 data, for the chip.
pub fn bytes_of(b64: &str) -> usize {
    b64.len() / 4 * 3 - b64.bytes().rev().take_while(|&c| c == b'=').count()
}

/// A pasted text that is just the path of an existing image file (quotes, `file://` and `~`
/// allowed, as file managers and terminals paste them).
pub fn path_in(text: &str, cwd: &Path) -> Option<PathBuf> {
    let t = text.trim().trim_matches(|c| c == '\'' || c == '"');
    let t = t.strip_prefix("file://").unwrap_or(t);
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    let t = t.replace("\\ ", " ");
    let p = match t.strip_prefix("~/") {
        Some(rest) => std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)?.join(rest),
        None => cwd.join(&t),
    };
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    (EXTS.contains(&ext.as_str()) && p.is_file()).then_some(p)
}

fn run(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    (o.status.success() && !o.stdout.is_empty()).then_some(o.stdout)
}

/// The clipboard's image, if it holds one. Shells out to the platform's own tools.
pub fn clipboard() -> Option<Vec<u8>> {
    if cfg!(target_os = "macos") {
        if let Some(b) = run("pngpaste", &["-"]) {
            return Some(b);
        }
        let tmp = std::env::temp_dir().join(format!("qh-clip-{}.png", std::process::id()));
        let script = format!("set f to open for access POSIX file \"{}\" with write permission\n\
            write (the clipboard as «class PNGf») to f\nclose access f", tmp.display());
        run("osascript", &["-e", &script]);
        let b = std::fs::read(&tmp).ok().filter(|b| !b.is_empty());
        let _ = std::fs::remove_file(&tmp);
        b
    } else if cfg!(windows) {
        let ps = "Add-Type -AssemblyName System.Windows.Forms,System.Drawing;\
            $i=[System.Windows.Forms.Clipboard]::GetImage(); if($i){$m=New-Object IO.MemoryStream;\
            $i.Save($m,[System.Drawing.Imaging.ImageFormat]::Png);[Convert]::ToBase64String($m.ToArray())}";
        let out = run("powershell", &["-NoProfile", "-STA", "-Command", ps])?;
        let b = crate::oauth::b64url_decode(String::from_utf8_lossy(&out).trim());
        (!b.is_empty()).then_some(b)
    } else {
        run("wl-paste", &["--no-newline", "--type", "image/png"])
            .or_else(|| run("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]))
    }
}

/// Whether the model probably reads images. Ollama reports it; elsewhere guess from the name.
pub fn likely_vision(provider: &str, model: &str) -> bool {
    if let Some(p) = crate::providers::resolve(provider).filter(|p| p.kind == crate::providers::Kind::Ollama) {
        return crate::tier::caps(&p.base, model).vision;
    }
    let m = model.to_ascii_lowercase();
    if ["vl", "vision", "llava", "gemma3", "pixtral", "4o", "gpt-4.1", "gpt-5", "claude", "gemini"].iter().any(|k| m.contains(k)) {
        return true;
    }
    !["deepseek", "coder", "qwq", "llama-3", "llama3", "mistral-7b", "gpt-oss", "kimi", "glm-4.5"].iter().any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(bytes_of(&b64(b"hello")), 5);
    }

    #[test]
    fn mime_comes_from_the_data() {
        assert_eq!(mime(&b64(b"\x89PNG\r\n\x1a\n")), "image/png");
        assert_eq!(mime(&b64(b"\xff\xd8\xff\xe0")), "image/jpeg");
        assert_eq!(mime(&b64(b"GIF89a")), "image/gif");
        assert_eq!(mime(&b64(b"RIFF\0\0\0\0WEBPVP8 ")), "image/webp");
    }

    #[test]
    fn refuses_big_or_non_images() {
        assert!(encode(b"hello").is_err());
        let mut big = b"\x89PNG".to_vec();
        big.resize(MAX + 1, 0);
        assert!(encode(&big).unwrap_err().contains("5 MB"));
        assert!(encode(b"\x89PNG\r\n").is_ok());
    }

    #[test]
    fn a_pasted_image_path_is_recognised() {
        let d = std::env::temp_dir().join(format!("qh-img-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("shot one.PNG"), b"\x89PNG").unwrap();
        std::fs::write(d.join("notes.txt"), b"x").unwrap();
        let full = d.join("shot one.PNG");
        assert_eq!(path_in(&format!("'{}'", full.display()), &d), Some(full.clone()));
        assert_eq!(path_in(&format!("file://{}\n", full.display()), &d), Some(full.clone()));
        assert_eq!(path_in("shot\\ one.PNG", &d), Some(full));
        assert_eq!(path_in("notes.txt", &d), None);
        assert_eq!(path_in("missing.png", &d), None);
        assert_eq!(path_in("look at shot one.PNG please", &d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn staged_images_go_to_the_next_message_once() {
        stage("t-img", vec!["a".into()]);
        stage("t-img", vec!["b".into()]);
        assert_eq!(take("t-img"), vec!["a", "b"]);
        assert!(take("t-img").is_empty());
    }
}
