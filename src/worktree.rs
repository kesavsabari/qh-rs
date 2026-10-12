//! Git worktree per tab.
//!
//! Design note (v0.2.0):
//! - A tab can work in its own checkout so two agents editing one repo never trample each other.
//!   `/new --worktree [name]` or `/worktree [name]` runs `git worktree add -b qh/<name>
//!   .qh/worktrees/<name> HEAD` from the main checkout and moves the tab's cwd there.
//! - `.qh/worktrees/` is listed in the repo's `info/exclude` (never in `.gitignore`, so nothing
//!   shows up in the user's diff).
//! - Everything is derived from the path: a cwd under `<main>/.qh/worktrees/<name>` is a worktree
//!   tab on branch `qh/<name>`. The branch it came from is kept in git config
//!   (`branch.qh/<name>.qhbase`) so it survives restarts.
//! - `/merge` shows a summary vs the base, then (after a confirm picker) commits the tab's work
//!   and runs `git merge --no-ff` in the main checkout. It refuses if the main checkout is dirty
//!   or not on the base branch; a conflicting merge is aborted (`git merge --abort`) so both
//!   sides are left as they were.
//! - `/merge --discard` (after a confirm picker) commits leftovers so `git worktree remove` needs
//!   no `--force`, removes the worktree and deletes the branch; the note prints the last commit so
//!   it can still be recovered with `git branch <name> <sha>` until git gc.
//! - No `--force` anywhere; nothing is deleted without a confirm.
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DIR: &str = ".qh/worktrees";

#[derive(Debug, Clone, PartialEq)]
pub struct Wt {
    pub name: String,
    pub path: PathBuf, // the worktree checkout
    pub main: PathBuf, // the main checkout
}

