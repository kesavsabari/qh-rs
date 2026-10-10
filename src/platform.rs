//! Everything that differs between Linux, macOS and Windows lives here, so the rest of the
//! harness can stay platform-blind.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

pub fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default()
}

/// Config folder: ~/.config/qwen-harness, or %APPDATA%\qwen-harness on Windows.
pub fn config_base() -> PathBuf {
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("APPDATA") {
            return PathBuf::from(a).join("qwen-harness");
        }
    }
    home().join(".config/qwen-harness")
}

/// State folder: ~/.local/state/qwen-harness, or %LOCALAPPDATA%\qwen-harness on Windows.
pub fn state_base() -> PathBuf {
    if cfg!(windows) {
        if let Some(a) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(a).join("qwen-harness");
        }
    }
    home().join(".local/state/qwen-harness")
}

fn on_path(exe: &str) -> bool {
    let names: Vec<String> = if cfg!(windows) { vec![format!("{exe}.exe"), exe.into()] } else { vec![exe.into()] };
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| names.iter().any(|n| d.join(n).is_file())))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shell {
    Bash,
    PowerShell,
}

/// Git Bash on Windows. Never `C:\Windows\System32\bash.exe`: that is the WSL launcher,
/// which fails outright on machines without a Linux distro installed.
fn git_bash() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)", "LOCALAPPDATA"].iter()
        .filter_map(std::env::var_os)
        .flat_map(|b| { let b = PathBuf::from(b); [b.join(r"Git\bin\bash.exe"), b.join(r"Programs\Git\bin\bash.exe")] })
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        for d in std::env::split_paths(&path) {
            if d.join("git.exe").is_file() {
                // …\Git\cmd\git.exe → …\Git\bin\bash.exe
                if let Some(root) = d.parent() { c.push(root.join(r"bin\bash.exe")) }
            }
            let lower = d.to_string_lossy().to_lowercase();
            if !lower.contains("system32") && !lower.contains("windowsapps") {
                c.push(d.join("bash.exe"));
            }
        }
    }
    c.into_iter().find(|p| p.is_file())
}

fn bash_exe() -> &'static Option<PathBuf> {
    static B: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    B.get_or_init(|| if cfg!(windows) { git_bash() } else { Some(PathBuf::from("bash")) })
}

/// bash wherever it really exists (always on Linux/macOS, Git Bash on Windows), else PowerShell.
/// QH_SHELL=powershell|bash overrides.
pub fn shell() -> Shell {
    static S: std::sync::OnceLock<Shell> = std::sync::OnceLock::new();
    *S.get_or_init(|| match std::env::var("QH_SHELL").map(|v| v.to_lowercase()).as_deref() {
        Ok("powershell") | Ok("pwsh") => Shell::PowerShell,
        Ok("bash") => Shell::Bash,
        _ => if bash_exe().is_some() { Shell::Bash } else { Shell::PowerShell },
    })
}

pub fn shell_name() -> &'static str {
    match shell() { Shell::Bash => "bash", Shell::PowerShell => "PowerShell" }
}

/// A command for `script` in the platform shell.
pub fn shell_cmd(script: &str) -> Command {
    shell_cmd_for(shell(), script)
}

pub fn shell_cmd_for(sh: Shell, script: &str) -> Command {
    match sh {
        Shell::Bash => {
            let mut c = Command::new(bash_exe().clone().unwrap_or_else(|| PathBuf::from("bash")));
            c.arg("-c").arg(script);
            c
        }
        Shell::PowerShell => {
            let exe = if on_path("pwsh") { "pwsh" } else { "powershell" };
            let mut c = Command::new(exe);
            c.args(["-NoProfile", "-NonInteractive", "-Command", script]);
            c
        }
    }
}

/// `script` followed by a line printing `mark` and the shell's final directory, keeping
/// the script's exit code.
pub fn with_cwd_report(script: &str, mark: &str) -> String {
    with_cwd_report_for(shell(), script, mark)
}

