//! `--filter` — a small expression language over calls.
//!
//! Grammar (keywords are case-insensitive):
//!
//! ```text
//! expr   := or
//! or     := and (("or" | "|" | "||") and)*
//! and    := unary (("and" | "&" | "&&")? unary)*      // adjacency = and
//! unary  := ("not" | "!") unary | "(" or ")" | pred
//! pred   := field op value | flag ("=" bool)? | word  // bare word = model~word
//! ```
//!
//! Text fields `model`, `family` (resolved pricing label), `source`, and
//! `session` (id or title) take `=`/`!=` (exact) and `~`/`:`/`!~`/`!:`
//! (substring) — both sides normalized like pricing rules (lowercase,
//! non-alnum → `-`). `tokens`, `input`, `cached`, `output`, and `cost`
//! (the call's list-price USD) take `< <= = != >= >` against numbers with
//! optional `k`/`m`/`b` suffixes. `date` compares against `YYYY-MM-DD`
//! (the whole local day), `YYYY-MM-DDTHH:MM`, RFC3339, `today` /
//! `yesterday`, or a relative offset `Nmin`/`Nh`/`Nd`/`Nw`. Flags
//! `estimated`, `free`, `paid`, `unpriced` stand alone or take `= bool`.
//!
//! Evaluation is three-valued so pricing-dependent predicates (family,
//! cost, free/paid/unpriced) can be deferred: `eval` with `resolved:
//! None` returns `NeedPricing` iff the answer depends on the model's
//! resolved pricing.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

use crate::pricing::{self, Pricing, Resolved};
use crate::report::Call;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Contains,
    NotContains,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Contains => "~",
            Self::NotContains => "!~",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Op(Op),
    Word(String),
}

fn show(t: &Tok) -> String {
    match t {
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::And => "'and'".into(),
        Tok::Or => "'or'".into(),
        Tok::Not => "'not'".into(),
        Tok::Op(o) => format!("'{}'", o.as_str()),
        Tok::Word(w) => format!("'{w}'"),
    }
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    const SPECIAL: &str = "()&|!=~:><\"'";
    let cs: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => toks.push(Tok::LParen),
            ')' => toks.push(Tok::RParen),
            '&' => {
                toks.push(Tok::And);
                i += (cs.get(i + 1) == Some(&'&')) as usize;
            }
            '|' => {
                toks.push(Tok::Or);
                i += (cs.get(i + 1) == Some(&'|')) as usize;
            }
            '!' => match cs.get(i + 1) {
                Some('=') => {
                    toks.push(Tok::Op(Op::Ne));
                    i += 1;
                }
                Some('~') | Some(':') => {
                    toks.push(Tok::Op(Op::NotContains));
                    i += 1;
                }
                _ => toks.push(Tok::Not),
            },
            '=' => {
                toks.push(Tok::Op(Op::Eq));
                i += (cs.get(i + 1) == Some(&'=')) as usize;
            }
            '~' | ':' => toks.push(Tok::Op(Op::Contains)),
            '>' => {
                if cs.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op(Op::Ge));
                    i += 1;
                } else {
                    toks.push(Tok::Op(Op::Gt));
                }
            }
            '<' => {
                if cs.get(i + 1) == Some(&'=') {
                    toks.push(Tok::Op(Op::Le));
                    i += 1;
                } else {
                    toks.push(Tok::Op(Op::Lt));
                }
            }
            '"' | '\'' => {
                let mut w = String::new();
                let mut j = i + 1;
                while j < cs.len() && cs[j] != c {
                    w.push(cs[j]);
                    j += 1;
                }
                if j == cs.len() {
                    bail!("unterminated quote in filter");
                }
                toks.push(Tok::Word(w));
                i = j;
            }
            _ => {
                let mut w = String::new();
                while i < cs.len() {
                    let c = cs[i];
                    // A ':' between digits is word-internal (times: "14:30").
                    if c == ':'
                        && w.chars().last().is_some_and(|p| p.is_ascii_digit())
                        && cs.get(i + 1).is_some_and(|n| n.is_ascii_digit())
                    {
                        w.push(c);
                        i += 1;
                        continue;
                    }
                    if c.is_whitespace() || SPECIAL.contains(c) {
                        break;
                    }
                    w.push(c);
                    i += 1;
                }
                toks.push(Tok::Word(w));
                continue;
            }
        }
        i += 1;
    }
    Ok(toks)
}

