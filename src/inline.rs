//! Inline edit (Cmd+I): rewrite a selected line range of one file with a single tool-less model
//! call, then apply it through the checkpoints so it shows in review like an agent edit.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SYSTEM: &str = "Rewrite only the selected lines. Reply with the replacement lines only, no fences, no commentary.";
pub const CONTEXT: usize = 30;

pub struct Pending {
    pub tab: String,
    pub abs: PathBuf,
    pub rel: String,
    pub start: usize,
    pub end: usize,
    pub before: String,
    pub after: String,
}

/// Split text into lines keeping their endings, so slices rejoin exactly.
fn lines(text: &str) -> Vec<&str> { text.split_inclusive('\n').collect() }

/// 1-based inclusive `start..=end`, clamped. Returns (before-context, selection, after-context).
pub fn slice(text: &str, start: usize, end: usize, ctx: usize) -> Result<(String, String, String), String> {
    let ls = lines(text);
    if start == 0 || end < start || start > ls.len().max(1) { return Err(format!("bad range {start}-{end} ({} lines)", ls.len())) }
    let (s, e) = (start - 1, end.min(ls.len()));
    Ok((ls[s.saturating_sub(ctx)..s].concat(), ls[s..e].concat(), ls[e..(e + ctx).min(ls.len())].concat()))
}

/// Replace lines start..=end with `new`, keeping a trailing newline if the selection had one.
pub fn splice(text: &str, start: usize, end: usize, new: &str) -> String {
    let ls = lines(text);
    let (s, e) = ((start - 1).min(ls.len()), end.min(ls.len()));
    let mut new = new.to_string();
    let sel_nl = ls[s..e].concat().ends_with('\n');
    if sel_nl && !new.is_empty() && !new.ends_with('\n') { new.push('\n') }
    format!("{}{}{}", ls[..s].concat(), new, ls[e..].concat())
}

/// Drop a ``` fence the model wrapped its answer in anyway.
pub fn strip_fences(s: &str) -> String {
    let t = s.trim_matches('\n');
    let trimmed = t.trim();
    if trimmed.starts_with("```") {
        let body = trimmed.split_once('\n').map(|x| x.1).unwrap_or("");
        let body = body.trim_end();
        let body = body.strip_suffix("```").unwrap_or(body);
        return body.trim_end_matches([' ', '\t']).trim_end_matches('\n').to_string() + "\n";
    }
    t.to_string() + if t.is_empty() { "" } else { "\n" }
}

/// Resolve `rel` inside `root`, refusing anything that escapes it.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = crate::sandbox::canon(&root.join(rel));
    if !p.starts_with(crate::sandbox::canon(root)) { return Err(format!("{rel} is outside the project")) }
    Ok(p)
}

pub fn user_msg(rel: &str, pre: &str, sel: &str, post: &str, instruction: &str) -> String {
    format!("File: {rel}\n\n<before>\n{pre}</before>\n<selected>\n{sel}</selected>\n<after>\n{post}</after>\n\nInstruction: {instruction}")
}

/// The one model call: streams text through `on`, returns the cleaned replacement.
pub fn run(provider: &str, model: &str, ctx: usize, keep_alive: &str, user: &str,
           on: &mut dyn FnMut(&str)) -> Result<String, String> {
    let prov = crate::providers::resolve(provider).ok_or_else(|| format!("no provider {provider}"))?;
    let msgs = [json!({"role": "system", "content": SYSTEM}), json!({"role": "user", "content": user})];
    let mut cb = |thinking: bool, t: &str| { if !thinking { on(t) } true };
    let r = crate::backend::chat(&prov, model, &msgs, &[], "", ctx, keep_alive, &mut cb).map_err(|e| e.to_string())?;
    Ok(strip_fences(r.msg["content"].as_str().unwrap_or("")))
}

pub fn result_json(id: u64, p: &Pending) -> Value {
    json!({"type": "inline_result", "id": id, "tab": p.tab, "path": p.rel, "start": p.start, "end": p.end,
           "before": p.before, "after": p.after})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fences_stripped() {
        assert_eq!(strip_fences("```rust\nfn a() {}\n```"), "fn a() {}\n");
        assert_eq!(strip_fences("\n```\nx\ny\n```\n"), "x\ny\n");
        assert_eq!(strip_fences("plain\n"), "plain\n");
        assert_eq!(strip_fences("  indented"), "  indented\n");
        assert_eq!(strip_fences(""), "");
    }
    #[test]
    fn ranges_slice_and_splice() {
        let t = "a\nb\nc\nd\ne\n";
        let (pre, sel, post) = slice(t, 2, 3, 1).unwrap();
        assert_eq!((pre.as_str(), sel.as_str(), post.as_str()), ("a\n", "b\nc\n", "d\n"));
        let (pre, sel, post) = slice(t, 1, 99, 30).unwrap();
        assert_eq!((pre.as_str(), sel.as_str(), post.as_str()), ("", t, ""));
        assert!(slice(t, 0, 1, 1).is_err() && slice(t, 3, 2, 1).is_err() && slice(t, 9, 9, 1).is_err());
        assert_eq!(splice(t, 2, 3, "X"), "a\nX\nd\ne\n");
        assert_eq!(splice("a\nb", 2, 2, "B"), "a\nB");
        assert_eq!(splice(t, 5, 5, ""), "a\nb\nc\nd\n");
    }
    #[test]
    fn resolve_stays_inside() {
        let d = std::env::temp_dir();
        assert!(resolve(&d, "../../etc/passwd").is_err());
        assert!(resolve(&d, "x.txt").is_ok());
    }
}
