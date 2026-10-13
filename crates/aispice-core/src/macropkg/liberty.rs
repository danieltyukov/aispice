//! Liberty, far enough to list a cell's pins with their direction, supply
//! type and timing arcs. ChipLeap's `chipleap.macro.views.parse_liberty`
//! reads it the same way.

use super::lef::too_many_tokens;
use super::limits::{LimitError, Limits};
use super::text::quote;
use super::{is_ws, parse_int};
use std::collections::{BTreeMap, HashSet};

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

/// A pin of `cell`, refused when the cell already has one of its name.
fn add(pins: &mut BTreeMap<String, LibPin>, cell: &str, pin: LibPin) -> Result<(), LibertyError> {
    if pins.contains_key(&pin.name) {
        return fail(format!("pin {} of cell {cell} is defined twice", pin.name));
    }
    pins.insert(pin.name.clone(), pin);
    Ok(())
}

/// Pins of every cell in the library group (or only of `cell`), by cell then
/// pin bit name. Bus bits come from the bus's `bus_type` and from `pin`
/// groups inside the bus. Fails on a syntax error, when a cell, or a pin of
/// the cell read, is defined twice (OpenSTA keeps the last cell of a name and
/// both pins), and when the file has too many tokens, nests groups too deep or
/// a cell declares too many pins.
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
    let mut seen = HashSet::new();
    for c in &lib.groups {
        let Some(cell_name) = c.args.first() else {
            continue;
        };
        if c.kind != "cell" {
            continue;
        }
        if !seen.insert(cell_name.as_str()) {
            return fail(format!("cell {cell_name} is defined twice"));
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
                        let pin = LibPin {
                            name: name.clone(),
                            pg: true,
                            pg_type: g.attrs.get("pg_type").cloned().unwrap_or_default(),
                            ..LibPin::default()
                        };
                        add(&mut pins, cell_name, pin)?;
                    }
                }
                "pin" => {
                    declare(g.args.len() as i128)?;
                    for name in &g.args {
                        add(&mut pins, cell_name, lib_pin(name, g, "", 0))?;
                    }
                }
                "bus" => {
                    let Some(bus) = g.args.first() else {
                        continue;
                    };
                    let direction = g.attrs.get("direction").cloned().unwrap_or_default();
                    let timing = timing_count(g);
                    let range = g.attrs.get("bus_type").and_then(|t| types.get(t.as_str()));
                    let mut bits = HashSet::new();
                    if let Some(&(a, b)) = range {
                        let (lo, hi) = (a.min(b), a.max(b));
                        declare(i128::from(hi) - i128::from(lo) + 1)?;
                        for i in lo..=hi {
                            let bit = format!("{bus}[{i}]");
                            let pin = LibPin {
                                name: bit.clone(),
                                direction: direction.clone(),
                                timing,
                                ..LibPin::default()
                            };
                            add(&mut pins, cell_name, pin)?;
                            bits.insert(bit);
                        }
                    }
                    // A pin group inside the bus describes one of its bits, once.
                    let mut inner_seen = HashSet::new();
                    for inner in g.groups.iter().filter(|x| x.kind == "pin") {
                        declare(inner.args.len() as i128)?;
                        for name in &inner.args {
                            if !inner_seen.insert(name.as_str()) {
                                return fail(format!(
                                    "pin {name} of cell {cell_name} is defined twice"
                                ));
                            }
                            let p = lib_pin(name, inner, &direction, timing);
                            match pins.get_mut(name).filter(|_| bits.contains(name)) {
                                Some(existing) => {
                                    existing.direction = p.direction;
                                    existing.timing = p.timing;
                                }
                                None => add(&mut pins, cell_name, p)?,
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

    fn error(text: &str) -> String {
        parse_liberty(text).unwrap_err().to_string()
    }

    #[test]
    fn a_cell_or_pin_defined_twice_cannot_be_read() {
        // As in ChipLeap's test_a_view_that_defines_something_twice_cannot_be_read.
        let lib = |body: &str| format!("library (l) {{ cell (m) {{ {body} }} }}");
        let twice = "pin a of cell m is defined twice";
        assert_eq!(error(&lib("pin (a) { } pg_pin (a) { }")), twice);
        assert_eq!(error(&lib("pin (a, a) { }")), twice);
        let bus = "type (t) { bit_from : 0 ; bit_to : 1 ; } ";
        assert_eq!(
            error(&format!(
                "library (l) {{ {bus}cell (m) {{ pin (d[0]) {{ }} bus (d) {{ bus_type : t ; }} }} }}"
            )),
            "pin d[0] of cell m is defined twice"
        );
        assert_eq!(
            error(&format!(
                "library (l) {{ {bus}cell (m) {{ bus (d) {{ bus_type : t ; pin (d[1]) {{ }} pin (d[1]) {{ }} }} }} }}"
            )),
            "pin d[1] of cell m is defined twice"
        );
        // A bus's own pin groups describe its bits, once each.
        let cells = parse_liberty(&format!(
            "library (l) {{ {bus}cell (m) {{ bus (d) {{ bus_type : t ; direction : input ; \
             pin (d[1]) {{ direction : output ; }} }} }} }}"
        ))
        .unwrap();
        assert_eq!(cells["m"]["d[1]"].direction, "output");
        assert_eq!(cells["m"]["d[0]"].direction, "input");
        // OpenSTA keeps the last cell of a name; any cell defined twice makes
        // the file unreadable, even when another cell is the one read.
        let cells = "library (l) { cell (m) { } cell (x) { } cell (x) { } }";
        assert_eq!(error(cells), "cell x is defined twice");
        let only_m = parse_liberty_with(cells, &Limits::DEFAULT, Some("m"));
        assert_eq!(only_m.unwrap_err().to_string(), "cell x is defined twice");
    }
}
