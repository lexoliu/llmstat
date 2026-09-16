//! Pricing resolution: user TOML rules → built-in rules → LiteLLM pricebook.
//!
//! Rules encode semantics LiteLLM can't express — free-in-CLI models priced
//! at an equivalent public model (SWE-2 → kimi-k3). A rule may carry `as`
//! (a LiteLLM key) to borrow current prices from the pricebook; its explicit
//! `input`/`cached`/`output` numbers apply when the key isn't listed.

pub mod litellm;

use anyhow::{Context, Result};
use litellm::LiteBook;
use serde::Deserialize;
use std::path::Path;

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy)]
pub struct Price {
    /// Uncached input tokens.
    pub input: f64,
    /// Cache-hit input tokens.
    pub cached: f64,
    /// Output/completion tokens.
    pub output: f64,
}

#[derive(Debug, Clone)]
pub enum Pricing {
    /// Billed at `price`.
    Paid,
    /// Free in the CLI — `price` is the equivalent list price, shown struck
    /// through; actual charge is $0.
    Free,
    /// No price found anywhere; shown as "?".
    Unpriced,
}

#[derive(Debug, Clone)]
pub struct Rule {
    /// Lowercase-normalized substring matched against the normalized model name.
    pub pattern: String,
    pub label: String,
    pub free: bool,
    /// LiteLLM key whose current price this model borrows (free models).
    pub priced_as: Option<String>,
    pub price: Option<Price>,
}

pub struct Resolved {
    pub label: String,
    pub pricing: Pricing,
    pub price: Option<Price>,
    /// Display label for the price source (litellm key, rule label, "list").
    pub priced_as: String,
}

pub struct PriceBook {
    rules: Vec<Rule>,
    litellm: LiteBook,
    /// Provenance note for the report footer.
    pub source_note: String,
}

/// Normalize a raw model name for matching: lowercase, runs of
/// non-alphanumerics become a single '-'.
/// "SWE-1.7 Max" -> "swe-1-7-max", "claude-opus-4-5-20251101" stays dashed.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = true;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

impl PriceBook {
    pub fn new(extra: Vec<Rule>, litellm: LiteBook) -> Self {
        let mut rules = extra;
        rules.extend(default_rules());
        Self {
            rules,
            source_note: litellm.note.clone(),
            litellm,
        }
    }

    pub fn resolve(&self, raw_model: &str) -> Resolved {
        let norm = normalize(raw_model);
        for r in &self.rules {
            if !norm.contains(&r.pattern) {
                continue;
            }
            // `as` borrows the live LiteLLM price; explicit rule price is the
            // declared equivalent when the key isn't listed.
            let (price, as_label) = match &r.priced_as {
                Some(k) => match self.litellm.lookup(&normalize(k)) {
                    Some((key, p)) => (Some(p), key),
                    None => (r.price, k.clone()),
                },
                None => (r.price, "list".to_string()),
            };
            return Resolved {
                label: r.label.clone(),
                pricing: if r.free { Pricing::Free } else { Pricing::Paid },
                price,
                priced_as: as_label,
            };
        }
        if let Some((key, price)) = self.litellm.lookup(&norm) {
            return Resolved {
                label: raw_model.trim().to_string(),
                pricing: Pricing::Paid,
                price: Some(price),
                priced_as: key,
            };
        }
        Resolved {
            label: raw_model.trim().to_string(),
            pricing: Pricing::Unpriced,
            price: None,
            priced_as: "?".into(),
        }
    }
}

