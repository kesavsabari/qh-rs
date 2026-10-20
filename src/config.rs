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
    /// pause and ask once spend on API keys passes this many dollars: per session / per UTC day (0 = off)
    pub budget_session: f64,
    pub budget_day: f64,
    pub profile: String,
    pub keep_alive: String,
    /// tried in order when the model is rate-limited or down; a plain string is a list of one
    pub fallback_model: Vec<String>,
    /// `[routing] ask_model`: where /ask-other and the ask_model tool go when no model is named
    pub ask_model: String,
    /// `[routing] fetch_model`: a cheap model that only runs read-only lookups for the main one (off when empty)
    pub fetch_model: String,
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
    /// `verify`: the check run after a turn that edited files; "" = auto-detect, "off" = never,
    /// "test" = auto with tests
    pub verify: String,
    /// `team_share_files = "auto" | "off"`: whether teammates' agents may read this machine's
    /// working copy of a team project over the line (read_team_file / list_team_files)
    pub team_share_files: bool,
    /// `review = "auto" | "off"`: before the agent's `git push` in a team project, ask the owners'
    /// agents to review the changes in their area
    pub review: bool,
    /// `review_wait`: seconds a push waits for those reviews before it goes ahead (0 = don't wait)
    pub review_wait: u64,
    /// look for a newer qh once a day (`update_check = "off"` or false stops it)
    pub update_check: bool,
    /// `memory = false`: no saved memories in the prompt and no remember/forget/recall tools
    pub memory: bool,
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
    /// `notify`: "auto" (bell + terminal notification), "desktop" (also notify-send / osascript) or "off"
    pub notify: String,
    /// `lsp`: "auto" (use language servers found on PATH) or "off"
    pub lsp: String,
    /// `lsp_wait_ms`: how long an edit waits for the language server's diagnostics
    pub lsp_wait_ms: u64,
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
    set_raw(key, &quote(value))
}

/// Set `key` (or `table.key`) to a TOML literal (`"x"`, `true`, `3`, `["a"]`); an empty
/// literal removes the line. The rest of the file stays as it is.
pub fn set_raw(key: &str, literal: &str) -> Result<PathBuf, String> {
    if !literal.is_empty() && format!("x = {literal}").parse::<toml::Table>().is_err() {
        return Err(format!("not a TOML value: {literal}"));
    }
    let path = config_file();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let (table, k) = key.rsplit_once('.').map(|(t, k)| (Some(t), k)).unwrap_or((None, key));
    let out = set_in_table(&text, table, k, literal);
    // never write a file qh itself can't read back
    out.parse::<toml::Table>().map_err(|e| format!("config.toml would not parse: {e}"))?;
    // private: it can hold a relay's secret
    crate::platform::write_private(&path, out.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

pub fn config_file() -> PathBuf {
    std::env::var("QH_CONFIG").map(PathBuf::from).unwrap_or_else(|_| crate::platform::config_base().join("config.toml"))
}

fn quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
fn set_in(text: &str, key: &str, value: &str) -> String {
    set_in_table(text, None, key, &quote(value))
}

/// The edit itself: in `[table]` (or the top level), replace `key`'s line or add it; an empty
/// literal drops it. A missing table is added at the end.
fn set_in_table(text: &str, table: Option<&str>, key: &str, literal: &str) -> String {
    let line = (!literal.is_empty()).then(|| format!("{key} = {literal}"));
    let mut out: Vec<String> = vec![];
    let mut done = line.is_none();
    let mut here = table.is_none(); // inside the section we edit
    let mut seen = table.is_none();
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').split(']').next().unwrap_or("").trim();
            if here && !done {
                // the section ends: add before the blank lines that close it
                let at = out.iter().rposition(|x| !x.trim().is_empty()).map_or(0, |i| i + 1);
                out.insert(at, line.clone().unwrap());
                done = true;
            }
            here = table == Some(name) && !t.starts_with("[[");
            seen |= here;
        }
        if here && !t.starts_with('[') && t.split('=').next().is_some_and(|k| k.trim() == key) {
            if !done {
                out.push(line.clone().unwrap());
                done = true;
            }
            continue;
        }
        out.push(l.to_string());
    }
    if !done {
        let l = line.unwrap();
        if !seen {
            if out.last().is_some_and(|x| !x.trim().is_empty()) { out.push(String::new()) }
            out.push(format!("[{}]", table.unwrap()));
        }
        out.push(l);
    }
    out.join("\n") + "\n"
}

