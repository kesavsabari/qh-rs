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
use std::path::PathBuf;

const MAX_ERRORS: usize = 5;

/// Runs a turn and returns the final assistant text.
pub fn run(hub: &Arc<Hub>, m: &Arc<Member>, user: String) -> String {
    let mut msg = json!({"role": "user", "content": user.clone()});
    let imgs = crate::image::take(&m.name);
    if !imgs.is_empty() {
        msg["images"] = imgs.into();
    }
    m.push(msg);
    let (fin, edited) = turn(hub, m, &user).unwrap_or_else(|e| {
        let e = if matches!(e, Error::Cancelled) { "interrupted".to_string() } else { format!("error: {e}") };
        hub.emit(&m.name, Event::Notice(e.clone()));
        (e, false)
    });
    if edited {
        verify(hub, m);
    }
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

/// The project's check command: config `verify` (\"off\" = none, \"test\" = auto with tests), else
/// from the project's markers.
pub fn verify_cmd(cfg: &str, cwd: &std::path::Path) -> Option<String> {
    let cfg = cfg.trim();
    if cfg == "off" {
        return None;
    }
    if !cfg.is_empty() && cfg != "test" {
        return Some(cfg.to_string());
    }
    let test = cfg == "test";
    if cwd.join("Cargo.toml").exists() {
        return Some(if test { "cargo check -q && cargo test -q" } else { "cargo check -q" }.into());
    }
    if let Ok(p) = std::fs::read_to_string(cwd.join("package.json")) {
        let v: Value = serde_json::from_str(&p).unwrap_or_default();
        let has = |k: &str| v["scripts"][k].is_string();
        let order: &[&str] = if test { &["test", "typecheck", "build"] } else { &["typecheck", "build", "test"] };
        if let Some(k) = order.iter().find(|k| has(k)) {
            return Some(format!("npm run -s {k}"));
        }
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("pytest.ini").exists() || cwd.join("setup.py").exists() {
        return Some("python -m pytest -q -x".into());
    }
    if cwd.join("go.mod").exists() {
        return Some("go build ./...".into());
    }
    None
}

/// Runs the project's check through the bash sandbox and reports it to clients.
fn verify(hub: &Arc<Hub>, m: &Arc<Member>) {
    let cwd = m.cwd();
    let Some(cmd) = verify_cmd(&hub.cfg.verify, &cwd) else { return };
    let start = std::time::Instant::now();
    let script = format!("{cmd} 2>&1");
    let (ok, out) = match crate::sandbox::run(&hub.sandbox_ctx(m), &hub.cfg, &script, false, 120, &cwd) {
        Ok((o, killed, _)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            if killed { s.push_str("\n(killed after 120 s)") }
            (o.status.success() && !killed, s)
        }
        Err(e) => (false, format!("error: {e}")),
    };
    let lines: Vec<&str> = out.lines().collect();
    let tail = lines[lines.len().saturating_sub(40)..].join("\n");
    hub.emit(&m.name, Event::Verify { cmd, ok, secs: start.elapsed().as_secs_f64(), tail });
}

/// Saved memory into the system prompt, once per session (on its first turn, when the model's
/// window is known: 200k and up gets every file, smaller ones the index).
fn with_memory(hub: &Arc<Hub>, m: &Arc<Member>) {
    if !hub.cfg.memory || lock(&m.msgs).first().and_then(|x| x["content"].as_str()).is_none_or(|s| s.contains(crate::memory::MARK)) {
        return;
    }
    let ctx = hub.ctx_for(m, true).0;
    let team = crate::team::running_here(&m.root).then(|| crate::team::team_notes(&m.root)).flatten();
    let block = crate::memory::prompt_block(&m.root, ctx, team.as_deref());
    let mut msgs = lock(&m.msgs);
    if let Some(first) = msgs.first_mut().filter(|x| x["role"] == "system") {
        first["content"] = format!("{}{block}", first["content"].as_str().unwrap_or("")).into();
    }
    m.touched(&msgs);
}

fn turn(hub: &Arc<Hub>, m: &Arc<Member>, user: &str) -> Result<(String, bool), Error> {
    let mut edited = false;
    let mut errors = 0;
    let (want, mut wrote, mut nudged) = (wanted_file(user), false, false);
    // code edited since the last build/test run, and whether we already asked for one
    let (mut unchecked, mut check_nudged) = (false, false);
    let mut stops = 0; // stop-hook refusals this turn, capped so a broken hook can't loop forever
    // the same call with the same result, counted over the turn: a model going round in circles
    let mut repeats: std::collections::HashMap<u64, u32> = Default::default();
    let mut circling: Option<String> = None;
    let mut budget_ok = false; // "yes" to a budget ask covers the rest of this turn
    with_memory(hub, m);
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
        if !budget_ok {
            budget_ok = budget_gate(hub, m)?;
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
            lock(&m.turn).done = true; // Done is sent by the hub once the tab is free
            return Ok((content(&msg).to_string(), edited));
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
                    edited = true;
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
                return Ok((note, false));
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
            return Ok((note, false));
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

/// A second opinion from another model: no tools, nothing added to the session, the tab pays.
/// `model` empty = the configured second model.
pub fn ask_other(hub: &Arc<Hub>, m: &Arc<Member>, model: &str, question: &str, context: &str) -> String {
    let (p, mo) = if model.trim().is_empty() {
        let (cp, cm) = { let s = lock(&m.set); (s.provider.clone(), s.model.clone()) };
        match crate::route::other_model(&hub.cfg, &cp, &cm) {
            Some(x) => x,
            None => return "error: no other model to ask: name one, or set [routing] ask_model in config.toml".into(),
        }
    } else { crate::route::model_id(model.trim()) };
    let r = call_as(hub, m, Some((&p, &mo)), &crate::route::ask_msgs(question, context), &[], true);
    settle(hub, m);
    let who = crate::providers::display(&p, &mo);
    match r {
        Ok(x) if !content(&x).trim().is_empty() => format!("second opinion from {who}:\n{}", content(&x).trim()),
        Ok(_) => format!("error: {who} gave no answer"),
        Err(e) => format!("error: {who}: {e}"),
    }
}

/// The `fetch` tool: the fetch model picks lookups for `goal`; their raw outputs, not its words,
/// go back to the main model. It sees only the lookup tools, and any other call is refused unrun.
fn fetch(hub: &Arc<Hub>, m: &Arc<Member>, goal: &str) -> String {
    const ROUNDS: usize = 4;
    let (p, mo) = crate::route::model_id(&hub.cfg.fetch_model);
    let who = crate::providers::display(&p, &mo);
    let schemas = crate::route::fetch_schemas(&lock(&m.set).tools);
    let mut cwd = m.cwd();
    let mut msgs = crate::route::fetch_msgs(goal, &cwd);
    let mut raw = vec![];
    for _ in 0..ROUNDS {
        if m.cancel.load(Relaxed) { break }
        let msg = match call_as(hub, m, Some((&p, &mo)), &msgs, &schemas, true) {
            Ok(x) => x,
            Err(e) => { raw.push(format!("(fetch model {who} failed: {e})")); break }
        };
        msgs.push(msg.clone());
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        if calls.is_empty() { break }
        for c in calls {
            let name = c["function"]["name"].as_str().unwrap_or("").to_string();
            let args = parse_args(&c["function"]["arguments"]);
            let out = if !crate::route::fetch_allowed(&name) || !args.is_object() {
                format!("error: {name} isn't allowed: lookups only ({})", crate::route::FETCH_TOOLS.join(", "))
            } else if let Some(why) = crate::sandbox::guard(&name, &args, &cwd, &m.root, &hub.cfg) {
                format!("error: not here: it {why}")
            } else {
                tools::call(&name, &args, &hub.cfg, &mut cwd, &hub.limits(m), &crate::sandbox::Ctx { root: m.root.clone(), proxy: None, tab: m.name.clone() })
            };
            // no notices here: the UI pins this result to the fetch line, and a note in between would unpin it
            if !out.starts_with("error") {
                raw.push(format!("### {name} {args}\n{out}"));
            }
            msgs.push(json!({"role": "tool", "tool_name": name, "content": out}));
        }
    }
    settle(hub, m);
    if raw.is_empty() {
        return format!("{who} fetched nothing for this; look it up yourself with read_file / grep.");
    }
    let steps: Vec<&str> = raw.iter().filter_map(|r| r.strip_prefix("### ")?.split(' ').next()).collect();
    format!("{} lookup{} by {who}: {} (raw outputs, its own words dropped)\n\n{}", steps.len(), if steps.len() == 1 { "" } else { "s" },
            steps.join(", "), raw.join("\n\n"))
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
    call_as(hub, m, None, msgs, schemas, quiet)
}

/// `call` on another model than the tab's (`over` = provider, model): a second opinion or a
/// routed fetch. No thinking, no fallback; its cost still counts to the tab.
fn call_as(hub: &Arc<Hub>, m: &Arc<Member>, over: Option<(&str, &str)>, msgs: &[Value], schemas: &[Value], quiet: bool) -> Result<Value, Error> {
    let local = match over {
        Some((p, mo)) => crate::providers::is_local(p, mo),
        None => { let s = lock(&m.set); crate::providers::is_local(&s.provider, &s.model) }
    };
    if !local {
        *lock(&m.status) = "working";
        hub.changed();
        return call_locked(hub, m, over, msgs, schemas, quiet);
    }
    let _hold = hub.model.acquire(|| {
        *lock(&m.status) = "waiting-model";
        hub.changed();
    });
    *lock(&hub.holder) = m.name.clone();
    *lock(&m.status) = "working";
    hub.changed();
    let r = call_locked(hub, m, over, msgs, schemas, quiet);
    lock(&hub.holder).clear();
    hub.changed();
    r
}

fn call_locked(hub: &Arc<Hub>, m: &Arc<Member>, over: Option<(&str, &str)>, msgs: &[Value], schemas: &[Value], quiet: bool) -> Result<Value, Error> {
    let (think, model, provider) = match over {
        Some((p, mo)) => (String::new(), mo.to_string(), p.to_string()),
        None => {
            let s = lock(&m.set);
            let think = if !s.think { String::new() } else if s.effort.is_empty() { "on".into() } else { s.effort.clone() };
            (think, s.model.clone(), s.provider.clone())
        }
    };
    let Some(prov) = crate::providers::resolve(&provider) else {
        return Err(Error::Other(format!("no provider {provider}; /provider lists them")));
    };
    let cfg = &hub.cfg;
    let ctx = if over.is_some() { cfg.ctx } else { hub.ctx_for(m, true).0 };
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
                    session::partial_add(&lock(&m.session), text);
                    hub.emit(&m.name, Event::Token(text.to_string()));
                }
            }
            true
        };
        // with thinking on, the wait before the first answer token is the model reasoning; some
        // providers (the ChatGPT plan) only send their reasoning summary after it's done
        if !quiet { m.thinking.store(!think.is_empty(), Relaxed) }
        let r = backend::chat(&prov, &model, msgs, schemas, &think, ctx, &cfg.keep_alive, &mut on);
        if !quiet { session::partial_clear(&lock(&m.session)) }
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
        Err(ref e) if outage(e).is_some() && over.is_none() && !cfg.fallback_model.is_empty() && !m.cancel.load(Relaxed) => {
            let what = outage(e).unwrap_or_default();
            let Some(next) = next_fallback(&cfg.fallback_model, &provider, &model) else {
                return Err(Error::Other(format!("{} {what}; no fallback_model left to try", crate::providers::display(&provider, &model))));
            };
            let (fp, fm) = crate::providers::parse_model(next);
            hub.emit(&m.name, Event::Notice(format!("{} {what}; {} answers instead", crate::providers::display(&provider, &model), crate::providers::display(&fp, &fm))));
            {
                let mut s = lock(&m.set);
                (s.provider, s.model) = (fp, fm);
            }
            return call_locked(hub, m, None, msgs, schemas, quiet);
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
    let (spent, delta) = { let mut s = lock(&hub.spent); let b = s.cost; s.add(r.usage, price, paid); (s.cost, s.cost - b) };
    if delta > 0.0 && hub.cfg.budget_day > 0.0 { crate::budget::add_day(delta); }
    if let Some(w) = hub.cost_warn.check(spent, cfg.cost_warn) {
        hub.emit(&m.name, Event::Notice(w));
    }
    Ok(r.msg)
}

/// Before each model call: past `budget_session`/`budget_day`, ask like a tool would. Ok(true) =
/// the user said yes for this turn, Ok(false) = under budget, Err = they said no.
fn budget_gate(hub: &Hub, m: &Member) -> Result<bool, Error> {
    let (cfg, (rs, rd)) = (&hub.cfg, *lock(&hub.budget_raise));
    if cfg.budget_session <= 0.0 && cfg.budget_day <= 0.0 { return Ok(false) }
    let session = lock(&hub.spent).cost;
    let day = if cfg.budget_day > 0.0 { crate::budget::day_spent() } else { 0.0 };
    let lim = |l: f64, r: f64| if l > 0.0 { l + r } else { 0.0 };
    let Some((spent, limit, when)) = crate::budget::over(session, lim(cfg.budget_session, rs), day, lim(cfg.budget_day, rd)) else { return Ok(false) };
    let text = format!("Spent {} of your {} budget for {when}. Continue?", crate::usage::dollars(spent), crate::usage::dollars(limit));
    let args = json!({"text": text, "spent": spent, "budget": limit, "period": if when == "today" { "day" } else { "session" }});
    match hub.ask(m, "budget", &args, Some(&text)) {
        crate::hub::Answer::Yes => Ok(true),
        crate::hub::Answer::Always(_) => {
            // another full budget on top of what's spent, for as long as this qh runs
            let mut r = lock(&hub.budget_raise);
            if when == "today" { r.1 += spent - limit + cfg.budget_day } else { r.0 += spent - limit + cfg.budget_session }
            Ok(true)
        }
        crate::hub::Answer::No => Err(Error::Other(format!("paused: {text} (answered no)"))),
    }
}

/// Why a call failed in a way another model may not: rate limit, outage (5xx), unreachable.
fn outage(e: &Error) -> Option<String> {
    match e {
        Error::Limited(..) => Some("is rate-limited".into()),
        Error::Connect(_) => Some("is unreachable".into()),
        Error::Other(m) => {
            let l = m.to_lowercase();
            if let Some(c) = l.strip_prefix("http ").and_then(|r| r.get(..3)).filter(|c| c.starts_with('5')) {
                return Some(format!("is down (HTTP {c})"));
            }
            ["timed out", "connection reset", "connection refused", "unexpected eof", "overloaded"].iter()
                .any(|k| l.contains(k)).then(|| "is unreachable".into())
        }
        Error::Cancelled => None,
    }
}

/// The fallback after the model now in use, in config order; the first when it isn't one.
fn next_fallback<'a>(list: &'a [String], provider: &str, model: &str) -> Option<&'a str> {
    let same = |f: &String| { let (p, m) = crate::providers::parse_model(f); p == provider && m == model };
    let from = list.iter().position(same).map_or(0, |i| i + 1);
    list[from..].iter().find(|f| !same(f)).map(String::as_str)
}

