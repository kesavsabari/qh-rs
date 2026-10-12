//! What each model call used and cost. Every backend reports its provider's own usage numbers;
//! this turns them into one shape, prices them, and adds them up per tab and per session.
//! A price comes from OpenRouter's model listing or a short built-in table; an unknown model
//! shows tokens only, never a guessed price. Plans (ChatGPT, Ollama, coding plans) show "plan".
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// One call's tokens. `input` includes `cached`.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Calls {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

/// Running totals for a tab or the whole session.
#[derive(Clone, Copy, Default, Debug)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub cost: f64,
    /// some calls went to a pay-per-token key
    pub paid: bool,
    /// some paid calls had no known price
    pub unpriced: bool,
}

impl Usage {
    pub fn add(&mut self, c: Calls, price: Option<Price>, paid: bool) {
        self.input += c.input;
        self.cached += c.cached;
        self.output += c.output;
        if paid {
            self.paid = true;
            match price {
                Some(p) => self.cost += p.cost(c),
                None => self.unpriced = true,
            }
        }
    }

    /// The `$` part of the status bar: nothing for plans or unknown prices.
    pub fn status(&self) -> String {
        if !self.paid || self.cost <= 0.0 { return String::new() }
        format!(" · {}", dollars(self.cost))
    }

    /// One line for /tokens.
    pub fn line(&self) -> String {
        let cost = if !self.paid { "plan".to_string() }
            else if self.cost == 0.0 && self.unpriced { "price unknown".to_string() }
            else if self.unpriced { format!("{} (some calls unpriced)", dollars(self.cost)) }
            else { dollars(self.cost) };
        format!("input {} (cached {}) · output {} · {cost}", self.input, self.cached, self.output)
    }
}

pub fn dollars(c: f64) -> String {
    if c < 0.01 { format!("${c:.4}") } else { format!("${c:.2}") }
}

/// $ per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
}

impl Price {
    pub fn cost(&self, c: Calls) -> f64 {
        let fresh = c.input.saturating_sub(c.cached);
        (fresh as f64 * self.input + c.cached as f64 * self.cached + c.output as f64 * self.output) / 1e6
    }
}

/// OpenAI-compatible `usage`: prompt_tokens (includes cached), completion_tokens,
/// prompt_tokens_details.cached_tokens (OpenAI, OpenRouter) or prompt_cache_hit_tokens (DeepSeek).
pub fn openai(u: &Value) -> Calls {
    let input = u["prompt_tokens"].as_u64().unwrap_or(0);
    let output = u["completion_tokens"].as_u64()
        .unwrap_or_else(|| u["total_tokens"].as_u64().unwrap_or(0).saturating_sub(input));
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64()
        .or_else(|| u["prompt_cache_hit_tokens"].as_u64()).unwrap_or(0);
    Calls { input, cached: cached.min(input), output }
}

/// Anthropic `message_start` usage: input_tokens excludes the cache reads and writes.
pub fn anthropic(u: &Value) -> Calls {
    let read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    let input = u["input_tokens"].as_u64().unwrap_or(0) + read + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    Calls { input, cached: read, output: u["output_tokens"].as_u64().unwrap_or(0) }
}

/// Responses API `response.completed` usage (ChatGPT plan).
pub fn responses(u: &Value) -> Calls {
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let output = u["output_tokens"].as_u64()
        .unwrap_or_else(|| u["total_tokens"].as_u64().unwrap_or(0).saturating_sub(input));
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Calls { input, cached: cached.min(input), output }
}

/// Ollama's final chunk.
pub fn ollama(chunk: &Value) -> Calls {
    Calls { input: chunk["prompt_eval_count"].as_u64().unwrap_or(0), cached: 0, output: chunk["eval_count"].as_u64().unwrap_or(0) }
}

/// Built-in prices for the providers with no price list, $/M (input, cached input, output),
/// from their pricing pages as of 2026-10. Matched exactly (after a date suffix), never by guess.
const PRICES: &[(&str, &str, f64, f64, f64)] = &[
    ("openai", "gpt-5", 1.25, 0.125, 10.0),
    ("openai", "gpt-5-mini", 0.25, 0.025, 2.0),
    ("openai", "gpt-5-nano", 0.05, 0.005, 0.4),
    ("openai", "gpt-4.1", 2.0, 0.5, 8.0),
    ("openai", "gpt-4.1-mini", 0.4, 0.1, 1.6),
    ("openai", "gpt-4.1-nano", 0.1, 0.025, 0.4),
    ("openai", "gpt-4o", 2.5, 1.25, 10.0),
    ("openai", "gpt-4o-mini", 0.15, 0.075, 0.6),
    ("anthropic", "claude-opus-4", 15.0, 1.5, 75.0),
    ("anthropic", "claude-opus-4-1", 15.0, 1.5, 75.0),
    ("anthropic", "claude-sonnet-4", 3.0, 0.3, 15.0),
    ("anthropic", "claude-sonnet-4-5", 3.0, 0.3, 15.0),
    ("anthropic", "claude-haiku-4-5", 1.0, 0.1, 5.0),
];

