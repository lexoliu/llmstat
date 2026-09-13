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
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

/// Stateful scanner holding the db connection open: `tick` probes
/// `max(row_id)` and range-scans only the unseen tail, so a `monitor` poll is
/// a sub-millisecond B-tree descent when idle. The first tick is the full
/// incremental scan (disk cache + parallel ranges), same as a one-shot run.
pub struct Scanner {
    conn: Connection,
    path: PathBuf,
    /// First row_id not yet scanned.
    next_rowid: i64,
    /// All matched rows — persisted to the incremental cache on change.
    rows: Vec<RawRow>,
    /// (session, mid) -> (max ntp, earliest ts) dedup over `rows`.
    best: HashMap<(String, String), (u64, i64)>,
    /// (session, mid) pairs already emitted as calls.
    emitted: HashSet<(String, String)>,
    /// session id -> (model on the session row, task title).
    pub session_meta: HashMap<String, (String, Option<String>)>,
}

fn session_meta(conn: &Connection) -> Result<HashMap<String, (String, Option<String>)>> {
    let mut out = HashMap::new();
    let mut st = conn.prepare("SELECT id, COALESCE(model,''), title FROM sessions")?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for r in rows {
        let (id, m, t) = r?;
        out.insert(id, (m, t));
    }
    Ok(out)
}

fn max_rowid(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(row_id), 0) FROM message_nodes",
        [],
        |r| r.get(0),
    )?)
}

impl Scanner {
    /// Open the db, load the incremental cache, prime the dedup map. The
    /// first `tick` scans whatever tail the cache doesn't cover.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = open(path)?;
        let models = session_meta(&conn)?;
        let cur_max = max_rowid(&conn)?;
        let (rows, next_rowid) = match cache::load(path, cur_max) {
            Some((max_rowid, cached)) => (cached, max_rowid + 1),
            None => (Vec::new(), 0),
        };
        let mut best = HashMap::with_capacity(rows.len());
        for r in &rows {
            let e = best
                .entry((r.session.clone(), r.mid.clone()))
                .or_insert((0, i64::MAX));
            e.0 = e.0.max(r.ntp);
            e.1 = e.1.min(r.ts);
        }
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            next_rowid,
            rows,
            best,
            emitted: HashSet::new(),
            session_meta: models,
        })
    }

    /// Range-scan [lo, hi) over `workers` parallel read-only connections,
    /// prefetching the file tail into the OS page cache first.
    fn scan(&self, lo: i64, hi: i64, mp: &MultiProgress) -> Result<Vec<RawRow>> {
        let file_len = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let stop = prefetch(&self.path, file_len * lo as u64 / (hi as u64 + 1));

        let span = hi - lo;
        let workers = if span < 20_000 { 1 } else { workers() };
        let chunk = (span + workers as i64 - 1) / workers as i64;

        // Only worth a spinner for a real (multi-second) scan; a cache-hit
        // tail finishes before the first frame would draw.
        let pb = (span >= 50_000).then(|| {
            let pb = mp.add(ProgressBar::new_spinner());
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("static template"),
            );
            pb.set_message(format!("scanning {}", self.path.display()));
            pb.enable_steady_tick(std::time::Duration::from_millis(80));
            pb
        });

        let t0 = std::time::Instant::now();
        let mut parts: Vec<Result<Vec<RawRow>>> = Vec::new();
        let path = &self.path;
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for w in 0..workers {
                let wlo = lo + w as i64 * chunk;
                let whi = (wlo + chunk).min(hi);
                if wlo >= whi {
                    break;
                }
                handles.push(s.spawn(move || scan_range(path, wlo, whi)));
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
        stop.store(true, Ordering::Relaxed);
        let mut out = Vec::new();
        for p in parts {
            out.extend(p?);
        }
        Ok(out)
    }

    /// Probe the high-water mark; on growth, scan the tail and return calls
    /// whose (session, mid) first reached a nonzero token count. A node
    /// pair whose metadata copy lands in a later tick emits then — same
    /// totals as a one-shot run.
    pub fn tick(&mut self, mp: &MultiProgress) -> Result<Vec<DbCall>> {
        let cur_max = max_rowid(&self.conn)?;
        let mut out = Vec::new();
        if cur_max < self.next_rowid {
            return Ok(out);
        }
        let new_rows = self.scan(self.next_rowid, cur_max + 1, mp)?;
        self.next_rowid = cur_max + 1;
        for r in new_rows {
            let key = (r.session.clone(), r.mid.clone());
            let e = self.best.entry(key.clone()).or_insert((0, i64::MAX));
            e.0 = e.0.max(r.ntp);
            e.1 = e.1.min(r.ts);
            // Emit once a mid first shows a nonzero count — rows recorded
            // without metadata (ntp=0) never produce a call, matching the
            // one-shot path's `prompt == 0` skip.
            if e.0 > 0 && self.emitted.insert(key) {
                out.push(DbCall {
                    session: r.session.clone(),
                    ts: e.1,
                    prompt: e.0,
                });
            }
            self.rows.push(r);
        }
        self.session_meta = session_meta(&self.conn)?;
        cache::save(&self.path, cur_max, &self.rows);
        Ok(out)
    }
}
