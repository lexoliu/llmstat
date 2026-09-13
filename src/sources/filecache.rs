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

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use xxhash_rust::xxh3::Xxh3;

use crate::report::{Call, Usage};

const VERSION: u32 = 4;
const PROBE: usize = 64;

/// 128-bit dedup key. Sources hash their identifying parts — a false
/// collision would drop a real call, so 64 bits is not enough.
pub fn key_of(parts: &[&[u8]]) -> u128 {
    let mut h = Xxh3::new();
    for p in parts {
        // length prefix keeps tuple boundaries unambiguous
        h.update(&(p.len() as u32).to_le_bytes());
        h.update(p);
    }
    h.digest128()
}

/// Per-file string table: session/model names repeat per call, so entries
/// store u32 indices into this instead of copies.
#[derive(Default)]
pub struct Dict {
    strings: Vec<String>,
    index: HashMap<String, u32>,
}

impl Dict {
    /// Rebuild a Dict from a cached `Entry.dict` so resumed parses keep
    /// existing indices stable.
    pub fn from_vec(strings: Vec<String>) -> Self {
        let index = strings
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as u32))
            .collect();
        Self { strings, index }
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.index.get(s) {
            return i;
        }
        let i = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.index.insert(s.to_string(), i);
        i
    }

    pub fn into_strings(self) -> Vec<String> {
        self.strings
    }
}

/// Find-or-push on a plain string vec — `fixup` runs after the `Dict` is
/// consumed, so it interns by hand.
pub fn dict_get_or_push(dict: &mut Vec<String>, s: &str) -> u32 {
    if let Some(i) = dict.iter().position(|d| d == s) {
        return i as u32;
    }
    dict.push(s.to_string());
    (dict.len() - 1) as u32
}

/// Serializable form of `Call` (`Call.source` is `&'static str`, set by the
/// owning source on rehydration; `session`/`model` index the entry's dict).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCall {
    /// xxh3-128 dedup key (see `key_of`).
    pub key: u128,
    /// Index into `Entry::dict`.
    pub session: u32,
    /// Optional display name for the session, index into `Entry::dict`.
    pub session_name: Option<u32>,
    /// Index into `Entry::dict`.
    pub model: u32,
    /// unix seconds
    pub ts: Option<i64>,
    pub usage: Usage,
    pub estimated: bool,
}

