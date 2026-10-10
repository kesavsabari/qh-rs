//! `qh setup`, also run on first start when there is no config file. One screen to tick any mix
//! of model sources (local Ollama, Ollama's cloud via sign-in, API-key providers), a step for
//! each, then a filterable list of every model they unlocked to pick the default. All ticked
//! sources stay usable afterwards through /model. Falls back to numbered prompts without a TTY.
use crate::backend::http;
use crate::config::{config_dir, state_dir};
use crate::providers;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::time::Duration;

const LOCAL_PICK: &str = "qwen3.5:9b"; // what the harness is tuned and evaluated on
const ACCENT: Color = Color::Cyan;
const OPENROUTER_LOGIN: bool = false; // browser login: built, switched on once tested end to end

struct Item {
    label: String,
    note: String,
    on: bool,
}

fn item(label: impl Into<String>, note: impl Into<String>) -> Item {
    Item { label: label.into(), note: note.into(), on: false }
}

fn tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

// ---- plain prompts (no terminal: piped input, CI) ----------------------------------------

fn ask(q: &str) -> String {
    print!("{q}");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    if matches!(std::io::stdin().read_line(&mut s), Ok(0) | Err(_)) {
        println!("\n(input closed; setup cancelled)");
        std::process::exit(1);
    }
    s.trim().to_string()
}

fn plain_pick(title: &str, items: &mut [Item], multi: bool, filter: bool) -> Option<usize> {
    println!("\n{title}");
    let mut shown: Vec<usize> = (0..items.len()).collect();
    if filter && items.len() > 20 {
        let q = ask("Search (Enter for all): ").to_lowercase();
        shown.retain(|&i| items[i].label.to_lowercase().contains(&q));
        shown.truncate(20);
    }
    for (n, &i) in shown.iter().enumerate() {
        println!("  {}) {}  {}", n + 1, items[i].label, items[i].note);
    }
    let a = ask(if multi { "Numbers, separated by spaces: " } else { "Number: " });
    let picked: Vec<usize> = a.split([' ', ',']).filter_map(|x| x.parse::<usize>().ok())
        .filter(|n| (1..=shown.len()).contains(n)).map(|n| shown[n - 1]).collect();
    if multi {
        for (i, it) in items.iter_mut().enumerate() { it.on = picked.contains(&i) }
        Some(usize::MAX)
    } else {
        picked.first().copied()
    }
}

// ---- interactive widgets -----------------------------------------------------------------

/// A list to pick from. `multi`: space ticks, Enter confirms (result in `items[..].on`).
/// `filter`: typing narrows the list. Esc cancels (None).
fn pick(title: &str, help: &str, items: &mut [Item], multi: bool, filter: bool) -> Option<usize> {
    if !tty() {
        return plain_pick(title, items, multi, filter);
    }
    let mut term = ratatui::init();
    let (mut query, mut state) = (String::new(), ListState::default().with_selected(Some(0)));
    let result = loop {
        let q = query.to_lowercase();
        let shown: Vec<usize> = (0..items.len())
            .filter(|&i| q.is_empty() || items[i].label.to_lowercase().contains(&q) || items[i].note.to_lowercase().contains(&q))
            .collect();
        if state.selected().is_none_or(|s| s >= shown.len()) {
            state.select(if shown.is_empty() { None } else { Some(shown.len().saturating_sub(1).min(state.selected().unwrap_or(0))) });
        }
        let _ = term.draw(|f| {
            let [head, body, foot] = Layout::vertical([Constraint::Length(if filter { 3 } else { 2 }), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
            let mut top = vec![Line::from(Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))];
            if filter {
                top.push(Line::from(vec![Span::raw("search: "), Span::styled(format!("{query}▏"), Style::new().fg(Color::Yellow))]));
            }
            f.render_widget(Paragraph::new(top), head);
            let rows: Vec<ListItem> = shown.iter().map(|&i| {
                let it = &items[i];
                let mark = if multi { if it.on { "[x] " } else { "[ ] " } } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(mark, Style::new().fg(if it.on { Color::Green } else { Color::DarkGray })),
                    Span::raw(it.label.clone()),
                    Span::styled(format!("  {}", it.note), Style::new().fg(Color::DarkGray)),
                ]))
            }).collect();
            let count = if filter { format!(" {} of {} ", shown.len(), items.len()) } else { String::new() };
            let list = List::new(rows).block(Block::bordered().title(count).border_style(Style::new().fg(Color::DarkGray)))
                .highlight_style(Style::new().fg(Color::Black).bg(ACCENT)).highlight_symbol("› ");
            f.render_stateful_widget(list, body, &mut state);
            f.render_widget(Paragraph::new(Span::styled(help, Style::new().fg(Color::DarkGray))), foot);
        });
        let Ok(Event::Key(k)) = event::read() else { continue };
        if k.kind == KeyEventKind::Release { continue }
        let sel = state.selected().and_then(|s| shown.get(s).copied());
        match k.code {
            KeyCode::Esc => break None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Up => state.select_previous(),
            KeyCode::Down => state.select_next(),
            KeyCode::PageUp => state.scroll_up_by(10),
            KeyCode::PageDown => state.scroll_down_by(10),
            KeyCode::Char(' ') if multi => if let Some(i) = sel { items[i].on = !items[i].on },
            KeyCode::Enter if multi => break Some(usize::MAX),
            KeyCode::Enter => if sel.is_some() { break sel },
            KeyCode::Backspace if filter => { query.pop(); }
            KeyCode::Char(c) if filter => { query.push(c); state.select(Some(0)); }
            _ => {}
        }
    };
    ratatui::restore();
    result
}

