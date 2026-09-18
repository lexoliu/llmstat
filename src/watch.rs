//! `llmstat watch` — tiled, read-only tails of every running agent session.
//!
//! Live detection per source:
//! - Devin: `session_locks/<session>.lock` is an advisory `flock` held by the
//!   serving process; a non-blocking try-lock failing means the session is
//!   running (the OS releases it on exit — no PID reuse issues). Output is
//!   tailed from sessions.db `message_nodes`.
//! - Claude: `~/.claude/sessions/<pid>.json` registers each running session;
//!   the PID is verified alive, then the matching
//!   `projects/**/<sessionId>.jsonl` is tailed.
//! - Codex: `rollout-*.jsonl` files with a fresh mtime are live; tailed by
//!   byte offset like the filecache scanner.

use anyhow::Result;
use chrono::{TimeZone, Utc};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Lines of scrollback kept per pane.
const CAP: usize = 1000;
/// A codex rollout is live while its mtime is this fresh.
const CODEX_FRESH: Duration = Duration::from_secs(300);
/// An ended pane lingers this long before being dropped.
const ENDED_TTL: Duration = Duration::from_secs(120);
/// Never show more panes than this; the most recently active win.
const MAX_PANES: usize = 24;
/// Baseline rows fetched when attaching to a live session.
const BACKFILL: i64 = 60;
/// Content pulled in on each tail call (start ~64KiB back for context).
const TAIL_BYTES: u64 = 64 << 10;

/// One normalized stream item emitted by a source tailer.
enum Ev {
    /// User prompt / interjection.
    Prompt(String),
    /// Assistant message text.
    Text(String),
    /// Reasoning / thinking summary.
    Think(String),
    /// Tool invocation one-liner (`sub` = claude subagent sidechain).
    Tool {
        name: String,
        detail: String,
        sub: bool,
    },
    /// Tool result excerpt.
    Output(String),
}

/// What a source learned about one session this tick.
pub(crate) struct FeedMsg {
    src: &'static str,
    /// Canonical session id within the source.
    id: String,
    /// Display name; `id` shown when absent.
    title: Option<String>,
    /// Process still running.
    live: bool,
    evts: Vec<Ev>,
    /// Latest event timestamp (unix seconds); 0 = unknown.
    last_ts: i64,
}

/// A source of live-agent streams — `poll` reports every session it knows
/// about, `live` flag included, so callers see ends as well as starts.
pub(crate) trait Source {
    fn poll(&mut self, out: &mut Vec<FeedMsg>);
}

/// Whitespace-collapsed, `max`-chars-truncated preview of `s`.
fn oneline(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut it = flat.chars();
    let head: String = it.by_ref().take(max).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Most identifying argument of a tool call, as a one-line preview.
fn arg_detail(v: &serde_json::Value) -> String {
    let Some(map) = v.as_object() else {
        return v.as_str().map(|s| oneline(s, 90)).unwrap_or_default();
    };
    for k in [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "subject",
        "description",
        "query",
        "url",
        "skill",
        "task",
        "to",
        "name",
        "title",
        "prompt",
        "message",
    ] {
        if let Some(s) = map.get(k).and_then(|x| x.as_str())
            && !s.trim().is_empty()
        {
            return oneline(s, 90);
        }
    }
    // no known key: first short string value, else compact json
    for x in map.values() {
        if let Some(s) = x.as_str()
            && !s.trim().is_empty()
        {
            return oneline(s, 90);
        }
    }
    oneline(&serde_json::to_string(map).unwrap_or_default(), 90)
}

/// Extract visible text from a content field that may be a string or an
/// array of typed parts (`{type: "text"/"input_text"/"output_text"}`).
fn content_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| p.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

// ── Devin: flock-held session locks + sessions.db tail ─────────────────────

/// `chat_message` rows in `message_nodes`.
#[derive(Deserialize)]
struct ChatMsg {
    message_id: Option<String>,
    role: Option<String>,
    content: Option<serde_json::Value>,
    tool_calls: Option<Vec<ToolCall>>,
    thinking: Option<serde_json::Value>,
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct ToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: Option<serde_json::Value>,
}

/// Try-lock probe: a held flock means the lock's owner process is alive
/// (the OS drops the lock on exit, so this never goes stale).
#[cfg(unix)]
fn lock_held(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    // EWOULDBLOCK = another process holds it. If we acquire it ourselves
    // the fd drop at scope end releases it again immediately.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    rc != 0
}

/// Is `pid` a live process named like `want` ("claude", "devin")? The name
/// check defeats PID reuse by unrelated processes.
#[cfg(unix)]
fn pid_alive_named(pid: u32, want: &str) -> bool {
    // kill(pid, 0): 0 = alive, EPERM = alive but not ours.
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return false;
    }
    proc_name(pid).is_none_or(|n| n.contains(want))
}

#[cfg(target_os = "macos")]
fn proc_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).to_string())
}

