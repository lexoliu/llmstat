//! Codex CLI: `~/.codex/sessions/**/rollout-*.jsonl` and
//! `~/.codex/archived_sessions/*.jsonl`.
//!
//! `event_msg`/`token_count` payloads carry `last_token_usage` (per-call
//! delta) plus cumulative `total_token_usage`; the delta is what we count.
//! `session_meta`/`turn_context` are marked by the top-level `type` and hold
//! the session id / current model.

use anyhow::Result;
use chrono::{DateTime, Utc};
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::report::{Call, Usage};
use crate::sources::SourceOut;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    model: Option<String>,
    id: Option<String>,
    session_id: Option<String>,
    info: Option<Info>,
}

#[derive(Deserialize)]
struct Info {
    last_token_usage: Option<Tokens>,
    total_token_usage: Option<Tokens>,
}

#[derive(Deserialize)]
struct Tokens {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

/// Session dirs under a codex root (`~/.codex`).
pub fn dirs_for(root: &Path) -> Vec<PathBuf> {
    vec![root.join("sessions"), root.join("archived_sessions")]
}

fn session_id_of(path: &Path) -> String {
    // rollout-<ts>-<uuid>.jsonl -> the uuid tail
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    stem.rsplit('-')
        .take(5) // uuid is 5 dash-separated segments
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("-")
}

/// `(dedup_key, call)` pairs found in one rollout file.
fn parse_file(path: &Path) -> Vec<(String, Call)> {
    let mut out = Vec::new();
    let mut model = String::from("unknown");
    let mut sid = session_id_of(path);
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return out,
    };
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if !line.contains("\"model\"")
            && !line.contains("token_count")
            && !line.contains("session_meta")
        {
            continue;
        }
        let l: Line = match serde_json::from_str(&line) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let Some(p) = l.payload else { continue };
        match l.kind.as_deref() {
            Some("session_meta") | Some("turn_context") => {
                if let Some(m) = &p.model {
                    model = m.clone();
                }
                if let Some(id) = p.session_id.as_ref().or(p.id.as_ref()) {
                    sid = id.clone();
                }
            }
            Some("event_msg") if p.kind.as_deref() == Some("token_count") => {
                let Some(info) = p.info else { continue };
                let (Some(last), Some(total)) = (info.last_token_usage, info.total_token_usage)
                else {
                    continue;
                };
                if last.input_tokens + last.output_tokens + last.reasoning_output_tokens == 0 {
                    continue;
                }
                let ts = l.timestamp.clone().unwrap_or_default();
                // (session, instant, cumulative total) identifies one event —
                // the same event is stored in both sessions/ and
                // archived_sessions/
                let key = format!("{sid}:{ts}:{}", total.total_tokens);
                out.push((
                    key,
                    Call {
                        source: "codex",
                        session: sid.clone(),
                        model: model.clone(),
                        ts: DateTime::parse_from_rfc3339(&ts)
                            .ok()
                            .map(|t| t.with_timezone(&Utc)),
                        usage: Usage {
                            input: last.input_tokens.saturating_sub(last.cached_input_tokens),
                            cached: last.cached_input_tokens,
                            output: last.output_tokens + last.reasoning_output_tokens,
                        },
                        estimated: false,
                    },
                ));
            }
            _ => {}
        }
    }
    // token events can precede the first turn_context (truncated rollouts);
    // attribute them to the file's first observed model
    if model != "unknown" {
        for (_, c) in &mut out {
            if c.model == "unknown" {
                c.model = model.clone();
            }
        }
    }
    out
}

pub fn load(dirs: &[PathBuf]) -> Result<SourceOut> {
    let mut files = Vec::new();
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        files.extend(
            walkdir::WalkDir::new(dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
                        && e.path().extension().is_some_and(|x| x == "jsonl")
                })
                .map(|e| e.into_path()),
        );
    }

    let per_file: Vec<Vec<(String, Call)>> = files.par_iter().map(|p| parse_file(p)).collect();

    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut dupes = 0usize;
    for entries in per_file {
        for (key, c) in entries {
            if seen.insert(key) {
                calls.push(c);
            } else {
                dupes += 1;
            }
        }
    }

    let mut note = format!("codex: {} rollout files", files.len());
    if dupes > 0 {
        note.push_str(&format!(" · {dupes} dupes skipped"));
    }
    Ok(SourceOut { calls, note })
}
