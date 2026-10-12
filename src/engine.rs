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
    let mut msg = json!({"role": "user", "content": user.clone()});
    let imgs = crate::image::take(&m.name);
    if !imgs.is_empty() {
        msg["images"] = imgs.into();
    }
    m.push(msg);
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
/// The same call with the same result this many times in a turn ends the turn.
const REPEAT_STOP: u32 = 5;
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
    // the same call with the same result, counted over the turn: a model going round in circles
    let mut repeats: std::collections::HashMap<u64, u32> = Default::default();
    let mut circling: Option<String> = None;
    loop {
        compact(hub, m, false)?;
        // what teammates' agents just changed, before this session builds on stale ideas
        // the files this session has looked at or changed get full notes, others a headline
        let seen: Vec<String> = lock(&m.msgs).iter().filter_map(|x| x["key"].as_str())
            .filter_map(|k| k.split_once(':')).filter(|(t, _)| matches!(*t, "read_file" | "edit_file" | "write_file"))
            .map(|(_, p)| p.to_string()).collect();
        let notes = crate::team::notes_for(&m.name, &m.cwd(), &seen);
        if !notes.is_empty() {
            hub.emit(&m.name, Event::Notice(format!("{} teammate change{} noted", notes.len(), if notes.len() == 1 { "" } else { "s" })));
            m.push(json!({"role": "user", "content": format!(
                "[team] Teammates changed code in this project. If your work relies on it, adapt. This is background, not a request: don't mention it unless it matters to what you're asked.\n\n{}",
                notes.join("\n\n"))}));
        }
        // answers to questions this session sent teammates' agents while it kept working
        for a in crate::team::answers_for(&m.name) {
            hub.emit(&m.name, Event::Notice("teammate's answer arrived".into()));
            m.push(json!({"role": "user", "content": a}));
        }
        // a helper's result or the waiting session's follow-up, arrived mid-turn: this turn takes
        // it in, instead of answering it in a turn of its own afterwards
        for f in hub.take_followups(m) {
            m.push(json!({"role": "user", "content": f}));
        }
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
            hub.emit(&m.name, Event::Done { tokens: t.tokens });
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
            let n = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (name, args.to_string(), &result).hash(&mut h);
                let n = repeats.entry(h.finish()).or_insert(0);
                *n += 1;
                *n
            };
            m.push(msg);
            if n == 3 {
                circling = Some(format!("{name} {}", args.to_string().chars().take(80).collect::<String>()));
            }
            if n >= REPEAT_STOP {
                let note = format!("stopped: the same {name} call gave the same result {n} times. Tell me how to proceed.");
                hub.emit(&m.name, Event::Notice(note.clone()));
                m.push(json!({"role": "assistant", "content": note}));
                return Ok(note);
            }
        }
        // the third identical call and result: say so once, before the next try
        if let Some(c) = circling.take() {
            hub.emit(&m.name, Event::Notice(format!("the model is repeating itself ({c}); told it to change approach")));
            m.push(json!({"role": "user", "content": format!(
                "[harness] You have now made this exact call three times and got the same result each time: {c}. \
Repeating it won't change the outcome. Re-read the result, find out why it happens, and try a different approach; \
if you're blocked, say what you need.")}));
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

/// After a quiet call outside a turn: call() marked the session working.
fn settle(hub: &Arc<Hub>, m: &Arc<Member>) {
    if !lock(&m.ctl).running {
        *lock(&m.status) = "idle";
        hub.changed();
    }
}

/// Answer a teammate agent's question from a read-only copy of this session: everything it
/// knows, plus a few read-only lookups. Nothing is added to the session itself; the owner pays.
/// Returns (answer, names the answer promises).
pub fn answer_question(hub: &Arc<Hub>, m: &Arc<Member>, who: &str, question: &str) -> (String, Vec<String>) {
    const LOOKS: usize = 8;
    let mut msgs = lock(&m.msgs).clone();
    repair(&mut msgs); // it may be mid-turn: close any open tool calls in the copy
    msgs.push(json!({"role": "user", "content": format!("[team question from {who}, a teammate's agent] {question}\n\n\
Answer from what you know about your own work in this session. If you need to, look at the code with the read-only tools \
(at most {LOOKS} calls); you can't change anything now and nothing you do here enters your own session. Be short and concrete: names, \
signatures, what's changing and when. If you don't know, or someone else owns it, say so and name them if you can. \
The asker has their own clone: your uncommitted files aren't in it (they can read them with read_team_file, or get them once you push), \
so never say they're in a shared workspace. \
End with one line 'RELIED ON: <comma-separated function, type or field names your answer promises>' or 'RELIED ON: none'.")}));
    let schemas: Vec<Value> = lock(&m.set).tools.iter()
        .filter(|t| matches!(t["function"]["name"].as_str(), Some("read_file" | "grep" | "find_references"))).cloned().collect();
    let mut cwd = m.cwd();
    let mut text = String::new();
    for round in 0..=LOOKS {
        if round == LOOKS {
            msgs.push(json!({"role": "user", "content": "[harness] No more lookups: answer now with what you have."}));
        }
        let msg = match call(hub, m, &msgs, &schemas, true) {
            Ok(x) => x,
            Err(e) => { text = format!("(couldn't answer: {e})"); break }
        };
        msgs.push(msg.clone());
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() || round == LOOKS {
            text = content(&msg).to_string();
            break;
        }
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or("").to_string();
            let args = parse_args(&c["function"]["arguments"]);
            let guarded = crate::sandbox::guard(&name, &args, &cwd, &m.root, &hub.cfg);
            let out = if let Some(why) = guarded {
                format!("error: not here: it {why}")
            } else if schemas.iter().any(|s| s["function"]["name"] == name.as_str()) && read_only(hub, &name, &args) {
                tools::call(&name, &args, &hub.cfg, &mut cwd, &hub.limits(m), &crate::sandbox::Ctx { root: m.root.clone(), proxy: None, tab: m.name.clone() })
            } else {
                format!("error: {name} isn't available here: read_file, grep and find_references only")
            };
            msgs.push(json!({"role": "tool", "tool_name": name, "content": out}));
        }
    }
    settle(hub, m);
    let (text, relied) = split_relied(&text);
    (if text.is_empty() { "(no answer)".into() } else { text }, relied)
}