#[cfg(target_os = "linux")]
fn proc_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(not(any(unix, target_os = "linux")))]
fn pid_alive_named(_pid: u32, _want: &str) -> bool {
    true
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn proc_name(_pid: u32) -> Option<String> {
    None
}

/// Per-session tail position in `message_nodes`.
struct DevinCursor {
    /// First row_id not yet consumed.
    next: i64,
    /// Recently emitted message_ids — each message is stored twice
    /// (with/without metadata).
    mids: VecDeque<String>,
    /// tool_call id -> tool name, so results render `⎿ name`.
    tools: HashMap<String, String>,
}

pub(crate) struct Devin {
    conn: rusqlite::Connection,
    locks: PathBuf,
    curs: HashMap<String, DevinCursor>,
    /// session -> title from the sessions table.
    titles: HashMap<String, Option<String>>,
    /// session -> last_activity_at (non-unix liveness fallback).
    activity: HashMap<String, i64>,
    /// Sessions whose end was already reported — skipped until they revive
    /// (lock re-held), so an ended pane isn't resurrected by repeat updates.
    dead: HashSet<String>,
}

impl Devin {
    pub(crate) fn open(db_path: &Path) -> Result<Self> {
        Ok(Self {
            conn: crate::sources::devin::db::open(db_path)?,
            locks: db_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("session_locks"),
            curs: HashMap::new(),
            titles: HashMap::new(),
            activity: HashMap::new(),
            dead: HashSet::new(),
        })
    }

    /// Sessions whose lock is currently held. On unix the flock probe is
    /// definitive; elsewhere fall back to recent `last_activity_at`.
    fn live_ids(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        if let Ok(rd) = std::fs::read_dir(&self.locks) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "lock") {
                    continue;
                }
                let Some(id) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                    continue;
                };
                #[cfg(unix)]
                let live = lock_held(&p);
                #[cfg(not(unix))]
                let live = {
                    let now = Utc::now().timestamp();
                    self.activity.get(&id).is_some_and(|t| now - t < 120)
                };
                if live {
                    out.insert(id);
                }
            }
        }
        out
    }

    /// Refresh the titles/activity maps (sessions table is small enough to
    /// re-read each tick).
    fn refresh_meta(&mut self) {
        if let Ok(mut st) = self
            .conn
            .prepare("SELECT id, title, last_activity_at FROM sessions")
            && let Ok(rows) = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
        {
            for r in rows.flatten() {
                self.titles.insert(r.0.clone(), r.1);
                self.activity.insert(r.0, r.2);
            }
        }
    }

    /// Convert one `chat_message` row into events.
    fn msg_events(cur: &mut DevinCursor, json: &str) -> Vec<Ev> {
        let Ok(m) = serde_json::from_str::<ChatMsg>(json) else {
            return Vec::new();
        };
        // the db stores each message twice (with and without metadata)
        if let Some(mid) = &m.message_id {
            if cur.mids.contains(mid) {
                return Vec::new();
            }
            if cur.mids.len() >= 128 {
                cur.mids.pop_front();
            }
            cur.mids.push_back(mid.clone());
        }
        let mut out = Vec::new();
        match m.role.as_deref() {
            Some("user") => {
                let t = m.content.as_ref().map(content_text).unwrap_or_default();
                if !t.trim().is_empty() && !t.starts_with('<') {
                    out.push(Ev::Prompt(t));
                }
            }
            Some("assistant") => {
                if let Some(th) = &m.thinking {
                    let t = th
                        .get("thinking")
                        .and_then(|x| x.as_str())
                        .or_else(|| th.as_str())
                        .unwrap_or_default();
                    if !t.trim().is_empty() {
                        out.push(Ev::Think(t.to_string()));
                    }
                }
                let t = m.content.as_ref().map(content_text).unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(Ev::Text(t));
                }
                for tc in m.tool_calls.iter().flatten() {
                    let name = tc.name.clone().unwrap_or_else(|| "?".into());
                    if let Some(id) = &tc.id {
                        cur.tools.insert(id.clone(), name.clone());
                    }
                    out.push(Ev::Tool {
                        detail: tc.arguments.as_ref().map(arg_detail).unwrap_or_default(),
                        name,
                        sub: false,
                    });
                }
            }
            Some("tool") => {
                let t = m.content.as_ref().map(content_text).unwrap_or_default();
                if !t.trim().is_empty() {
                    let name = m
                        .tool_call_id
                        .as_ref()
                        .and_then(|id| cur.tools.get(id))
                        .cloned();
                    out.push(Ev::Output(match name {
                        Some(n) => format!("{n}: {}", oneline(&t, 300)),
                        None => t,
                    }));
                }
            }
            _ => {}
        }
        out
    }

    /// Attach: emit the last `BACKFILL` rows so the pane opens with context.
    fn attach(&mut self, id: &str) -> Result<(DevinCursor, Vec<(i64, String)>)> {
        let mut st = self.conn.prepare(
            "SELECT row_id, chat_message, created_at FROM message_nodes \
             WHERE session_id = ?1 ORDER BY row_id DESC LIMIT ?2",
        )?;
        let mut rows: Vec<(i64, String, i64)> = st
            .query_map(rusqlite::params![id, BACKFILL], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        // Empty history (brand-new session): start at the global tail —
        // new rows always land above it (rowid is autoincrement).
        let next = match rows.last() {
            Some((r, _, _)) => r + 1,
            None => self.conn.query_row(
                "SELECT COALESCE(MAX(row_id), 0) + 1 FROM message_nodes",
                [],
                |r| r.get(0),
            )?,
        };
        Ok((
            DevinCursor {
                next,
                mids: VecDeque::new(),
                tools: HashMap::new(),
            },
            rows.into_iter().map(|(_, j, ts)| (ts, j)).collect(),
        ))
    }
}

