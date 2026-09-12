/// Grouped integer: 26815 -> "26,815".
pub fn int(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Human-readable token count: 123 -> "123", 12_345 -> "12.3K", 152_400_000 -> "152.4M"
pub fn tokens(n: u64) -> String {
    const UNITS: [(f64, &str); 3] = [(1e9, "B"), (1e6, "M"), (1e3, "K")];
    for (u, s) in UNITS {
        let v = n as f64 / u;
        if v >= 1.0 {
            return if v >= 100.0 {
                format!("{v:.0}{s}")
            } else if v >= 10.0 {
                format!("{v:.1}{s}")
            } else {
                format!("{v:.2}{s}")
            };
        }
    }
    n.to_string()
}

/// "$1,234.56"; small non-zero values show as "<$0.01".
pub fn money(x: f64) -> String {
    if x != 0.0 && x.abs() < 0.005 {
        return "<$0.01".to_string();
    }
    let neg = x < 0.0;
    let x = x.abs();
    let int_part = x.trunc() as u64;
    let frac = ((x.fract() * 100.0).round() as u64).min(99);
    let digits = int_part.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}${grouped}.{frac:02}", if neg { "-" } else { "" })
}

/// "Sep 12 14:03" local time.
pub fn datetime(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.with_timezone(&chrono::Local)
        .format("%b %d %H:%M")
        .to_string()
}
