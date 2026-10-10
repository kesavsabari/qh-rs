//! One turn of one session: call the model, run its tools, repeat until it
//! answers without tools. Events go to the hub's emitter as they happen.
use std::sync::Mutex;
use crate::backend::{self, Error};
use crate::hooks;
use crate::context::{self, chars, content, estimate, role};
use crate::hub::{lock, Event, Hub, Member, HUB_TOOLS};
use crate::{session, tools};
use serde_json::{json, Value};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Instant;

const MAX_ERRORS: usize = 5;

/// Runs a turn and returns the final assistant text.
pub fn run(hub: &Arc<Hub>, m: &Arc<Member>, user: String) -> String {
    m.push(json!({"role": "user", "content": user.clone()}));
    let fin = turn(hub, m, &user).unwrap_or_else(|e| {
        let e = if matches!(e, Error::Cancelled) { "interrupted".to_string() } else { format!("error: {e}") };
        hub.emit(&m.name, Event::Notice(e.clone()));
        e
    });
    let mut msgs = lock(&m.msgs);
    repair(&mut msgs);
    m.touched(&msgs);
    session::save(&lock(&m.session), &msgs);
    fin
}

/// A file the user asked to have written ("write … to answer.txt"), if any.
fn wanted_file(user: &str) -> Option<String> {
    static ASK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b(write|save|put|create|store|record)\b").unwrap()
    });
    static FILE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[\w./-]+\.[A-Za-z0-9]{1,5}\b").unwrap()
    });
    if !ASK.is_match(user) {
        return None;
    }
    FILE.find_iter(user).map(|f| f.as_str().trim_end_matches('.').to_string())
        .find(|f| !f.contains("://") && !f.starts_with("http") && f.chars().any(|c| c.is_alphabetic()))
}

// handing the job to another session counts as doing it
const WRITERS: [&str; 7] = ["write_file", "edit_file", "rename_symbol", "bash", "send_message", "ask", "spawn"];

/// A build or test command, as opposed to looking around.
fn checks(cmd: &str) -> bool {
    static CHECK: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\b(cargo (check|build|test|clippy|run)|pytest|python3? -m (pytest|py_compile|unittest)|npm (test|run (build|test|lint))|pnpm (test|build)|go (build|test|vet)|make\b|tsc\b|mvn |gradle)").unwrap()
    });
    CHECK.is_match(cmd)
}

/// Code project markers whose build/test command the model should run after editing.
fn buildable(cwd: &std::path::Path) -> bool {
    ["Cargo.toml", "pyproject.toml", "package.json", "go.mod", "Makefile", "setup.py"].iter().any(|f| cwd.join(f).exists())
}