impl Source for Devin {
    fn poll(&mut self, out: &mut Vec<FeedMsg>) {
        self.refresh_meta();
        let live = self.live_ids();
        // New sessions get a cursor seeded from the tail of their history.
        for id in &live {
            self.dead.remove(id);
            if self.curs.contains_key(id.as_str()) {
                continue;
            }
            match self.attach(id) {
                Ok((cur, rows)) => {
                    let mut cur = cur;
                    let mut evts = Vec::new();
                    let mut last_ts = 0;
                    for (ts, j) in rows {
                        last_ts = ts;
                        evts.extend(Self::msg_events(&mut cur, &j));
                    }
                    let title = self.titles.get(id).cloned().flatten();
                    out.push(FeedMsg {
                        src: "devin",
                        id: id.clone(),
                        title,
                        live: true,
                        evts,
                        last_ts,
                    });
                    self.curs.insert(id.clone(), cur);
                }
                Err(e) => tracing::warn!("devin tail attach {id}: {e:#}"),
            }
        }
        // Ongoing tails for every live session with a cursor. Dead ones are
        // skipped: their end was already reported and no more rows can come.
        for (id, cur) in self.curs.iter_mut() {
            let still = live.contains(id);
            if !still && self.dead.contains(id) {
                continue;
            }
            let mut evts = Vec::new();
            let mut last_ts = 0;
            // rowid range scan: reads only rows newer than this session's
            // cursor, filtered to the session — cheap even on a huge table.
            let res = self
                .conn
                .prepare(
                    "SELECT row_id, chat_message, created_at FROM message_nodes \
                     WHERE row_id >= ?1 AND session_id = ?2 ORDER BY row_id LIMIT 2000",
                )
                .and_then(|mut st| {
                    st.query_map(rusqlite::params![cur.next, id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
                });
            match res {
                Ok(rows) => {
                    for (rid, j, ts) in rows {
                        cur.next = rid + 1;
                        last_ts = ts;
                        evts.extend(Self::msg_events(cur, &j));
                    }
                }
                Err(e) => tracing::warn!("devin tail {id}: {e:#}"),
            }
            if !evts.is_empty() || !still {
                if !still {
                    self.dead.insert(id.clone());
                }
                out.push(FeedMsg {
                    src: "devin",
                    id: id.clone(),
                    title: self.titles.get(id).cloned().flatten(),
                    live: still,
                    evts,
                    last_ts,
                });
            }
        }
    }
}

// ── Claude: sessions/<pid>.json registry + projects/**/*.jsonl tail ─────────

/// `~/.claude/sessions/<pid>.json` — the live-session registry entry.
#[derive(Deserialize)]
struct ClaudeReg {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: Option<String>,
    name: Option<String>,
}

struct ClaudeTail {
    path: PathBuf,
    offset: u64,
    /// tool_use id -> name, so tool_result renders `⎿ name`.
    tools: HashMap<String, String>,
}

pub(crate) struct Claude {
    sessions_dir: PathBuf,
    projects: PathBuf,
    tails: HashMap<String, ClaudeTail>,
    /// sessionId -> registry display name.
    names: HashMap<String, String>,
    /// Sessions whose end was already reported — see `Devin::dead`.
    dead: HashSet<String>,
    /// sessionId -> when the transcript search last failed; the projects
    /// walk is expensive, so misses wait a few seconds before retrying.
    miss: HashMap<String, Instant>,
}

/// Retry delay after a transcript lookup miss.
const MISS_RETRY: Duration = Duration::from_secs(5);

impl Claude {
    /// `sessions_dir` = `~/.claude/sessions` (pid registry), `projects` =
    /// `~/.claude/projects` (transcript files).
    pub(crate) fn new(sessions_dir: PathBuf, projects: PathBuf) -> Self {
        Self {
            sessions_dir,
            projects,
            tails: HashMap::new(),
            names: HashMap::new(),
            dead: HashSet::new(),
            miss: HashMap::new(),
        }
    }
}

/// Claude's projects dir naming: every non-alphanumeric char becomes '-'.
fn claude_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Find `<sessionId>.jsonl` under the projects dir when the cwd-derived
/// path misses (renamed dirs, odd encodings).
fn find_claude_file(projects: &Path, sid: &str) -> Option<PathBuf> {
    let want = format!("{sid}.jsonl");
    walkdir::WalkDir::new(projects)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy() == want)
        .map(|e| e.into_path())
}

/// Parse one Claude transcript record into events.
fn claude_events(
    tools: &mut HashMap<String, String>,
    kind: &str,
    sidechain: bool,
    msg: &serde_json::Value,
) -> Vec<Ev> {
    let mut out = Vec::new();
    let content = msg
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match kind {
        "assistant" => {
            for p in content.as_array().into_iter().flatten() {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str())
                            && !t.trim().is_empty()
                        {
                            out.push(Ev::Text(t.to_string()));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = p.get("thinking").and_then(|t| t.as_str())
                            && !t.trim().is_empty()
                        {
                            out.push(Ev::Think(t.to_string()));
                        }
                    }
                    Some("tool_use") => {
                        let name = p
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("?")
                            .to_string();
                        if let Some(id) = p.get("id").and_then(|i| i.as_str()) {
                            tools.insert(id.to_string(), name.clone());
                        }
                        out.push(Ev::Tool {
                            detail: p.get("input").map(arg_detail).unwrap_or_default(),
                            name,
                            sub: sidechain,
                        });
                    }
                    _ => {}
                }
            }
        }
        "user" => match &content {
            serde_json::Value::String(s) => {
                if !s.trim().is_empty() && !s.starts_with('<') {
                    out.push(Ev::Prompt(s.clone()));
                }
            }
            serde_json::Value::Array(parts) => {
                for p in parts {
                    match p.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = p.get("text").and_then(|t| t.as_str())
                                && !t.trim().is_empty()
                                && !t.starts_with('<')
                            {
                                out.push(Ev::Prompt(t.to_string()));
                            }
                        }
                        Some("tool_result") => {
                            let t = p.get("content").map(content_text).unwrap_or_default();
                            if !t.trim().is_empty() {
                                let name = p
                                    .get("tool_use_id")
                                    .and_then(|i| i.as_str())
                                    .and_then(|id| tools.get(id))
                                    .cloned();
                                out.push(Ev::Output(match name {
                                    Some(n) => format!("{n}: {}", oneline(&t, 300)),
                                    None => t,
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
    out
}

impl Source for Claude {
    fn poll(&mut self, out: &mut Vec<FeedMsg>) {
        // Live session ids from the pid registry.
        let mut live: HashMap<String, ClaudeReg> = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(&self.sessions_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let Some(reg) = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|s| serde_json::from_str::<ClaudeReg>(&s).ok())
                else {
                    continue;
                };
                if pid_alive_named(reg.pid, "claude") {
                    live.insert(reg.session_id.clone(), reg);
                }
            }
        }

        // Attach tails for newly-live sessions.
        for (sid, reg) in &live {
            self.dead.remove(sid);
            self.names.insert(
                sid.clone(),
                reg.name.clone().unwrap_or_else(|| {
                    reg.cwd
                        .as_deref()
                        .and_then(|c| c.rsplit('/').next())
                        .unwrap_or(sid)
                        .to_string()
                }),
            );
            if self.tails.contains_key(sid) {
                continue;
            }
            // The session is alive even if its transcript hasn't resolved
            // yet — an empty pane beats invisibility.
            out.push(FeedMsg {
                src: "claude",
                id: sid.clone(),
                title: self.names.get(sid).cloned(),
                live: true,
                evts: Vec::new(),
                last_ts: 0,
            });
            if self.miss.get(sid).is_some_and(|t| t.elapsed() < MISS_RETRY) {
                continue;
            }
            let path = reg
                .cwd
                .as_deref()
                .map(|c| {
                    self.projects
                        .join(claude_dir_name(c))
                        .join(format!("{sid}.jsonl"))
                })
                .filter(|p| p.exists())
                .or_else(|| find_claude_file(&self.projects, sid));
            match path {
                Some(path) => {
                    self.miss.remove(sid);
                    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    self.tails.insert(
                        sid.clone(),
                        ClaudeTail {
                            path,
                            offset: len.saturating_sub(TAIL_BYTES),
                            tools: HashMap::new(),
                        },
                    );
                }
                None => {
                    self.miss.insert(sid.clone(), Instant::now());
                }
            }
        }

        // Tail every known session; report ended ones once.
        for (sid, t) in self.tails.iter_mut() {
            let still = live.contains_key(sid);
            if !still && self.dead.contains(sid) {
                continue;
            }
            let mut evts = Vec::new();
            let mut last_ts = 0i64;
            let tools = &mut t.tools;
            let _ = crate::sources::filecache::read_lines(
                &t.path,
                t.offset,
                |s| serde_json::from_str::<serde_json::Value>(s).is_ok(),
                |line| {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                        return;
                    };
                    let kind = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    if kind != "assistant" && kind != "user" {
                        return;
                    }
                    if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str())
                        && let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts)
                    {
                        last_ts = t.timestamp();
                    }
                    let side = v
                        .get("isSidechain")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false);
                    let msg = v.get("message").cloned().unwrap_or(serde_json::Value::Null);
                    evts.extend(claude_events(tools, kind, side, &msg));
                },
            )
            .map(|new| t.offset = new);
            if !evts.is_empty() || !still {
                if !still {
                    self.dead.insert(sid.clone());
                }
                out.push(FeedMsg {
                    src: "claude",
                    id: sid.clone(),
                    title: self.names.get(sid).cloned(),
                    live: still,
                    evts,
                    last_ts,
                });
            }
        }

        // Sessions that never got a tail still owe their pane an end notice.
        for sid in self.names.keys() {
            if !live.contains_key(sid)
                && !self.tails.contains_key(sid)
                && self.dead.insert(sid.clone())
            {
                out.push(FeedMsg {
                    src: "claude",
                    id: sid.clone(),
                    title: self.names.get(sid).cloned(),
                    live: false,
                    evts: Vec::new(),
                    last_ts: 0,
                });
            }
        }
    }
}

// ── Codex: fresh rollout-*.jsonl tails ──────────────────────────────────────

struct CodexTail {
    offset: u64,
    sid: String,
    /// item ids already emitted — items surface both as `response_item`
    /// and `item_completed`.
    seen: HashSet<String>,
}

pub(crate) struct Codex {
    dirs: Vec<PathBuf>,
    index_path: PathBuf,
    tails: HashMap<PathBuf, CodexTail>,
    /// session id -> thread name from session_index.jsonl.
    names: HashMap<String, String>,
    index_len: u64,
    /// Files whose end was already reported — see `Devin::dead`.
    dead: HashSet<PathBuf>,
}

impl Codex {
    pub(crate) fn new(dirs: Vec<PathBuf>, root: &Path) -> Self {
        Self {
            dirs,
            index_path: root.join("session_index.jsonl"),
            tails: HashMap::new(),
            names: HashMap::new(),
            index_len: 0,
            dead: HashSet::new(),
        }
    }

