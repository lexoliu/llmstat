//! Antigravity CLI: `~/.gemini/antigravity-cli/conversations/<id>.db`.
//!
//! Each `gen_metadata` row is a protobuf blob. Uncached input is field
//! `1.4.2`, output `1.4.3`, cached `1.4.5` (absent on the first call of a
//! turn, then grows), model `1.19`. Timestamps come from the matching
//! `steps` row with `step_type = 15` (assistant generation), field `1.1`
//! (Timestamp seconds). Rows are insert-only by `idx`, so a scan tails
//! the high-water mark. Titles come from sibling `conversation_summaries.db`.

use anyhow::{Context, Result};
use chrono::DateTime;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::proto::{Reader, Value};
use crate::report::{Call, Usage};
use crate::sources::SourceOut;

const SOURCE: &str = "antigravity";
const CACHE_VERSION: u32 = 1;
/// Assistant generation step — one per `gen_metadata` row, in order.
const STEP_GENERATION: i64 = 15;

/// One decoded generation, persisted between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawGen {
    idx: i64,
    input: u64,
    cached: u64,
    output: u64,
    model: String,
    ts: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    db_path: String,
    max_idx: i64,
    rows: Vec<RawGen>,
}

fn cache_file(db: &Path) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    db.hash(&mut h);
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(format!(
            ".cache/llmstat/antigravity-{:016x}.bin",
            h.finish()
        ))
}

fn cache_load(db: &Path, cur_max: i64) -> Option<(i64, Vec<RawGen>)> {
    let bytes = std::fs::read(cache_file(db)).ok()?;
    let c: Cache = bincode::deserialize(&bytes).ok()?;
    if c.version != CACHE_VERSION || c.db_path != db.to_string_lossy() || c.max_idx > cur_max {
        return None;
    }
    Some((c.max_idx, c.rows))
}

fn cache_save(db: &Path, max_idx: i64, rows: &[RawGen]) {
    let file = cache_file(db);
    let Some(dir) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let c = Cache {
        version: CACHE_VERSION,
        db_path: db.to_string_lossy().into_owned(),
        max_idx,
        rows: rows.to_vec(),
    };
    let Ok(bytes) = bincode::serialize(&c) else {
        return;
    };
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(tmp, file);
    }
}

pub fn default_dir() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".gemini/antigravity-cli")
}

/// Open a conversation db read-only. Shared with `watch`.
pub(crate) fn open(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))
}

/// Decode `gen_metadata.data`: usage at `1.4.{2,3,5}`, model at `1.19`.
pub(crate) fn decode_gen(data: &[u8]) -> Option<(Usage, String)> {
    let mut usage_msg: Option<&[u8]> = None;
    let mut model = String::new();
    for (f, v) in Reader::new(data) {
        if f != 1 {
            continue;
        }
        let Value::Bytes(inner) = v else { continue };
        for (f2, v2) in Reader::new(inner) {
            match (f2, v2) {
                (4, Value::Bytes(u)) => usage_msg = Some(u),
                (19, Value::Bytes(m)) => {
                    if let Ok(s) = std::str::from_utf8(m)
                        && !s.is_empty()
                    {
                        model = s.to_string();
                    }
                }
                _ => {}
            }
        }
        break;
    }
    let u = usage_msg?;
    let mut input = 0u64;
    let mut output = 0u64;
    let mut cached = 0u64;
    for (f, v) in Reader::new(u) {
        let Value::Varint(n) = v else { continue };
        match f {
            2 => input = n,
            3 => output = n,
            5 => cached = n,
            _ => {}
        }
    }
    if input + output + cached == 0 {
        return None;
    }
    Some((
        Usage {
            input,
            cached,
            output,
        },
        model,
    ))
}

/// Timestamp seconds from a `step_type = 15` metadata blob (`1.1`).
pub(crate) fn ts_of_step(meta: &[u8]) -> Option<i64> {
    for (f, v) in Reader::new(meta) {
        if f != 1 {
            continue;
        }
        let Value::Bytes(ts) = v else { continue };
        for (f2, v2) in Reader::new(ts) {
            if f2 == 1
                && let Value::Varint(s) = v2
            {
                return Some(s as i64);
            }
        }
        return None;
    }
    None
}