fn turn(hub: &Arc<Hub>, m: &Arc<Member>, user: &str) -> Result<String, Error> {
    let mut errors = 0;
    let (want, mut wrote, mut nudged) = (wanted_file(user), false, false);
    // code edited since the last build/test run, and whether we already asked for one
    let (mut unchecked, mut check_nudged) = (false, false);
    let mut stops = 0; // stop-hook refusals this turn, capped so a broken hook can't loop forever
    loop {
        compact(hub, m, false)?;
        let (snapshot, schemas) = (lock(&m.msgs).clone(), hub.visible_tools(m));
        let msg = call(hub, m, &snapshot, &schemas, false)?;
        m.push(msg.clone());
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() {
            // the model stated an answer but never wrote the file it was asked for: remind it once
            if let (Some(f), false, false) = (&want, wrote, nudged) {
                nudged = true;
                hub.emit(&m.name, Event::Notice(format!("reminding the model to write {f}")));
                m.push(json!({"role": "user", "content": format!(
                    "[harness] You were asked to write {f} but no tool has written it yet. Do that now with a tool, then reply.")}));
                continue;
            }
            // code changed and nothing built or tested it since: ask once for the check
            if unchecked && !check_nudged && lock(&m.set).profile == "coding" && buildable(&m.cwd()) {
                check_nudged = true;
                hub.emit(&m.name, Event::Notice("reminding the model to build/test its changes".into()));
                m.push(json!({"role": "user", "content": "[harness] You changed code but haven't built or tested it since. \
Run the project's build or test command now, fix what fails, then reply."}));
                continue;
            }
            // a stop hook can refuse the ending (tests still red, lint failing): its reason goes back in
            if stops < 3 {
                let input = json!({"event": "stop", "reply": content(&msg), "session": m.name, "cwd": m.cwd()});
                if let hooks::Outcome::Block(why) = hooks::fire(&hub.cfg.hooks, "stop", "", &input, &m.cwd()) {
                    stops += 1;
                    hub.emit(&m.name, Event::Notice(format!("stop hook: {}", why.lines().next().unwrap_or(""))));
                    m.push(json!({"role": "user", "content": format!("[hook] Not done yet: {why}")}));
                    continue;
                }
            }
            let t = lock(&m.turn);
            hub.emit(&m.name, Event::Done { tokens: t.tokens, tps: t.tps });
            return Ok(content(&msg).to_string());
        }
        // normalise up front so the UI shows the arguments the tool will actually get
        let schemas = lock(&m.set).tools.clone();
        let parsed: Vec<(String, Value)> = calls.iter().map(|c| {
            let name = c["function"]["name"].as_str().unwrap_or("").to_string();
            let mut args = parse_args(&c["function"]["arguments"]);
            if let Some(s) = schemas.iter().find(|t| t["function"]["name"] == name.as_str()) {
                if args.is_object() {
                    args = coerce(&args, &s["function"]["parameters"]);
                }
            }
            (name, args)
        }).collect();
        for (name, args) in &parsed {
            hub.emit(&m.name, Event::ToolCall { name: name.clone(), args: args.clone() });
        }
        if parsed.iter().any(|(n, _)| crate::browser::TOOLS.contains(&n.as_str())) {
            lock(&m.turn).browsing = true;
        }
        let results: Vec<String> = if parsed.len() > 1 && parsed.iter().all(|(n, a)| read_only(hub, n, a)) {
            std::thread::scope(|s| {
                let hs: Vec<_> = parsed.iter().map(|(n, a)| s.spawn(|| dispatch(hub, m, n, a))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| "error: tool panicked".into())).collect()
            })
        } else {
            let mut out = vec![];
            for (n, a) in &parsed {
                if m.cancel.load(Relaxed) {
                    out.push("interrupted by user".to_string());
                    continue;
                }
                out.push(dispatch(hub, m, n, a));
            }
            out
        };
        for ((name, args), result) in parsed.iter().zip(results) {
            wrote |= WRITERS.contains(&name.as_str()) && !result.starts_with("error") && result != "denied by user";
            hub.emit(&m.name, Event::ToolResult { name: name.clone(), result: result.clone() });
            let mut msg = json!({"role": "tool", "tool_name": name, "content": result});
            if crate::browser::TOOLS.contains(&name.as_str()) {
                msg["key"] = "page:".into();
                if let Some(img) = crate::browser::take_image().filter(|_| name == "screenshot") {
                    msg["images"] = json!([img]);
                }
            }
            if let Some(p) = args["path"].as_str() {
                msg["key"] = format!("{name}:{p}").into();
                if (name == "write_file" || name == "edit_file") && !result.starts_with("error") && result != "denied by user" {
                    lock(&m.turn).changed.insert(p.to_string());
                    hub.note_write(&m.name, p, &m.cwd());
                    unchecked |= !p.ends_with(".md") && !p.ends_with(".txt");
                }
            }
            if name == "bash" && checks(args["cmd"].as_str().unwrap_or("")) {
                unchecked = false;
            }
            errors = if result.starts_with("error") { errors + 1 } else { 0 };
            m.push(msg);
        }
        if m.cancel.load(Relaxed) {
            return Err(Error::Cancelled);
        }
        if errors >= MAX_ERRORS {
            let note = format!("stopped: {errors} tool errors in a row. Tell me what you need.");
            hub.emit(&m.name, Event::Notice(note.clone()));
            m.push(json!({"role": "assistant", "content": note}));
            return Ok(note);
        }
    }
}

