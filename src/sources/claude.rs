//! Claude Code: `~/.claude/projects/**/*.jsonl` (incl. `subagents/`).
//!
//! Each `type:"assistant"` line is one API response carrying
//! `message.model` and `message.usage`{input_tokens,
//! cache_creation_input_tokens, cache_read_input_tokens, output_tokens}.
//! Cache writes are billed as input; `cache_read` is the discounted part.

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

/// `(dedup_key, call)` pairs found in one transcript file.
fn parse_file(path: &Path) -> Vec<(String, Call)> {
    let file_stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut out = Vec::new();
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return out,
    };
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if !line.contains("\"assistant\"") {
            continue;
        }
        let l: Line = match serde_json::from_str(&line) {
            Ok(l) => l,
            Err(_) => continue,
        };
        if l.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(msg) = l.message else { continue };
        let (Some(model), Some(u)) = (msg.model, msg.usage) else {
            continue;
        };
        // "<synthetic>" placeholders and zero-usage records carry no call
        if model.starts_with('<')
            || (u.input_tokens
                + u.cache_creation_input_tokens
                + u.cache_read_input_tokens
                + u.output_tokens)
                == 0
        {
            continue;
        }
        // msg.id + requestId identify an API response; the same response is
        // copied into later transcript files on resume/compact, so the dedup
        // key must not include the file name.
        let key = match (&msg.id, &l.request_id) {
            (Some(m), Some(r)) => format!("{m}:{r}"),
            _ => format!("{file_stem}:{}", l.uuid.as_deref().unwrap_or("")),
        };
        out.push((
            key,
            Call {
                source: "claude",
                session: l.session_id.unwrap_or_else(|| file_stem.clone()),
                model,
                ts: l
                    .timestamp
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Utc)),
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
    }
    out
}

pub fn load(dir: &Path) -> Result<SourceOut> {
    let files: Vec<PathBuf> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .map(|e| e.into_path())
        .collect();

    let per_file: Vec<Vec<(String, Call)>> = files.par_iter().map(|p| parse_file(p)).collect();

    let mut seen = HashSet::new();
    let mut calls = Vec::new();
    let mut skipped_dupes = 0usize;
    for entries in per_file {
        for (key, c) in entries {
            if seen.insert(key) {
                calls.push(c);
            } else {
                skipped_dupes += 1;
            }
        }
    }

    let mut note = format!("claude: {} transcript files", files.len());
    if skipped_dupes > 0 {
        note.push_str(&format!(" · {skipped_dupes} dupes skipped"));
    }
    Ok(SourceOut { calls, note })
}
