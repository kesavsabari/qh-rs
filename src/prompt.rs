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
            if !SKIP.contains(&n.as_str()) && !n.ends_with(".egg-info") { dirs.push(e.path()) }
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
/// the working folder up to the project root, else the user's global ~/.config/qwen-harness/QH.md.
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

pub fn system(profile: &str, cwd: &Path, lim: &crate::tier::Limits) -> String {
    let mut text = if profile == "general" {
        format!("You are a terse personal assistant on the user's computer. Today is {}.
You have live tools: web_fetch reads any URL, bash runs commands. Use them for anything current
or online instead of saying you lack access. Answer directly when no tool is needed. Keep replies short.",
                crate::session::stamp().get(..8).unwrap_or(""))
    } else {
        format!("You are a terse coding agent working in {}.
Use tools to inspect before editing. Prefer edit_file over write_file.
Keep replies short. When done, say what changed in one or two lines.", cwd.display())
    };
    if cfg!(windows) {
        text += &format!("\nThis is Windows; the bash tool runs {}. Use Windows paths.", crate::platform::shell_name());
    }
    // small models often state the answer and stop; the requested action is the job
    text += "\nIf the user asks you to write, save, create, send or change something, do it with a tool \
before replying; stating the answer in chat does not count.";
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
