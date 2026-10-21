mod activity;
mod backend;
mod budget;
mod bg;
mod browser;
mod chatgpt;
mod checkpoints;
mod config;
mod context;
mod engine;
mod lsp;
mod map;
mod oauth;
mod platform;
mod mcp;
mod memory;
mod notify;
mod hooks;
mod hub;
mod inline;
mod ide;
mod jsonl;
mod image;
mod link;
mod prompt;
mod providers;
mod proxy;
mod relay;
mod review;
mod route;
mod sandbox;
#[cfg(target_os = "linux")]
mod sandbox_linux;
mod sandbox_macos;
mod sandbox_windows;
mod serve;
mod session;
mod setup;
mod skills;
mod team;
mod teamcrypto;
mod teamsetup;
mod teamview;
mod textcall;
mod tier;
mod tools;
mod ui;
mod update;
mod usage;
mod worktree;

use hub::{lock, Event, Hub};
use std::io::Write;
use std::path::PathBuf;

const USAGE: &str = "qh [--resume [id|latest]] [--profile coding|general|all] [--model NAME|provider:MODEL] [--think] [--mode ask|auto-edit|auto|plan]
qh setup                connect a model (local Ollama or a cloud API key)
qh team create [user…]  share this project on GitHub and invite friends · qh join owner/repo [code]
qh docs                 open the guide in your browser
qh update               install the newest qh (qh --version shows this one)
qh login chatgpt        use your ChatGPT plan, Go included (qh logout chatgpt)
qh login <provider>     save an API key (openrouter, ollama-cloud, openai, groq, anthropic…)
qh -p \"prompt\" [-p \"next turn\" ...] [-y] [--json] [--max-turns N] [--ctx N] [--stats FILE]
    run prompts as turns of one session without the UI and print the replies
    -y approves every tool; --ctx overrides the context window (compacts at 88%);
    --stats writes {tokens, calls, compacts} as JSON. QH_STATE_DIR isolates history.
    --json prints JSONL events; --max-turns stops after N tool rounds per prompt.
    exit 0 ok, 1 agent error, 2 bad usage, 3 a permission was needed and refused";

struct Args {
    resume: Option<String>,
    profile: Option<String>,
    model: Option<String>,
    think: bool,
    print: Vec<String>,
    yes: bool,
    ctx: Option<usize>,
    stats: Option<String>,
    mode: Option<String>,
    json: bool,
    max_turns: Option<usize>,
    serve: bool,
}

fn parse() -> Args {
    let mut a = Args { resume: None, profile: None, model: None, think: false, print: vec![], yes: false, ctx: None, stats: None, mode: None, json: false, max_turns: None, serve: false };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--resume" | "-r" => a.resume = Some(it.next_if(|v| !v.starts_with('-')).unwrap_or_else(|| "latest".into())),
            "--profile" => a.profile = it.next(),
            "--model" | "-m" => {
                a.model = it.next();
                if let Some(e) = a.model.as_deref().and_then(crate::providers::bare_provider) {
                    eprintln!("qh: {e}");
                    std::process::exit(2);
                }
            }
            "--think" => a.think = true,
            "-p" | "--print" => a.print.extend(it.next()),
            "--ctx" => a.ctx = it.next().and_then(|v| v.parse().ok()),
            "--stats" => a.stats = it.next(),
            "-y" | "--yes" => a.yes = true,
            "--mode" => a.mode = it.next(),
            "--json" => a.json = true,
            "serve" => a.serve = true,
            "--max-turns" => match it.next().and_then(|v| v.parse().ok()).filter(|n| *n > 0) {
                Some(n) => a.max_turns = Some(n),
                None => {
                    eprintln!("--max-turns needs a number above 0");
                    std::process::exit(2);
                }
            },
            _ => {
                println!("{USAGE}");
                std::process::exit(if x == "-h" || x == "--help" { 0 } else { 2 });
            }
        }
    }
    a
}

