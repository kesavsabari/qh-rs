//! First-time team setup from the terminal. `qh team create [friend…]` puts the current folder
//! on GitHub as a private repo if it isn't there yet and invites friends by GitHub username;
//! `qh join owner/repo` accepts the invite, downloads the project and turns team mode on.
//! Every step that changes something on GitHub is shown and confirmed first.
use crate::team::{api, auth_env, github_repo, remember, token};
use serde_json::json;
use std::io::Write;
use std::path::Path;
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

/// Files that look like secrets and that git would upload.
fn secrets(dir: &Path) -> Vec<String> {
    let out = git(dir, &["ls-files", "--others", "--cached", "--exclude-standard"], &[]).unwrap_or_default();
    out.lines().filter(|f| {
        let name = f.rsplit('/').next().unwrap_or(f).to_lowercase();
        name == ".env" || name.starts_with(".env.") || name.ends_with(".pem") || name.ends_with(".key")
            || name.starts_with("id_rsa") || name.starts_with("id_ed25519") || name.contains("credentials") || name == "secrets.toml"
    }).map(String::from).collect()
}

pub fn create(friends: &[String]) -> i32 {
    match create_inner(friends) {
        Ok(()) => 0,
        Err(e) => { eprintln!("✗ {e}"); 1 }
    }
}

fn create_inner(friends: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let tok = token().ok_or("no GitHub login found: install the GitHub CLI and run `gh auth login`, or set GITHUB_TOKEN")?;
    let (_, _, me) = api("GET", "/user", &tok, None, None)?;
    let me = me["login"].as_str().ok_or("GitHub didn't say who you are")?.to_string();
    println!("GitHub account: {me}");
    let repo = match github_repo(&cwd) {
        Some((_, r)) => { println!("This project is already on GitHub: {r}"); r }
        None => {
            if git(&cwd, &["rev-parse", "--show-toplevel"], &[]).is_err() {
                if !yes(&format!("{} isn't a git project yet. Start one here?", cwd.display())) { return Err("cancelled".into()) }
                git(&cwd, &["init", "-q", "-b", "main"], &[])?;
            }
            let found = secrets(&cwd);
            if !found.is_empty() {
                println!("These look like secrets and would be uploaded:\n  {}", found.join("\n  "));
                if yes("Keep them out (adds them to .gitignore)?") {
                    let mut gi = std::fs::OpenOptions::new().create(true).append(true).open(cwd.join(".gitignore")).map_err(|e| e.to_string())?;
                    for f in &found { let _ = writeln!(gi, "{f}"); }
                } else if !yes("Upload them anyway?") {
                    return Err("cancelled".into());
                }
            }
            let default = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or("project".into());
            let name = Some(ask(&format!("Repo name [{default}]: "))).filter(|n| !n.is_empty()).unwrap_or(default);
            let private = yes("Private repo (only people you invite can see it)?");
            if !yes(&format!("Create {} repo {me}/{name} on GitHub and upload this folder?", if private { "private" } else { "PUBLIC" })) {
                return Err("cancelled".into());
            }
            api("POST", "/user/repos", &tok, Some(&json!({"name": name, "private": private})), None)?;
            let url = format!("https://github.com/{me}/{name}.git");
            if git(&cwd, &["remote", "get-url", "origin"], &[]).is_ok() {
                git(&cwd, &["remote", "set-url", "origin", &url], &[])?;
            } else {
                git(&cwd, &["remote", "add", "origin", &url], &[])?;
            }
            git(&cwd, &["add", "-A"], &[])?;
            let _ = git(&cwd, &["commit", "-q", "-m", "First commit"], &[]); // nothing new to commit is fine
            let branch = git(&cwd, &["branch", "--show-current"], &[]).unwrap_or("main".into());
            git(&cwd, &["push", "-q", "-u", &url, &format!("HEAD:{branch}")], &auth_env(&tok))?;
            println!("✓ created and uploaded https://github.com/{me}/{name}");
            format!("{me}/{name}")
        }
    };
    let mut invited = vec![];
    let mut friends: Vec<String> = friends.to_vec();
    if friends.is_empty() {
        let f = ask("Friends' GitHub usernames to invite (space-separated, Enter to skip): ");
        friends = f.split_whitespace().map(String::from).collect();
    }
    for f in &friends {
        let (code, _, u) = api("GET", &format!("/users/{f}"), &tok, None, None)?;
        if code == 404 { println!("✗ no GitHub user {f}"); continue }
        let shown = u["name"].as_str().filter(|n| !n.is_empty()).map(|n| format!("{n} ({f})")).unwrap_or(f.clone());
        if !yes(&format!("Invite {shown} to {repo} with push access?")) { continue }
        match api("PUT", &format!("/repos/{repo}/collaborators/{f}"), &tok, Some(&json!({"permission": "push"})), None) {
            Ok((201, _, _)) => { println!("✓ invited {f}: GitHub emails them"); invited.push(f.clone()) }
            Ok((204, _, _)) => println!("✓ {f} already has access"),
            Ok((c, _, _)) => println!("✗ GitHub answered {c} for {f}"),
            Err(e) => println!("✗ {f}: {e}"),
        }
    }
    remember(&repo, true);
    println!("\nTeam mode turns on when you run `qh` here.");
    if !invited.is_empty() {
        println!("Send your friend{} this to join:\n\n    qh join {repo}\n", if invited.len() > 1 { "s" } else { "" });
    }
    Ok(())
}

pub fn join(repo: &str) -> i32 {
    match join_inner(repo) {
        Ok(()) => 0,
        Err(e) => { eprintln!("✗ {e}"); 1 }
    }
}

fn join_inner(repo: &str) -> Result<(), String> {
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
    println!("✓ joined {repo}. Start working together:\n\n    cd {name}\n    qh\n\nTeam mode turns on by itself; your teammates see you within seconds.");
    Ok(())
}