/// After an interrupt, give every dangling tool call a result so the history
/// stays valid for the next request.
fn repair(msgs: &mut Vec<Value>) {
    let mut pending: Vec<String> = vec![];
    for x in msgs.iter() {
        if role(x) == "assistant" {
            pending = x["tool_calls"].as_array().into_iter().flatten()
                .map(|c| c["function"]["name"].as_str().unwrap_or("").to_string()).collect();
        } else if role(x) == "tool" && !pending.is_empty() {
            pending.remove(0);
        }
    }
    for n in pending {
        msgs.push(json!({"role": "tool", "tool_name": n, "content": "interrupted by user"}));
    }
}

pub fn compact(hub: &Arc<Hub>, m: &Arc<Member>, force: bool) -> Result<(), Error> {
    let (_, at) = hub.ctx_for(m, true);
    let keep = hub.limits(m).keep_recent;
    let ratio = lock(&m.turn).ratio;
    let (old, after, first) = {
        let mut msgs = lock(&m.msgs);
        let est = estimate(&msgs, ratio);
        if !force && est < at {
            return Ok(());
        }
        context::stub_superseded(&mut msgs);
        if force || estimate(&msgs, ratio) >= at {
            context::stub_tool_outputs(&mut msgs, keep);
        }
        let after = estimate(&msgs, ratio);
        m.touched(&msgs);
        // only report a stub pass that actually freed something (>2% of the window)
        if after + at / 50 < est {
            hub.emit(&m.name, Event::Compact { stage: "stub", before: est, after });
        }
        if !force && after < at {
            return Ok(());
        }
        let cut = context::split_point(&msgs, keep);
        // the opening request stays verbatim: a worker's whole job is in it
        let first = if msgs.len() > 2 && context::role(&msgs[1]) == "user" && cut > 2 { 2 } else { 1 };
        let old = msgs[first..cut].to_vec();
        // summarising a sliver costs a model call and can grow the context
        if (chars(&old) as f64) < 0.3 * chars(&msgs) as f64 {
            if force {
                hub.emit(&m.name, Event::Notice("nothing old enough to summarise".into()));
            }
            return Ok(());
        }
        (old, after, first)
    };
    let text = context::render(&old);
    let mut start = text.len().saturating_sub(30000);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let prompt = format!("{}{}", context::SUMMARY_PROMPT, &text[start..]);
    let summary = call(hub, m, &[json!({"role": "user", "content": prompt})], &[], true)?;
    let changed = lock(&m.turn).changed.iter().cloned().collect::<Vec<_>>().join(", ");
    let facts = if changed.is_empty() { String::new() } else { format!("\nFiles changed this session: {changed}") };
    let mut msgs = lock(&m.msgs);
    // the history may have grown while summarising; keep everything after the old part
    let n_old = old.len();
    if msgs.len() < first + n_old {
        return Ok(());
    }
    let mut out = msgs[..first].to_vec();
    out.push(json!({"role": "user", "content": format!("[Summary of earlier work]\n{}{facts}", content(&summary))}));
    if msgs.get(first + n_old).is_some_and(|t| context::role(t) == "user") {
        out.push(json!({"role": "assistant", "content": "Understood, continuing from the summary."}));
    }
    out.extend_from_slice(&msgs[first + n_old..]);
    if chars(&out) >= chars(&msgs) {
        return Ok(());
    }
    *msgs = out;
    m.touched(&msgs);
    session::save(&lock(&m.session), &msgs);
    hub.emit(&m.name, Event::Compact { stage: "summary", before: after, after: estimate(&msgs, ratio) });
    Ok(())
}

/// A 2-5 word label for the job a request starts, from one quiet model call.
pub fn label(hub: &Arc<Hub>, m: &Arc<Member>, request: &str) -> Option<String> {
    let req: String = request.chars().take(1500).collect();
    let prompt = format!("Name the job this request starts in 2 to 5 words, lowercase, no punctuation. \
Reply with the words only.\n\nRequest: {req}");
    let r = call(hub, m, &[json!({"role": "user", "content": prompt})], &[], true);
    if !lock(&m.ctl).running {
        *lock(&m.status) = "idle"; // call() marks the session working
        hub.changed();
    }
    let text = content(&r.ok()?).lines().find(|l| !l.trim().is_empty())?.to_string();
    let words: Vec<&str> = text.trim_matches(|c: char| !c.is_alphanumeric()).split_whitespace().take(5).collect();
    let role = words.join(" ").chars().take(40).collect::<String>().to_lowercase();
    (!role.is_empty()).then_some(role)
}

