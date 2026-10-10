//! User hooks: shell commands run on agent events, configured as
//! `[[hooks]]` with `event` (pre_tool | post_tool | stop), optional `match` (regex on the
//! tool name) and `cmd`. The event arrives as JSON on stdin. Exit 2 means "no": a pre_tool
//! hook blocks the call, a stop hook sends its stderr back so the agent keeps working.
//! A post_tool hook's stdout is added to the tool result (formatter output, test failures).
use crate::config::Hook;
use serde_json::Value;
use std::path::Path;

pub enum Outcome {
    Pass(String), // stdout, possibly empty
    Block(String), // stderr (or stdout) explaining why
}

fn run(cmd: &str, input: &Value, cwd: &Path) -> Outcome {
    let o = crate::platform::run_timeout(crate::platform::shell_cmd(cmd), Some(input.to_string().as_bytes()), 60, cwd);
    let o = match o {
        Ok((_, true)) => return Outcome::Pass(format!("[hook timed out after 60 s: {cmd}]")),
        Ok((o, false)) => o,
        Err(_) => return Outcome::Pass(format!("[hook failed to start: {cmd}]")),
    };
    let (out, err) = (String::from_utf8_lossy(&o.stdout).trim().to_string(), String::from_utf8_lossy(&o.stderr).trim().to_string());
    if o.status.code() == Some(2) {
        Outcome::Block(if err.is_empty() { out } else { err })
    } else {
        Outcome::Pass(out)
    }
}

fn applies(h: &Hook, event: &str, tool: &str) -> bool {
    h.event == event && (h.matcher.is_empty()
        || regex::Regex::new(&format!("^(?:{})$", h.matcher)).is_ok_and(|r| r.is_match(tool)))
}

/// Run every matching hook; the first block wins, passes' output is joined.
pub fn fire(hooks: &[Hook], event: &str, tool: &str, input: &Value, cwd: &Path) -> Outcome {
    let mut said = vec![];
    for h in hooks.iter().filter(|h| applies(h, event, tool)) {
        match run(&h.cmd, input, cwd) {
            Outcome::Block(why) => return Outcome::Block(why),
            Outcome::Pass(s) if !s.is_empty() => said.push(s),
            _ => {}
        }
    }
    Outcome::Pass(said.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(event: &str, matcher: &str, cmd: &str) -> Hook {
        Hook { event: event.into(), matcher: matcher.into(), cmd: cmd.into() }
    }

    #[test]
    fn block_pass_and_match() {
        let d = std::env::temp_dir();
        let hs = vec![hook("pre_tool", "bash", "grep -q 'rm' && { echo 'no rm' >&2; exit 2; }; exit 0")];
        assert!(matches!(fire(&hs, "pre_tool", "bash", &json!({"args": {"cmd": "rm x"}}), &d), Outcome::Block(w) if w == "no rm"));
        assert!(matches!(fire(&hs, "pre_tool", "bash", &json!({"args": {"cmd": "ls"}}), &d), Outcome::Pass(_)));
        assert!(matches!(fire(&hs, "pre_tool", "bash_x", &json!({"args": {"cmd": "rm"}}), &d), Outcome::Pass(_)), "match is anchored");
        let post = vec![hook("post_tool", "write_file|edit_file", "echo formatted")];
        assert!(matches!(fire(&post, "post_tool", "edit_file", &json!({}), &d), Outcome::Pass(s) if s == "formatted"));
    }
}
