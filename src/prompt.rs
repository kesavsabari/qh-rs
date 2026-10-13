use std::path::Path;

const SKIP: [&str; 13] = [".git", ".venv", "venv", "node_modules", "__pycache__", "dist", "build", ".qh",
                          ".mypy_cache", ".pytest_cache", ".ruff_cache", "target", ".next"];
const MARKERS: [&str; 7] = [".git", "pyproject.toml", "package.json", "Cargo.toml", "go.mod", "setup.py", "requirements.txt"];
const MAX_FILES: usize = 400;

/// File tree plus top-level defs, capped to a few hundred tokens.
fn repomap(root: &Path, max_chars: usize) -> String {
    if !MARKERS.iter().any(|m| root.join(m).exists()) {
        return String::new();
    }
    let ignore = gitignore(root);
    let mut files = vec![];
    walk(root, root, &ignore, &mut files);
    // a folder of many plain files (logs, fixtures, assets) becomes one line
    let mut by_dir: std::collections::BTreeMap<std::path::PathBuf, usize> = Default::default();
    for f in &files {
        *by_dir.entry(f.parent().unwrap_or(root).to_path_buf()).or_default() += 1;
    }
    let mut lines = vec![];
    let mut folded = std::collections::BTreeSet::new();
    for f in files.iter().take(MAX_FILES) {
        let dir = f.parent().unwrap_or(root).to_path_buf();
        let syms = symbols(f);
        if !is_code(f) && dir != root && by_dir[&dir] > 6 {
            if folded.insert(dir.clone()) {
                lines.push(format!("{}/ ({} files)", dir.strip_prefix(root).unwrap_or(&dir).display(), by_dir[&dir]));
            }
            continue;
        }
        let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
        lines.push(if syms.is_empty() { rel } else { format!("{rel}: {}", syms.join(", ")) });
    }
    let mut text = lines.join("\n");
    if text.len() > max_chars {
        let mut cut = max_chars;
        while !text.is_char_boundary(cut) { cut -= 1 }
        let keep = text[..cut].rfind('\n').unwrap_or(cut);
        text = format!("{}\n… {} files total", &text[..keep], lines.len());
    }
    text
}