fn read_only(hub: &Hub, name: &str, a: &Value) -> bool {
    a.is_object() && !HUB_TOOLS.contains(&name) && !matches!(name, "remember" | "bash")
        && tools::builtin_schema(name).is_some() && !tools::risky(name, a, &hub.cfg)
}

/// The model's own checklist, per session. Resent in full each call so a small model sees
/// the whole plan in its latest tool result instead of having to remember it.
type PlanList = Vec<(String, &'static str)>;
static PLANS: Mutex<Vec<(String, PlanList)>> = Mutex::new(Vec::new());

/// The session's current checklist as (text, pending|active|done), for apps showing a live plan.
pub fn plan_items(session: &str) -> PlanList {
    lock(&PLANS).iter().find(|(s, _)| s == session).map(|p| p.1.clone()).unwrap_or_default()
}

#[cfg(test)]
pub fn todo_for_test(session: &str, a: &Value) -> String { todo(session, a) }

fn todo(session: &str, a: &Value) -> String {
    static LISTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    let Some(items) = a["items"].as_array() else {
        return "error: items must be a list of {text, status}".into();
    };
    let plan: PlanList = items.iter().map(|i| {
        let st = match i["status"].as_str().unwrap_or("pending") {
            "done" | "completed" => "done",
            "doing" | "in_progress" | "active" => "active",
            _ => "pending",
        };
        (i["text"].as_str().or_else(|| i.as_str()).unwrap_or("").to_string(), st)
    }).collect();
    {
        let mut p = lock(&PLANS);
        p.retain(|(s, _)| s != session);
        p.push((session.into(), plan));
    }
    let rows: Vec<String> = items.iter().map(|i| {
        let mark = match i["status"].as_str().unwrap_or("pending") {
            "done" | "completed" => "[x]",
            "doing" | "in_progress" | "active" => "[~]",
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

/// remember / forget / recall. Scope "team" proposes a note to the team: a person approves it.
fn memory_tool(hub: &Arc<Hub>, m: &Arc<Member>, name: &str, args: &Value) -> String {
    if !hub.cfg.memory {
        return "error: memory is off (memory = false in config)".into();
    }
    if name == "remember" && args["scope"].as_str().is_some_and(|s| s.trim().eq_ignore_ascii_case("team")) {
        let cwd = m.cwd();
        if !crate::team::running_here(&cwd) {
            return "error: this project isn't connected to a team; use scope project or user".into();
        }
        return propose_team_note(hub, m, args["fact"].as_str().unwrap_or(""), |t| crate::team::add_team_note(&cwd, t));
    }
    match crate::memory::call(&m.root, name, args) {
        Ok(d) => {
            if d.action != "read" {
                hub.emit(&m.name, Event::Remembered { action: d.action.into(), scope: d.scope, name: d.name, path: d.path });
            }
            d.text
        }
        Err(e) => format!("error: {e}"),
    }
}

/// Ask the user before a note goes to the whole team; `add` publishes it on the board.
/// A reviewer asked for changes or handed it to their person: the push doesn't run unless the
/// user says so. Some(result for the agent) when it's held.
fn hold_push(hub: &Arc<Hub>, m: &Arc<Member>, r: &crate::review::Outcome) -> Option<String> {
    let me = crate::team::me().unwrap_or_default();
    let login = |to: &str| to.split('/').next().unwrap_or(to).to_string();
    let hand = |list: &[(String, String)]| -> Vec<String> {
        list.iter().map(|(to, a)| match crate::team::dm_send(&login(to), &crate::review::handoff_text(&me, &r.files, a)) {
            Ok(_) => format!("@{}", login(to)),
            Err(e) => format!("@{} (couldn't message: {e})", login(to)),
        }).collect()
    };
    if !r.handoff.is_empty() {
        let who = hand(&r.handoff).join(", ");
        hub.emit(&m.name, Event::Notice(format!("push held: handed to {who} in team chat")));
        return Some(format!("{}\n\nPush NOT run: the reviewer handed this to their person, and {who} got a private message in team chat. \
Tell the user to talk it through with them there; push again only once the user says so.", r.text));
    }
    if r.changes.is_empty() || crate::review::pushed_anyway(&r.head) { return None }
    let owners: Vec<String> = r.changes.iter().map(|(t, _)| login(t)).collect();
    let comments: String = r.changes.iter().map(|(t, a)| format!("{t}: {}", a.trim())).collect::<Vec<_>>().join("\n\n");
    let args = json!({"owners": owners.join(", "), "text": comments.chars().take(2000).collect::<String>()});
    match hub.ask(m, "review_hold", &args, Some("a reviewer asked for changes before this push")) {
        crate::hub::Answer::Yes => { crate::review::push_anyway(&r.head); None }
        crate::hub::Answer::Always(_) => {
            let who = hand(&r.changes).join(", ");
            hub.emit(&m.name, Event::Notice(format!("push held: handed to {who} in team chat")));
            Some(format!("{}\n\nPush NOT run: the user handed this to {who} in team chat. Wait for the user before pushing again.", r.text))
        }
        crate::hub::Answer::No => Some(format!("{}\n\nPush NOT run: a reviewer asked for changes and the user held the push. \
Make the changes they asked for (or tell the user why not), commit, and push again.", r.text)),
    }
}

fn propose_team_note(hub: &Arc<Hub>, m: &Arc<Member>, text: &str, add: impl FnOnce(&str) -> Result<PathBuf, String>) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "error: the note is empty".into();
    }
    let ev = |action: &str, path: PathBuf| Event::Remembered { action: action.into(), scope: "team".into(), name: text.chars().take(80).collect(), path };
    hub.emit(&m.name, ev("proposed", PathBuf::new()));
    if !hub.confirm(m, "team_note", &json!({"text": text}), Some("adds a note every teammate's agent reads at the start of a session")) {
        hub.emit(&m.name, ev("declined", PathBuf::new()));
        return "declined by the user: the note was not added to the team notes. Keep it as a project memory if it still matters.".into();
    }
    match add(&text) {
        Ok(p) => {
            hub.emit(&m.name, ev("saved", p));
            "approved: added to the team notes; teammates' agents read it from their next session".into()
        }
        Err(e) => format!("error: {e}"),
    }
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
    if name == "ask_model" {
        // read-only: a question to another model, never asked about. It can't see files: without the
        // code in `context` it can only guess (seen live: "couldn't tell without inspecting calc.py")
        if args["context"].as_str().is_none_or(|c| c.trim().is_empty()) {
            return "error: context is empty. The other model has no tools and sees nothing but what you send: read the \
relevant files first, then call ask_model again with the code, error or plan pasted into context.".into();
        }
        return ask_other(hub, m, args["model"].as_str().unwrap_or(""), args["question"].as_str().unwrap_or(""),
                         args["context"].as_str().unwrap_or(""));
    }
    if name == "fetch" && !hub.cfg.fetch_model.is_empty() {
        return fetch(hub, m, args["goal"].as_str().unwrap_or(""));
    }
    if hub.mode_name() == "plan" && (tools::risky(name, &args, &hub.cfg) || matches!(name, "remember" | "forget" | "send_message" | "spawn")) {
        return "error: plan mode is read-only. Finish exploring, then reply with a numbered plan; the user switches mode (Shift+Tab) to let you carry it out.".into();
    }
    let input = json!({"event": "pre_tool", "tool": name, "args": args, "session": m.name, "cwd": m.cwd()});
    if let hooks::Outcome::Block(why) = hooks::fire(&hub.cfg.hooks, "pre_tool", name, &input, &m.cwd()) {
        return format!("error: blocked by a hook: {why}");
    }
    if matches!(name, "remember" | "forget" | "recall") {
        return memory_tool(hub, m, name, &args);
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
    // a push in a team project: the owners of the changed code get to review it first (bounded wait)
    let review = if name == "bash" && crate::review::is_push(args["cmd"].as_str().unwrap_or("")) && crate::team::running_here(&cwd) {
        crate::review::before_push(&m.name, &cwd, &hub.cfg, &|t| hub.emit(&m.name, Event::Notice(t)), &|| m.cancel.load(Relaxed))
    } else { None };
    if let Some(held) = review.as_ref().and_then(|r| hold_push(hub, m, r)) {
        return held;
    }
    let review = review.map(|r| r.text);
    let (turn, prompt) = { // which user message this turn answers, so the app can rewind to before it
        let msgs = lock(&m.msgs);
        let users: Vec<&Value> = msgs.iter().filter(|x| x["role"] == "user").collect();
        (users.len().saturating_sub(1), users.last().and_then(|x| x["content"].as_str()).unwrap_or("").to_string())
    };
    lock(&m.turn).ckpt.snapshot(name, &args, &cwd, turn, &prompt);
    let sb = if name == "bash" { hub.sandbox_ctx(m) } else { crate::sandbox::Ctx { root: m.root.clone(), proxy: None, tab: m.name.clone() } };
    // `/allow localhost:3000`: addresses the user let this session's browser open
    let browse: Vec<String> = if crate::browser::TOOLS.contains(&name) {
        lock(&m.set).allow.iter().filter_map(|r| r.strip_prefix("browse:")).map(String::from).collect()
    } else { vec![] };
    let with_browse;
    let cfg = if browse.is_empty() { &hub.cfg } else {
        with_browse = { let mut c = hub.cfg.clone(); c.browser_allow_hosts.extend(browse); c };
        &with_browse
    };
    let t0 = std::time::Instant::now();
    let before: Vec<(PathBuf, String)> = if matches!(name, "write_file" | "edit_file") {
        tools::preview(&cwd, name, &args).into_iter().map(|(p, _)| { let b = std::fs::read_to_string(&p).unwrap_or_default(); (p, b) }).collect()
    } else { vec![] };
    let ran_in = cwd.clone();
    let mut out = if name == "past_sessions" {
        // this session isn't a past one: leave it out of the list and the search
        let me = lock(&m.session).clone();
        crate::session::past(&crate::session::dir(), &m.root, &args, &me)
    } else {
        tools::call(name, &args, cfg, &mut cwd, &hub.limits(m), &sb)
    };
    log_action(m, name, &args, &out, &ran_in, &before, t0.elapsed().as_secs_f64(), crate::sandbox::enabled(&hub.cfg));
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
    if let Some(r) = review {
        out = format!("{r}\n\n{out}");
    }
    if name == "bash" {
        *lock(&m.cwd) = cwd; // a cd in the command moves this session only
    }
    out
}

/// One timeline line for a command run or a file changed.
#[allow(clippy::too_many_arguments)]
fn log_action(m: &Member, name: &str, a: &Value, out: &str, cwd: &std::path::Path, before: &[(PathBuf, String)], secs: f64, sandboxed: bool) {
    let session = lock(&m.session).clone();
    match name {
        "bash" => {
            let cmd = a["cmd"].as_str().unwrap_or("");
            let exit = crate::activity::exit_code(out);
            let sandboxed = sandboxed && a["unsandboxed"].as_bool() != Some(true);
            crate::activity::record(&session, "cmd", cmd, exit == Some(0) || (exit.is_none() && !out.starts_with("error")),
                json!({"cmd": cmd, "cwd": cwd, "sandboxed": sandboxed, "exit": exit, "secs": (secs * 10.0).round() / 10.0,
                       "background": a["background"].as_bool() == Some(true)}));
        }
        "write_file" | "edit_file" => {
            let ok = !out.starts_with("error");
            for (p, b) in before {
                let after = std::fs::read_to_string(p).unwrap_or_default();
                let (add, del) = crate::activity::plus_minus(b, &after);
                crate::activity::record(&session, "edit", &format!("{} +{add} -{del}", p.display()), ok,
                    json!({"path": p, "added": add, "removed": del, "tool": name}));
            }
        }
        _ => {}
    }
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
    }

    #[test]
    fn verify_detection() {
        let d = std::env::temp_dir().join(format!("qh-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(verify_cmd("", &d), None);
        assert_eq!(verify_cmd("make lint", &d).as_deref(), Some("make lint"));
        std::fs::write(d.join("go.mod"), "module x").unwrap();
        assert_eq!(verify_cmd("", &d).as_deref(), Some("go build ./..."));
        std::fs::write(d.join("pyproject.toml"), "").unwrap();
        assert_eq!(verify_cmd("", &d).as_deref(), Some("python -m pytest -q -x"));
        std::fs::write(d.join("package.json"), r#"{"scripts":{"build":"tsc","test":"jest"}}"#).unwrap();
        assert_eq!(verify_cmd("", &d).as_deref(), Some("npm run -s build"));
        assert_eq!(verify_cmd("test", &d).as_deref(), Some("npm run -s test"));
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        assert_eq!(verify_cmd("", &d).as_deref(), Some("cargo check -q"));
        assert_eq!(verify_cmd("test", &d).as_deref(), Some("cargo check -q && cargo test -q"));
        assert_eq!(verify_cmd("off", &d), None);
        let _ = std::fs::remove_dir_all(&d);
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

#[cfg(test)]
mod fallback_tests {
    use super::*;

    #[test]
    fn outages_and_limits_fall_back_other_errors_do_not() {
        assert_eq!(outage(&Error::Other("HTTP 503: busy".into())).as_deref(), Some("is down (HTTP 503)"));
        assert_eq!(outage(&Error::Other("HTTP 500: oops".into())).as_deref(), Some("is down (HTTP 500)"));
        assert!(outage(&Error::Connect("refused".into())).is_some());
        assert!(outage(&Error::Other("read: timed out".into())).is_some());
        assert!(outage(&Error::Limited("slow down".into(), None)).is_some());
        // a plan limit hours long: not retried, but it does fall back
        let plan = Error::Limited("you've hit your ChatGPT plan's limit".into(), Some(7200));
        assert!(!crate::backend::transient(&plan));
        assert!(outage(&plan).is_some());
        assert!(outage(&Error::Other("HTTP 400: bad request".into())).is_none());
        assert!(outage(&Error::Cancelled).is_none());
    }

    #[test]
    fn fallbacks_are_walked_in_order_once() {
        let l = ["b".to_string(), "c".to_string()];
        assert_eq!(next_fallback(&l, "", "a"), Some("b"));
        assert_eq!(next_fallback(&l, "", "b"), Some("c"));
        assert_eq!(next_fallback(&l, "", "c"), None);
        assert_eq!(next_fallback(&["a".to_string(), "b".to_string()], "", "a"), Some("b"));
        assert_eq!(next_fallback(&[], "", "a"), None);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    struct Env(PathBuf);
    impl Drop for Env {
        fn drop(&mut self) {
            std::env::remove_var("QH_CONFIG");
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn setup(tag: &str) -> Env {
        let root = crate::sandbox::canon(&std::env::temp_dir()).join(format!("qh-memory-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proj")).unwrap();
        std::env::set_var("QH_STATE_DIR", root.join("state"));
        std::env::set_var("QH_CONFIG", root.join("cfg/config.toml"));
        Env(root)
    }

    fn sys(m: &Member) -> String {
        lock(&m.msgs)[0]["content"].as_str().unwrap_or("").to_string()
    }

    #[test]
    fn remember_saves_tells_the_ui_and_reaches_the_next_session() {
        let _env = lock(&crate::hub::tests::ENV);
        let e = setup("on");
        let events = Arc::new(Mutex::new(vec![]));
        let seen = events.clone();
        let emit: crate::hub::Emit = Box::new(move |_, ev| if let Event::Remembered { action, scope, name, .. } = ev {
            lock(&seen).push(format!("{action} {scope} {name}"));
        });
        let hub = Hub::new(crate::config::load_defaults(), Some(format!("mem-{}", std::process::id())), emit, true);
        let m = hub.add("a", None, "", "", None, false, Some(e.0.join("proj"))).unwrap();
        let out = dispatch(&hub, &m, "remember", &json!({"fact": "deploys need the VPN (2026-10-20): the API is internal-only",
            "name": "Deploy needs VPN", "description": "deploys need the VPN", "type": "gotcha"}));
        assert!(out.starts_with("saved \"Deploy needs VPN\" (project memory"), "{out}");
        dispatch(&hub, &m, "remember", &json!({"fact": "prefers short answers", "scope": "user", "name": "Style"}));
        assert_eq!(*lock(&events), vec!["saved project Deploy needs VPN", "saved user Style"]);
        assert!(crate::memory::project_dir(&m.root).join("deploy-needs-vpn.md").exists());
        assert!(e.0.join("cfg/memory/MEMORY.md").exists(), "user memory sits in the config folder");
        assert!(!m.root.join("NOTES.md").exists(), "nothing is written into the project");
        // the next session starts with the index, once
        let b = hub.add("b", None, "", "", None, false, Some(e.0.join("proj"))).unwrap();
        with_memory(&hub, &b);
        with_memory(&hub, &b);
        let s = sys(&b);
        assert_eq!(s.matches(crate::memory::MARK).count(), 1);
        assert!(s.contains("[Deploy needs VPN](deploy-needs-vpn.md) — deploys need the VPN") && s.contains("[Style](style.md)"), "{s}");
        let r = dispatch(&hub, &b, "recall", &json!({"name": "deploy needs vpn"}));
        assert!(r.contains("the API is internal-only"), "{r}");
        dispatch(&hub, &b, "forget", &json!({"name": "Style", "scope": "user"}));
        assert_eq!(lock(&events).last().unwrap(), "forgot user Style");
        assert!(dispatch(&hub, &b, "remember", &json!({"fact": "x", "scope": "team"})).starts_with("error: this project isn't connected"));
    }

    #[test]
    fn memory_off_means_no_tools_and_no_prompt_block() {
        let _env = lock(&crate::hub::tests::ENV);
        let e = setup("off");
        crate::memory::save(&crate::memory::global_dir(), "Who", "", "", "Kesav", &[]).unwrap();
        let mut cfg = crate::config::load_defaults();
        cfg.memory = false;
        let hub = Hub::new(cfg, Some(format!("memoff-{}", std::process::id())), Box::new(|_, _| {}), true);
        let m = hub.add("a", None, "", "", None, false, Some(e.0.join("proj"))).unwrap();
        let names: Vec<String> = lock(&m.set).tools.iter().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect();
        assert!(!names.iter().any(|n| matches!(n.as_str(), "remember" | "forget" | "recall")), "{names:?}");
        assert!(dispatch(&hub, &m, "remember", &json!({"fact": "x"})).starts_with("error: unknown tool"));
        with_memory(&hub, &m);
        assert!(!sys(&m).contains(crate::memory::MARK));
        assert!(memory_tool(&hub, &m, "remember", &json!({"fact": "x"})).starts_with("error: memory is off"));
    }

    #[test]
    fn team_notes_are_added_only_when_a_person_approves() {
        let _env = lock(&crate::hub::tests::ENV);
        let e = setup("team");
        let yes = Arc::new(AtomicBool::new(false));
        let (events, seen, y) = (Arc::new(Mutex::new(vec![])), Arc::new(Mutex::new(vec![])), yes.clone());
        let (ev2, seen2) = (events.clone(), seen.clone());
        let emit: crate::hub::Emit = Box::new(move |_, ev| match ev {
            Event::Confirm { tool, reply, guard, .. } => {
                lock(&seen2).push((tool, guard));
                let _ = reply.send(if y.load(Relaxed) { crate::hub::Answer::Yes } else { crate::hub::Answer::No });
            }
            Event::Remembered { action, scope, .. } => lock(&ev2).push(format!("{action} {scope}")),
            _ => {}
        });
        // -y and auto mode don't approve it: a person does
        let hub = Hub::new(crate::config::load_defaults(), Some(format!("memteam-{}", std::process::id())), emit, true);
        hub.set_mode("auto");
        let m = hub.add("a", None, "", "", None, false, Some(e.0.join("proj"))).unwrap();
        let file = e.0.join("team-notes.md");
        let add = |t: &str| { std::fs::write(&file, format!("- {t}\n")).unwrap(); Ok(file.clone()) };
        let out = propose_team_note(&hub, &m, "staging deploys  go through CI", add);
        assert!(out.starts_with("declined") && !file.exists(), "{out}");
        yes.store(true, Relaxed);
        let out = propose_team_note(&hub, &m, "staging deploys  go through CI", add);
        assert!(out.starts_with("approved"), "{out}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "- staging deploys go through CI\n");
        assert_eq!(*lock(&seen), vec![("team_note".to_string(), true), ("team_note".to_string(), true)]);
        assert_eq!(*lock(&events), vec!["proposed team", "declined team", "proposed team", "saved team"]);
        assert!(propose_team_note(&hub, &m, "  ", add).starts_with("error"));
    }
}
