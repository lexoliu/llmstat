//! Per-CLI data sources. Each produces `Call`s plus a coverage note.

pub mod claude;
pub mod codex;
pub mod devin;
pub mod filecache;

use anyhow::Result;
use indicatif::MultiProgress;
use std::path::PathBuf;

use crate::report::Call;

pub struct SourceOut {
    pub calls: Vec<Call>,
    /// One-line coverage summary for the report header.
    pub note: String,
}

/// The closed set of source scanners for `monitor` mode — one variant per
/// CLI, each holding whatever state its incremental format needs.
pub enum AnyScanner {
    Devin(Box<devin::Scanner>),
    Claude {
        sc: claude::Scanner,
        dir: PathBuf,
    },
    Codex {
        sc: codex::Scanner,
        dirs: Vec<PathBuf>,
    },
}

impl AnyScanner {
    pub fn devin(dir: &std::path::Path, db_path: Option<&std::path::Path>) -> Result<Self> {
        Ok(Self::Devin(Box::new(devin::Scanner::open(dir, db_path)?)))
    }

    pub fn claude(dir: PathBuf) -> Self {
        Self::Claude {
            sc: claude::Scanner::open(vec![dir.clone()]),
            dir,
        }
    }

    pub fn codex(dirs: Vec<PathBuf>) -> Self {
        Self::Codex {
            sc: codex::Scanner::open(dirs.clone()),
            dirs,
        }
    }

    /// Which source this scanner emits.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Devin(_) => "devin",
            Self::Claude { .. } => "claude",
            Self::Codex { .. } => "codex",
        }
    }

    /// One scan pass. First call is the full incremental scan; later calls
    /// return only newly observed calls.
    pub fn tick(&mut self, mp: &MultiProgress) -> Result<Vec<Call>> {
        match self {
            Self::Devin(sc) => sc.tick(mp),
            Self::Claude { sc, dir } => Ok(sc.tick(&claude::walk(dir), mp).calls),
            Self::Codex { sc, dirs } => Ok(sc.tick(&codex::walk(dirs), mp).calls),
        }
    }

    /// Persist any pending incremental state (file scanners keep theirs
    /// in memory between ticks).
    pub fn save(&mut self) {
        match self {
            Self::Devin(_) => {}
            Self::Claude { sc, .. } => sc.save(),
            Self::Codex { sc, .. } => sc.save(),
        }
    }
}
