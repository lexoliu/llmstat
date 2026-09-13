//! Incremental cache for append-only JSONL logs (Claude transcripts, Codex
//! rollouts).
//!
//! A file is consumed up to a line boundary: an unterminated tail line that
//! doesn't parse as complete JSON is left for the next run, so a line can
//! never be half-read. Before resuming mid-file we memcmp a 64-byte probe
//! ending at the stored offset — if the prefix was rewritten the file is
//! reparsed from scratch.
//!
//! Cache file: ~/.cache/llmstat/<name>.bin (bincode, versioned).

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::report::{Call, Usage};

const VERSION: u32 = 1;
const PROBE: usize = 64;

/// Serializable form of `Call` (`Call.source` is `&'static str`, set by the
/// owning source on rehydration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCall {
    pub session: String,
    pub model: String,
    /// unix seconds
    pub ts: Option<i64>,
    pub usage: Usage,
    pub estimated: bool,
}

impl CachedCall {
    pub fn to_call(&self, source: &'static str) -> Call {
        Call {
            source,
            session: self.session.clone(),
            model: self.model.clone(),
            ts: self.ts.and_then(|s| chrono::DateTime::from_timestamp(s, 0)),
            usage: self.usage,
            estimated: self.estimated,
        }
    }
}

/// Everything remembered about one scanned file. `S` is the parser state
/// needed to resume mid-file (e.g. Codex's current model/session id).
#[derive(Serialize, Deserialize)]
pub struct Entry<S> {
    /// Bytes consumed, always at a line boundary.
    pub offset: u64,
    /// File mtime (unix secs) at parse time.
    pub mtime: i64,
    /// Up to `PROBE` bytes ending at `offset` — append-safety probe.
    pub boundary: Vec<u8>,
    pub state: S,
    /// (dedup key, call) pairs parsed from the file.
    pub entries: Vec<(String, CachedCall)>,
}

#[derive(Serialize)]
struct CacheFile<'a, S: Serialize> {
    version: u32,
    files: &'a HashMap<PathBuf, Entry<S>>,
}

/// On-disk shape for reading back.
#[derive(Deserialize)]
struct CacheFileRead<S> {
    version: u32,
    files: HashMap<PathBuf, Entry<S>>,
}

fn cache_path(name: &str) -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(format!(".cache/llmstat/{name}.bin"))
}

pub fn load<S: DeserializeOwned>(name: &str) -> HashMap<PathBuf, Entry<S>> {
    let Ok(bytes) = std::fs::read(cache_path(name)) else {
        return HashMap::new();
    };
    match bincode::deserialize::<CacheFileRead<S>>(&bytes) {
        Ok(c) if c.version == VERSION => c.files,
        _ => HashMap::new(),
    }
}

pub fn save<S: Serialize>(name: &str, files: &HashMap<PathBuf, Entry<S>>) {
    let file = cache_path(name);
    if let Some(dir) = file.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return;
    }
    let c = CacheFile {
        version: VERSION,
        files,
    };
    let Ok(bytes) = bincode::serialize(&c) else {
        return;
    };
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(tmp, file);
    }
}

/// What to do with a file this run.
pub enum Plan<S> {
    /// offset == len and mtime unchanged — reuse cached entries, no reads.
    Reuse,
    /// File grew past the stored offset and the prefix probe verified —
    /// parse only the tail starting at this offset with this state.
    Resume(u64, S),
    /// New, shrunk, rewritten, or unverifiable — parse from byte 0.
    Full,
}

pub fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Classify `path` against its cached entry. `len` comes from the directory
/// walk's metadata. Opens the file only when a tail parse is plausible.
pub fn plan<S: Clone>(path: &Path, len: u64, cached: Option<&Entry<S>>) -> Plan<S> {
    let Some(e) = cached else { return Plan::Full };
    if len == e.offset && e.mtime == mtime(path) {
        return Plan::Reuse;
    }
    if len > e.offset && probe_ok(path, e) {
        return Plan::Resume(e.offset, e.state.clone());
    }
    Plan::Full
}

fn probe_ok<S>(path: &Path, e: &Entry<S>) -> bool {
    let n = e.boundary.len() as u64;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; n as usize];
    f.seek(SeekFrom::Start(e.offset - n)).is_ok()
        && f.read_exact(&mut buf).is_ok()
        && buf == e.boundary
}

/// Read the ≤`PROBE` bytes ending at `offset` (the probe stored for the next
/// run's append check).
pub fn boundary(path: &Path, offset: u64) -> Vec<u8> {
    let n = offset.min(PROBE as u64);
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; n as usize];
    if f.seek(SeekFrom::Start(offset - n)).is_err() || f.read_exact(&mut buf).is_err() {
        return Vec::new();
    }
    buf
}

/// Iterate lines of `path` starting at byte `offset`, handing each to `f`.
/// Returns bytes consumed, always at a line boundary. A final line without a
/// trailing newline is consumed only if `is_complete` says it's already a
/// well-formed record (a partial append is left for the next run).
pub fn read_lines(
    path: &Path,
    offset: u64,
    is_complete: impl Fn(&str) -> bool,
    mut f: impl FnMut(&str),
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut consumed = offset;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = match reader.read_line(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.ends_with('\n') {
            consumed += n as u64;
            f(&buf);
        } else {
            if is_complete(&buf) {
                consumed += n as u64;
                f(&buf);
            }
            break;
        }
    }
    Ok(consumed)
}
