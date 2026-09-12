mod fmt;
mod pricing;
mod render;
mod report;
mod sources;

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};

use crate::pricing::PriceBook;
use crate::render::Pal;
use crate::report::BucketKind;
use crate::sources::SourceOut;

/// Token usage distribution and cost across local LLM CLIs
/// (Devin, Claude Code, Codex).
#[derive(Parser)]
#[command(name = "llmstat", version, about)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Data sources to read (comma-separated). Default: every source whose
    /// data directory exists.
    #[arg(
        long,
        value_delimiter = ',',
        value_name = "devin,claude,codex",
        global = true
    )]
    sources: Vec<String>,

    /// Devin transcript directory.
    #[arg(long, value_name = "DIR", global = true)]
    devin_transcripts: Option<PathBuf>,

    /// Devin sessions.db path (recovers calls missing from transcripts).
    #[arg(long, value_name = "FILE", global = true)]
    devin_db: Option<PathBuf>,

    /// Only count Devin transcript files; do not read sessions.db.
    #[arg(long, global = true)]
    devin_transcripts_only: bool,

    /// Claude projects directory (~/.claude/projects).
    #[arg(long, value_name = "DIR", global = true)]
    claude_dir: Option<PathBuf>,

    /// Codex root directory (~/.codex) containing sessions/ and
    /// archived_sessions/.
    #[arg(long, value_name = "DIR", global = true)]
    codex_dir: Option<PathBuf>,

    /// TOML file with extra [[rule]] pricing entries (takes precedence over
    /// LiteLLM and built-ins).
    #[arg(long, value_name = "FILE", global = true)]
    pricing: Option<PathBuf>,

    /// Re-fetch the LiteLLM pricebook even if the cache is fresh.
    #[arg(long, global = true)]
    refresh_prices: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Last 24 hours, per-hour timeline.
    #[command(visible_alias = "24h")]
    Day,
    /// Last 7 days, per-day timeline.
    Week,
    /// Last 30 days, per-day timeline.
    Month,
    /// All recorded history (default).
    All,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    let now = Utc::now();
    let (desc, since, bucket) = match args.cmd.unwrap_or(Cmd::All) {
        Cmd::Day => (
            "last 24h",
            Some(now - Duration::hours(24)),
            BucketKind::Hour,
        ),
        Cmd::Week => (
            "last 7 days",
            Some(now - Duration::days(7)),
            BucketKind::Day,
        ),
        Cmd::Month => (
            "last 30 days",
            Some(now - Duration::days(30)),
            BucketKind::Day,
        ),
        Cmd::All => ("all time", None, BucketKind::Auto),
    };

    let mut rules = pricing::load_default_rules();
    if let Some(p) = &args.pricing {
        rules.extend(pricing::load_rules(p)?);
    }
    let litellm = pricing::litellm::LiteBook::load(args.refresh_prices);
    let book = PriceBook::new(rules, litellm);

    // Resolve which sources to read. An explicitly named source errors when
    // its data is missing; an auto-detected one is skipped silently.
    let explicit = !args.sources.is_empty();
    let wanted: HashSet<String> = if explicit {
        args.sources
            .iter()
            .map(|s| s.trim().to_lowercase())
            .collect()
    } else {
        ["devin", "claude", "codex"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    };
    for s in &wanted {
        if !["devin", "claude", "codex"].contains(&s.as_str()) {
            anyhow::bail!("unknown source '{s}' (expected devin, claude, or codex)");
        }
    }

    let devin_dir = args
        .devin_transcripts
        .unwrap_or_else(sources::devin::default_transcripts_dir);
    let claude_dir = args.claude_dir.unwrap_or_else(sources::claude::default_dir);
    let codex_root = args.codex_dir.unwrap_or_else(|| {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".codex")
    });
    let codex_dirs = sources::codex::dirs_for(&codex_root);

    // The three scans are independent — run them concurrently.
    type Job = (
        &'static str,
        bool,
        Box<dyn FnOnce() -> anyhow::Result<SourceOut> + Send>,
    );
    let jobs: Vec<Job> = {
        let devin_dir = devin_dir.clone();
        let args_db = args.devin_db.clone();
        let transcripts_only = args.devin_transcripts_only;
        let claude_dir = claude_dir.clone();
        let codex_dirs = codex_dirs.clone();
        let codex_present = codex_dirs.iter().any(|d| d.exists());
        vec![
            (
                "devin",
                devin_dir.exists(),
                Box::new(move || {
                    let db = if transcripts_only {
                        None
                    } else {
                        Some(
                            args_db
                                .clone()
                                .unwrap_or_else(sources::devin::default_db_path),
                        )
                    };
                    sources::devin::load(&devin_dir, db.as_deref().filter(|p| p.exists()))
                }) as _,
            ),
            (
                "claude",
                claude_dir.exists(),
                Box::new(move || sources::claude::load(&claude_dir)) as _,
            ),
            (
                "codex",
                codex_present,
                Box::new(move || sources::codex::load(&codex_dirs)) as _,
            ),
        ]
    };

    let mut calls = Vec::new();
    let mut coverage = Vec::new();
    let mut warnings = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = jobs
            .into_iter()
            .filter(|(name, present, _)| wanted.contains(*name) && (*present || explicit))
            .map(|(name, _, f)| (name, s.spawn(f)))
            .collect();
        for (name, h) in handles {
            match h.join() {
                Ok(Ok(out)) => {
                    coverage.push(out.note);
                    calls.extend(out.calls);
                }
                // a source the user named explicitly gets a visible warning;
                // auto-detected ones just log
                Ok(Err(e)) if explicit => {
                    warnings.push(format!("{name} source failed: {e:#}"));
                }
                Ok(Err(e)) => tracing::warn!("{name} source failed: {e:#}"),
                Err(_) => warnings.push(format!("{name} source panicked")),
            }
        }
    });

    coverage.push(format!("prices: {}", book.source_note));
    if calls.is_empty() {
        warnings.push("no usage data found".to_string());
    }
    let report = report::build(calls, &book, since, bucket, coverage, warnings);
    print!("{}", render::render(&report, desc, Pal::detect()));
    Ok(())
}