pub fn with_cwd_report_for(sh: Shell, script: &str, mark: &str) -> String {
    match sh {
        // pwd -W gives C:/… under Git Bash, where $PWD would be /c/…
        Shell::Bash if cfg!(windows) => format!("{script}\n__qh=$?; printf '\\n{mark}%s' \"$(pwd -W 2>/dev/null || pwd)\"; exit $__qh"),
        Shell::Bash => format!("{script}\n__qh=$?; printf '\\n{mark}%s' \"$PWD\"; exit $__qh"),
        Shell::PowerShell => format!("{script}\n$__qh = if ($LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }}\n\
[Console]::Out.Write(\"`n{mark}\" + (Get-Location).Path); exit $__qh"),
    }
}

/// Run to completion or kill after `secs`; stdout/stderr are drained on threads so a chatty
/// command can't deadlock on a full pipe.
pub fn run_timeout(mut cmd: Command, stdin: Option<&[u8]>, secs: u64, cwd: &Path) -> std::io::Result<(Output, bool)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // so a timeout can kill what the shell started, not just the shell
    }
    let mut child = cmd.current_dir(cwd).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    if let (Some(data), Some(mut i)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        let _ = i.write_all(data);
    }
    let drain = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut v = vec![];
            if let Some(mut r) = r {
                let mut buf = [0u8; 8192];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 { break }
                    v.extend_from_slice(&buf[..n]);
                    let _ = tx.send(Some(buf[..n].to_vec()));
                }
            }
            let _ = tx.send(None);
        });
        rx
    };
    let out = drain(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let t0 = Instant::now();
    let mut killed = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if t0.elapsed() > Duration::from_secs(secs) {
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-9", &format!("-{}", child.id())]).status();
            #[cfg(windows)]
            let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &child.id().to_string()]).output();
            let _ = child.kill();
            killed = true;
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    // a background process left running may hold the pipe open: take what's there and go
    let collect = |rx: std::sync::mpsc::Receiver<Option<Vec<u8>>>| {
        let mut v = vec![];
        let end = Instant::now() + Duration::from_millis(if killed { 300 } else { 2000 });
        while let Ok(Some(chunk)) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            v.extend(chunk);
        }
        v
    };
    Ok((Output { status, stdout: collect(out), stderr: collect(err) }, killed))
}

/// Git Bash's path style back to a Windows path: /c/Users/x → C:/Users/x, /tmp/x → %TEMP%/x.
/// None when it isn't one (and always on other systems).
pub fn from_msys(path: &str) -> Option<String> {
    if !cfg!(windows) || !path.starts_with('/') {
        return None;
    }
    let b = path.as_bytes();
    if b.len() >= 2 && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
        return Some(format!("{}:/{}", (b[1] as char).to_ascii_uppercase(), path.get(3..).unwrap_or("")));
    }
    let rest = path.strip_prefix("/tmp")?;
    (rest.is_empty() || rest.starts_with('/'))
        .then(|| format!("{}{rest}", std::env::temp_dir().display().to_string().trim_end_matches(['\\', '/'])))
}

pub const DOCS: &str = "https://kesavsabari.github.io/qh-rs/";

/// Open a URL in the default browser, best effort.
pub fn open_url(url: &str) {
    let _ = if cfg!(windows) {
        Command::new("cmd").args(["/C", "start", "", url]).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    } else {
        Command::new("xdg-open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    };
}

/// Read a line from the terminal without echoing it (API keys).
pub fn read_hidden() -> String {
    use ratatui::crossterm::event::{read, Event, KeyCode, KeyEventKind, KeyModifiers};
    use ratatui::crossterm::terminal;
    if terminal::enable_raw_mode().is_err() {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        return s;
    }
    let mut s = String::new();
    while let Ok(ev) = read() {
        let Event::Key(k) = ev else { continue };
        if k.kind == KeyEventKind::Release { continue }
        match k.code {
            KeyCode::Enter => break,
            KeyCode::Backspace => { s.pop(); }
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => { s.clear(); break }
            KeyCode::Char(c) => s.push(c),
            _ => {}
        }
    }
    let _ = terminal::disable_raw_mode();
    s
}

/// Local time as YYYYMMDD-HHMMSS. `date` where it exists (it knows the time zone); UTC otherwise.
pub fn stamp() -> String {
    if !cfg!(windows) {
        if let Ok(o) = Command::new("date").arg("+%Y%m%d-%H%M%S").output() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.len() == 15 { return s }
        }
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Chromium-family browsers to try, by name on PATH and at their usual install paths.
pub fn browsers() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["chromium", "google-chrome-stable", "google-chrome", "brave", "microsoft-edge"]
        .iter().map(PathBuf::from).collect();
    if cfg!(target_os = "macos") {
        for app in ["Google Chrome", "Chromium", "Brave Browser", "Microsoft Edge"] {
            v.push(PathBuf::from(format!("/Applications/{app}.app/Contents/MacOS/{app}")));
        }
    }
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"].iter().filter_map(std::env::var_os) {
            let b = PathBuf::from(base);
            v.push(b.join(r"Google\Chrome\Application\chrome.exe"));
            v.push(b.join(r"BraveSoftware\Brave-Browser\Application\brave.exe"));
            v.push(b.join(r"Microsoft\Edge\Application\msedge.exe"));
        }
    }
    v
}

