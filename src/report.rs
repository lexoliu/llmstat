//! Source-agnostic aggregation: a `Call` is one model invocation with token
//! usage; `build()` folds calls into the per-model/session/timeline `Report`.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::energy::{Basis, Energy};
use crate::filter::{Filter, Verdict};
use crate::pricing::{Price, PriceBook, Pricing, Resolved};

/// Token usage for a single call or aggregate. `input` is the *uncached*
/// portion of prompt tokens; `cached` is the cache-hit subset.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.cached + self.output
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.cached += other.cached;
        self.output += other.output;
    }

    pub fn cost(&self, p: &Price) -> f64 {
        (self.input as f64 * p.input
            + self.cached as f64 * p.cached
            + self.output as f64 * p.output)
            / 1_000_000.0
    }
}

/// One model call emitted by a source. Session/model are `Arc<str>` — the
/// file cache interns them per file, so rehydration is a refcount bump.
pub struct Call {
    /// Which CLI produced it: "devin" | "claude" | "codex".
    pub source: &'static str,
    /// Session identifier within that source.
    pub session: Arc<str>,
    /// Human-readable session name when the source tracks one (claude slug,
    /// codex first prompt, devin title) — `session` stays the canonical id.
    pub session_name: Option<Arc<str>>,
    /// Raw model name as recorded by the CLI.
    pub model: Arc<str>,
    pub ts: Option<DateTime<Utc>>,
    pub usage: Usage,
    /// Token split was estimated (e.g. db-recovered calls only record the
    /// exact prompt total; cached/output are projected from ratios).
    pub estimated: bool,
}

#[derive(Debug)]
pub struct ModelStat {
    pub source: &'static str,
    /// Canonical display label (rule label, litellm key, or raw name).
    pub label: String,
    /// Raw model names folded into this stat.
    pub raw_names: Vec<String>,
    pub usage: Usage,
    pub sessions: usize,
    pub steps: usize,
    pub pricing: Pricing,
    /// Per-1M-token price used for the list-price estimate.
    pub price: Option<Price>,
    /// What the price was borrowed from (litellm key, rule label, "list").
    pub priced_as: String,
}

impl ModelStat {
    /// Cost at the equivalent list price (the crossed-out figure for free models).
    pub fn list_cost(&self) -> Option<f64> {
        self.price.as_ref().map(|p| self.usage.cost(p))
    }
}

#[derive(Debug)]
pub struct Session {
    /// "source:name" — shown under "heaviest session".
    pub name: String,
    pub last_ts: Option<DateTime<Utc>>,
    pub usage: Usage,
    pub steps: usize,
    /// (source, model label) -> usage within this session.
    pub models: BTreeMap<(&'static str, Arc<str>), Usage>,
    pub list_cost: f64,
    pub actual_cost: f64,
    pub has_unpriced: bool,
}

/// Usage in one time bucket (hour / day / week).
#[derive(Debug, Default)]
pub struct Bucket {
    pub label: String,
    pub usage: Usage,
    pub list_cost: f64,
    pub actual_cost: f64,
    pub has_unpriced: bool,
}

/// How the per-bucket timeline is sliced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketKind {
    Hour,
    Day,
    Week,
    /// Day if the span is short, Week for long spans.
    Auto,
}

#[derive(Debug)]
pub struct Report {
    pub models: Vec<ModelStat>,
    pub sessions: Vec<Session>,
    pub buckets: Vec<Bucket>,
    pub total: Usage,
    pub total_calls: usize,
    pub list_cost: f64,
    pub actual_cost: f64,
    pub has_unpriced: bool,
    /// Per-source coverage notes (e.g. files read, calls recovered).
    pub coverage: Vec<String>,
    /// Parse failures worth surfacing.
    pub warnings: Vec<String>,
    pub earliest: Option<DateTime<Utc>>,
    pub latest: Option<DateTime<Utc>>,
    /// True when any call's token split was estimated rather than recorded.
    pub has_estimated: bool,
    /// Estimated serving energy of all included calls, in joules.
    pub energy_j: f64,
    /// True when any model's energy came from price inversion or the
    /// generic fallback rather than architecture parameters.
    pub energy_inferred: bool,
}

/// Which calls enter the report and how the timeline is sliced.
pub struct Spec<'a> {
    /// Drop calls older than this.
    pub since: Option<DateTime<Utc>>,
    pub bucket: BucketKind,
    /// `--filter` expression; only matching calls are folded in.
    pub filter: Option<&'a Filter>,
}

