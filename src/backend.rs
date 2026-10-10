//! Streaming chat backends. Messages stay in Ollama's native shape; the
//! OpenAI-compatible and Anthropic adapters convert them. `on` sees each chunk
//! (thinking?, text) and returns false to abort the stream.
use crate::providers::{Kind, Provider};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::sync::OnceLock;
use std::time::Duration;

pub struct Reply {
    pub msg: Value,
    pub prompt: u64,
    pub total: u64,
}

pub enum Error {
    Connect(String),
    Cancelled,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::Connect(e) => write!(f, "connect failed: {e}"),
            Error::Cancelled => write!(f, "interrupted"),
            Error::Other(e) => write!(f, "{e}"),
        }
    }
}

pub fn http() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build())
}

fn post(url: &str, body: &Value, auth: Option<&str>) -> Result<ureq::Response, Error> {
    let mut req = http().post(url).set("Content-Type", "application/json");
    if let Some(k) = auth {
        req = req.set("Authorization", &format!("Bearer {k}"));
    }
    req.send_string(&body.to_string()).map_err(|e| match e {
        ureq::Error::Status(code, r) => Error::Other(format!("HTTP {code}: {}", r.into_string().unwrap_or_default())),
        ureq::Error::Transport(t) => match t.kind() {
            ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Dns => Error::Connect(t.to_string()),
            _ => Error::Other(t.to_string()),
        },
    })
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

#[allow(clippy::too_many_arguments)]
pub fn ollama(host: &str, key: Option<&str>, model: &str, messages: &[Value], tools: &[Value], think: bool, ctx: usize,
              keep_alive: &str, on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    let mut body = json!({"model": model, "messages": messages, "stream": true, "think": think,
                          "keep_alive": keep_alive, "options": {"num_ctx": ctx}});
    if !tools.is_empty() {
        body["tools"] = Value::from(tools.to_vec());
    }
    let r = post(&format!("{}/api/chat", host.trim_end_matches('/')), &body, key)?;
    let (mut content, mut calls, mut prompt, mut total) = (String::new(), Vec::new(), 0, 0);
    for line in BufReader::new(r.into_reader()).lines() {
        let line = line.map_err(|e| Error::Other(e.to_string()))?;
        if line.is_empty() {
            continue;
        }
        let chunk: Value = serde_json::from_str(&line).map_err(|e| Error::Other(e.to_string()))?;
        if let Some(e) = chunk.get("error").and_then(|e| e.as_str()) {
            return Err(Error::Other(e.to_string()));
        }
        let m = &chunk["message"];
        let t = s(m, "thinking");
        if !t.is_empty() && !on(true, t) {
            return Err(Error::Cancelled);
        }
        let c = s(m, "content");
        if !c.is_empty() {
            content.push_str(c);
            if !on(false, c) {
                return Err(Error::Cancelled);
            }
        }
        if let Some(tc) = m.get("tool_calls").and_then(|v| v.as_array()) {
            calls.extend(tc.iter().cloned());
        }
        if chunk["done"].as_bool() == Some(true) {
            prompt = chunk["prompt_eval_count"].as_u64().unwrap_or(0);
            total = prompt + chunk["eval_count"].as_u64().unwrap_or(0);
        }
    }
    let mut msg = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        msg["tool_calls"] = Value::from(calls);
    }
    Ok(Reply { msg, prompt, total })
}

fn to_openai(messages: &[Value]) -> Vec<Value> {
    let (mut out, mut ids) = (Vec::new(), std::collections::VecDeque::new());
    for m in messages {
        let role = s(m, "role");
        if let (true, Some(tc)) = (role == "assistant", m.get("tool_calls").and_then(|v| v.as_array())) {
            let calls: Vec<Value> = tc.iter().enumerate().map(|(i, c)| {
                let id = format!("call_{}_{i}", out.len());
                ids.push_back(id.clone());
                let args = &c["function"]["arguments"];
                let args = match args {
                    Value::String(a) => a.clone(),
                    Value::Null => "{}".into(),
                    a => a.to_string(),
                };
                json!({"id": id, "type": "function",
                       "function": {"name": c["function"]["name"], "arguments": args}})
            }).collect();
            let content = Some(s(m, "content")).filter(|c| !c.is_empty());
            out.push(json!({"role": "assistant", "content": content, "tool_calls": calls}));
        } else if role == "tool" {
            let id = ids.pop_front().unwrap_or_else(|| "call_x".into());
            out.push(json!({"role": "tool", "tool_call_id": id, "content": m["content"]}));
            // tool messages can't carry images; a screenshot follows as a user message
            if let Some(imgs) = m["images"].as_array().filter(|a| !a.is_empty()) {
                out.push(json!({"role": "user", "content": image_parts("(screenshot from the tool above)", imgs)}));
            }
        } else if let Some(imgs) = m["images"].as_array().filter(|a| !a.is_empty()) {
            out.push(json!({"role": role, "content": image_parts(s(m, "content"), imgs)}));
        } else {
            out.push(json!({"role": role, "content": m["content"]}));
        }
    }
    out
}

