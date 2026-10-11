//! Liberty, far enough to list a cell's pins with their direction, supply
//! type and timing arcs. ChipLeap's `chipleap.macro.views.parse_liberty`
//! reads it the same way.

use super::{is_ws, parse_int};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LibertyError(pub String);

fn fail<T>(msg: impl Into<String>) -> Result<T, LibertyError> {
    Err(LibertyError(msg.into()))
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

fn tokens(text: &str) -> Result<Vec<Tok>, LibertyError> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    let push = |toks: &mut Vec<Tok>, text: String, quoted: bool, line_start: &mut bool| {
        toks.push(Tok {
            text,
            line_start: *line_start,
            quoted,
        });
        *line_start = false;
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
            );
            i = j + 1;
        } else if "(){}:;,".contains(c) {
            push(&mut toks, c.to_string(), false, &mut line_start);
            i += 1;
        } else {
            let mut j = i;
            while j < n && !is_ws(s[j]) && !"(){}:;,\"".contains(s[j]) {
                if starts(&s, j, "/*") || starts(&s, j, "//") {
                    break;
                }
                j += 1;
            }
            push(&mut toks, s[i..j].iter().collect(), false, &mut line_start);
            i = j;
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    /// Statements up to the end of the file (top) or the closing brace.
    /// Returns the child groups and the attributes.
    #[allow(clippy::type_complexity)]
    fn body(&mut self, top: bool) -> Result<(Vec<Group>, BTreeMap<String, String>), LibertyError> {
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
                    self.pos += 1;
                    let (children, attrs) = self.body(false)?;
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

/// Pins of every cell in the library group, by cell then pin bit name. Bus
/// bits come from the bus's `bus_type` and from `pin` groups inside the bus.
pub fn parse_liberty(
    text: &str,
) -> Result<BTreeMap<String, BTreeMap<String, LibPin>>, LibertyError> {
    let mut parser = Parser {
        toks: tokens(text)?,
        pos: 0,
    };
    let (groups, _) = parser.body(true)?;
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
    for cell in &lib.groups {
        if cell.kind != "cell" || cell.args.is_empty() {
            continue;
        }
        let mut pins: BTreeMap<String, LibPin> = BTreeMap::new();
        for g in &cell.groups {
            match g.kind.as_str() {
                "pg_pin" => {
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
                    for name in &g.args {
                        pins.entry(name.clone())
                            .or_insert_with(|| lib_pin(name, g, "", 0));
                    }
                }
                "bus" if !g.args.is_empty() => {
                    let direction = g.attrs.get("direction").cloned().unwrap_or_default();
                    let timing = timing_count(g);
                    let range = g.attrs.get("bus_type").and_then(|t| types.get(t.as_str()));
                    if let Some(&(a, b)) = range {
                        for i in a.min(b)..=a.max(b) {
                            let bit = format!("{}[{i}]", g.args[0]);
                            pins.entry(bit.clone()).or_insert_with(|| LibPin {
                                name: bit,
                                direction: direction.clone(),
                                timing,
                                ..LibPin::default()
                            });
                        }
                    }
                    for inner in g.groups.iter().filter(|x| x.kind == "pin") {
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
        cells.entry(cell.args[0].clone()).or_insert(pins);
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
