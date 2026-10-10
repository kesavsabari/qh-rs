//! Built-in tools. Each takes JSON args and returns text for the model.
use crate::backend::http;
use crate::browser;
use crate::config::Config;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

thread_local! {
    /// Size cap for one tool result on this thread; set per call from the session's window.
    static OUT_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(6000) };
}

fn max_chars() -> usize {
    OUT_CAP.with(|c| c.get())
}
const OUTLINE_OVER: usize = 300; // lines; bigger files return an outline unless a range is given

pub fn cap(text: String) -> String {
    let max = max_chars();
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    // end on a whole line so the model never sees half of one
    let cut = text[..cut].rfind('\n').filter(|&i| i > max / 2).unwrap_or(cut);
    let (shown, total) = (text[..cut].matches('\n').count() + 1, text.matches('\n').count() + 1);
    format!("{}\n…output truncated: showed {shown} of {total} lines ({cut} of {} chars)", &text[..cut], text.len())
}

/// File errors name the path and where relative paths resolve, so a model
/// that guessed an absolute path can correct itself.
fn ferr(cwd: &Path, path: &str, e: std::io::Error) -> String {
    format!("{path}: {e}. The working directory is {}; use a path relative to it (e.g. answer.txt)", cwd.display())
}

/// A path argument resolved against the session's working directory (`~` expanded).
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let msys = crate::platform::from_msys(path);
    let path = msys.as_deref().unwrap_or(path);
    let p = if let Some(r) = path.strip_prefix('~') {
        crate::config::home().join(r.trim_start_matches(['/', '\\']))
    } else {
        PathBuf::from(path)
    };
    if p.is_absolute() { p } else { cwd.join(p) }
}

fn sa<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// The path to use for a file argument. A model told about its working directory
/// sometimes drops the leading slash ("tmp/x/f.py" for "/tmp/x/f.py"), which would
/// silently create a nested copy of the directory; map that back.
fn fix_path(cwd: &Path, path: &str) -> String {
    if cfg!(unix) && !path.starts_with('/') && !path.starts_with('~') && !path.is_empty() {
        let abs = format!("/{path}");
        if Path::new(&abs).starts_with(cwd) && !cwd.join(path).exists() {
            return abs;
        }
    }
    resolve(cwd, path).display().to_string()
}

fn ia(a: &Value, k: &str, d: i64) -> i64 {
    a.get(k).and_then(|v| v.as_i64()).unwrap_or(d)
}

static DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:async\s+)?(def|class|fn|pub fn|pub\(crate\) fn|struct|pub struct|enum|pub enum|impl|function|export function|export default function|interface|type)\s+[A-Za-z_]").unwrap()
});

fn outline(path: &str, lines: &[&str]) -> String {
    let defs: Vec<String> = lines.iter().enumerate().filter(|(_, l)| DEF.is_match(l))
        .map(|(i, l)| format!("{}\t{}", i + 1, l.trim_end())).collect();
    let body = if defs.is_empty() {
        lines.iter().take(60).enumerate().map(|(i, l)| format!("{}\t{l}", i + 1)).collect::<Vec<_>>().join("\n")
    } else {
        defs.join("\n")
    };
    cap(format!("OUTLINE ONLY of {path} ({} lines): names, no code. You have NOT read this file. \
Call read_file with start/end before describing, quoting or changing it.\n{body}", lines.len()))
}

fn read_file(cwd: &Path, a: &Value) -> Result<String, String> {
    let path = &fix_path(cwd, sa(a, "path"));
    let text = std::fs::read_to_string(path).map_err(|e| ferr(cwd, sa(a, "path"), e))?;
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = (ia(a, "start", 1).max(1) as usize, ia(a, "end", 0).max(0) as usize);
    if start == 1 && end == 0 && lines.len() > OUTLINE_OVER {
        return Ok(outline(path, &lines));
    }
    if start > lines.len() {
        return Err(format!("{path} has only {} lines", lines.len()));
    }
    let end = if end == 0 { lines.len() } else { end.min(lines.len()) };
    // whole lines up to the size budget, then say exactly where to resume
    let (mut out, mut last) = (String::new(), start - 1);
    for i in start..=end {
        let row = format!("{i}\t{}\n", lines[i - 1]);
        if !out.is_empty() && out.len() + row.len() > max_chars() {
            break;
        }
        out.push_str(&row);
        last = i;
    }
    if last < end {
        out += &format!("…showing lines {start}-{last} of {}; call read_file with start={} to continue", lines.len(), last + 1);
    }
    Ok(out.trim_end().to_string())
}

