pub const NS_PER_MS: i64 = 1_000_000;
pub const SECOND_MS: i64 = 1_000;
pub const MINUTE_MS: i64 = 60 * SECOND_MS;
pub const HOUR_MS: i64 = 60 * MINUTE_MS;
pub const DAY_MS: i64 = 24 * HOUR_MS;

/// A span such as `90s`, `30m`, `1h30m`, or `2d`, in milliseconds.
pub fn parse(text: &str) -> Option<i64> {
    let mut total: i64 = 0;
    let mut digits = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: i64 = digits.parse().ok()?;
        let unit = match c {
            's' => SECOND_MS,
            'm' => MINUTE_MS,
            'h' => HOUR_MS,
            'd' => DAY_MS,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
        digits.clear();
    }
    (digits.is_empty() && total > 0).then_some(total)
}

/// A span of milliseconds in its two largest units, such as `1h 5m` or `4m 12s`.
pub fn format(ms: i64) -> String {
    if ms < SECOND_MS {
        return format!("{ms}ms");
    }
    let parts = [
        (ms / DAY_MS, "d"),
        (ms / HOUR_MS % 24, "h"),
        (ms / MINUTE_MS % 60, "m"),
        (ms / SECOND_MS % 60, "s"),
    ];
    let first = parts.iter().position(|&(n, _)| n > 0).unwrap_or(3);
    let mut out = format!("{}{}", parts[first].0, parts[first].1);
    if let Some(&(n, unit)) = parts.get(first + 1).filter(|&&(n, _)| n > 0) {
        out.push_str(&format!(" {n}{unit}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_take_compound_units() {
        assert_eq!(parse("90s"), Some(90_000));
        assert_eq!(parse("1h30m"), Some(5_400_000));
        assert_eq!(parse("2d"), Some(172_800_000));
        assert_eq!(parse("15"), None);
        assert_eq!(parse("5x"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn spans_too_long_to_count_and_zero_spans_are_rejected() {
        assert_eq!(parse("9999999999999999999d"), None);
        assert_eq!(parse("999999999999999d"), None);
        assert_eq!(parse("0s"), None);
        assert_eq!(parse("1h0m"), Some(3_600_000));
    }

    #[test]
    fn spans_show_their_two_largest_units() {
        assert_eq!(format(450), "450ms");
        assert_eq!(format(42_000), "42s");
        assert_eq!(format(252_000), "4m 12s");
        assert_eq!(format(3_600_000), "1h");
        assert_eq!(format(90_061_000), "1d 1h");
    }
}