/// UTF-8 string at a protobuf field path, or `None`.
pub(crate) fn string_at<'a>(buf: &'a [u8], path: &[u32]) -> Option<&'a str> {
    let Some((&head, rest)) = path.split_first() else {
        return std::str::from_utf8(buf).ok();
    };
    for (f, v) in Reader::new(buf) {
        if f != head {
            continue;
        }
        let Value::Bytes(inner) = v else { return None };
        if rest.is_empty() {
            return std::str::from_utf8(inner).ok();
        }
        return string_at(inner, rest);
    }
    None
}

fn max_idx(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(idx), -1) FROM gen_metadata", [], |r| {
            r.get(0)
        })?,
    )
}

fn generation_timestamps(conn: &Connection) -> Result<Vec<Option<i64>>> {
    let mut st = conn.prepare("SELECT metadata FROM steps WHERE step_type = ?1 ORDER BY idx")?;
    let rows = st.query_map([STEP_GENERATION], |r| r.get::<_, Option<Vec<u8>>>(0))?;
    let mut out = Vec::new();
    for r in rows {
        let blob = r?;
        out.push(blob.as_deref().and_then(ts_of_step));
    }
    Ok(out)
}

fn scan_tail(path: &Path, lo: i64) -> Result<Vec<RawGen>> {
    let conn = open(path)?;
    let timestamps = generation_timestamps(&conn)?;
    let mut st = conn.prepare("SELECT idx, data FROM gen_metadata WHERE idx >= ?1 ORDER BY idx")?;
    let rows = st.query_map([lo], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
    })?;
    let mut out = Vec::new();
    // `lo` is the next idx; with contiguous 0-based idx that's also the
    // zip position into the ordered step_type=15 timestamps.
    let mut pos = if lo <= 0 { 0usize } else { lo as usize };
    for r in rows {
        let (idx, data) = r?;
        let Some(data) = data else {
            pos += 1;
            continue;
        };
        let Some((usage, model)) = decode_gen(&data) else {
            pos += 1;
            continue;
        };
        let ts = timestamps.get(pos).copied().flatten();
        pos += 1;
        out.push(RawGen {
            idx,
            input: usage.input,
            cached: usage.cached,
            output: usage.output,
            model,
            ts,
        });
    }
    Ok(out)
}

/// Conversation dbs under each Antigravity CLI root.
pub fn walk(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in dirs {
        let conv = dir.join("conversations");
        let Ok(rd) = std::fs::read_dir(&conv) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "db") {
                files.push(p);
            }
        }
    }
    files.sort();
    files
}

fn load_titles(dirs: &[PathBuf]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for dir in dirs {
        let p = dir.join("conversation_summaries.db");
        if !p.exists() {
            continue;
        }
        let Ok(conn) = open(&p) else { continue };
        let Ok(mut st) = conn.prepare("SELECT conversation_id, title FROM conversation_summaries")
        else {
            continue;
        };
        let Ok(rows) = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            continue;
        };
        for r in rows.flatten() {
            if !r.1.is_empty() {
                out.insert(r.0, super::titleize(&r.1));
            }
        }
    }
    out
}