/// Appended to edit/write results so the model sees breakage immediately.
fn check_syntax(path: &str) -> String {
    if !path.ends_with(".py") {
        return String::new();
    }
    match Command::new("python").args(["-m", "py_compile", path]).output() {
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr);
            format!("\nsyntax error:\n{}", err.trim().lines().last().unwrap_or("?"))
        }
        _ => String::new(),
    }
}

fn write_file(cwd: &Path, a: &Value) -> Result<String, String> {
    let (path, content) = (&fix_path(cwd, sa(a, "path")), sa(a, "content"));
    if let Some(d) = Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(d).map_err(|e| ferr(cwd, path, e))?;
    }
    std::fs::write(path, content).map_err(|e| ferr(cwd, path, e))?;
    Ok(format!("wrote {} chars to {path}{}", content.chars().count(), check_syntax(path)))
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Find `old` ignoring indentation and trailing whitespace: (start, end, indent delta) for a unique match.
fn fuzzy_span(text: &str, old: &str) -> Option<(usize, usize, isize)> {
    let lines: Vec<&str> = text.lines().collect();
    let old = old.trim_matches('\n');
    let want: Vec<&str> = old.lines().map(str::trim).collect();
    if want.is_empty() || want.len() > lines.len() {
        return None;
    }
    let hits: Vec<usize> = (0..=lines.len() - want.len())
        .filter(|&i| lines[i..i + want.len()].iter().map(|l| l.trim()).eq(want.iter().copied())).collect();
    if hits.len() != 1 {
        return None;
    }
    let i = hits[0];
    let first = old.lines().next().unwrap_or("");
    Some((i, i + want.len(), indent(lines[i]) as isize - indent(first) as isize))
}

fn edit_file(cwd: &Path, a: &Value) -> Result<String, String> {
    let (path, old, new) = (&fix_path(cwd, sa(a, "path")), sa(a, "old"), sa(a, "new"));
    let text = std::fs::read_to_string(path).map_err(|e| ferr(cwd, sa(a, "path"), e))?;
    let n = if old.is_empty() { 0 } else { text.matches(old).count() };
    if n == 1 {
        std::fs::write(path, text.replacen(old, new, 1)).map_err(|e| e.to_string())?;
        return Ok(format!("edited {path}{}", check_syntax(path)));
    }
    if n > 1 {
        return Ok(format!("error: old string found {n} times; include more surrounding lines so it is unique"));
    }
    let Some((s, e, delta)) = fuzzy_span(&text, old) else {
        return Ok("error: old string not found. read_file the exact lines and copy them verbatim".into());
    };
    let pad = |l: &str| -> String {
        if l.trim().is_empty() {
            l.to_string()
        } else if delta >= 0 {
            " ".repeat(delta as usize) + l
        } else {
            l[(-delta as usize).min(indent(l))..].to_string()
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines[..s].iter().map(|l| l.to_string()).collect();
    out.extend(new.trim_matches('\n').lines().map(pad));
    out.extend(lines[e..].iter().map(|l| l.to_string()));
    let mut out = out.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(format!("edited {path} (matched lines {}-{e} ignoring whitespace){}", s + 1, check_syntax(path)))
}

fn remember(cwd: &Path, a: &Value) -> Result<String, String> {
    use std::io::Write;
    let (path, fact) = (cwd.join("NOTES.md"), sa(a, "fact").trim());
    let norm = |s: &str| s.trim_start_matches("- ").trim().to_lowercase();
    if std::fs::read_to_string(&path).unwrap_or_default().lines().any(|l| norm(l) == norm(fact)) {
        return Ok(format!("already in {}", path.display()));
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    writeln!(f, "- {fact}").map_err(|e| e.to_string())?;
    Ok(format!("saved to {}", path.display()))
}

const SKIP_DIRS: [&str; 7] = [".git", ".venv", "venv", "node_modules", "__pycache__", "dist", "build"];

pub fn source_files(cwd: &Path, root: &str) -> Vec<(PathBuf, String)> {
    let root = if root.is_empty() { cwd.to_path_buf() } else { resolve(cwd, root) };
    let git = Command::new("git").arg("ls-files").current_dir(&root).output();
    let files: Vec<PathBuf> = match git {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout).lines().map(|f| root.join(f)).collect()
        }
        _ => {
            let mut out = vec![];
            let mut stack = vec![root.clone()];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let p = e.path();
                    let name = e.file_name();
                    if p.is_dir() {
                        if !SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                            stack.push(p);
                        }
                    } else {
                        out.push(p);
                    }
                }
            }
            out
        }
    };
    files.into_iter().filter_map(|f| {
        let md = std::fs::metadata(&f).ok()?;
        if md.len() >= 1_000_000 {
            return None;
        }
        let t = std::fs::read_to_string(&f).ok()?;
        Some((f, t))
    }).collect()
}

