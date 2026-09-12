//! Devin CLI: transcripts + sessions.db recovery.
//!
//! Transcripts (`*.json`, one per session) only serialize the *current* chain;
//! `db` adds calls from `sessions.db` that transcripts dropped — resumed,
//! compacted or forked chains, and subagent sessions that never get a
//! transcript at all.

pub mod cache;
pub mod db;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::fmt;
use crate::report::{Call, Usage};
use crate::sources::SourceOut;

#[derive(Deserialize)]
struct Transcript {
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    model_name: Option<String>,
    timestamp: Option<String>,
    metrics: Option<Metrics>,
}

#[derive(Deserialize)]
struct Metrics {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    cached_tokens: u64,
}

pub fn default_transcripts_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".local/share/devin/cli/transcripts")
}

pub fn default_db_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".local/share/devin/cli/sessions.db")
}

/// Scan `dir` for transcript JSONs, then recover extra calls from `db_path`
/// (sessions.db) unless it is None.
pub fn load(dir: &Path, db_path: Option<&Path>) -> Result<SourceOut> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("cannot read transcript dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let mut calls: Vec<Call> = Vec::new();
    // session -> multiset of prompt sizes, for exact dedup against db calls
    let mut prompt_ms: BTreeMap<String, BTreeMap<u64, u64>> = BTreeMap::new();
    // session -> (cached sum, prompt sum, output sum) -> share estimates
    let mut ratios: HashMap<String, (u64, u64, u64)> = HashMap::new();
    // session -> (dominant raw model, its prompt sum) as fallback model name
    let mut top_model: HashMap<String, (String, u64)> = HashMap::new();
    let mut g_cached: u64 = 0;
    let mut g_prompt: u64 = 0;
    let mut g_output: u64 = 0;
    let mut files_read = 0usize;
    let mut files_failed = 0usize;

    for path in files {
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                files_failed += 1;
                continue;
            }
        };
        let t: Transcript = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(_) => {
                files_failed += 1;
                continue;
            }
        };
        files_read += 1;
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        for step in &t.steps {
            let (Some(m), Some(raw_model)) = (&step.metrics, &step.model_name) else {
                continue;
            };
            if m.prompt_tokens == 0 && m.completion_tokens == 0 {
                continue;
            }
            let ts = step
                .timestamp
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc));
            let cached = m.cached_tokens.min(m.prompt_tokens);
            *prompt_ms
                .entry(name.clone())
                .or_default()
                .entry(m.prompt_tokens)
                .or_default() += 1;
            let r = ratios.entry(name.clone()).or_default();
            r.0 += cached;
            r.1 += m.prompt_tokens;
            r.2 += m.completion_tokens;
            g_cached += cached;
            g_prompt += m.prompt_tokens;
            g_output += m.completion_tokens;
            let tm = top_model.entry(name.clone()).or_default();
            if m.prompt_tokens > tm.1 {
                *tm = (raw_model.clone(), m.prompt_tokens);
            }
            calls.push(Call {
                source: "devin",
                session: name.clone(),
                model: raw_model.clone(),
                ts,
                usage: Usage {
                    input: m.prompt_tokens - cached,
                    cached,
                    output: m.completion_tokens,
                },
                estimated: false,
            });
        }
    }

    // ── recover calls from sessions.db ──────────────────────────────────
    let mut recovered_calls = 0usize;
    let mut recovered_tokens = 0u64;
    let db_ok = if let Some(p) = db_path {
        match db::load(p) {
            Ok(d) => {
                let g_ratio = if g_prompt > 0 {
                    (
                        g_cached as f64 / g_prompt as f64,
                        g_output as f64 / g_prompt as f64,
                    )
                } else {
                    (0.9, 0.006)
                };
                for call in d.calls {
                    if call.prompt == 0 {
                        continue;
                    }
                    // exact match against a transcript step's prompt => same call
                    if let Some(ms) = prompt_ms.get_mut(&call.session)
                        && let Some(n) = ms.get_mut(&call.prompt)
                        && *n > 0
                    {
                        *n -= 1;
                        continue;
                    }
                    // split the recovered prompt into cached/uncached at the
                    // session's observed ratio; output at its out:in ratio
                    let (cs, ps, os) = ratios.get(&call.session).copied().unwrap_or((0, 0, 0));
                    let (cr, or_) = if ps > 0 {
                        (cs as f64 / ps as f64, os as f64 / ps as f64)
                    } else {
                        g_ratio
                    };
                    let cached = ((call.prompt as f64 * cr).round() as u64).min(call.prompt);
                    let output = (call.prompt as f64 * or_).round() as u64;
                    let model = d
                        .session_models
                        .get(&call.session)
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .or_else(|| top_model.get(&call.session).map(|(m, _)| m.clone()))
                        .unwrap_or_else(|| "unknown".to_string());
                    recovered_calls += 1;
                    recovered_tokens += call.prompt;
                    calls.push(Call {
                        source: "devin",
                        session: call.session,
                        model,
                        ts: DateTime::from_timestamp(call.ts, 0),
                        usage: Usage {
                            input: call.prompt - cached,
                            cached,
                            output,
                        },
                        estimated: true,
                    });
                }
                true
            }
            Err(e) => {
                tracing::warn!("sessions.db unreadable: {e:#}");
                false
            }
        }
    } else {
        false
    };

    let note = match db_ok {
        true => format!(
            "devin: {files_read} transcripts · +{recovered_calls} calls ({} tok) recovered from sessions.db",
            fmt::tokens(recovered_tokens)
        ),
        false => format!("devin: {files_read} transcripts (sessions.db skipped)"),
    };
    let note = if files_failed > 0 {
        format!("{note} · {files_failed} files failed")
    } else {
        note
    };
    Ok(SourceOut { calls, note })
}
