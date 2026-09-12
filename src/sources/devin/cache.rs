//! Incremental cache for sessions.db scans.
//!
//! message_nodes rows are insert-only (row_id is AUTOINCREMENT), so a scan
//! only ever needs to read the tail beyond the last cached row_id. The raw
//! matched rows are persisted here; dedup happens in memory on every run.
//!
//! Cache file: ~/.cache/llmstat/<hash-of-db-path>.bin (bincode).

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::db::RawRow;

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    db_path: String,
    /// Highest message_nodes.row_id covered by `rows`.
    max_rowid: i64,
    rows: Vec<RawRow>,
}

fn cache_file(db: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    db.hash(&mut h);
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(format!(".cache/llmstat/{:016x}.bin", h.finish()))
}

/// Cached (covered max row_id, rows) if the file matches this db and isn't
/// from a newer db state (a rebuilt db resets row_ids, which shows up as
/// cur_max < cached max).
pub fn load(db: &Path, cur_max_rowid: i64) -> Option<(i64, Vec<RawRow>)> {
    let bytes = std::fs::read(cache_file(db)).ok()?;
    let c: Cache = bincode::deserialize(&bytes).ok()?;
    if c.version != VERSION || c.db_path != db.to_string_lossy() || c.max_rowid > cur_max_rowid {
        return None;
    }
    Some((c.max_rowid, c.rows))
}

pub fn save(db: &Path, max_rowid: i64, rows: &[RawRow]) {
    let file = cache_file(db);
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let c = Cache {
        version: VERSION,
        db_path: db.to_string_lossy().into_owned(),
        max_rowid,
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
