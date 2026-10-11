//! `macro.toml` and the spec table are TOML 1.0, which ChipLeap reads with
//! Python's `tomllib`. The toml crate reads TOML 1.1, which adds four things:
//! the `\e` and `\xHH` escapes, newlines and trailing commas in inline tables,
//! and times without seconds. This finds them, so both tools refuse the same
//! files.

/// The first TOML 1.1 construct in `text`, as a message with its position,
/// or None. Only ASCII bytes matter, so it walks bytes.
pub(crate) fn toml11_syntax(text: &str) -> Option<String> {
    let s = text.as_bytes();
    let n = s.len();
    // Open arrays and inline tables, by their opening bracket.
    let mut open: Vec<u8> = Vec::new();
    let mut line_start = true;
    let mut i = 0;
    let found = |at: usize, what: &str| {
        let before = &s[..at];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |k| k + 1);
        let column = String::from_utf8_lossy(&s[start..at]).chars().count() + 1;
        Some(format!(
            "{what} is TOML 1.1; macro packages are TOML 1.0 (at line {line}, column {column})"
        ))
    };
    while i < n {
        let c = s[i];
        match c {
            b'#' => {
                i = s[i..].iter().position(|&b| b == b'\n').map_or(n, |k| i + k);
                continue;
            }
            b'"' | b'\'' => {
                match string_end(s, i) {
                    Ok(end) => i = end,
                    Err(at) => return found(at, "this escape"),
                }
                line_start = false;
                continue;
            }
            b'\n' => {
                if open.last() == Some(&b'{') {
                    return found(i, "a line break in an inline table");
                }
                line_start = true;
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            b'[' if open.is_empty() && line_start => {
                // A table header: skip it, quoted keys and all.
                i += 1;
                while i < n && s[i] != b']' && s[i] != b'\n' {
                    if s[i] == b'"' || s[i] == b'\'' {
                        match string_end(s, i) {
                            Ok(end) => i = end,
                            Err(at) => return found(at, "this escape"),
                        }
                        continue;
                    }
                    i += 1;
                }
                while i < n && s[i] == b']' {
                    i += 1;
                }
                line_start = false;
                continue;
            }
            _ => {}
        }
        line_start = false;
        match c {
            b'[' | b'{' => open.push(c),
            b']' | b'}' => {
                open.pop();
            }
            b',' if open.last() == Some(&b'{') => {
                let next = s[i + 1..].iter().find(|&&b| b != b' ' && b != b'\t');
                if next == Some(&b'}') {
                    return found(i, "a trailing comma in an inline table");
                }
            }
            // Outside strings a colon is only ever in a time. The one between
            // hours and minutes (not after a sign, as in an offset, nor after
            // the minutes) must be followed by seconds in TOML 1.0.
            b':' if i >= 2
                && i + 2 < n
                && s[i - 2].is_ascii_digit()
                && s[i - 1].is_ascii_digit()
                && s[i + 1].is_ascii_digit()
                && s[i + 2].is_ascii_digit()
                && !(i >= 3 && matches!(s[i - 3], b':' | b'+' | b'-'))
                && s.get(i + 3) != Some(&b':') =>
            {
                return found(i, "a time without seconds");
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The index just after the string that starts at `s[i]`, or the index of a
/// TOML 1.1 escape in it. An unterminated one-line string ends at the line
/// end, an unterminated multi-line one at the end.
fn string_end(s: &[u8], i: usize) -> Result<usize, usize> {
    let n = s.len();
    let q = s[i];
    let triple_at = |k: usize| k + 2 < n && s[k] == q && s[k + 1] == q && s[k + 2] == q;
    let triple = triple_at(i);
    let mut j = i + if triple { 3 } else { 1 };
    while j < n {
        let c = s[j];
        if c == b'\\' && q == b'"' {
            if matches!(s.get(j + 1), Some(b'e' | b'x')) {
                return Err(j);
            }
            j += 2;
            continue;
        }
        if c == b'\n' && !triple {
            return Ok(j);
        }
        if c == q {
            if !triple {
                return Ok(j + 1);
            }
            if triple_at(j) {
                let mut k = j + 3;
                while k < n && k < j + 5 && s[k] == q {
                    k += 1;
                }
                return Ok(k);
            }
        }
        j += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_what_tomllib_refuses() {
        for text in [
            "x = \"\\e\"",
            "x = \"\\x41\"",
            "x = \"\"\"a\\eb\"\"\"",
            "x = {a=1,}",
            "x = {a=1, }",
            "x = {a=1,\nb=2}",
            "x = {a=1, # c\nb=2}",
            "x = 07:32",
            "x = 1979-05-27T07:32Z",
            "x = 1979-05-27 07:32",
            "x = 1979-05-27T00:32-07:00",
            "[a]\nx = [{b = 1,}]",
        ] {
            assert!(toml11_syntax(text).is_some(), "{text:?}");
            assert!(toml::from_str::<toml::Table>(text).is_ok(), "{text:?}");
        }
        for text in [
            "x = 'C:\\e'",
            "x = \"\\\\e\"",
            "x = { a = [\n1] }",
            "x = 07:32:00",
            "x = 1979-05-27T00:32:00-07:00",
            "x = 1979-05-27t07:32:00.5z",
            "x = [1,]",
            "x = [\n 1, # c\n 2,\n]\n",
            "x = \"07:32\" # 07:32",
            "[\"a,}\"]\nx = 1",
            "x = '''\n{a=1,}\n'''",
        ] {
            assert_eq!(toml11_syntax(text), None, "{text:?}");
        }
        assert_eq!(
            toml11_syntax("a = 1\nx = {a=1,}").unwrap(),
            "a trailing comma in an inline table is TOML 1.1; macro packages are TOML 1.0 (at line 2, column 9)"
        );
    }
}
