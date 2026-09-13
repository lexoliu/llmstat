//! `llmstat live` — a real-time token monitor.
//!
//! Scanners tick on an interval and emit only newly-observed calls; the
//! state below accumulates them into a rolling per-source rate chart plus
//! a per-model table. Usage lands when each API call completes, so the
//! granularity is per-call at second resolution.

use anyhow::Result;
use chrono::Utc;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use indicatif::{MultiProgress, ProgressDrawTarget};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
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
use crate::pricing::{PriceBook, Pricing, Resolved};
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

/// One per-(source, session) table row. Cost accumulates per call so a
/// session that mixed models still bills correctly; `list` is the
/// equivalent list price, `actual` what the CLI charges (free → 0).
struct Row {
    usage: Usage,
    calls: u64,
    ring: Ring,
    /// model -> tokens, to show the session's dominant model.
    models: HashMap<Arc<str>, u64>,
    list: f64,
    actual: f64,
    unpriced: bool,
}

/// Everything the dashboard shows; `apply` folds each tick's new calls in.
pub struct State {
    /// Keyed by (source, session id/name).
    rows: HashMap<(&'static str, Arc<str>), Row>,
    /// Per-source tok/s history for the chart.
    series: HashMap<&'static str, Ring>,
    /// raw model -> resolved pricing (memoized like report::build).
    resolved: HashMap<Arc<str>, usize>,
    resolved_list: Vec<Resolved>,
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
                self.resolved_list.push(book.resolve(&c.model));
                self.resolved_list.len() - 1
            });
            let r = &self.resolved_list[ridx];
            let (pricing, price) = (r.pricing.clone(), r.price);
            let sec = c.ts.map(|t| t.timestamp()).unwrap_or(now).min(now);
            let tot = c.usage.total();

            let row = self
                .rows
                .entry((c.source, c.session.clone()))
                .or_insert_with(|| Row {
                    usage: Usage::default(),
                    calls: 0,
                    ring: Ring::new(),
                    models: HashMap::new(),
                    list: 0.0,
                    actual: 0.0,
                    unpriced: false,
                });
            row.usage.add(&c.usage);
            row.calls += 1;
            row.ring.add(sec, tot);
            *row.models.entry(c.model.clone()).or_default() += tot;
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
                    row.list += cost;
                    self.list_cost += cost;
                    if matches!(pricing, Pricing::Paid) {
                        row.actual += cost;
                        self.actual_cost += cost;
                        self.session_cost += cost;
                    }
                }
                None => {
                    row.unpriced = true;
                    self.unpriced = true;
                }
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

/// Sortable table columns, in display order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Col {
    Src,
    Session,
    Model,
    Tokens,
    Calls,
    Rate,
    Cost,
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Asc,
    Desc,
}

impl Dir {
    fn flip(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

const COLS: [(Col, &str, Constraint); 7] = [
    (Col::Src, "SRC", Constraint::Length(7)),
    (Col::Session, "SESSION", Constraint::Min(20)),
    (Col::Model, "MODEL", Constraint::Length(18)),
    (Col::Tokens, "TOKENS", Constraint::Length(10)),
    (Col::Calls, "CALLS", Constraint::Length(9)),
    (Col::Rate, "RATE", Constraint::Length(10)),
    (Col::Cost, "COST", Constraint::Length(20)),
];

/// First click on a column picks this direction (metrics descend, names
/// ascend — the usual expectation).
fn default_dir(c: Col) -> Dir {
    match c {
        Col::Src | Col::Session | Col::Model => Dir::Asc,
        _ => Dir::Desc,
    }
}

/// The three vertical regions — shared by draw and mouse hit-testing.
fn areas(frame: Rect) -> (Rect, Rect, Rect) {
    let [chart, table, foot] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Min(6),
        Constraint::Length(2),
    ])
    .areas(frame);
    (chart, table, foot)
}

/// Column hit-test on the table header row (the line under the top border).
fn header_hit(frame: Rect, x: u16, y: u16) -> Option<Col> {
    let (_, t, _) = areas(frame);
    if y != t.y + 1 {
        return None;
    }
    let header = Rect::new(t.x, t.y + 1, t.width, 1);
    let widths = COLS.map(|(_, _, w)| w);
    let cells = Layout::horizontal(widths).spacing(1).split(header);
    for (i, r) in cells.iter().enumerate() {
        if x >= r.x && x < r.x + r.width {
            return Some(COLS[i].0);
        }
    }
    None
}

