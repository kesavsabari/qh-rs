//! First-time team setup from the terminal. `qh team create [friend…]` puts the current folder
//! on GitHub as a private repo if it isn't there yet and invites friends by GitHub username;
//! `qh join owner/repo` accepts the invite, downloads the project and turns team mode on.
//! Every step that changes something on GitHub is shown and confirmed first.
use crate::team::{api, auth_env, github_repo, remember, token};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn ask(q: &str) -> String {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    if matches!(std::io::stdin().read_line(&mut s), Ok(0) | Err(_)) {
        println!();
        std::process::exit(1);
    }
    s.trim().to_string()
}

fn yes(q: &str) -> bool {
    !matches!(ask(&format!("{q} [Y/n] ")).to_lowercase().as_str(), "n" | "no")
}

fn git(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Result<String, String> {
    let mut c = Command::new("git");
    c.current_dir(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
    for (k, v) in env { c.env(k, v); }
    let o = c.output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()) }
    else { Err(String::from_utf8_lossy(&o.stderr).trim().chars().take(300).collect()) }
}

fn looks_secret(name: &str) -> bool {
    let n = name.to_lowercase();
    n == ".env" || n.starts_with(".env.") || n.ends_with(".pem") || n.ends_with(".key") || n.starts_with("id_rsa")
        || n.starts_with("id_ed25519") || n.contains("credentials") || n == "secrets.toml"
}

/// Files under `dir` that look like secrets, relative paths (skips .git and build folders).
fn secrets(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, d: &Path, out: &mut Vec<String>, depth: usize) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if depth < 6 && !matches!(name.as_str(), ".git" | "node_modules" | "target" | ".venv" | "venv" | "dist" | "build") {
                    walk(root, &p, out, depth + 1);
                }
            } else if looks_secret(&name) {
                out.push(p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out, 0);
    // already ignored by git → not uploaded anyway
    if Path::new(&dir.join(".git")).exists() {
        out.retain(|f| git(dir, &["check-ignore", "-q", f], &[]).is_err());
    }
    out.sort();
    out
}

/// Your home folder (or anything above it) is never a project: it holds your keys and every
/// other project. Refuse before asking anything.
pub fn not_a_project(cwd: &Path) -> Result<(), String> {
    let home = crate::platform::home();
    let (c, h) = (cwd.canonicalize().unwrap_or(cwd.into()), home.canonicalize().unwrap_or(home));
    if h.starts_with(&c) {
        return Err(format!("{} is your home folder (or above it), not a project. Pick or make a project folder: /team create in qh opens a folder browser.", cwd.display()));
    }
    Ok(())
}

pub fn create(friends: &[String]) -> i32 {
    match create_inner(friends) {
        Ok(()) => 0,
        Err(e) => { eprintln!("✗ {e}"); 1 }
    }
}