/// One model call under the shared model lock, streaming tokens as events.
/// `quiet` calls (summaries) stream nothing and leave the token counts alone.
/// One model call. Local models take turns on the GPU, in arrival order; cloud models
/// run in parallel.
fn call(hub: &Arc<Hub>, m: &Arc<Member>, msgs: &[Value], schemas: &[Value], quiet: bool) -> Result<Value, Error> {
    let local = { let s = lock(&m.set); crate::providers::is_local(&s.provider, &s.model) };
    if !local {
        *lock(&m.status) = "working";
        hub.changed();
        return call_locked(hub, m, msgs, schemas, quiet);
    }
    let _hold = hub.model.acquire(|| {
        *lock(&m.status) = "waiting-model";
        hub.changed();
    });
    *lock(&hub.holder) = m.name.clone();
    *lock(&m.status) = "working";
    hub.changed();
    let r = call_locked(hub, m, msgs, schemas, quiet);
    lock(&hub.holder).clear();
    hub.changed();
    r
}

fn call_locked(hub: &Arc<Hub>, m: &Arc<Member>, msgs: &[Value], schemas: &[Value], quiet: bool) -> Result<Value, Error> {
    let (think, model, provider) = {
        let s = lock(&m.set);
        (s.think, s.model.clone(), s.provider.clone())
    };
    let Some(prov) = crate::providers::resolve(&provider) else {
        return Err(Error::Other(format!("no provider {provider}; /provider lists them")));
    };
    let cfg = &hub.cfg;
    let ctx = hub.ctx_for(m, true).0;
    let mut attempt = 0;
    let (r, t0, first) = loop {
        let t0 = Instant::now();
        let mut first: Option<f64> = None;
        let mut on = |thinking: bool, text: &str| {
            if m.cancel.load(Relaxed) {
                return false;
            }
            first.get_or_insert_with(|| t0.elapsed().as_secs_f64());
            if !quiet {
                lock(&m.turn).streamed += 1;
                if !thinking {
                    hub.emit(&m.name, Event::Token(text.to_string()));
                }
            }
            true
        };
        let r = backend::chat(&prov, &model, msgs, schemas, think, ctx, &cfg.keep_alive, &mut on);
        match r {
            // a server that is loading, overloaded or briefly down: retry, but only before any
            // output, so nothing is shown twice
            Err(e) if attempt < 2 && first.is_none() && backend::transient(&e) && !m.cancel.load(Relaxed) => {
                attempt += 1;
                hub.emit(&m.name, Event::Notice(format!("{e}; retrying ({attempt}/2)")));
                std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
            }
            r => break (r, t0, first),
        }
    };
    let r = match r {
        Err(Error::Connect(_)) if !cfg.fallback_model.is_empty() => {
            let (fp, fm) = crate::providers::parse_model(&cfg.fallback_model);
            if (fp.as_str(), fm.as_str()) == (provider.as_str(), model.as_str()) {
                return Err(Error::Other(format!("{} is unreachable", crate::providers::display(&provider, &model))));
            }
            hub.emit(&m.name, Event::Notice(format!("{} unreachable, falling back to {}", prov.name, cfg.fallback_model)));
            {
                let mut s = lock(&m.set);
                (s.provider, s.model) = (fp, fm);
            }
            return call_locked(hub, m, msgs, schemas, quiet);
        }
        r => r?,
    };
    if !quiet {
        let secs = t0.elapsed().as_secs_f64();
        let gen = (secs - first.unwrap_or(0.0)).max(1e-6);
        let mut t = lock(&m.turn);
        if r.total > 0 {
            t.tps = ((r.total - r.prompt) as f64 / gen * 10.0).round() / 10.0;
        }
        if r.prompt > 0 {
            // total already includes the reply, which gets appended right after
            t.tokens = r.total;
            t.chars_at = chars(msgs) + chars(std::slice::from_ref(&r.msg));
            t.streamed = 0;
            t.ratio = (chars(msgs) as f64 / r.prompt as f64).max(1.5);
        }
    }
    // every call costs, quiet ones (summaries, role names) included
    if let Some(info) = crate::providers::info(&provider, &model) {
        let out = r.total.saturating_sub(r.prompt);
        lock(&m.turn).cost += (r.prompt as f64 * info.price_in + out as f64 * info.price_out) / 1e6;
    }
    Ok(r.msg)
}

