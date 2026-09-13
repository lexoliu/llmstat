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
use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::report::Usage;
use crate::sources::SourceOut;
use crate::sources::filecache::{self, CachedCall, Dict, Entry, Jsonl};

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
pub struct State {
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

/// Codex CLI's JSONL shape.
pub struct Codex;

/// Calls parsed from a file region (dict indices into the entry's dict).
type Entries = Vec<CachedCall>;

/// Parse `path` starting at `offset` with `state`, extending `dict`.
/// Returns (consumed bytes, final state, dict, new tail entries).
fn parse_file(
    path: &Path,
    offset: u64,
    mut state: State,
    dict: Vec<String>,
) -> std::io::Result<(u64, State, Vec<String>, Entries)> {
    let mut dict = Dict::from_vec(dict);
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
                    let total_b = total.total_tokens.to_le_bytes();
                    let key = filecache::key_of(&[state.sid.as_bytes(), ts.as_bytes(), &total_b]);
                    entries.push(CachedCall {
                        key,
                        session: dict.intern(&state.sid),
                        model: dict.intern(&state.model),
                        ts: DateTime::parse_from_rfc3339(&ts)
                            .ok()
                            .map(|t| t.timestamp()),
                        usage: Usage {
                            input: last.input_tokens.saturating_sub(last.cached_input_tokens),
                            cached: last.cached_input_tokens,
                            output: last.output_tokens + last.reasoning_output_tokens,
                        },
                        estimated: false,
                    });
                }
                _ => {}
            }
        },
    )?;
    Ok((consumed, state, dict.into_strings(), entries))
}
impl Jsonl for Codex {
    type State = State;
    const SOURCE: &'static str = "codex";

    fn fresh(path: &Path) -> State {
        State {
            model: "unknown".into(),
            sid: session_id_of(path),
        }
    }

    fn parse(
        path: &Path,
        offset: u64,
        state: State,
        dict: Vec<String>,
    ) -> std::io::Result<(u64, State, Vec<String>, Entries)> {
        parse_file(path, offset, state, dict)
    }

    /// Token events can precede the first turn_context (truncated
    /// rollouts); attribute them to the file's final model.
    fn fixup(e: &mut Entry<State>) {
        if e.state.model == "unknown" {
            return;
        }
        let Some(u) = e.dict.iter().position(|s| s == "unknown") else {
            return;
        };
        let m = match e.dict.iter().position(|s| s == &e.state.model) {
            Some(i) => i as u32,
            None => {
                e.dict.push(e.state.model.clone());
                (e.dict.len() - 1) as u32
            }
        };
        for c in &mut e.entries {
            if c.model == u as u32 {
                c.model = m;
            }
        }
    }
}

/// Live-capable scanner: first `tick` is the full incremental scan, later
/// ticks emit only newly appended calls.
pub type Scanner = filecache::Scanner<Codex>;

/// `rollout-*.jsonl` under each session dir, as (path, len).
pub fn walk(dirs: &[PathBuf]) -> Vec<(PathBuf, u64)> {
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
                .filter_map(|e| e.metadata().ok().map(|m| (e.into_path(), m.len()))),
        );
    }
    files
}

pub fn load(dirs: &[PathBuf], mp: &MultiProgress) -> Result<SourceOut> {
    let t0 = std::time::Instant::now();
    let mut sc = Scanner::open(dirs.to_vec());
    let found = walk(dirs);
    let t = sc.tick(&found, mp);
    sc.save();
    tracing::debug!(
        files = t.files,
        parsed = t.parsed,
        bytes = t.bytes,
        calls = t.calls.len(),
        dupes = t.dupes,
        elapsed = ?t0.elapsed(),
        "codex source"
    );
    let mut note = format!("codex: {} rollout files · {} reparsed", t.files, t.parsed);
    if t.dupes > 0 {
        note.push_str(&format!(" · {} dupes skipped", t.dupes));
    }
    Ok(SourceOut {
        calls: t.calls,
        note,
    })
}
