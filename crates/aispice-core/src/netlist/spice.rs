//! A SPICE netlist, read and written as text.
//!
//! The model keeps device lines split into name, nodes and the remainder
//! (value, model and parameters, kept as written), and dot-commands as text.
//! That is enough to rename nets, translate between simulator dialects, lint,
//! and lay a netlist out as a schematic, without pretending to be a full SPICE
//! front end.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Netlist {
    /// The first line of a SPICE deck is always the title.
    pub title: String,
    pub items: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Line {
    Element(Element),
    /// A dot-command, including its dot: `.tran 1m`, `.model D1N4148 D(...)`.
    Directive {
        text: String,
    },
    /// A `.subckt` definition with its body.
    Subckt(Subckt),
    Comment {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// Instance name including the device letter: `R1`, `XU1`, `Q3`.
    pub name: String,
    pub nodes: Vec<String>,
    /// Everything after the nodes, as written: `1k`, `2N3904`, `opamp Aol=100K`.
    pub rest: String,
}

impl Element {
    /// The device letter, upper-cased.
    pub fn letter(&self) -> char {
        self.name.chars().next().unwrap_or('?').to_ascii_uppercase()
    }

    /// For `X` lines, the subcircuit name; for devices with a model, the model
    /// name; for passives and sources, the value. Always the first word of
    /// `rest`.
    pub fn first_word(&self) -> Option<&str> {
        self.rest.split_whitespace().next()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subckt {
    pub name: String,
    pub ports: Vec<String>,
    /// Parameters on the `.subckt` line, as written (`Aol=100K GBW=10Meg`).
    pub params: String,
    pub body: Vec<Line>,
}

impl Netlist {
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.items.iter().filter_map(|l| match l {
            Line::Element(e) => Some(e),
            _ => None,
        })
    }

    pub fn directives(&self) -> impl Iterator<Item = &str> {
        self.items.iter().filter_map(|l| match l {
            Line::Directive { text } => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn subckts(&self) -> impl Iterator<Item = &Subckt> {
        self.items.iter().filter_map(|l| match l {
            Line::Subckt(s) => Some(s),
            _ => None,
        })
    }

    pub fn element(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// Every distinct node name used by top-level elements, in first-use order.
    pub fn nodes(&self) -> Vec<String> {
        let mut seen = Vec::<String>::new();
        for e in self.elements() {
            for n in &e.nodes {
                if !seen.iter().any(|s| s.eq_ignore_ascii_case(n)) {
                    seen.push(n.clone());
                }
            }
        }
        seen
    }

    /// Names of models defined with `.model`, upper-cased.
    pub fn model_names(&self) -> Vec<String> {
        self.directives()
            .filter_map(|d| {
                let mut words = d.split_whitespace();
                let kw = words.next()?;
                kw.eq_ignore_ascii_case(".model")
                    .then(|| words.next().map(|w| w.to_ascii_uppercase()))?
            })
            .collect()
    }

    /// The first analysis command (`.tran`, `.ac`, `.dc`, `.op`, `.noise`,
    /// `.tf`, `.pz`), if any.
    pub fn analysis(&self) -> Option<&str> {
        self.directives().find(|d| is_analysis(d))
    }
}

/// Whether a directive is an analysis command.
pub fn is_analysis(directive: &str) -> bool {
    let kw = directive
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        kw.as_str(),
        ".tran" | ".ac" | ".dc" | ".op" | ".noise" | ".tf" | ".pz" | ".sens"
    )
}

/// Parse SPICE netlist text. The first line is the title, as in every SPICE.
/// `+` continuation lines are joined; `*` lines are comments; `;` and `$ ` start
/// inline comments, which are dropped.
pub fn parse(text: &str) -> Netlist {
    let mut logical: Vec<String> = Vec::new();
    let mut lines = text.lines();
    let title = lines
        .next()
        .unwrap_or("")
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string();
    // Lines of a `.control` block are control-language commands, not
    // netlist lines: they are kept as written (a `$` there substitutes a
    // variable), and the netlist policy checks them against its allowlist.
    let mut in_control = false;
    for raw in lines {
        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();
        if let Some(cont) = trimmed.strip_prefix('+')
            && let Some(last) = logical.last_mut()
        {
            last.push(' ');
            if in_control {
                last.push_str(cont.trim());
            } else {
                last.push_str(strip_inline_comment(cont).trim());
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with(".control") {
            in_control = true;
        }
        if in_control {
            if lower.starts_with(".endc") {
                in_control = false;
            }
            logical.push(trimmed.to_string());
            continue;
        }
        if trimmed.starts_with('*') {
            logical.push(trimmed.to_string());
        } else {
            let body = strip_inline_comment(trimmed).trim();
            if !body.is_empty() {
                logical.push(body.to_string());
            }
        }
    }
    let mut iter = logical.into_iter().peekable();
    let items = parse_block(&mut iter, 0);
    Netlist { title, items }
}

/// Subcircuit definitions nest at most this deep. Real decks nest one or two
/// levels; the cap keeps a hostile file from exhausting the stack.
const MAX_SUBCKT_DEPTH: usize = 32;

fn parse_block(
    iter: &mut std::iter::Peekable<std::vec::IntoIter<String>>,
    depth: usize,
) -> Vec<Line> {
    let in_subckt = depth > 0;
    let mut items = Vec::new();
    let mut in_control = false;
    while let Some(line) = iter.next() {
        // A `.control` block, `.control` and `.endc` included, is kept line
        // by line as written, never read as elements.
        let lower_start = line.to_ascii_lowercase();
        if lower_start.starts_with(".control") {
            in_control = true;
        }
        if in_control {
            if lower_start.starts_with(".endc") {
                in_control = false;
            }
            items.push(Line::Directive { text: line });
            continue;
        }
        if let Some(c) = line.strip_prefix('*') {
            items.push(Line::Comment {
                text: c.trim().to_string(),
            });
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with(".ends") {
            if in_subckt {
                return items;
            }
            continue;
        }
        if lower == ".end" {
            break;
        }
        if lower.starts_with(".subckt") && depth >= MAX_SUBCKT_DEPTH {
            items.push(Line::Comment {
                text: format!("ignored: subcircuit nesting deeper than {MAX_SUBCKT_DEPTH}: {line}"),
            });
            continue;
        }
        if lower.starts_with(".subckt") {
            let mut words = line.split_whitespace().skip(1);
            let name = words.next().unwrap_or("").to_string();
            let mut ports = Vec::new();
            let mut params = Vec::new();
            for w in words {
                if w.contains('=') || !params.is_empty() || w.eq_ignore_ascii_case("params:") {
                    if !w.eq_ignore_ascii_case("params:") {
                        params.push(w);
                    }
                } else {
                    ports.push(w.to_string());
                }
            }
            let body = parse_block(iter, depth + 1);
            items.push(Line::Subckt(Subckt {
                name,
                ports,
                params: params.join(" "),
                body,
            }));
            continue;
        }
        if line.starts_with('.') {
            items.push(Line::Directive { text: line });
            continue;
        }
        items.push(match parse_element(&line) {
            Some(e) => Line::Element(e),
            None => Line::Comment {
                text: format!("unparsed: {line}"),
            },
        });
    }
    items
}

fn strip_inline_comment(s: &str) -> &str {
    let mut cut = s.len();
    if let Some(i) = s.find(';') {
        cut = cut.min(i);
    }
    if let Some(i) = s.find(" $ ") {
        cut = cut.min(i);
    }
    &s[..cut]
}

/// How many nodes a device line has, when it can be told from the letter.
fn fixed_node_count(letter: char) -> Option<usize> {
    Some(match letter {
        'R' | 'C' | 'L' | 'V' | 'I' | 'D' | 'B' | 'F' | 'H' | 'W' => 2,
        'J' | 'Z' => 3,
        'M' | 'E' | 'G' | 'S' | 'T' | 'O' => 4,
        'K' => 0,
        'A' => 8,
        _ => return None,
    })
}

/// Split a device line into name, nodes and the rest.
pub fn parse_element(line: &str) -> Option<Element> {
    let tokens = tokenize(line);
    let name = tokens.first()?.clone();
    let letter = name.chars().next()?.to_ascii_uppercase();
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let n = match (letter, fixed_node_count(letter)) {
        // E and G can be POLY or behavioural with fewer nodes; take two when
        // the third token is a keyword or expression.
        ('E' | 'G', Some(4)) => {
            let third = tokens
                .get(3)
                .map(|t| t.to_ascii_lowercase())
                .unwrap_or_default();
            if third.starts_with("value")
                || third.starts_with("table")
                || third.starts_with("laplace")
                || third.starts_with("poly")
                || third.contains('=')
                || third.contains('{')
            {
                2
            } else {
                4
            }
        }
        (_, Some(k)) => k,
        ('Q', None) => {
            // Q c b e [s] model [area] [params]: four nodes only if a sixth
            // token exists that looks like a model name rather than a value.
            let sixth = tokens.get(5);
            if sixth.is_some_and(|t| !t.contains('=') && crate::units::parse(t).is_none()) {
                4
            } else {
                3
            }
        }
        ('X', None) => {
            // Nodes are every token before the subcircuit name, which is the
            // last token that is not a parameter assignment.
            let first_param = tokens
                .iter()
                .skip(1)
                .position(|t| t.contains('=') || t.eq_ignore_ascii_case("params:"));
            let end = first_param.map(|p| p + 1).unwrap_or(tokens.len());
            end.saturating_sub(2)
        }
        // U (LTspice digital) and anything else: nodes until a token with '='.
        _ => tokens
            .iter()
            .skip(1)
            .take_while(|t| !t.contains('='))
            .count()
            .saturating_sub(1),
    };
    let n = n.min(tokens.len().saturating_sub(1));
    let nodes = tokens[1..1 + n].to_vec();
    let rest = tokens[1 + n..].join(" ");
    Some(Element { name, nodes, rest })
}

/// Split on whitespace, keeping parenthesised and braced groups whole so
/// `SINE(0 1 1k)` and `{R * 2}` stay one token.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    for c in line.chars() {
        match c {
            '(' | '{' | '[' => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Write the netlist as SPICE text ending in `.end`.
pub fn write(netlist: &Netlist) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        if netlist.title.is_empty() {
            "* aispice netlist"
        } else {
            &netlist.title
        }
    );
    write_block(&mut out, &netlist.items);
    out.push_str(".end\n");
    out
}

fn write_block(out: &mut String, items: &[Line]) {
    for item in items {
        match item {
            Line::Element(e) => {
                let _ = write!(out, "{}", e.name);
                for n in &e.nodes {
                    let _ = write!(out, " {n}");
                }
                if !e.rest.is_empty() {
                    let _ = write!(out, " {}", e.rest);
                }
                out.push('\n');
            }
            Line::Directive { text } => {
                let _ = writeln!(out, "{text}");
            }
            Line::Comment { text } => {
                let _ = writeln!(out, "* {text}");
            }
            Line::Subckt(s) => {
                let _ = write!(out, ".subckt {}", s.name);
                for p in &s.ports {
                    let _ = write!(out, " {p}");
                }
                if !s.params.is_empty() {
                    let _ = write!(out, " {}", s.params);
                }
                out.push('\n');
                write_block(out, &s.body);
                let _ = writeln!(out, ".ends {}", s.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A control block is kept line by line as written: its commands are
    /// not read as elements, and a `$` there is a variable, not a comment.
    #[test]
    fn a_control_block_round_trips_as_written() {
        let text = "t\nV1 a 0 1\nR1 a 0 1k\n.op\n.control\nrun\nmeas op va find v(a)\nlet x = v(a) * 2\nforeach t 1 2\necho $t\nend\nwrdata out.txt v(a)\n.endc\nR2 a 0 2k\n.end\n";
        let n = parse(text);
        let out = write(&n);
        for line in [
            ".control",
            "run",
            "meas op va find v(a)",
            "let x = v(a) * 2",
            "echo $t",
            "wrdata out.txt v(a)",
            ".endc",
        ] {
            assert!(out.lines().any(|l| l == line), "{line}: {out}");
        }
        // The netlist after the block is read as a netlist again.
        assert!(n.element("R2").is_some(), "{out}");
        assert!(n.element("run").is_none() && n.element("echo").is_none());
    }

    #[test]
    fn parses_devices_and_directives() {
        let text = "RC test\nV1 in 0 SINE(0 1 1k) AC 1\nR1 in out 1k\nC1 out 0 100n ; load\n.ac dec 20 10 100k\n.end\n";
        let n = parse(text);
        assert_eq!(n.title, "RC test");
        let v1 = n.element("v1").unwrap();
        assert_eq!(v1.nodes, vec!["in", "0"]);
        assert_eq!(v1.rest, "SINE(0 1 1k) AC 1");
        assert_eq!(n.element("C1").unwrap().rest, "100n");
        assert_eq!(n.analysis(), Some(".ac dec 20 10 100k"));
        assert_eq!(n.nodes(), vec!["in", "0", "out"]);
    }

    #[test]
    fn joins_continuations_and_reads_subckts() {
        let text = "deck\nXU1 inp inn out opamp\n+ Aol=100K GBW=10Meg\n.subckt opamp 1 2 3 Aol=1 GBW=1\nE1 3 0 2 1 {Aol}\n.ends opamp\n.end\n";
        let n = parse(text);
        let x = n.element("XU1").unwrap();
        assert_eq!(x.nodes, vec!["inp", "inn", "out"]);
        assert_eq!(x.rest, "opamp Aol=100K GBW=10Meg");
        let s = n.subckts().next().unwrap();
        assert_eq!(s.ports, vec!["1", "2", "3"]);
        assert_eq!(s.params, "Aol=1 GBW=1");
        assert_eq!(s.body.len(), 1);
    }

    #[test]
    fn bjt_node_count_heuristic() {
        assert_eq!(parse_element("Q1 c b e 2N3904").unwrap().nodes.len(), 3);
        assert_eq!(parse_element("Q1 c b e 0 2N3904").unwrap().nodes.len(), 4);
        assert_eq!(
            parse_element("Q1 c b e 2N3904 area=2").unwrap().nodes.len(),
            3
        );
        assert_eq!(parse_element("Q1 c b e 2N3904 2").unwrap().nodes.len(), 3);
    }

    #[test]
    fn behavioural_e_source_has_two_nodes() {
        let e = parse_element("E1 out 0 value={V(a)*2}").unwrap();
        assert_eq!(e.nodes, vec!["out", "0"]);
        let e = parse_element("E2 out 0 inp inn 1e5").unwrap();
        assert_eq!(e.nodes.len(), 4);
    }

    #[test]
    fn deeply_nested_subckts_do_not_overflow() {
        let mut text = String::from("deck\n");
        for i in 0..100_000 {
            text.push_str(&format!(".subckt s{i} a b\n"));
        }
        let n = parse(&text);
        assert_eq!(n.subckts().count(), 1);
    }

    #[test]
    fn write_round_trip_is_stable() {
        let text = "deck\nR1 a b 1k\n.subckt sub x y\nR1 x y 1\n.ends sub\n.op\n.end\n";
        let once = write(&parse(text));
        assert_eq!(write(&parse(&once)), once);
    }
}
