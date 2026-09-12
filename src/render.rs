use std::io::IsTerminal;

use crate::fmt;
use crate::pricing::Pricing;
use crate::report::{Report, Usage};

/// ANSI styling, disabled when stdout is not a terminal or NO_COLOR is set.
#[derive(Clone, Copy)]
pub struct Pal {
    on: bool,
}

impl Pal {
    pub fn detect() -> Self {
        Self {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn st(&self, s: impl AsRef<str>, code: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{}\x1b[0m", s.as_ref())
        } else {
            s.as_ref().to_string()
        }
    }

    pub fn bold(&self, s: impl AsRef<str>) -> String {
        self.st(s, "1")
    }
    pub fn dim(&self, s: impl AsRef<str>) -> String {
        self.st(s, "2")
    }
    pub fn input(&self, s: impl AsRef<str>) -> String {
        self.st(s, "36") // cyan
    }
    pub fn cached(&self, s: impl AsRef<str>) -> String {
        self.st(s, "90") // bright black
    }
    pub fn output(&self, s: impl AsRef<str>) -> String {
        self.st(s, "33") // yellow
    }
    pub fn money(&self, s: impl AsRef<str>) -> String {
        self.st(s, "31") // red — real money spent
    }
    pub fn free(&self, s: impl AsRef<str>) -> String {
        self.st(s, "32") // green — $0.00
    }
    /// List price of a free model: struck through + dimmed.
    pub fn waived(&self, s: impl AsRef<str>) -> String {
        self.st(s, "2;9")
    }
}

/// Pad a *plain* string first, then style it, so ANSI escapes don't break
/// column alignment. `align`: '<' left, '>' right.
fn pad_st(p: &Pal, s: &str, w: usize, right: bool, style: fn(&Pal, String) -> String) -> String {
    let padded = if right {
        format!("{s:>w$}")
    } else {
        format!("{s:<w$}")
    };
    style(p, padded)
}

fn bar_seg(p: &Pal, n: usize, kind: char) -> String {
    if n == 0 {
        return String::new();
    }
    let s = "█".repeat(n);
    match kind {
        'i' => p.input(s),
        'c' => p.cached(s),
        _ => p.output(s),
    }
}

/// Stacked input/cached/output bar of display width ≤ `w` for `u`.
/// Returns the rendered string and the number of cells used.
fn stacked_bar(p: &Pal, u: &Usage, w: usize) -> (String, usize) {
    let t = u.total();
    if t == 0 || w == 0 {
        return (String::new(), 0);
    }
    let mut wi = ((u.input as f64 / t as f64) * w as f64).round() as usize;
    let mut wc = ((u.cached as f64 / t as f64) * w as f64).round() as usize;
    let mut wo = ((u.output as f64 / t as f64) * w as f64).round() as usize;
    if u.input > 0 && wi == 0 {
        wi = 1;
    }
    if u.cached > 0 && wc == 0 {
        wc = 1;
    }
    if u.output > 0 && wo == 0 {
        wo = 1;
    }
    while wi + wc + wo > w {
        if wc >= wi && wc >= wo && wc > 1 {
            wc -= 1;
        } else if wi >= wo && wi > 1 {
            wi -= 1;
        } else if wo > 1 {
            wo -= 1;
        } else {
            break;
        }
    }
    (
        format!(
            "{}{}{}",
            bar_seg(p, wi, 'i'),
            bar_seg(p, wc, 'c'),
            bar_seg(p, wo, 'o')
        ),
        wi + wc + wo,
    )
}

/// (list, actual) cells for a row, pre-padded to `wl`/`wa` then styled.
/// Free rows get a struck-through list price and green $0.00.
fn cost_cells(
    p: &Pal,
    pricing: &Pricing,
    list: Option<f64>,
    wl: usize,
    wa: usize,
) -> (String, String) {
    match (pricing, list) {
        (Pricing::Free, Some(c)) => (
            pad_st(p, &fmt::money(c), wl, true, |p, s| p.waived(s)),
            pad_st(p, "$0.00", wa, true, |p, s| p.free(s)),
        ),
        (Pricing::Paid, Some(c)) => {
            let m = fmt::money(c);
            (
                pad_st(p, &m, wl, true, |_, s| s),
                pad_st(p, &m, wa, true, |p, s| p.money(s)),
            )
        }
        (Pricing::Free, None) => (
            pad_st(p, "?", wl, true, |p, s| p.dim(s)),
            pad_st(p, "$0.00", wa, true, |p, s| p.free(s)),
        ),
        _ => (
            pad_st(p, "?", wl, true, |p, s| p.dim(s)),
            pad_st(p, "?", wa, true, |p, s| p.dim(s)),
        ),
    }
}

fn hr(p: &Pal, title: &str) -> String {
    let head = format!("── {title} ");
    format!(
        "{}{}",
        p.dim(&head),
        p.dim("─".repeat(64usize.saturating_sub(head.len())))
    )
}

pub fn render(r: &Report, range_desc: &str, p: Pal) -> String {
    let p = &p;
    let mut o = String::new();
    let range = match (r.earliest, r.latest) {
        (Some(a), Some(b)) => format!("{} → {}", fmt::datetime(a), fmt::datetime(b)),
        _ => "—".into(),
    };

    // ── header ──────────────────────────────────────────────────────────
    o.push_str(&format!(
        "{}\n",
        p.bold(format!("llmstat · {range_desc} · {range}"))
    ));
    for w in &r.warnings {
        o.push_str(&format!("{}\n", p.st(format!("warning: {w}"), "31")));
    }
    o.push_str(&format!(
        "{} sessions · {} calls · {} tokens\n",
        r.sessions.len(),
        fmt::int(r.total_calls),
        p.bold(fmt::tokens(r.total.total()))
    ));
    for note in &r.coverage {
        o.push_str(&format!("{}\n", p.dim(note)));
    }
    if r.has_estimated {
        o.push_str(&format!(
            "{}\n",
            p.dim("recovered calls' cached/output splits are estimated")
        ));
    }
    o.push_str(&format!(
        "{} {} · {} {} · {} {}\n",
        p.input("input"),
        p.input(fmt::tokens(r.total.input)),
        p.cached("cached"),
        p.cached(fmt::tokens(r.total.cached)),
        p.output("output"),
        p.output(fmt::tokens(r.total.output)),
    ));
    o.push_str(&format!(
        "list (equiv.) {}   actual {}{}\n",
        fmt::money(r.list_cost),
        if r.actual_cost == 0.0 {
            p.free(fmt::money(r.actual_cost))
        } else {
            p.money(fmt::money(r.actual_cost))
        },
        if r.has_unpriced {
            p.dim("   · some models unpriced")
        } else {
            String::new()
        }
    ));
    o.push('\n');

    // ── by model ────────────────────────────────────────────────────────
    o.push_str(&hr(p, "by model"));
    o.push('\n');
    let multi_src = r
        .models
        .first()
        .is_some_and(|f| r.models.iter().any(|m| m.source != f.source));
    let lw = r
        .models
        .iter()
        .map(|m| m.label.len())
        .max()
        .unwrap_or(5)
        .clamp(5, 20);
    let pw = r
        .models
        .iter()
        .map(|m| m.priced_as.len() + 2)
        .max()
        .unwrap_or(4)
        .clamp(4, 22);
    let tw = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(120);
    // full = show in/cached/out columns; dist bar uses whatever width is left
    let src_w = if multi_src { 8 } else { 0 };
    let base_w = 2 + src_w + lw + 8 + 7 + pw + 22; // without in/cached/out or dist
    let full = base_w + 27 + 16 <= tw;
    let dist_w = if full {
        14
    } else {
        (tw.saturating_sub(base_w + 2)).min(20)
    };
    let show_dist = dist_w >= 6;

    {
        let mut h = String::new();
        if multi_src {
            h.push_str(&format!(" {:<6}", p.bold("SRC")));
        }
        h.push_str(&format!(
            " {:<lw$} {:>7} {:>6}",
            p.bold("MODEL"),
            p.bold("TOTAL"),
            p.bold("SHARE"),
            lw = lw
        ));
        if full {
            h.push_str(&format!(
                " {:>8} {:>8} {:>8}",
                p.bold("IN"),
                p.bold("CACHED"),
                p.bold("OUT")
            ));
        }
        h.push_str(&format!(
            "  {:<pw$} {:>10} {:>9}",
            p.bold("PRICED AS"),
            p.bold("LIST"),
            p.bold("ACTUAL"),
            pw = pw
        ));
        if show_dist {
            h.push_str(&format!("  {}", p.bold("DIST")));
        }
        o.push_str(&h);
        o.push('\n');
    }

    let grand = r.total.total().max(1);
    let max_t = r.models.iter().map(|m| m.usage.total()).max().unwrap_or(1);
    for m in &r.models {
        let priced_as = match m.pricing {
            Pricing::Free => format!("{} *", m.priced_as),
            _ => m.priced_as.clone(),
        };
        let (list, actual) = cost_cells(p, &m.pricing, m.list_cost(), 10, 9);
        let mut row = String::new();
        if multi_src {
            row.push_str(&format!(
                " {}",
                pad_st(p, m.source, 6, false, |p, s| p.dim(s))
            ));
        }
        row.push_str(&format!(
            " {:<lw$} {:>7} {:>5.1}%",
            m.label,
            fmt::tokens(m.usage.total()),
            m.usage.total() as f64 / grand as f64 * 100.0,
            lw = lw
        ));
        if full {
            row.push_str(&format!(
                " {:>8} {:>8} {:>8}",
                fmt::tokens(m.usage.input),
                fmt::tokens(m.usage.cached),
                fmt::tokens(m.usage.output),
            ));
        }
        row.push_str(&format!(
            "  {} {} {}",
            pad_st(p, &priced_as, pw, false, |p, s| p.dim(s)),
            list,
            actual,
        ));
        if show_dist {
            let bw = (m.usage.total() as f64 / max_t as f64 * dist_w as f64).round() as usize;
            row.push_str(&format!("  {}", stacked_bar(p, &m.usage, bw).0));
        }
        o.push_str(&row);
        o.push('\n');
    }
    o.push_str(&format!(
        "{}\n",
        p.dim(" * free in the CLI — struck list price, actual $0.00")
    ));
    o.push('\n');

    // ── timeline ────────────────────────────────────────────────────────
    if !r.buckets.is_empty() {
        o.push_str(&hr(p, "timeline"));
        o.push('\n');
        let bl = r
            .buckets
            .iter()
            .map(|b| b.label.len())
            .max()
            .unwrap_or(5)
            .max(5);
        let max_b = r
            .buckets
            .iter()
            .map(|b| b.usage.total())
            .max()
            .unwrap_or(1)
            .max(1);
        for b in &r.buckets {
            let bw = (b.usage.total() as f64 / max_b as f64 * 30.0).round() as usize;
            let (list, actual) = if b.list_cost > 0.0 {
                if b.actual_cost > 0.0 {
                    (
                        pad_st(p, &fmt::money(b.list_cost), 10, true, |_, s| s),
                        pad_st(p, &fmt::money(b.actual_cost), 9, true, |p, s| p.money(s)),
                    )
                } else {
                    (
                        pad_st(p, &fmt::money(b.list_cost), 10, true, |p, s| p.waived(s)),
                        pad_st(p, "$0.00", 9, true, |p, s| p.free(s)),
                    )
                }
            } else {
                (
                    pad_st(p, "—", 10, true, |p, s| p.dim(s)),
                    pad_st(p, "$0.00", 9, true, |p, s| p.dim(s)),
                )
            };
            let (bar, used) = stacked_bar(p, &b.usage, bw);
            o.push_str(&format!(
                " {:<bl$}  {}{} {:>8} {} {}\n",
                b.label,
                bar,
                " ".repeat(30usize.saturating_sub(used)),
                fmt::tokens(b.usage.total()),
                list,
                actual,
                bl = bl
            ));
        }
        o.push('\n');
    }

    // ── footer notes ────────────────────────────────────────────────────
    if let Some(top) = r.sessions.iter().max_by(|a, b| {
        a.list_cost
            .partial_cmp(&b.list_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        o.push_str(&format!(
            "{}\n",
            p.dim(format!(
                "heaviest session: {} — {} tok, {} list{}",
                top.name,
                fmt::tokens(top.usage.total()),
                fmt::money(top.list_cost),
                if top.actual_cost > 0.0 {
                    format!(", {} actual", fmt::money(top.actual_cost))
                } else {
                    String::new()
                }
            ))
        ));
    }
    if r.has_unpriced {
        o.push_str(&p.dim(
            "note: unpriced models excluded from costs — add [[rule]] via --pricing or ~/.config/llmstat.toml\n",
        ));
    }
    o
}
