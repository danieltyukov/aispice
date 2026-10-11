//! Liberty, far enough to list a cell's pins with their direction, supply
//! type and timing arcs. ChipLeap's `chipleap.macro.views.parse_liberty`
//! reads it the same way.

use super::lef::too_many_tokens;
use super::limits::{LimitError, Limits};
use super::text::quote;
use super::{is_ws, parse_int};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum LibertyError {
    #[error("{0}")]
    Syntax(String),
    #[error(transparent)]
    Limit(#[from] LimitError),
}

fn fail<T>(msg: impl Into<String>) -> Result<T, LibertyError> {
    Err(LibertyError::Syntax(msg.into()))
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibPin {
    pub name: String,
    pub pg: bool,
    pub pg_type: String,
    pub direction: String,
    pub timing: usize,
}

#[derive(Debug, Clone, Default)]
struct Group {
    kind: String,
    args: Vec<String>,
    attrs: BTreeMap<String, String>,
    groups: Vec<Group>,
}

/// A token, whether it starts a line, and whether it was a quoted string.
#[derive(Debug, Clone)]
struct Tok {
    text: String,
    line_start: bool,
    quoted: bool,
}

impl Tok {
    fn is(&self, punct: &str) -> bool {
        !self.quoted && self.text == punct
    }
}

fn starts(s: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, c)| s.get(i + k) == Some(&c))
}

fn tokens(text: &str, limits: &Limits) -> Result<Vec<Tok>, LibertyError> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    let push = |toks: &mut Vec<Tok>,
                text: String,
                quoted: bool,
                line_start: &mut bool|
     -> Result<(), LibertyError> {
        if toks.len() >= limits.tokens {
            return Err(too_many_tokens("liberty file", limits).into());
        }
        toks.push(Tok {
            text,
            line_start: *line_start,
            quoted,
        });
        *line_start = false;
        Ok(())
    };
    while i < n {
        let c = s[i];
        if c == '\n' {
            line_start = true;
            i += 1;
        } else if is_ws(c) {
            i += 1;
        } else if c == '\\' && i + 1 < n && (s[i + 1] == '\r' || s[i + 1] == '\n') {
            // A line continuation: the newline does not end the line.
            i += 1;
            while i < n && (s[i] == '\r' || s[i] == '\n') {
                i += 1;
            }
        } else if starts(&s, i, "/*") {
            let Some(j) = (i + 2..n).find(|&j| starts(&s, j, "*/")) else {
                return fail("unterminated /* comment");
            };
            line_start = line_start || s[i..j].contains(&'\n');
            i = j + 2;
        } else if starts(&s, i, "//") {
            while i < n && s[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            let mut j = i + 1;
            while j < n && s[j] != '"' {
                j += if s[j] == '\\' { 2 } else { 1 };
            }
            if j >= n {
                return fail("unterminated string");
            }
            push(
                &mut toks,
                s[i + 1..j].iter().collect(),
                true,
                &mut line_start,
            )?;
            i = j + 1;
        } else if "(){}:;,".contains(c) {
            push(&mut toks, c.to_string(), false, &mut line_start)?;
            i += 1;
        } else {
            let mut j = i;
            while j < n && !is_ws(s[j]) && !"(){}:;,\"".contains(s[j]) {
                if starts(&s, j, "/*") || starts(&s, j, "//") {
                    break;
                }
                j += 1;
            }
            push(&mut toks, s[i..j].iter().collect(), false, &mut line_start)?;
            i = j;
        }
    }
    Ok(toks)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    limits: &'a Limits,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// Statements up to the end of the file (top) or the closing brace, in a
    /// group nested `depth` deep. Returns the child groups and the attributes.
    #[allow(clippy::type_complexity)]
    fn body(
        &mut self,
        top: bool,
        depth: usize,
    ) -> Result<(Vec<Group>, BTreeMap<String, String>), LibertyError> {
        let mut groups = Vec::new();
        let mut attrs = BTreeMap::new();
        loop {
            let Some(tok) = self.peek().cloned() else {
                if top {
                    return Ok((groups, attrs));
                }
                return fail("unexpected end of file inside a group");
            };
            if tok.is("}") {
                if top {
                    return fail("unmatched }");
                }
                self.pos += 1;
                return Ok((groups, attrs));
            }
            if tok.is(";") {
                self.pos += 1;
                continue;
            }
            let name = tok.text;
            self.pos += 1;
            let next = self.peek().cloned();
            if next.as_ref().is_some_and(|t| t.is(":")) {
                self.pos += 1;
                let mut value: Vec<String> = Vec::new();
                while let Some(t) = self.peek() {
                    if t.is(";") || t.is("}") || (t.line_start && !value.is_empty()) {
                        break;
                    }
                    value.push(t.text.clone());
                    self.pos += 1;
                }
                if self.peek().is_some_and(|t| t.is(";")) {
                    self.pos += 1;
                }
                attrs.insert(name, value.join(" "));
            } else if next.as_ref().is_some_and(|t| t.is("(")) {
                self.pos += 1;
                let mut args: Vec<String> = Vec::new();
                let mut cur: Vec<String> = Vec::new();
                loop {
                    let Some(t) = self.peek().cloned() else {
                        return fail(format!("unclosed ( after {name}"));
                    };
                    self.pos += 1;
                    if t.is(")") {
                        break;
                    }
                    if t.is(",") {
                        args.push(cur.join(" "));
                        cur.clear();
                    } else {
                        cur.push(t.text);
                    }
                }
                if !cur.is_empty() || !args.is_empty() {
                    args.push(cur.join(" "));
                }
                if self.peek().is_some_and(|t| t.is("{")) {
                    if depth >= self.limits.nesting {
                        return Err(LimitError(format!(
                            "groups nest more than {} deep",
                            self.limits.nesting
                        ))
                        .into());
                    }
                    self.pos += 1;
                    let (children, attrs) = self.body(false, depth + 1)?;
                    groups.push(Group {
                        kind: name,
                        args,
                        attrs,
                        groups: children,
                    });
                } else if self.peek().is_some_and(|t| t.is(";")) {
                    self.pos += 1;
                }
            } else {
                return fail(format!("expected : or ( after {name}"));
            }
        }
    }
}

