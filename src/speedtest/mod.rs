//! `llmstat speedtest` — live latency/throughput probes against inference
//! backends (Devin's Connect-RPC API, Antigravity's Cloud Code endpoint).

mod antigravity;
mod devin;
mod proto;

use std::time::Duration;

use anyhow::Context;
use clap::ValueEnum;

const DEFAULT_PROMPT: &str = "Count from 1 to 100 separated by spaces.";

#[derive(Clone, Copy, ValueEnum)]
pub enum ProviderKind {
    /// Devin CLI's backend (server.codeium.com Connect-RPC).
    Devin,
    /// Antigravity's Cloud Code Assist backend (cloudcode-pa).
    Antigravity,
}

/// One entry of a provider's live model catalog.
pub struct ModelEntry {
    /// Model uid sent on the wire (`swe-2-max`, `gemini-3.8-flash-low`).
    pub uid: String,
    /// Human label when the catalog provides one.
    pub label: String,
    /// Cost/price hint for display (credit multiplier or quota fraction).
    pub cost_hint: String,
}

/// What one measured run observed.
pub struct RunStats {
    /// First content delta after request start.
    pub ttft: Duration,
    /// Whole request duration.
    pub total: Duration,
    /// Billed/visible output tokens (including hidden thinking when the
    /// backend counts them).
    pub output_tokens: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    /// Provider stop/finish reason, if reported.
    pub stop: String,
    /// Server-reported TTFT, when the API exposes one.
    pub server_ttft: Option<f64>,
    /// Characters of visible text received (sanity check the model spoke).
    pub text_chars: usize,
}

trait Provider {
    /// Fetch the live model catalog.
    fn catalog(&mut self) -> anyhow::Result<Vec<ModelEntry>>;
    /// Run one streaming call and measure it.
    fn stream(&self, uid: &str, prompt: &str, max_tokens: Option<u64>) -> anyhow::Result<RunStats>;
}

/// Map `--model <family> --effort <tier>` onto a concrete catalog uid.
///
/// Accepts `<model>-<effort>` (and `<MODEL>_<EFFORT>` legacy ids); a bare
/// family uid is allowed when it exists and its label names the tier.
/// Anything else errors and lists the family's real tiers.
fn resolve_uid(catalog: &[ModelEntry], model: &str, effort: &str) -> anyhow::Result<String> {
    let m = model.to_lowercase();
    let e = effort.to_lowercase().replace('_', "-");
    let family: Vec<&ModelEntry> = catalog
        .iter()
        .filter(|c| {
            let u = c.uid.to_lowercase();
            u == m || u.starts_with(&format!("{m}-")) || u.starts_with(&format!("{m}_"))
        })
        .collect();
    if family.is_empty() {
        let sample: Vec<&str> = catalog.iter().map(|c| c.uid.as_str()).collect();
        anyhow::bail!(
            "unknown model family '{model}'. catalog has {} entries, e.g. {}",
            catalog.len(),
            sample[..sample.len().min(12)].join(", ")
        );
    }
    let candidates = [
        format!("{m}-{e}"),
        format!("{}_{}", m.to_uppercase(), e.to_uppercase()),
    ];
    for c in &family {
        let u = c.uid.to_lowercase();
        if candidates.contains(&u) {
            return Ok(c.uid.clone());
        }
    }
    // bare uid whose label states the tier (e.g. uid "glm-5-2" = "GLM-5.2 High")
    for c in &family {
        let u = c.uid.to_lowercase();
        if u == m
            && (u == format!("{m}-{e}")
                || u.ends_with(&format!("-{e}"))
                || c.label
                    .to_lowercase()
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|w| w == e))
        {
            return Ok(c.uid.clone());
        }
    }
    let variants: Vec<String> = family
        .iter()
        .map(|c| format!("{} ({})", c.uid, c.label))
        .collect();
    anyhow::bail!(
        "no '{e}' tier for '{model}'. available: {}",
        variants.join(", ")
    )
}

fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    kind: ProviderKind,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
    runs: u32,
    max_tokens: Option<u64>,
    list: bool,
) -> anyhow::Result<()> {
    let mut provider: Box<dyn Provider> = match kind {
        ProviderKind::Devin => Box::new(devin::Devin::new()?),
        ProviderKind::Antigravity => Box::new(antigravity::Antigravity::new()?),
    };
    let catalog = provider.catalog().context("fetching model catalog")?;

    if list {
        for c in &catalog {
            println!("{:40} {:36} {}", c.uid, c.label, c.cost_hint);
        }
        return Ok(());
    }

    let model = model.context("--model is required (e.g. --model swe-2)")?;
    let effort = effort.context("--effort is required (e.g. --effort high)")?;
    let uid = resolve_uid(&catalog, &model, &effort)?;
    let entry = catalog.iter().find(|c| c.uid.eq_ignore_ascii_case(&uid));
    println!(
        "model: {model} · effort: {effort} → uid {}{}{}",
        uid,
        entry.map(|e| format!(" ({})", e.label)).unwrap_or_default(),
        entry
            .map(|e| if e.cost_hint.is_empty() {
                String::new()
            } else {
                format!(", {}", e.cost_hint)
            })
            .unwrap_or_default(),
    );
    let prompt = prompt.unwrap_or_else(|| DEFAULT_PROMPT.to_string());
    println!("prompt: {prompt:?} · runs: {runs}\n");

    let mut rows = Vec::new();
    for i in 0..runs {
        let s = provider
            .stream(&uid, &prompt, max_tokens)
            .with_context(|| format!("run {}", i + 1))?;
        let decode = s.total.saturating_sub(s.ttft);
        let tps = if decode.as_secs_f64() > 0.0 {
            s.output_tokens as f64 / decode.as_secs_f64()
        } else {
            0.0
        };
        println!(
            "run {:>2}  ttft {:>5.2}s{}  decode {:>5.2}s  out {:>4} tok ({:>5.0} tok/s)  in {} (cache {})  text {}ch  {}",
            i + 1,
            s.ttft.as_secs_f64(),
            s.server_ttft
                .map(|t| format!(" [srv {:.2}s]", t))
                .unwrap_or_default(),
            decode.as_secs_f64(),
            fmt_tok(s.output_tokens),
            tps,
            fmt_tok(s.input_tokens),
            fmt_tok(s.cache_read_tokens),
            s.text_chars,
            s.stop,
        );
        rows.push((s, tps));
    }
    if rows.len() > 1 {
        let mut ttfts: Vec<f64> = rows.iter().map(|(s, _)| s.ttft.as_secs_f64()).collect();
        let mut tps: Vec<f64> = rows.iter().map(|(_, t)| *t).collect();
        ttfts.sort_by(f64::total_cmp);
        tps.sort_by(f64::total_cmp);
        let med = |v: &[f64]| {
            if v.len() % 2 == 1 {
                v[v.len() / 2]
            } else {
                (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
            }
        };
        println!(
            "\nmedian  ttft {:.2}s (min {:.2} / max {:.2})  decode {:.0} tok/s (min {:.0} / max {:.0})",
            med(&ttfts),
            ttfts[0],
            ttfts[ttfts.len() - 1],
            med(&tps),
            tps[0],
            tps[tps.len() - 1],
        );
    }
    Ok(())
}