#[derive(Clone, Copy)]
enum StrField {
    Model,
    Family,
    Source,
    Session,
}

#[derive(Clone, Copy)]
enum NumField {
    Tokens,
    Input,
    Cached,
    Output,
}

#[derive(Clone, Copy)]
enum FlagField {
    Estimated,
    Free,
    Paid,
    Unpriced,
}

enum Field {
    Str(StrField),
    Num(NumField),
    Cost,
    When,
    Flag(FlagField),
}

fn field(name: &str) -> Option<Field> {
    Some(match name.to_ascii_lowercase().as_str() {
        "model" => Field::Str(StrField::Model),
        "family" | "label" => Field::Str(StrField::Family),
        "source" | "src" => Field::Str(StrField::Source),
        "session" | "sess" => Field::Str(StrField::Session),
        "tokens" | "total" => Field::Num(NumField::Tokens),
        "input" | "in" => Field::Num(NumField::Input),
        "cached" | "cache" => Field::Num(NumField::Cached),
        "output" | "out" => Field::Num(NumField::Output),
        "cost" => Field::Cost,
        "date" | "time" | "ts" => Field::When,
        "estimated" | "est" => Field::Flag(FlagField::Estimated),
        "free" => Field::Flag(FlagField::Free),
        "paid" => Field::Flag(FlagField::Paid),
        "unpriced" => Field::Flag(FlagField::Unpriced),
        _ => return None,
    })
}

enum Pred {
    Str {
        field: StrField,
        op: Op,
        value: String,
    },
    Num {
        field: NumField,
        op: Op,
        value: u64,
    },
    Cost {
        op: Op,
        usd: f64,
    },
    When {
        op: Op,
        bound: Bound,
    },
    Flag {
        field: FlagField,
        want: bool,
    },
}

enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    Pred(Pred),
}

struct Parser<'a> {
    toks: &'a [Tok],
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.i)
    }

    fn next(&mut self) -> Option<&'a Tok> {
        let t = self.toks.get(self.i);
        self.i += t.is_some() as usize;
        t
    }

    fn word_is(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
    }

    fn parse(&mut self) -> Result<Expr> {
        let e = self.or()?;
        if let Some(t) = self.peek() {
            bail!("unexpected {} in filter", show(t));
        }
        Ok(e)
    }

    fn or(&mut self) -> Result<Expr> {
        let mut xs = vec![self.and()?];
        while matches!(self.peek(), Some(Tok::Or)) || self.word_is("or") {
            self.next();
            xs.push(self.and()?);
        }
        Ok(if xs.len() == 1 {
            xs.pop().unwrap()
        } else {
            Expr::Or(xs)
        })
    }

    fn and(&mut self) -> Result<Expr> {
        let mut xs = vec![self.unary()?];
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.next();
                }
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("and") => {
                    self.next();
                }
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("or") => break,
                // Adjacent predicate/group/negation ANDs in.
                Some(Tok::LParen) | Some(Tok::Not) | Some(Tok::Word(_)) => {}
                _ => break,
            }
            xs.push(self.unary()?);
        }
        Ok(if xs.len() == 1 {
            xs.pop().unwrap()
        } else {
            Expr::And(xs)
        })
    }

    fn unary(&mut self) -> Result<Expr> {
        match self.peek() {
            Some(Tok::Not) => {
                self.next();
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("not") => {
                self.next();
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            Some(Tok::LParen) => {
                self.next();
                let e = self.or()?;
                match self.next() {
                    Some(Tok::RParen) => Ok(e),
                    _ => bail!("unbalanced '(' in filter"),
                }
            }
            Some(Tok::Word(_)) => self.pred(),
            Some(t) => bail!("expected a predicate in filter, found {}", show(t)),
            None => bail!("filter ended early — expected a predicate"),
        }
    }

    fn pred(&mut self) -> Result<Expr> {
        let Some(Tok::Word(w)) = self.next() else {
            unreachable!("pred called on a non-word")
        };
        if let Some(Tok::Op(op)) = self.peek() {
            let op = *op;
            self.next();
            let f = field(w).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown field '{w}' — fields: model, family, source, session, \
                     date, tokens, input, cached, output, cost, estimated, free, \
                     paid, unpriced"
                )
            })?;
            let Some(Tok::Word(v)) = self.next() else {
                bail!("'{w} {}' needs a value", op.as_str());
            };
            return Ok(Expr::Pred(mk_pred(f, op, v)?));
        }
        Ok(Expr::Pred(match field(w) {
            Some(Field::Flag(f)) => Pred::Flag {
                field: f,
                want: true,
            },
            Some(_) => bail!("field '{w}' needs an operator: = != ~ : > >= < <="),
            None => Pred::Str {
                field: StrField::Model,
                op: Op::Contains,
                value: pricing::normalize(w),
            },
        }))
    }
}