/// One line of text input; `hidden` shows dots (API keys). Esc cancels.
fn input(title: &str, help: &str, hidden: bool) -> Option<String> {
    if !tty() {
        return Some(ask(&format!("{title}: ")));
    }
    let mut term = ratatui::init();
    let mut s = String::new();
    let result = loop {
        let shown = if hidden { "•".repeat(s.chars().count()) } else { s.clone() };
        let _ = term.draw(|f| {
            let [head, field, foot] = Layout::vertical([Constraint::Length(2), Constraint::Length(3), Constraint::Min(1)]).areas(f.area());
            f.render_widget(Paragraph::new(Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))), head);
            f.render_widget(Paragraph::new(format!("{shown}▏")).block(Block::bordered().border_style(Style::new().fg(Color::DarkGray))), field);
            f.render_widget(Paragraph::new(Span::styled(help, Style::new().fg(Color::DarkGray))), foot);
        });
        let Ok(Event::Key(k)) = event::read() else { continue };
        if k.kind == KeyEventKind::Release { continue }
        match k.code {
            KeyCode::Esc => break None,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Enter => break Some(s.trim().to_string()),
            KeyCode::Backspace => { s.pop(); }
            KeyCode::Char(c) => s.push(c),
            _ => {}
        }
    };
    ratatui::restore();
    result
}

fn yes(title: &str, yes_label: &str, no_label: &str) -> bool {
    let mut opts = [item(yes_label, ""), item(no_label, "")];
    pick(title, "↑↓ move · Enter choose", &mut opts, false, false) == Some(0)
}

// ---- sources -----------------------------------------------------------------------------

fn local_models(host: &str) -> Option<Vec<String>> {
    let r = http().get(&format!("{host}/api/tags")).timeout(Duration::from_secs(3)).call().ok()?;
    let v: Value = serde_json::from_str(&r.into_string().ok()?).ok()?;
    Some(v["models"].as_array()?.iter().filter_map(|m| m["name"].as_str().map(String::from))
        .filter(|n| !crate::backend::is_cloud(n)).collect())
}

fn pull(host: &str, model: &str) -> bool {
    println!("Downloading {model} (several GB; Ctrl+C to stop)…");
    let r = http().post(&format!("{host}/api/pull")).send_string(&json!({"model": model, "stream": true}).to_string());
    let Ok(r) = r else { println!("download failed"); return false };
    let mut last = String::new();
    for line in BufReader::new(r.into_reader()).lines().map_while(Result::ok) {
        let v: Value = serde_json::from_str(&line).unwrap_or_default();
        if let Some(e) = v["error"].as_str() { println!("\n{e}"); return false }
        let status = v["status"].as_str().unwrap_or("");
        let pct = match (v["completed"].as_f64(), v["total"].as_f64()) {
            (Some(c), Some(t)) if t > 0.0 => format!(" {:.0}%", c / t * 100.0),
            _ => String::new(),
        };
        let now = format!("{status}{pct}");
        if now != last { print!("\r  {now:<60}"); let _ = std::io::stdout().flush(); last = now }
        if status == "success" { println!(); return true }
    }
    println!();
    false
}