fn session_id_of(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn to_call(titles: &HashMap<String, String>, path: &Path, g: &RawGen) -> Call {
    let sid = session_id_of(path);
    let name = titles.get(&sid).cloned().map(Arc::from);
    let model = if g.model.is_empty() {
        "antigravity".to_string()
    } else {
        g.model.clone()
    };
    Call {
        source: SOURCE,
        session: Arc::from(sid),
        session_name: name,
        model: Arc::from(model),
        ts: g.ts.and_then(|s| DateTime::from_timestamp(s, 0)),
        usage: Usage {
            input: g.input,
            cached: g.cached,
            output: g.output,
        },
        estimated: false,
    }
}

/// Per-conversation high-water scanner. First tick emits cached rows plus
/// any tail; later ticks emit only newly observed generations.
struct Conv {
    path: PathBuf,
    next_idx: i64,
    rows: Vec<RawGen>,
    emitted: HashSet<i64>,
    dirty: bool,
}

impl Conv {
    fn open(path: PathBuf) -> Result<Self> {
        let conn = open(&path)?;
        let cur_max = max_idx(&conn)?;
        let (rows, next_idx) = match cache_load(&path, cur_max) {
            Some((max_idx, cached)) => (cached, max_idx + 1),
            None => (Vec::new(), 0),
        };
        Ok(Self {
            path,
            next_idx,
            rows,
            emitted: HashSet::new(),
            dirty: false,
        })
    }

    fn tick(&mut self) -> Result<Vec<RawGen>> {
        let conn = open(&self.path)?;
        let cur_max = max_idx(&conn)?;
        drop(conn);
        let mut fresh = Vec::new();
        if cur_max >= self.next_idx {
            let new_rows = scan_tail(&self.path, self.next_idx)?;
            self.next_idx = cur_max + 1;
            self.rows.extend(new_rows.iter().cloned());
            self.dirty = true;
            cache_save(&self.path, cur_max, &self.rows);
            fresh = new_rows;
        }
        Ok(fresh)
    }
}

pub struct Scanner {
    dirs: Vec<PathBuf>,
    convs: HashMap<PathBuf, Conv>,
    titles: HashMap<String, String>,
    first: bool,
    files: usize,
    failed: usize,
    gens: usize,
}

impl Scanner {
    pub fn open(dirs: &[PathBuf]) -> Result<Self> {
        Ok(Self {
            dirs: dirs.to_vec(),
            convs: HashMap::new(),
            titles: HashMap::new(),
            first: true,
            files: 0,
            failed: 0,
            gens: 0,
        })
    }

    pub fn tick(&mut self, mp: &MultiProgress) -> Result<Vec<Call>> {
        self.titles = load_titles(&self.dirs);
        let found = walk(&self.dirs);
        let pb = (self.first && found.len() >= 10).then(|| {
            let pb = mp.add(ProgressBar::new_spinner());
            pb.set_style(
                ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("static template"),
            );
            pb.set_message(format!(
                "scanning {} antigravity conversations",
                found.len()
            ));
            pb.enable_steady_tick(std::time::Duration::from_millis(80));
            pb
        });

        let mut calls = Vec::new();
        for path in found {
            let newly = !self.convs.contains_key(&path);
            if newly {
                match Conv::open(path.clone()) {
                    Ok(c) => {
                        self.files += 1;
                        self.convs.insert(path.clone(), c);
                    }
                    Err(e) => {
                        self.failed += 1;
                        tracing::warn!("{} unreadable: {e:#}", path.display());
                        continue;
                    }
                }
            }
            let emit_all = newly || self.first;
            let gens = {
                let conv = self.convs.get_mut(&path).expect("inserted");
                match conv.tick() {
                    Ok(fresh) => {
                        if emit_all {
                            conv.rows
                                .iter()
                                .filter(|g| conv.emitted.insert(g.idx))
                                .cloned()
                                .collect::<Vec<_>>()
                        } else {
                            fresh
                                .into_iter()
                                .filter(|g| conv.emitted.insert(g.idx))
                                .collect()
                        }
                    }
                    Err(e) => {
                        self.failed += 1;
                        tracing::warn!("{} scan failed: {e:#}", path.display());
                        continue;
                    }
                }
            };
            self.gens += gens.len();
            calls.extend(gens.iter().map(|g| to_call(&self.titles, &path, g)));
        }
        self.first = false;
        if let Some(pb) = pb {
            pb.finish_and_clear();
        }
        Ok(calls)
    }

    pub fn save(&mut self) {
        for c in self.convs.values_mut() {
            if c.dirty {
                cache_save(&c.path, c.next_idx.saturating_sub(1), &c.rows);
                c.dirty = false;
            }
        }
    }

    pub fn note(&self) -> String {
        let mut note = format!(
            "antigravity: {} conversations · {} generations",
            self.files, self.gens
        );
        if self.failed > 0 {
            note.push_str(&format!(" · {} failed", self.failed));
        }
        note
    }
}

pub fn load(dirs: &[PathBuf], mp: &MultiProgress) -> Result<SourceOut> {
    let t0 = std::time::Instant::now();
    let mut sc = Scanner::open(dirs)?;
    let calls = sc.tick(mp)?;
    sc.save();
    tracing::debug!(
        files = sc.files,
        gens = sc.gens,
        failed = sc.failed,
        calls = calls.len(),
        elapsed = ?t0.elapsed(),
        "antigravity source"
    );
    Ok(SourceOut {
        note: sc.note(),
        calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Writer;

    fn gen_blob(input: u64, cached: Option<u64>, output: u64, model: &str) -> Vec<u8> {
        let mut usage = Writer::new();
        usage.varint_field(2, input);
        usage.varint_field(3, output);
        if let Some(c) = cached {
            usage.varint_field(5, c);
        }
        let mut inner = Writer::new();
        inner.message(4, &usage);
        inner.string(19, model);
        let mut top = Writer::new();
        top.message(1, &inner);
        top.into_vec()
    }

    fn step_meta(ts: i64) -> Vec<u8> {
        let mut stamp = Writer::new();
        stamp.varint_field(1, ts as u64);
        let mut top = Writer::new();
        top.message(1, &stamp);
        top.into_vec()
    }

    fn write_conv(dir: &Path, sid: &str, gens: &[(u64, Option<u64>, u64, &str, i64)]) -> PathBuf {
        let conv_dir = dir.join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let path = conv_dir.join(format!("{sid}.db"));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx));
             CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0, has_subtrajectory numeric NOT NULL DEFAULT false, metadata blob, error_details blob, permissions blob, task_details blob, render_info blob, step_payload blob, step_format integer NOT NULL DEFAULT 0, PRIMARY KEY (idx));",
        )
        .unwrap();
        for (i, (input, cached, output, model, ts)) in gens.iter().enumerate() {
            let blob = gen_blob(*input, *cached, *output, model);
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
                rusqlite::params![i as i64, blob, 0i64],
            )
            .unwrap();
            // step_type=15 rows sit on odd idx like the live dbs; zip is by order.
            conn.execute(
                "INSERT INTO steps (idx, step_type, metadata) VALUES (?1, ?2, ?3)",
                rusqlite::params![i as i64 * 2 + 1, STEP_GENERATION, step_meta(*ts)],
            )
            .unwrap();
        }
        path
    }

    fn write_title(dir: &Path, sid: &str, title: &str) {
        let path = dir.join("conversation_summaries.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversation_summaries (conversation_id text PRIMARY KEY, title text NOT NULL DEFAULT '');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title) VALUES (?1, ?2)",
            rusqlite::params![sid, title],
        )
        .unwrap();
    }

    #[test]
    fn gen_metadata_splits_cache_and_keeps_cjk_title() {
        let tmp = std::env::temp_dir().join(format!("llmstat-agy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let sid = "aa7a3405-f2c6-48a6-aeab-85aa90186a3f";
        // Real numbers from aa7a3405 idx 0 (no cache) and idx 1 (cache grown).
        write_conv(
            &tmp,
            sid,
            &[
                (11013, None, 437, "gemini-3.8-flash-tiered", 1794704612),
                (2894, Some(8968), 81, "gemini-3.8-flash-tiered", 1794704617),
            ],
        );
        write_title(&tmp, sid, "扫描 マージ済み worktree 병합된 세션");

        let blob0 = gen_blob(11013, None, 437, "gemini-3.8-flash-tiered");
        let (u0, m0) = decode_gen(&blob0).unwrap();
        assert_eq!(u0.input, 11013);
        assert_eq!(u0.cached, 0);
        assert_eq!(u0.output, 437);
        assert_eq!(m0, "gemini-3.8-flash-tiered");

        let mp = MultiProgress::new();
        mp.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        let mut sc = Scanner::open(std::slice::from_ref(&tmp)).unwrap();
        let calls = sc.tick(&mp).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].source, "antigravity");
        assert_eq!(calls[0].session.as_ref(), sid);
        assert_eq!(
            calls[0].session_name.as_deref(),
            Some("扫描 マージ済み worktree 병합된 세션")
        );
        assert_eq!(calls[0].usage.input, 11013);
        assert_eq!(calls[0].usage.cached, 0);
        assert_eq!(calls[0].usage.output, 437);
        assert_eq!(calls[0].ts, DateTime::from_timestamp(1794704612, 0));
        assert_eq!(calls[1].usage.input, 2894);
        assert_eq!(calls[1].usage.cached, 8968);
        assert_eq!(calls[1].usage.output, 81);
        assert_eq!(calls[1].model.as_ref(), "gemini-3.8-flash-tiered");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
