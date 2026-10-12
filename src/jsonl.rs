//! `qh -p … --json`: one JSON object per line on stdout, for scripts and CI.
//! Event shapes: start, text, tool_call, tool_result, diff, permission_denied, usage, error, done.
use crate::hub::Event;
use serde_json::{json, Value};

/// Exit codes for headless runs.
pub const OK: i32 = 0;
pub const AGENT_ERROR: i32 = 1;
pub const BAD_USAGE: i32 = 2;
pub const NEEDS_PERMISSION: i32 = 3;

/// A tool result's first line, clipped: enough to see what happened without the whole output.
pub fn summary(result: &str) -> String {
    let line = result.lines().next().unwrap_or("");
    let mut s: String = line.chars().take(200).collect();
    if s.len() < line.len() {
        s.push('…');
    }
    s
}

fn ok(result: &str) -> bool {
    !(result.starts_with("error") || result == "denied by user" || result.starts_with("refused"))
}

fn lines(s: &str) -> usize {
    s.lines().count()
}

/// Lines added and removed by a successful edit, from its arguments.
pub fn diff(tool: &str, args: &Value, result: &str) -> Option<Value> {
    if !ok(result) {
        return None;
    }
    let path = args["path"].as_str()?;
    let (added, removed) = match tool {
        "edit_file" => (lines(args["new"].as_str().unwrap_or("")), lines(args["old"].as_str().unwrap_or(""))),
        "write_file" => (lines(args["content"].as_str().unwrap_or("")), 0),
        _ => return None,
    };
    Some(json!({"type": "diff", "path": path, "added": added, "removed": removed}))
}

/// The events one hub event becomes (Confirm is answered by the caller, not here).
pub fn events(ev: &Event, last_call: &mut Option<(String, Value)>) -> Vec<Value> {
    match ev {
        Event::Token(t) => vec![json!({"type": "text", "delta": t})],
        Event::ToolCall { name, args } => {
            *last_call = Some((name.clone(), args.clone()));
            vec![json!({"type": "tool_call", "name": name, "args": args})]
        }
        Event::ToolResult { name, result } => {
            let mut v = vec![json!({"type": "tool_result", "name": name, "ok": ok(result), "summary": summary(result)})];
            if let Some((n, a)) = last_call.as_ref().filter(|(n, _)| n == name) {
                v.extend(diff(n, a, result));
            }
            v
        }
        Event::Notice(n) if n.starts_with("error") => vec![json!({"type": "error", "message": n})],
        Event::Notice(n) => vec![json!({"type": "notice", "message": n})],
        Event::Compact { stage, before, after } => vec![json!({"type": "compact", "stage": stage, "before": before, "after": after})],
        _ => vec![],
    }
}

pub fn usage(u: &crate::usage::Usage) -> Value {
    json!({"type": "usage", "input": u.input, "cached": u.cached, "output": u.output,
           "cost": if u.paid && (!u.unpriced || u.cost > 0.0) { Some(u.cost) } else { None },
           "plan": !u.paid})
}

pub fn write(out: &mut impl std::io::Write, v: &Value) {
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_tools() {
        let mut last = None;
        assert_eq!(events(&Event::Token("hi".into()), &mut last), vec![json!({"type":"text","delta":"hi"})]);
        let args = json!({"path": "a.rs", "old": "x\ny", "new": "z"});
        let c = events(&Event::ToolCall { name: "edit_file".into(), args: args.clone() }, &mut last);
        assert_eq!(c[0]["type"], "tool_call");
        assert_eq!(c[0]["args"], args);
        let r = events(&Event::ToolResult { name: "edit_file".into(), result: "edited a.rs\nmore".into() }, &mut last);
        assert_eq!(r[0], json!({"type":"tool_result","name":"edit_file","ok":true,"summary":"edited a.rs"}));
        assert_eq!(r[1], json!({"type":"diff","path":"a.rs","added":1,"removed":2}));
    }

    #[test]
    fn failed_tool_has_no_diff() {
        let mut last = Some(("write_file".to_string(), json!({"path": "b", "content": "1\n2\n"})));
        let r = events(&Event::ToolResult { name: "write_file".into(), result: "denied by user".into() }, &mut last);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["ok"], false);
    }

    #[test]
    fn errors_and_usage() {
        let mut last = None;
        let e = events(&Event::Notice("error: no model".into()), &mut last);
        assert_eq!(e[0], json!({"type":"error","message":"error: no model"}));
        let mut u = crate::usage::Usage::default();
        u.input = 10;
        u.output = 3;
        assert_eq!(usage(&u), json!({"type":"usage","input":10,"cached":0,"output":3,"cost":null,"plan":true}));
    }

    #[test]
    fn one_line_per_event() {
        let mut buf = vec![];
        write(&mut buf, &json!({"type": "text", "delta": "a\nb"}));
        write(&mut buf, &json!({"type": "done", "ok": true, "answer": ""}));
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.lines().count(), 2);
        for l in s.lines() {
            serde_json::from_str::<Value>(l).unwrap();
        }
        assert_eq!(BAD_USAGE, 2);
        assert_eq!(NEEDS_PERMISSION, 3);
    }

    #[test]
    fn summary_clips() {
        assert_eq!(summary(&"x".repeat(300)).chars().count(), 201);
    }
}
