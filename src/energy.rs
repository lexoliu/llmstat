//! Serving-energy estimate behind the report's "did you know" footer.
//!
//! Per-model J/token, two paths:
//!
//! 1. **Known-parameter models** — `2 * P_active` FLOPs per forward-pass
//!    token at ~200 GFLOP/J datacenter-effective (H100 BF16, ~30% serving
//!    MFU, 8-GPU node ~10.5kW incl. CPU/NVLink/PSU, PUE 1.15):
//!    `J/token = P_active[B] / 100`. Params come from upstream model
//!    cards; post-trained variants inherit the base's architecture
//!    (SWE-2 → Kimi K3, 2.8T total / 104B activated).
//! 2. **Everything else** — invert the list price. Energy is only ~3-5%
//!    of serving cost (a ~$1.5/hr GPU slot draws ~$0.10/hr of power), so
//!    price implies GPU-slot-seconds per token, and slot-seconds convert
//!    to energy directly: `J/token = price_$/Mtok * 3 * (1 - margin)`.
//!    Input/output are inverted separately — output decode is bandwidth-
//!    bound and priced ~5x input, which is physically meaningful.
//!
//! Cache-hit tokens get zero weight: the prefill that produced them was
//! already counted as input when it ran; counting them again would
//! double-count. Order-of-magnitude estimates only — serving energy
//! varies several-fold with batch utilization.

use crate::pricing::{self, Resolved};
use crate::report::Usage;

/// Datacenter-effective FLOPs per joule — H100 BF16 ~989 TFLOPS peak,
/// ~30% MFU under decode-heavy serving, node+PUE overhead.
const GFLOP_PER_JOULE: f64 = 200e9;
/// H100-class GPU slot amortized cost ($/hr) — capex dominates energy.
const GPU_SLOT_USD_HR: f64 = 1.5;
/// Node-slot power incl. share of CPU/NVLink/PSU/PUE (watts).
const GPU_SLOT_W: f64 = 1250.0;
/// Assumed serving gross margin folded into API list prices.
const DEFAULT_MARGIN: f64 = 0.5;
/// EIA US industrial retail average, $/kWh.
pub const INDUSTRIAL_USD_KWH: f64 = 0.081;
/// Fallback J/token for models with neither params nor a price.
const DEFAULT_J: f64 = 0.5;

/// How a model's coefficient was derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Architecture parameters from the model card.
    Params,
    /// List price inverted to GPU-slot-seconds (closed models).
    Price,
    /// No data — generic coefficient.
    Default,
}

/// Joules per input and output token for one model.
#[derive(Debug, Clone, Copy)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    pub basis: Basis,
}

/// Activated params (billions) for known architectures — normalized
/// substring patterns, checked against the raw name, the resolved label,
/// and the `priced_as` LiteLLM key (so `swe-2 → kimi-k3` inherits 104B).
const PARAMS: &[(&str, f64)] = &[
    // Moonshot — HF model cards / config.json
    ("kimi-k3", 104.0), // 2.8T total, KDA hybrid MoE
    ("kimi-k2", 32.0),  // 1.04T / 32B, covers k2.5/k2.6/k2.7-code
    // Zhipu — GLM-5 card: 744B/40B; GLM-4.5/4.6: 355B/32B
    ("glm-5", 40.0),
    ("glm-4", 32.0),
    // DeepSeek V3/R1 family
    ("deepseek", 37.0),
    // Qwen3
    ("qwen3-coder", 35.0), // 480B-A35B
    ("qwen3-235", 22.0),   // 235B-A22B
    ("qwen3-30", 3.3),     // 30B-A3B
    ("qwen3-32", 32.0),    // dense
    // Meta dense
    ("llama-3-1-405", 405.0),
    ("llama-3-1-70", 70.0),
    ("llama-3-3-70", 70.0),
    ("llama-3-1-8", 8.0),
    ("llama-3-2-3", 3.0),
    ("llama-3-2-1", 1.0),
    // OpenAI open-weight
    ("gpt-oss-120", 5.1),
    ("gpt-oss-20", 3.6),
    // Mistral
    ("mistral-large-3", 41.0), // 675B/41B
    ("mistral-large-2", 123.0),
    ("ministral-14", 14.0),
    ("ministral-8", 8.0),
    ("ministral-3", 3.0),
];