/// A row flattened for sorting/rendering.
struct View {
    source: &'static str,
    session: Arc<str>,
    /// Dominant model by tokens.
    model: Arc<str>,
    usage: Usage,
    calls: u64,
    rate: f64,
    list: f64,
    actual: f64,
    unpriced: bool,
}

fn draw(f: &mut Frame, st: &State, interval: Duration, sort: (Col, Dir)) {
    let now = Utc::now().timestamp();
    let (chart_a, table_a, foot_a) = areas(f.area());

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

    // ── per-session table ─────────────────────────────────────────────
    let mut views: Vec<View> = st
        .rows
        .iter()
        .map(|((source, session), r)| View {
            source,
            session: session.clone(),
            model: r
                .models
                .iter()
                .max_by_key(|(_, t)| **t)
                .map(|(m, _)| m.clone())
                .unwrap_or_default(),
            usage: r.usage,
            calls: r.calls,
            rate: r.ring.sum_since(now - RATE_WIN) as f64 / RATE_WIN as f64,
            list: r.list,
            actual: r.actual,
            unpriced: r.unpriced,
        })
        .collect();
    let (col, dir) = sort;
    views.sort_by(|a, b| {
        let ord = match col {
            Col::Src => a.source.cmp(b.source),
            Col::Session => a.session.cmp(&b.session),
            Col::Model => a.model.cmp(&b.model),
            Col::Tokens => a.usage.total().cmp(&b.usage.total()),
            Col::Calls => a.calls.cmp(&b.calls),
            Col::Rate => a.rate.total_cmp(&b.rate),
            Col::Cost => a.actual.total_cmp(&b.actual),
        };
        match dir {
            Dir::Asc => ord,
            Dir::Desc => ord.reverse(),
        }
        // stable secondary: heavier sessions first
        .then_with(|| b.usage.total().cmp(&a.usage.total()))
    });
    let num = |s: String| Cell::from(Line::from(s).alignment(Alignment::Right));
    let body = views.iter().map(|v| {
        let mut spans = Vec::new();
        if v.list > v.actual + f64::EPSILON {
            spans.push(Span::styled(
                fmt::money(v.list),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                fmt::money(v.actual),
                Style::default().fg(Color::Green),
            ));
        } else if v.unpriced && v.list == 0.0 {
            spans.push(Span::raw("?"));
        } else {
            spans.push(Span::raw(fmt::money(v.actual)));
        }
        if v.unpriced && v.list > 0.0 {
            spans.push(Span::styled(" ?", Style::default().fg(Color::DarkGray)));
        }
        TRow::new(vec![
            Cell::from(Span::styled(
                v.source.to_string(),
                Style::default().fg(color_of(v.source)),
            )),
            Cell::from(v.session.to_string()),
            Cell::from(Span::styled(
                v.model.to_string(),
                Style::default().fg(Color::DarkGray),
            )),
            num(fmt::tokens(v.usage.total())),
            num(fmt::int(v.calls as usize)),
            num(format!("{}/s", fmt::tokens(v.rate as u64))),
            Cell::from(Line::from(spans).alignment(Alignment::Right)),
        ])
    });
    let header = TRow::new(COLS.iter().map(|(c, name, _)| {
        let mut s = (*name).to_string();
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if *c == col {
            s.push(if dir == Dir::Desc { '▼' } else { '▲' });
            style = style.fg(Color::Cyan);
        }
        Cell::from(Span::styled(s, style))
    }));
    let table = Table::new(body, COLS.map(|(_, _, w)| w))
        .header(header)
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
            format!(
                "  │  {}ms tick · click header to sort · q quit",
                interval.as_millis()
            ),
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
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = Guard;
    let mut term = Terminal::new(CrosstermBackend::new(stdout))?;

    // Scanner progress must not draw over the TUI.
    let hidden = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
    let mut save_at = Instant::now() + SAVE_EVERY;
    let mut sort = (Col::Rate, Dir::Desc);
    'outer: loop {
        term.draw(|f| draw(f, &st, interval, sort))?;
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
                Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    let size = term.size()?;
                    let frame = Rect::new(0, 0, size.width, size.height);
                    if let Some(c) = header_hit(frame, m.column, m.row) {
                        sort = if sort.0 == c {
                            (c, sort.1.flip())
                        } else {
                            (c, default_dir(c))
                        };
                        continue 'outer;
                    }
                }
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