fn read_only(hub: &Hub, name: &str, a: &Value) -> bool {
    a.is_object() && !HUB_TOOLS.contains(&name) && !matches!(name, "remember" | "bash")
        && tools::builtin_schema(name).is_some() && !tools::risky(name, a, &hub.cfg)
}

/// The model's own checklist, per session. Resent in full each call so a small model sees
/// the whole plan in its latest tool result instead of having to remember it.
fn todo(session: &str, a: &Value) -> String {
    static LISTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let Some(items) = a["items"].as_array() else {
        return "error: items must be a list of {text, status}".into();
    };
    let rows: Vec<String> = items.iter().map(|i| {
        let mark = match i["status"].as_str().unwrap_or("pending") {
            "done" | "completed" => "[x]",
            "doing" | "in_progress" => "[~]",
            _ => "[ ]",
        };
        format!("{mark} {}", i["text"].as_str().or_else(|| i.as_str()).unwrap_or(""))
    }).collect();
    let text = rows.join("\n");
    let mut l = lock(&LISTS);
    l.retain(|(s, _)| s != session);
    l.push((session.into(), text.clone()));
    let left = rows.iter().filter(|r| !r.starts_with("[x]")).count();
    format!("todo saved ({left} open):\n{text}")
}

fn dispatch(hub: &Arc<Hub>, m: &Arc<Member>, name: &str, args: &Value) -> String {
    let Some(schema) = lock(&m.set).tools.iter().find(|t| t["function"]["name"] == name).cloned() else {
        let names: Vec<String> = lock(&m.set).tools.iter()
            .filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect();
        return format!("error: unknown tool {name}. Available: {}", names.join(", "));
    };
    let params = &schema["function"]["parameters"];
    if !args.is_object() {
        return format!("error: arguments must be a JSON object matching {params}");
    }
    let args = coerce(args, params);
    if HUB_TOOLS.contains(&name) {
        return hub.tool(m, name, &args);
    }
    if name == "todo" {
        return todo(&m.name, &args);
    }
    if hub.mode_name() == "plan" && (tools::risky(name, &args, &hub.cfg) || matches!(name, "remember" | "send_message" | "spawn")) {
        return "error: plan mode is read-only. Finish exploring, then reply with a numbered plan; the user switches mode (Shift+Tab) to let you carry it out.".into();
    }
    let input = json!({"event": "pre_tool", "tool": name, "args": args, "session": m.name, "cwd": m.cwd()});
    if let hooks::Outcome::Block(why) = hooks::fire(&hub.cfg.hooks, "pre_tool", name, &input, &m.cwd()) {
        return format!("error: blocked by a hook: {why}");
    }
    let mut cwd = m.cwd();
    if let Some(err) = hub.claim_file(&m.name, name, &args, &cwd) {
        return err;
    }
    if tools::risky(name, &args, &hub.cfg) && !allowed(&lock(&m.set).allow, name, &args)
        && !hub.confirm(m, name, &args) {
        return "denied by user".into();
    }
    lock(&m.turn).ckpt.snapshot(name, &args, &cwd);
    let mut out = tools::call(name, &args, &hub.cfg, &mut cwd, &hub.limits(m));
    let input = json!({"event": "post_tool", "tool": name, "args": args, "result": out, "session": m.name, "cwd": cwd});
    match hooks::fire(&hub.cfg.hooks, "post_tool", name, &input, &cwd) {
        hooks::Outcome::Pass(s) | hooks::Outcome::Block(s) if !s.is_empty() => out += &format!("\n[hook] {s}"),
        _ => {}
    }
    if name == "bash" {
        *lock(&m.cwd) = cwd; // a cd in the command moves this session only
    }
    out
}