    /// Re-read session_index.jsonl when it grew.
    fn refresh_names(&mut self) {
        let Ok(meta) = std::fs::metadata(&self.index_path) else {
            return;
        };
        if meta.len() == self.index_len {
            return;
        }
        self.index_len = meta.len();
        if let Ok(text) = std::fs::read_to_string(&self.index_path) {
            for l in text.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(l)
                    && let (Some(id), Some(n)) = (
                        v.get("id").and_then(|x| x.as_str()),
                        v.get("thread_name").and_then(|x| x.as_str()),
                    )
                {
                    self.names.insert(id.to_string(), n.to_string());
                }
            }
        }
    }

    /// One rollout line → events. Handles both the desktop app's
    /// `item_completed` items and the CLI's `response_item` records.
    fn events(seen: &mut HashSet<String>, line: &str) -> Vec<Ev> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        let p = match v.get("payload") {
            Some(p) => p,
            None => return Vec::new(),
        };
        let ptype = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let mut out = Vec::new();
        match (v.get("type").and_then(|x| x.as_str()).unwrap_or(""), ptype) {
            ("event_msg", "item_completed") => {
                let Some(item) = p.get("item") else {
                    return out;
                };
                if let Some(id) = item.get("id").and_then(|x| x.as_str())
                    && !seen.insert(id.to_string())
                {
                    return out;
                }
                match item.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                    "AgentMessage" => {
                        let t = item.get("content").map(content_text).unwrap_or_default();
                        if !t.trim().is_empty() {
                            out.push(Ev::Text(t));
                        }
                    }
                    "UserMessage" => {
                        let t = item.get("content").map(content_text).unwrap_or_default();
                        if !t.trim().is_empty() && !t.starts_with('<') && !t.starts_with('#') {
                            out.push(Ev::Prompt(t));
                        }
                    }
                    "Reasoning" => {
                        let t = item
                            .get("summary_text")
                            .map(|s| match s {
                                serde_json::Value::Array(a) => a
                                    .iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                serde_json::Value::String(s) => s.clone(),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        if !t.trim().is_empty() {
                            out.push(Ev::Think(t));
                        }
                    }
                    "CommandExecution" => {
                        let cmd = item
                            .get("command")
                            .map(|c| match c {
                                serde_json::Value::Array(a) => a
                                    .iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" "),
                                serde_json::Value::String(s) => s.clone(),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        out.push(Ev::Tool {
                            name: "exec".into(),
                            detail: oneline(&cmd, 90),
                            sub: false,
                        });
                    }
                    "FileChange" => {
                        let paths = item
                            .get("changes")
                            .and_then(|c| c.as_object())
                            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                            .unwrap_or_default();
                        out.push(Ev::Tool {
                            name: "edit".into(),
                            detail: oneline(&paths, 90),
                            sub: false,
                        });
                    }
                    "McpToolCall" => {
                        let name = format!(
                            "{}.{}",
                            item.get("server").and_then(|x| x.as_str()).unwrap_or("mcp"),
                            item.get("tool").and_then(|x| x.as_str()).unwrap_or("?")
                        );
                        out.push(Ev::Tool {
                            name,
                            detail: item.get("arguments").map(arg_detail).unwrap_or_default(),
                            sub: false,
                        });
                        if let Some(r) = item.get("result") {
                            let t = content_text(r);
                            if !t.trim().is_empty() {
                                out.push(Ev::Output(t));
                            }
                        }
                    }
                    "Extension" => {
                        let kind = item.get("kind").and_then(|x| x.as_str()).unwrap_or("ext");
                        let detail = item
                            .get("query")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                            .or_else(|| item.get("action").map(arg_detail))
                            .unwrap_or_default();
                        out.push(Ev::Tool {
                            name: kind.to_string(),
                            detail: oneline(&detail, 90),
                            sub: false,
                        });
                    }
                    "ImageView" => {
                        if let Some(p) = item.get("path").and_then(|x| x.as_str()) {
                            out.push(Ev::Output(format!("image: {p}")));
                        }
                    }
                    _ => {}
                }
            }
            ("event_msg", "agent_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str())
                    && !m.trim().is_empty()
                {
                    out.push(Ev::Text(m.to_string()));
                }
            }
            ("event_msg", "user_message") => {
                if let Some(m) = p.get("message").and_then(|x| x.as_str())
                    && !m.trim().is_empty()
                    && !m.starts_with('<')
                    && !m.starts_with('#')
                {
                    out.push(Ev::Prompt(m.to_string()));
                }
            }
            ("response_item", "message") => {
                let role = p.get("role").and_then(|x| x.as_str()).unwrap_or("");
                if let Some(id) = p.get("id").and_then(|x| x.as_str())
                    && !seen.insert(id.to_string())
                {
                    return out;
                }
                let t = p.get("content").map(content_text).unwrap_or_default();
                match role {
                    "assistant" if !t.trim().is_empty() => out.push(Ev::Text(t)),
                    "user"
                        if !t.trim().is_empty() && !t.starts_with('<') && !t.starts_with('#') =>
                    {
                        out.push(Ev::Prompt(t));
                    }
                    _ => {}
                }
            }
            ("response_item", "reasoning") => {
                let t = p
                    .get("summary")
                    .and_then(|s| s.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(Ev::Think(t));
                }
            }
            ("response_item", "function_call") | ("response_item", "custom_tool_call") => {
                let name = p
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string();
                // OpenAI-style arguments arrive as a JSON *string*.
                let detail = p
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                    .or_else(|| p.get("arguments").cloned())
                    .map(|a| arg_detail(&a))
                    .or_else(|| p.get("input").map(|i| oneline(&content_text(i), 90)))
                    .unwrap_or_default();
                out.push(Ev::Tool {
                    name,
                    detail,
                    sub: false,
                });
            }
            ("response_item", "function_call_output")
            | ("response_item", "custom_tool_call_output") => {
                let t = p
                    .get("output")
                    .map(|o| match o {
                        serde_json::Value::String(s) => s.clone(),
                        other => content_text(other),
                    })
                    .unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(Ev::Output(t));
                }
            }
            _ => {}
        }
        out
    }
}

