//! Claude Code: `~/.claude/projects/**/*.jsonl` (incl. `subagents/`).
//!
//! Each `type:"assistant"` line is one API response carrying
//! `message.model` and `message.usage`{input_tokens,
//! cache_creation_input_tokens, cache_read_input_tokens, output_tokens}.
//! Cache writes are billed as input; `cache_read` is the discounted part.
//!
//! Files are append-only: parsed entries are cached in
//! `~/.cache/llmstat/claude-files.bin` and later runs only read appended
//! tails (see `filecache`).

use anyhow::Result;
use chrono::DateTime;
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::report::Usage;
use crate::sources::SourceOut;
use crate::sources::filecache::{self, CachedCall, Entry, Plan};

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<Msg>,
    uuid: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<U>,
}

#[derive(Deserialize)]
struct U {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

pub fn default_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".claude/projects")
}

/// Parse `path` from `offset`, returning (consumed bytes, new tail entries).
fn parse_file(path: &Path, offset: u64) -> std::io::Result<(u64, Vec<(String, CachedCall)>)> {
    let file_stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut entries = Vec::new();
    let consumed = filecache::read_lines(
        path,
        offset,
        |s| serde_json::from_str::<Line>(s).is_ok(),
        |line| {
            if !line.contains("\"assistant\"") {
                return;
            }
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                return;
            };
            if l.kind.as_deref() != Some("assistant") {
                return;
            }
            let Some(msg) = l.message else { return };
            let (Some(model), Some(u)) = (msg.model, msg.usage) else {
                return;
            };
            // "<synthetic>" placeholders and zero-usage records carry no call
            if model.starts_with('<')
                || (u.input_tokens
                    + u.cache_creation_input_tokens
                    + u.cache_read_input_tokens
                    + u.output_tokens)
                    == 0
            {
                return;
            }
            // msg.id + requestId identify an API response; the same response
            // is copied into later transcript files on resume/compact, so
            // the dedup key must not include the file name.
            let key = match (&msg.id, &l.request_id) {
                (Some(m), Some(r)) => format!("{m}:{r}"),
                _ => format!("{file_stem}:{}", l.uuid.as_deref().unwrap_or("")),
            };
            entries.push((
                key,
                CachedCall {
                    session: l.session_id.unwrap_or_else(|| file_stem.clone()),
                    model,
                    ts: l
                        .timestamp
                        .as_deref()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| t.timestamp()),
                    usage: Usage {
                        // cache-write tokens bill as input; cache-read is the
                        // discounted subset
                        input: u.input_tokens + u.cache_creation_input_tokens,
                        cached: u.cache_read_input_tokens,
                        output: u.output_tokens,
                    },
                    estimated: false,
                },
            ));
        },
    )?;
    Ok((consumed, entries))
}

/// What the scan produced for one file.
enum Outcome {
    Reuse,
    /// Parsed from `offset`; `entries` are only the new tail.
    Resumed(u64, Vec<(String, CachedCall)>),
    /// Parsed from 0; `entries` replace any cached ones.
    Full(u64, Vec<(String, CachedCall)>),
}

pub fn load(dir: &Path) -> Result<SourceOut> {
    let mut cache: HashMap<PathBuf, Entry<()>> = filecache::load("claude-files");

    let files: Vec<(PathBuf, u64)> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().map(|m| (e.into_path(), m.len())))
        .collect();

    let results: Vec<(PathBuf, Option<Outcome>)> = files
        .par_iter()
        .map(|(path, len)| {
            let oc = match filecache::plan(path, *len, cache.get(path)) {
                Plan::Reuse => Some(Outcome::Reuse),
                Plan::Resume(offset, ()) => parse_file(path, offset)
                    .ok()
                    .map(|(c, e)| Outcome::Resumed(c, e)),
                Plan::Full => parse_file(path, 0).ok().map(|(c, e)| Outcome::Full(c, e)),
            };
            (path.clone(), oc)
        })
        .collect();

    let mut new_cache: HashMap<PathBuf, Entry<()>> = HashMap::with_capacity(results.len());
    let mut parsed_new = 0usize;
    for (path, oc) in results {
        let Some(oc) = oc else { continue }; // unreadable file: dropped
        let (offset, mut tail, keep_old) = match oc {
            Outcome::Reuse => {
                if let Some(e) = cache.remove(&path) {
                    new_cache.insert(path, e);
                }
                continue;
            }
            Outcome::Resumed(o, t) => (o, t, true),
            Outcome::Full(o, t) => (o, t, false),
        };
        let mut entries = if keep_old {
            cache.remove(&path).map(|e| e.entries).unwrap_or_default()
        } else {
            cache.remove(&path);
            Vec::new()
        };
        entries.append(&mut tail);
        new_cache.insert(
            path.clone(),
            Entry {
                offset,
                mtime: filecache::mtime(&path),
                boundary: filecache::boundary(&path, offset),
                state: (),
                entries,
            },
        );
        parsed_new += 1;
    }
    // rewrite the cache only when something changed — leftovers in `cache`
    // are files deleted since the last run
    if parsed_new > 0 || !cache.is_empty() {
        filecache::save("claude-files", &new_cache);
    }

    // iterate in walk order — deterministic across runs
    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut skipped_dupes = 0usize;
    for (path, _) in &files {
        let Some(e) = new_cache.get(path) else {
            continue;
        };
        for (key, c) in &e.entries {
            if seen.insert(key.clone()) {
                calls.push(c.to_call("claude"));
            } else {
                skipped_dupes += 1;
            }
        }
    }

    let mut note = format!(
        "claude: {} transcript files · {parsed_new} reparsed",
        files.len()
    );
    if skipped_dupes > 0 {
        note.push_str(&format!(" · {skipped_dupes} dupes skipped"));
    }
    Ok(SourceOut { calls, note })
}