/// Auto-approve rules: 'pytest*' matches bash commands, 'edit_file:src/*' a tool + path.
fn allowed(rules: &[String], name: &str, a: &Value) -> bool {
    if name == "bash" {
        // Every part of a chain needs its own rule: 'pytest*' must not approve 'pytest; rm -rf ~'.
        return match tools::segments(a["cmd"].as_str().unwrap_or("")) {
            Some(segs) => !segs.is_empty() && segs.iter().all(|s| seg_ok(rules, s)),
            None => false,
        };
    }
    let target = format!("{name}:{}", a["path"].as_str().unwrap_or(""));
    rules.iter().any(|p| glob(p.as_bytes(), target.as_bytes()))
}

/// One simple command: read-only, a plain `cd`, or matched by a rule.
pub fn seg_ok(rules: &[String], s: &str) -> bool {
    tools::is_safe(s) || s == "cd" || (s.starts_with("cd ") && !s[3..].trim().contains(' '))
        || rules.iter().any(|p| glob(p.as_bytes(), s.as_bytes()))
}

/// fnmatch with * and ?.
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    match (p.first(), s.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob(&p[1..], s) || (!s.is_empty() && glob(p, &s[1..])),
        (Some(b'?'), Some(_)) => glob(&p[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob(&p[1..], &s[1..]),
        _ => false,
    }
}

/// Fix the type slips small models make: "3" for 3, 3 for "3", "true" for true,
/// and a single misnamed key ("command" for "cmd").
fn coerce(args: &Value, params: &Value) -> Value {
    let props = &params["properties"];
    let mut a = args.as_object().cloned().unwrap_or_default();
    let unknown: Vec<String> = a.keys().filter(|k| props.get(k.as_str()).is_none()).cloned().collect();
    let missing: Vec<&str> = params["required"].as_array().into_iter().flatten()
        .filter_map(|r| r.as_str()).filter(|r| !a.contains_key(*r)).collect();
    if unknown.len() == 1 && missing.len() == 1 {
        if let Some(v) = a.remove(&unknown[0]) {
            a.insert(missing[0].to_string(), v);
        }
    }
    for (k, v) in a.iter_mut() {
        let t = props[k.as_str()]["type"].as_str().unwrap_or("");
        let new = match (t, &*v) {
            ("integer", Value::String(s)) => s.trim().parse::<i64>().ok().map(Value::from),
            ("integer", Value::Number(n)) if n.is_f64() => n.as_f64().map(|f| Value::from(f as i64)),
            ("string", Value::String(_)) => None,
            ("string", x) => Some(Value::from(x.to_string())),
            ("boolean", Value::String(s)) => Some(Value::from(matches!(s.trim().to_lowercase().as_str(), "true" | "1" | "yes"))),
            _ => None,
        };
        if let Some(n) = new {
            *v = n;
        }
    }
    Value::Object(a)
}

fn parse_args(a: &Value) -> Value {
    match a {
        Value::String(s) if s.trim().is_empty() => json!({}),
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        Value::Null => json!({}),
        v => v.clone(),
    }
}

#[cfg(test)]
mod rule_tests {
    use super::*;
    use serde_json::json;

    fn ok(rules: &[&str], cmd: &str) -> bool {
        let r: Vec<String> = rules.iter().map(|s| s.to_string()).collect();
        allowed(&r, "bash", &json!({ "cmd": cmd }))
    }

    #[test]
    fn chains() {
        assert!(ok(&["cargo*"], "cd /home/k/x && cargo check --workspace 2>&1 | tail -30"));
        assert!(ok(&["mkdir*"], "cd ~ && mkdir -p a && cd a"));
        assert!(!ok(&["cargo*"], "cd x && cargo check; rm -rf ~"));
        assert!(!ok(&["cargo*"], "cargo check > out.txt"));
        assert!(!ok(&["cargo*"], "cargo build & rm x"));
        assert!(!ok(&["cargo*"], "cargo $(rm x)"));
        assert!(!ok(&[], "cd x && cargo check"));
    }
}