/// Top-level .gitignore entries: plain names/paths and "*.ext" patterns, enough to keep build
/// output and logs out of the map.
fn gitignore(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default().lines()
        .map(|l| l.trim().trim_start_matches('/').trim_end_matches('/').to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .collect()
}

fn is_code(f: &Path) -> bool {
    const CODE: [&str; 22] = ["py", "rs", "js", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp",
                              "rb", "php", "swift", "cs", "lua", "sh", "vue", "svelte"];
    f.extension().is_some_and(|e| CODE.contains(&e.to_string_lossy().as_ref()))
}

fn ignored(rel: &Path, ignore: &[String]) -> bool {
    let r = rel.display().to_string();
    let name = rel.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    ignore.iter().any(|p| match p.strip_prefix("*.") {
        Some(ext) => name.ends_with(&format!(".{ext}")),
        None => !p.contains('*') && (r == *p || name == *p || r.starts_with(&format!("{p}/"))),
    })
}

fn walk(root: &Path, dir: &Path, ignore: &[String], out: &mut Vec<std::path::PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    let (mut dirs, mut names) = (vec![], vec![]);
    for e in entries {
        let n = e.file_name().to_string_lossy().to_string();
        if n.starts_with('.') || ignored(e.path().strip_prefix(root).unwrap_or(&e.path()), ignore) {
            continue;
        }
        if e.path().is_dir() {
            // CACHEDIR.TAG marks build output (any cargo target dir, whatever it is called)
            if !SKIP.contains(&n.as_str()) && !n.ends_with(".egg-info") && !e.path().join("CACHEDIR.TAG").exists() {
                dirs.push(e.path())
            }
        } else if !n.ends_with(".pyc") && !n.ends_with(".lock") {
            names.push(e.path());
        }
    }
    out.extend(names);
    for d in dirs {
        if out.len() > MAX_FILES { return }
        walk(root, &d, ignore, out);
    }
}

fn symbols(f: &Path) -> Vec<String> {
    if f.extension().is_none_or(|e| e != "py") || std::fs::metadata(f).map_or(true, |m| m.len() > 200_000) {
        return vec![];
    }
    let text = std::fs::read_to_string(f).unwrap_or_default();
    text.lines().filter_map(|l| {
        let l = l.strip_prefix("async ").unwrap_or(l);
        if let Some(r) = l.strip_prefix("def ") {
            Some(format!("{}()", r.split(['(', ':']).next()?.trim()))
        } else {
            l.strip_prefix("class ").and_then(|r| r.split(['(', ':']).next()).map(|s| s.trim().to_string())
        }
    }).take(12).collect()
}

/// The project's standing instructions: the first of QH.md, AGENTS.md, CLAUDE.md found from
/// the working folder up to the project root, else the user's global ~/.config/qh/QH.md.
fn instructions(cwd: &Path) -> Option<(String, String)> {
    const NAMES: [&str; 3] = ["QH.md", "AGENTS.md", "CLAUDE.md"];
    for dir in cwd.ancestors() {
        for n in NAMES {
            if let Ok(t) = std::fs::read_to_string(dir.join(n)) {
                return Some((dir.join(n).display().to_string(), t));
            }
        }
        if MARKERS.iter().any(|m| dir.join(m).exists()) || dir == crate::config::home() {
            break;
        }
    }
    let g = crate::config::config_dir().join("QH.md");
    std::fs::read_to_string(&g).ok().map(|t| (g.display().to_string(), t))
}

/// What the model would otherwise guess: the git branch, what's uncommitted, and the commands
/// that build and test this project (so the first test run is the right one).
fn facts(cwd: &Path) -> String {
    let git = |args: &[&str]| std::process::Command::new("git").current_dir(cwd).args(args)
        .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string());
    let mut out = vec![];
    if let Some(branch) = git(&["branch", "--show-current"]) {
        let changed: Vec<String> = git(&["status", "--porcelain"]).unwrap_or_default().lines()
            .filter_map(|l| l.get(3..)).map(|p| p.trim_matches('"').to_string()).collect();
        let shown: Vec<&str> = changed.iter().take(8).map(String::as_str).collect();
        let changes = match changed.len() {
            0 => "nothing uncommitted".to_string(),
            n => format!("{n} uncommitted: {}{}", shown.join(", "), if n > 8 { ", …" } else { "" }),
        };
        out.push(format!("Git: branch {}; {changes}.", if branch.is_empty() { "(detached)" } else { &branch }));
    }
    let cmds = commands(cwd);
    if !cmds.is_empty() {
        out.push(format!("Build and test with: {}.", cmds.join(", ")));
    }
    out.join("\n")
}

/// This project's build, test and lint commands, from its manifests.
fn commands(cwd: &Path) -> Vec<String> {
    let has = |f: &str| cwd.join(f).exists();
    let read = |f: &str| std::fs::read_to_string(cwd.join(f)).unwrap_or_default();
    let mut v: Vec<String> = vec![];
    if has("Cargo.toml") {
        v.extend(["cargo build".into(), "cargo test".into()]);
    }
    if has("go.mod") {
        v.extend(["go build ./...".into(), "go test ./...".into()]);
    }
    if has("package.json") {
        let pkg: serde_json::Value = serde_json::from_str(&read("package.json")).unwrap_or_default();
        let pm = if has("pnpm-lock.yaml") { "pnpm" } else if has("yarn.lock") { "yarn" } else if has("bun.lockb") || has("bun.lock") { "bun" } else { "npm" };
        for s in ["build", "test", "lint", "typecheck"] {
            if pkg["scripts"][s].is_string() {
                v.push(if s == "test" { format!("{pm} test") } else { format!("{pm} run {s}") });
            }
        }
    }
    if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        let py = read("pyproject.toml") + &read("setup.cfg");
        if py.contains("pytest") || has("pytest.ini") || has("tests") || has("conftest.py") {
            v.push("pytest".into());
        }
        if py.contains("ruff") {
            v.push("ruff check .".into());
        }
    }
    // plain Python tests with no manifest: test_*.py / *_test.py here or in tests/
    let tests: Vec<std::path::PathBuf> = [cwd.to_path_buf(), cwd.join("tests")].iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()))
        .filter(|p| p.file_name().map(|n| n.to_string_lossy().to_string())
            .is_some_and(|n| n.ends_with(".py") && (n.starts_with("test_") || n.ends_with("_test.py"))))
        .collect();
    if !v.iter().any(|c| c.starts_with("pytest")) && !tests.is_empty() {
        // unittest only finds TestCase classes; bare test_ functions need pytest (unittest would
        // find nothing and still say OK)
        let unittest = tests.iter().all(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("unittest")));
        v.push(if !unittest { "pytest" } else if cfg!(windows) { "python -m unittest" } else { "python3 -m unittest" }.into());
    }
    if has("Makefile") {
        for line in read("Makefile").lines() {
            if let Some(t) = line.split(':').next().filter(|t| matches!(*t, "build" | "test" | "lint" | "check")) {
                if !v.contains(&format!("make {t}")) {
                    v.push(format!("make {t}"));
                }
            }
        }
    }
    v
}