fn str_op(op: Op) -> Result<Op> {
    match op {
        Op::Eq | Op::Ne | Op::Contains | Op::NotContains => Ok(op),
        o => bail!(
            "operator '{}' doesn't apply to text — use = != ~ :",
            o.as_str()
        ),
    }
}

fn cmp_op(op: Op, what: &str) -> Result<Op> {
    match op {
        Op::Contains | Op::NotContains => bail!(
            "operator '{}' doesn't apply to {what} — use = != > >= < <=",
            op.as_str()
        ),
        o => Ok(o),
    }
}

/// "123", "1.5k", "2m", "3b" — suffixes scale by 1e3/1e6/1e9.
fn scaled(s: &str) -> Result<f64> {
    let (num, mult) = match s.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&s[..s.len() - 1], 1e3),
        Some(b'm') | Some(b'M') => (&s[..s.len() - 1], 1e6),
        Some(b'b') | Some(b'B') => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let n: f64 = num
        .trim()
        .parse()
        .with_context(|| format!("'{s}' is not a number"))?;
    if n < 0.0 {
        bail!("'{s}' must be non-negative");
    }
    Ok(n * mult)
}

fn parse_bool(s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("'{s}' is not a boolean (true/false)"),
    }
}

/// "30min", "12h", "7d", "2w" → a duration back from now.
fn rel_duration(s: &str) -> Option<Duration> {
    let n_end = s.find(|c: char| c.is_ascii_alphabetic())?;
    let n: i64 = s[..n_end].parse().ok()?;
    Some(match &s[n_end..] {
        "min" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => return None,
    })
}

/// A local calendar day as a half-open [lo, hi) UTC interval.
fn day_bound(d: NaiveDate) -> Result<Bound> {
    let midnight = |d: NaiveDate| -> Result<DateTime<Utc>> {
        Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("00:00:00 exists"))
            .earliest()
            .map(|t| t.with_timezone(&Utc))
            .context("local midnight doesn't exist on that date")
    };
    let next = d
        .checked_add_days(chrono::Days::new(1))
        .context("date overflow")?;
    Ok(Bound::Day {
        lo: midnight(d)?,
        hi: midnight(next)?,
    })
}

/// Date bound: an exact instant or a whole local day. Day-grained `=`
/// matches anywhere inside the day, `<` before it, `>` after it.
enum Bound {
    At(DateTime<Utc>),
    Day {
        lo: DateTime<Utc>,
        hi: DateTime<Utc>,
    },
}