/// "claude-sonnet-4-20250514" -> "claude-sonnet-4", "gpt-4o-2024-08-06" -> "gpt-4o".
fn undated(model: &str) -> &str {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((head, tail)) = model.rsplit_once('-') {
        if tail.len() == 8 && digits(tail) { return head }
    }
    if model.len() > 11 {
        let (head, tail) = model.split_at(model.len() - 11);
        let p: Vec<&str> = tail[1..].split('-').collect();
        if tail.starts_with('-') && p.len() == 3 && p[0].len() == 4 && p.iter().all(|x| digits(x)) { return head }
    }
    model
}

pub fn builtin(provider: &str, model: &str) -> Option<Price> {
    let m = undated(model);
    PRICES.iter().find(|(p, id, ..)| *p == provider && *id == m)
        .map(|&(_, _, input, cached, output)| Price { input, cached, output })
}

/// Pay-per-token key (true) or a plan / local model (false).
pub fn paid(provider: &str, model: &str) -> bool {
    use crate::providers::Kind;
    let Some(p) = crate::providers::resolve(provider) else { return false };
    let _ = model;
    !(p.local || matches!(p.kind, Kind::ChatGPT | Kind::Ollama)
      || crate::providers::is_plan(&p.name))
}

/// The model's price: OpenRouter's listing (already fetched for the context window), else the table.
pub fn price(provider: &str, model: &str) -> Option<Price> {
    let name = crate::providers::resolve(provider).map(|p| p.name).unwrap_or_default();
    if let Some(i) = crate::providers::info(provider, model).filter(|i| i.price_in > 0.0 || i.price_out > 0.0) {
        let cached = if i.price_cached > 0.0 { i.price_cached } else { i.price_in };
        return Some(Price { input: i.price_in, cached, output: i.price_out });
    }
    builtin(&name, model)
}

/// Says once when a paid session first passes `limit` dollars (0 = never).
#[derive(Default)]
pub struct Warn(AtomicBool);

impl Warn {
    pub fn check(&self, spent: f64, limit: f64) -> Option<String> {
        if limit <= 0.0 || spent < limit || self.0.swap(true, Ordering::Relaxed) { return None }
        Some(format!("this session has spent {} on API keys (cost_warn = {limit:.2}); /tokens shows the breakdown",
                     dollars(spent)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_parses_each_provider() {
        let o = json!({"prompt_tokens": 1000, "completion_tokens": 50, "total_tokens": 1050,
                       "prompt_tokens_details": {"cached_tokens": 800}});
        assert_eq!(openai(&o), Calls { input: 1000, cached: 800, output: 50 });
        let d = json!({"prompt_tokens": 10, "total_tokens": 15, "prompt_cache_hit_tokens": 4});
        assert_eq!(openai(&d), Calls { input: 10, cached: 4, output: 5 });
        let a = json!({"input_tokens": 20, "cache_read_input_tokens": 900, "cache_creation_input_tokens": 80, "output_tokens": 1});
        assert_eq!(anthropic(&a), Calls { input: 1000, cached: 900, output: 1 });
        let r = json!({"input_tokens": 500, "input_tokens_details": {"cached_tokens": 300}, "output_tokens": 40, "total_tokens": 540});
        assert_eq!(responses(&r), Calls { input: 500, cached: 300, output: 40 });
        let l = json!({"done": true, "prompt_eval_count": 70, "eval_count": 9});
        assert_eq!(ollama(&l), Calls { input: 70, cached: 0, output: 9 });
        assert_eq!(openai(&json!({})), Calls::default());
    }

    #[test]
    fn cost_math() {
        let p = Price { input: 3.0, cached: 0.3, output: 15.0 };
        let c = Calls { input: 1_000_000, cached: 500_000, output: 100_000 };
        assert!((p.cost(c) - (1.5 + 0.15 + 1.5)).abs() < 1e-9);
        let mut u = Usage::default();
        u.add(c, Some(p), true);
        u.add(c, None, true);
        assert!((u.cost - 3.15).abs() < 1e-9 && u.unpriced && u.input == 2_000_000);
        assert_eq!(u.status(), " · $3.15");
        let mut plan = Usage::default();
        plan.add(c, Some(p), false);
        assert_eq!((plan.cost, plan.status().as_str()), (0.0, ""));
        assert!(plan.line().ends_with("plan"));
    }

    #[test]
    fn builtin_prices_match_exactly() {
        assert_eq!(builtin("anthropic", "claude-sonnet-4-20250514").map(|p| p.input), Some(3.0));
        assert_eq!(builtin("openai", "gpt-4o-2024-08-06").map(|p| p.output), Some(10.0));
        assert_eq!(builtin("openai", "gpt-4o-mini").map(|p| p.input), Some(0.15));
        assert!(builtin("openai", "gpt-5.9").is_none());
        assert!(builtin("groq", "gpt-4o").is_none());
    }

    #[test]
    fn warns_once() {
        let w = Warn::default();
        assert!(w.check(1.0, 2.0).is_none());
        assert!(w.check(2.5, 0.0).is_none());
        assert!(w.check(2.01, 2.0).is_some());
        assert!(w.check(5.0, 2.0).is_none());
    }
}