fn timing_count(g: &Group) -> usize {
    g.groups.iter().filter(|x| x.kind == "timing").count()
}

fn lib_pin(name: &str, g: &Group, direction: &str, timing: usize) -> LibPin {
    LibPin {
        name: name.to_string(),
        direction: g
            .attrs
            .get("direction")
            .cloned()
            .unwrap_or_else(|| direction.to_string()),
        timing: timing + timing_count(g),
        ..LibPin::default()
    }
}

/// Pins of every cell in the library group, with the default limits.
pub fn parse_liberty(
    text: &str,
) -> Result<BTreeMap<String, BTreeMap<String, LibPin>>, LibertyError> {
    parse_liberty_with(text, &Limits::DEFAULT, None)
}

/// Pins of every cell in the library group (or only of `cell`), by cell then
/// pin bit name. The first cell of a name counts; later ones are skipped.
/// Bus bits come from the bus's `bus_type` and from `pin` groups inside the
/// bus. Fails on a syntax error, and when the file has too many tokens, nests
/// groups too deep or a cell declares too many pins.
pub fn parse_liberty_with(
    text: &str,
    limits: &Limits,
    cell: Option<&str>,
) -> Result<BTreeMap<String, BTreeMap<String, LibPin>>, LibertyError> {
    let mut parser = Parser {
        toks: tokens(text, limits)?,
        pos: 0,
        limits,
    };
    let (groups, _) = parser.body(true, 0)?;
    let Some(lib) = groups.iter().find(|g| g.kind == "library") else {
        return fail("no library group");
    };
    let mut types: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for g in &lib.groups {
        if g.kind == "type" && !g.args.is_empty() {
            let from = g.attrs.get("bit_from").and_then(|v| parse_int(v));
            let to = g.attrs.get("bit_to").and_then(|v| parse_int(v));
            // A type without integer bit_from and bit_to declares no bits.
            if let (Some(a), Some(b)) = (from, to) {
                types.insert(&g.args[0], (a, b));
            }
        }
    }
    let mut cells = BTreeMap::new();
    for c in &lib.groups {
        let Some(cell_name) = c.args.first() else {
            continue;
        };
        if c.kind != "cell" || cells.contains_key(cell_name) {
            continue;
        }
        if cell.is_some_and(|want| want != cell_name) {
            continue;
        }
        let mut pins: BTreeMap<String, LibPin> = BTreeMap::new();
        // Counted before bits are made, so a bus of 2**63 bits costs nothing.
        let mut declared: i128 = 0;
        let mut declare = |n: i128| -> Result<(), LibertyError> {
            declared += n;
            if declared > limits.pins as i128 {
                return Err(LimitError(format!(
                    "cell {} has more than {} pins",
                    quote(cell_name),
                    limits.pins
                ))
                .into());
            }
            Ok(())
        };
        for g in &c.groups {
            match g.kind.as_str() {
                "pg_pin" => {
                    declare(g.args.len() as i128)?;
                    for name in &g.args {
                        pins.entry(name.clone()).or_insert_with(|| LibPin {
                            name: name.clone(),
                            pg: true,
                            pg_type: g.attrs.get("pg_type").cloned().unwrap_or_default(),
                            ..LibPin::default()
                        });
                    }
                }
                "pin" => {
                    declare(g.args.len() as i128)?;
                    for name in &g.args {
                        pins.entry(name.clone())
                            .or_insert_with(|| lib_pin(name, g, "", 0));
                    }
                }
                "bus" => {
                    let Some(bus) = g.args.first() else {
                        continue;
                    };
                    let direction = g.attrs.get("direction").cloned().unwrap_or_default();
                    let timing = timing_count(g);
                    let range = g.attrs.get("bus_type").and_then(|t| types.get(t.as_str()));
                    if let Some(&(a, b)) = range {
                        let (lo, hi) = (a.min(b), a.max(b));
                        declare(i128::from(hi) - i128::from(lo) + 1)?;
                        for i in lo..=hi {
                            let bit = format!("{bus}[{i}]");
                            pins.entry(bit.clone()).or_insert_with(|| LibPin {
                                name: bit,
                                direction: direction.clone(),
                                timing,
                                ..LibPin::default()
                            });
                        }
                    }
                    for inner in g.groups.iter().filter(|x| x.kind == "pin") {
                        declare(inner.args.len() as i128)?;
                        for name in &inner.args {
                            let p = lib_pin(name, inner, &direction, timing);
                            match pins.get_mut(name) {
                                Some(existing) => {
                                    existing.direction = p.direction;
                                    existing.timing = p.timing;
                                }
                                None => {
                                    pins.insert(name.clone(), p);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        cells.insert(cell_name.clone(), pins);
    }
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_chipleap_reads() {
        // The same input as ChipLeap's test_liberty_reader.
        let text = r#"
    library (l) {
      /* comment */ type (b4) { bit_width : 4 ; bit_from : 0 ; bit_to : 3 ; }
      date : "Mon Oct 11"
      cell ("blk") {
        pin (a, b) { direction : input ; }
        pin (y) { direction : output
          timing () { related_pin : "a" ; }
          timing () { related_pin : "b" ; } }
        bus (d) {
          bus_type : b4 ;
          direction : output ;
          timing () { related_pin : "a" ; }
          pin (d[2]) { direction : inout ; }
        }
        pg_pin (VDD) { pg_type : primary_power ; }
        comment : "multi \
          line" ;
      }
    }
    "#;
        let cells = parse_liberty(text).unwrap();
        let pins = &cells["blk"];
        let names: Vec<&str> = pins.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["VDD", "a", "b", "d[0]", "d[1]", "d[2]", "d[3]", "y"]
        );
        assert_eq!(
            (pins["y"].direction.as_str(), pins["y"].timing),
            ("output", 2)
        );
        assert_eq!(
            (pins["d[0]"].direction.as_str(), pins["d[0]"].timing),
            ("output", 1)
        );
        assert_eq!(pins["d[2]"].direction, "inout");
        assert!(pins["VDD"].pg && pins["VDD"].pg_type == "primary_power");
        assert!(parse_liberty("library (l) { cell (x) { pin (a) { direction : input ; }").is_err());
    }
}