impl Bound {
    fn parse(v: &str) -> Result<Bound> {
        let lv = v.trim().to_lowercase();
        match lv.as_str() {
            "today" => return day_bound(Local::now().date_naive()),
            "yesterday" => return day_bound(Local::now().date_naive() - Duration::days(1)),
            "now" => return Ok(Bound::At(Utc::now())),
            _ => {}
        }
        if let Some(d) = rel_duration(&lv) {
            return Ok(Bound::At(Utc::now() - d));
        }
        if let Ok(d) = NaiveDate::parse_from_str(&lv, "%Y-%m-%d") {
            return day_bound(d);
        }
        for f in [
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%dT%H:%M",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d %H:%M",
        ] {
            if let Ok(t) = NaiveDateTime::parse_from_str(&lv, f) {
                let local = Local
                    .from_local_datetime(&t)
                    .earliest()
                    .with_context(|| format!("'{v}' isn't a valid local time"))?;
                return Ok(Bound::At(local.with_timezone(&Utc)));
            }
        }
        if let Ok(t) = DateTime::parse_from_rfc3339(v) {
            return Ok(Bound::At(t.with_timezone(&Utc)));
        }
        bail!(
            "invalid date '{v}' — use YYYY-MM-DD, YYYY-MM-DDTHH:MM, RFC3339, \
             today/yesterday, or an offset like 12h/7d/2w"
        )
    }

    fn test(&self, op: Op, ts: DateTime<Utc>) -> bool {
        match self {
            Self::At(t) => cmp(op, ts, *t),
            Self::Day { lo, hi } => match op {
                Op::Eq => ts >= *lo && ts < *hi,
                Op::Ne => ts < *lo || ts >= *hi,
                Op::Lt => ts < *lo,
                Op::Le => ts < *hi,
                Op::Ge => ts >= *lo,
                Op::Gt => ts >= *hi,
                Op::Contains | Op::NotContains => unreachable!("validated at parse"),
            },
        }
    }
}

fn mk_pred(f: Field, op: Op, v: &str) -> Result<Pred> {
    Ok(match f {
        Field::Str(f) => Pred::Str {
            field: f,
            op: str_op(op)?,
            value: pricing::normalize(v),
        },
        Field::Num(f) => Pred::Num {
            field: f,
            op: cmp_op(op, "a count")?,
            value: scaled(v)?.round() as u64,
        },
        Field::Cost => Pred::Cost {
            op: cmp_op(op, "cost")?,
            usd: scaled(v)?,
        },
        Field::When => Pred::When {
            op: cmp_op(op, "date")?,
            bound: Bound::parse(v)?,
        },
        Field::Flag(f) => {
            if !matches!(op, Op::Eq | Op::Ne) {
                bail!(
                    "operator '{}' doesn't apply to a flag — use = true/false",
                    op.as_str()
                );
            }
            let want = parse_bool(v)? == (op == Op::Eq);
            Pred::Flag { field: f, want }
        }
    })
}

fn str_match(op: Op, hay: &str, needle: &str) -> bool {
    match op {
        Op::Eq => hay == needle,
        Op::Ne => hay != needle,
        Op::Contains => hay.contains(needle),
        Op::NotContains => !hay.contains(needle),
        _ => unreachable!("validated at parse"),
    }
}

fn cmp<T: PartialOrd + PartialEq>(op: Op, a: T, b: T) -> bool {
    match op {
        Op::Eq => a == b,
        Op::Ne => a != b,
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
        Op::Contains | Op::NotContains => unreachable!("validated at parse"),
    }
}

/// Three-valued result — pricing predicates report `U` until the model's
/// `Resolved` is supplied. `U` propagates like SQL NULL.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    T,
    F,
    U,
}

impl Tri {
    fn of(b: bool) -> Tri {
        if b { Tri::T } else { Tri::F }
    }
}

/// How a call fared against the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    /// The expression's truth depends on resolved pricing — resolve the
    /// call's model and re-`eval` with `Some`.
    NeedPricing,
}