fn install_hint() {
    if cfg!(windows) {
        println!("Install Ollama in PowerShell:  winget install Ollama.Ollama   (then start it and run `qh setup` again)");
    } else if cfg!(target_os = "macos") {
        println!("Install Ollama from https://ollama.com/download (or: brew install ollama), start it, then run `qh setup` again.");
    } else {
        println!("Install Ollama:  curl -fsSL https://ollama.com/install.sh | sh   (then run `qh setup` again)");
    }
}

/// Models from the local Ollama, after offering the recommended one.
fn local_step(host: &str, mut models: Vec<String>) -> Vec<String> {
    if !models.iter().any(|m| m == LOCAL_PICK) {
        let gb = crate::tier::vram().map(|(total, _)| total / 1024.0);
        let fit = match gb {
            Some(g) if g < 7.5 => format!("needs ~7 GB of GPU memory, you have {g:.0} GB: it will run partly on the CPU, slowly"),
            Some(g) => format!("fits your {g:.0} GB GPU"),
            None => "no NVIDIA GPU found: Apple/AMD/CPU work, more slowly".into(),
        };
        if yes(&format!("Download {LOCAL_PICK}, the model qh is tuned on? ({fit})"), "Yes, download it (~6 GB)", "No, skip")
            && pull(host, LOCAL_PICK) {
            models.insert(0, LOCAL_PICK.into());
        }
    }
    if models.is_empty() {
        println!("No local models yet: `ollama pull <name>` (see https://ollama.com/library) adds one; it shows up in /model.");
    } else {
        println!("✓ local: {} model{}", models.len(), if models.len() == 1 { "" } else { "s" });
    }
    models
}

/// Ollama's own cloud: the app signs in to ollama.com (browser login, no API key) and then runs
/// `…-cloud` models on their servers.
fn ollama_cloud_step(host: &str) -> Vec<String> {
    println!("Signing your Ollama app in to ollama.com (a browser opens; free tier, no API key)…");
    match std::process::Command::new("ollama").arg("signin").status() {
        Ok(s) if s.success() => println!("✓ signed in"),
        Ok(_) => println!("(sign-in didn't finish: you can also sign in from the Ollama app's settings)"),
        Err(_) => println!("(no `ollama` command here: sign in from the Ollama app's settings)"),
    }
    let all: Vec<String> = crate::backend::list_models(host).into_iter().filter(|m| crate::backend::is_cloud(m)).collect();
    if all.is_empty() {
        println!("✗ couldn't load Ollama's cloud model list (internet?)");
    } else {
        println!("✓ Ollama cloud: {} models", all.len());
    }
    all
}

/// Key → test → models for one API provider. A saved key that still works is reused.
fn provider_step(name: &str, site: &str) -> Vec<String> {
    let works = |n: &str| providers::get(n).is_some_and(|p| !p.key.is_empty()) && providers::test(n).is_ok();
    if works(name) {
        println!("✓ {name}: using the saved key");
    } else {
        // OpenRouter can log in through the browser instead of a pasted key
        if name == "openrouter" && OPENROUTER_LOGIN && yes("Connect OpenRouter", "Log in with the browser (easiest)", "Paste an API key instead") {
            match crate::oauth::openrouter().and_then(|k| providers::set_key(name, &k)).and_then(|_| providers::test(name)) {
                Ok(n) => println!("✓ openrouter: logged in, {n} models"),
                Err(e) => { let _ = providers::remove_key(name); println!("✗ openrouter login failed: {e}"); return vec![] }
            }
            return providers::get(name).and_then(|p| providers::models(&p).ok())
                .map(|v| v.into_iter().map(|m| providers::display(name, &m.id)).collect()).unwrap_or_default();
        }
        loop {
            let Some(key) = input(&format!("Paste your {name} API key"), &format!("get one at {site} · Enter to save · Esc to skip {name}"), true)
                .filter(|k| !k.is_empty()) else {
                println!("– {name}: skipped");
                return vec![];
            };
            if let Err(e) = providers::set_key(name, &key) {
                println!("✗ {name}: couldn't save the key: {e}");
                return vec![];
            }
            match providers::test(name) {
                Ok(n) => { println!("✓ {name}: key works, {n} models"); break }
                Err(e) => {
                    let _ = providers::remove_key(name); // never keep a key that doesn't work
                    if !yes(&format!("{name} didn't accept that key: {e}"), "Try another key", &format!("Skip {name}")) {
                        println!("– {name}: skipped");
                        return vec![];
                    }
                }
            }
        }
    }
    providers::get(name).and_then(|p| providers::models(&p).ok())
        .map(|v| v.into_iter().map(|m| providers::display(name, &m.id)).collect()).unwrap_or_default()
}

