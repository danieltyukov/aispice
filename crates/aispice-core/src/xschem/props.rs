//! xschem property strings: `name=R1 value="1k" m=1`.
//!
//! Tokens are separated by space, tab, newline or `;`. A value may be quoted
//! with `"`; a backslash keeps the next character, and unescaped quotes and
//! backslashes are not part of the value. This follows xschem's
//! `get_tok_value`, so a value reads here exactly as xschem's netlister sees
//! it (before any `tcleval(...)` evaluation, which aispice never does).

fn is_space(c: char) -> bool {
    matches!(c, '\n' | ' ' | '\t' | '\0' | ';')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Begin,
    Token,
    EndToken,
    Sep,
    Value,
    End,
}

/// Every `key=value` pair in order. A key written without `=` has an empty
/// value.
pub fn pairs(props: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    scan(props, |k, v| {
        out.push((k.to_string(), v.to_string()));
        false
    });
    out
}

/// The value of `key`, or `None` when the key is not there. Like xschem, the
/// first occurrence wins and a key with no value reads as empty.
pub fn get(props: &str, key: &str) -> Option<String> {
    let mut found = None;
    scan(props, |k, v| {
        if k == key {
            found = Some(v.to_string());
            true
        } else {
            false
        }
    });
    found
}

/// Whether `key` is set to a true value (`true`, `1`, `yes`, any case).
pub fn is_true(props: &str, key: &str) -> bool {
    get(props, key).is_some_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("yes")
    })
}

/// Walk the pairs, calling `f(key, value)` until it returns true. A key
/// without a value is reported with an empty value when the next token
/// starts, as xschem does; one at the very end counts as absent, also as
/// xschem does.
fn scan(props: &str, mut f: impl FnMut(&str, &str) -> bool) {
    let mut state = State::Begin;
    let mut token = String::new();
    let mut value = String::new();
    // A completed key that has not seen `=` yet.
    let mut pending: Option<String> = None;
    let mut current_key = String::new();
    let mut quote = false;
    let mut escape = false;
    for c in props.chars().chain(std::iter::once('\0')) {
        let space = is_space(c);
        if matches!(state, State::Begin | State::EndToken) && !space && c != '=' {
            state = State::Token;
        } else if state == State::Token && space && !quote && !escape {
            state = State::EndToken;
        } else if matches!(state, State::Token | State::EndToken) && c == '=' {
            state = State::Sep;
        } else if state == State::Sep && !space {
            state = State::Value;
        } else if state == State::Value && space && !quote && !escape {
            state = State::End;
        }
        if c == '"' && !escape {
            quote = !quote;
        }
        match state {
            State::Token => {
                if let Some(k) = pending.take()
                    && f(&k, "")
                {
                    return;
                }
                if escape || (c != '\\' && c != '"') {
                    token.push(c);
                }
            }
            State::Value => {
                if escape || (c != '\\' && c != '"') {
                    value.push(c);
                }
            }
            State::EndToken | State::Sep => {
                if !token.is_empty() {
                    let k = std::mem::take(&mut token);
                    if state == State::Sep {
                        current_key = k;
                        pending = None;
                    } else {
                        current_key = k.clone();
                        pending = Some(k);
                    }
                } else if state == State::Sep {
                    pending = None;
                }
            }
            State::End => {
                if f(&current_key, &value) {
                    return;
                }
                value.clear();
                current_key.clear();
                state = State::Begin;
            }
            State::Begin => {}
        }
        escape = c == '\\' && !escape;
    }
}

/// A value as it must be written so [`get`] reads it back unchanged: bare
/// when it is a plain word, otherwise in quotes with `"` and `\` escaped.
pub fn quote(value: &str) -> String {
    let plain = !value.is_empty()
        && !value
            .chars()
            .any(|c| is_space(c) || c == '"' || c == '\\' || c == '=' || c == '{' || c == '}');
    if plain {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Pairs written one per line, as xschem writes instance properties.
pub fn write(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", quote(v)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `props` with `key` set to `value`, replacing the first occurrence or
/// appending. Other text is kept as written.
pub fn set(props: &str, key: &str, value: &str) -> String {
    let mut list = pairs(props);
    match list.iter_mut().find(|(k, _)| k == key) {
        Some(p) => p.1 = value.to_string(),
        None => list.push((key.to_string(), value.to_string())),
    }
    write(&list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_quoted_and_escaped_values() {
        let p = "name=M1\nl=0.45u w=1.0u;ng=1\nvalue=\"dc 1 ac 1\" lab=a\\\"b x=\"q\\\\z\"";
        assert_eq!(get(p, "name").as_deref(), Some("M1"));
        assert_eq!(get(p, "w").as_deref(), Some("1.0u"));
        assert_eq!(get(p, "ng").as_deref(), Some("1"));
        assert_eq!(get(p, "value").as_deref(), Some("dc 1 ac 1"));
        assert_eq!(get(p, "lab").as_deref(), Some("a\"b"));
        assert_eq!(get(p, "x").as_deref(), Some("q\\z"));
        assert_eq!(get(p, "missing"), None);
    }

    #[test]
    fn nested_templates_read_in_two_steps() {
        // A symbol's K block, after the file layer removed its escapes.
        let k = "type=capacitor\nformat=\"@name @pinlist @value m=@m\"\ntemplate=\"name=C1\nm=1\nvalue=1p\ndevice=\\\"ceramic capacitor\\\"\"";
        assert_eq!(
            get(k, "format").as_deref(),
            Some("@name @pinlist @value m=@m")
        );
        let t = get(k, "template").unwrap();
        assert_eq!(get(&t, "value").as_deref(), Some("1p"));
        assert_eq!(get(&t, "device").as_deref(), Some("ceramic capacitor"));
    }

    #[test]
    fn keys_without_values_and_first_wins() {
        assert_eq!(get("embed  name=x", "embed").as_deref(), Some(""));
        assert_eq!(get("name=x embed", "embed"), None);
        assert_eq!(get("a=1 a=2", "a").as_deref(), Some("1"));
        assert_eq!(
            pairs("a=1 b c=\"x y\""),
            [
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), String::new()),
                ("c".to_string(), "x y".to_string())
            ]
        );
        assert_eq!(pairs("a=1 b"), [("a".to_string(), "1".to_string())]);
    }

    #[test]
    fn quoting_round_trips() {
        for v in [
            "1k",
            "",
            "dc 0 ac 1 sin(0 1 1k)",
            "a\"b",
            "c:\\x",
            "x;y",
            ".lib cornerMOSlv.lib mos_tt\n",
            "tcleval( @value )",
            "w=1u",
        ] {
            let p = write(&[("value".to_string(), v.to_string())]);
            assert_eq!(get(&p, "value").as_deref(), Some(v), "{p}");
        }
        assert_eq!(quote("1k"), "1k");
        assert_eq!(quote("a b"), "\"a b\"");
    }

    #[test]
    fn set_replaces_or_appends() {
        assert_eq!(set("name=R1 value=1k", "value", "2k"), "name=R1\nvalue=2k");
        assert_eq!(set("name=R1", "m", "1"), "name=R1\nm=1");
    }

    #[test]
    fn truth_values() {
        assert!(is_true("only_toplevel=true", "only_toplevel"));
        assert!(is_true("x=1", "x"));
        assert!(!is_true("x=false", "x"));
        assert!(!is_true("", "x"));
    }
}