impl Source for Codex {
    fn poll(&mut self, out: &mut Vec<FeedMsg>) {
        self.refresh_names();
        let now = Utc::now().timestamp();
        for (path, len) in crate::sources::codex::walk(&self.dirs) {
            let mt = crate::sources::filecache::mtime(&path);
            let fresh = now - mt < CODEX_FRESH.as_secs() as i64;
            if fresh {
                self.dead.remove(&path);
            }
            // A file stale at first sight was never running during this
            // run — don't tail it or open an already-ended pane for it.
            if !self.tails.contains_key(&path) {
                if !fresh {
                    continue;
                }
                self.tails.insert(
                    path.clone(),
                    CodexTail {
                        offset: len.saturating_sub(TAIL_BYTES),
                        sid: crate::sources::codex::session_id_of(&path),
                        seen: HashSet::new(),
                    },
                );
            }
            if !fresh && self.dead.contains(&path) {
                continue;
            }
            let t = self.tails.get_mut(&path).expect("inserted above");
            let mut evts = Vec::new();
            let seen = &mut t.seen;
            let _ = crate::sources::filecache::read_lines(
                &path,
                t.offset,
                |s| serde_json::from_str::<serde_json::Value>(s).is_ok(),
                |line| evts.extend(Self::events(seen, line)),
            )
            .map(|new| t.offset = new);
            if !evts.is_empty() || !fresh {
                if !fresh {
                    self.dead.insert(path.clone());
                }
                out.push(FeedMsg {
                    src: "codex",
                    id: t.sid.clone(),
                    title: self.names.get(&t.sid).cloned(),
                    live: fresh,
                    evts,
                    last_ts: mt,
                });
            }
        }
    }
}

