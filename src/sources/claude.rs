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
use indicatif::MultiProgress;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::report::Usage;
use crate::sources::SourceOut;
use crate::sources::filecache::{self, CachedCall, Dict, Jsonl};

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

/// Claude Code's JSONL shape — no parser state to carry across resumes.
pub struct Claude;

/// Parse `path` from `offset`, extending `dict` with new strings. Returns
/// (consumed bytes, dict, new tail entries).
fn parse_file(
    path: &Path,
    offset: u64,
    dict: Vec<String>,
) -> std::io::Result<(u64, Vec<String>, Vec<CachedCall>)> {
    let file_stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut dict = Dict::from_vec(dict);
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
                (Some(m), Some(r)) => filecache::key_of(&[m.as_bytes(), r.as_bytes()]),
                _ => filecache::key_of(&[
                    file_stem.as_bytes(),
                    l.uuid.as_deref().unwrap_or("").as_bytes(),
                ]),
            };
            entries.push(CachedCall {
                key,
                session: dict.intern(l.session_id.as_deref().unwrap_or(&file_stem)),
                model: dict.intern(&model),
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
            });
        },
    )?;
    Ok((consumed, dict.into_strings(), entries))
}

impl Jsonl for Claude {
    type State = ();
    const SOURCE: &'static str = "claude";

    fn fresh(_path: &Path) {}

    fn parse(
        path: &Path,
        offset: u64,
        (): (),
        dict: Vec<String>,
    ) -> std::io::Result<(u64, (), Vec<String>, Vec<CachedCall>)> {
        parse_file(path, offset, dict).map(|(c, d, e)| (c, (), d, e))
    }
}

/// Live-capable scanner: first `tick` is the full incremental scan, later
/// ticks emit only newly appended calls.
pub type Scanner = filecache::Scanner<Claude>;

/// `*.jsonl` under `dir`, as (path, len).
pub fn walk(dir: &Path) -> Vec<(PathBuf, u64)> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| e.metadata().ok().map(|m| (e.into_path(), m.len())))
        .collect()
}

pub fn load(dir: &Path, mp: &MultiProgress) -> Result<SourceOut> {
    let t0 = std::time::Instant::now();
    let mut sc = Scanner::open(vec![dir.to_path_buf()]);
    let found = walk(dir);
    let t = sc.tick(&found, mp);
    sc.save();
    tracing::debug!(
        files = t.files,
        parsed = t.parsed,
        bytes = t.bytes,
        calls = t.calls.len(),
        dupes = t.dupes,
        elapsed = ?t0.elapsed(),
        "claude source"
    );
    let mut note = format!(
        "claude: {} transcript files · {} reparsed",
        t.files, t.parsed
    );
    if t.dupes > 0 {
        note.push_str(&format!(" · {} dupes skipped", t.dupes));
    }
    Ok(SourceOut {
        calls: t.calls,
        note,
    })
}
