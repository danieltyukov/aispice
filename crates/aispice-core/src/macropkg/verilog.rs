//! Verilog, far enough to list a module's ports with direction and width.
//! ChipLeap's `chipleap.macro.views.verilog_ports` reads it the same way.

use super::{is_ws, parse_int};

#[derive(Debug, Clone, PartialEq)]
pub struct VerilogPort {
    pub name: String,
    /// input, output, inout; empty when the module never declares it.
    pub direction: String,
    /// None for a scalar; Some(0) when the range is not a literal.
    pub width: Option<u32>,
}

const DIRECTIONS: [&str; 3] = ["input", "output", "inout"];
const TYPE_WORDS: [&str; 18] = [
    "wire",
    "reg",
    "logic",
    "wreal",
    "real",
    "var",
    "signed",
    "unsigned",
    "tri",
    "tri0",
    "tri1",
    "supply0",
    "supply1",
    "wand",
    "wor",
    "bit",
    "integer",
    "interconnect",
];

fn is_direction(t: &str) -> bool {
    DIRECTIONS.contains(&t)
}

fn starts(s: &[char], i: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, c)| s.get(i + k) == Some(&c))
}

fn find(s: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..s.len()).find(|&j| starts(s, j, pat))
}

/// Drop comments, attributes and preprocessor lines; keep both sides of an
/// `ifdef`.
fn clean(text: &str) -> String {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if starts(&s, i, "//") {
            while i < n && s[i] != '\n' {
                i += 1;
            }
        } else if starts(&s, i, "/*") {
            i = find(&s, i + 2, "*/").map_or(n, |j| j + 2);
            out.push(' ');
        } else if starts(&s, i, "(*") && !starts(&s, i, "(*)") {
            i = find(&s, i + 2, "*)").map_or(n, |j| j + 2);
            out.push(' ');
        } else if s[i] == '"' {
            let mut j = i + 1;
            while j < n && s[j] != '"' {
                j += if s[j] == '\\' { 2 } else { 1 };
            }
            out.push_str("\"\"");
            i = j + 1;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out.split('\n')
        .map(|line| {
            if line.trim_start_matches(is_ws).starts_with('`') {
                " "
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_$`'".contains(c)
}

fn tokens(text: &str) -> Vec<String> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < n {
        let c = s[i];
        if is_ws(c) {
            i += 1;
        } else if is_word(c) {
            let mut j = i;
            while j < n && is_word(s[j]) {
                j += 1;
            }
            toks.push(s[i..j].iter().collect());
            i = j;
        } else if c == '\\' {
            let mut j = i;
            while j < n && !is_ws(s[j]) {
                j += 1;
            }
            toks.push(s[i + 1..j].iter().collect());
            i = j;
        } else {
            toks.push(c.to_string());
            i += 1;
        }
    }
    toks
}

/// Width of `[ msb : lsb ]` given the tokens between the brackets; 0 if unknown.
fn range_width(toks: &[String]) -> u32 {
    if toks.len() == 3 && toks[1] == ":" {
        let unsigned = |t: &str| !t.starts_with(['+', '-']);
        if let (Some(msb), Some(lsb)) = (parse_int(&toks[0]), parse_int(&toks[2]))
            && unsigned(&toks[0])
            && unsigned(&toks[2])
        {
            return ((msb - lsb).unsigned_abs() + 1) as u32;
        }
    }
    0
}

/// Skip net and data type words and one packed range from `toks[k]`. Returns
/// the index of the name and the range's width (None without a range).
fn types_and_range(toks: &[String], mut k: usize) -> (usize, Option<u32>) {
    let mut width = None;
    while k < toks.len() {
        if TYPE_WORDS.contains(&toks[k].as_str()) {
            k += 1;
        } else if toks[k] == "[" && width.is_none() {
            let j = toks[k..]
                .iter()
                .position(|t| t == "]")
                .map_or(toks.len(), |p| k + p);
            width = Some(range_width(&toks[(k + 1).min(j)..j]));
            k = j + 1;
        } else {
            break;
        }
    }
    (k, width)
}

/// Split tokens at top-level commas (outside brackets and parentheses).
fn split(toks: &[String]) -> Vec<Vec<String>> {
    let mut items: Vec<Vec<String>> = vec![Vec::new()];
    let mut depth = 0i32;
    for t in toks {
        match t.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            _ => {}
        }
        if depth == 0 && t == "," {
            items.push(Vec::new());
        } else {
            items.last_mut().expect("never empty").push(t.clone());
        }
    }
    items.retain(|x| !x.is_empty());
    items
}

/// The index just after the parenthesis that closes `toks[i]`.
fn skip_parens(toks: &[String], mut i: usize) -> usize {
    if i >= toks.len() || toks[i] != "(" {
        return i;
    }
    let mut depth = 0i32;
    while i < toks.len() {
        if toks[i] == "(" {
            depth += 1;
        } else if toks[i] == ")" {
            depth -= 1;
        }
        i += 1;
        if depth == 0 {
            break;
        }
    }
    i
}

/// The ports of `module` in declaration order, or None if the module is absent.
pub fn verilog_ports(text: &str, module: &str) -> Option<Vec<VerilogPort>> {
    let toks = tokens(&clean(text));
    let start = (0..toks.len().saturating_sub(1))
        .find(|&k| (toks[k] == "module" || toks[k] == "macromodule") && toks[k + 1] == module)?;
    let mut i = start + 2;
    if i < toks.len() && toks[i] == "#" {
        i = skip_parens(&toks, i + 1);
    }
    let mut ports: Vec<VerilogPort> = Vec::new();
    let mut ansi = false;
    if i < toks.len() && toks[i] == "(" {
        let end = skip_parens(&toks, i);
        let items = split(&toks[(i + 1).min(end.saturating_sub(1))..end.saturating_sub(1)]);
        i = end;
        ansi = items.first().is_some_and(|it| is_direction(&it[0]));
        let mut direction = String::new();
        let mut width: Option<u32> = None;
        for item in &items {
            if ansi {
                let k;
                if is_direction(&item[0]) {
                    direction = item[0].clone();
                    (k, width) = types_and_range(item, 1);
                } else {
                    // A port without a direction keyword continues the previous one.
                    let (kk, own) = types_and_range(item, 0);
                    k = kk;
                    if own.is_some() {
                        width = own;
                    }
                }
                if k < item.len() {
                    ports.push(VerilogPort {
                        name: item[k].clone(),
                        direction: direction.clone(),
                        width,
                    });
                }
            } else {
                let name = if item[0] == "." && item.len() > 1 {
                    &item[1]
                } else {
                    &item[0]
                };
                ports.push(VerilogPort {
                    name: name.clone(),
                    direction: String::new(),
                    width: None,
                });
            }
        }
    }
    if ansi {
        return Some(ports);
    }
    while i < toks.len() && toks[i] != "endmodule" {
        let t = toks[i].as_str();
        if t == "function" || t == "task" {
            // Their input and output declarations are not the module's.
            let end = format!("end{t}");
            while i < toks.len() && toks[i] != end {
                i += 1;
            }
        } else if is_direction(t) {
            let j = (i..toks.len())
                .find(|&j| toks[j] == ";")
                .unwrap_or(toks.len());
            let (k, width) = types_and_range(&toks, i + 1);
            for item in split(&toks[k.min(j)..j]) {
                // A later declaration of the same name wins, as a dict does.
                for p in ports.iter_mut().filter(|p| p.name == item[0]) {
                    p.direction = t.to_string();
                    p.width = width;
                }
            }
            i = j;
        }
        i += 1;
    }
    Some(ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name(ports: Vec<VerilogPort>) -> Vec<(String, String, Option<u32>)> {
        ports
            .into_iter()
            .map(|p| (p.name, p.direction, p.width))
            .collect()
    }

    fn row(n: &str, d: &str, w: Option<u32>) -> (String, String, Option<u32>) {
        (n.into(), d.into(), w)
    }

    #[test]
    fn reads_what_chipleap_reads() {
        // The same inputs as ChipLeap's test_verilog_reader.
        let non_ansi = r#"
    module other(a); input a; endmodule
    module blk (clk, d, q, \esc[0] , .alias(x));
      parameter W = 4;
      input clk;
      input wire [W-1:0] d;
      output reg [0:3] q;
      function f; input x; endfunction
      task t; output y; endtask
      inout \esc[0] ;
    endmodule
    "#;
        assert_eq!(
            by_name(verilog_ports(non_ansi, "blk").unwrap()),
            [
                row("clk", "input", None),
                row("d", "input", Some(0)),
                row("q", "output", Some(4)),
                row("esc[0]", "inout", None),
                row("alias", "", None),
            ]
        );
        let ansi = r#"
    module blk #(parameter W = 2) (
      input  logic        clk, rst,
      output logic [7:0]  a, b,
      input               c = 1'b0,
      inout  wreal        vin
    );
      always @(*) begin end
    endmodule
    "#;
        assert_eq!(
            by_name(verilog_ports(ansi, "blk").unwrap()),
            [
                row("clk", "input", None),
                row("rst", "input", None),
                row("a", "output", Some(8)),
                row("b", "output", Some(8)),
                row("c", "input", None),
                row("vin", "inout", None),
            ]
        );
        assert_eq!(verilog_ports("module x; endmodule", "blk"), None);
        assert_eq!(verilog_ports("module blk; endmodule", "blk"), Some(vec![]));
    }
}
