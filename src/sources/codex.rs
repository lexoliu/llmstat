//! Codex CLI: `~/.codex/sessions/**/rollout-*.jsonl` and
//! `~/.codex/archived_sessions/*.jsonl`.
//!
//! `event_msg`/`token_count` payloads carry `last_token_usage` (per-call
//! delta) plus cumulative `total_token_usage`; the delta is what we count.
//! `session_meta`/`turn_context` are marked by the top-level `type` and hold
//! the session id / current model.
//!
//! Files are append-only: parsed entries are cached in
//! `~/.cache/llmstat/codex-files.bin` and later runs only read appended
//! tails (see `filecache`). The cached `State` carries the current
//! model/session id so a tail parse attributes calls correctly.

use anyhow::Result;
use chrono::DateTime;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::report::{Call, Usage};
use crate::sources::SourceOut;
use crate::sources::filecache::{self, CachedCall, Entry, Plan};

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

/// Parser state needed to resume mid-file.
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    model: String,
    sid: String,
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

/// (dedup key, call) pairs parsed from a file region.
type Entries = Vec<(String, CachedCall)>;

/// Parse `path` starting at `offset` with `state`, returning
/// (consumed bytes, final state, new tail entries).
fn parse_file(
    path: &Path,
    offset: u64,
    mut state: State,
) -> std::io::Result<(u64, State, Entries)> {
    let mut entries = Vec::new();
    let consumed = filecache::read_lines(
        path,
        offset,
        |s| serde_json::from_str::<Line>(s).is_ok(),
        |line| {
            if !line.contains("\"model\"")
                && !line.contains("token_count")
                && !line.contains("session_meta")
            {
                return;
            }
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                return;
            };
            // session_meta / turn_context are marked by the top-level `type`;
            // token_count rides inside an `event_msg` payload
            let Some(p) = l.payload else { return };
            match l.kind.as_deref() {
                Some("session_meta") | Some("turn_context") => {
                    if let Some(m) = &p.model {
                        state.model = m.clone();
                    }
                    if let Some(id) = p.session_id.as_ref().or(p.id.as_ref()) {
                        state.sid = id.clone();
                    }
                }
                Some("event_msg") if p.kind.as_deref() == Some("token_count") => {
                    let Some(info) = p.info else { return };
                    let (Some(last), Some(total)) = (info.last_token_usage, info.total_token_usage)
                    else {
                        return;
                    };
                    if last.input_tokens + last.output_tokens + last.reasoning_output_tokens == 0 {
                        return;
                    }
                    let ts = l.timestamp.clone().unwrap_or_default();
                    // (session, instant, cumulative total) identifies one
                    // event — the same event is stored in both sessions/ and
                    // archived_sessions/
                    let key = format!("{}:{ts}:{}", state.sid, total.total_tokens);
                    entries.push((
                        key,
                        CachedCall {
                            session: state.sid.clone(),
                            model: state.model.clone(),
                            ts: DateTime::parse_from_rfc3339(&ts)
                                .ok()
                                .map(|t| t.timestamp()),
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
        },
    )?;
    Ok((consumed, state, entries))
}

enum Outcome {
    Reuse,
    Resumed(u64, State, Entries),
    Full(u64, State, Entries),
}

pub fn load(dirs: &[PathBuf]) -> Result<SourceOut> {
    let mut cache: HashMap<PathBuf, Entry<State>> = filecache::load("codex-files");

    let mut files: Vec<(PathBuf, u64)> = Vec::new();
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
                .filter_map(|e| e.metadata().ok().map(|m| (e.into_path(), m.len()))),
        );
    }

    let results: Vec<(PathBuf, Option<Outcome>)> = files
        .par_iter()
        .map(|(path, len)| {
            let fresh = || State {
                model: "unknown".into(),
                sid: session_id_of(path),
            };
            let oc = match filecache::plan(path, *len, cache.get(path)) {
                Plan::Reuse => Some(Outcome::Reuse),
                Plan::Resume(offset, st) => parse_file(path, offset, st)
                    .ok()
                    .map(|(c, s, e)| Outcome::Resumed(c, s, e)),
                Plan::Full => parse_file(path, 0, fresh())
                    .ok()
                    .map(|(c, s, e)| Outcome::Full(c, s, e)),
            };
            (path.clone(), oc)
        })
        .collect();

    let mut new_cache: HashMap<PathBuf, Entry<State>> = HashMap::with_capacity(results.len());
    let mut parsed_new = 0usize;
    for (path, oc) in results {
        let Some(oc) = oc else { continue };
        let (offset, state, mut tail, keep_old) = match oc {
            Outcome::Reuse => {
                if let Some(e) = cache.remove(&path) {
                    new_cache.insert(path, e);
                }
                continue;
            }
            Outcome::Resumed(o, s, t) => (o, s, t, true),
            Outcome::Full(o, s, t) => (o, s, t, false),
        };
        let mut entries = if keep_old {
            cache.remove(&path).map(|e| e.entries).unwrap_or_default()
        } else {
            cache.remove(&path);
            Vec::new()
        };
        entries.append(&mut tail);
        // token events can precede the first turn_context (truncated
        // rollouts); attribute them to the file's final model
        if state.model != "unknown" {
            for (_, c) in &mut entries {
                if c.model == "unknown" {
                    c.model = state.model.clone();
                }
            }
        }
        new_cache.insert(
            path.clone(),
            Entry {
                offset,
                mtime: filecache::mtime(&path),
                boundary: filecache::boundary(&path, offset),
                state,
                entries,
            },
        );
        parsed_new += 1;
    }
    // rewrite the (large) cache file only when something changed —
    // leftovers in `cache` are files deleted since the last run
    if parsed_new > 0 || !cache.is_empty() {
        filecache::save("codex-files", &new_cache);
    }

    // iterate in walk order — deterministic across runs
    let mut seen = HashSet::new();
    let mut calls: Vec<Call> = Vec::new();
    let mut dupes = 0usize;
    for (path, _) in &files {
        let Some(e) = new_cache.get(path) else {
            continue;
        };
        for (key, c) in &e.entries {
            if seen.insert(key.clone()) {
                calls.push(c.to_call("codex"));
            } else {
                dupes += 1;
            }
        }
    }

    let mut note = format!(
        "codex: {} rollout files · {parsed_new} reparsed",
        files.len()
    );
    if dupes > 0 {
        note.push_str(&format!(" · {dupes} dupes skipped"));
    }
    Ok(SourceOut { calls, note })
}