pub fn system(profile: &str, cwd: &Path, lim: &crate::tier::Limits) -> String {
    let mut text = if profile == "general" {
        format!("You are a terse personal assistant on the user's computer. Today is {}.
You have live tools: web_search searches the web, web_fetch reads any URL, bash runs commands. Use them for anything current
or online instead of saying you lack access. Answer directly when no tool is needed. Keep replies short.",
                crate::session::stamp().get(..8).unwrap_or(""))
    } else {
        format!("You are a terse coding agent working in {}.
Use tools to inspect before editing. Prefer edit_file over write_file.
Keep replies short. When done, say what changed in one or two lines.
past_sessions lists, searches and reads this folder's earlier sessions (\"last session\", \"what did we do yesterday\").", cwd.display())
    };
    if cfg!(windows) {
        text += &format!("\nThis is Windows; the bash tool runs {}. Use Windows paths.", crate::platform::shell_name());
    }
    // small models often state the answer and stop; the requested action is the job
    text += "\nIf the user asks you to write, save, create, send or change something, do it with a tool \
before replying; stating the answer in chat does not count.";
    if profile == "coding" {
        let f = facts(cwd);
        if !f.is_empty() {
            text += &format!("\n\n{f}");
        }
    }
    if profile == "coding" && lim.map_chars > 0 {
        let tree = repomap(cwd, lim.map_chars);
        if !tree.is_empty() {
            text += &format!("\n\nProject map:\n{tree}");
        }
    }
    if let Some((file, body)) = instructions(cwd) {
        text += &format!("\n\nProject instructions ({file}), follow them:\n{}", body.chars().take(lim.notes_chars).collect::<String>());
    }
    if lim.browser { // M and up: S windows can't spare the listing
        text += &crate::skills::listing(cwd);
    }
    if let Ok(notes) = std::fs::read_to_string(cwd.join("NOTES.md")) {
        text += &format!("\n\nProject notes:\n{}", notes.chars().take(lim.notes_chars).collect::<String>());
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_output_stays_out_of_the_map() {
        let d = std::env::temp_dir().join(format!("qh-map-{}", std::process::id()));
        std::fs::create_dir_all(d.join("out-dev/debug")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::write(d.join("main.rs"), "").unwrap();
        std::fs::write(d.join("out-dev/CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
        std::fs::write(d.join("out-dev/debug/qh"), "").unwrap();
        let map = repomap(&d, 4000);
        assert!(map.contains("main.rs") && !map.contains("out-dev"), "{map}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn facts_name_branch_changes_and_commands() {
        let d = std::env::temp_dir().join(format!("qh-facts-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts": {"test": "vitest", "build": "vite build"}}"#).unwrap();
        std::fs::write(d.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(d.join("Makefile"), "test:\n\tpnpm test\nlint: build\n").unwrap();
        let run = |a: &[&str]| assert!(std::process::Command::new("git").current_dir(&d).args(a).output().unwrap().status.success());
        run(&["init", "-q", "-b", "work"]);
        let f = facts(&d);
        assert!(f.contains("Git: branch work; 3 uncommitted: "), "{f}");
        assert!(f.contains("Build and test with: pnpm run build, pnpm test, make test, make lint."), "{f}");
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(facts(&std::env::temp_dir().join("qh-no-such-dir")), "");
        // a bare folder of Python files with unittest tests
        let d = std::env::temp_dir().join(format!("qh-facts-py-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("math_utils.py"), "").unwrap();
        std::fs::write(d.join("test_math_utils.py"), "import unittest\nclass T(unittest.TestCase): pass\n").unwrap();
        assert!(commands(&d)[0].ends_with(" -m unittest"));
        // bare test functions: unittest would find none
        std::fs::write(d.join("test_more.py"), "def test_x():\n    assert True\n").unwrap();
        assert_eq!(commands(&d), vec!["pytest".to_string()]);
        // a JavaScript tests/ folder is not Python
        let js = d.join("js");
        std::fs::create_dir_all(js.join("tests")).unwrap();
        std::fs::write(js.join("tests/app.test.js"), "").unwrap();
        assert!(commands(&js).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
