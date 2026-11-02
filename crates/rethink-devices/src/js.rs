//! JavaScript string-to-number conversions, for handlers that must accept exactly the MQTT
//! payloads their upstream TypeScript counterparts accept. `None` stands for `NaN`.

fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$` — the decimal literals `Number()` accepts.
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - int_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - frac_start;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

/// `Number(s)`: surrounding whitespace ignored, `""` is 0, `0x`/`0o`/`0b` prefixes and
/// `Infinity` accepted.
pub fn number(s: &str) -> Option<f64> {
    let t = s.trim_matches(is_js_space);
    if t.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            return Some(digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            }));
        }
    }
    match t {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    if is_decimal_literal(t) {
        t.parse().ok()
    } else {
        None
    }
}

/// `parseInt(s, 10)`: leading whitespace and sign, then the longest run of decimal digits.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start_matches(is_js_space);
    let (negative, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    let value: i64 = digits[..end].parse().ok()?;
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_matches_javascript() {
        for (input, expected) in [
            ("2", Some(2.0)),
            (" 36 ", Some(36.0)),
            ("36.0", Some(36.0)),
            ("4.5", Some(4.5)),
            ("", Some(0.0)),
            ("   ", Some(0.0)),
            ("0x2a", Some(42.0)),
            ("0b101", Some(5.0)),
            ("0o17", Some(15.0)),
            ("4.2e1", Some(42.0)),
            (".5", Some(0.5)),
            ("5.", Some(5.0)),
            ("-3", Some(-3.0)),
            ("+3", Some(3.0)),
            ("-Infinity", Some(f64::NEG_INFINITY)),
            ("abc", None),
            ("3abc", None),
            ("0x", None),
            ("-0x10", None),
            ("inf", None),
            ("NaN", None),
            ("1e", None),
            (".", None),
            ("1_000", None),
        ] {
            assert_eq!(number(input), expected, "{input:?}");
        }
    }

    #[test]
    fn parse_int_matches_javascript() {
        for (input, expected) in [
            ("2", Some(2)),
            ("2.5", Some(2)),
            (" 3", Some(3)),
            ("3abc", Some(3)),
            ("-7", Some(-7)),
            ("+7", Some(7)),
            ("", None),
            ("abc", None),
            ("-", None),
            ("0x10", Some(0)),
        ] {
            assert_eq!(parse_int(input), expected, "{input:?}");
        }
    }
}