/// Write a file only the user can read (API keys).
pub fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    // Windows: files under %APPDATA% already inherit a per-user ACL
    o.open(path)?.write_all(body)
}

/// Unified-style diff of two texts: `diff -u` when it exists, else a small LCS diff.
pub fn diff(label: &str, before: Option<&str>, after: Option<&str>) -> String {
    let (a, b) = (before.unwrap_or(""), after.unwrap_or(""));
    if a == b { return String::new() }
    let (x, y): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let mut out = format!("--- {label}\n+++ {label}\n");
    if x.len() * y.len() > 4_000_000 {
        return out + &format!("(too large to diff here: {} → {} lines)\n", x.len(), y.len());
    }
    let mut t = vec![vec![0u32; y.len() + 1]; x.len() + 1];
    for i in (0..x.len()).rev() {
        for j in (0..y.len()).rev() {
            t[i][j] = if x[i] == y[j] { t[i + 1][j + 1] + 1 } else { t[i + 1][j].max(t[i][j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut rows: Vec<String> = vec![];
    while i < x.len() || j < y.len() {
        if i < x.len() && j < y.len() && x[i] == y[j] { rows.push(format!(" {}", x[i])); i += 1; j += 1 }
        else if i < x.len() && (j == y.len() || t[i + 1][j] >= t[i][j + 1]) { rows.push(format!("-{}", x[i])); i += 1 }
        else { rows.push(format!("+{}", y[j])); j += 1 }
    }
    // keep changed lines plus 3 lines of context around them
    let near = |k: usize| rows[k.saturating_sub(3)..(k + 4).min(rows.len())].iter().any(|r| !r.starts_with(' '));
    let mut gap = false;
    for k in 0..rows.len() {
        if near(k) { out += &rows[k]; out.push('\n'); gap = false }
        else if !gap { out += "@@\n"; gap = true }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_shape_and_diff() {
        assert_eq!(stamp().len(), 15);
        let d = diff("f", Some("a\nb\nc\n"), Some("a\nB\nc\n"));
        assert!(d.contains("-b\n+B\n") && d.contains(" a\n"), "{d}");
        assert!(diff("f", Some("x"), Some("x")).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn powershell_reports_its_folder() {
        let d = std::env::temp_dir();
        let script = with_cwd_report_for(Shell::PowerShell, "Set-Location ..; Write-Output hi", "MARK:");
        let (o, _) = run_timeout(shell_cmd_for(Shell::PowerShell, &script), None, 60, &d).unwrap();
        let out = String::from_utf8_lossy(&o.stdout);
        let dir = out.rsplit("MARK:").next().unwrap().trim();
        assert!(out.contains("hi") && o.status.success(), "{out}");
        assert_eq!(std::path::Path::new(dir).canonicalize().unwrap(), d.parent().unwrap().canonicalize().unwrap());
    }

    #[test]
    fn timeout_kills_and_output_survives() {
        let (o, killed) = run_timeout(shell_cmd("echo hi; sleep 5"), None, 1, &std::env::temp_dir()).unwrap();
        assert!(killed && String::from_utf8_lossy(&o.stdout).contains("hi"));
    }
}