/// `qh login <provider>`: read a key without echoing it, save it (chmod 600), test it.
fn login(name: &str) -> i32 {
    if name == "chatgpt" || name == "chatgpt-official" {
        let say = |s: &str| println!("{s}\n");
        let r = if name == "chatgpt" { chatgpt::login(&say) } else { chatgpt::login_official(&say) };
        return match r {
            Ok(email) => { println!("✓ signed in with ChatGPT as {email}. Pick a model with /model chatgpt:"); 0 }
            Err(e) => { eprintln!("✗ {e}"); 1 }
        };
    }
    let Some(p) = providers::get(name) else {
        let names: Vec<String> = providers::all().into_iter().map(|p| p.name).collect();
        eprintln!("no provider {name}. Known: {}", names.join(", "));
        return 2;
    };
    if p.local {
        println!("{} runs locally and needs no key", p.name);
        return 0;
    }
    eprint!("API key for {} (input hidden): ", p.name);
    let _ = std::io::stderr().flush();
    let key = platform::read_hidden();
    eprintln!();
    if key.trim().is_empty() {
        eprintln!("no key entered");
        return 1;
    }
    if let Err(e) = providers::set_key(&p.name, &key) {
        eprintln!("could not save: {e}");
        return 1;
    }
    match providers::test(&p.name) {
        Ok(n) => { println!("✓ saved; {n} models available. Pick one with /model {}:", p.name); 0 }
        Err(e) => { println!("✗ saved, but the test call failed: {e}"); 1 }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // the command sandbox's helper: first, while this process still has one thread (namespaces need that)
    #[cfg(target_os = "linux")]
    if argv.get(1).map(String::as_str) == Some("__sandbox") {
        std::process::exit(sandbox_linux::helper_main(&argv[2..]));
    }
    // the Windows sandbox: launcher (as you), runner (as the sandbox user), and the elevated setup step
    #[cfg(windows)]
    {
        let rest: Vec<std::ffi::OsString> = std::env::args_os().skip(2).collect();
        match argv.get(1).map(String::as_str) {
            Some("__winsbx") => std::process::exit(sandbox_windows::launcher_main(&rest)),
            Some("__winsbx-in") => std::process::exit(sandbox_windows::runner_main(&rest)),
            Some("__winsbx-admin") => std::process::exit(sandbox_windows::admin_main(&rest)),
            _ => {}
        }
    }
    // the Mac app's sidebar: which of these folders are team projects (and their repo)
    if argv.get(1).map(String::as_str) == Some("__team") {
        let out: serde_json::Map<String, serde_json::Value> = argv[2..].iter().map(|p| {
            let repo = team::github_repo(std::path::Path::new(p)).filter(|(_, r)| team::enabled(r)).map(|(_, r)| r);
            (p.clone(), serde_json::json!(repo))
        }).collect();
        println!("{}", serde_json::Value::Object(out));
        std::process::exit(0);
    }
    // the Mac app's Settings window: `qh __config` prints the settings, `qh __config set k v` sets one
    if argv.get(1).map(String::as_str) == Some("__config") {
        match (argv.get(2).map(String::as_str), argv.get(3), argv.get(4)) {
            (None, ..) => println!("{}", config::dump()),
            (Some("set"), Some(k), Some(v)) => if let Err(e) = config::set_raw(k, v) { eprintln!("{e}"); std::process::exit(1) },
            _ => { eprintln!("usage: qh __config [set <key|table.key> <toml value, empty to remove>]"); std::process::exit(2) }
        }
        std::process::exit(0);
    }
    if argv.get(1).map(String::as_str) == Some("__tool") {
        std::process::exit(tool_once(&argv[2..]));
    }
    match (argv.get(1).map(String::as_str), argv.get(2).map(String::as_str)) {
        (Some("team"), Some("create")) => std::process::exit(teamsetup::create(&argv[3..])),
        (Some("join"), Some(repo)) => std::process::exit(teamsetup::join(repo, argv.get(3).map(String::as_str))),
        (Some("link"), _) => std::process::exit(link::cli(&argv[2..])),
        (Some("relay"), _) => std::process::exit(relay::cli(&argv[2..])),
        (Some("--version" | "-V"), _) => {
            println!("qh {}", update::VERSION);
            std::process::exit(0);
        }
        (Some("update"), _) => std::process::exit(update::cli()),
        (Some("sandbox"), Some(sub @ ("setup" | "remove" | "reset"))) => {
            // Windows: setup makes the sandbox users (one admin prompt), remove undoes all of it, reset
            // takes back the folder permissions only (they return on the next command)
            #[cfg(windows)]
            let r = match sub {
                "setup" => sandbox_windows::setup(),
                "remove" => sandbox_windows::remove(),
                _ => sandbox_windows::reset().map(|n| format!("removed the sandbox's permissions from {n} folders")).map_err(|e| e.to_string()),
            };
            #[cfg(not(windows))]
            let r: Result<String, String> = Ok(format!("nothing to {sub}: only Windows needs a sandbox set up"));
            match r {
                Ok(s) => { println!("{s}"); std::process::exit(0) }
                Err(e) => { eprintln!("qh sandbox {sub}: {e}"); std::process::exit(1) }
            }
        }
        (Some("docs"), _) => {
            platform::open_url(platform::DOCS);
            println!("{}", platform::DOCS);
            std::process::exit(0);
        }
        _ => {}
    }
    if std::env::args().nth(1).as_deref() == Some("logout") && std::env::args().nth(2).as_deref() == Some("chatgpt") {
        chatgpt::sign_out();
        println!("signed out of ChatGPT");
        std::process::exit(0);
    }
    if std::env::args().nth(1).as_deref() == Some("login") {
        providers::init(&config::load());
        let name = std::env::args().nth(2).unwrap_or_default();
        std::process::exit(if name.is_empty() { eprintln!("usage: qh login <provider>"); 2 } else { login(&name) });
    }
    if std::env::args().nth(1).as_deref() == Some("setup") {
        providers::init(&config::load());
        std::process::exit(if setup::run() { 0 } else { 1 });
    }
    let args = parse();
    if (args.json || args.max_turns.is_some()) && args.print.is_empty() {
        eprintln!("--json and --max-turns work with -p \"prompt\"");
        std::process::exit(jsonl::BAD_USAGE);
    }
    if args.print.is_empty() && std::io::IsTerminal::is_terminal(&std::io::stdin())
        && (setup::needed() || setup::no_model(&config::load())) {
        providers::init(&config::load());
        if !setup::needed() {
            println!("No model is set up on this computer yet.");
        }
        if !setup::run() {
            std::process::exit(1);
        }
    }
    let mut cfg = config::load();
    providers::init(&cfg);
    // no ctx in config: size the window to what the GPU can hold
    if !cfg.ctx_set {
        if let Some(d) = tier::detect(&cfg.host, &cfg.model) {
            (cfg.ctx, cfg.compact_at) = (d.ctx, d.ctx * 88 / 100);
        }
    }
    if let Some(c) = args.ctx {
        (cfg.ctx, cfg.compact_at, cfg.ctx_set) = (c, c * 88 / 100, true);
    }
    mcp::init(&cfg.mcp);
    let update_check = cfg.update_check;
    let (tx, rx) = ui::channel_pair();
    let resume_hub = args.resume.as_deref() == Some("latest") && args.print.is_empty();
    let hub_path = if resume_hub { Hub::latest() } else { None };
    let hub = Hub::new(cfg, hub_path.as_ref().and_then(|p| p.file_stem()).map(|s| s.to_string_lossy().into()),
                       ui::emitter(tx.clone()), args.yes);
    if let Some(p) = &hub_path {
        hub.restore(p);
    }
    if let Some(mo) = &args.mode {
        if !hub.set_mode(mo) {
            eprintln!("unknown mode {mo}: ask, auto-edit, auto or plan");
            std::process::exit(2);
        }
    }
    if hub.names().is_empty() {
        // a single-session resume from before hubs existed
        let session = match args.resume.as_deref() {
            Some("latest") => session::latest(),
            Some(id) => Some(session::dir().join(format!("{id}.jsonl"))).filter(|p: &PathBuf| p.exists()),
            None => None,
        };
        if let Err(e) = hub.add("main", args.profile.as_deref(), "", "", session, args.think, None) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    if let Some(model) = &args.model {
        for n in hub.names() {
            if let Some(m) = hub.get(&n) {
                let mut s = lock(&m.set);
                (s.provider, s.model) = providers::parse_model(model);
            }
        }
    }
    // killed (SIGTERM, a closed window): the reply being streamed is saved too, not just what came before
    platform::on_kill(if args.print.is_empty() && !args.serve { killed_tui } else { killed });
    if !args.print.is_empty() {
        let opts = Headless { stats: args.stats.as_deref(), json: args.json, max_turns: args.max_turns };
        let code = headless(&hub, &args.print, rx, &opts);
        bg::kill_all(); // its background tasks end with it, as in the TUI (seen live: dev servers outlived qh -p)
        browser::shutdown();
        lsp::shutdown();
        std::process::exit(code);
    }
    if args.serve {
        let code = serve::run(hub, rx);
        bg::kill_all();
        browser::shutdown();
        lsp::shutdown();
        std::process::exit(code);
    }
    update::check_in_background(update_check);
    let r = ui::run(hub, tx, rx);
    bg::kill_all();
    browser::shutdown();
    lsp::shutdown();
    if let Err(e) = r {
        eprintln!("{e}");
    }
}

struct Headless<'a> {
    stats: Option<&'a str>,
    json: bool,
    max_turns: Option<usize>,
}

fn killed() {
    session::save_partials();
    bg::kill_all();
}

fn killed_tui() {
    killed();
    ratatui::restore();
}

/// Run each prompt as a turn of "main", printing replies to stdout and tool
/// calls to stderr (or, with --json, every event as a JSON line on stdout).
fn headless(hub: &std::sync::Arc<Hub>, prompts: &[String], rx: std::sync::mpsc::Receiver<ui::Ui>, o: &Headless) -> i32 {
    let main = hub.names()[0].clone();
    let Some(m) = hub.get(&main) else { return jsonl::AGENT_ERROR };
    let mut out = std::io::stdout();
    let (mut code, mut calls, mut compacts, mut peak) = (jsonl::OK, 0, 0, 0);
    let mut denied = false;
    let mut last_call = None;
    let mut answer = String::new();
    if o.json {
        let set = lock(&m.set);
        let model = if set.provider.is_empty() { set.model.clone() } else { format!("{}:{}", set.provider, set.model) };
        jsonl::write(&mut out, &serde_json::json!({"type": "start", "model": model, "mode": hub.mode_name(),
            "version": update::VERSION, "prompts": prompts.len(), "max_turns": o.max_turns}));
    }
    for p in prompts {
        hub.send(&main, p, "user", 0, None);
        let (mut rounds, mut in_calls) = (0usize, false);
        answer.clear();
        while let Ok(ui::Ui::Hub(name, ev)) = rx.recv() {
            let is_call = matches!(ev, Event::ToolCall { .. });
            if is_call && !in_calls && name == main {
                rounds += 1;
                if o.max_turns.is_some_and(|n| rounds > n) && !m.cancel.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    let msg = format!("stopped: more than {} tool rounds (--max-turns)", rounds - 1);
                    if o.json { jsonl::write(&mut out, &serde_json::json!({"type": "error", "message": msg})) } else { eprintln!("[{name}] {msg}") }
                    code = jsonl::AGENT_ERROR;
                }
            }
            in_calls = is_call;
            if o.json && name == main {
                for v in jsonl::events(&ev, &mut last_call) {
                    jsonl::write(&mut out, &v);
                }
            }
            match ev {
                Event::Token(t) if name == main => {
                    answer.push_str(&t);
                    if !o.json {
                        let _ = out.write_all(t.as_bytes());
                        let _ = out.flush();
                    }
                }
                Event::ToolCall { name: t, args } => {
                    calls += 1;
                    if !o.json { eprintln!("[{name}] tool {t} {args}") }
                }
                Event::ToolResult { .. } => answer.clear(), // the answer is the text after the last tool
                Event::Compact { .. } => compacts += 1,
                Event::Done { tokens, .. } => peak = peak.max(tokens),
                Event::Notice(n) => {
                    if !o.json { eprintln!("[{name}] {n}") }
                    if n.starts_with("error") {
                        code = jsonl::AGENT_ERROR;
                    }
                }
                Event::Confirm { tool, args, reply, warn, .. } => {
                    // nobody to ask: refuse, and say so
                    denied = true;
                    let why = format!("{tool} needs approval in {} mode and nobody can approve headless; refused (pass -y or --mode auto)", hub.mode_name());
                    if o.json {
                        jsonl::write(&mut out, &serde_json::json!({"type": "permission_denied", "session": name, "tool": tool, "args": args, "warn": warn, "message": why}));
                    } else {
                        eprintln!("[{name}] {why}");
                    }
                    let _ = reply.send(hub::Answer::No);
                }
                _ => {}
            }
            // spawned helpers report back to main, so wait for the whole hub, not just main
            if hub.quiet() {
                if !o.json { println!() }
                break;
            }
        }
        m.cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        if o.json {
            jsonl::write(&mut out, &jsonl::usage(&lock(&m.turn).usage));
        }
        if code != jsonl::OK {
            break;
        }
    }
    if code == jsonl::OK && denied {
        code = jsonl::NEEDS_PERMISSION;
    }
    if o.json {
        jsonl::write(&mut out, &serde_json::json!({"type": "done", "ok": code == jsonl::OK, "code": code, "answer": answer}));
    }
    peak = peak.max(lock(&m.turn).tokens);
    if let Some(path) = o.stats {
        let _ = std::fs::write(path, serde_json::json!({"tokens": peak, "calls": calls, "compacts": compacts}).to_string());
    }
    code
}

