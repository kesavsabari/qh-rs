//! The editor qh runs inside. The VS Code extension starts qh with `QH_IDE_PORT` and
//! `QH_IDE_TOKEN`: a server on 127.0.0.1 that only this qh knows the token for. Through it the
//! model sees what you have open and selected, VS Code's problems, and can open a file for you.
//! Nothing here edits files: edits still go through write_file / edit_file and their approvals.
use serde_json::{json, Value};
use std::time::Duration;

pub const TOOLS: [&str; 3] = ["ide_context", "ide_diagnostics", "ide_open"];

/// (port, token) when qh runs inside an editor that offers them.
fn link() -> Option<(u16, String)> {
    let port = std::env::var("QH_IDE_PORT").ok()?.parse().ok()?;
    let token = std::env::var("QH_IDE_TOKEN").ok().filter(|t| t.len() >= 16)?;
    Some((port, token))
}

pub fn active() -> bool {
    link().is_some()
}

pub fn schemas() -> Vec<Value> {
    use crate::tools::schema;
    vec![
        schema("ide_context", "What the user has open in VS Code: the active file, cursor line, the selected lines and their text, \
and the open tabs. Check it when they say \"this\", \"here\" or \"the selected code\".", &[], &[]),
        schema("ide_diagnostics", "VS Code's problems (errors and warnings from its language servers and linters) for one file, \
or for the whole workspace when path is left out.", &[("path", "string")], &[]),
        schema("ide_open", "Open a file in the user's VS Code editor, at a line if given, to show them something.",
               &[("path", "string"), ("line", "integer")], &["path"]),
    ]
}

/// Ask the editor; its answer is plain text for the model.
pub fn call(name: &str, a: &Value) -> String {
    let Some((port, token)) = link() else { return "error: not running inside an editor".into() };
    // a client of its own: no proxy settings apply to the editor on this machine
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build();
    let body = json!({"path": a["path"], "line": a["line"]}).to_string();
    match agent.post(&format!("http://127.0.0.1:{port}/{name}")).set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json").send_string(&body) {
        Ok(r) => r.into_string().unwrap_or_else(|e| format!("error: {e}")),
        Err(ureq::Error::Status(_, r)) => format!("error: {}", r.into_string().unwrap_or_default()),
        Err(e) => format!("error: the editor didn't answer ({e}); is VS Code still open?"),
    }
}

/// A line for the system prompt while inside an editor.
pub fn note() -> Option<&'static str> {
    active().then_some("You are running inside the user's VS Code. ide_context shows their open file and selection, \
ide_diagnostics VS Code's errors and warnings, and ide_open opens a file at a line in their editor. Files you edit update in \
their editor on their own.")
}