pub struct Filter {
    expr: Expr,
    /// Original `--filter` strings for the report's coverage line.
    raw: Vec<String>,
    /// Normalized-string memo — model/session names repeat massively
    /// across calls, so each distinct string is normalized once.
    norm: RefCell<HashMap<Arc<str>, Arc<str>>>,
}

impl Filter {
    /// Parse `--filter` occurrences into one ANDed expression — `None`
    /// when no filter was given. Each expression is parenthesized so its
    /// `or`s can't bleed across the join.
    pub fn parse(exprs: &[String]) -> Result<Option<Self>> {
        if exprs.is_empty() {
            return Ok(None);
        }
        let joined = exprs
            .iter()
            .map(|e| format!("({e})"))
            .collect::<Vec<_>>()
            .join(" and ");
        let toks = tokenize(&joined)?;
        let mut p = Parser { toks: &toks, i: 0 };
        let expr = p.parse().context("invalid --filter")?;
        Ok(Some(Self {
            expr,
            raw: exprs.to_vec(),
            norm: RefCell::new(HashMap::new()),
        }))
    }

    /// The original expression strings, one per `--filter` occurrence.
    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    /// Evaluate against a call. Pass `resolved: None` first — a
    /// `NeedPricing` verdict means the answer depends on the model's
    /// resolved pricing; resolve it and re-evaluate with `Some`.
    pub fn eval(&self, ev: &Call, resolved: Option<&Resolved>) -> Verdict {
        match self.tri(&self.expr, ev, resolved) {
            Tri::T => Verdict::Pass,
            Tri::F => Verdict::Fail,
            Tri::U => Verdict::NeedPricing,
        }
    }

    fn normalized(&self, s: &str) -> Arc<str> {
        if let Some(v) = self.norm.borrow().get(s) {
            return v.clone();
        }
        let v: Arc<str> = pricing::normalize(s).into();
        self.norm.borrow_mut().insert(Arc::from(s), v.clone());
        v
    }

    fn tri(&self, e: &Expr, ev: &Call, r: Option<&Resolved>) -> Tri {
        match e {
            Expr::And(xs) => {
                let mut u = false;
                for x in xs {
                    match self.tri(x, ev, r) {
                        Tri::F => return Tri::F,
                        Tri::U => u = true,
                        Tri::T => {}
                    }
                }
                if u { Tri::U } else { Tri::T }
            }
            Expr::Or(xs) => {
                let mut u = false;
                for x in xs {
                    match self.tri(x, ev, r) {
                        Tri::T => return Tri::T,
                        Tri::U => u = true,
                        Tri::F => {}
                    }
                }
                if u { Tri::U } else { Tri::F }
            }
            Expr::Not(x) => match self.tri(x, ev, r) {
                Tri::T => Tri::F,
                Tri::F => Tri::T,
                Tri::U => Tri::U,
            },
            Expr::Pred(p) => self.pred(p, ev, r),
        }
    }

