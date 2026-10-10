//! Model providers: where a model runs, how to reach it, and its key. Presets cover the
//! common ones; `[providers.<name>]` tables in config add more. Keys live in their own
//! file (chmod 600) so the config can be shared, and a per-provider env var overrides.
use crate::backend::http;
use crate::config::{config_dir, Config};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Ollama,
    OpenAI,
    Anthropic,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub name: String,
    pub kind: Kind,
    pub base: String,
    pub key_env: String,
    pub key: String,
    pub local: bool, // runs on this machine: shares the GPU lock, needs no key
}

#[derive(Clone, Debug, Default)]
pub struct ModelInfo {
    pub id: String,
    pub ctx: Option<usize>,
    pub price_in: f64, // $ per million tokens; 0 = free or unknown
    pub price_out: f64,
}

// name, kind, base url, key env var, local
// Anthropic's key var is deliberately not ANTHROPIC_API_KEY: exporting that globally switches
// Claude Code itself to per-token billing.
const PRESETS: [(&str, Kind, &str, &str, bool); 9] = [
    ("openrouter", Kind::OpenAI, "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY", false),
    ("ollama-cloud", Kind::Ollama, "https://ollama.com", "OLLAMA_API_KEY", false),
    ("openai", Kind::OpenAI, "https://api.openai.com/v1", "OPENAI_API_KEY", false),
    ("groq", Kind::OpenAI, "https://api.groq.com/openai/v1", "GROQ_API_KEY", false),
    ("deepseek", Kind::OpenAI, "https://api.deepseek.com/v1", "DEEPSEEK_API_KEY", false),
    ("mistral", Kind::OpenAI, "https://api.mistral.ai/v1", "MISTRAL_API_KEY", false),
    ("anthropic", Kind::Anthropic, "https://api.anthropic.com", "QH_ANTHROPIC_KEY", false),
    ("lmstudio", Kind::OpenAI, "http://localhost:1234/v1", "", true),
    ("llamacpp", Kind::OpenAI, "http://localhost:8080/v1", "", true),
];

fn reg() -> &'static RwLock<Vec<Provider>> {
    static R: OnceLock<RwLock<Vec<Provider>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(vec![]))
}

fn creds_path() -> std::path::PathBuf {
    config_dir().join("credentials.toml")
}

fn read_creds() -> BTreeMap<String, String> {
    let t = std::fs::read_to_string(creds_path()).ok().and_then(|s| s.parse::<toml::Table>().ok()).unwrap_or_default();
    let keys = t.get("keys").and_then(|v| v.as_table()).cloned().unwrap_or_default();
    keys.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect()
}

