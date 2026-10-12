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
    /// warn once when a session's spend on API keys passes this many dollars (0 = never)
    pub cost_warn: f64,
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
    /// qh-link relay server for teammates nothing else reaches: "default", "none", or
    /// "host:port#secret" (from `qh relay`), comma-separated for several
    pub relay: String,
    /// look for a newer qh once a day (`update_check = "off"` or false stops it)
    pub update_check: bool,
    /// the safety guard and the OS sandbox for commands (`[sandbox] mode = "off"` turns both off)
    pub sandbox: bool,
    /// `[sandbox] writable`: more folders the model may write without asking
    pub sandbox_write: Vec<String>,
    /// `[sandbox] allow_hosts`: more hosts commands may reach without asking ("example.com", "*.example.com")
    pub sandbox_hosts: Vec<String>,
    /// `[sandbox] env_keep`: secret-looking variables commands still get
    pub sandbox_env_keep: Vec<String>,
    /// `[sandbox] network`: "allowlist" (default: through the proxy, other hosts ask), "off" or "open"
    pub sandbox_network: String,
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

/// Set one top-level `key = "value"` in config.toml, keeping the rest of the file (comments,
/// tables) as it is: an existing line is replaced, otherwise the setting goes above the first table.
pub fn set(key: &str, value: &str) -> Result<PathBuf, String> {
    let path = std::env::var("QH_CONFIG").map(PathBuf::from).unwrap_or_else(|_| crate::platform::config_base().join("config.toml"));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // private: it can hold a relay's secret
    crate::platform::write_private(&path, set_in(&text, key, value).as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

fn set_in(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = \"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
    let mut out: Vec<String> = vec![];
    let (mut done, mut in_table) = (false, false);
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            if !done {
                out.push(line.clone());
                done = true;
            }
            in_table = true;
        }
        if !in_table && t.split('=').next().is_some_and(|k| k.trim() == key) {
            if !done {
                out.push(line.clone());
                done = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        out.push(line);
    }
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_changes_one_setting_and_keeps_the_rest() {
        let text = "# mine\nmodel = \"a\"\nrelay = \"none\"\n\n[providers.x]\nrelay = \"table, untouched\"\n";
        let out = super::set_in(text, "relay", "1.2.3.4:47232#k");
        assert_eq!(out, "# mine\nmodel = \"a\"\nrelay = \"1.2.3.4:47232#k\"\n\n[providers.x]\nrelay = \"table, untouched\"\n");
        let out = super::set_in("model = \"a\"\n[t]\nx = 1\n", "relay", "none");
        assert_eq!(out, "model = \"a\"\nrelay = \"none\"\n[t]\nx = 1\n");
        assert_eq!(super::set_in("", "relay", "say \"hi\""), "relay = \"say \\\"hi\\\"\"\n");
        let parsed: toml::Table = super::set_in("", "relay", "a\\b\"c").parse().unwrap();
        assert_eq!(parsed["relay"].as_str(), Some("a\\b\"c"));
    }
}

/// The built-in settings, before config.toml.
pub fn load_defaults() -> Config {
    Config {
        // OLLAMA_HOST is what the ollama CLI itself reads ("127.0.0.1:11434" or a full URL)
        host: std::env::var("OLLAMA_HOST").ok().filter(|h| !h.is_empty() && !h.starts_with("0.0.0.0"))
            .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
            .unwrap_or_else(|| "http://127.0.0.1:11434".into()),
        model: "qwen3.5:9b".into(),
        ctx: 32768,
        compact_at: 11000,
        cloud_ctx: 0, // no cap: a cloud model uses its whole window
        cost_warn: 2.0,
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
        relay: "default".into(),
        update_check: true,
        sandbox: true,
        sandbox_write: vec![],
        sandbox_hosts: vec![],
        sandbox_env_keep: vec![],
        sandbox_network: "allowlist".into(),
    }
}

/// The config loaded once, for code with no Config at hand (tool schemas, the file walker).
pub fn load_cached() -> &'static Config {
    static C: std::sync::OnceLock<Config> = std::sync::OnceLock::new();
    C.get_or_init(load)
}

pub fn load() -> Config {
    let mut c = load_defaults();
    let path = std::env::var("QH_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::platform::config_base().join("config.toml"));
    if let Some(t) = std::fs::read_to_string(&path).ok().and_then(|s| s.parse::<toml::Table>().ok()) {
        // it holds secrets (a relay's key, API keys): only its owner may read it
        let secret = t.get("relay").and_then(|v| v.as_str()).is_some_and(|r| r.contains('#'))
            || t.contains_key("openrouter_key") || t.contains_key("providers");
        #[cfg(unix)]
        if secret {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        #[cfg(not(unix))]
        let _ = secret;
        let s = |k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
        let n = |k: &str| t.get(k).and_then(|v| v.as_integer()).map(|v| v as usize);
        if let Some(v) = s("host") { c.host = v }
        if let Some(v) = s("model") { c.model = v }
        if let Some(v) = n("ctx") { c.ctx = v; c.ctx_set = true }
        if let Some(v) = n("compact_at") { c.compact_at = v }
        if let Some(v) = n("cloud_ctx") { c.cloud_ctx = v }
        if let Some(v) = t.get("cost_warn").and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))) { c.cost_warn = v }
        if let Some(v) = s("profile") { c.profile = v }
        if let Some(v) = s("keep_alive") { c.keep_alive = v }
        if let Some(v) = s("fallback_model") { c.fallback_model = v }
        if let Some(v) = s("openrouter_key") { c.openrouter_key = v }
        if let Some(a) = t.get("allow").and_then(|v| v.as_array()) {
            c.allow = a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(v) = s("mode") { c.mode = v }
        if let Some(v) = s("relay") { c.relay = v }
        match t.get("update_check") {
            Some(toml::Value::Boolean(b)) => c.update_check = *b,
            Some(toml::Value::String(v)) => c.update_check = !matches!(v.as_str(), "off" | "false" | "no"),
            _ => {}
        }
        if let Some(sb) = t.get("sandbox").and_then(|v| v.as_table()) {
            let list = |k: &str| sb.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            c.sandbox = !matches!(sb.get("mode"), Some(toml::Value::String(m)) if matches!(m.as_str(), "off" | "false" | "no"))
                && !matches!(sb.get("mode"), Some(toml::Value::Boolean(false)));
            (c.sandbox_write, c.sandbox_hosts, c.sandbox_env_keep) = (list("writable"), list("allow_hosts"), list("env_keep"));
            if let Some(n) = sb.get("network").and_then(|v| v.as_str()) {
                c.sandbox_network = n.to_lowercase();
            }
        }
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
