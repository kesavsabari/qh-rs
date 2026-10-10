//! Keep the conversation inside a small context window. Stage 1 replaces old
//! tool outputs with one-line stubs; stage 2 has the model fold older turns
//! into a single summary note.
use serde_json::Value;


pub fn role(m: &Value) -> &str {
    m["role"].as_str().unwrap_or("")
}

pub fn content(m: &Value) -> &str {
    m["content"].as_str().unwrap_or("")
}

pub fn chars(messages: &[Value]) -> usize {
    messages.iter().map(|m| {
        let c = match &m["content"] {
            Value::String(s) => s.len(),
            Value::Null => 0,
            v => v.to_string().len(),
        };
        c + m.get("tool_calls").map_or(0, |t| t.to_string().len())
    }).sum()
}

pub fn estimate(messages: &[Value], ratio: f64) -> usize {
    (chars(messages) as f64 / ratio) as usize
}

/// Stage 0: an older read of a file is dead weight once it is read again.
pub fn stub_superseded(messages: &mut [Value]) -> usize {
    let mut seen = std::collections::HashSet::new();
    let mut saved = 0;
    for m in messages.iter_mut().rev() {
        let Some(key) = m.get("key").and_then(|k| k.as_str()).map(String::from) else { continue };
        if role(m) != "tool" || !(key.starts_with("read_file:") || key.starts_with("page:")) {
            continue;
        }
        if seen.contains(&key) && !content(m).starts_with("[elided") {
            saved += content(m).len();
            if let Some(o) = m.as_object_mut() {
                o.remove("images");
            }
            let what = if key == "page:" { "page snapshot".to_string() } else { format!("read of {}", &key[10..]) };
            m["content"] = format!("[elided older {what}; a newer one is below]").into();
        }
        seen.insert(key);
    }
    saved
}

pub fn stub_tool_outputs(messages: &mut [Value], keep_recent: usize) -> usize {
    let keep = messages.iter().rposition(|m| role(m) == "tool" && content(m).starts_with("error"));
    let end = messages.len().saturating_sub(keep_recent);
    let mut saved = 0;
    for i in 1..end {
        let m = &mut messages[i];
        if role(m) == "assistant" {
            saved += stub_call_args(m);
            continue;
        }
        // a stub is ~40 chars; replacing anything shorter would grow the context
        if Some(i) == keep || role(m) != "tool" || content(m).starts_with("[elided") || content(m).len() < 200 {
            continue;
        }
        let n = content(m).len();
        saved += n;
        let name = m["tool_name"].as_str().unwrap_or("tool").to_string();
        if let Some(o) = m.as_object_mut() {
            o.remove("images");
        }
        m["content"] = format!("[elided {name} output, {n} chars]").into();
    }
    saved
}

/// File bodies the model wrote (write_file content, edit_file old/new) are already on disk;
/// in old turns they are dead weight.
fn stub_call_args(m: &mut Value) -> usize {
    let mut saved = 0;
    let Some(calls) = m.get_mut("tool_calls").and_then(|t| t.as_array_mut()) else { return 0 };
    for c in calls {
        let Some(a) = c["function"]["arguments"].as_object_mut() else { continue };
        for k in ["content", "new", "old"] {
            if let Some(Value::String(v)) = a.get(k) {
                if v.len() > 300 {
                    saved += v.len();
                    let n = v.len();
                    a.insert(k.into(), format!("[elided {n} chars]").into());
                }
            }
        }
    }
    saved
}

/// Index where the kept tail starts, so no tool result is cut off from its call: a user
/// message, or, when the only user message is the opening task (a worker's single long
/// turn), an assistant message, whose results all follow it.
pub fn split_point(messages: &[Value], keep_recent: usize) -> usize {
    let start = messages.len().saturating_sub(keep_recent);
    let mut cut = start;
    while cut > 1 && role(&messages[cut]) != "user" {
        cut -= 1;
    }
    if cut <= 2 {
        cut = start;
        while cut > 2 && role(&messages[cut]) != "assistant" {
            cut -= 1;
        }
    }
    cut.max(1)
}

fn clip(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub fn render(messages: &[Value]) -> String {
    let mut lines = vec![];
    for m in messages {
        if role(m) == "tool" {
            lines.push(format!("[{} result] {}", m["tool_name"].as_str().unwrap_or(""), clip(content(m), 300)));
        } else if let Some(tc) = m.get("tool_calls").and_then(|t| t.as_array()) {
            for c in tc {
                let a = c["function"]["arguments"].to_string();
                lines.push(format!("[assistant called {}] {}", c["function"]["name"].as_str().unwrap_or(""), clip(&a, 300)));
            }
            if !content(m).is_empty() {
                lines.push(format!("assistant: {}", content(m)));
            }
        } else {
            lines.push(format!("{}: {}", role(m), content(m)));
        }
    }
    lines.join("\n")
}

pub const SUMMARY_PROMPT: &str = "Summarise this agent session so work can continue from the summary alone.
Keep: the user's goals, decisions, files touched and their current state, open problems,
exact names/paths/commands that matter. Drop chit-chat and raw tool output. Under 250 words.

";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worker_turn(n: usize) -> Vec<Value> {
        let mut v = vec![json!({"role": "system", "content": "sys"}), json!({"role": "user", "content": "build it"})];
        for i in 0..n {
            v.push(json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "write_file",
                "arguments": {"path": format!("f{i}.rs"), "content": "x".repeat(2000)}}}]}));
            v.push(json!({"role": "tool", "tool_name": "write_file", "content": "y".repeat(1500)}));
        }
        v
    }

    #[test]
    fn single_turn_can_be_cut() {
        let v = worker_turn(10);
        let cut = split_point(&v, 6);
        assert!(cut > 2 && role(&v[cut]) == "assistant", "cut {cut}");
    }

    #[test]
    fn old_file_bodies_are_stubbed() {
        let mut v = worker_turn(10);
        let before = chars(&v);
        stub_tool_outputs(&mut v, 6);
        assert!(chars(&v) < before / 3, "{} -> {}", before, chars(&v));
        assert_eq!(content(&v[1]), "build it");
    }
}

#[cfg(test)]
mod stub_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_outputs_never_grow() {
        let mut v = vec![json!({"role": "system", "content": "s"}), json!({"role": "user", "content": "go"})];
        for _ in 0..20 {
            v.push(json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "bash", "arguments": {"cmd": "cargo check"}}}]}));
            v.push(json!({"role": "tool", "tool_name": "bash", "content": "exit 0"}));
        }
        let before = chars(&v);
        stub_tool_outputs(&mut v, 6);
        assert!(chars(&v) <= before);
    }
}
