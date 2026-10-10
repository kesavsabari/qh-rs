//! Tool calling for models that have no native tool support: the tools are described in the
//! system prompt, the model answers with one JSON object, and the harness turns it back into
//! the `tool_calls` shape the engine already understands. History is rewritten on the way out
//! (tool calls become the JSON the model wrote, results become user messages) because such
//! models' chat templates reject `tool` roles.
use serde_json::{json, Value};

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn describe(tools: &[Value]) -> String {
    tools.iter().map(|t| {
        let f = &t["function"];
        let props = f["parameters"]["properties"].as_object();
        let req: Vec<&str> = f["parameters"]["required"].as_array().map(|r| r.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let args = props.map(|p| p.iter().map(|(k, v)| {
            format!("{k}{}: {}", if req.contains(&k.as_str()) { "" } else { "?" }, s(v, "type"))
        }).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        format!("- {}({args}) {}", s(f, "name"), s(f, "description"))
    }).collect::<Vec<_>>().join("\n")
}

/// Messages for a model without native tools: protocol appended to the system prompt, tool
/// traffic flattened into plain text.
pub fn wrap(messages: &[Value], tools: &[Value]) -> Vec<Value> {
    let protocol = format!("\n\nYou can use tools. To use one, reply with ONLY a JSON object, no other text:\n\
{{\"tool\": \"<name>\", \"args\": {{...}}}}\nYou will get the result as the next message. When you need no tool, answer normally.\n\
Tools:\n{}", describe(tools));
    let mut out = vec![];
    for m in messages {
        match s(m, "role") {
            "system" if out.is_empty() => out.push(json!({"role": "system", "content": format!("{}{protocol}", s(m, "content"))})),
            "assistant" if m.get("tool_calls").is_some() => {
                let calls: Vec<String> = m["tool_calls"].as_array().into_iter().flatten().map(|c| {
                    json!({"tool": c["function"]["name"], "args": c["function"]["arguments"]}).to_string()
                }).collect();
                out.push(json!({"role": "assistant", "content": calls.join("\n")}));
            }
            "tool" => out.push(json!({"role": "user", "content": format!("[result of {}]\n{}", s(m, "tool_name"), s(m, "content"))})),
            _ => out.push(m.clone()),
        }
    }
    out
}

/// The JSON object a model wrote, if the whole reply is one tool call (bare or in a code fence).
pub fn parse(content: &str, tools: &[Value]) -> Option<Value> {
    let t = content.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let v: Value = serde_json::from_str(t).ok()?;
    let name = v.get("tool").or_else(|| v.get("name"))?.as_str()?;
    tools.iter().any(|x| x["function"]["name"] == name).then_some(())?;
    // small models often put the arguments beside "tool" instead of under "args"
    let args = v.get("args").or_else(|| v.get("arguments")).cloned().unwrap_or_else(|| {
        let flat: serde_json::Map<String, Value> = v.as_object().into_iter().flatten()
            .filter(|(k, _)| !matches!(k.as_str(), "tool" | "name")).map(|(k, x)| (k.clone(), x.clone())).collect();
        Value::Object(flat)
    });
    Some(json!({"function": {"name": name, "arguments": args}}))
}

/// Turn a reply that is a tool call into a message with `tool_calls`, in place.
pub fn unwrap(msg: &mut Value, tools: &[Value]) {
    if let Some(call) = parse(s(msg, "content"), tools) {
        msg["tool_calls"] = json!([call]);
        msg["content"] = "".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Value> {
        vec![json!({"type": "function", "function": {"name": "read_file", "description": "Read a file.",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "start": {"type": "integer"}}, "required": ["path"]}}})]
    }

    #[test]
    fn parses_bare_and_fenced_calls() {
        let t = tools();
        let c = parse(r#"{"tool": "read_file", "args": {"path": "a.py"}}"#, &t).unwrap();
        assert_eq!(c["function"]["name"], "read_file");
        assert_eq!(c["function"]["arguments"]["path"], "a.py");
        assert!(parse("```json\n{\"tool\": \"read_file\", \"args\": {\"path\": \"x\"}}\n```", &t).is_some());
        assert!(parse(r#"{"tool": "rm_rf", "args": {}}"#, &t).is_none(), "unknown tool");
        let flat = parse(r#"{"tool": "read_file", "path": "b.py"}"#, &t).unwrap();
        assert_eq!(flat["function"]["arguments"]["path"], "b.py");
        assert!(parse("I will read the file now.", &t).is_none());
    }

    #[test]
    fn history_is_flattened() {
        let t = tools();
        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a"}}}]}),
            json!({"role": "tool", "tool_name": "read_file", "content": "hello"}),
        ];
        let w = wrap(&msgs, &t);
        assert!(w[0]["content"].as_str().unwrap().contains("read_file(path: string, start?: integer)"));
        assert_eq!(w[1]["content"], r#"{"args":{"path":"a"},"tool":"read_file"}"#);
        assert_eq!(w[2]["role"], "user");
        assert!(w.iter().all(|m| m["role"] != "tool"));
    }
}
