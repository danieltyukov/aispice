//! Tokens of Verilog and Verilog-A source, for the checks that run before
//! either is compiled.
//!
//! The checks look for names (`$system`, `` `include ``, `import "DPI-C"`)
//! that must appear literally: neither language can build a system task or
//! a directive out of pieces except by SystemVerilog's `` `` `` token
//! pasting, which [`tokens`] reports so the callers can refuse it. Comments
//! are skipped as the compilers skip them. A string ends at its closing
//! quote, a backslash escaping the character after it, or at a line break
//! the compilers would refuse: what follows is then read as code, so text
//! hidden behind a broken string is still seen.

/// One token, with the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub line: usize,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A name or keyword (`module`, `q`), escaped names without their `\`.
    Ident(String),
    /// A system task or function, with its `$`: `$display`, `$value$plusargs`.
    System(String),
    /// A compiler directive or macro use, without its backtick: `include`,
    /// `define`, `WIDTH`.
    Directive(String),
    /// SystemVerilog's ``` `` ``` token pasting.
    Paste,
    /// A string literal's contents, escapes left as written.
    Str(String),
    /// Anything else: one punctuation character, or a number's text.
    Other(String),
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// The tokens of `text`.
pub fn tokens(text: &str) -> Vec<Token> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut out = Vec::new();
    let mut line = 1usize;
    let mut i = 0;
    while i < n {
        let c = s[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && s.get(i + 1) == Some(&'/') {
            while i < n && s[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && s.get(i + 1) == Some(&'*') {
            i += 2;
            while i < n && !(s[i] == '*' && s.get(i + 1) == Some(&'/')) {
                if s[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        let start = line;
        if c == '"' {
            let mut j = i + 1;
            let mut text = String::new();
            let mut closed = false;
            while j < n {
                match s[j] {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\n' => break,
                    '\\' if j + 1 < n => {
                        if s[j + 1] == '\n' {
                            line += 1;
                        }
                        text.push(s[j]);
                        text.push(s[j + 1]);
                        j += 2;
                    }
                    ch => {
                        text.push(ch);
                        j += 1;
                    }
                }
            }
            out.push(Token {
                line: start,
                kind: Kind::Str(text),
            });
            i = if closed { j + 1 } else { j };
            continue;
        }
        if c == '`' {
            if s.get(i + 1) == Some(&'`') {
                out.push(Token {
                    line: start,
                    kind: Kind::Paste,
                });
                i += 2;
                continue;
            }
            let mut j = i + 1;
            while j < n && ident_char(s[j]) {
                j += 1;
            }
            out.push(Token {
                line: start,
                kind: Kind::Directive(s[i + 1..j].iter().collect()),
            });
            i = j.max(i + 1);
            continue;
        }
        if c == '$' {
            let mut j = i + 1;
            while j < n && ident_char(s[j]) {
                j += 1;
            }
            out.push(Token {
                line: start,
                kind: Kind::System(s[i..j].iter().collect()),
            });
            i = j;
            continue;
        }
        if c == '\\' {
            // An escaped name runs to the next whitespace.
            let mut j = i + 1;
            while j < n && !s[j].is_whitespace() {
                j += 1;
            }
            out.push(Token {
                line: start,
                kind: Kind::Ident(s[i + 1..j].iter().collect()),
            });
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i;
            while j < n && ident_char(s[j]) {
                j += 1;
            }
            out.push(Token {
                line: start,
                kind: Kind::Ident(s[i..j].iter().collect()),
            });
            i = j;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n && (s[j].is_ascii_alphanumeric() || matches!(s[j], '.' | '_' | '\'')) {
                j += 1;
            }
            out.push(Token {
                line: start,
                kind: Kind::Other(s[i..j].iter().collect()),
            });
            i = j;
            continue;
        }
        out.push(Token {
            line: start,
            kind: Kind::Other(c.to_string()),
        });
        i += 1;
    }
    out
}

impl Kind {
    pub fn ident(&self) -> Option<&str> {
        match self {
            Kind::Ident(s) => Some(s),
            _ => None,
        }
    }

    pub fn is(&self, punct: &str) -> bool {
        matches!(self, Kind::Other(s) if s == punct)
    }
}

/// A module declared in the tokens: its name and the line it starts on.
pub fn modules(tokens: &[Token]) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (k, t) in tokens.iter().enumerate() {
        if matches!(t.kind.ident(), Some("module" | "macromodule"))
            && let Some(name) = tokens.get(k + 1).and_then(|n| n.kind.ident())
        {
            out.push((name.to_string(), t.line));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Kind> {
        tokens(text).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn reads_names_systems_directives_and_strings() {
        assert_eq!(
            kinds("`include \"a.vh\" $value$plusargs(\"x=%d\", v); \\esc[0] q"),
            vec![
                Kind::Directive("include".into()),
                Kind::Str("a.vh".into()),
                Kind::System("$value$plusargs".into()),
                Kind::Other("(".into()),
                Kind::Str("x=%d".into()),
                Kind::Other(",".into()),
                Kind::Ident("v".into()),
                Kind::Other(")".into()),
                Kind::Other(";".into()),
                Kind::Ident("esc[0]".into()),
                Kind::Ident("q".into()),
            ]
        );
    }

    #[test]
    fn comments_and_strings_hide_what_compilers_ignore() {
        let k = kinds("// $system(\"x\")\n/* $fopen */ x = \"$system \\\" `include\";");
        assert!(!k.iter().any(|k| matches!(k, Kind::System(_))), "{k:?}");
        assert!(!k.iter().any(|k| matches!(k, Kind::Directive(_))), "{k:?}");
        assert!(k.contains(&Kind::Str("$system \\\" `include".into())));
    }

    #[test]
    fn token_pasting_is_reported() {
        assert!(kinds("`define C(a,b) a``b").contains(&Kind::Paste));
    }

    #[test]
    fn lines_are_counted() {
        let t = tokens("a\n/* x\n y */ b\n\"s\\\ntring\" c");
        let lines: Vec<usize> = t.iter().map(|t| t.line).collect();
        assert_eq!(lines, vec![1, 3, 4, 5]);
        // A string broken by a line break ends there; the next line is code.
        let k = kinds("x = \"abc\n$system(\"id\");");
        assert!(k.contains(&Kind::System("$system".into())), "{k:?}");
        assert_eq!(
            modules(&tokens("module a(x); endmodule\nmacromodule b; endmodule")),
            vec![("a".to_string(), 1), ("b".to_string(), 2)]
        );
    }
}