impl Wt {
    pub fn branch(&self) -> String {
        format!("qh/{}", self.name)
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("git: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    } else {
        let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
        Err(if e.is_empty() { format!("git {} failed", args.join(" ")) } else { e })
    }
}

/// The worktree a cwd belongs to, from the path alone (cheap enough for every frame).
pub fn info(cwd: &Path) -> Option<Wt> {
    let comps: Vec<_> = cwd.components().collect();
    for i in 0..comps.len().saturating_sub(2) {
        if comps[i].as_os_str() == ".qh" && comps[i + 1].as_os_str() == "worktrees" {
            let name = comps.get(i + 2)?.as_os_str().to_string_lossy().to_string();
            let main: PathBuf = comps[..i].iter().collect();
            let path = main.join(DIR).join(&name);
            if path.join(".git").is_file() {
                return Some(Wt { name, path, main });
            }
        }
    }
    None
}

/// The checkout holding `.git` above cwd, by walking the filesystem (no git process).
pub fn checkout(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// The current branch of the checkout containing cwd, read from HEAD (for the pane title).
pub fn branch(cwd: &Path) -> Option<String> {
    let root = checkout(cwd)?;
    let dotgit = root.join(".git");
    let gitdir = if dotgit.is_dir() {
        dotgit
    } else {
        let s = std::fs::read_to_string(&dotgit).ok()?;
        let p = PathBuf::from(s.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    match head.trim().strip_prefix("ref: refs/heads/") {
        Some(b) => Some(b.to_string()),
        None => Some(head.trim().chars().take(7).collect()),
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && !n.starts_with(['.', '-'])
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !n.contains("..")
}

/// Create `.qh/worktrees/<name>` on `qh/<name>` from the main checkout's HEAD.
pub fn create(cwd: &Path, name: &str) -> Result<Wt, String> {
    if !valid_name(name) {
        return Err(format!("bad worktree name '{name}': use letters, digits, - _ ."));
    }
    let main = match info(cwd) {
        Some(w) => w.main,
        None => PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).map_err(|_| "not in a git repository".to_string())?),
    };
    let base = git(&main, &["symbolic-ref", "--short", "HEAD"])
        .map_err(|_| "the main checkout has no branch checked out (detached HEAD); check one out first".to_string())?;
    git(&main, &["rev-parse", "--verify", "HEAD"]).map_err(|_| "the repository has no commits yet".to_string())?;
    let branch = format!("qh/{name}");
    if git(&main, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok() {
        return Err(format!("branch {branch} already exists; pick another name"));
    }
    let path = main.join(DIR).join(name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    exclude(&main)?;
    git(&main, &["worktree", "add", "-b", &branch, &path.to_string_lossy(), "HEAD"])?;
    let _ = git(&main, &["config", &format!("branch.{branch}.qhbase"), &base]);
    Ok(Wt { name: name.into(), path, main })
}

/// Make sure `.qh/worktrees/` is in the repo's info/exclude.
fn exclude(main: &Path) -> Result<(), String> {
    let common = PathBuf::from(git(main, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() { common } else { main.join(common) };
    let f = common.join("info").join("exclude");
    let cur = std::fs::read_to_string(&f).unwrap_or_default();
    if cur.lines().any(|l| l.trim() == "/.qh/worktrees/" || l.trim() == ".qh/worktrees/") {
        return Ok(());
    }
    let _ = std::fs::create_dir_all(common.join("info"));
    let sep = if cur.is_empty() || cur.ends_with('\n') { "" } else { "\n" };
    std::fs::write(&f, format!("{cur}{sep}/.qh/worktrees/\n")).map_err(|e| format!("{}: {e}", f.display()))
}

pub fn base(w: &Wt) -> String {
    git(&w.main, &["config", &format!("branch.{}.qhbase", w.branch())]).unwrap_or_else(|_| "main".into())
}

/// What `/merge` would bring in: commits ahead, files changed (committed or not), untracked files.
pub fn summary(w: &Wt) -> Result<String, String> {
    let base = base(w);
    let log = git(&w.path, &["log", "--oneline", &format!("{base}..HEAD")])?;
    // intent-to-add is not used: untracked files are listed separately so the index stays untouched
    let stat = git(&w.path, &["diff", "--stat", &base])?;
    let status = git(&w.path, &["status", "--porcelain"])?;
    let untracked: Vec<&str> = status.lines().filter_map(|l| l.strip_prefix("?? ")).collect();
    if log.is_empty() && stat.is_empty() && untracked.is_empty() {
        return Ok(format!("{} has no changes vs {base}", w.branch()));
    }
    let mut s = format!("{} → {base}\n", w.branch());
    if !log.is_empty() {
        s += &format!("commits:\n{log}\n");
    }
    if !stat.is_empty() {
        s += &format!("{stat}\n");
    }
    if !untracked.is_empty() {
        s += &format!("new files: {}\n", untracked.join(", "));
    }
    Ok(s.trim_end().into())
}

/// Commit the tab's work and merge it into the base branch in the main checkout.
pub fn merge(w: &Wt, msg: &str) -> Result<String, String> {
    let base = base(w);
    let on = git(&w.main, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    if on != base {
        return Err(format!("the main checkout is on '{on}', not {base}; check out {base} there first"));
    }
    let dirty = git(&w.main, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!("the main checkout has uncommitted changes; commit or stash them first:\n{dirty}"));
    }
    git(&w.path, &["add", "-A"])?;
    if !git(&w.path, &["diff", "--cached", "--name-only"])?.is_empty() {
        git(&w.path, &["commit", "-q", "-m", msg])?;
    }
    if git(&w.main, &["rev-list", "--count", &format!("{base}..{}", w.branch())])? == "0" {
        return Ok(format!("nothing to merge: {} has no commits beyond {base}", w.branch()));
    }
    let m = format!("Merge {} (qh)", w.branch());
    match git(&w.main, &["merge", "--no-ff", "-m", &m, &w.branch()]) {
        Ok(_) => Ok(format!("merged {} into {base}", w.branch())),
        Err(e) => {
            let abort = git(&w.main, &["merge", "--abort"]);
            let files = e.lines().filter(|l| l.starts_with("CONFLICT")).collect::<Vec<_>>().join("\n");
            Err(format!("merge conflicts, aborted ({}); both sides left as they were:\n{}",
                if abort.is_ok() { "main checkout restored" } else { "abort failed: check the main checkout" },
                if files.is_empty() { e } else { files }))
        }
    }
}

/// Remove the worktree and its branch. Leftover edits are committed first so no --force is needed.
pub fn discard(w: &Wt) -> Result<String, String> {
    git(&w.path, &["add", "-A"])?;
    if !git(&w.path, &["diff", "--cached", "--name-only"])?.is_empty() {
        git(&w.path, &["commit", "-q", "-m", &format!("qh: {} (discarded)", w.name)])?;
    }
    let sha = git(&w.path, &["rev-parse", "--short", "HEAD"])?;
    git(&w.main, &["worktree", "remove", &w.path.to_string_lossy()])?;
    git(&w.main, &["branch", "-D", &w.branch()])?;
    let _ = git(&w.main, &["config", "--unset", &format!("branch.{}.qhbase", w.branch())]);
    Ok(format!("removed {} and {} (last commit {sha}; `git branch {} {sha}` brings it back until git gc)",
        w.path.display(), w.branch(), w.branch()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!("qh-wt-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        for a in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"],
                  &["config", "commit.gpgsign", "false"], &["config", "core.autocrlf", "false"]] {
            git(&d, a).unwrap();
        }
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        git(&d, &["add", "."]).unwrap();
        git(&d, &["commit", "-q", "-m", "init"]).unwrap();
        d
    }

    #[test]
    fn create_edit_merge() {
        let d = repo();
        let w = create(&d, "t1").unwrap();
        assert_eq!(info(&w.path.join("sub")).map(|x| x.name), Some("t1".into()));
        assert_eq!(branch(&w.path).as_deref(), Some("qh/t1"));
        assert!(std::fs::read_to_string(d.join(".git/info/exclude")).unwrap().contains("/.qh/worktrees/"));
        assert!(git(&d, &["status", "--porcelain"]).unwrap().is_empty());
        std::fs::write(w.path.join("a.txt"), "two\n").unwrap();
        std::fs::write(w.path.join("b.txt"), "new\n").unwrap();
        let s = summary(&w).unwrap();
        assert!(s.contains("a.txt") && s.contains("b.txt"), "{s}");
        merge(&w, "qh: t1").unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "two\n");
        assert!(d.join("b.txt").exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn merge_refused_on_dirty_main() {
        let d = repo();
        let w = create(&d, "t2").unwrap();
        std::fs::write(w.path.join("a.txt"), "two\n").unwrap();
        std::fs::write(d.join("a.txt"), "local\n").unwrap();
        let e = merge(&w, "x").unwrap_err();
        assert!(e.contains("uncommitted"), "{e}");
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "local\n");
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "two\n");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn conflict_aborts_cleanly() {
        let d = repo();
        let w = create(&d, "t3").unwrap();
        std::fs::write(w.path.join("a.txt"), "theirs\n").unwrap();
        std::fs::write(d.join("a.txt"), "ours\n").unwrap();
        git(&d, &["commit", "-qam", "ours"]).unwrap();
        let head = git(&d, &["rev-parse", "HEAD"]).unwrap();
        let e = merge(&w, "x").unwrap_err();
        assert!(e.contains("conflict"), "{e}");
        assert_eq!(git(&d, &["rev-parse", "HEAD"]).unwrap(), head);
        assert!(git(&d, &["status", "--porcelain"]).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "ours\n");
        assert_eq!(std::fs::read_to_string(w.path.join("a.txt")).unwrap(), "theirs\n");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn discard_removes() {
        let d = repo();
        let w = create(&d, "t4").unwrap();
        std::fs::write(w.path.join("a.txt"), "junk\n").unwrap();
        discard(&w).unwrap();
        assert!(!w.path.exists());
        assert!(git(&d, &["rev-parse", "--verify", "--quiet", "refs/heads/qh/t4"]).is_err());
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "one\n");
        assert!(create(&d, "../x").is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