const PROVIDERS: [(&str, &str, &str); 7] = [
    ("openrouter", "OpenRouter", "https://openrouter.ai/keys"),
    ("groq", "Groq", "https://console.groq.com/keys"),
    ("openai", "OpenAI", "https://platform.openai.com/api-keys"),
    ("anthropic", "Anthropic", "https://console.anthropic.com/settings/keys"),
    ("deepseek", "DeepSeek", "https://platform.deepseek.com/api_keys"),
    ("mistral", "Mistral", "https://console.mistral.ai/api-keys"),
    ("ollama-cloud", "Ollama cloud by API key", "https://ollama.com/settings/keys"),
];

fn write_config(host: &str, model: Option<&str>) -> Result<std::path::PathBuf, String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("config.toml");
    let mut body = format!("# written by `qh setup`; run it again to change, or edit by hand\nhost = \"{host}\"\n");
    if let Some(m) = model { body += &format!("model = \"{m}\"\n") }
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Interactive setup. Returns false if the user backed out without a working model.
pub fn run() -> bool {
    println!("qh setup: connect your models.\n");
    let host = crate::config::load().host;
    let local = local_models(&host);
    let saved = |n: &str| providers::get(n).is_some_and(|p| !p.key.is_empty());

    let mut src = vec![
        item("Local models (Ollama)", match &local {
            Some(m) => format!("running · {} model{} · private, free, uses your GPU", m.len(), if m.len() == 1 { "" } else { "s" }),
            None => "Ollama not running · tick for install help".into(),
        }),
        item("Ollama cloud", if local.is_some() { "sign in to ollama.com · no API key · big models without a GPU" }
             else { "needs the Ollama app" }),
    ];
    src[0].on = local.is_some();
    for (id, label, _) in PROVIDERS {
        let mut it = item(label, if saved(id) { "key saved".to_string() } else { "API key".to_string() });
        it.on = saved(id);
        src.push(it);
    }
    if pick("Which models do you want to use? Tick as many as you like.", "↑↓ move · Space tick · Enter continue · Esc quit",
            &mut src, true, false).is_none() {
        return false;
    }
    if !src.iter().any(|s| s.on) {
        println!("Nothing ticked, so nothing to set up. Run `qh setup` again when you're ready.");
        return false;
    }

    // (model id as /model takes it, where it comes from)
    let mut all: Vec<(String, String)> = vec![];
    if src[0].on {
        match &local {
            Some(m) => all.extend(local_step(&host, m.clone()).into_iter().map(|m| (m, "local".to_string()))),
            None => install_hint(),
        }
    }
    if src[1].on {
        if local.is_some() {
            all.extend(ollama_cloud_step(&host).into_iter().map(|m| (m, "Ollama cloud".to_string())));
        } else {
            println!("Ollama cloud runs through the Ollama app:");
            install_hint();
        }
    }
    for (i, (id, label, site)) in PROVIDERS.iter().enumerate() {
        if src[2 + i].on {
            all.extend(provider_step(id, site).into_iter().map(|m| (m, label.to_string())));
        }
    }
    if all.is_empty() {
        println!("\nNo models connected yet. Fix the step above and run `qh setup` again.");
        return false;
    }

    let mut list: Vec<Item> = all.iter().map(|(m, from)| item(m.clone(), from.clone())).collect();
    let default = if list.len() == 1 { Some(0) } else {
        pick(&format!("Pick your default model ({} available; all stay in /model).", list.len()),
             "type to search · ↑↓ move · Enter choose · Esc keeps the first one", &mut list, false, true)
    }.unwrap_or(0);
    let chosen = all[default].0.clone();
    let plain_ollama = providers::parse_model(&chosen).0.is_empty();
    let first_local = all.iter().find(|(_, f)| f == "local" || f == "Ollama cloud").map(|(m, _)| m.as_str());
    match write_config(&host, if plain_ollama { Some(&chosen) } else { first_local }) {
        Ok(p) => println!("\n✓ saved {}", p.display()),
        Err(e) => { println!("could not write config: {e}"); return false }
    }
    let _ = std::fs::create_dir_all(state_dir());
    let _ = std::fs::write(state_dir().join("default_model"), &chosen);
    println!("✓ default model: {chosen}");
    println!("Ready. Type a request, /model to switch models, /help for the rest. `qh setup` redoes this.\n");
    true
}

/// First start: no config file anywhere.
pub fn needed() -> bool {
    std::env::var_os("QH_CONFIG").is_none() && !config_dir().join("config.toml").exists()
}