/// "… RELIED ON: get_user, `UserDto.name`, the schema" -> the answer without it, and the code
/// names it lists (prose like "the schema" dropped). Models put the line anywhere.
fn split_relied(text: &str) -> (String, Vec<String>) {
    let Some(i) = text.rfind("RELIED ON:") else { return (text.trim().to_string(), vec![]) };
    let names = text[i + "RELIED ON:".len()..].lines().next().unwrap_or("").split(',')
        .map(|x| x.trim().trim_matches(|c| c == '`' || c == '.' || c == '*').trim_end_matches("()"))
        .map(|x| x.rsplit('.').next().unwrap_or(x).to_string())
        .filter(|x| x.len() > 1 && x != "none" && x.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && x.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .collect();
    let rest = text[i..].split_once('\n').map(|(_, r)| r).unwrap_or("");
    (format!("{}\n{rest}", &text[..i]).trim().to_string(), names)
}

#[cfg(test)]
mod relied_tests {
    #[test]
    fn relied_on_anywhere() {
        let (t, n) = super::split_relied("It won't change. RELIED ON: price_with_tax, function signatures");
        assert_eq!((t.as_str(), n), ("It won't change.", vec!["price_with_tax".to_string()]));
        let (t, n) = super::split_relied("Yes.\nRELIED ON: `get_user()`, UserDto.full_name\n");
        assert_eq!((t.as_str(), n), ("Yes.", vec!["get_user".to_string(), "full_name".to_string()]));
        assert_eq!(super::split_relied("No idea.\nRELIED ON: none").1, Vec::<String>::new());
        assert_eq!(super::split_relied("plain").0, "plain");
    }
}

/// Does this change to `func` break what a teammate counts on? Some(reason) if it does. When the
/// check itself fails, the edit goes ahead: a promise mustn't block work on a flaky call.
fn breaks(hub: &Arc<Hub>, m: &Arc<Member>, func: &str, promise: &str, before: &str, after: &str) -> Option<String> {
    let prompt = format!("A teammate's code counts on this staying true about `{func}`:\n\"{promise}\"\n\n\
Before the change:\n```\n{before}\n```\n\nAfter the change:\n```\n{after}\n```\n\n\
Does the change break that? Only count real differences in behaviour or signature that the teammate's statement depends on; \
refactors, renamed locals, comments and added optional behaviour don't break it. Reply with exactly one line: KEEPS, or BREAKS: <why, in one sentence>.");
    let r = call(hub, m, &[json!({"role": "user", "content": prompt})], &[], true);
    let text = content(&r.ok()?).to_string();
    let line = text.lines().map(str::trim).find(|l| l.starts_with("BREAKS") || l.starts_with("KEEPS"))?;
    line.strip_prefix("BREAKS").map(|w| w.trim_start_matches([':', ' ']).trim().to_string()).map(|w| if w.is_empty() { "it changes what they rely on".into() } else { w })
}

/// The fact sheet's "decided" and "next" lines after a finished task, from one quiet call.
pub fn sheet(hub: &Arc<Hub>, m: &Arc<Member>, request: &str, result: &str) -> Option<(String, String)> {
    let clip = |s: &str| s.chars().take(2000).collect::<String>();
    let prompt = format!("Teammates working on the same project read a short fact sheet about this session. From the task and \
its result below, write exactly two lines and nothing else:\nDECIDED: <the one decision teammates should know (a name, an API shape, \
an approach), or none>\nNEXT: <what this session is likely to do next, or none>\n\nTask: {}\n\nResult: {}", clip(request), clip(result));
    let r = call(hub, m, &[json!({"role": "user", "content": prompt})], &[], true);
    settle(hub, m);
    let text = content(&r.ok()?).to_string();
    let get = |k: &str| text.lines().find_map(|l| l.trim().strip_prefix(k)).map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty() && x.to_lowercase() != "none").unwrap_or_default();
    Some((get("DECIDED:"), get("NEXT:")))
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
        let think = if !s.think { String::new() } else if s.effort.is_empty() { "on".into() } else { s.effort.clone() };
        (think, s.model.clone(), s.provider.clone())
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
                m.thinking.store(thinking, Relaxed);
                lock(&m.turn).streamed += 1;
                if !thinking {
                    hub.emit(&m.name, Event::Token(text.to_string()));
                }
            }
            true
        };
        // with thinking on, the wait before the first answer token is the model reasoning; some
        // providers (the ChatGPT plan) only send their reasoning summary after it's done
        if !quiet { m.thinking.store(!think.is_empty(), Relaxed) }
        let r = backend::chat(&prov, &model, msgs, schemas, &think, ctx, &cfg.keep_alive, &mut on);
        m.thinking.store(false, Relaxed);
        match r {
            // a server that is loading, overloaded or briefly down: retry, but only before any
            // output, so nothing is shown twice
            Err(e) if attempt < 2 && first.is_none() && backend::transient(&e) && !m.cancel.load(Relaxed) => {
                attempt += 1;
                // a rate limit wants longer than a hiccup: the provider's Retry-After, else 5 s then 15 s
                let wait = match &e {
                    Error::Limited(_, w) => w.unwrap_or(if attempt == 1 { 5 } else { 15 }),
                    _ => 2 * attempt,
                };
                hub.emit(&m.name, Event::Notice(format!("{e}; retrying in {wait} s ({attempt}/2)")));
                // in steps, so Esc still stops it
                for _ in 0..wait * 10 {
                    if m.cancel.load(Relaxed) { break }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            r => break (r, t0, first),
        }
    };
    let r = match r {
        Err(ref e @ (Error::Connect(_) | Error::Limited(..))) if !cfg.fallback_model.is_empty() && !m.cancel.load(Relaxed) => {
            let what = if matches!(e, Error::Limited(..)) { "is rate-limited" } else { "is unreachable" };
            let (fp, fm) = crate::providers::parse_model(&cfg.fallback_model);
            if (fp.as_str(), fm.as_str()) == (provider.as_str(), model.as_str()) {
                return Err(Error::Other(format!("{} {what}", crate::providers::display(&provider, &model))));
            }
            hub.emit(&m.name, Event::Notice(format!("{} {what}, falling back to {}", crate::providers::display(&provider, &model), cfg.fallback_model)));
            {
                let mut s = lock(&m.set);
                (s.provider, s.model) = (fp, fm);
            }
            return call_locked(hub, m, msgs, schemas, quiet);
        }
        Err(Error::Limited(why, _)) => return Err(Error::Other(format!(
            "{} is rate-limited: {why}\nWait a minute, pick another model with /model, or set fallback_model in config.toml to switch on its own.",
            crate::providers::display(&provider, &model)))),
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
    let (price, paid) = (crate::usage::price(&provider, &model), crate::usage::paid(&provider, &model));
    {
        let mut t = lock(&m.turn);
        t.usage.add(r.usage, price, paid);
        if r.plan_pct.is_some() { t.plan_pct = r.plan_pct }
    }
    let spent = { let mut s = lock(&hub.spent); s.add(r.usage, price, paid); s.cost };
    if let Some(w) = hub.cost_warn.check(spent, cfg.cost_warn) {
        hub.emit(&m.name, Event::Notice(w));
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
    if name == "tell_team" {
        let (to, text) = (args["to"].as_str().unwrap_or("").trim_start_matches('@'), args["text"].as_str().unwrap_or(""));
        let (who, session) = to.split_once('/').unwrap_or((to, ""));
        let session = if session.is_empty() {
            match crate::team::people().into_iter().find(|p| p.0 == who).and_then(|p| p.3.into_iter().next()) {
                Some(s) => s,
                None => return format!("error: {who} isn't a teammate with a session open right now; tell the user"),
            }
        } else { session.to_string() };
        return match crate::team::send_from(who, &session, &format!("[from {}'s agent] {text}", crate::team::me().unwrap_or_default()), &m.name) {
            Ok(m) => m,
            Err(e) => format!("error: {e}"),
        };
    }
    if name == "read_team_file" {
        return crate::team::read_team_file(&m.cwd(), args["to"].as_str().unwrap_or(""), args["path"].as_str().unwrap_or(""),
                                           args["start_line"].as_u64(), args["end_line"].as_u64());
    }
    if name == "list_team_files" {
        return crate::team::list_team_files(&m.cwd(), args["to"].as_str().unwrap_or(""));
    }
    if name == "ask_team" {
        return crate::team::ask_team(&m.name, &m.cwd(), args["question"].as_str().unwrap_or(""),
                                     args["to"].as_str().unwrap_or(""), args["deep"].as_bool().unwrap_or(false));
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
    // a teammate is changing the same function right now: wait, before even asking the user
    let touched: Vec<crate::team::Touched> = tools::preview(&cwd, name, &args).into_iter()
        .filter_map(|(full, after)| crate::team::touched(&cwd, &full, &after)).collect();
    if let Some((notice, msg)) = touched.iter().find_map(|t| crate::team::check_edit(&m.name, t)) {
        hub.emit(&m.name, Event::Notice(notice));
        return msg;
    }
    // behaviour promises teammates hold on these functions: a quick model check, paid by the editor
    for t in &touched {
        for (who, func, promise, before, after) in crate::team::promises_at_risk(&m.name, t) {
            hub.emit(&m.name, Event::Notice(format!("checking the edit against what {who} counts on in {func}")));
            if let Some(why) = breaks(hub, m, &func, &promise, &before, &after) {
                let (notice, msg) = crate::team::promise_held(t, &who, &func, &promise, &why);
                hub.emit(&m.name, Event::Notice(notice));
                return msg;
            }
        }
    }
    // a secret, a write outside the project, a protected path, an unsandboxed run: asked in every mode
    if let Some(why) = crate::sandbox::guard(name, &args, &cwd, &m.root, &hub.cfg) {
        if !hub.confirm(m, name, &args, Some(&why)) {
            return format!("denied by user: it {why}. Don't try another way around it; ask the user if it's needed.");
        }
    } else if tools::risky(name, &args, &hub.cfg) && !allowed(&lock(&m.set).allow, name, &args)
        && !hub.confirm(m, name, &args, None) {
        return "denied by user".into();
    }
    lock(&m.turn).ckpt.snapshot(name, &args, &cwd);
    let sb = if name == "bash" { hub.sandbox_ctx(m) } else { crate::sandbox::Ctx { root: m.root.clone(), proxy: None, tab: m.name.clone() } };
    let mut out = tools::call(name, &args, &hub.cfg, &mut cwd, &hub.limits(m), &sb);
    if !out.starts_with("error") {
        for t in &touched {
            crate::team::claim(&m.name, &lock(&m.purpose).clone(), t);
        }
    }
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

#[cfg(test)]
mod guard_tests {
    use super::*;
    use serde_json::json;

    /// auto mode and -y approve ordinary calls, but never one the safety guard flags: that asks, and
    /// with nobody to say yes it is refused. Every target here is harmless if the guard ever failed.
    #[test]
    fn auto_mode_and_yes_still_ask_about_guarded_calls() {
        let _env = lock(&crate::hub::tests::ENV);
        let root = crate::sandbox::canon(&std::env::temp_dir()).join(format!("qh-guard-e2e-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        std::fs::write(root.join(".env"), "API_KEY=s3cr3t-value\n").unwrap();
        let asked = Arc::new(Mutex::new(vec![]));
        let seen = asked.clone();
        let emit: crate::hub::Emit = Box::new(move |_, ev| if let Event::Confirm { tool, reply, guard, .. } = ev {
            lock(&seen).push((tool, guard));
            let _ = reply.send(crate::hub::Answer::No);
        });
        let mut cfg = crate::config::load_defaults();
        cfg.sandbox = true;
        let hub = Hub::new(cfg, Some(format!("guard-{}", std::process::id())), emit, true);
        hub.set_mode("auto");
        let m = hub.add("g", None, "", "", None, false, Some(root.clone())).unwrap();
        let outside = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/qh-guard-outside.txt");
        let out = dispatch(&hub, &m, "write_file", &json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh\necho hi"}));
        assert!(out.starts_with("denied") && !root.join(".git/hooks/pre-commit").exists(), "{out}");
        let out = dispatch(&hub, &m, "write_file", &json!({"path": outside, "content": "x"}));
        assert!(out.starts_with("denied") && !outside.exists(), "{out}");
        let out = dispatch(&hub, &m, "read_file", &json!({"path": ".env"}));
        assert!(out.starts_with("denied") && !out.contains("s3cr3t"), "{out}");
        assert_eq!(lock(&asked).len(), 3);
        assert!(lock(&asked).iter().all(|(_, g)| *g), "asked as guarded calls");
        // an ordinary edit in the project goes through without asking in auto
        let out = dispatch(&hub, &m, "write_file", &json!({"path": "src/ok.rs", "content": "// ok"}));
        assert!(root.join("src/ok.rs").exists() && lock(&asked).len() == 3, "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
