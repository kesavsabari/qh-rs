//! Size the context window to the GPU: free VRAM after the model's weights and whatever
//! else is using the card, divided by the KV cache one token costs. Only used when the
//! config doesn't set `ctx` itself. Tiers name the result so features can scale with it.
use crate::backend::http;
use serde_json::{json, Value};
use std::time::Duration;

pub struct Detect {
    pub ctx: usize,
    pub why: String,
}

pub fn tier(ctx: usize) -> &'static str {
    match ctx {
        0..16384 => "S",
        16384..65536 => "M",
        65536..131072 => "L",
        _ => "XL",
    }
}

/// What a context window can afford. Everything that used to be a constant scales from here,
/// so a 4k model isn't handed 6k-character tool results and a 128k one isn't starved.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub out_cap: usize,    // chars of one tool result
    pub keep_recent: usize, // messages compaction never touches
    pub map_chars: usize,  // repo map in the system prompt (0 = none)
    pub notes_chars: usize, // NOTES.md in the system prompt
    pub browser: bool,
}

impl Limits {
    pub fn for_ctx(ctx: usize) -> Limits {
        match tier(ctx) {
            "S" => Limits { out_cap: 2000, keep_recent: 4, map_chars: 0, notes_chars: 600, browser: false },
            "M" => Limits { out_cap: 6000, keep_recent: 6, map_chars: 1600, notes_chars: 2000, browser: true },
            "L" => Limits { out_cap: 12000, keep_recent: 8, map_chars: 3200, notes_chars: 4000, browser: true },
            _ => Limits { out_cap: 24000, keep_recent: 12, map_chars: 6000, notes_chars: 8000, browser: true },
        }
    }
}

/// What the model can do, from Ollama's /api/show. Unknown means "assume yes": a wrong
/// guess then fails loudly at the server instead of silently crippling a capable model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Caps {
    pub tools: bool,
    pub vision: bool,
    pub thinking: bool,
}

pub const ALL_CAPS: Caps = Caps { tools: true, vision: true, thinking: true };

pub fn caps(host: &str, model: &str) -> Caps {
    static CACHE: std::sync::Mutex<Vec<(String, Caps)>> = std::sync::Mutex::new(Vec::new());
    let key = format!("{host}|{model}");
    if let Some((_, c)) = CACHE.lock().unwrap().iter().find(|(k, _)| *k == key) {
        return *c;
    }
    let shown = http().post(&format!("{}/api/show", host.trim_end_matches('/'))).timeout(Duration::from_secs(6))
        .send_string(&json!({"model": model}).to_string()).ok()
        .and_then(|r| r.into_string().ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let force_text = std::env::var_os("QH_TEXT_TOOLS").is_some(); // test/escape hatch: skip native tool calling
    let c = match shown.as_ref().and_then(|v| v["capabilities"].as_array()) {
        Some(list) => {
            let has = |n: &str| list.iter().any(|x| x == n);
            Caps { tools: has("tools") && !force_text, vision: has("vision"), thinking: has("thinking") }
        }
        None => return ALL_CAPS, // not cached, so a server that was still starting gets asked again
    };
    CACHE.lock().unwrap().push((key, c));
    c
}

fn get(url: &str) -> Option<Value> {
    serde_json::from_str(&http().get(url).timeout(Duration::from_secs(4)).call().ok()?.into_string().ok()?).ok()
}

/// (total, used) MiB on the first NVIDIA GPU.
fn vram() -> Option<(f64, f64)> {
    let o = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total,memory.used", "--format=csv,noheader,nounits"]).output().ok()?;
    let line = String::from_utf8_lossy(&o.stdout).lines().next()?.to_string();
    let mut it = line.split(',').map(|x| x.trim().parse::<f64>().ok());
    Some((it.next()??, it.next()??))
}

pub fn detect(host: &str, model: &str) -> Option<Detect> {
    let (total, used) = vram()?;
    let host = host.trim_end_matches('/');
    let show: Value = serde_json::from_str(&http().post(&format!("{host}/api/show")).timeout(Duration::from_secs(6))
        .send_string(&json!({"model": model}).to_string()).ok()?.into_string().ok()?).ok()?;
    let mi = show["model_info"].as_object()?;
    let f = |suffix: &str| mi.iter().find(|(k, _)| k.ends_with(suffix)).and_then(|(_, v)| v.as_f64());
    let layers = f(".block_count")?;
    // hybrid models (qwen3.5) only keep a KV cache on every Nth layer
    let attn_layers = f(".full_attention_interval").map_or(layers, |n| (layers / n.max(1.0)).ceil());
    let kv_heads = f(".attention.head_count_kv").or_else(|| f(".attention.head_count"))?;
    let head_dim = f(".attention.key_length")
        .or_else(|| Some(f(".embedding_length")? / f(".attention.head_count")?))?;
    let per_token = 2.0 * attn_layers * kv_heads * head_dim * 2.0 / 1_048_576.0; // MiB, f16 K and V
    let max_ctx = f(".context_length").unwrap_or(131072.0);
    // weights: from the loaded model if it is in memory (minus its KV cache), else the file size
    let loaded = get(&format!("{host}/api/ps")).and_then(|v| {
        v["models"].as_array()?.iter().find(|m| m["name"] == model || m["model"] == model).cloned()
    });
    let (weights, others) = match &loaded {
        Some(m) => {
            let vram = m["size_vram"].as_f64()? / 1_048_576.0;
            let kv = m["context_length"].as_f64().unwrap_or(0.0) * per_token;
            (vram - kv, (used - vram).max(0.0))
        }
        None => {
            let tags = get(&format!("{host}/api/tags"))?;
            let size = tags["models"].as_array()?.iter().find(|m| m["name"] == model)?["size"].as_f64()? / 1_048_576.0;
            (size, used)
        }
    };
    let free = total - others - weights - 512.0; // margin for compute buffers
    if free <= 0.0 || per_token <= 0.0 {
        return Some(Detect { ctx: 4096, why: format!("{model} barely fits in {total:.0} MiB; using 4k") });
    }
    let fits = (free / per_token) as usize;
    let mut ctx = 4096;
    while ctx * 2 <= fits && ctx * 2 <= max_ctx as usize && ctx * 2 <= 131072 {
        ctx *= 2;
    }
    Some(Detect {
        ctx,
        why: format!("{total:.0} MiB GPU, {others:.0} used elsewhere, {weights:.0} for weights, \
{:.0} KiB per token of context → {}k (tier {})", per_token * 1024.0, ctx / 1024, tier(ctx)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_grow_with_the_window() {
        let l: Vec<Limits> = [4096, 32768, 100_000, 200_000].iter().map(|&c| Limits::for_ctx(c)).collect();
        assert!(l.windows(2).all(|w| w[0].out_cap < w[1].out_cap && w[0].keep_recent <= w[1].keep_recent));
        assert_eq!(l[0].map_chars, 0);
        assert!(!l[0].browser && l[1].browser);
        assert_eq!(tier(32768), "M");
    }
}
