//! Read per-call usage out of Devin's `sessions.db` (SQLite via rusqlite).
//!
//! Every message node carries `metadata.num_tokens_preceding`, which equals
//! the exact `prompt_tokens` of the inference call that produced it
//! (verified against `response_dimensions` and transcript `metrics` — they
//! agree to the token). The same logical message is stored twice per node
//! (with/without metadata), so calls are deduped by `message_id`, keeping
//! the max `num_tokens_preceding`.
//!
//! Rows are insert-only, so matched rows are cached on disk and each run
//! scans just the `row_id` tail beyond the cached high-water mark.

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::cache;

const SCAN_SQL: &str = include_str!("message_nodes.sql");

/// One inference call recovered from the sessions.db message tree.
#[derive(Debug)]
pub struct DbCall {
    pub session: String,
    /// Node creation time, unix seconds.
    pub ts: i64,
    /// Exact prompt tokens for this call; 0 when the node recorded none.
    pub prompt: u64,
}

/// A matched assistant message row — the unit persisted between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawRow {
    pub row_id: i64,
    pub session: String,
    pub mid: String,
    /// num_tokens_preceding, 0 when the node recorded none.
    pub ntp: u64,
    pub ts: i64,
}

#[derive(Debug, Default)]
pub struct DbData {
    /// session id -> model recorded on the session row (may be empty).
    pub session_models: HashMap<String, String>,
    /// Inference calls deduped by message_id.
    pub calls: Vec<DbCall>,
}

/// Sequentially read the db + wal into the OS page cache on background
/// threads, starting at `offset` for the main file. SQLite then fetches its
/// scattered 4KB pages from RAM instead of doing ~300k random reads against
/// a live, WAL-mode multi-GB file. Returns a flag that stops the threads.
fn prefetch(path: &Path, offset: u64) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    for suffix in ["", "-wal"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        let stop = stop.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = match std::fs::File::open(&p) {
                Ok(f) => f,
                Err(_) => return,
            };
            if suffix.is_empty() && f.seek(SeekFrom::Start(offset)).is_err() {
                return;
            }
            let mut buf = vec![0u8; 8 << 20];
            while !stop.load(Ordering::Relaxed) && matches!(f.read(&mut buf), Ok(n) if n > 0) {}
        });
    }
    stop
}

fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))?;
    // Memory-map the multi-GB file: turns the scan into page-cache hits
    // instead of read() syscalls. mmap is capped at 2GB < file size — the
    // tail is read through the page cache, so give it room.
    conn.pragma_update(None, "mmap_size", 8_000_000_000i64)?;
    conn.pragma_update(None, "cache_size", -2_000_000i64)?;
    Ok(conn)
}

fn scan_range(path: &Path, lo: i64, hi: i64) -> Result<Vec<RawRow>> {
    let conn = open(path)?;
    let mut st = conn.prepare(SCAN_SQL)?;
    let rs = st.query_map([lo, hi], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rs {
        let (row_id, session, mid, ntp, ts) = r?;
        out.push(RawRow {
            row_id,
            session,
            mid,
            ntp: ntp as u64,
            ts,
        });
    }
    Ok(out)
}

fn workers() -> usize {
    std::env::var("LLMSTAT_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4)
        })
        .max(1)
}

pub fn load(path: &Path) -> Result<DbData> {
    let conn = open(path)?;

    let mut session_models = HashMap::new();
    {
        let mut st = conn.prepare("SELECT id, COALESCE(model,'') FROM sessions")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for r in rows {
            let (id, m) = r?;
            session_models.insert(id, m);
        }
    }

    let cur_max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(row_id), 0) FROM message_nodes",
        [],
        |r| r.get(0),
    )?;

    // Resume from the incremental cache when possible.
    let (mut rows, start) = match cache::load(path, cur_max) {
        Some((max_rowid, cached)) => (cached, max_rowid + 1),
        None => (Vec::new(), 0),
    };

    if start <= cur_max {
        // Estimate the byte offset of row `start` (row_ids grow roughly
        // linearly with file bytes) and warm the page cache from there.
        let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let stop = prefetch(path, file_len * start as u64 / (cur_max as u64 + 1));

        // Split [start, cur_max] over several read-only connections — WAL
        // mode allows concurrent readers, and each worker's B-tree range
        // scan reads disjoint page runs.
        let span = cur_max - start + 1;
        let workers = if span < 20_000 { 1 } else { workers() };
        let chunk = (span + workers as i64 - 1) / workers as i64;

        // Only worth a spinner for a real (multi-second) scan; a cache-hit
        // tail finishes before the first frame would draw.
        let pb = (span >= 50_000).then(|| {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("static template"),
            );
            pb.set_message(format!("scanning {}", path.display()));
            pb.enable_steady_tick(std::time::Duration::from_millis(80));
            pb
        });

        let t0 = std::time::Instant::now();
        let mut parts: Vec<Result<Vec<RawRow>>> = Vec::new();
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for w in 0..workers {
                let lo = start + w as i64 * chunk;
                let hi = (lo + chunk).min(cur_max + 1);
                if lo >= hi {
                    break;
                }
                handles.push(s.spawn(move || scan_range(path, lo, hi)));
            }
            for h in handles {
                parts.push(
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("scan panicked"))),
                );
            }
        });
        tracing::debug!(workers, elapsed = ?t0.elapsed(), "message_nodes scan");
        if let Some(pb) = pb {
            pb.finish_and_clear();
        }
        for p in parts {
            rows.extend(p?);
        }
        stop.store(true, Ordering::Relaxed);
        cache::save(path, cur_max, &rows);
    }

    // Dedup by (session, message_id): keep the max token count (metadata
    // and metadata-less copies of the same node pair) and earliest time.
    let mut best: HashMap<(String, String), (u64, i64)> = HashMap::new();
    for r in rows {
        let e = best.entry((r.session, r.mid)).or_insert((0, i64::MAX));
        e.0 = e.0.max(r.ntp);
        e.1 = e.1.min(r.ts);
    }
    let calls = best
        .into_iter()
        .map(|((session, _), (prompt, ts))| DbCall {
            session,
            ts,
            prompt,
        })
        .collect();

    Ok(DbData {
        session_models,
        calls,
    })
}