/// Fold `calls` into a `Report`, per `spec`.
pub fn build(
    calls: Vec<Call>,
    book: &PriceBook,
    energy: &Energy,
    spec: Spec<'_>,
    coverage: Vec<String>,
    warnings: Vec<String>,
) -> Report {
    let since = spec.since;
    let bucket = spec.bucket;
    let mut report = Report {
        models: Vec::new(),
        sessions: Vec::new(),
        buckets: Vec::new(),
        total: Usage::default(),
        total_calls: 0,
        list_cost: 0.0,
        actual_cost: 0.0,
        has_unpriced: false,
        coverage,
        warnings,
        earliest: None,
        latest: None,
        has_estimated: false,
        energy_j: 0.0,
        energy_inferred: false,
    };
    // (source, model label) -> index into report.models
    let mut model_idx: BTreeMap<(&'static str, Arc<str>), usize> = BTreeMap::new();
    // (source, session) -> Session
    let mut sessions: BTreeMap<(&'static str, Arc<str>), Session> = BTreeMap::new();
    let mut day_buckets: BTreeMap<NaiveDate, Bucket> = BTreeMap::new();
    let mut hour_buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    // raw model -> resolved pricing; model names repeat massively across
    // calls, so resolve once per distinct name instead of per call
    let mut resolved_list: Vec<Resolved> = Vec::new();
    let mut label_arc: Vec<Arc<str>> = Vec::new();
    // energy J/token memoized per resolved model — same slot as `resolved`
    let mut energy_rate: Vec<crate::energy::Rate> = Vec::new();
    let mut resolve_idx: HashMap<Arc<str>, usize> = HashMap::new();
    // Any call that passed the `since` bound — distinguishes "filter
    // matched nothing" from "no data in range".
    let mut in_range = false;

    for ev in calls {
        if let (Some(c), Some(t)) = (since, ev.ts)
            && t < c
        {
            continue;
        }
        in_range = true;
        let usage = ev.usage;
        let ridx = *resolve_idx.entry(ev.model.clone()).or_insert_with(|| {
            let r = book.resolve(&ev.model);
            energy_rate.push(energy.rate(&ev.model, &r));
            label_arc.push(r.label.as_str().into());
            resolved_list.push(r);
            resolved_list.len() - 1
        });
        let resolved = &resolved_list[ridx];
        if let Some(f) = spec.filter
            && f.eval(&ev, Some(resolved)) == Verdict::Fail
        {
            continue;
        }
        report.has_estimated |= ev.estimated;
        let rate = energy_rate[ridx];
        report.energy_j += energy.joules(&usage, rate);
        report.energy_inferred |= rate.basis != Basis::Params;
        let step_cost = resolved.price.as_ref().map(|p| usage.cost(p));
        let step_paid = matches!(resolved.pricing, Pricing::Paid);

        // distinct raw names can share a label ("swe-2-max", "SWE-2 Max") —
        // group by label, not by resolved index
        let key = (ev.source, label_arc[ridx].clone());
        let idx = *model_idx.entry(key).or_insert_with(|| {
            report.models.push(ModelStat {
                source: ev.source,
                label: resolved.label.clone(),
                raw_names: Vec::new(),
                usage: Usage::default(),
                sessions: 0,
                steps: 0,
                pricing: resolved.pricing.clone(),
                price: resolved.price,
                priced_as: resolved.priced_as.clone(),
            });
            report.models.len() - 1
        });
        let stat = &mut report.models[idx];
        if !stat.raw_names.iter().any(|n| n.as_str() == &*ev.model) {
            stat.raw_names.push(ev.model.to_string());
        }
        stat.usage.add(&usage);
        stat.steps += 1;

        let skey = (ev.source, ev.session.clone());
        let session = sessions.entry(skey).or_insert_with(|| Session {
            name: format!("{}:{}", ev.source, ev.session),
            last_ts: None,
            usage: Usage::default(),
            steps: 0,
            models: BTreeMap::new(),
            list_cost: 0.0,
            actual_cost: 0.0,
            has_unpriced: false,
        });
        session
            .models
            .entry((ev.source, label_arc[ridx].clone()))
            .or_default()
            .add(&usage);
        session.usage.add(&usage);
        session.steps += 1;
        session.last_ts = match (session.last_ts, ev.ts) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, b) => b,
            (a, None) => a,
        };
        if let Some(c) = step_cost {
            session.list_cost += c;
            if step_paid {
                session.actual_cost += c;
            }
        } else {
            session.has_unpriced = true;
        }

        if let Some(t) = ev.ts {
            let local = t.with_timezone(&Local);
            let day_key = local.date_naive();
            let bump = |b: &mut Bucket| {
                b.usage.add(&usage);
                if let Some(c) = step_cost {
                    b.list_cost += c;
                    if step_paid {
                        b.actual_cost += c;
                    }
                } else {
                    b.has_unpriced = true;
                }
            };
            bump(day_buckets.entry(day_key).or_default());
            if bucket == BucketKind::Hour {
                let key = local.format("%Y-%m-%d %H").to_string();
                bump(hour_buckets.entry(key).or_default());
            }
        }

        report.total.add(&usage);
        report.total_calls += 1;
        report.earliest = match (report.earliest, ev.ts) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (None, b) => b,
            (a, None) => a,
        };
        report.latest = match (report.latest, ev.ts) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, b) => b,
            (a, None) => a,
        };
    }

    for session in sessions.into_values() {
        for key in session.models.keys() {
            if let Some(&i) = model_idx.get(key) {
                report.models[i].sessions += 1;
            }
        }
        if session.steps > 0 {
            report.sessions.push(session);
        }
    }

    for m in &report.models {
        if let Some(cost) = m.list_cost() {
            report.list_cost += cost;
            if matches!(m.pricing, Pricing::Paid) {
                report.actual_cost += cost;
            }
        } else {
            report.has_unpriced = true;
        }
    }
    if report.total_calls == 0 && in_range && spec.filter.is_some() {
        report.warnings.push("filter matched no calls".to_string());
    }

    // resolve Auto and build the final ordered bucket list, filling gaps
    let span_days = match (report.earliest, report.latest) {
        (Some(a), Some(b)) => (b - a).num_days().max(1),
        _ => 1,
    };
    let kind = match bucket {
        BucketKind::Auto => {
            if span_days > 45 {
                BucketKind::Week
            } else {
                BucketKind::Day
            }
        }
        k => k,
    };
    report.buckets = match kind {
        BucketKind::Hour => finalize_hours(hour_buckets, since, report.latest),
        BucketKind::Day => finalize_days(day_buckets, since, report.earliest, report.latest),
        BucketKind::Week => finalize_weeks(day_buckets),
        BucketKind::Auto => unreachable!(),
    };

    report
        .models
        .sort_by_key(|m| std::cmp::Reverse(m.usage.total()));
    report
        .sessions
        .sort_by_key(|s| std::cmp::Reverse(s.last_ts));
    report
}

