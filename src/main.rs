mod fmt;
mod live;
mod pricing;
mod render;
mod report;
mod sources;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{Duration, Utc};
use clap::{Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

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
    /// Real-time monitor: rolling tok/s chart + per-model table (TUI).
    Live {
        /// Refresh interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
}

/// Resolved set of data sources and where to read them.
struct Sources {
    /// Lowercased names the run covers.
    wanted: HashSet<String>,
    /// `--sources` was passed explicitly (missing dirs warn, not skip).
    explicit: bool,
    devin_dir: PathBuf,
    /// sessions.db path, None under `--devin-transcripts-only`.
    devin_db: Option<PathBuf>,
    claude_dir: PathBuf,
    codex_dirs: Vec<PathBuf>,
}

fn resolve_sources(args: &Args) -> anyhow::Result<Sources> {
    // An explicitly named source errors when its data is missing; an
    // auto-detected one is skipped silently.
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
        .clone()
        .unwrap_or_else(sources::devin::default_transcripts_dir);
    let devin_db = if args.devin_transcripts_only {
        None
    } else {
        Some(
            args.devin_db
                .clone()
                .unwrap_or_else(sources::devin::default_db_path),
        )
    };
    let claude_dir = args
        .claude_dir
        .clone()
        .unwrap_or_else(sources::claude::default_dir);
    let codex_root = args.codex_dir.clone().unwrap_or_else(|| {
        std::env::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".codex")
    });
    Ok(Sources {
        wanted,
        explicit,
        devin_dir,
        devin_db,
        claude_dir,
        codex_dirs: sources::codex::dirs_for(&codex_root),
    })
}

/// Whole-pipeline spinner that appears only after ~400ms — a warm run
/// finishes before the first frame and never flashes.
fn spawn_overall_spinner<'scope, 'env>(
    s: &'scope std::thread::Scope<'scope, 'env>,
    done: Arc<AtomicBool>,
    mp: &'env MultiProgress,
) {
    s.spawn(move || {
        for _ in 0..20 {
            if done.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}").expect("static template"),
        );
        pb.set_message("scanning usage data");
        pb.enable_steady_tick(std::time::Duration::from_millis(80));
        while !done.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        pb.finish_and_clear();
    });
}

/// `llmstat live`: build resident scanners, take one full tick with the
/// usual progress UX, then hand off to the TUI loop.
fn run_live(args: &Args, interval: std::time::Duration) -> anyhow::Result<()> {
    let src = resolve_sources(args)?;
    let mut rules = pricing::load_default_rules();
    if let Some(p) = &args.pricing {
        rules.extend(pricing::load_rules(p)?);
    }

    let mut scanners: Vec<sources::AnyScanner> = Vec::new();
    if src.wanted.contains("devin") && (src.devin_dir.exists() || src.explicit) {
        scanners.push(sources::AnyScanner::devin(
            &src.devin_dir,
            src.devin_db.as_deref().filter(|p| p.exists()),
        )?);
    }
    if src.wanted.contains("claude") && (src.claude_dir.exists() || src.explicit) {
        scanners.push(sources::AnyScanner::claude(src.claude_dir.clone()));
    }
    if src.wanted.contains("codex") && (src.codex_dirs.iter().any(|d| d.exists()) || src.explicit) {
        scanners.push(sources::AnyScanner::codex(src.codex_dirs.clone()));
    }
    if scanners.is_empty() {
        anyhow::bail!("no usage data sources found");
    }

    let mp = MultiProgress::new();
    let done = Arc::new(AtomicBool::new(false));
    let mut state = live::State::new();
    let book = std::thread::scope(|s| {
        spawn_overall_spinner(s, done.clone(), &mp);
        let litellm_h = s.spawn(|| pricing::litellm::LiteBook::load(args.refresh_prices));
        let names: Vec<&'static str> = scanners.iter().map(|s| s.name()).collect();
        let mp_ref = &mp;
        let handles: Vec<_> = scanners
            .iter_mut()
            .map(|sc| s.spawn(move || sc.tick(mp_ref)))
            .collect();
        let mut all = Vec::new();
        let mut status = Vec::new();
        for (name, h) in names.iter().zip(handles) {
            match h.join() {
                Ok(Ok(calls)) => all.extend(calls),
                Ok(Err(e)) => status.push(format!("{name}: {e:#}")),
                Err(_) => status.push(format!("{name}: panicked")),
            }
        }
        let litellm = litellm_h.join().expect("litellm price load panicked");
        let book = PriceBook::new(rules, litellm);
        state.apply(std::mem::take(&mut all), &book);
        if !status.is_empty() {
            state.set_status(status.join("  ·  "));
        }
        done.store(true, Ordering::Relaxed);
        book
    });
    live::run(&mut scanners, state, &book, interval)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let mut args = Args::parse();
    let now = Utc::now();
    let (desc, since, bucket) = match args.cmd.take().unwrap_or(Cmd::All) {
        Cmd::Live { interval_ms } => {
            return run_live(
                &args,
                std::time::Duration::from_millis(interval_ms.max(200)),
            );
        }
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
    let src = resolve_sources(&args)?;
    let (wanted, explicit) = (&src.wanted, src.explicit);

    // The three scans are independent — run them concurrently under one
    // MultiProgress so their progress bars stack instead of clobbering.
    let mp = MultiProgress::new();
    type Job<'a> = (
        &'static str,
        bool,
        Box<dyn FnOnce() -> anyhow::Result<SourceOut> + Send + 'a>,
    );
    let jobs: Vec<Job<'_>> = {
        let devin_dir = src.devin_dir.clone();
        let devin_db = src.devin_db.clone();
        let claude_dir = src.claude_dir.clone();
        let codex_dirs = src.codex_dirs.clone();
        let codex_present = codex_dirs.iter().any(|d| d.exists());
        let mp = &mp;
        vec![
            (
                "devin",
                devin_dir.exists(),
                Box::new(move || {
                    sources::devin::load(&devin_dir, devin_db.as_deref().filter(|p| p.exists()), mp)
                }) as _,
            ),
            (
                "claude",
                claude_dir.exists(),
                Box::new(move || sources::claude::load(&claude_dir, mp)) as _,
            ),
            (
                "codex",
                codex_present,
                Box::new(move || sources::codex::load(&codex_dirs, mp)) as _,
            ),
        ]
    };

    let mut calls = Vec::new();
    let mut coverage = Vec::new();
    let mut warnings = Vec::new();
    let done = Arc::new(AtomicBool::new(false));
    let report = std::thread::scope(|s| {
        spawn_overall_spinner(s, done.clone(), &mp);

        let litellm_h = s.spawn(|| pricing::litellm::LiteBook::load(args.refresh_prices));
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
        let litellm = litellm_h.join().expect("litellm price load panicked");
        let book = PriceBook::new(rules, litellm);

        coverage.push(format!("prices: {}", book.source_note));
        if calls.is_empty() {
            warnings.push("no usage data found".to_string());
        }
        let t0 = std::time::Instant::now();
        let n_calls = calls.len();
        let report = report::build(calls, &book, since, bucket, coverage, warnings);
        tracing::debug!(n_calls, elapsed = ?t0.elapsed(), "report build");
        done.store(true, Ordering::Relaxed);
        report
    });
    print!("{}", render::render(&report, desc, Pal::detect()));
    Ok(())
}
