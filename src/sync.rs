//! `llmstat sync <host>` — mirror another machine's usage data over ssh.
//!
//! Each host gets a mirror under `~/.local/share/llmstat/hosts/<host>/`:
//!
//! ```text
//! devin/transcripts/*.json    ← ~/.local/share/devin/cli/transcripts/
//! devin/sessions.db           ← sqlite .backup of ~/.local/share/devin/cli/sessions.db
//! claude/**/*.jsonl           ← ~/.claude/projects/
//! codex/sessions/**           ← ~/.codex/sessions/
//! codex/archived_sessions/**  ← ~/.codex/archived_sessions/
//! ```
//!
//! Reports and live modes pick up every mirrored host automatically — the
//! per-source dedup keys make overlap between the local tree and a mirror
//! (resumed sessions, files copied by hand) count once. The mirror is a
//! verbatim rsync copy: `--delete` keeps it exact, so files the host
//! removed stop counting too.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Root holding every synced host's mirror.
pub fn hosts_root() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".local/share/llmstat/hosts")
}

/// `(host name, mirror root)` for each entry under `hosts_root()`,
/// sorted by name for a deterministic scan order (the file caches hash the
/// dir list — an unstable order would forfeit them).
pub fn host_mirrors() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(hosts_root()) {
        for e in rd.filter_map(|e| e.ok()) {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            out.push((name, e.path()));
        }
    }
    out.sort();
    out
}

/// One remote data path mirrored verbatim by rsync.
struct Part {
    /// Label in the summary.
    name: &'static str,
    /// Path relative to the remote home dir (rsync remote paths are
    /// home-relative).
    remote: &'static str,
    /// Dir inside the mirror receiving the tree's *contents*.
    local: &'static str,
}

/// The rsync'd trees. `sessions.db` is handled separately — it is pulled
/// through `sqlite3 .backup` so a live database yields a consistent copy.
const TREES: &[Part] = &[
    Part {
        name: "claude",
        remote: ".claude/projects",
        local: "claude",
    },
    Part {
        name: "codex/sessions",
        remote: ".codex/sessions",
        local: "codex/sessions",
    },
    Part {
        name: "codex/archived",
        remote: ".codex/archived_sessions",
        local: "codex/archived_sessions",
    },
    Part {
        name: "devin/transcripts",
        remote: ".local/share/devin/cli/transcripts",
        local: "devin/transcripts",
    },
];

const DB_REMOTE: &str = ".local/share/devin/cli/sessions.db";
const DB_LOCAL: &str = "devin/sessions.db";

/// Run `script` on `host` through `bash -s` over ssh — stdin delivery keeps
/// the login shell (fish, anything) out of the quoting business entirely.
fn ssh_script(host: &str, script: &str) -> Result<Output> {
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=15")
        .arg(host)
        .arg("bash")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("ssh failed to start")?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(script.as_bytes())?;
    child.wait_with_output().context("ssh wait failed")
}

/// Probe which remote paths exist, in one ssh round-trip.
fn probe(host: &str) -> Result<Vec<String>> {
    let paths: Vec<String> = TREES
        .iter()
        .map(|p| format!("$HOME/{}", p.remote))
        .chain(std::iter::once(format!("$HOME/{DB_REMOTE}")))
        .collect();
    let script = format!(
        "for p in {}; do [ -e \"$p\" ] && echo \"HAVE:$p\"; done",
        paths.join(" ")
    );
    let out = ssh_script(host, &script).context("ssh probe failed to start")?;
    if !out.status.success() {
        bail!(
            "ssh {host} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("HAVE:").map(str::to_string))
        .collect())
}

fn rsync_pull(host: &str, remote: &str, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    let status = Command::new("rsync")
        .arg("-a")
        .arg("--delete")
        .arg("--timeout=120")
        .arg(format!("{host}:{remote}/"))
        .arg(dst)
        .stderr(Stdio::piped())
        .status()
        .context("rsync failed to start")?;
    if !status.success() {
        bail!("rsync {host}:{remote} exited {status}");
    }
    Ok(())
}

/// Consistent snapshot of the live sessions.db via sqlite's backup API.
fn pull_db(host: &str, dst: &Path) -> Result<()> {
    let tmp = ".cache/llmstat-sessions.db";
    let backup = ssh_script(
        host,
        &format!(
            "mkdir -p \"$HOME/.cache\" && rm -f \"$HOME/{tmp}\" && sqlite3 \"$HOME/{DB_REMOTE}\" \".backup $HOME/{tmp}\""
        ),
    )
    .context("ssh backup failed to start")?;
    if !backup.status.success() {
        bail!(
            "remote sqlite backup failed: {}",
            String::from_utf8_lossy(&backup.stderr).trim()
        );
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pull = Command::new("rsync")
        .arg("-a")
        .arg(format!("{host}:{tmp}"))
        .arg(dst)
        .stderr(Stdio::piped())
        .output()
        .context("rsync failed to start")?;
    let _ = ssh_script(host, &format!("rm -f \"$HOME/{tmp}\""));
    if !pull.status.success() {
        bail!(
            "rsync of sessions.db failed: {}",
            String::from_utf8_lossy(&pull.stderr).trim()
        );
    }
    Ok(())
}

/// Files + total bytes under `dir`, for the summary line.
fn dir_size(dir: &Path) -> (usize, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for e in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if e.file_type().is_file()
            && let Ok(m) = e.metadata()
        {
            files += 1;
            bytes += m.len();
        }
    }
    (files, bytes)
}

/// Mirror `host`'s data into `hosts_root()/host/`. Parts run concurrently;
/// each reports independently.
pub fn run(host: &str) -> Result<()> {
    if host.is_empty()
        || host.contains('/')
        || host.contains('\\')
        || host.starts_with('.')
        || host.chars().any(|c| c.is_whitespace())
    {
        bail!("'{host}' is not a usable host name (it becomes a directory)");
    }
    let root = hosts_root().join(host);
    let present = probe(host)?;

    std::thread::scope(|s| {
        let mut jobs = Vec::new();
        for part in TREES {
            if !present.iter().any(|p| p.ends_with(part.remote)) {
                println!("{:<20} not on {host}", part.name);
                continue;
            }
            let dst = root.join(part.local);
            jobs.push((
                part.name,
                dst.clone(),
                s.spawn(move || rsync_pull(host, part.remote, &dst)),
            ));
        }
        if present.iter().any(|p| p.ends_with(DB_REMOTE)) {
            let dst = root.join(DB_LOCAL);
            jobs.push((
                "devin/sessions.db",
                dst.clone(),
                s.spawn(move || pull_db(host, &dst)),
            ));
        } else {
            println!("{:<20} not on {host}", "devin/sessions.db");
        }

        let mut failed = 0;
        let mut ran = 0;
        for (name, dst, h) in jobs {
            ran += 1;
            match h
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("panicked")))
            {
                Ok(()) => {
                    let (files, bytes) = dir_size(&dst);
                    println!(
                        "{:<20} {} files · {}",
                        name,
                        files,
                        crate::fmt::bytes(bytes)
                    );
                }
                Err(e) => {
                    failed += 1;
                    println!("{name:<20} failed: {e:#}");
                }
            }
        }
        if ran == 0 {
            bail!("{host} has no llmstat-readable data");
        }
        if failed > 0 {
            bail!("{failed} of {ran} parts failed");
        }
        println!("mirror at {}", root.display());
        Ok(())
    })
}
