//! Skills: a folder with SKILL.md whose front matter has `name` and
//! `description`. Only the one-line descriptions sit in the system prompt; the model loads a
//! body with the `skill` tool when a task calls for it. Searched in .qh/skills (project,
//! walking up), then ~/.config/qwen-harness/skills.
use std::path::{Path, PathBuf};

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

fn front(text: &str, key: &str) -> Option<String> {
    let body = text.strip_prefix("---")?;
    let head = &body[..body.find("\n---")?];
    head.lines().find_map(|l| l.strip_prefix(&format!("{key}:")).map(|v| v.trim().trim_matches('"').to_string()))
}

pub fn find(cwd: &Path) -> Vec<Skill> {
    let mut out: Vec<Skill> = vec![];
    let dirs = cwd.ancestors().map(|d| d.join(".qh/skills")).chain([crate::config::config_dir().join("skills")]);
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("SKILL.md")).filter(|p| p.is_file()).collect();
        paths.sort();
        for p in paths {
            let Ok(t) = std::fs::read_to_string(&p) else { continue };
            let name = front(&t, "name").unwrap_or_else(|| p.parent().and_then(|d| d.file_name()).unwrap_or_default().to_string_lossy().into());
            if out.iter().any(|s| s.name == name) {
                continue; // the nearer one wins
            }
            out.push(Skill { name, description: front(&t, "description").unwrap_or_default(), path: p });
        }
    }
    out
}

/// The system prompt's list, or "" when there are none.
pub fn listing(cwd: &Path) -> String {
    let s = find(cwd);
    if s.is_empty() {
        return String::new();
    }
    let rows: Vec<String> = s.iter().map(|s| format!("- {}: {}", s.name, s.description.chars().take(200).collect::<String>())).collect();
    format!("\n\nSkills (call the skill tool with the name to load one before doing a task it covers):\n{}", rows.join("\n"))
}

pub fn load(cwd: &Path, name: &str) -> String {
    match find(cwd).into_iter().find(|s| s.name == name) {
        Some(s) => {
            let body = std::fs::read_to_string(&s.path).unwrap_or_default();
            let dir = s.path.parent().map(|d| d.display().to_string()).unwrap_or_default();
            crate::tools::cap(format!("Skill {name} (files it mentions are in {dir}):\n{body}"))
        }
        None => format!("error: no skill {name}. Known: {}", find(cwd).iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_front_matter() {
        let d = std::env::temp_dir().join(format!("qh-skill-{}", std::process::id()));
        std::fs::create_dir_all(d.join(".qh/skills/release")).unwrap();
        std::fs::write(d.join(".qh/skills/release/SKILL.md"), "---\nname: release\ndescription: \"Cut a release\"\n---\nStep 1: bump.").unwrap();
        let s = find(&d);
        assert_eq!((s[0].name.as_str(), s[0].description.as_str()), ("release", "Cut a release"));
        assert!(load(&d, "release").contains("Step 1: bump."));
        assert!(listing(&d).contains("- release: Cut a release"));
    }
}
