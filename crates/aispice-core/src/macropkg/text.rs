//! Formatting shared by every macro package message.
//!
//! ChipLeap's validator (`chipleap.macro.text`) formats the same values the
//! same way, so both tools print identical messages. Change them together.

use super::schema::Val;

/// A number as C's `%.6g` prints it (and Python's `format(x, ".6g")`):
/// 20 -> `20`, 1.5e6 -> `1.5e+06`, 2.5e-5 -> `2.5e-05`.
pub fn fmt_g(x: f64) -> String {
    const P: i32 = 6;
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.into();
    }
    // The exponent after rounding to six significant digits decides between
    // fixed and exponent notation, exactly as %g does.
    let e = format!("{:.*e}", (P - 1) as usize, x);
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..P).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip_zeros(mantissa), exp.abs())
    } else {
        strip_zeros(&format!("{:.*}", (P - 1 - exp) as usize, x))
    }
}

fn strip_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

/// A name or value from a file, in double quotes with control characters
/// escaped (JSON string syntax, non-ASCII kept).
pub fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

pub fn plural(n: usize, word: &str, words: Option<&str>) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        match words {
            Some(w) => format!("{n} {w}"),
            None => format!("{n} {word}s"),
        }
    }
}

/// A TOML or JSON value in a message.
pub fn describe(v: &Val) -> String {
    match v {
        Val::Null => "null".into(),
        Val::Bool(b) => b.to_string(),
        Val::Str(s) => quote(s),
        Val::Int(i) => fmt_g(*i as f64),
        Val::Float(f) => fmt_g(*f),
        Val::Arr(_) => "an array".into(),
        Val::Obj(_) => "a table".into(),
        Val::Datetime => "a date or time".into(),
    }
}

pub fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Quoted names for a message: `none` for no names, and after the first ten
/// only how many more there are, so a file with a million cells gives a
/// short line.
pub fn names(items: &[String]) -> String {
    const SHOWN: usize = 10;
    if items.is_empty() {
        return "none".into();
    }
    let text = items
        .iter()
        .take(SHOWN)
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(", ");
    match items.len().checked_sub(SHOWN) {
        Some(rest) if rest > 0 => format!("{text} and {rest} more"),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_g_matches_printf() {
        // The same table as ChipLeap's test_fmt_g_matches_printf.
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (20.0, "20"),
            (12.5, "12.5"),
            (-40.0, "-40"),
            (1e7, "1e+07"),
            (42.0e6, "4.2e+07"),
            (25e-6, "2.5e-05"),
            (0.0001, "0.0001"),
            (0.00012345678, "0.000123457"),
            (123456.5, "123456"),
            (999999.5, "1e+06"),
            (1234567.0, "1.23457e+06"),
            (0.1 + 0.2, "0.3"),
            (28.4, "28.4"),
        ];
        for (x, want) in cases {
            assert_eq!(fmt_g(*x), *want, "{x}");
        }
    }

    #[test]
    fn quote_escapes_like_json() {
        assert_eq!(quote("a\"b\\c\n\u{1b}é"), "\"a\\\"b\\\\c\\n\\u001bé\"");
    }
}