fn write_creds(keys: &BTreeMap<String, String>) -> Result<(), String> {
    let _ = std::fs::create_dir_all(config_dir());
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut body = String::from("# API keys for qh. Keep this file private (chmod 600).\n[keys]\n");
    for (k, v) in keys {
        body += &format!("\"{}\" = \"{}\"\n", esc(k), esc(v));
    }
    let path = creds_path();
    let tmp = path.with_extension("tmp");
    crate::platform::write_private(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Build the registry: the local Ollama from config, presets, `[providers.x]` tables, then keys.
pub fn init(cfg: &Config) {
    let mut v = vec![Provider {
        name: "ollama".into(), kind: Kind::Ollama, base: cfg.host.clone(), key_env: String::new(),
        key: String::new(), local: true,
    }];
    for (name, kind, base, env, local) in PRESETS {
        v.push(Provider { name: name.into(), kind, base: base.into(), key_env: env.into(), key: String::new(), local });
    }
    for (name, t) in &cfg.providers {
        let g = |k: &str| t.get(k).and_then(|x| x.as_str()).map(String::from);
        let kind = match g("kind").as_deref() {
            Some("anthropic") => Kind::Anthropic,
            Some("ollama") => Kind::Ollama,
            _ => Kind::OpenAI,
        };
        let local = t.get("local").and_then(|x| x.as_bool())
            .unwrap_or_else(|| g("base_url").is_some_and(|b| b.contains("localhost") || b.contains("127.0.0.1")));
        let p = Provider {
            name: name.clone(), kind, base: g("base_url").unwrap_or_default(), key_env: g("api_key_env").unwrap_or_default(),
            key: g("api_key").unwrap_or_default(), local,
        };
        match v.iter_mut().find(|x| x.name == *name) {
            Some(x) => {
                if !p.base.is_empty() {
                    x.base = p.base;
                }
                if !p.key_env.is_empty() {
                    x.key_env = p.key_env;
                }
                if !p.key.is_empty() {
                    x.key = p.key;
                }
                if t.contains_key("kind") {
                    x.kind = p.kind;
                }
            }
            None if !p.base.is_empty() => v.push(p),
            None => {}
        }
    }
    let creds = read_creds();
    for p in v.iter_mut() {
        if let Some(k) = creds.get(&p.name) {
            p.key = k.clone();
        }
        if p.name == "openrouter" && p.key.is_empty() {
            p.key = cfg.openrouter_key.clone();
        }
        if let Ok(k) = std::env::var(&p.key_env) {
            if !p.key_env.is_empty() && !k.is_empty() {
                p.key = k;
            }
        }
    }
    *reg().write().unwrap_or_else(|e| e.into_inner()) = v;
}

pub fn get(name: &str) -> Option<Provider> {
    let name = if name == "or" { "openrouter" } else { name };
    reg().read().unwrap_or_else(|e| e.into_inner()).iter().find(|p| p.name == name).cloned()
}

pub fn all() -> Vec<Provider> {
    reg().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Providers a model can be picked from: local ones, plus remote ones that have a key.
pub fn usable() -> Vec<Provider> {
    all().into_iter().filter(|p| p.local || !p.key.is_empty()).collect()
}

/// "openrouter:qwen/qwen3-coder" -> ("openrouter", "qwen/qwen3-coder"); "or:x" is short for
/// openrouter. Anything else ("qwen3.5:9b") is a model on the local Ollama.
pub fn parse_model(id: &str) -> (String, String) {
    match id.split_once(':') {
        Some(("ollama", m)) => (String::new(), m.to_string()),
        Some((p, m)) if !m.is_empty() => match get(p) {
            Some(prov) => (prov.name, m.to_string()),
            None => (String::new(), id.to_string()),
        },
        _ => (String::new(), id.to_string()),
    }
}

/// How a model is shown and typed: "qwen3.5:9b" for the local Ollama, else "provider:model".
pub fn display(provider: &str, model: &str) -> String {
    if provider.is_empty() || provider == "ollama" { model.to_string() } else { format!("{provider}:{model}") }
}

pub fn resolve(provider: &str) -> Option<Provider> {
    get(if provider.is_empty() { "ollama" } else { provider })
}

/// Runs on this machine's GPU (and so takes turns with the other local sessions).
pub fn is_local(provider: &str, model: &str) -> bool {
    resolve(provider).is_some_and(|p| p.local) && !crate::backend::is_cloud(model)
}

pub fn set_key(name: &str, key: &str) -> Result<(), String> {
    let p = get(name).ok_or_else(|| format!("no provider {name}; add a [providers.{name}] table with kind and base_url"))?;
    let mut keys = read_creds();
    keys.insert(p.name.clone(), key.trim().to_string());
    write_creds(&keys)?;
    let mut r = reg().write().unwrap_or_else(|e| e.into_inner());
    if let Some(x) = r.iter_mut().find(|x| x.name == p.name) {
        x.key = key.trim().to_string();
    }
    drop(r);
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(&p.name);
    Ok(())
}

pub fn remove_key(name: &str) -> Result<(), String> {
    let p = get(name).ok_or_else(|| format!("no provider {name}"))?;
    let mut keys = read_creds();
    keys.remove(&p.name);
    write_creds(&keys)?;
    if let Some(x) = reg().write().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|x| x.name == p.name) {
        x.key.clear();
    }
    Ok(())
}

fn cache() -> &'static Mutex<BTreeMap<String, Vec<ModelInfo>>> {
    static C: OnceLock<Mutex<BTreeMap<String, Vec<ModelInfo>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

fn get_json(p: &Provider, url: &str) -> Result<Value, String> {
    let mut req = http().get(url).timeout(Duration::from_secs(8));
    match p.kind {
        Kind::Anthropic => req = req.set("x-api-key", &p.key).set("anthropic-version", "2023-06-01"),
        _ if !p.key.is_empty() => req = req.set("Authorization", &format!("Bearer {}", p.key)),
        _ => {}
    }
    let r = req.call().map_err(|e| match e {
        ureq::Error::Status(c, r) => format!("HTTP {c}: {}", r.into_string().unwrap_or_default().chars().take(200).collect::<String>()),
        e => e.to_string(),
    })?;
    serde_json::from_str(&r.into_string().map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

fn fetch_models(p: &Provider) -> Result<Vec<ModelInfo>, String> {
    let base = p.base.trim_end_matches('/');
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
    match p.kind {
        Kind::Ollama => {
            let v = get_json(p, &format!("{base}/api/tags"))?;
            Ok(v["models"].as_array().into_iter().flatten()
                .filter_map(|m| m["name"].as_str()).map(|n| ModelInfo { id: n.into(), ..Default::default() }).collect())
        }
        Kind::Anthropic => {
            let v = get_json(p, &format!("{base}/v1/models?limit=100"))?;
            Ok(v["data"].as_array().into_iter().flatten()
                .filter_map(|m| m["id"].as_str()).map(|n| ModelInfo { id: n.into(), ctx: Some(200_000), ..Default::default() }).collect())
        }
        Kind::OpenAI => {
            let v = get_json(p, &format!("{base}/models"))?;
            Ok(v["data"].as_array().into_iter().flatten().filter(|m| {
                // OpenRouter says which models take tools; keep only those
                m["supported_parameters"].as_array().is_none_or(|a| a.iter().any(|x| x == "tools"))
            }).filter_map(|m| {
                let ctx = m["context_length"].as_u64().or_else(|| m["context_window"].as_u64()).map(|c| c as usize);
                Some(ModelInfo {
                    id: m["id"].as_str()?.into(), ctx,
                    price_in: num(&m["pricing"]["prompt"]) * 1e6, price_out: num(&m["pricing"]["completion"]) * 1e6,
                })
            }).collect())
        }
    }
}

/// A provider's models, fetched once per process.
pub fn models(p: &Provider) -> Result<Vec<ModelInfo>, String> {
    if let Some(v) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&p.name) {
        return Ok(v.clone());
    }
    let mut v = fetch_models(p)?;
    v.sort_by(|a, b| a.id.cmp(&b.id));
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(p.name.clone(), v.clone());
    Ok(v)
}

/// What is known about a model without touching the network.
pub fn info(provider: &str, model: &str) -> Option<ModelInfo> {
    let p = resolve(provider)?;
    cache().lock().unwrap_or_else(|e| e.into_inner()).get(&p.name)?.iter().find(|m| m.id == model).cloned()
}

/// Check a key works: lists the provider's models.
pub fn test(name: &str) -> Result<usize, String> {
    let p = get(name).ok_or_else(|| format!("no provider {name}"))?;
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(&p.name);
    models(&p).map(|m| m.len())
}
