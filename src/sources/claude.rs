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
use crate::sources::filecache::{self, CachedCall, Dict, Entry, Jsonl};

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
    /// Human-readable session slug, present on many record types.
    slug: Option<String>,
    /// Generated session title on `ai-title` records.
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    /// Task title on `agent-name` records.
    #[serde(rename = "agentName")]
    agent_name: Option<String>,
    /// Sidechain records belong to subagents, not the session's task.
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
}

#[derive(Deserialize)]
struct Msg {
    id: Option<String>,
    model: Option<String>,
    usage: Option<U>,
    /// `user` records carry the prompt in `message.content` — a string for
    /// real prompts, an array for tool results.
    content: Option<serde_json::Value>,
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

/// Claude Code's JSONL shape.
pub struct Claude;

/// Parser state needed to resume mid-file: the session's display name is
/// discovered on non-assistant records, possibly after calls were emitted.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// `aiTitle`/`agentName` — a generated task title.
    title: Option<String>,
    /// First real user prompt — next best name.
    prompt: Option<String>,
    /// `slug` — generated word-triplet, still better than a uuid.
    slug: Option<String>,
}

/// Parse `path` from `offset`, extending `dict` with new strings. Returns
/// (consumed bytes, final state, dict, new tail entries).
fn parse_file(
    path: &Path,
    offset: u64,
    mut state: State,
    dict: Vec<String>,
) -> std::io::Result<(u64, State, Vec<String>, Vec<CachedCall>)> {
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
            if !line.contains("\"assistant\"")
                && !line.contains("slug")
                && !line.contains("agent-name")
                && !line.contains("ai-title")
                && !line.contains("\"type\":\"user\"")
            {
                return;
            }
            let Ok(l) = serde_json::from_str::<Line>(line) else {
                return;
            };
            if let Some(t) = l.ai_title.or(l.agent_name) {
                state.title.get_or_insert(t);
            }
            if let Some(s) = l.slug {
                state.slug.get_or_insert(s);
            }
            if l.kind.as_deref() == Some("user")
                && state.prompt.is_none()
                && l.is_sidechain != Some(true)
                && let Some(m) = &l.message
                && let Some(serde_json::Value::String(s)) = &m.content
            {
                let t = super::titleize(s);
                if !t.is_empty() && !t.starts_with('<') {
                    state.prompt = Some(t);
                }
            }
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
                // filled by `fixup` once the name record is seen
                session_name: None,
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
    Ok((consumed, state, dict.into_strings(), entries))
}

impl Jsonl for Claude {
    type State = State;
    const SOURCE: &'static str = "claude";

    fn fresh(_path: &Path) -> State {
        State::default()
    }

    fn parse(
        path: &Path,
        offset: u64,
        state: State,
        dict: Vec<String>,
    ) -> std::io::Result<(u64, State, Vec<String>, Vec<CachedCall>)> {
        parse_file(path, offset, state, dict)
    }

    /// The session name may be discovered after calls were parsed — stamp
    /// it onto every entry once the file tail is in. Priority: generated
    /// title, then first user prompt, then the word-triplet slug.
    fn fixup(e: &mut Entry<State>) {
        let Some(name) = e
            .state
            .title
            .as_ref()
            .or(e.state.prompt.as_ref())
            .or(e.state.slug.as_ref())
        else {
            return;
        };
        let i = filecache::dict_get_or_push(&mut e.dict, name);
        for c in &mut e.entries {
            c.session_name = Some(i);
        }
    }
}

/// Monitor-capable scanner: first `tick` is the full incremental scan, later
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
