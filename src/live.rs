//! `llmstat live` — a real-time token monitor.
//!
//! Scanners tick on an interval and emit only newly-observed calls; the
//! state below accumulates them into a rolling per-source rate chart plus
//! a per-model table. Usage lands when each API call completes, so the
//! granularity is per-call at second resolution.

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use indicatif::{MultiProgress, ProgressDrawTarget};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row as TRow, Table,
};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::io::{IsTerminal, Stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::fmt;
use crate::pricing::{Price, PriceBook, Pricing, Resolved};
use crate::report::{Call, Usage};
use crate::sources::AnyScanner;

/// Seconds of per-second history kept for the chart and rate columns.
const WIN: usize = 600;
/// The RATE column averages over this many seconds.
const RATE_WIN: i64 = 30;
/// Chart line smoothing: moving average over this many seconds.
const SMOOTH: usize = 5;
/// Persist dirty scanner caches at most this often.
const SAVE_EVERY: Duration = Duration::from_secs(30);

/// Per-second token counts over the last `WIN` seconds. Each slot is
/// tagged with its epoch second, so stale data never reads as current.
struct Ring {
    slots: Box<[(i64, u64)]>,
}

impl Ring {
    fn new() -> Self {
        Self {
            slots: vec![(0, 0); WIN].into_boxed_slice(),
        }
    }

    fn add(&mut self, sec: i64, n: u64) {
        let i = sec.rem_euclid(WIN as i64) as usize;
        if self.slots[i].0 != sec {
            self.slots[i] = (sec, 0);
        }
        self.slots[i].1 += n;
    }

    /// Tokens recorded in `(since, now]`-ish — slots older than the window
    /// carry a stale epoch tag and are skipped.
    fn sum_since(&self, since: i64) -> u64 {
        self.slots
            .iter()
            .filter(|(s, _)| *s > since)
            .map(|(_, v)| *v)
            .sum()
    }

    /// `(-age_seconds, smoothed tok/s)` points covering the whole window.
    fn series(&self, now: i64) -> Vec<(f64, f64)> {
        let lo = now - WIN as i64 + 1;
        let raw: Vec<u64> = (lo..=now)
            .map(|sec| {
                let i = sec.rem_euclid(WIN as i64) as usize;
                if self.slots[i].0 == sec {
                    self.slots[i].1
                } else {
                    0
                }
            })
            .collect();
        raw.iter()
            .enumerate()
            .map(|(j, _)| {
                let from = j.saturating_sub(SMOOTH - 1);
                let s: u64 = raw[from..=j].iter().sum();
                (
                    (j as i64 + lo - now) as f64,
                    s as f64 / (j - from + 1) as f64,
                )
            })
            .collect()
    }
}

/// One per-(source, model-label) table row.
struct Row {
    pricing: Pricing,
    price: Option<Price>,
    usage: Usage,
    calls: u64,
    ring: Ring,
}

/// Everything the dashboard shows; `apply` folds each tick's new calls in.
pub struct State {
    rows: HashMap<(&'static str, Arc<str>), Row>,
    /// Per-source tok/s history for the chart.
    series: HashMap<&'static str, Ring>,
    /// raw model -> resolved pricing (memoized like report::build).
    resolved: HashMap<Arc<str>, usize>,
    resolved_list: Vec<(Arc<str>, Resolved)>,
    /// All-time totals across every call seen.
    total: Usage,
    calls: u64,
    list_cost: f64,
    actual_cost: f64,
    unpriced: bool,
    /// Since `run` started.
    session_usage: Usage,
    session_calls: u64,
    session_cost: f64,
    /// Transient note in the footer (scanner errors, etc).
    status: String,
}

impl State {
    pub fn new() -> Self {
        Self {
            rows: HashMap::new(),
            series: HashMap::new(),
            resolved: HashMap::new(),
            resolved_list: Vec::new(),
            total: Usage::default(),
            calls: 0,
            list_cost: 0.0,
            actual_cost: 0.0,
            unpriced: false,
            session_usage: Usage::default(),
            session_calls: 0,
            session_cost: 0.0,
            status: String::new(),
        }
    }

    pub fn set_status(&mut self, s: String) {
        self.status = s;
    }

    pub fn apply(&mut self, calls: Vec<Call>, book: &PriceBook) {
        let now = Utc::now().timestamp();
        for c in calls {
            let ridx = *self.resolved.entry(c.model.clone()).or_insert_with(|| {
                let r = book.resolve(&c.model);
                self.resolved_list.push((r.label.as_str().into(), r));
                self.resolved_list.len() - 1
            });
            let (label, pricing, price) = {
                let (l, r) = &self.resolved_list[ridx];
                (l.clone(), r.pricing.clone(), r.price)
            };
            let sec = c.ts.map(|t| t.timestamp()).unwrap_or(now).min(now);
            let tot = c.usage.total();

            let row = self.rows.entry((c.source, label)).or_insert_with(|| Row {
                pricing: pricing.clone(),
                price,
                usage: Usage::default(),
                calls: 0,
                ring: Ring::new(),
            });
            row.usage.add(&c.usage);
            row.calls += 1;
            row.ring.add(sec, tot);
            self.series
                .entry(c.source)
                .or_insert_with(Ring::new)
                .add(sec, tot);

            self.total.add(&c.usage);
            self.calls += 1;
            self.session_usage.add(&c.usage);
            self.session_calls += 1;
            match price {
                Some(p) => {
                    let cost = c.usage.cost(&p);
                    self.list_cost += cost;
                    if matches!(pricing, Pricing::Paid) {
                        self.actual_cost += cost;
                        self.session_cost += cost;
                    }
                }
                None => self.unpriced = true,
            }
        }
    }

    /// Tokens/s across all sources, averaged over `secs`.
    fn rate(&self, secs: i64) -> f64 {
        let now = Utc::now().timestamp();
        self.series
            .values()
            .map(|r| r.sum_since(now - secs) as f64)
            .sum::<f64>()
            / secs as f64
    }
}

fn color_of(source: &str) -> Color {
    match source {
        "devin" => Color::Cyan,
        "claude" => Color::Yellow,
        _ => Color::Magenta,
    }
}

fn draw(f: &mut Frame, st: &State, interval: Duration) {
    let now = Utc::now().timestamp();
    let [chart_a, table_a, foot_a] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .areas(f.area());

    // ── rolling rate chart ────────────────────────────────────────────
    let mut data = Vec::new();
    let mut ymax = 1.0f64;
    for s in ["devin", "claude", "codex"] {
        if let Some(r) = st.series.get(s) {
            let pts = r.series(now);
            ymax = pts.iter().map(|p| p.1).fold(ymax, f64::max);
            data.push((s, pts));
        }
    }
    let datasets: Vec<Dataset> = data
        .iter()
        .map(|(name, pts)| {
            Dataset::default()
                .name(*name)
                .marker(symbols::Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color_of(name)))
                .data(pts)
        })
        .collect();
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " llmstat live · tokens/s · last 10m · now {}/s ",
                    fmt::tokens(st.rate(SMOOTH as i64) as u64)
                ))
                .title_alignment(Alignment::Left),
        )
        .x_axis(
            Axis::default()
                .bounds([-(WIN as f64), 0.0])
                .labels(vec![
                    Span::from("-10m"),
                    Span::from("-5m"),
                    Span::from("now"),
                ])
                .style(Style::default().fg(Color::DarkGray)),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, ymax])
                .labels(vec![
                    Span::from("0"),
                    Span::from(fmt::tokens((ymax / 2.0) as u64)),
                    Span::from(fmt::tokens(ymax as u64)),
                ])
                .style(Style::default().fg(Color::DarkGray)),
        );
    f.render_widget(chart, chart_a);

    // ── per-model table ───────────────────────────────────────────────
    let mut rows: Vec<(&(&'static str, Arc<str>), &Row)> = st.rows.iter().collect();
    rows.sort_by_key(|(_, r)| std::cmp::Reverse(r.usage.total()));
    let body = rows.iter().map(|((source, label), r)| {
        let rate = r.ring.sum_since(now - RATE_WIN) as f64 / RATE_WIN as f64;
        let num = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
        let cost = match r.price {
            Some(p) => {
                let list = fmt::money(r.usage.cost(&p));
                match r.pricing {
                    Pricing::Free => Cell::from(
                        Line::from(vec![
                            Span::styled(
                                list,
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::CROSSED_OUT),
                            ),
                            Span::raw(" "),
                            Span::styled("$0.00", Style::default().fg(Color::Green)),
                        ])
                        .alignment(Alignment::Right),
                    ),
                    _ => num(list),
                }
            }
            None => num("?".into()),
        };
        TRow::new(vec![
            Cell::from(Span::styled(
                source.to_string(),
                Style::default().fg(color_of(source)),
            )),
            Cell::from(label.to_string()),
            num(fmt::tokens(r.usage.total())),
            num(fmt::int(r.calls as usize)),
            num(format!("{}/s", fmt::tokens(rate as u64))),
            cost,
        ])
    });
    let table = Table::new(
        body,
        [
            Constraint::Length(7),
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(18),
        ],
    )
    .header(
        TRow::new(vec!["SRC", "MODEL", "TOKENS", "CALLS", "RATE", "COST"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::TOP));
    f.render_widget(table, table_a);

    // ── footer: session delta · all-time totals · status ──────────────
    let mut spans = vec![
        Span::styled(
            format!(
                "+{} tok · {} calls · {}",
                fmt::tokens(st.session_usage.total()),
                fmt::int(st.session_calls as usize),
                fmt::money(st.session_cost)
            ),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!(
                "  │  total {} tok · {} calls · {}{}",
                fmt::tokens(st.total.total()),
                fmt::int(st.calls as usize),
                fmt::money(st.actual_cost),
                if st.list_cost > st.actual_cost {
                    format!(" (list {})", fmt::money(st.list_cost))
                } else {
                    String::new()
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  │  {}ms tick · q quit", interval.as_millis()),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if st.unpriced {
        spans.push(Span::styled(
            "  │  ? = unpriced",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let status = if st.status.is_empty() {
        Line::default()
    } else {
        Line::from(Span::styled(
            format!(" {} ", st.status),
            Style::default().fg(Color::Red),
        ))
    };
    f.render_widget(Paragraph::new(vec![Line::from(spans), status]), foot_a);
}

/// Restore the terminal on drop.
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// Event loop: redraw every `interval`, tick scanners between draws.
/// Returns the scanners so caches can be flushed on exit.
pub fn run(
    scanners: &mut [AnyScanner],
    mut st: State,
    book: &PriceBook,
    interval: Duration,
) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("`live` needs a terminal");
    }
    enable_raw_mode()?;
    let mut stdout: Stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = Guard;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    // Scanner progress must not draw over the TUI.
    let hidden = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
    let mut save_at = Instant::now() + SAVE_EVERY;
    'outer: loop {
        term.draw(|f| draw(f, &st, interval))?;
        let deadline = Instant::now() + interval;
        while event::poll(deadline.saturating_duration_since(Instant::now()))? {
            match event::read()? {
                Event::Key(k)
                    if k.kind == KeyEventKind::Press
                        && (matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                            || (k.code == KeyCode::Char('c')
                                && k.modifiers.contains(KeyModifiers::CONTROL))) =>
                {
                    break 'outer;
                }
                Event::Resize(_, _) => continue 'outer,
                _ => {}
            }
        }
        for sc in scanners.iter_mut() {
            match sc.tick(&hidden) {
                Ok(calls) => {
                    if !calls.is_empty() {
                        tracing::debug!(src = sc.name(), n = calls.len(), "tick emitted");
                    }
                    st.apply(calls, book);
                }
                Err(e) => st.status = format!("{}: {e:#}", sc.name()),
            }
        }
        if Instant::now() >= save_at {
            for sc in scanners.iter_mut() {
                sc.save();
            }
            save_at = Instant::now() + SAVE_EVERY;
        }
    }
    for sc in scanners.iter_mut() {
        sc.save();
    }
    Ok(())
}