/// Rules matched in order — specific patterns before generic ones. Explicit
/// prices are public API list prices (USD / 1M tokens) used when the `as`
/// key isn't in the LiteLLM book.
fn default_rules() -> Vec<Rule> {
    let r = |pattern: &str,
             label: &str,
             free: bool,
             priced_as: Option<&str>,
             i: f64,
             c: f64,
             o: f64| Rule {
        pattern: normalize(pattern),
        label: label.to_string(),
        free,
        priced_as: priced_as.map(|s| s.to_string()),
        price: Some(Price {
            input: i,
            cached: c,
            output: o,
        }),
    };
    vec![
        // --- Cognition (free in Devin CLI), priced at Moonshot equivalents ---
        r(
            "swe-1-7",
            "SWE-1.7",
            true,
            Some("kimi-k2.7-code"),
            0.95,
            0.19,
            4.00,
        ),
        r(
            "swe-1-6",
            "SWE-1.6",
            true,
            Some("kimi-k2.6"),
            0.95,
            0.16,
            4.00,
        ),
        r("swe-2", "SWE-2", true, Some("kimi-k3"), 3.00, 0.30, 15.00),
        r(
            "adaptive",
            "Adaptive",
            true,
            Some("kimi-k3"),
            3.00,
            0.30,
            15.00,
        ),
        r(
            "fusion",
            "Fusion",
            true,
            Some("claude-fable-5-1"),
            10.00,
            0.25,
            50.00,
        ),
        // --- Devin CLI display names (also priced by LiteLLM if present) ---
        r(
            "gpt-6-astra",
            "GPT-6 Astra",
            false,
            None,
            10.00,
            1.00,
            50.00,
        ),
        r("gpt-5-6-sol", "GPT-5.6 Sol", false, None, 4.00, 0.40, 20.00),
        r(
            "gpt-5-6-terra",
            "GPT-5.6 Terra",
            false,
            None,
            2.00,
            0.20,
            12.00,
        ),
        r(
            "gpt-5-6-luna",
            "GPT-5.6 Luna",
            false,
            None,
            0.20,
            0.02,
            1.20,
        ),
        // open models offered free inside Devin CLI; no public equivalent
        Rule {
            pattern: normalize("penguin"),
            label: "Penguin".into(),
            free: true,
            priced_as: None,
            price: None,
        },
        r("glm-5", "GLM-5", true, Some("glm-5"), 1.40, 0.26, 4.40),
    ]
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    rule: Vec<RuleToml>,
    energy: Option<EnergyToml>,
    #[serde(default)]
    param: Vec<ParamToml>,
}

#[derive(Deserialize)]
struct EnergyToml {
    /// Gross margin assumed when inverting list prices into energy.
    margin: Option<f64>,
}

/// `[[param]] pattern active_b` — activated params (billions) for a model
/// the built-in table doesn't know.
#[derive(Deserialize)]
struct ParamToml {
    pattern: String,
    active_b: f64,
}

#[derive(Deserialize)]
struct RuleToml {
    pattern: String,
    label: Option<String>,
    #[serde(default)]
    free: bool,
    /// LiteLLM key to borrow prices from.
    #[serde(rename = "as")]
    priced_as: Option<String>,
    /// USD per 1M uncached input tokens.
    input: Option<f64>,
    /// USD per 1M cached input tokens (defaults to `input`).
    cached: Option<f64>,
    /// USD per 1M output tokens.
    output: Option<f64>,
}

fn parse_config(text: &str, path: &Path) -> Result<(Vec<Rule>, crate::energy::Energy)> {
    let file: ConfigFile =
        toml::from_str(text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    let rules = file
        .rule
        .into_iter()
        .map(|r| Rule {
            pattern: normalize(&r.pattern),
            label: r.label.unwrap_or_else(|| r.pattern.clone()),
            free: r.free,
            priced_as: r.priced_as,
            price: r.input.map(|input| Price {
                input,
                cached: r.cached.unwrap_or(input),
                output: r.output.unwrap_or(0.0),
            }),
        })
        .collect();
    let mut energy = crate::energy::Energy::default();
    if let Some(m) = file.energy.and_then(|e| e.margin) {
        energy.margin = m;
    }
    energy.params = file
        .param
        .into_iter()
        .map(|p| (normalize(&p.pattern), p.active_b))
        .collect();
    Ok((rules, energy))
}

/// Load rules (+ energy settings) from a TOML file; they take precedence
/// over built-ins.
pub fn load_config(path: &Path) -> Result<(Vec<Rule>, crate::energy::Energy)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read pricing file {}", path.display()))?;
    parse_config(&text, path)
}

/// Load extra rules from a TOML file; they take precedence over built-ins.
pub fn load_rules(path: &Path) -> Result<Vec<Rule>> {
    load_config(path).map(|(rules, _)| rules)
}

pub fn default_config_path() -> std::path::PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".config/llmstat.toml")
}

/// Load `~/.config/llmstat.toml` if it exists.
pub fn load_default_config() -> (Vec<Rule>, crate::energy::Energy) {
    let p = default_config_path();
    if p.exists() {
        load_config(&p).unwrap_or_default()
    } else {
        Default::default()
    }
}
