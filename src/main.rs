mod backend;
mod browser;
mod chatgpt;
mod checkpoints;
mod config;
mod context;
mod engine;
mod map;
mod oauth;
mod platform;
mod mcp;
mod notify;
mod hooks;
mod hub;
mod link;
mod prompt;
mod providers;
mod proxy;
mod relay;
mod sandbox;
#[cfg(target_os = "linux")]
mod sandbox_linux;
mod sandbox_macos;
mod sandbox_windows;
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
qh -p \"prompt\" [-p \"next turn\" ...] [-y] [--ctx N] [--stats FILE]
    run prompts as turns of one session without the UI and print the replies
    -y approves every tool; --ctx overrides the context window (compacts at 88%);
    --stats writes {tokens, calls, compacts} as JSON. QH_STATE_DIR isolates history.";

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
}

fn parse() -> Args {
    let mut a = Args { resume: None, profile: None, model: None, think: false, print: vec![], yes: false, ctx: None, stats: None, mode: None };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--resume" | "-r" => a.resume = Some(it.next_if(|v| !v.starts_with('-')).unwrap_or_else(|| "latest".into())),
            "--profile" => a.profile = it.next(),
            "--model" | "-m" => a.model = it.next(),
            "--think" => a.think = true,
            "-p" | "--print" => a.print.extend(it.next()),
            "--ctx" => a.ctx = it.next().and_then(|v| v.parse().ok()),
            "--stats" => a.stats = it.next(),
            "-y" | "--yes" => a.yes = true,
            "--mode" => a.mode = it.next(),
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
        (Some("sandbox"), Some("reset")) => {
            // Windows: take back every folder permission the sandbox granted (they return on the next command)
            #[cfg(windows)]
            match sandbox_windows::reset() {
                Ok(n) => { println!("removed the sandbox's permissions from {n} folders"); std::process::exit(0) }
                Err(e) => { eprintln!("{e}"); std::process::exit(1) }
            }
            #[cfg(not(windows))]
            {
                println!("nothing to reset: only the Windows sandbox changes folder permissions");
                std::process::exit(0);
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
    if args.print.is_empty() && setup::needed() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        providers::init(&config::load());
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
    if !args.print.is_empty() {
        let code = headless(&hub, &args.print, rx, args.stats.as_deref());
        browser::shutdown();
        std::process::exit(code);
    }
    update::check_in_background(update_check);
    let r = ui::run(hub, tx, rx);
    browser::shutdown();
    if let Err(e) = r {
        eprintln!("{e}");
    }
}

/// Run each prompt as a turn of "main", printing replies to stdout and tool
/// calls to stderr.
fn headless(hub: &std::sync::Arc<Hub>, prompts: &[String], rx: std::sync::mpsc::Receiver<ui::Ui>, stats: Option<&str>) -> i32 {
    let main = hub.names()[0].clone();
    let Some(m) = hub.get(&main) else { return 1 };
    let mut out = std::io::stdout();
    let (mut code, mut calls, mut compacts, mut peak) = (0, 0, 0, 0);
    for p in prompts {
        hub.send(&main, p, "user", 0, None);
        while let Ok(ui::Ui::Hub(name, ev)) = rx.recv() {
            match ev {
                Event::Token(t) if name == main => {
                    let _ = out.write_all(t.as_bytes());
                    let _ = out.flush();
                }
                Event::ToolCall { name: t, args } => {
                    calls += 1;
                    eprintln!("[{name}] tool {t} {args}");
                }
                Event::Compact { .. } => compacts += 1,
                Event::Done { tokens, .. } => peak = peak.max(tokens),
                Event::Notice(n) => {
                    eprintln!("[{name}] {n}");
                    if n.starts_with("error") {
                        code = 1;
                    }
                }
                Event::Confirm { reply, .. } => {
                    let _ = reply.send(hub::Answer::No);
                }
                _ => {}
            }
            // spawned helpers report back to main, so wait for the whole hub, not just main
            if hub.quiet() {
                println!();
                break;
            }
        }
    }
    peak = peak.max(lock(&m.turn).tokens);
    if let Some(path) = stats {
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
    let (Some(name), Some(a)) = (args.first(), args.get(1).and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())) else {
        eprintln!("usage: qh __tool <name> '<json args>'");
        return 2;
    };
    let mut cwd = std::env::current_dir().unwrap_or_default();
    if let Some(why) = sandbox::guard(name, &a, &cwd, &cwd, &cfg) {
        println!("refused: it {why}");
        return 3;
    }
    let hosts = cfg.sandbox_hosts.clone();
    let proxy = proxy::start(std::sync::Arc::new(move |h: &str, _| sandbox::host_listed(h, &hosts))).ok();
    #[cfg(unix)]
    let proxy = proxy.map(|p| (p.port, p.sock));
    #[cfg(not(unix))]
    let proxy = proxy.map(|p| (p.port, None));
    let sb = sandbox::Ctx { root: cwd.clone(), proxy: if cfg!(windows) { None } else { proxy } };
    println!("{}", tools::call(name, &a, &cfg, &mut cwd, &tier::Limits::for_ctx(32768), &sb));
    0
}