pub struct Energy {
    /// Serving gross margin assumed in price inversion (0..1).
    pub margin: f64,
    /// User overrides: (normalized pattern, active params in billions).
    pub params: Vec<(String, f64)>,
}

impl Default for Energy {
    fn default() -> Self {
        Self {
            margin: DEFAULT_MARGIN,
            params: Vec::new(),
        }
    }
}

impl Energy {
    fn active_params(&self, names: &[&str]) -> Option<f64> {
        for name in names {
            for (pat, b) in &self.params {
                if name.contains(pat.as_str()) {
                    return Some(*b);
                }
            }
            for (pat, b) in PARAMS {
                if name.contains(pat) {
                    return Some(*b);
                }
            }
        }
        None
    }

    /// J/token for `resolved`'s model. Params win; price inversion is the
    /// fallback so closed frontier models still get an estimate.
    pub fn rate(&self, raw_model: &str, resolved: &Resolved) -> Rate {
        let raw = pricing::normalize(raw_model);
        let label = pricing::normalize(&resolved.label);
        let priced_as = pricing::normalize(&resolved.priced_as);
        if let Some(b) = self.active_params(&[&raw, &label, &priced_as]) {
            let j = b * 1e9 * 2.0 / GFLOP_PER_JOULE;
            return Rate {
                input: j,
                output: j,
                basis: Basis::Params,
            };
        }
        if let Some(p) = &resolved.price {
            // $/Mtok → $/tok → GPU-slot-seconds → joules at slot power.
            let k = 3_600.0 / GPU_SLOT_USD_HR * GPU_SLOT_W * (1.0 - self.margin) / 1e6;
            return Rate {
                input: p.input * k,
                output: p.output * k,
                basis: Basis::Price,
            };
        }
        Rate {
            input: DEFAULT_J,
            output: DEFAULT_J,
            basis: Basis::Default,
        }
    }

    pub fn joules(&self, usage: &Usage, rate: Rate) -> f64 {
        usage.input as f64 * rate.input + usage.output as f64 * rate.output
    }
}

/// One everyday equivalence: `kwh` per unit of `name`, `group` keeps
/// different units of the same idea (a US home's day/month/year) from
/// appearing twice in one line.
const EQUIVALENCES: &[(&str, &str, f64)] = &[
    ("Google searches", "search", 0.000_3),
    ("phone charges", "phone", 0.018),
    ("laptop hours", "laptop", 0.05),
    ("kettle boils", "kettle", 0.11),
    ("mi in an EV", "ev", 0.25),
    ("10-min showers", "shower", 1.5),
    ("hours of central AC", "ac", 3.5),
    ("days of a US home", "us-home", 29.0),
    ("months of a US home", "us-home", 875.0),
    ("EV trips around the equator", "ev-eq", 6_225.0),
    ("years of a US home", "us-home", 10_500.0),
];

/// The 3 equivalences nearest `kwh` on a log scale, e.g. "84,000 mi in an
/// EV". Items counting under half a unit are dropped ("0 kettle boils"
/// reads as noise, not trivia).
pub fn equivalences(kwh: f64) -> Vec<String> {
    if kwh <= 0.0 {
        return Vec::new();
    }
    let mut ranked: Vec<(&str, &str, f64)> = EQUIVALENCES.to_vec();
    ranked.sort_by(|a, b| {
        let da = (kwh / a.2).abs().ln();
        let db = (kwh / b.2).abs().ln();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut seen = std::collections::HashSet::new();
    ranked
        .iter()
        .filter(|(_, group, _)| seen.insert(*group))
        .map(|(name, _, per)| (name, kwh / per))
        .filter(|(_, n)| *n >= 0.5)
        .take(3)
        .map(|(name, n)| {
            let c = if n >= 100.0 {
                crate::fmt::int(n as usize)
            } else if n >= 10.0 {
                format!("{}", n as u64)
            } else {
                format!("{n:.1}")
            };
            format!("{c} {name}")
        })
        .collect()
}
