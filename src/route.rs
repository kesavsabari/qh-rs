//! A second model in the loop: a second opinion (/ask-other and the ask_model tool), read-only,
//! with no tools of its own; and, opt-in (`[routing] fetch_model`), a cheap model that only runs
//! lookups whose raw outputs go to the main model, which decides everything.
use crate::config::Config;
use crate::tools::schema;
use serde_json::{json, Value};
use std::path::Path;

const OPINION: &str = "You are a second, independent reviewer of another coding agent's work. You can't run tools or \
see more than what is below. Be blunt and short: at most 15 lines, under exactly these three headings, each with a few \
'- ' bullets (or '- none'):\nAGREE:\nDISAGREE:\nRISKS:";

/// "provider:model", "provider/model", "ollama/name" or a plain local name -> (provider, model).
pub fn model_id(s: &str) -> (String, String) {
    match s.split_once('/') {
        Some((p, m)) if p == "ollama" || crate::providers::get(p).is_some() => crate::providers::parse_model(&format!("{p}:{m}")),
        _ => crate::providers::parse_model(s),
    }
}

/// `/ask-other [model] [question]`: the first word is the model, the rest an optional question.
pub fn parse_ask_other(arg: &str) -> (Option<String>, String) {
    let arg = arg.trim();
    let (model, q) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    (Some(model.to_string()).filter(|m| !m.is_empty()), q.trim().to_string())
}

/// The model a second opinion goes to when none is named: `[routing] ask_model`, else the
/// fallback model, else the local model, whichever isn't the one asking.
pub fn other_model(cfg: &Config, provider: &str, model: &str) -> Option<(String, String)> {
    if !cfg.ask_model.is_empty() {
        return Some(model_id(&cfg.ask_model));
    }
    let cur = (provider.to_string(), model.to_string());
    cfg.fallback_model.iter().map(|s| model_id(s))
        .chain(std::iter::once((String::new(), cfg.model.clone()))).find(|x| *x != cur)
}

/// The request a second opinion gets: no tools, just the question and what it's about.
pub fn ask_msgs(question: &str, context: &str) -> Vec<Value> {
    let q = if question.trim().is_empty() { "Review this. Is it right, and what would you do differently?" } else { question };
    let ctx = if context.trim().is_empty() { String::new() } else { format!("\n\n<context>\n{}\n</context>", clip(context, 40_000)) };
    vec![json!({"role": "system", "content": OPINION}), json!({"role": "user", "content": format!("{q}{ctx}")})]
}

/// What /ask-other sends: the tab's uncommitted diff, else its last answer (the plan).
pub fn subject(cwd: &Path, msgs: &[Value]) -> String {
    let diff = std::process::Command::new("git").arg("-C").arg(cwd).args(["diff", "HEAD"]).output().ok()
        .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    if !diff.trim().is_empty() {
        return format!("The current diff:\n{diff}");
    }
    msgs.iter().rev().filter(|x| x["role"] == "assistant").filter_map(|x| x["content"].as_str())
        .find(|c| !c.trim().is_empty()).map(|c| format!("The agent's latest answer / plan:\n{c}")).unwrap_or_default()
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n { return s.to_string() }
    let mut c = n;
    while !s.is_char_boundary(c) { c -= 1 }
    format!("{}\n… (cut)", &s[..c])
}

pub fn ask_schema() -> Value {
    schema("ask_model", "Ask another model for a second opinion when you're stuck or unsure. It has no tools and can't see \
files: it sees only your question and context, so read the relevant files first and paste the code, error or plan it \
needs into context. It answers with agreements, disagreements and risks. model is optional (\"provider:model\"); the \
default is the configured second model.",
           &[("question", "string"), ("context", "string"), ("model", "string")], &["question", "context"])
}

/// What the fetch model may call: lookups only. Anything else it asks for is refused, never run.
pub const FETCH_TOOLS: [&str; 4] = ["read_file", "grep", "find_references", "web_search"];

