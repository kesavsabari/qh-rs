//! `qh setup`, also run on first start when there is no config file: finds a local Ollama
//! (lists its models, can pull one) or walks through a cloud provider (key, test, model),
//! then writes config.toml.
use crate::backend::http;
use crate::config::{config_dir, state_dir};
use crate::providers;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

const LOCAL_PICK: &str = "qwen3.5:9b"; // what the harness is tuned and evaluated on

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

fn choose(q: &str, opts: &[String]) -> Option<usize> {
    for (i, o) in opts.iter().enumerate() {
        println!("  {}) {o}", i + 1);
    }
    loop {
        let a = ask(q);
        if a.is_empty() || a == "q" { return None }
        match a.parse::<usize>() {
            Ok(n) if (1..=opts.len()).contains(&n) => return Some(n - 1),
            _ => println!("  type a number from 1 to {}", opts.len()),
        }
    }
}

fn local_models(host: &str) -> Option<Vec<String>> {
    let r = http().get(&format!("{host}/api/tags")).timeout(Duration::from_secs(3)).call().ok()?;
    let v: Value = serde_json::from_str(&r.into_string().ok()?).ok()?;
    Some(v["models"].as_array()?.iter().filter_map(|m| m["name"].as_str().map(String::from))
        .filter(|n| !n.ends_with("cloud")).collect())
}

fn pull(host: &str, model: &str) -> bool {
    println!("pulling {model} (several GB; Ctrl+C to stop)…");
    let r = http().post(&format!("{host}/api/pull")).send_string(&json!({"model": model, "stream": true}).to_string());
    let Ok(r) = r else { println!("pull failed"); return false };
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

fn write_config(lines: &[String]) -> Result<std::path::PathBuf, String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("config.toml");
    let mut body = String::from("# written by `qh setup`; run it again to change, or edit by hand\n");
    for l in lines { body += l; body.push('\n') }
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path)
}

fn local(host: &str) -> Option<Vec<String>> {
    let mut models = local_models(host)?;
    println!("✓ Ollama is running at {host}");
    let gb = crate::tier::vram().map(|(total, _)| total / 1024.0);
    match gb {
        Some(g) => println!("  GPU: {g:.0} GB VRAM"),
        None => println!("  no NVIDIA GPU found (Apple/AMD/CPU work too, just slower or with a smaller window)"),
    }
    if !models.iter().any(|m| m == LOCAL_PICK) {
        let fits = gb.is_none_or(|g| g >= 7.5);
        let hint = if fits { "recommended" } else { "needs ~7 GB; it will run partly on CPU" };
        if ask(&format!("Pull {LOCAL_PICK} ({hint})? [Y/n] ")).to_lowercase() != "n" && pull(host, LOCAL_PICK) {
            models.insert(0, LOCAL_PICK.into());
        }
    }
    if models.is_empty() {
        println!("No models yet. Pull one with `ollama pull <name>` (see https://ollama.com/library), then run `qh setup`.");
        return None;
    }
    let i = if models.len() == 1 { 0 } else { choose("Model for new sessions [number]: ", &models)? };
    Some(vec![format!("host = \"{host}\""), format!("model = \"{}\"", models[i])])
}

fn cloud() -> Option<(Vec<String>, String)> {
    let opts: Vec<(&str, &str)> = vec![
        ("openrouter", "one key, hundreds of models, some free — https://openrouter.ai/keys"),
        ("groq", "very fast, free tier — https://console.groq.com/keys"),
        ("ollama-cloud", "Ollama's hosted models by API key (with the Ollama app, sign-in is easier) — https://ollama.com/settings/keys"),
        ("openai", "https://platform.openai.com/api-keys"),
        ("anthropic", "https://console.anthropic.com/settings/keys"),
        ("deepseek", "https://platform.deepseek.com/api_keys"),
        ("mistral", "https://console.mistral.ai/api-keys"),
    ];
    let labels: Vec<String> = opts.iter().map(|(n, d)| format!("{n:<13} {d}")).collect();
    let name = opts[choose("Provider [number]: ", &labels)?].0;
    loop {
        print!("Paste your {name} API key (hidden): ");
        let _ = std::io::stdout().flush();
        let key = crate::platform::read_hidden();
        println!();
        if key.trim().is_empty() { return None }
        if let Err(e) = providers::set_key(name, &key) { println!("could not save the key: {e}"); return None }
        match providers::test(name) {
            Ok(n) => { println!("✓ key works, {n} models"); break }
            Err(e) => {
                let _ = providers::remove_key(name); // don't keep a key that doesn't work
                println!("✗ {e}");
                if ask("Try another key? [Y/n] ").to_lowercase() == "n" { return None }
            }
        }
    }
    let all: Vec<String> = providers::get(name).and_then(|p| providers::models(&p).ok())
        .map(|v| v.into_iter().map(|m| m.id).collect()).unwrap_or_default();
    Some((vec![], providers::display(name, &search(&all))))
}