fn finalize_hours(
    mut buckets: BTreeMap<String, Bucket>,
    since: Option<DateTime<Utc>>,
    latest: Option<DateTime<Utc>>,
) -> Vec<Bucket> {
    let end = latest.unwrap_or_else(Utc::now).with_timezone(&Local);
    let start = since.unwrap_or_else(|| Utc::now() - Duration::hours(23));
    let mut cur = start
        .with_timezone(&Local)
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or_else(|| start.with_timezone(&Local));
    let mut out = Vec::new();
    while cur <= end {
        let key = cur.format("%Y-%m-%d %H").to_string();
        let mut b = buckets.remove(&key).unwrap_or_default();
        b.label = cur.format("%b %d %H:00").to_string();
        out.push(b);
        cur += Duration::hours(1);
    }
    for (_, mut b) in buckets {
        if b.usage.total() > 0 {
            b.label = "?".into();
            out.push(b);
        }
    }
    let first = out.iter().position(|b| b.usage.total() > 0).unwrap_or(0);
    out.drain(..first);
    out
}

fn finalize_days(
    mut buckets: BTreeMap<NaiveDate, Bucket>,
    since: Option<DateTime<Utc>>,
    earliest: Option<DateTime<Utc>>,
    latest: Option<DateTime<Utc>>,
) -> Vec<Bucket> {
    let end = latest
        .unwrap_or_else(Utc::now)
        .with_timezone(&Local)
        .date_naive();
    let start = since
        .or(earliest)
        .map(|t| t.with_timezone(&Local).date_naive())
        .unwrap_or(end);
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        let mut b = buckets.remove(&cur).unwrap_or_default();
        b.label = cur.format("%b %d").to_string();
        out.push(b);
        cur += Duration::days(1);
    }
    for (_, mut b) in buckets {
        if b.usage.total() > 0 {
            b.label = "?".into();
            out.push(b);
        }
    }
    let first = out.iter().position(|b| b.usage.total() > 0).unwrap_or(0);
    out.drain(..first);
    out
}

fn finalize_weeks(days: BTreeMap<NaiveDate, Bucket>) -> Vec<Bucket> {
    let mut weeks: BTreeMap<(i32, u32), Bucket> = BTreeMap::new();
    for (d, b) in days {
        if b.usage.total() == 0 {
            continue;
        }
        let w = d.iso_week();
        weeks
            .entry((w.year(), w.week()))
            .or_default()
            .usage
            .add(&b.usage);
        let wb = weeks.get_mut(&(w.year(), w.week())).unwrap();
        wb.list_cost += b.list_cost;
        wb.actual_cost += b.actual_cost;
        wb.has_unpriced |= b.has_unpriced;
    }
    weeks
        .into_iter()
        .map(|((y, w), mut b)| {
            let mon = NaiveDate::from_isoywd_opt(y, w, chrono::Weekday::Mon);
            let sun = NaiveDate::from_isoywd_opt(y, w, chrono::Weekday::Sun);
            b.label = match (mon, sun) {
                (Some(m), Some(s)) => format!("{}–{}", m.format("%b %d"), s.format("%b %d")),
                _ => format!("{y}-W{w:02}"),
            };
            b
        })
        .collect()
}
