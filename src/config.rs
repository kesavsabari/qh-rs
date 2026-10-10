use std::path::PathBuf;

#[derive(Clone)]
pub struct Webhook {
    pub name: String,
    pub url: String,
    pub description: String,
    pub confirm: bool,
}

#[derive(Clone)]
pub struct Hook {
    pub event: String,   // pre_tool | post_tool | stop
    pub matcher: String, // regex on the tool name; empty = every tool
    pub cmd: String,
}

#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub model: String,
    pub ctx: usize,
    pub compact_at: usize,
    /// cap on a cloud model's window: every request resends the whole context, so a full
    /// 128k+ window burns plan usage fast
    pub cloud_ctx: usize,
    pub profile: String,
    pub keep_alive: String,
    pub fallback_model: String,
    pub openrouter_key: String,
    pub allow: Vec<String>,
    pub webhooks: Vec<Webhook>,
    pub hooks: Vec<Hook>,
    pub mcp: Vec<crate::mcp::ServerCfg>,
    pub browser_headless: bool,
    pub mode: String, // ask | auto-edit | auto
    pub browser_allow_hosts: Vec<String>, // local-network hosts the browser may open
    pub providers: Vec<(String, toml::Table)>, // [providers.<name>] tables
    pub default_provider: String,
    pub ctx_set: bool,
    pub mouse: bool, // ctx came from config or --ctx; otherwise it is sized to the GPU
}

pub fn config_dir() -> PathBuf {
    std::env::var("QH_CONFIG").ok().and_then(|p| PathBuf::from(p).parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(crate::platform::config_base)
}

pub fn home() -> PathBuf {
    crate::platform::home()
}

/// Shared with the Python harness, so sessions and hubs resume across both.
pub fn state_dir() -> PathBuf {
    std::env::var("QH_STATE_DIR").map(PathBuf::from).unwrap_or_else(|_| crate::platform::state_base())
}

pub fn load() -> Config {
    let mut c = Config {
        // OLLAMA_HOST is what the ollama CLI itself reads ("127.0.0.1:11434" or a full URL)
        host: std::env::var("OLLAMA_HOST").ok().filter(|h| !h.is_empty() && !h.starts_with("0.0.0.0"))
            .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
            .unwrap_or_else(|| "http://127.0.0.1:11434".into()),
        model: "qwen3.5:9b".into(),
        ctx: 32768,
        compact_at: 11000,
        cloud_ctx: 65536,
        profile: "coding".into(),
        keep_alive: "30m".into(),
        fallback_model: String::new(),
        openrouter_key: String::new(),
        allow: vec![],
        webhooks: vec![],
        hooks: vec![],
        mcp: vec![],
        browser_headless: false,
        mode: "ask".into(),
        browser_allow_hosts: vec![],
        providers: vec![],
        default_provider: String::new(),
        ctx_set: false,
        mouse: true,
    };
    let path = std::env::var("QH_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::platform::config_base().join("config.toml"));
    if let Some(t) = std::fs::read_to_string(path).ok().and_then(|s| s.parse::<toml::Table>().ok()) {
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
        let n = |k: &str| t.get(k).and_then(|v| v.as_integer()).map(|v| v as usize);
        if let Some(v) = s("host") { c.host = v }
        if let Some(v) = s("model") { c.model = v }
        if let Some(v) = n("ctx") { c.ctx = v; c.ctx_set = true }
        if let Some(v) = n("compact_at") { c.compact_at = v }
        if let Some(v) = n("cloud_ctx") { c.cloud_ctx = v }
        if let Some(v) = s("profile") { c.profile = v }
        if let Some(v) = s("keep_alive") { c.keep_alive = v }
        if let Some(v) = s("fallback_model") { c.fallback_model = v }
        if let Some(v) = s("openrouter_key") { c.openrouter_key = v }
        if let Some(a) = t.get("allow").and_then(|v| v.as_array()) {
            c.allow = a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(v) = s("mode") { c.mode = v }
        if let Some(p) = t.get("providers").and_then(|v| v.as_table()) {
            c.providers = p.iter().filter_map(|(k, v)| v.as_table().map(|t| (k.clone(), t.clone()))).collect();
        }
        if let Some(v) = t.get("mouse").and_then(|v| v.as_bool()) { c.mouse = v }
        if let Some(v) = t.get("browser_headless").and_then(|v| v.as_bool()) { c.browser_headless = v }
        if let Some(a) = t.get("browser_allow_hosts").and_then(|v| v.as_array()) {
            c.browser_allow_hosts = a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(m) = t.get("mcp").and_then(|v| v.as_table()) {
            for (name, v) in m {
                let Some(h) = v.as_table() else { continue };
                let Some(cmd) = h.get("cmd").and_then(|v| v.as_str()) else { continue };
                let strs = |k: &str| h.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
                let env = h.get("env").and_then(|v| v.as_table()).map(|e| e.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()).unwrap_or_default();
                c.mcp.push(crate::mcp::ServerCfg { name: name.replace(crate::mcp::SEP, "_"), cmd: cmd.into(), args: strs("args"), env,
                    trusted: h.get("trusted").and_then(|v| v.as_bool()).unwrap_or(false) });
            }
        }
        if let Some(a) = t.get("hooks").and_then(|v| v.as_array()) {
            for h in a.iter().filter_map(|v| v.as_table()) {
                let g = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !g("cmd").is_empty() && ["pre_tool", "post_tool", "stop"].contains(&g("event").as_str()) {
                    c.hooks.push(Hook { event: g("event"), matcher: g("match"), cmd: g("cmd") });
                }
            }
        }
        if let Some(a) = t.get("webhooks").and_then(|v| v.as_array()) {
            for h in a.iter().filter_map(|v| v.as_table()) {
                let g = |k: &str| h.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
                if g("name").is_empty() || g("url").is_empty() {
                    continue;
                }
                c.webhooks.push(Webhook {
                    name: g("name"),
                    url: g("url"),
                    description: Some(g("description")).filter(|d| !d.is_empty())
                        .unwrap_or_else(|| "Call an n8n workflow.".into()),
                    confirm: h.get("confirm").and_then(|v| v.as_bool()).unwrap_or(true),
                });
            }
        }
    }
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
        c.openrouter_key = k;
    }
    c
}