// ── pane + grid TUI ─────────────────────────────────────────────────────────

/// `s` as styled lines: first line gets `first` as prefix, the rest `rest`;
/// beyond `cap` lines a `…` marker replaces the tail.
fn txt_lines(
    s: &str,
    style: Style,
    first: &'static str,
    rest: &'static str,
    cap: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = s
        .lines()
        .take(cap)
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::styled(if i == 0 { first } else { rest }, style),
                Span::styled(l.to_string(), style),
            ])
        })
        .collect();
    if s.lines().count() > cap {
        lines.push(Line::from(Span::styled(
            format!("{rest}…"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

/// Per-event styled logical lines (wrapped at draw time to the pane width).
fn ev_lines(ev: &Ev) -> Vec<Line<'static>> {
    match ev {
        Ev::Prompt(t) => txt_lines(t, Style::default().fg(Color::Yellow), "❯ ", "  ", 12),
        Ev::Text(t) => txt_lines(t, Style::default().fg(Color::White), "", "", 20),
        Ev::Think(t) => txt_lines(
            t,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
            "· ",
            "  ",
            4,
        ),
        Ev::Tool { name, detail, sub } => {
            let style = if *sub {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let glyph = if *sub { "◦ " } else { "▸ " };
            let s = if detail.is_empty() {
                name.clone()
            } else {
                format!("{name} {detail}")
            };
            vec![Line::from(vec![
                Span::styled(glyph, style),
                Span::styled(s, style),
            ])]
        }
        Ev::Output(t) => txt_lines(t, Style::default().fg(Color::DarkGray), "  ⎿ ", "    ", 3),
    }
}

/// One pane = one running (or just-ended) agent session.
struct Pane {
    src: &'static str,
    id: Arc<str>,
    title: String,
    /// Logical (pre-wrap) lines.
    lines: VecDeque<Line<'static>>,
    /// Wrapped cache at `flat_w`; rebuilt when `dirty`.
    flat: Vec<Line<'static>>,
    flat_w: u16,
    dirty: bool,
    /// Scroll offset in display lines from the bottom; 0 = tailing.
    scroll: u16,
    /// Inner height at last draw — page-scroll step.
    vis: u16,
    /// Max scroll observed at last draw (clamps wheel scrolling).
    max_off: u16,
    live: bool,
    last_ts: i64,
    ended_at: Option<Instant>,
}

impl Pane {
    fn new(src: &'static str, id: String, title: Option<String>) -> Self {
        Self {
            src,
            id: Arc::from(id.as_str()),
            title: title.filter(|t| !t.trim().is_empty()).unwrap_or(id),
            lines: VecDeque::new(),
            flat: Vec::new(),
            flat_w: 0,
            dirty: true,
            scroll: 0,
            vis: 0,
            max_off: 0,
            live: true,
            last_ts: 0,
            ended_at: None,
        }
    }

    fn push(&mut self, evts: Vec<Ev>) {
        for e in evts {
            for l in ev_lines(&e) {
                self.lines.push_back(l);
            }
        }
        while self.lines.len() > CAP {
            self.lines.pop_front();
        }
        self.dirty = true;
    }
}

/// Wrap `lines` to `w` display cells (grapheme-aware: CJK width, emoji
/// clusters), preserving span styles. Hard wrap — matches `tail` behavior.
fn wrap_lines(lines: &VecDeque<Line<'static>>, w: u16) -> Vec<Line<'static>> {
    let w = (w as usize).max(8);
    let mut out = Vec::with_capacity(lines.len());
    for l in lines {
        if l.width() <= w {
            out.push(l.clone());
            continue;
        }
        let mut cur = Line::default();
        let mut cur_w = 0usize;
        for span in &l.spans {
            let mut buf = String::new();
            for g in span.content.as_ref().graphemes(true) {
                let gw = UnicodeWidthStr::width(g);
                if cur_w + gw > w && cur_w > 0 {
                    if !buf.is_empty() {
                        cur.spans
                            .push(Span::styled(std::mem::take(&mut buf), span.style));
                    }
                    out.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                buf.push_str(g);
                cur_w += gw;
            }
            if !buf.is_empty() {
                cur.spans.push(Span::styled(buf, span.style));
            }
        }
        if cur.width() > 0 || l.spans.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn color_of(source: &str) -> Color {
    match source {
        "devin" => Color::Cyan,
        "claude" => Color::Yellow,
        _ => Color::Magenta,
    }
}

/// Grid cell count: pick columns so cells land near a 3:1 char aspect and
/// stay usable (≥28 wide, ≥6 tall). `cols` overrides the auto choice.
fn grid(n: usize, area: Rect, cols: Option<usize>) -> Vec<Rect> {
    if n == 0 || area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let best = |c: usize| {
        let rows = n.div_ceil(c) as u16;
        let (cw, ch) = (area.width / c as u16, area.height / rows);
        // penalize cramped cells, then prefer ~3:1 aspect
        let mut cost = (cw as i32 * 100 / ch.max(1) as i32 - 300).unsigned_abs();
        if cw < 28 || ch < 6 {
            cost += 10_000;
        }
        cost
    };
    let c = cols
        .unwrap_or_else(|| (1..=n.min(8)).min_by_key(|&c| best(c)).unwrap_or(1))
        .clamp(1, n);
    let rows = n.div_ceil(c);
    let v = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    let mut cells = Vec::with_capacity(n);
    for row in v.iter() {
        let h = Layout::horizontal(vec![Constraint::Ratio(1, c as u32); c]).split(*row);
        cells.extend(h.iter().copied());
    }
    cells.truncate(n);
    cells
}

/// Render one frame; `cells` is filled with each pane's on-screen rect so
/// mouse hit-testing lands on what the user sees.
#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut Frame,
    panes: &mut [Pane],
    focus: usize,
    zoom: Option<usize>,
    cols: Option<usize>,
    n_live: usize,
    interval: Duration,
    cells: &mut Vec<Rect>,
) {
    let [body, foot] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    cells.clear();
    if let Some(z) = zoom.filter(|&z| z < panes.len()) {
        cells.resize(panes.len(), Rect::default());
        cells[z] = body;
    } else {
        cells.extend(grid(panes.len(), body, cols));
    }
    for (i, pane) in panes.iter_mut().enumerate() {
        let Some(&cell) = cells.get(i) else { continue };
        if cell.width == 0 || cell.height == 0 {
            continue;
        }
        let inner_w = cell.width.saturating_sub(2);
        let inner_h = cell.height.saturating_sub(2);
        if pane.dirty || pane.flat_w != inner_w {
            pane.flat = wrap_lines(&pane.lines, inner_w);
            pane.flat_w = inner_w;
            pane.dirty = false;
        }
        pane.vis = inner_h;
        pane.max_off = pane
            .flat
            .len()
            .saturating_sub(inner_h as usize)
            .min(u16::MAX as usize) as u16;
        pane.scroll = pane.scroll.min(pane.max_off);
        let y = pane.max_off - pane.scroll;

        let title = if pane.live {
            let ts = if pane.last_ts > 0 {
                chrono::Local
                    .timestamp_opt(pane.last_ts, 0)
                    .single()
                    .map(|t| t.format("%H:%M:%S").to_string())
                    .unwrap_or_default()
            } else {
                "--:--:--".into()
            };
            format!(" {} {} · {} ", pane.src, pane.title, ts)
        } else {
            format!(" {} {} · ended ", pane.src, pane.title)
        };
        let focused = i == focus && zoom.is_none();
        let border = if focused {
            Style::default()
                .fg(color_of(pane.src))
                .add_modifier(Modifier::BOLD)
        } else if pane.live {
            Style::default().fg(color_of(pane.src))
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border);
        let body = Paragraph::new(pane.flat.clone())
            .scroll((y, 0))
            .block(block);
        f.render_widget(body, cell);
    }

    let status = format!(
        " {n_live} live · {} shown · click=zoom · wheel/↑↓=scroll · Tab/Enter=focus/zoom · -/+=columns · {}ms · q quit",
        panes.len(),
        interval.as_millis(),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        ))),
        foot,
    );
}

/// Which pane owns point (x, y), given the `cells` map from the last draw.
fn pane_at(cells: &[Rect], x: u16, y: u16) -> Option<usize> {
    cells
        .iter()
        .position(|r| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
}

/// Restore the terminal on drop.
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

/// Event loop: poll sources on `interval`, redraw on events and ticks.
pub(crate) fn run(mut sources: Vec<Box<dyn Source>>, interval: Duration) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("`watch` needs a terminal");
    }
    enable_raw_mode()?;
    let mut stdout: Stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = Guard;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut panes: Vec<Pane> = Vec::new();
    let mut focus = 0usize;
    let mut zoom: Option<usize> = None;
    let mut cols: Option<usize> = None;
    // Pane index -> on-screen rect, refreshed by every draw.
    let mut cells: Vec<Rect> = Vec::new();
    let mut updates: Vec<FeedMsg> = Vec::new();
    // Poll once immediately so panes exist in the first frame.
    let mut next_tick = Instant::now();

    'outer: loop {
        // Data tick first so the very first frame already shows sessions.
        if Instant::now() >= next_tick {
            next_tick = Instant::now() + interval;

            updates.clear();
            for s in sources.iter_mut() {
                s.poll(&mut updates);
            }
            for u in updates.drain(..) {
                let pos = panes
                    .iter()
                    .position(|p| p.src == u.src && p.id.as_ref() == u.id.as_str());
                match pos {
                    Some(i) => {
                        let p = &mut panes[i];
                        p.push(u.evts);
                        if u.last_ts > 0 {
                            p.last_ts = u.last_ts;
                        }
                        if let Some(t) = u.title {
                            p.title = t;
                        }
                        if !u.live && p.live {
                            p.ended_at = Some(Instant::now());
                        }
                        p.live = u.live;
                    }
                    None => {
                        let mut p = Pane::new(u.src, u.id, u.title);
                        p.live = u.live;
                        p.last_ts = u.last_ts;
                        p.ended_at = (!u.live).then(Instant::now);
                        p.push(u.evts);
                        panes.push(p);
                    }
                }
            }
            // Prune: ended panes linger briefly, then drop.
            let now = Instant::now();
            panes.retain(|p| p.live || p.ended_at.is_none_or(|t| now - t < ENDED_TTL));
            // At most MAX_PANES are drawn — the creation-order prefix — so
            // overflow panes keep state without churning the grid.
            let shown = panes.len().min(MAX_PANES);
            if shown == 0 {
                focus = 0;
            } else {
                focus = focus.min(shown - 1);
            }
            if zoom.is_some_and(|z| z >= shown) {
                zoom = None;
            }
        }

        let shown = panes.len().min(MAX_PANES);
        let n_live = panes.iter().filter(|p| p.live).count();
        term.draw(|f| {
            draw(
                f,
                &mut panes[..shown],
                focus,
                zoom,
                cols,
                n_live,
                interval,
                &mut cells,
            )
        })?;
        while event::poll(next_tick.saturating_duration_since(Instant::now()))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') => break 'outer,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        break 'outer;
                    }
                    KeyCode::Esc => {
                        if zoom.take().is_none() {
                            break 'outer;
                        }
                        continue 'outer;
                    }
                    KeyCode::Tab | KeyCode::Right if zoom.is_none() => {
                        focus = (focus + 1) % shown.max(1);
                        continue 'outer;
                    }
                    KeyCode::BackTab | KeyCode::Left if zoom.is_none() => {
                        focus = (focus + shown.saturating_sub(1)) % shown.max(1);
                        continue 'outer;
                    }
                    KeyCode::Enter => {
                        zoom = match zoom {
                            Some(_) => None,
                            None if shown > 0 => Some(focus.min(shown - 1)),
                            None => None,
                        };
                        continue 'outer;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let i = zoom.unwrap_or(focus);
                        if let Some(p) = panes.get_mut(i) {
                            p.scroll = p.scroll.saturating_add(1).min(p.max_off);
                        }
                        continue 'outer;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let i = zoom.unwrap_or(focus);
                        if let Some(p) = panes.get_mut(i) {
                            p.scroll = p.scroll.saturating_sub(1);
                        }
                        continue 'outer;
                    }
                    KeyCode::PageUp => {
                        let i = zoom.unwrap_or(focus);
                        if let Some(p) = panes.get_mut(i) {
                            p.scroll = p.scroll.saturating_add(p.vis.max(1)).min(p.max_off);
                        }
                        continue 'outer;
                    }
                    KeyCode::PageDown => {
                        let i = zoom.unwrap_or(focus);
                        if let Some(p) = panes.get_mut(i) {
                            p.scroll = p.scroll.saturating_sub(p.vis.max(1));
                        }
                        continue 'outer;
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        let i = zoom.unwrap_or(focus);
                        if let Some(p) = panes.get_mut(i) {
                            p.scroll = 0;
                        }
                        continue 'outer;
                    }
                    KeyCode::Char('-') => {
                        cols = Some(cols.unwrap_or(4).saturating_sub(1).max(1));
                        continue 'outer;
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        cols = Some((cols.unwrap_or(4) + 1).min(8));
                        continue 'outer;
                    }
                    KeyCode::Char('0') => {
                        cols = None;
                        continue 'outer;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => continue 'outer,
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(i) = pane_at(&cells, m.column, m.row) {
                            zoom = match zoom {
                                Some(z) if z == i => None,
                                _ => Some(i),
                            };
                        }
                        continue 'outer;
                    }
                    MouseEventKind::ScrollUp => {
                        if let Some(i) = pane_at(&cells, m.column, m.row)
                            && let Some(p) = panes.get_mut(i)
                        {
                            p.scroll = p.scroll.saturating_add(3).min(p.max_off);
                            continue 'outer;
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Some(i) = pane_at(&cells, m.column, m.row)
                            && let Some(p) = panes.get_mut(i)
                        {
                            p.scroll = p.scroll.saturating_sub(3);
                            continue 'outer;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            // A flooded event queue must not starve the data tick.
            if Instant::now() >= next_tick {
                break;
            }
        }
    }
    Ok(())
}