fn word(name: &str) -> Result<Regex, String> {
    Regex::new(&format!(r"\b{}\b", regex::escape(name))).map_err(|e| e.to_string())
}

fn find_references(cwd: &Path, a: &Value) -> Result<String, String> {
    let pat = word(sa(a, "name"))?;
    let mut hits = vec![];
    for (f, t) in source_files(cwd, sa(a, "path")) {
        for (i, l) in t.lines().enumerate().filter(|(_, l)| pat.is_match(l)) {
            hits.push(format!("{}:{}: {}", f.display(), i + 1, l.trim()));
        }
    }
    Ok(cap(if hits.is_empty() { "no references".into() } else { hits.join("\n") }))
}

fn rename_symbol(cwd: &Path, a: &Value) -> Result<String, String> {
    let (old, new) = (sa(a, "old"), sa(a, "new"));
    let pat = word(old)?;
    let mut changed = vec![];
    for (f, t) in source_files(cwd, sa(a, "path")) {
        let n = pat.find_iter(&t).count();
        if n > 0 {
            std::fs::write(&f, pat.replace_all(&t, regex::NoExpand(new)).as_ref()).map_err(|e| e.to_string())?;
            changed.push(format!("{} ({n}){}", f.display(), check_syntax(&f.to_string_lossy())));
        }
    }
    Ok(if changed.is_empty() { format!("no occurrences of {old}") } else { format!("renamed in:\n{}", changed.join("\n")) })
}

const SAFE: [&str; 19] = ["cd", "ls", "pwd", "cat", "head", "tail", "wc", "tree", "file", "stat", "which", "echo",
                          "grep", "rg", "sort", "uniq", "du", "df", "date"];
const SAFE_GIT: [&str; 5] = ["status", "diff", "log", "show", "branch"];

static CD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^\s*cd\s+("[^"]*"|'[^']*'|[^\s;&|]+)\s*(?:&&\s*(.*))?$"#).unwrap()
});

/// 'cd X' or 'cd X && rest' -> (X, rest), for deciding whether the rest is read-only.
fn split_cd(cmd: &str) -> (Option<String>, String) {
    match CD.captures(cmd) {
        Some(c) => (Some(c[1].trim_matches(|ch| ch == '"' || ch == '\'').to_string()),
                    c.get(2).map_or("", |m| m.as_str()).trim().to_string()),
        None => (None, cmd.to_string()),
    }
}

static DEVNULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*[12&]?>\s*/dev/null|\s*2>&1").unwrap());
static SUBST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[<>`]|\$\(").unwrap());
static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|\||&&|[;|]").unwrap());
static FIND_BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-(exec|execdir|delete|ok|fprint)").unwrap());

/// The simple commands in a chain (`cd x && cargo check | tail`), or None when the command uses
/// substitution, redirection (other than to /dev/null) or a background `&`, which no rule may cover.
pub fn segments(cmd: &str) -> Option<Vec<String>> {
    let c = DEVNULL.replace_all(cmd, "");
    if SUBST.is_match(&c) || c.replace("&&", "").contains('&') || c.contains('\n') {
        return None;
    }
    Some(SEP.split(&c).map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
}

/// Read-only commands that run without asking. Chaining, redirection or
/// substitution still asks.
pub fn is_safe(cmd: &str) -> bool {
    let (_, rest) = split_cd(cmd);
    if rest.is_empty() {
        return true;
    }
    let rest = DEVNULL.replace_all(&rest, "");
    if SUBST.is_match(&rest) {
        return false;
    }
    SEP.split(&rest).all(|p| safe_simple(p.trim()))
}

fn safe_simple(rest: &str) -> bool {
    if rest.is_empty() || rest.contains('&') {
        return false;
    }
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words[0] {
        "git" => words.len() > 1 && SAFE_GIT.contains(&words[1]),
        "sort" => !words.iter().any(|w| w.starts_with("-o") || w.starts_with("--output")),
        "find" => !FIND_BAD.is_match(rest),
        w => SAFE.contains(&w) || w == "whoami",
    }
}

const CWD_MARK: &str = "\x1eQHCWD:";

/// Run in the session's folder. The shell reports its final directory, so any `cd` in the
/// command (first, last or in the middle of a chain) carries over to the next call, for this
/// session only.
/// "mkdir -p tmp/x/lib" run in /tmp/x means /tmp/x/lib: a model told its folder sometimes drops
/// the leading slash, which would quietly create a nested copy. Put it back.
fn fix_slash(cwd: &Path, cmd: &str) -> String {
    let full = cwd.display().to_string();
    let bare = full.trim_start_matches('/');
    if bare.is_empty() || !full.starts_with('/') || cwd.join(bare).exists() {
        return cmd.to_string();
    }
    let re = Regex::new(&format!(r#"(^|[\s'"=]){}\b"#, regex::escape(bare))).unwrap();
    re.replace_all(cmd, format!("${{1}}{}", regex::escape(&full).replace('$', "$$")).as_str()).to_string()
}

fn bash(cwd: &mut PathBuf, a: &Value) -> Result<String, String> {
    let cmd = &fix_slash(cwd, sa(a, "cmd"));
    if !cwd.is_dir() {
        *cwd = crate::config::home();
    }
    let script = crate::platform::with_cwd_report(cmd, CWD_MARK);
    let (o, killed) = crate::platform::run_timeout(crate::platform::shell_cmd(&script), None, 120, cwd)
        .map_err(|e| e.to_string())?;
    let killed = if killed { "\n(killed after 120 s; run long or never-ending commands in the background)" } else { "" };
    let mut out = String::from_utf8_lossy(&o.stdout).to_string();
    let mut moved = String::new();
    if let Some(i) = out.rfind(CWD_MARK) {
        let dir = PathBuf::from(out[i + CWD_MARK.len()..].trim_end());
        out.truncate(i);
        if out.ends_with('\n') {
            out.pop();
        }
        if dir.is_dir() && dir != *cwd {
            moved = format!("\n(cwd is now {})", dir.display());
            *cwd = dir;
        }
    }
    Ok(cap(format!("exit {}\n{out}{}{moved}{killed}", o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stderr))))
}

fn grep(cwd: &Path, a: &Value) -> Result<String, String> {
    let pattern = sa(a, "pattern");
    let pat = Regex::new(pattern).or_else(|_| Regex::new(&regex::escape(pattern))).map_err(|e| e.to_string())?;
    let path = sa(a, "path");
    let file = resolve(cwd, path);
    let files = if !path.is_empty() && file.is_file() {
        vec![(file.clone(), std::fs::read_to_string(&file).map_err(|e| e.to_string())?)]
    } else {
        source_files(cwd, path)
    };
    let mut hits = vec![];
    for (f, t) in &files {
        for (i, l) in t.lines().enumerate().filter(|(_, l)| pat.is_match(l)) {
            hits.push(format!("{}:{}: {}", f.display(), i + 1, l.trim_end()));
        }
    }
    Ok(cap(if hits.is_empty() { "no matches".into() } else { hits.join("\n") }))
}

/// Visible text of an HTML page: tags dropped, script/style/etc skipped.
fn html_text(html: &str) -> String {
    const SKIP: [&str; 5] = ["script", "style", "noscript", "svg", "head"];
    let (mut out, mut depth, mut rest) = (String::new(), 0usize, html);
    while let Some(lt) = rest.find('<') {
        if depth == 0 {
            out.push_str(&rest[..lt]);
        }
        rest = &rest[lt + 1..];
        let gt = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..gt];
        let closing = tag.starts_with('/');
        let name: String = tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>().to_ascii_lowercase();
        if SKIP.contains(&name.as_str()) && !tag.ends_with('/') {
            if closing { depth = depth.saturating_sub(1) } else { depth += 1 }
        }
        if matches!(name.as_str(), "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4") {
            out.push('\n');
        }
        rest = &rest[(gt + 1).min(rest.len())..];
    }
    if depth == 0 {
        out.push_str(rest);
    }
    let out = out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
        .replace("&#39;", "'").replace("&nbsp;", " ");
    static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
    static NL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n+").unwrap());
    NL.replace_all(&WS.replace_all(&out, " "), "\n\n").trim().to_string()
}

fn web_fetch(a: &Value) -> Result<String, String> {
    let r = http().get(sa(a, "url")).set("User-Agent", "qwen-harness").timeout(std::time::Duration::from_secs(20)).call();
    let r = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(e.to_string()),
    };
    let (status, html) = (r.status(), r.content_type().contains("html"));
    let body = r.into_string().map_err(|e| e.to_string())?;
    Ok(cap(format!("HTTP {status}\n{}", if html { html_text(&body) } else { body })))
}

fn webhook(url: &str, a: &Value) -> Result<String, String> {
    let r = http().post(url).set("Content-Type", "application/json").timeout(std::time::Duration::from_secs(120))
        .send_string(&json!({"input": sa(a, "input")}).to_string());
    let r = match r {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(e.to_string()),
    };
    let status = r.status();
    Ok(cap(format!("HTTP {status}\n{}", r.into_string().unwrap_or_default())))
}

pub fn schema(name: &str, desc: &str, props: &[(&str, &str)], required: &[&str]) -> Value {
    let p: serde_json::Map<String, Value> = props.iter().map(|(k, t)| (k.to_string(), json!({"type": t}))).collect();
    json!({"type": "function", "function": {"name": name, "description": desc,
           "parameters": {"type": "object", "properties": p, "required": required}}})
}

pub fn builtin_schema(name: &str) -> Option<Value> {
    Some(match name {
        "skill" => schema(name, "Load a skill's instructions by name (listed under Skills in the system prompt).", &[("name", "string")], &["name"]),
        "todo" => json!({"type": "function", "function": {"name": "todo",
            "description": "Keep a checklist for multi-step work. Send the whole list each time; status is pending, doing or done.",
            "parameters": {"type": "object", "required": ["items"], "properties": {"items": {"type": "array",
                "items": {"type": "object", "properties": {"text": {"type": "string"}, "status": {"type": "string"}}}}}}}}),
        "read_file" => schema(name, "Read a file with line numbers.", &[("path", "string"), ("start", "integer"), ("end", "integer")], &["path"]),
        "write_file" => schema(name, "Create or overwrite a file.", &[("path", "string"), ("content", "string")], &["path", "content"]),
        "edit_file" => schema(name, "Replace one exact unique string in a file.", &[("path", "string"), ("old", "string"), ("new", "string")], &["path", "old", "new"]),
        "find_references" => schema(name, "List every line using a name (whole word).", &[("name", "string"), ("path", "string")], &["name"]),
        "rename_symbol" => schema(name, "Rename a name everywhere in the project (whole word).", &[("old", "string"), ("new", "string"), ("path", "string")], &["old", "new"]),
        "remember" => schema(name, "Save a lasting project fact to NOTES.md for future sessions.", &[("fact", "string")], &["fact"]),
        "bash" => schema(name, if crate::platform::shell() == crate::platform::Shell::PowerShell {
            "Run a PowerShell command (Windows)." } else { "Run a shell command." }, &[("cmd", "string")], &["cmd"]),
        "grep" => schema(name, "Search files recursively for a regex.", &[("pattern", "string"), ("path", "string")], &["pattern"]),
        "web_fetch" => schema(name, "Fetch a URL as plain text.", &[("url", "string")], &["url"]),
        _ => return None,
    })
}

/// Commands that always ask, whatever the mode.
pub static DANGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\brm\b|--force|-f\b|\bdd\b|mkfs|chmod|chown|sudo|git (push|reset|clean)|>\s*/|curl.*\|\s*sh|wget.*\|\s*sh|\bkill(all)?\b|shutdown|reboot|remove-item|\bdel\b|\brd\b|rmdir|format-volume|stop-process|taskkill").unwrap()
});

/// Browser clicks that always ask: money or deleting things.
pub static COSTLY_CLICK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)pay|buy|purchase|checkout|order|delete|remove|unsubscribe|transfer|donate").unwrap()
});

pub const PROFILES: [&str; 4] = ["coding", "general", "browser", "all"];

/// Tool names for a profile; "*webhooks" expands to the configured hooks.
pub fn profile_tools(profile: &str) -> &'static [&'static str] {
    match profile {
        "general" => &["read_file", "bash", "web_fetch", "remember", "skill", "*browser", "*webhooks"],
        "browser" => &["read_file", "write_file", "remember", "*browser"],
        "all" => &["read_file", "write_file", "edit_file", "find_references", "rename_symbol", "remember", "todo", "skill",
                   "bash", "grep", "web_fetch", "*browser", "*webhooks"],
        _ => &["read_file", "write_file", "edit_file", "rename_symbol", "find_references", "bash", "grep", "remember", "todo", "skill", "*browser"],
    }
}

pub fn build(profile: &str, cfg: &Config) -> Vec<Value> {
    let mut out = vec![];
    for n in profile_tools(profile) {
        // tier S (under 16k of context): the browser's tools and page snapshots don't fit
        // a checklist costs more than it saves in a tier S window
        if *n == "todo" && crate::tier::tier(cfg.ctx) == "S" {
            continue;
        }
        if *n == "skill" && (crate::tier::tier(cfg.ctx) == "S" || crate::skills::find(&std::env::current_dir().unwrap_or_default()).is_empty()) {
            continue;
        }
        if *n == "*browser" && !crate::tier::Limits::for_ctx(cfg.ctx).browser && profile != "browser" {
            continue;
        }
        if *n == "*browser" {
            out.extend(browser::schemas());
        } else if *n == "*webhooks" {
            for h in &cfg.webhooks {
                out.push(schema(&h.name, &h.description, &[("input", "string")], &["input"]));
            }
        } else if let Some(s) = builtin_schema(n) {
            out.push(s);
        }
    }
    if profile != "browser" {
        out.extend(crate::mcp::schemas());
    }
    out
}

/// Needs the user's OK before running.
pub fn risky(name: &str, a: &Value, cfg: &Config) -> bool {
    static RISKY_LABEL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)submit|buy|pay|order|checkout|send|delete|remove|confirm|purchase|sign ?up|apply|post").unwrap()
    });
    match name {
        "write_file" | "edit_file" | "rename_symbol" | "type" => true,
        "bash" => !is_safe(sa(a, "cmd")),
        "click" => RISKY_LABEL.is_match(&browser::label(a)),
        _ if crate::mcp::is_mcp(name) => !crate::mcp::trusted(name),
        _ => cfg.webhooks.iter().any(|h| h.name == name && h.confirm),
    }
}

pub fn call(name: &str, a: &Value, cfg: &Config, cwd: &mut PathBuf, lim: &crate::tier::Limits) -> String {
    OUT_CAP.with(|c| c.set(lim.out_cap));
    if browser::TOOLS.contains(&name) {
        return browser::call(name, a, cfg.browser_headless, &cfg.browser_allow_hosts);
    }
    if crate::mcp::is_mcp(name) {
        return crate::mcp::call(name, a);
    }
    let r = match name {
        "skill" => Ok(crate::skills::load(cwd, sa(a, "name"))),
        "read_file" => read_file(cwd, a),
        "write_file" => write_file(cwd, a),
        "edit_file" => edit_file(cwd, a),
        "find_references" => find_references(cwd, a),
        "rename_symbol" => rename_symbol(cwd, a),
        "remember" => remember(cwd, a),
        "bash" => bash(cwd, a),
        "grep" => grep(cwd, a),
        "web_fetch" => web_fetch(a),
        _ => match cfg.webhooks.iter().find(|h| h.name == name) {
            Some(h) => webhook(&h.url, a),
            None => return format!("error: unknown tool {name}"),
        },
    };
    r.unwrap_or_else(|e| format!("error: {e}"))
}

#[cfg(test)]
mod cwd_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qh-test-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(d.join("a/b"));
        d
    }

    #[test]
    fn last_cd_sticks_and_only_for_this_session() {
        let root = tmp("cd");
        let (mut s1, mut s2) = (root.clone(), root.clone());
        let out = bash(&mut s1, &json!({"cmd": "cd a && mkdir -p c && cd c"})).unwrap();
        assert!(out.contains("cwd is now"), "{out}");
        assert_eq!(s1.canonicalize().unwrap(), root.join("a/c").canonicalize().unwrap());
        assert_eq!(s2, root, "another session's folder must not move");
        let out = bash(&mut s2, &json!({"cmd": "pwd"})).unwrap();
        // Git Bash prints its own path style; what matters is that it maps back to the same folder
        let shown = out.lines().nth(1).unwrap_or("").trim();
        assert_eq!(resolve(&s2, shown).canonicalize().unwrap(), root.canonicalize().unwrap(), "{out}");
        assert!(!out.contains("QHCWD"), "{out}");
        write_file(&s1, &json!({"path": "f.txt", "content": "x"})).unwrap();
        assert!(root.join("a/c/f.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn dropped_leading_slash_is_restored() {
        let d = PathBuf::from("/tmp/qhx1");
        assert_eq!(fix_slash(&d, "mkdir -p tmp/qhx1/lib && rm tmp/qhx1/u.py"), "mkdir -p /tmp/qhx1/lib && rm /tmp/qhx1/u.py");
        assert_eq!(fix_slash(&d, "cat /tmp/qhx1/a"), "cat /tmp/qhx1/a");
        assert_eq!(fix_slash(&d, "ls mytmp/qhx1"), "ls mytmp/qhx1");
    }

    #[test]
    fn exit_code_is_kept() {
        let mut d = tmp("exit");
        assert!(bash(&mut d, &json!({"cmd": "false"})).unwrap().starts_with("exit 1"));
    }

    #[test]
    fn notes_go_to_the_session_folder_once() {
        let d = tmp("notes");
        remember(&d, &json!({"fact": "uses axum"})).unwrap();
        let r = remember(&d, &json!({"fact": "Uses axum"})).unwrap();
        assert!(r.starts_with("already"));
        assert_eq!(std::fs::read_to_string(d.join("NOTES.md")).unwrap().lines().count(), 1);
    }
}