/// `qh __tool <name> '<json args>'`: one tool call in the current folder as a session would make
/// it, with the safety guard and the sandbox, no model and nobody to ask (a guarded call is
/// refused, exit 3). For the red-team tests; network asks are refused too.
fn tool_once(args: &[String]) -> i32 {
    let cfg = config::load();
    if args.first().map(String::as_str) == Some("--status") {
        match sandbox::unavailable() {
            None => println!("sandboxed"),
            Some(why) => println!("unsandboxed: {why}"),
        }
        return 0;
    }
    // `qh __tool --seq '[[tab, name, args], …]'`: several calls in one qh, in order, each for its tab
    // (background tasks and the sandbox network are per tab); outputs separated by SEQ_MARK
    let calls: Vec<(String, String, serde_json::Value)> = if args.first().map(String::as_str) == Some("--seq") {
        match args.get(1).and_then(|a| serde_json::from_str(a).ok()) {
            Some(v) => v,
            None => { eprintln!("usage: qh __tool --seq '[[tab, name, args], …]'"); return 2 }
        }
    } else {
        let (Some(name), Some(a)) = (args.first(), args.get(1).and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())) else {
            eprintln!("usage: qh __tool <name> '<json args>'");
            return 2;
        };
        vec![(String::new(), name.clone(), a)]
    };
    let mut cwd = std::env::current_dir().unwrap_or_default();
    let hosts = cfg.sandbox_hosts.clone();
    let mut proxy = None;
    let mut code = 0;
    for (i, (tab, name, a)) in calls.iter().enumerate() {
        if i > 0 {
            println!("--qh-seq--");
        }
        // the tab's ports the browser may open (forwarded out of its namespace on Linux), for the tests
        #[cfg(target_os = "linux")]
        if name == "--tab-ports" {
            println!("ports {:?}", sandbox_linux::tab_ports(tab));
            continue;
        }
        if let Some(why) = sandbox::guard(name, a, &cwd, &cwd, &cfg) {
            println!("refused: it {why}");
            code = 3;
            continue;
        }
        if proxy.is_none() {
            let hosts = hosts.clone();
            let p = proxy::start(std::sync::Arc::new(move |h: &str, _| sandbox::host_listed(h, &hosts))).ok();
            #[cfg(unix)]
            let p = p.map(|p| (p.port, p.sock));
            #[cfg(not(unix))]
            let p = p.map(|p| (p.port, None));
            proxy = Some(p);
        }
        let px = proxy.clone().flatten();
        let sb = sandbox::Ctx { root: cwd.clone(), proxy: px, tab: tab.clone() };
        println!("{}", tools::call(name, a, &cfg, &mut cwd, &tier::Limits::for_ctx(32768), &sb));
    }
    bg::kill_all();
    code
}
