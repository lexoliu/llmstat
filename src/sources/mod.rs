//! Per-CLI data sources. Each produces `Call`s plus a coverage note.

pub mod claude;
pub mod codex;
pub mod devin;
pub mod filecache;

use crate::report::Call;

pub struct SourceOut {
    pub calls: Vec<Call>,
    /// One-line coverage summary for the report header.
    pub note: String,
}
