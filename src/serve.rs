//! `llmstat serve` — headless live-stat emitter.
//!
//! Runs the same incremental scanners as `monitor`, but instead of a TUI
//! writes a JSON snapshot to a file every tick (default
//! `~/.cache/llmstat/live.json`). External consumers — widgets, pets,
//! scripts — read the file instead of scraping a terminal.
//!
//! Also tails Claude transcripts for usage-limit records so consumers can
//! react the moment a session is throttled.

use anyhow::Result;
use chrono::{DateTime, Utc};
use indicatif::{MultiProgress, ProgressDrawTarget};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::filter::Filter;
use crate::monitor::State;
use crate::pricing::PriceBook;
use crate::sources::AnyScanner;

/// Persist dirty scanner caches at most this often (same as monitor).
const SAVE_EVERY: Duration = Duration::from_secs(30);
/// Rate windows reported in the snapshot.
const RATE_FAST: i64 = 5;
const RATE_SLOW: i64 = 30;
/// A limit hit counts as fresh for this long after `at`.
const LIMIT_FRESH: i64 = 600;
/// Only hits this recent matter: transcript replays (compact/resume)
/// re-emit old records, so anything older is ignored.
const LIMIT_MAX_AGE: i64 = 3600;

pub fn default_out() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".cache/llmstat/live.json")
}

#[derive(Serialize)]
struct Snapshot<'a> {
    /// Unix seconds when this snapshot was written.
    ts: i64,
    /// Tokens/s across all sources over the last `RATE_FAST` seconds.
    tok_s: f64,
    /// Tokens/s across all sources over the last `RATE_SLOW` seconds.
    rate_30s: f64,
    /// Per-source tok/s over the fast window.
    sources: HashMap<&'static str, f64>,
    /// Newest Claude usage-limit hit, if any was observed recently.
    limit: Option<&'a LimitHit>,
}

#[derive(Serialize)]
struct LimitHit {
    /// Unix seconds of the transcript record carrying the notice.
    at: i64,
    /// The "resets 9:20am" hint verbatim, when the record carries one.
    reset: Option<String>,
}

/// Incremental tail-watcher for limit records in Claude transcripts.
/// Claude Code writes `"You've hit your session limit · resets <when>"`
/// as a `<synthetic>` assistant record; the usage parser drops those, so
/// serve watches raw appended bytes itself.
struct LimitWatch {
    dir: PathBuf,
    offsets: HashMap<PathBuf, u64>,
    hit: Option<LimitHit>,
}

impl LimitWatch {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            offsets: HashMap::new(),
            hit: None,
        }
    }

    fn tick(&mut self) {
        let now = Utc::now().timestamp();
        for (path, len) in crate::sources::claude::walk(&self.dir) {
            let off = self.offsets.entry(path.clone()).or_insert(len);
            // First sighting or truncation: establish the offset without
            // scanning history — only records appended while serve runs
            // (or written just before start, see LIMIT_MAX_AGE) count.
            if len < *off {
                *off = len;
                continue;
            }
            if len == *off {
                continue;
            }
            let Ok(mut f) = std::fs::File::open(&path) else {
                continue;
            };
            if f.seek(SeekFrom::Start(*off)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                continue;
            }
            *off = len;
            self.scan(&buf, now);
        }
    }

    fn scan(&mut self, buf: &[u8], now: i64) {
        let text = String::from_utf8_lossy(buf);
        for line in text.lines() {
            if !line.contains("hit your session limit")
                && !line.contains("usage limit reached")
            {
                continue;
            }
            let at = find_field(line, "\"timestamp\":\"")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp())
                .unwrap_or(now);
            // Replayed/compacted history carries old hits — skip them.
            if now - at > LIMIT_MAX_AGE {
                continue;
            }
            let reset = find_field(line, "resets ")
                .map(|s| s.trim_end_matches(')').trim().to_string());
            let newer = self.hit.as_ref().is_none_or(|h| at >= h.at);
            if newer {
                self.hit = Some(LimitHit { at, reset });
            }
        }
    }

    /// The latest hit, kept only while fresh so consumers can distinguish
    /// "throttled right now" from history.
    fn fresh(&self) -> Option<&LimitHit> {
        let h = self.hit.as_ref()?;
        (Utc::now().timestamp() - h.at <= LIMIT_FRESH).then_some(h)
    }
}

/// After `needle`, return the text up to the next `"` or `(`.
fn find_field<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let i = line.find(needle)? + needle.len();
    let rest = &line[i..];
    let end = rest.find(['"', '(']).unwrap_or(rest.len());
    Some(&rest[..end])
}

pub fn run(
    scanners: &mut [AnyScanner],
    mut state: State,
    book: &PriceBook,
    filter: Option<&Filter>,
    interval: Duration,
    out: &Path,
    claude_dir: Option<PathBuf>,
) -> Result<()> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = out.with_extension("tmp");
    let hidden = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
    let mut limits = claude_dir.map(LimitWatch::new);
    let mut save_at = Instant::now() + SAVE_EVERY;

    tracing::info!(out = %out.display(), interval_ms = interval.as_millis() as u64, "serve started");
    loop {
        for sc in scanners.iter_mut() {
            match sc.tick(&hidden) {
                Ok(calls) => state.apply(calls, book, filter),
                Err(e) => state.set_status(format!("{}: {e:#}", sc.name())),
            }
        }
        if let Some(w) = &mut limits {
            w.tick();
        }
        let snap = Snapshot {
            ts: Utc::now().timestamp(),
            tok_s: state.source_rates(RATE_FAST).iter().map(|(_, v)| *v).sum(),
            rate_30s: state.source_rates(RATE_SLOW).iter().map(|(_, v)| *v).sum(),
            sources: state.source_rates(RATE_FAST).into_iter().collect(),
            limit: limits.as_ref().and_then(|w| w.fresh()),
        };
        std::fs::write(&tmp, serde_json::to_vec(&snap)?)?;
        std::fs::rename(&tmp, out)?;

        if Instant::now() >= save_at {
            for sc in scanners.iter_mut() {
                sc.save();
            }
            save_at = Instant::now() + SAVE_EVERY;
        }
        std::thread::sleep(interval);
    }
}