    fn pred(&self, p: &Pred, ev: &Call, r: Option<&Resolved>) -> Tri {
        match p {
            Pred::Str { field, op, value } => match field {
                StrField::Model => Tri::of(str_match(*op, &self.normalized(&ev.model), value)),
                StrField::Source => Tri::of(str_match(*op, &self.normalized(ev.source), value)),
                StrField::Session => Tri::of(
                    str_match(*op, &self.normalized(&ev.session), value)
                        || ev
                            .session_name
                            .as_ref()
                            .is_some_and(|n| str_match(*op, &self.normalized(n), value)),
                ),
                StrField::Family => match r {
                    None => Tri::U,
                    Some(r) => Tri::of(str_match(*op, &self.normalized(&r.label), value)),
                },
            },
            Pred::Num { field, op, value } => {
                let n = match field {
                    NumField::Tokens => ev.usage.total(),
                    NumField::Input => ev.usage.input,
                    NumField::Cached => ev.usage.cached,
                    NumField::Output => ev.usage.output,
                };
                Tri::of(cmp(*op, n, *value))
            }
            Pred::Cost { op, usd } => match r {
                None => Tri::U,
                Some(r) => match &r.price {
                    Some(p) => Tri::of(cmp(*op, ev.usage.cost(p), *usd)),
                    None => Tri::F,
                },
            },
            Pred::When { op, bound } => match ev.ts {
                None => Tri::F,
                Some(ts) => Tri::of(bound.test(*op, ts)),
            },
            Pred::Flag { field, want } => match field {
                FlagField::Estimated => Tri::of(ev.estimated == *want),
                f => match r {
                    None => Tri::U,
                    Some(r) => {
                        let b = match f {
                            FlagField::Free => matches!(r.pricing, Pricing::Free),
                            FlagField::Paid => matches!(r.pricing, Pricing::Paid),
                            FlagField::Unpriced => matches!(r.pricing, Pricing::Unpriced),
                            FlagField::Estimated => unreachable!(),
                        };
                        Tri::of(b == *want)
                    }
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Price;
    use crate::report::Usage;

    fn filt(expr: &str) -> Filter {
        Filter::parse(&[expr.to_string()]).unwrap().unwrap()
    }

    fn call(source: &'static str, model: &str) -> Call {
        Call {
            source,
            session: "sess-1".into(),
            session_name: Some("fix the login bug".into()),
            model: model.into(),
            ts: None,
            usage: Usage {
                input: 500,
                cached: 2000,
                output: 100,
            },
            estimated: false,
        }
    }

    fn resolved(label: &str, pricing: Pricing, price: Option<Price>) -> Resolved {
        Resolved {
            label: label.into(),
            pricing,
            price,
            priced_as: label.into(),
        }
    }

    #[test]
    fn model_matching() {
        let c = call("devin", "SWE-2 Max");
        assert_eq!(filt("model~swe-2").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("model:swe-2").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("model=swe-2-max").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("model=swe-2").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("model!~swe").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("model!=swe-2-max").eval(&c, None), Verdict::Fail);
        // bare word = model substring
        assert_eq!(filt("swe-2").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("opus").eval(&c, None), Verdict::Fail);
    }

    #[test]
    fn source_and_session() {
        let c = call("claude", "claude-opus-5");
        assert_eq!(filt("source=claude").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("source!=claude").eval(&c, None), Verdict::Fail);
        // session matches id or human name
        assert_eq!(filt("session~sess-1").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("session~login").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("session~nope").eval(&c, None), Verdict::Fail);
    }

    #[test]
    fn logic() {
        let c = call("devin", "SWE-2 Max");
        assert_eq!(
            filt("model~swe and source=devin").eval(&c, None),
            Verdict::Pass
        );
        assert_eq!(
            filt("model~swe source=claude").eval(&c, None),
            Verdict::Fail
        );
        assert_eq!(
            filt("model~opus or model~swe").eval(&c, None),
            Verdict::Pass
        );
        assert_eq!(filt("not model~swe").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("!(source=devin)").eval(&c, None), Verdict::Fail);
        assert_eq!(
            filt("(model~opus or model~swe) && source=devin").eval(&c, None),
            Verdict::Pass
        );
        assert_eq!(
            filt("model~swe or source=claude and tokens>1b").eval(&c, None),
            Verdict::Pass
        );
    }

    #[test]
    fn numbers() {
        let c = call("codex", "gpt-5");
        assert_eq!(filt("tokens>1k").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("tokens>3k").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("tokens=2600").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("input>=500").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("cached<1k").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("output<=100").eval(&c, None), Verdict::Pass);
    }

    #[test]
    fn dates() {
        let mut c = call("devin", "SWE-2");
        c.ts = Some(Utc::now());
        assert_eq!(filt("date>7d").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("date=today").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("date>1h").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("date<1h").eval(&c, None), Verdict::Fail);
        c.ts = Some(Utc::now() - Duration::days(30));
        assert_eq!(filt("date>7d").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("date<7d").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("date!=today").eval(&c, None), Verdict::Pass);
        // timestamped-only field: no ts → every date pred fails
        c.ts = None;
        assert_eq!(filt("date>7d").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("not date>7d").eval(&c, None), Verdict::Pass);
    }

    #[test]
    fn pricing_preds_defer() {
        let c = call("devin", "SWE-2");
        let f = filt("free");
        assert_eq!(f.eval(&c, None), Verdict::NeedPricing);
        let r = resolved(
            "SWE-2",
            Pricing::Free,
            Some(Price {
                input: 3.0,
                cached: 0.3,
                output: 15.0,
            }),
        );
        assert_eq!(f.eval(&c, Some(&r)), Verdict::Pass);
        assert_eq!(filt("paid").eval(&c, Some(&r)), Verdict::Fail);
        assert_eq!(filt("family~swe").eval(&c, Some(&r)), Verdict::Pass);
        assert_eq!(filt("family~kimi").eval(&c, Some(&r)), Verdict::Fail);
        // cost: usage 500 in + 2000 cached + 100 out at 3/0.3/15 → $3.6 per 1k
        assert_eq!(filt("cost>0.001").eval(&c, Some(&r)), Verdict::Pass);
        assert_eq!(filt("cost>1").eval(&c, Some(&r)), Verdict::Fail);
        // unpriced model fails cost preds outright
        let un = resolved("mystery", Pricing::Unpriced, None);
        assert_eq!(filt("cost>0").eval(&c, Some(&un)), Verdict::Fail);
        assert_eq!(filt("unpriced").eval(&c, Some(&un)), Verdict::Pass);
    }

    #[test]
    fn flags_and_bool() {
        let mut c = call("devin", "SWE-2");
        assert_eq!(filt("estimated").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("estimated=false").eval(&c, None), Verdict::Pass);
        c.estimated = true;
        assert_eq!(filt("estimated").eval(&c, None), Verdict::Pass);
        assert_eq!(filt("estimated!=true").eval(&c, None), Verdict::Fail);
    }

    #[test]
    fn resolve_is_lazy() {
        let c = call("devin", "SWE-2 Max");
        // pure predicates never ask for pricing
        assert_eq!(filt("model~swe").eval(&c, None), Verdict::Pass);
        // a decided side answers without resolving the other
        assert_eq!(filt("model~nope and free").eval(&c, None), Verdict::Fail);
        assert_eq!(filt("model~swe or free").eval(&c, None), Verdict::Pass);
        // undecided → caller resolves and re-evals
        let f = filt("model~opus or free");
        assert_eq!(f.eval(&c, None), Verdict::NeedPricing);
        assert_eq!(
            f.eval(&c, Some(&resolved("SWE-2", Pricing::Free, None))),
            Verdict::Pass
        );
    }

    #[test]
    fn multiple_exprs_are_anded() {
        let c = call("devin", "SWE-2 Max");
        let f = Filter::parse(&["model~swe or model~opus".into(), "source=devin".into()])
            .unwrap()
            .unwrap();
        assert_eq!(f.eval(&c, None), Verdict::Pass);
        let f = Filter::parse(&["model~swe or model~opus".into(), "source=claude".into()])
            .unwrap()
            .unwrap();
        assert_eq!(f.eval(&c, None), Verdict::Fail);
    }

    #[test]
    fn errors() {
        assert!(Filter::parse(&["bogus>5".into()]).is_err());
        assert!(Filter::parse(&["model>5".into()]).is_err());
        assert!(Filter::parse(&["tokens~5".into()]).is_err());
        assert!(Filter::parse(&["(model~swe".into()]).is_err());
        assert!(Filter::parse(&["model~".into()]).is_err());
        assert!(Filter::parse(&["date>blah".into()]).is_err());
        assert!(Filter::parse(&["cost=abc".into()]).is_err());
        assert!(Filter::parse(&["free~x".into()]).is_err());
        assert!(Filter::parse(&[String::new()]).is_err());
    }
}
