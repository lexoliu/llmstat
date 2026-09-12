//! LiteLLM's community pricebook as the base pricing source.
//!
//! `model_prices_and_context_window.json` is fetched once and cached under
//! `~/.cache/llmstat/` for `TTL`; a stale cache is used when the fetch fails
//! (offline), and an absent cache+failed fetch just leaves models unpriced.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{Price, normalize};

const URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Deserialize)]
struct LiteEntry {
    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
}

/// LiteLLM prices keyed by normalized model key.
pub struct LiteBook {
    map: HashMap<String, LiteEntry>,
    /// e.g. "litellm (cache)" or "unavailable" — shown in notes.
    pub note: String,
}

fn cache_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".cache/llmstat/litellm-prices.json")
}

fn fresh(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < TTL)
}

fn fetch() -> Option<String> {
    let resp = ureq::get(URL).call().ok()?;
    resp.into_body().read_to_string().ok()
}

fn parse(text: &str) -> HashMap<String, LiteEntry> {
    let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    raw.into_iter()
        .filter(|(k, _)| k != "sample_spec" && !k.starts_with("spec."))
        .filter_map(|(k, v)| serde_json::from_value::<LiteEntry>(v).ok().map(|e| (k, e)))
        .filter(|(_, e)| e.input_cost_per_token.is_some() || e.output_cost_per_token.is_some())
        .map(|(k, e)| (normalize(&k), e))
        .collect()
}

impl LiteBook {
    /// Load the pricebook: fresh cache → fetch+cache → stale cache → empty.
    pub fn load(refresh: bool) -> Self {
        let path = cache_path();
        let cached = std::fs::read_to_string(&path).ok();
        let cache_fresh = cached.is_some() && fresh(&path);

        if (!cache_fresh || refresh)
            && let Some(text) = fetch()
        {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &text);
            return Self {
                map: parse(&text),
                note: "litellm (fetched)".into(),
            };
        }
        match cached {
            Some(text) => Self {
                map: parse(&text),
                note: if cache_fresh {
                    "litellm (cache)".into()
                } else {
                    "litellm (stale cache — offline?)".into()
                },
            },
            None => Self {
                map: HashMap::new(),
                note: "litellm prices unavailable (offline, no cache)".into(),
            },
        }
    }

    /// Look up a normalized model name, returning the litellm key matched and
    /// the per-1M-token price. Exact match first, then the longest key that
    /// prefixes the model name or vice versa (dated aliases etc.).
    pub fn lookup(&self, normalized_name: &str) -> Option<(String, Price)> {
        let to_price = |e: &LiteEntry| Price {
            input: e.input_cost_per_token.unwrap_or(0.0) * 1e6,
            // no cache-read price listed -> cached bills at input rate
            cached: e
                .cache_read_input_token_cost
                .or(e.input_cost_per_token)
                .unwrap_or(0.0)
                * 1e6,
            output: e.output_cost_per_token.unwrap_or(0.0) * 1e6,
        };
        if let Some(e) = self.map.get(normalized_name) {
            return Some((normalized_name.to_string(), to_price(e)));
        }
        // prefix matching in either direction; keys <5 chars are too generic
        let mut best: Option<(&String, &LiteEntry)> = None;
        for (k, e) in &self.map {
            if k.len() < 5 {
                continue;
            }
            // prefix only counts at a '-' boundary: "gpt-5-6-sol" may match
            // key "gpt-5-6" but "gpt-4o" must not match key "gpt-4".
            let pref = |a: &str, b: &str| {
                a.len() > b.len() && a.starts_with(b) && a.as_bytes()[b.len()] == b'-'
            };
            let hit = normalized_name == k || pref(normalized_name, k) || pref(k, normalized_name);
            if hit && best.is_none_or(|(bk, _)| k.len() > bk.len()) {
                best = Some((k, e));
            }
        }
        best.map(|(k, e)| (k.clone(), to_price(e)))
    }
}