fn image_parts(text: &str, imgs: &[Value]) -> Value {
    let mut parts = vec![json!({"type": "text", "text": text})];
    for i in imgs.iter().filter_map(|i| i.as_str()) {
        parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{i}")}}));
    }
    Value::from(parts)
}

/// Any OpenAI-compatible /chat/completions server: OpenRouter, OpenAI, Groq, DeepSeek,
/// LM Studio, llama.cpp, vLLM…
pub fn openai(base: &str, key: &str, model: &str, messages: &[Value], tools: &[Value], think: bool,
              on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    let mut body = json!({"model": model, "messages": to_openai(messages), "stream": true,
                          "stream_options": {"include_usage": true}});
    if !tools.is_empty() {
        body["tools"] = Value::from(tools.to_vec());
    }
    if base.contains("openrouter.ai") && !think {
        body["reasoning"] = json!({"enabled": false});
    }
    let key = Some(key).filter(|k| !k.is_empty());
    let r = post(&format!("{}/chat/completions", base.trim_end_matches('/')), &body, key)?;
    let (mut content, mut prompt, mut total) = (String::new(), 0, 0);
    let mut calls: BTreeMap<u64, (String, String)> = BTreeMap::new();
    for line in BufReader::new(r.into_reader()).lines() {
        let line = line.map_err(|e| Error::Other(e.to_string()))?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if data == "[DONE]" {
            continue;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
            total = u["total_tokens"].as_u64().unwrap_or(0);
        }
        for ch in chunk["choices"].as_array().into_iter().flatten() {
            let d = &ch["delta"];
            let t = if s(d, "reasoning").is_empty() { s(d, "reasoning_content") } else { s(d, "reasoning") };
            if !t.is_empty() && !on(true, t) {
                return Err(Error::Cancelled);
            }
            let c = s(d, "content");
            if !c.is_empty() {
                content.push_str(c);
                if !on(false, c) {
                    return Err(Error::Cancelled);
                }
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let slot = calls.entry(tc["index"].as_u64().unwrap_or(0)).or_default();
                slot.0.push_str(s(&tc["function"], "name"));
                slot.1.push_str(s(&tc["function"], "arguments"));
            }
        }
    }
    let mut msg = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        msg["tool_calls"] = calls.into_values()
            .map(|(name, arguments)| json!({"function": {"name": name, "arguments": arguments}}))
            .collect();
    }
    Ok(Reply { msg, prompt, total })
}

/// Ollama-shaped history -> Anthropic Messages: system apart, tool calls as tool_use
/// blocks, results as tool_result blocks in the next user turn.
fn to_anthropic(messages: &[Value]) -> (String, Vec<Value>) {
    let (mut system, mut out): (String, Vec<Value>) = (String::new(), vec![]);
    let mut ids: std::collections::VecDeque<String> = Default::default();
    let push = |out: &mut Vec<Value>, role: &str, blocks: Vec<Value>| {
        if let Some(last) = out.last_mut().filter(|l| l["role"] == role) {
            if let Some(c) = last["content"].as_array_mut() {
                c.extend(blocks);
                return;
            }
        }
        out.push(json!({"role": role, "content": blocks}));
    };
    let images = |m: &Value| -> Vec<Value> {
        m["images"].as_array().into_iter().flatten().filter_map(|i| i.as_str()).map(|i| json!({
            "type": "image", "source": {"type": "base64", "media_type": "image/png", "data": i}})).collect()
    };
    for (n, m) in messages.iter().enumerate() {
        match s(m, "role") {
            "system" => system.push_str(s(m, "content")),
            "assistant" => {
                let mut blocks = vec![];
                if !s(m, "content").is_empty() {
                    blocks.push(json!({"type": "text", "text": s(m, "content")}));
                }
                for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
                    let id = format!("toolu_{n}_{i}");
                    ids.push_back(id.clone());
                    let input = match &c["function"]["arguments"] {
                        Value::String(a) => serde_json::from_str(a).unwrap_or(json!({})),
                        Value::Object(_) => c["function"]["arguments"].clone(),
                        _ => json!({}),
                    };
                    blocks.push(json!({"type": "tool_use", "id": id, "name": c["function"]["name"], "input": input}));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": "(no reply)"}));
                }
                push(&mut out, "assistant", blocks);
            }
            "tool" => {
                let id = ids.pop_front().unwrap_or_else(|| "toolu_x".into());
                let mut content = vec![json!({"type": "text", "text": s(m, "content")})];
                content.extend(images(m));
                push(&mut out, "user", vec![json!({"type": "tool_result", "tool_use_id": id, "content": content})]);
            }
            _ => {
                let mut blocks = vec![json!({"type": "text", "text": s(m, "content")})];
                blocks.extend(images(m));
                push(&mut out, "user", blocks);
            }
        }
    }
    (system, out)
}