fn search(all: &[String]) -> String {
    loop {
        let q = ask("Search models (e.g. qwen, coder, llama; Enter for all): ").to_lowercase();
        let hits: Vec<String> = all.iter().filter(|m| m.to_lowercase().contains(&q)).take(20).cloned().collect();
        if hits.is_empty() { println!("  nothing matches"); continue }
        if let Some(i) = choose("Model [number, Enter to search again]: ", &hits) { return hits[i].clone() }
    }
}

/// Ollama's own cloud: the local app signs in to ollama.com (browser login, no API key) and then
/// runs `…-cloud` models on their servers, so a laptop without a big GPU gets big models.
fn ollama_cloud(host: &str) -> Option<Vec<String>> {
    println!("Cloud models run on ollama.com through your Ollama app: no API key, just an ollama.com account (free tier).");
    match std::process::Command::new("ollama").arg("signin").status() {
        Ok(s) if s.success() => println!("✓ signed in"),
        Ok(_) => println!("(sign-in didn't finish; you can also sign in from the Ollama app's settings)"),
        Err(_) => println!("Sign in from the Ollama app (Settings → sign in), then come back here."),
    }
    let all: Vec<String> = crate::backend::list_models(host).into_iter().filter(|m| crate::backend::is_cloud(m)).collect();
    if all.is_empty() {
        println!("Couldn't load the cloud model list from ollama.com; check your internet and run `qh setup` again.");
        return None;
    }
    Some(vec![format!("host = \"{host}\""), format!("model = \"{}\"", search(&all))])
}

/// Interactive setup. Returns false if the user backed out without a working model.
pub fn run() -> bool {
    println!("qh setup — let's connect a model.\n");
    let host = crate::config::load().host;
    let has_ollama = local_models(&host).is_some();
    let mut choices = vec![];
    if has_ollama {
        choices.push("Local model with Ollama (private, free, uses your GPU)".to_string());
        choices.push("Ollama cloud models (sign in to ollama.com, no API key, no GPU needed)".to_string());
    }
    choices.push("Cloud model with an API key (no GPU needed)".to_string());
    if !has_ollama { choices.push("I want local models: how do I install Ollama?".to_string()) }
    let Some(i) = choose("How do you want to run models? [number]: ", &choices) else { return false };
    let pick = &choices[i];
    let (lines, default) = if pick.starts_with("Local") {
        match local(&host) { Some(l) => (l, String::new()), None => return false }
    } else if pick.starts_with("Ollama cloud") {
        match ollama_cloud(&host) { Some(l) => (l, String::new()), None => return false }
    } else if pick.starts_with("Cloud") {
        match cloud() { Some(x) => x, None => return false }
    } else {
        if cfg!(windows) {
            println!("\nRun in PowerShell:  winget install Ollama.Ollama");
        } else if cfg!(target_os = "macos") {
            println!("\nDownload from https://ollama.com/download (or: brew install ollama)");
        } else {
            println!("\nRun:  curl -fsSL https://ollama.com/install.sh | sh");
        }
        println!("Then start Ollama and run `qh setup` again.");
        return false;
    };
    let mut lines = lines;
    if lines.is_empty() { lines.push("# cloud default model is saved in the state folder".into()) }
    match write_config(&lines) {
        Ok(p) => println!("\n✓ saved {}", p.display()),
        Err(e) => { println!("could not write config: {e}"); return false }
    }
    let saved = state_dir().join("default_model");
    if default.is_empty() {
        let _ = std::fs::remove_file(&saved); // a local pick: config's model applies
    } else {
        let _ = std::fs::create_dir_all(state_dir());
        let _ = std::fs::write(&saved, &default);
        println!("✓ new sessions use {default}");
    }
    println!("Ready. Type a request, /help for commands, /model to switch models. Run `qh setup` any time to redo this.\n");
    true
}

/// First start: no config file anywhere.
pub fn needed() -> bool {
    std::env::var_os("QH_CONFIG").is_none() && !config_dir().join("config.toml").exists()
}
