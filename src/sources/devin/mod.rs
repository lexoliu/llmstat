//! Devin CLI: transcripts + sessions.db recovery.
//!
//! Transcripts (`*.json`, one per session) only serialize the *current* chain;
//! `db` adds calls from `sessions.db` that transcripts dropped — resumed,
//! compacted or forked chains, and subagent sessions that never get a
//! transcript at all.
//!
//! In `live` mode the scanner reads transcripts once (they establish the
//! exact-token multiset and the cached/output split ratios), then tails
//! sessions.db: new inference calls arrive as db rows and get their split
//! projected from the ratios — same accounting as a one-shot run.

pub mod cache;
pub mod db;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use indicatif::MultiProgress;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// Devin source with resident state: transcripts are read on the first
/// `tick` (they seed the dedup multiset and split ratios); every tick tails
/// sessions.db for calls not covered by any transcript step.
pub struct Scanner {
    dir: PathBuf,
    db: Option<db::Scanner>,
    first: bool,
    /// session -> multiset of transcript prompt sizes, for exact dedup
    /// against db calls.
    prompt_ms: BTreeMap<String, BTreeMap<u64, u64>>,
    /// session -> (cached sum, prompt sum, output sum) — split ratios.
    ratios: HashMap<String, (u64, u64, u64)>,
    /// session -> (dominant raw model, its prompt sum) — fallback naming.
    top_model: HashMap<String, (String, u64)>,
    /// Global (cached, prompt, output) sums — fallback ratio.
    g: (u64, u64, u64),
    files_read: usize,
    files_failed: usize,
    recovered_calls: usize,
    recovered_tokens: u64,
    db_failed: bool,
}

impl Scanner {
    /// `db_path` None = transcripts-only mode. A db that fails to open
    /// degrades to transcripts-only with a warning, same as one-shot.
    pub fn open(dir: &Path, db_path: Option<&Path>) -> Result<Self> {
        let db = match db_path {
            Some(p) => match db::Scanner::open(p) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("sessions.db unreadable: {e:#}");
                    None
                }
            },
            None => None,
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            db,
            first: true,
            prompt_ms: BTreeMap::new(),
            ratios: HashMap::new(),
            top_model: HashMap::new(),
            g: (0, 0, 0),
            files_read: 0,
            files_failed: 0,
            recovered_calls: 0,
            recovered_tokens: 0,
            db_failed: false,
        })
    }

    /// Read every transcript, emit its calls, and fill the dedup/ratio
    /// maps used to project db-recovered calls. Runs on the first tick.
    fn scan_transcripts(&mut self) -> Result<Vec<Call>> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.dir)
            .with_context(|| format!("cannot read transcript dir {}", self.dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();

        let mut calls = Vec::new();
        for path in files {
            let text = match fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => {
                    self.files_failed += 1;
                    continue;
                }
            };
            let t: Transcript = match serde_json::from_str(&text) {
                Ok(t) => t,
                Err(_) => {
                    self.files_failed += 1;
                    continue;
                }
            };
            self.files_read += 1;
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
                *self
                    .prompt_ms
                    .entry(name.clone())
                    .or_default()
                    .entry(m.prompt_tokens)
                    .or_default() += 1;
                let r = self.ratios.entry(name.clone()).or_default();
                r.0 += cached;
                r.1 += m.prompt_tokens;
                r.2 += m.completion_tokens;
                self.g.0 += cached;
                self.g.1 += m.prompt_tokens;
                self.g.2 += m.completion_tokens;
                let tm = self.top_model.entry(name.clone()).or_default();
                if m.prompt_tokens > tm.1 {
                    *tm = (raw_model.clone(), m.prompt_tokens);
                }
                calls.push(Call {
                    source: "devin",
                    session: name.as_str().into(),
                    model: raw_model.as_str().into(),
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
        Ok(calls)
    }

    /// Turn a recovered db call into a `Call` — None when it matches an
    /// already-counted transcript step (exact prompt-token match).
    fn project(&mut self, call: db::DbCall) -> Option<Call> {
        if call.prompt == 0 {
            return None;
        }
        if let Some(ms) = self.prompt_ms.get_mut(&call.session)
            && let Some(n) = ms.get_mut(&call.prompt)
            && *n > 0
        {
            *n -= 1;
            return None;
        }
        // split the recovered prompt into cached/uncached at the session's
        // observed ratio; output at its out:in ratio
        let (cs, ps, os) = self.ratios.get(&call.session).copied().unwrap_or((0, 0, 0));
        let (cr, or_) = if ps > 0 {
            (cs as f64 / ps as f64, os as f64 / ps as f64)
        } else if self.g.1 > 0 {
            (
                self.g.0 as f64 / self.g.1 as f64,
                self.g.2 as f64 / self.g.1 as f64,
            )
        } else {
            (0.9, 0.006)
        };
        let cached = ((call.prompt as f64 * cr).round() as u64).min(call.prompt);
        let output = (call.prompt as f64 * or_).round() as u64;
        let model = self
            .db
            .as_ref()
            .and_then(|d| d.session_models.get(&call.session))
            .filter(|s| !s.is_empty())
            .cloned()
            .or_else(|| self.top_model.get(&call.session).map(|(m, _)| m.clone()))
            .unwrap_or_else(|| "unknown".to_string());
        self.recovered_calls += 1;
        self.recovered_tokens += call.prompt;
        Some(Call {
            source: "devin",
            session: Arc::from(call.session.as_str()),
            model: model.into(),
            ts: DateTime::from_timestamp(call.ts, 0),
            usage: Usage {
                input: call.prompt - cached,
                cached,
                output,
            },
            estimated: true,
        })
    }

    /// First tick: transcripts + whatever db tail the cache missed. Later
    /// ticks: just the db tail — transcripts aren't re-read (their calls
    /// would double-count; db covers new calls anyway, as estimates).
    pub fn tick(&mut self, mp: &MultiProgress) -> Result<Vec<Call>> {
        let mut calls = Vec::new();
        if self.first {
            self.first = false;
            calls.extend(self.scan_transcripts()?);
        }
        if let Some(d) = &mut self.db {
            match d.tick(mp) {
                Ok(rows) => {
                    for c in rows {
                        calls.extend(self.project(c));
                    }
                }
                Err(e) => {
                    self.db_failed = true;
                    tracing::warn!("sessions.db scan failed: {e:#}");
                }
            }
        }
        Ok(calls)
    }

    /// Coverage line for the report header.
    pub fn note(&self) -> String {
        let mut note = match self.db.is_some() && !self.db_failed {
            true => format!(
                "devin: {} transcripts · +{} calls ({} tok) recovered from sessions.db",
                self.files_read,
                self.recovered_calls,
                fmt::tokens(self.recovered_tokens)
            ),
            false => format!(
                "devin: {} transcripts (sessions.db skipped)",
                self.files_read
            ),
        };
        if self.files_failed > 0 {
            note.push_str(&format!(" · {} files failed", self.files_failed));
        }
        note
    }
}

/// Scan `dir` for transcript JSONs, then recover extra calls from `db_path`
/// (sessions.db) unless it is None.
pub fn load(dir: &Path, db_path: Option<&Path>, mp: &MultiProgress) -> Result<SourceOut> {
    let mut sc = Scanner::open(dir, db_path)?;
    let calls = sc.tick(mp)?;
    let note = sc.note();
    Ok(SourceOut { calls, note })
}
