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

/// Truncate to at most `w` characters (adding `…` if cut) so over-long names
/// cannot overflow a fixed-width table column.
fn trunc(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w <= 1 {
        return "…".repeat(w);
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
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
    let tw = terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .unwrap_or(120);
    render_with_width(r, range_desc, p, tw)
}

fn render_with_width(r: &Report, range_desc: &str, p: Pal, tw: usize) -> String {
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
        .map(|m| m.label.chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 28);
    let pw = r
        .models
        .iter()
        .map(|m| {
            m.priced_as.chars().count()
                + if matches!(m.pricing, Pricing::Free) {
                    2
                } else {
                    0
                }
        })
        .max()
        .unwrap_or(9)
        .clamp(9, 28);
    // full = show in/cached/out columns; dist bar uses whatever width is left
    // "antigravity" is the longest source id.
    const SRC_W: usize = 12;
    let src_w = if multi_src { SRC_W + 2 } else { 0 };
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
            h.push_str(&format!(
                " {}",
                pad_st(p, "SRC", SRC_W, false, |p, s| p.bold(s))
            ));
        }
        h.push_str(&format!(
            " {} {} {}",
            pad_st(p, "MODEL", lw, false, |p, s| p.bold(s)),
            pad_st(p, "TOTAL", 7, true, |p, s| p.bold(s)),
            pad_st(p, "SHARE", 6, true, |p, s| p.bold(s)),
        ));
        if full {
            h.push_str(&format!(
                " {} {} {}",
                pad_st(p, "IN", 8, true, |p, s| p.bold(s)),
                pad_st(p, "CACHED", 8, true, |p, s| p.bold(s)),
                pad_st(p, "OUT", 8, true, |p, s| p.bold(s)),
            ));
        }
        h.push_str(&format!(
            "  {} {} {}",
            pad_st(p, "PRICED AS", pw, false, |p, s| p.bold(s)),
            pad_st(p, "LIST", 10, true, |p, s| p.bold(s)),
            pad_st(p, "ACTUAL", 9, true, |p, s| p.bold(s)),
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
        let label = trunc(&m.label, lw);
        let priced_as = trunc(&priced_as, pw);
        let (list, actual) = cost_cells(p, &m.pricing, m.list_cost(), 10, 9);
        let mut row = String::new();
        if multi_src {
            row.push_str(&format!(
                " {}",
                pad_st(p, &trunc(m.source, SRC_W), SRC_W, false, |p, s| p.dim(s))
            ));
        }
        row.push_str(&format!(
            " {:<lw$} {:>7} {:>5.1}%",
            label,
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

    // ── did you know ────────────────────────────────────────────────────
    let kwh = r.energy_j / 3.6e6;
    if kwh > 0.0 {
        let eqs = crate::energy::equivalences(kwh);
        o.push_str(&format!(
            "\n{}\n",
            p.bold(format!(
                "did you know? {} tokens ≈ {} of serving energy",
                fmt::tokens(r.total.total()),
                fmt::energy(kwh)
            ))
        ));
        if !eqs.is_empty() {
            o.push_str(&format!("  ≈ {}\n", p.dim(eqs.join(" · "))));
        }
        o.push_str(&format!(
            "  ≈ {} at the US industrial rate (${}/kWh){}\n",
            p.money(fmt::money(kwh * crate::energy::INDUSTRIAL_USD_KWH)),
            crate::energy::INDUSTRIAL_USD_KWH,
            if r.energy_inferred {
                p.dim("   · order-of-magnitude estimate — some models' energy is inferred from list price")
            } else {
                p.dim("   · order-of-magnitude estimate")
            }
        ));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Price;
    use crate::report::ModelStat;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn by_model_columns_align_with_ansi_and_long_names() {
        let p = Some(Price {
            input: 1.5,
            cached: 0.075,
            output: 7.5,
        });
        let report = Report {
            models: vec![
                ModelStat {
                    source: "antigravity",
                    label: "Gemini 4 Hydrogen".into(),
                    raw_names: vec![],
                    usage: Usage {
                        input: 59_300_000,
                        cached: 1_200_000_000,
                        output: 5_330_000,
                    },
                    sessions: 10,
                    steps: 100,
                    pricing: Pricing::Paid,
                    price: p,
                    priced_as: "gemini-gemini-4-hydrogen".into(),
                },
                ModelStat {
                    source: "devin",
                    label: "SWE-2".into(),
                    raw_names: vec![],
                    usage: Usage {
                        input: 369_000_000,
                        cached: 10_600_000_000,
                        output: 45_000_000,
                    },
                    sessions: 5,
                    steps: 50,
                    pricing: Pricing::Free,
                    price: p,
                    priced_as: "kimi-k3".into(),
                },
                ModelStat {
                    source: "antigravity",
                    label: "claude-opus-4-6-thinking".into(),
                    raw_names: vec![],
                    usage: Usage {
                        input: 16_100,
                        cached: 72_100,
                        output: 147,
                    },
                    sessions: 1,
                    steps: 2,
                    pricing: Pricing::Paid,
                    price: p,
                    priced_as: "claude-opus-4-6".into(),
                },
            ],
            sessions: vec![],
            buckets: vec![],
            total: Usage {
                input: 428_316_100,
                cached: 11_800_072_100,
                output: 50_330_147,
            },
            total_calls: 152,
            list_cost: 5000.0,
            actual_cost: 218.56,
            has_unpriced: false,
            coverage: vec![],
            warnings: vec![],
            earliest: None,
            latest: None,
            has_estimated: false,
            energy_j: 0.0,
            energy_inferred: false,
        };

        let rendered = strip_ansi(&render_with_width(
            &report,
            "all time",
            Pal { on: true },
            180,
        ));
        let lines: Vec<&str> = rendered.lines().collect();
        let hdr_idx = lines
            .iter()
            .position(|l| l.contains("SRC") && l.contains("MODEL"))
            .unwrap();
        let hdr = lines[hdr_idx];
        let row0 = lines[hdr_idx + 1];
        let row1 = lines[hdr_idx + 2];
        let row2 = lines[hdr_idx + 3];

        // Column start/end positions must match between the ANSI-styled header and every row.
        assert_eq!(hdr.find("MODEL"), row0.find("Gemini 4 Hydrogen"));
        assert_eq!(hdr.find("MODEL"), row1.find("SWE-2"));
        assert_eq!(hdr.find("MODEL"), row2.find("claude-opus-4-6-thinking"));

        assert_eq!(hdr.find("PRICED AS"), row0.find("gemini-gemini-4-hydrogen"));
        assert_eq!(hdr.find("PRICED AS"), row1.find("kimi-k3 *"));
        assert_eq!(hdr.find("PRICED AS"), row2.rfind("claude-opus-4-6"));

        let actual_end = hdr.find("ACTUAL").unwrap() + "ACTUAL".len();
        assert_eq!(actual_end, row1.find("$0.00").unwrap() + "$0.00".len());
        assert_eq!(actual_end, row2.rfind("$0.03").unwrap() + "$0.03".len());
        assert_eq!(
            hdr.find("DIST"),
            row0.find('█'),
            "header={hdr:?}\nrow0  ={row0:?}"
        );
        assert_eq!(hdr.find("DIST"), row1.find('█'));
    }
}