/// Two questions and one confirmation; everything else has a default.
pub fn create_inner(friends: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    not_a_project(&cwd)?;
    let tok = token().ok_or("no GitHub login found: install the GitHub CLI and run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, me) = api("GET", "/user", &tok, None, None)?;
    let me = me["login"].as_str().ok_or("GitHub didn't say who you are")?.to_string();
    let existing = github_repo(&cwd).map(|(_, r)| r);

    let private = existing.is_some() || {
        println!("Who can see the code?\n  1) just your team (private)  ← Enter\n  2) everyone (public)");
        ask("> ") != "2"
    };
    let mut friends: Vec<String> = friends.to_vec();
    if friends.is_empty() {
        friends = ask("Friends' GitHub usernames (space-separated, Enter to skip): ").split_whitespace().map(String::from).collect();
    }
    let mut people = vec![];
    for f in &friends {
        match api("GET", &format!("/users/{f}"), &tok, None, None)? {
            (404, _, _) => println!("✗ no GitHub user {f}, skipping"),
            (_, _, u) => people.push((f.clone(), u["name"].as_str().filter(|n| !n.is_empty()).map(|n| format!("{n} ({f})")).unwrap_or(f.clone()))),
        }
    }

    // pick a free repo name from the folder's name
    let (repo, name) = match &existing {
        Some(r) => (r.clone(), String::new()),
        None => {
            let base: String = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("project".into())
                .chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
            let mut name = base.clone();
            for i in 2.. {
                if api("GET", &format!("/repos/{me}/{name}"), &tok, None, None)?.0 == 404 { break }
                name = format!("{base}-{i}");
            }
            (format!("{me}/{name}"), name)
        }
    };
    // no commits yet: an empty folder, or a create that stopped before its first push
    let unborn = git(&cwd, &["rev-parse", "-q", "--verify", "HEAD"], &[]).is_err();
    let secret_files = if existing.is_none() || unborn { secrets(&cwd) } else { vec![] };

    println!("\nReady to:");
    match &existing {
        Some(r) if unborn => println!("  • make this folder's first commit and push it to {r}"),
        Some(r) => println!("  • use {r}, where this project already is"),
        None => println!("  • put this folder on GitHub as {repo} ({})", if private { "private" } else { "PUBLIC: anyone can see the code" }),
    }
    if !secret_files.is_empty() {
        let shown: Vec<&str> = secret_files.iter().take(5).map(String::as_str).collect();
        let more = if secret_files.len() > 5 { format!(" and {} more", secret_files.len() - 5) } else { String::new() };
        println!("  • leave out {}{more} ({})", shown.join(", "), if secret_files.len() == 1 { "looks like a secret" } else { "look like secrets" });
    }
    for (_, shown) in &people {
        println!("  • invite {shown}");
    }
    println!("  • turn on team mode here");
    if !yes("Go?") {
        return Err("cancelled, nothing was changed".into());
    }

    if existing.is_none() {
        if git(&cwd, &["rev-parse", "--show-toplevel"], &[]).is_err() {
            git(&cwd, &["init", "-q", "-b", "main"], &[])?;
        }
        api("POST", "/user/repos", &tok, Some(&json!({"name": name, "private": private})), None)?;
        let url = format!("https://github.com/{repo}.git");
        if git(&cwd, &["remote", "get-url", "origin"], &[]).is_ok() {
            git(&cwd, &["remote", "set-url", "origin", &url], &[])?;
        } else {
            git(&cwd, &["remote", "add", "origin", &url], &[])?;
        }
    }
    if existing.is_none() || unborn {
        if !secret_files.is_empty() {
            let mut gi = std::fs::OpenOptions::new().create(true).append(true).open(cwd.join(".gitignore")).map_err(|e| e.to_string())?;
            for f in &secret_files { let _ = writeln!(gi, "{f}"); }
        }
        let url = format!("https://github.com/{repo}.git");
        git(&cwd, &["add", "-A"], &[])?;
        // --allow-empty: an empty folder still gets a commit, or there's nothing to push
        git(&cwd, &["-c", "user.name=qh", "-c", "user.email=qh@localhost", "commit", "-q", "--allow-empty", "-m", "First commit"], &[])?;
        let branch = git(&cwd, &["branch", "--show-current"], &[]).ok().filter(|b| !b.trim().is_empty()).unwrap_or("main".into());
        git(&cwd, &["push", "-q", "-u", &url, &format!("HEAD:{}", branch.trim())], &auth_env(&tok))?;
        println!("✓ on GitHub: https://github.com/{repo}");
    }
    // the team key exists from the start, so each invite can carry it under its own code
    crate::team::ensure_key(&repo);
    let mut lines = vec![];
    for (f, shown) in &people {
        match api("PUT", &format!("/repos/{repo}/collaborators/{f}"), &tok, Some(&json!({"permission": "push"})), None) {
            Ok((c @ (201 | 204), _, _)) => {
                println!("✓ {shown} {}", if c == 201 { "invited" } else { "already has access" });
                if let Some(code) = crate::team::add_invite(&repo, f) { lines.push(format!("{f}:  qh join {repo} {code}")) }
            }
            Ok((c, _, _)) => println!("✗ GitHub answered {c} for {f}"),
            Err(e) => println!("✗ {f}: {e}"),
        }
    }
    remember(&repo, true);
    if !lines.is_empty() {
        println!("\nSend each friend their line (it lets them in even if you're offline):\n");
        for l in lines { println!("    {l}") }
        println!();
    }
    Ok(())
}

/// Give friends access to this project's repo; they then run `qh join owner/repo`.
pub fn invite(cwd: &Path, users: &[String]) -> Result<String, String> {
    let (_, repo) = github_repo(cwd).ok_or("this folder isn't on GitHub yet: /team create first")?;
    let tok = token().ok_or("no GitHub login found: run `gh auth login`")?;
    let mut out = vec![];
    let mut invited = false;
    let mut codes = vec![];
    for f in users {
        let ok = |f: &str, codes: &mut Vec<String>| {
            if let Some(c) = crate::team::add_invite(&repo, f) { codes.push(format!("{f}:  qh join {repo} {c}")) }
        };
        out.push(match api("PUT", &format!("/repos/{repo}/collaborators/{f}"), &tok, Some(&json!({"permission": "push"})), None) {
            Ok((201, _, _)) => { invited = true; ok(f, &mut codes); format!("✓ invited {f}") }
            Ok((204, _, _)) => { invited = true; ok(f, &mut codes); format!("✓ {f} already has access") }
            Ok((404, _, _)) => format!("✗ no GitHub user {f}"),
            Ok((403, _, _)) => format!("✗ GitHub refused: only the repo's owner or an admin can invite"),
            Ok((c, _, _)) => format!("✗ GitHub answered {c} for {f}"),
            Err(e) => format!("✗ {f}: {e}"),
        });
    }
    if invited && codes.is_empty() {
        out.push(format!("Send this to your friend: qh join {repo}  (you'll let them in when they turn up)"));
    } else if invited {
        out.push("Send each friend their line; it lets them in even if you're offline:".into());
        out.extend(codes.into_iter().map(|c| format!("  {c}")));
    }
    Ok(out.join("\n"))
}

/// Make the project's GitHub repo private or public.
pub fn set_visibility(cwd: &Path, private: bool) -> Result<String, String> {
    let (_, repo) = github_repo(cwd).ok_or("this folder isn't on GitHub")?;
    let tok = token().ok_or("no GitHub login found: run `gh auth login`")?;
    let (code, _, v) = api("PATCH", &format!("/repos/{repo}"), &tok, Some(&json!({"private": private})), None)?;
    if code >= 300 || v["private"] != private {
        return Err(format!("GitHub didn't change {repo} (only its owner or an admin can)"));
    }
    Ok(if private { format!("{repo} is now private: only people you invite can see it") }
       else { format!("{repo} is now public: anyone can see the code (team data stays encrypted)") })
}

pub fn join(repo: &str, code: Option<&str>) -> i32 {
    match join_inner(repo, code) {
        Ok(_) => 0,
        Err(e) => { eprintln!("✗ {e}"); 1 }
    }
}

pub fn join_inner(repo: &str, invite_code: Option<&str>) -> Result<PathBuf, String> {
    let repo = repo.trim().trim_start_matches("https://github.com/").trim_end_matches(".git").trim_end_matches('/');
    if repo.split('/').count() != 2 { return Err("use the form owner/repo, e.g. qh join kesavsabari/webshop".into()) }
    let tok = token().ok_or("no GitHub login found: install the GitHub CLI and run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, me) = api("GET", "/user", &tok, None, None)?;
    let me = me["login"].as_str().unwrap_or("").to_string();
    // accept a waiting invite, if there is one
    let (_, _, invites) = api("GET", "/user/repository_invitations", &tok, None, None)?;
    if let Some(inv) = invites.as_array().into_iter().flatten().find(|i| i["repository"]["full_name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(repo))) {
        let from = inv["inviter"]["login"].as_str().unwrap_or("someone");
        if !yes(&format!("{from} invited you ({me}) to {repo}. Accept?")) { return Err("cancelled".into()) }
        api("PATCH", &format!("/user/repository_invitations/{}", inv["id"]), &tok, None, None)?;
        println!("✓ accepted");
    }
    let (code, _, info) = api("GET", &format!("/repos/{repo}"), &tok, None, None)?;
    if code == 404 {
        return Err(format!("you ({me}) don't have access to {repo}. Ask its owner to run `qh team create {me}` or invite you on GitHub."));
    }
    if info["permissions"]["push"] != true {
        println!("note: you can read {repo} but not push, so you'll see the team but can't appear in it");
    }
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let dir = std::env::current_dir().map_err(|e| e.to_string())?.join(name);
    if dir.exists() {
        if github_repo(&dir).is_some_and(|(_, r)| r.eq_ignore_ascii_case(repo)) {
            println!("{} is already here", dir.display());
        } else {
            return Err(format!("{} already exists and isn't this project; run qh join from another folder", dir.display()));
        }
    } else {
        println!("downloading {repo} into {}…", dir.display());
        git(&std::env::current_dir().map_err(|e| e.to_string())?, &["clone", "-q", &format!("https://github.com/{repo}.git"), name], &auth_env(&tok))?;
    }
    remember(repo, true);
    if let Some(c) = invite_code.filter(|c| !c.trim().is_empty()) { crate::team::set_join_code(repo, c) }
    println!("✓ joined {repo}. Start working together:\n\n    cd {name}\n    qh\n\nTeam mode turns on by itself; your teammates see you within seconds.");
    Ok(dir)
}