pub fn fetch_allowed(name: &str) -> bool {
    FETCH_TOOLS.contains(&name)
}

/// The tab's tools the fetch model is shown: the lookups among them.
pub fn fetch_schemas(all: &[Value]) -> Vec<Value> {
    all.iter().filter(|t| t["function"]["name"].as_str().is_some_and(fetch_allowed)).cloned().collect()
}

pub fn fetch_schema() -> Value {
    schema("fetch", "Have the cheap lookup model gather raw material for you: it runs read_file / grep / find_references / \
web_search for the goal and you get their raw outputs back, unsummarised; it decides nothing. Use it for broad looking-around \
(\"where is X handled\", \"read the config loader\"); read a known file yourself.", &[("goal", "string")], &["goal"])
}

pub fn fetch_msgs(goal: &str, cwd: &Path) -> Vec<Value> {
    vec![json!({"role": "system", "content": format!("You fetch raw material for another model. Working folder: {}. \
Call read_file, grep, find_references or web_search to gather what the goal needs, several calls at once if you can. \
Don't explain or summarise: your words are thrown away, only the tool outputs are passed on. Reply 'done' when you have enough.",
        cwd.display())}), json!({"role": "user", "content": goal})]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_model_never_gets_a_mutating_tool() {
        let cfg = crate::config::load_defaults();
        let all = crate::tools::build("all", &cfg);
        let names: Vec<String> = fetch_schemas(&all).iter().map(|t| t["function"]["name"].as_str().unwrap().to_string()).collect();
        assert!(!names.is_empty() && names.iter().all(|n| fetch_allowed(n)), "{names:?}");
        for n in ["write_file", "edit_file", "rename_symbol", "bash", "remember", "task_stop", "send_message", "spawn", "type", "click",
                  "tell_team", "ask_model", "fetch", "todo"] {
            assert!(!fetch_allowed(n), "{n}");
        }
        // and every tool it may call is one that never asks: nothing it does changes anything
        for n in FETCH_TOOLS {
            assert!(!crate::tools::risky(n, &json!({}), &cfg) && crate::tools::builtin_schema(n).is_some(), "{n}");
        }
    }

    #[test]
    fn ask_other_takes_model_then_question() {
        assert_eq!(parse_ask_other(""), (None, String::new()));
        assert_eq!(parse_ask_other(" openai:gpt-5 "), (Some("openai:gpt-5".into()), String::new()));
        assert_eq!(parse_ask_other("qwen3:8b is the lock right?"), (Some("qwen3:8b".into()), "is the lock right?".into()));
    }

    #[test]
    fn default_second_model_is_never_the_asker() {
        let mut c = crate::config::load_defaults();
        c.model = "qwen3:8b".into();
        assert_eq!(other_model(&c, "", "qwen3:8b"), None);
        assert_eq!(other_model(&c, "openai", "gpt-5"), Some((String::new(), "qwen3:8b".into())));
        c.fallback_model = vec!["llama3:8b".into()];
        assert_eq!(other_model(&c, "", "qwen3:8b"), Some((String::new(), "llama3:8b".into())));
        c.ask_model = "ollama/gemma3:4b".into();
        assert_eq!(other_model(&c, "", "qwen3:8b"), Some((String::new(), "gemma3:4b".into())));
    }

    #[test]
    fn ask_model_is_read_only() {
        // no tools go with the request, it never asks the user, and it isn't a builtin that could run one
        let msgs = ask_msgs("why?", "fn f() {}");
        assert!(msgs.iter().all(|m| m.get("tools").is_none()) && msgs[1]["content"].as_str().unwrap().contains("fn f()"));
        assert!(!crate::tools::risky("ask_model", &json!({"question": "x"}), &crate::config::load_defaults()));
        assert!(crate::tools::builtin_schema("ask_model").is_none());
        assert_eq!(ask_schema()["function"]["name"], "ask_model");
    }
}