impl CachedCall {
    pub fn to_call(&self, source: &'static str, dict: &[Arc<str>]) -> Call {
        Call {
            source,
            session: dict[self.session as usize].clone(),
            session_name: self.session_name.map(|i| dict[i as usize].clone()),
            model: dict[self.model as usize].clone(),
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
    /// String table that `CachedCall.session`/`model` index into.
    pub dict: Vec<String>,
    /// Calls parsed from the file.
    pub entries: Vec<CachedCall>,
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

/// The cache is scoped to the directory set being scanned — a `--*-dir`
/// override must not read or clobber the default location's cache.
fn cache_path(name: &str, scope: &[&Path]) -> PathBuf {
    let mut h = Xxh3::new();
    for p in scope {
        let s = p.to_string_lossy();
        h.update(&(s.len() as u32).to_le_bytes());
        h.update(s.as_bytes());
    }
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(format!(".cache/llmstat/{name}-{:032x}.bin", h.digest128()))
}

pub fn load<S: DeserializeOwned>(name: &str, scope: &[&Path]) -> HashMap<PathBuf, Entry<S>> {
    let t0 = std::time::Instant::now();
    let Ok(bytes) = std::fs::read(cache_path(name, scope)) else {
        return HashMap::new();
    };
    let n = bytes.len();
    let out = match bincode::deserialize::<CacheFileRead<S>>(&bytes) {
        Ok(c) if c.version == VERSION => c.files,
        _ => HashMap::new(),
    };
    tracing::debug!(name, bytes = n, files = out.len(), elapsed = ?t0.elapsed(), "filecache load");
    out
}

pub fn save<S: Serialize>(name: &str, scope: &[&Path], files: &HashMap<PathBuf, Entry<S>>) {
    let file = cache_path(name, scope);
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
    tracing::debug!(
        name,
        bytes = bytes.len(),
        files = files.len(),
        "filecache save"
    );
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

/// Bytes a `Plan` will read — drives the progress-bar threshold.
pub fn plan_bytes<S>(len: u64, p: &Plan<S>) -> u64 {
    match p {
        Plan::Reuse => 0,
        Plan::Resume(o, _) => len - o,
        Plan::Full => len,
    }
}

/// Show a parse progress bar only above this much fresh input — below it the
/// scan finishes before a human can read the bar anyway.
pub const BAR_MIN_BYTES: u64 = 32 << 20;

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

/// A JSONL source plugged into `Scanner`: how to name its cache, seed a
/// fresh parse, parse a file region, and repair a merged entry.
pub trait Jsonl: 'static {
    /// Parser state carried across mid-file resumes.
    type State: Clone + Send + Sync + Serialize + DeserializeOwned;
    /// `Call::source` — also stems the cache file (`<SOURCE>-files`).
    const SOURCE: &'static str;
    /// State for a from-scratch parse of `path`.
    fn fresh(path: &Path) -> Self::State;
    /// Parse `path` from `offset` under `state`, extending `dict`.
    fn parse(
        path: &Path,
        offset: u64,
        state: Self::State,
        dict: Vec<String>,
    ) -> Parsed<Self::State>;
    /// Post-merge repair on a file's whole entry (codex backfills
    /// "unknown" model indices once a turn_context names the model).
    fn fixup(_entry: &mut Entry<Self::State>) {}
}

/// `Jsonl::parse` output: (consumed bytes, final state, dict, tail calls).
pub type Parsed<S> = std::io::Result<(u64, S, Vec<String>, Vec<CachedCall>)>;

/// What one parse pass produced for a file.
enum Outcome<S> {
    /// offset == len and mtime unchanged — keep the cached entry.
    Reuse,
    /// Parsed from the cached offset; `tail` appends to existing entries.
    Resumed(u64, S, Vec<String>, Vec<CachedCall>),
    /// Parsed from 0; `tail` replaces existing entries.
    Full(u64, S, Vec<String>, Vec<CachedCall>),
}

/// Per-tick counts (first tick = a full scan; later ticks are deltas).
#[derive(Default)]
pub struct Tick {
    pub calls: Vec<Call>,
    /// Files found on disk this pass.
    pub files: usize,
    /// Files (re)parsed this pass.
    pub parsed: usize,
    /// Fresh bytes parsed this pass.
    pub bytes: u64,
    /// New-looking entries dropped as already-seen.
    pub dupes: usize,
}

/// Stateful scanner for an append-only JSONL source. Holds the file cache
/// and the dedup set in memory; `tick` emits only calls not emitted before,
/// so `monitor` mode polls cheaply and one-shot `load` is `tick` once + save.
pub struct Scanner<J: Jsonl> {
    scope: Vec<PathBuf>,
    files: HashMap<PathBuf, Entry<J::State>>,
    /// Entries per file already checked against `seen` — entries are
    /// append-only within a file's vec, so a count is enough.
    emitted: HashMap<PathBuf, usize>,
    /// Dedup keys across files (rollout copies, claude resume copies).
    seen: HashSet<u128>,
    dirty: bool,
}

impl<J: Jsonl> Scanner<J> {
    /// Open with the on-disk cache for `scope` (corrupt/absent → empty).
    pub fn open(scope: Vec<PathBuf>) -> Self {
        let sref: Vec<&Path> = scope.iter().map(PathBuf::as_path).collect();
        Self {
            files: load(&format!("{}-files", J::SOURCE), &sref),
            scope,
            emitted: HashMap::new(),
            seen: HashSet::new(),
            dirty: false,
        }
    }

    /// One scan pass over `found` (walk results: path + len). Parses only
    /// files whose plan isn't `Reuse`, merges entries, and returns calls
    /// whose dedup keys haven't been emitted yet.
    pub fn tick(&mut self, found: &[(PathBuf, u64)], mp: &MultiProgress) -> Tick {
        let planned: Vec<Plan<J::State>> = found
            .iter()
            .map(|(p, len)| plan(p, *len, self.files.get(p)))
            .collect();
        let todo: u64 = planned
            .iter()
            .zip(found)
            .map(|(p, (_, len))| plan_bytes(*len, p))
            .sum();
        let pb = (todo >= BAR_MIN_BYTES).then(|| {
            let pb = mp.add(ProgressBar::new(todo));
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.cyan} {msg} {wide_bar:.cyan/blue} {bytes}/{total_bytes}",
                )
                .expect("static template"),
            );
            pb.set_message(format!("{}: parsing", J::SOURCE));
            pb
        });

        // Parse in parallel; results stay in `found` order.
        let results: Vec<Option<Outcome<J::State>>> = planned
            .into_par_iter()
            .zip(found)
            .map(|(plan, (path, len))| {
                let (oc, done) = match plan {
                    Plan::Reuse => (Some(Outcome::Reuse), 0),
                    Plan::Resume(offset, st) => {
                        let dict = self
                            .files
                            .get(path)
                            .map(|e| e.dict.clone())
                            .unwrap_or_default();
                        (
                            J::parse(path, offset, st, dict)
                                .ok()
                                .map(|(c, s, d, e)| Outcome::Resumed(c, s, d, e)),
                            len - offset,
                        )
                    }
                    Plan::Full => (
                        J::parse(path, 0, J::fresh(path), Vec::new())
                            .ok()
                            .map(|(c, s, d, e)| Outcome::Full(c, s, d, e)),
                        *len,
                    ),
                };
                if let Some(pb) = &pb {
                    pb.inc(done);
                }
                oc
            })
            .collect();
        if let Some(pb) = pb {
            pb.finish_and_clear();
        }

        // Merge outcomes into `self.files`.
        let mut tick = Tick {
            files: found.len(),
            ..Tick::default()
        };
        for (i, oc) in results.into_iter().enumerate() {
            let Some(oc) = oc else { continue }; // unreadable: retried next tick
            let path = &found[i].0;
            let (offset, state, dict, mut tail, keep_old) = match oc {
                Outcome::Reuse => continue,
                Outcome::Resumed(o, s, d, t) => (o, s, d, t, true),
                Outcome::Full(o, s, d, t) => (o, s, d, t, false),
            };
            let mut entries = if keep_old {
                self.files
                    .remove(path)
                    .map(|e| e.entries)
                    .unwrap_or_default()
            } else {
                // entries are being rebuilt — the old emit watermark is
                // meaningless against the new vec
                self.files.remove(path);
                self.emitted.insert(path.clone(), 0);
                Vec::new()
            };
            entries.append(&mut tail);
            let mut e = Entry {
                offset,
                mtime: mtime(path),
                boundary: boundary(path, offset),
                state,
                dict,
                entries,
            };
            J::fixup(&mut e);
            self.files.insert(path.clone(), e);
            tick.parsed += 1;
            self.dirty = true;
        }
        // Files gone since last pass: drop them (their calls stay emitted).
        let present: HashSet<&Path> = found.iter().map(|(p, _)| p.as_path()).collect();
        let before = self.files.len();
        self.files.retain(|p, _| present.contains(p.as_path()));
        self.emitted.retain(|p, _| present.contains(p.as_path()));
        self.dirty |= self.files.len() != before;

        // Emit entries not yet checked against `seen` — on the first tick
        // that's every cached entry; afterwards just appended tails. Walk
        // order keeps output deterministic across runs.
        for (path, _) in found {
            let Some(e) = self.files.get(path) else {
                continue;
            };
            let from = self.emitted.get(path).copied().unwrap_or(0);
            if from >= e.entries.len() {
                continue;
            }
            let dict: Vec<Arc<str>> = e.dict.iter().map(|s| s.as_str().into()).collect();
            for c in &e.entries[from..] {
                if self.seen.insert(c.key) {
                    tick.calls.push(c.to_call(J::SOURCE, &dict));
                } else {
                    tick.dupes += 1;
                }
            }
            self.emitted.insert(path.clone(), e.entries.len());
        }
        tick.bytes = todo;
        tick
    }

    /// Persist the cache when this pass changed anything.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let sref: Vec<&Path> = self.scope.iter().map(PathBuf::as_path).collect();
        save(&format!("{}-files", J::SOURCE), &sref, &self.files);
        self.dirty = false;
    }
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