/// Anthropic Messages API with an API key (console.anthropic.com).
pub fn anthropic(base: &str, key: &str, model: &str, messages: &[Value], tools: &[Value], think: bool,
                 on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    let (system, msgs) = to_anthropic(messages);
    let mut body = json!({"model": model, "max_tokens": 16000, "messages": msgs, "stream": true});
    if !system.is_empty() {
        body["system"] = system.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.iter().map(|t| json!({"name": t["function"]["name"],
            "description": t["function"]["description"], "input_schema": t["function"]["parameters"]})).collect();
    }
    if think {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": 8000});
    }
    let url = format!("{}/v1/messages", base.trim_end_matches('/'));
    let r = http().post(&url).set("Content-Type", "application/json").set("x-api-key", key)
        .set("anthropic-version", "2023-06-01").send_string(&body.to_string()).map_err(|e| match e {
            ureq::Error::Status(code, r) => Error::Other(format!("HTTP {code}: {}", r.into_string().unwrap_or_default())),
            ureq::Error::Transport(t) => Error::Connect(t.to_string()),
        })?;
    let (mut content, mut prompt, mut out_tokens) = (String::new(), 0u64, 0u64);
    let mut calls: Vec<(String, String)> = vec![]; // (name, json input so far)
    let mut current_tool: Option<usize> = None;
    for line in BufReader::new(r.into_reader()).lines() {
        let line = line.map_err(|e| Error::Other(e.to_string()))?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
        match s(&ev, "type") {
            "message_start" => {
                let u = &ev["message"]["usage"];
                prompt = u["input_tokens"].as_u64().unwrap_or(0) + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let b = &ev["content_block"];
                current_tool = (s(b, "type") == "tool_use").then(|| {
                    calls.push((s(b, "name").to_string(), String::new()));
                    calls.len() - 1
                });
            }
            "content_block_delta" => {
                let d = &ev["delta"];
                match s(d, "type") {
                    "text_delta" => {
                        content.push_str(s(d, "text"));
                        if !on(false, s(d, "text")) {
                            return Err(Error::Cancelled);
                        }
                    }
                    "thinking_delta" if !on(true, s(d, "thinking")) => return Err(Error::Cancelled),
                    "input_json_delta" => {
                        if let Some(i) = current_tool {
                            calls[i].1.push_str(s(d, "partial_json"));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => out_tokens = ev["usage"]["output_tokens"].as_u64().unwrap_or(out_tokens),
            "error" => return Err(Error::Other(ev["error"]["message"].as_str().unwrap_or("error").to_string())),
            _ => {}
        }
    }
    let mut msg = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        msg["tool_calls"] = calls.into_iter().map(|(name, a)| {
            let args: Value = serde_json::from_str(&a).unwrap_or(json!({}));
            json!({"function": {"name": name, "arguments": args}})
        }).collect();
    }
    Ok(Reply { msg, prompt, total: prompt + out_tokens })
}

/// One streamed chat call to whichever provider runs the model.
#[allow(clippy::too_many_arguments)]
pub fn chat(p: &Provider, model: &str, messages: &[Value], tools: &[Value], think: bool, ctx: usize,
            keep_alive: &str, on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    chat_raw(p, model, messages, tools, think, ctx, keep_alive, on).map_err(|e| explain(p, e))
}

/// Turn the errors a newcomer will actually hit into what to do about them.
fn explain(p: &Provider, e: Error) -> Error {
    let n = &p.name;
    match &e {
        Error::Other(m) if !p.local && (m.starts_with("HTTP 401") || m.starts_with("HTTP 403")) => Error::Other(format!(
            "{n} rejected the API key ({}). Fix it with `qh login {n}` (or /provider add {n}), or pick another model with /model.",
            &m[..8])),
        Error::Other(m) if p.local && p.kind == Kind::Ollama && m.starts_with("HTTP 401") => Error::Other(
            "Ollama isn't signed in to ollama.com, which cloud models need: run `ollama signin` (or sign in from the Ollama app).".into()),
        Error::Other(m) if m.starts_with("HTTP 402") => Error::Other(format!(
            "{n} says the account is out of credit (HTTP 402). Top it up, or pick a free/local model with /model.")),
        Error::Connect(m) if p.local && p.kind == Kind::Ollama => Error::Connect(format!(
            "{m}. Ollama isn't answering at {}: start it (`ollama serve` or the Ollama app), or run `qh setup`.", p.base)),
        _ => e,
    }
}

#[allow(clippy::too_many_arguments)]
fn chat_raw(p: &Provider, model: &str, messages: &[Value], tools: &[Value], think: bool, ctx: usize,
            keep_alive: &str, on: &mut dyn FnMut(bool, &str) -> bool) -> Result<Reply, Error> {
    if p.kind == Kind::ChatGPT {
        return crate::chatgpt::chat(model, messages, tools, think, on);
    }
    if !p.local && p.key.is_empty() {
        return Err(Error::Other(format!("no key for {}: run /provider add {} (or qh login {})", p.name, p.name, p.name)));
    }
    match p.kind {
        Kind::Ollama => {
            let key = Some(p.key.as_str()).filter(|k| !k.is_empty());
            let caps = crate::tier::caps(&p.base, model);
            let think = think && caps.thinking; // a model without thinking rejects the flag
            if !tools.is_empty() && !caps.tools {
                let wrapped = crate::textcall::wrap(messages, tools);
                let mut r = ollama(&p.base, key, model, &wrapped, &[], think, ctx, keep_alive, on)?;
                crate::textcall::unwrap(&mut r.msg, tools);
                return Ok(r);
            }
            ollama(&p.base, key, model, messages, tools, think, ctx, keep_alive, on)
        }
        Kind::OpenAI => openai(&p.base, &p.key, model, messages, tools, think, on),
        Kind::Anthropic => anthropic(&p.base, &p.key, model, messages, tools, think, on),
        Kind::ChatGPT => unreachable!("handled above"),
    }
}

/// Worth retrying: the server was briefly unreachable, overloaded or still loading the model.
pub fn transient(e: &Error) -> bool {
    match e {
        Error::Connect(_) => true,
        Error::Other(m) => {
            let m = m.to_lowercase();
            ["http 429", "http 500", "http 502", "http 503", "http 504", "http 529", "overloaded", "loading model",
             "unexpected eof", "connection reset", "timed out"].iter().any(|k| m.contains(k))
        }
        Error::Cancelled => false,
    }
}

fn tag_names(url: &str, secs: u64) -> Vec<String> {
    let r = http().get(url).timeout(Duration::from_secs(secs)).call();
    let Some(v) = r.ok().and_then(|r| r.into_string().ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return vec![];
    };
    v["models"].as_array().map(|a| a.iter().filter_map(|m| m["name"].as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Local models plus the ollama.com cloud catalog, named the way the local daemon accepts them
/// (`gpt-oss:20b` → `gpt-oss:20b-cloud`, `glm-5.2` → `glm-5.2:cloud`). The cloud list is fetched once.
pub fn list_models(host: &str) -> Vec<String> {
    static CLOUD: OnceLock<Vec<String>> = OnceLock::new();
    let mut out = tag_names(&format!("{}/api/tags", host.trim_end_matches('/')), 3);
    let cloud = CLOUD.get_or_init(|| {
        let mut v: Vec<String> = tag_names("https://ollama.com/api/tags", 5)
            .into_iter()
            .map(|n| if n.contains(':') { format!("{n}-cloud") } else { format!("{n}:cloud") })
            .collect();
        v.sort();
        v
    });
    for c in cloud {
        if !out.contains(c) {
            out.push(c.clone());
        }
    }
    out
}

pub fn is_cloud(model: &str) -> bool {
    model.ends_with("cloud")
}

/// A cloud model's real context window from Ollama's /api/show, cached per name. Local models
/// keep the configured window (it is what fits in VRAM). `fetch: false` only reads the cache,
/// so the UI never waits on the network.
pub fn cloud_ctx(host: &str, model: &str, fetch: bool) -> Option<usize> {
    static CACHE: OnceLock<std::sync::Mutex<BTreeMap<String, Option<usize>>>> = OnceLock::new();
    if !is_cloud(model) {
        return None;
    }
    let cache = CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(model) {
        return *v;
    }
    if !fetch {
        return None;
    }
    let url = format!("{}/api/show", host.trim_end_matches('/'));
    let v = http()
        .post(&url)
        .timeout(Duration::from_secs(8))
        .send_string(&json!({ "model": model }).to_string())
        .ok()
        .and_then(|r| r.into_string().ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v["model_info"].as_object()?.iter().find(|(k, _)| k.ends_with(".context_length")).and_then(|(_, n)| n.as_u64())
        })
        .map(|n| n as usize);
    cache.lock().unwrap().insert(model.to_string(), v);
    v
}