/// What the Mac app's Settings window shows: the settings as qh reads them.
pub fn dump() -> serde_json::Value {
    let c = load();
    serde_json::json!({
        "path": config_file(), "model": c.model, "fallback_model": c.fallback_model, "ask_model": c.ask_model,
        "fetch_model": c.fetch_model, "cost_warn": c.cost_warn, "budget_session": c.budget_session, "budget_day": c.budget_day, "cloud_ctx": c.cloud_ctx, "ctx": c.ctx,
        "compact_at": c.compact_at, "mode": c.mode, "notify": c.notify, "update_check": c.update_check,
        "relay": c.relay, "team_share_files": c.team_share_files, "review": c.review, "review_wait": c.review_wait, "sandbox": c.sandbox,
        "sandbox_write": c.sandbox_write, "sandbox_hosts": c.sandbox_hosts, "sandbox_network": c.sandbox_network,
        "lsp": c.lsp, "lsp_wait_ms": c.lsp_wait_ms, "openrouter_key": !c.openrouter_key.is_empty(),
        "providers": c.providers.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
        "hooks": c.hooks.len(), "mcp": c.mcp.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
    })
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
        budget_session: 0.0,
        budget_day: 0.0,
        profile: "coding".into(),
        keep_alive: "30m".into(),
        fallback_model: vec![],
        ask_model: String::new(),
        fetch_model: String::new(),
        openrouter_key: String::new(),
        allow: vec![],
        webhooks: vec![],
        hooks: vec![],
        mcp: vec![],
        browser_headless: false,
        // auto by default where commands run in the OS sandbox: on Windows once its sandbox users exist
        mode: if cfg!(windows) && !crate::sandbox_windows::ready() { "ask" } else { "auto" }.into(),
        browser_allow_hosts: vec![],
        providers: vec![],
        default_provider: String::new(),
        ctx_set: false,
        mouse: true,
        relay: "default".into(),
        verify: String::new(),
        team_share_files: true,
        review: true,
        review_wait: 60,
        update_check: true,
        memory: true,
        notify: "auto".into(),
        lsp: "auto".into(),
        lsp_wait_ms: 3000,
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
    let path = config_file();
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
        for (k, slot) in [("budget_session", &mut c.budget_session), ("budget_day", &mut c.budget_day)] {
            if let Some(v) = t.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64))) { *slot = v.max(0.0) }
        }
        if let Some(v) = s("profile") { c.profile = v }
        if let Some(v) = s("keep_alive") { c.keep_alive = v }
        if let Some(v) = fallbacks(t.get("fallback_model")) { c.fallback_model = v }
        routing(&mut c, &t);
        if let Some(v) = s("openrouter_key") { c.openrouter_key = v }
        if let Some(a) = t.get("allow").and_then(|v| v.as_array()) {
            c.allow = a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(v) = s("mode") { c.mode = v }
        if let Some(v) = s("relay") { c.relay = v }
        if let Some(v) = s("verify") { c.verify = v }
        match t.get("team_share_files") {
            Some(toml::Value::Boolean(b)) => c.team_share_files = *b,
            Some(toml::Value::String(v)) => c.team_share_files = !matches!(v.to_lowercase().as_str(), "off" | "false" | "no"),
            _ => {}
        }
        match t.get("review") {
            Some(toml::Value::Boolean(b)) => c.review = *b,
            Some(toml::Value::String(v)) => c.review = !matches!(v.to_lowercase().as_str(), "off" | "false" | "no"),
            _ => {}
        }
        if let Some(v) = t.get("review_wait").and_then(|v| v.as_integer()) { c.review_wait = v.clamp(0, 3600) as u64 }
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
        // sandbox off → nothing contains auto mode, so fall back to asking unless mode was set
        if !c.sandbox && s("mode").is_none() { c.mode = "ask".into() }
        if let Some(p) = t.get("providers").and_then(|v| v.as_table()) {
            c.providers = p.iter().filter_map(|(k, v)| v.as_table().map(|t| (k.clone(), t.clone()))).collect();
        }
        match t.get("notify") {
            Some(toml::Value::String(v)) => c.notify = v.clone(),
            Some(toml::Value::Boolean(b)) => c.notify = if *b { "auto" } else { "off" }.into(),
            _ => {}
        }
        match t.get("lsp") {
            Some(toml::Value::String(v)) => c.lsp = if v == "off" { "off" } else { "auto" }.into(),
            Some(toml::Value::Boolean(b)) => c.lsp = if *b { "auto" } else { "off" }.into(),
            _ => {}
        }
        if let Some(v) = t.get("lsp_wait_ms").and_then(|v| v.as_integer()) { c.lsp_wait_ms = v.clamp(0, 30_000) as u64 }
        if let Some(v) = t.get("mouse").and_then(|v| v.as_bool()) { c.mouse = v }
        if let Some(v) = t.get("memory").and_then(|v| v.as_bool()) { c.memory = v }
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

/// `fallback_model = "a"` or `["a", "b"]`; blanks dropped.
fn fallbacks(v: Option<&toml::Value>) -> Option<Vec<String>> {
    let v = match v? {
        toml::Value::String(s) => vec![s.clone()],
        toml::Value::Array(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => return None,
    };
    Some(v.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// `[routing]`: which other models qh may use, all off unless set.
fn routing(c: &mut Config, t: &toml::Table) {
    let Some(r) = t.get("routing").and_then(|v| v.as_table()) else { return };
    let s = |k: &str| r.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string()).unwrap_or_default();
    (c.ask_model, c.fetch_model) = (s("ask_model"), s("fetch_model"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn fallback_model_takes_a_string_or_a_list() {
        let f = |t: &str| super::fallbacks(t.parse::<toml::Table>().unwrap().get("fallback_model"));
        assert_eq!(f("fallback_model = \"a\""), Some(vec!["a".to_string()]));
        assert_eq!(f("fallback_model = [\"a\", \" \", \"or:b\"]"), Some(vec!["a".to_string(), "or:b".to_string()]));
        assert_eq!(f("fallback_model = \"\""), Some(vec![]));
        assert_eq!(f("model = \"x\""), None);
    }

    #[test]
    fn routing_is_off_unless_set() {
        let mut c = super::load_defaults();
        super::routing(&mut c, &"model = \"x\"".parse().unwrap());
        assert!(c.ask_model.is_empty() && c.fetch_model.is_empty());
        super::routing(&mut c, &"[routing]\nask_model = \" openai:gpt-5 \"\nfetch_model = \"ollama/qwen3:4b\"".parse().unwrap());
        assert_eq!((c.ask_model.as_str(), c.fetch_model.as_str()), ("openai:gpt-5", "ollama/qwen3:4b"));
        assert_eq!(crate::route::model_id(&c.fetch_model), (String::new(), "qwen3:4b".into()));
    }

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

    #[test]
    fn set_raw_edits_tables() {
        let t = "model = \"a\"\n\n[sandbox]\nmode = \"on\"\n\n[routing]\nask_model = \"x\"\n";
        let out = super::set_in_table(t, Some("sandbox"), "network", "\"open\"");
        assert_eq!(out, "model = \"a\"\n\n[sandbox]\nmode = \"on\"\nnetwork = \"open\"\n\n[routing]\nask_model = \"x\"\n");
        let out = super::set_in_table(t, Some("sandbox"), "mode", "\"off\"");
        assert!(out.contains("[sandbox]\nmode = \"off\"\n"));
        let out = super::set_in_table(t, Some("routing"), "ask_model", "");
        assert_eq!(out.parse::<toml::Table>().unwrap()["routing"].as_table().unwrap().len(), 0);
        let out = super::set_in_table("model = \"a\"\n", Some("sandbox"), "writable", "[\"/tmp\"]");
        assert_eq!(out, "model = \"a\"\n\n[sandbox]\nwritable = [\"/tmp\"]\n");
        let out = super::set_in_table(t, None, "notify", "\"off\"");
        assert!(out.starts_with("model = \"a\"\n\nnotify = \"off\"\n[sandbox]") || out.parse::<toml::Table>().unwrap()["notify"].as_str() == Some("off"));
        assert_eq!(super::set_in_table(t, None, "model", ""), t.replacen("model = \"a\"\n", "", 1));
    }
}
