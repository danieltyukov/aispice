//! Compare two netlists for electrical equivalence.
//!
//! Used to check aispice's netlister against LTspice's: element names and
//! values must match, labelled nets must match by name, and automatically
//! numbered nets (`N001`, `P001`, `NC_01`) may differ in number as long as the
//! two netlists connect the same pins together.
//!
//! [`compare_with`] relaxes the match for netlists written by other tools,
//! such as xschem: other names for ground, other automatic net names,
//! parameters in another order or spelling, and subcircuit bodies.

use super::spice::{Element, Line, Netlist};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch(pub String);

/// How [`compare_with`] matches two netlists. The default is [`compare`]'s
/// exact match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompareOptions {
    /// Node names that mean ground besides `0`, ignoring case: xschem's
    /// ground symbol names its net `GND`, which ngspice also reads as ground.
    pub ground: Vec<String>,
    /// Treat xschem's automatic net names (`net1`, `net27`) as automatic.
    pub xschem_names: bool,
    /// Compare what follows the nodes word by word, with `name=value`
    /// parameters as a set, numbers by value (`0.5u` equals `0.5e-6`), and
    /// `m=1` and `area=1`, SPICE's defaults, allowed on one side only.
    pub parametric: bool,
    /// Also compare the bodies of subcircuits with the same name, ports by
    /// position.
    pub subckts: bool,
    /// Directive keywords left out on both sides, lower case with the dot
    /// (`.global`).
    pub skip_directives: Vec<String>,
    /// Further node names, on either side, that are automatic names: a
    /// drawing's labels for nets the netlist had numbered (`n1`).
    pub auto_names: Vec<String>,
}

impl CompareOptions {
    /// What a netlist xschem wrote and one aispice wrote need to be compared:
    /// `GND` is ground, `netN` is automatic, parameters are compared by
    /// value, subcircuits are compared, and `.global` is ignored.
    pub fn xschem() -> Self {
        Self {
            ground: vec!["gnd".into()],
            xschem_names: true,
            parametric: true,
            subckts: true,
            skip_directives: vec![".global".into(), ".end".into()],
            auto_names: Vec::new(),
        }
    }

    fn is_ground(&self, n: &str) -> bool {
        n == "0" || self.ground.iter().any(|g| g.eq_ignore_ascii_case(n))
    }

    fn is_auto(&self, n: &str) -> bool {
        auto_named(n)
            || (self.xschem_names && xschem_auto_named(n))
            || self.auto_names.iter().any(|a| a.eq_ignore_ascii_case(n))
    }
}

fn auto_named(n: &str) -> bool {
    let up = n.to_ascii_uppercase();
    (up.starts_with('N') || up.starts_with('P'))
        && up.len() == 4
        && up[1..].bytes().all(|b| b.is_ascii_digit())
        || up.starts_with("NC_")
        || up.starts_with("MP_")
}

/// xschem names unlabelled nets `net1`, `net2`, ...
fn xschem_auto_named(n: &str) -> bool {
    n.len() > 3
        && n.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("net"))
        && n[3..].bytes().all(|b| b.is_ascii_digit())
}

fn norm_value(s: &str) -> String {
    s.replace(['\u{b5}', '\u{3bc}'], "u")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn norm_name(s: &str) -> String {
    s.replace('\u{a7}', "").to_ascii_uppercase()
}

/// Ok when `a` and `b` describe the same circuit. Directive lines are compared
/// as sets, ignoring `.lib` paths, `.backanno` and comments.
pub fn compare(a: &Netlist, b: &Netlist) -> Result<(), Vec<Mismatch>> {
    compare_with(a, b, &CompareOptions::default())
}

/// [`compare`], matched as `opts` says.
pub fn compare_with(a: &Netlist, b: &Netlist, opts: &CompareOptions) -> Result<(), Vec<Mismatch>> {
    let mut problems = Vec::new();
    compare_items(&a.items, &b.items, opts, "", 0, &mut problems);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn elements(items: &[Line]) -> impl Iterator<Item = &Element> {
    items.iter().filter_map(|l| match l {
        Line::Element(e) => Some(e),
        _ => None,
    })
}

fn directives(items: &[Line], opts: &CompareOptions) -> Vec<String> {
    let mut v: Vec<String> = items
        .iter()
        .filter_map(|l| match l {
            Line::Directive { text } => Some(text.as_str()),
            _ => None,
        })
        .filter(|d| {
            let kw = d
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            !matches!(kw.as_str(), ".lib" | ".backanno" | ".inc" | ".include")
                && !opts.skip_directives.contains(&kw)
        })
        .map(norm_value)
        .collect();
    v.sort();
    v
}

/// Subcircuit bodies nest at most this deep in a comparison, as in the parser.
const MAX_DEPTH: usize = 32;

fn compare_items(
    a: &[Line],
    b: &[Line],
    opts: &CompareOptions,
    scope: &str,
    depth: usize,
    problems: &mut Vec<Mismatch>,
) {
    let mut say = |text: String| problems.push(Mismatch(format!("{scope}{text}")));
    // Elements are matched by name, in order, so a deck that repeats a name
    // (two K1 couplings, say) still compares element by element.
    let mut eb: HashMap<String, VecDeque<&Element>> = HashMap::new();
    for e in elements(b) {
        eb.entry(norm_name(&e.name)).or_default().push_back(e);
    }
    let mut map_ab: HashMap<String, String> = HashMap::new();
    let mut map_ba: HashMap<String, String> = HashMap::new();
    let node = |n: &str| {
        if opts.is_ground(n) {
            "0".to_string()
        } else {
            n.to_ascii_uppercase()
        }
    };
    for ea in elements(a) {
        let key = norm_name(&ea.name);
        let Some(ebx) = eb.get_mut(&key).and_then(|q| q.pop_front()) else {
            say(format!("{} is only in the first netlist", ea.name));
            continue;
        };
        let same_rest = if opts.parametric {
            params_equal(&ea.rest, &ebx.rest)
        } else {
            norm_value(&ea.rest) == norm_value(&ebx.rest)
        };
        if !same_rest {
            say(format!("{}: `{}` vs `{}`", ea.name, ea.rest, ebx.rest));
        }
        if ea.nodes.len() != ebx.nodes.len() {
            say(format!(
                "{}: {} nodes vs {}",
                ea.name,
                ea.nodes.len(),
                ebx.nodes.len()
            ));
            continue;
        }
        for (na, nb) in ea.nodes.iter().zip(&ebx.nodes) {
            let (ua, ub) = (node(na), node(nb));
            if !opts.is_auto(&ua) || !opts.is_auto(&ub) {
                if ua != ub {
                    say(format!("{}: node {na} vs {nb}", ea.name));
                }
                continue;
            }
            match (map_ab.get(&ua), map_ba.get(&ub)) {
                (None, None) => {
                    map_ab.insert(ua.clone(), ub.clone());
                    map_ba.insert(ub, ua);
                }
                (Some(x), Some(y)) if *x == ub && *y == ua => {}
                _ => say(format!(
                    "{}: node {na} does not correspond to {nb} consistently",
                    ea.name
                )),
            }
        }
    }
    for rest in eb.values() {
        for e in rest {
            say(format!("{} is only in the second netlist", e.name));
        }
    }
    let (da, db) = (directives(a, opts), directives(b, opts));
    if da != db {
        say(format!("directives differ: {da:?} vs {db:?}"));
    }
    if opts.subckts && depth < MAX_DEPTH {
        compare_subckts(a, b, opts, depth, problems);
    }
}

/// Subcircuits by name on both sides, bodies compared with ports renamed by
/// position so their names do not matter.
fn compare_subckts(
    a: &[Line],
    b: &[Line],
    opts: &CompareOptions,
    depth: usize,
    problems: &mut Vec<Mismatch>,
) {
    let subckts = |items: &[Line]| -> Vec<super::spice::Subckt> {
        items
            .iter()
            .filter_map(|l| match l {
                Line::Subckt(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };
    let (sa, sb) = (subckts(a), subckts(b));
    for s in &sa {
        let Some(t) = sb.iter().find(|t| t.name.eq_ignore_ascii_case(&s.name)) else {
            problems.push(Mismatch(format!(
                "subcircuit {} is only in the first netlist",
                s.name
            )));
            continue;
        };
        if s.ports.len() != t.ports.len() {
            problems.push(Mismatch(format!(
                "subcircuit {}: {} ports vs {}",
                s.name,
                s.ports.len(),
                t.ports.len()
            )));
            continue;
        }
        let scope = format!("in {}: ", s.name);
        compare_items(
            &with_ports_renamed(s),
            &with_ports_renamed(t),
            opts,
            &scope,
            depth + 1,
            problems,
        );
    }
    for t in &sb {
        if !sa.iter().any(|s| s.name.eq_ignore_ascii_case(&t.name)) {
            problems.push(Mismatch(format!(
                "subcircuit {} is only in the second netlist",
                t.name
            )));
        }
    }
}

fn with_ports_renamed(s: &super::spice::Subckt) -> Vec<Line> {
    let port = |n: &str| {
        s.ports
            .iter()
            .position(|p| p.eq_ignore_ascii_case(n))
            .map(|i| format!("port#{i}"))
    };
    s.body
        .iter()
        .map(|l| match l {
            Line::Element(e) => {
                let mut e = e.clone();
                for n in e.nodes.iter_mut() {
                    if let Some(p) = port(n) {
                        *n = p;
                    }
                }
                Line::Element(e)
            }
            other => other.clone(),
        })
        .collect()
}

/// Split on whitespace, keeping `(...)`, `{...}` and `'...'` groups whole and
/// joining `name = value` into `name=value`.
fn words(s: &str) -> Vec<String> {
    let mut raw = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote = false;
    for c in s.chars() {
        match c {
            '\'' => {
                quote = !quote;
                cur.push(c);
            }
            '(' | '{' | '[' if !quote => {
                depth += 1;
                cur.push(c);
            }
            ')' | '}' | ']' if !quote => {
                depth -= 1;
                cur.push(c);
            }
            c if c.is_whitespace() && depth <= 0 && !quote => {
                if !cur.is_empty() {
                    raw.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        raw.push(cur);
    }
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let w = &raw[i];
        if w == "=" && !out.is_empty() && i + 1 < raw.len() {
            let last = out.pop().expect("checked");
            out.push(format!("{last}={}", raw[i + 1]));
            i += 2;
        } else if w.ends_with('=') && w.len() > 1 && i + 1 < raw.len() && !raw[i + 1].contains('=')
        {
            out.push(format!("{w}{}", raw[i + 1]));
            i += 2;
        } else if w.starts_with('=') && w.len() > 1 && !out.is_empty() {
            let last = out.pop().expect("checked");
            out.push(format!("{last}{w}"));
            i += 1;
        } else {
            out.push(w.clone());
            i += 1;
        }
    }
    out
}

/// `name=value` when the word is one, with the name lower-cased.
fn as_param(w: &str) -> Option<(String, &str)> {
    let (k, v) = w.split_once('=')?;
    let ident = !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    ident.then(|| (k.to_ascii_lowercase(), v))
}

fn same_word(a: &str, b: &str) -> bool {
    match (crate::units::parse(a), crate::units::parse(b)) {
        (Some(x), Some(y)) => x == y || (x - y).abs() <= 1e-9 * x.abs().max(y.abs()),
        _ => norm_value(a) == norm_value(b),
    }
}

/// Defaults SPICE assumes when the parameter is absent.
fn is_default(key: &str, value: &str) -> bool {
    matches!(key, "m" | "area") && crate::units::parse(value) == Some(1.0)
}

fn params_equal(a: &str, b: &str) -> bool {
    let split = |s: &str| {
        let mut positional = Vec::new();
        let mut params: Vec<(String, String)> = Vec::new();
        for w in words(s) {
            match as_param(&w) {
                Some((k, v)) => params.push((k, v.to_string())),
                None => positional.push(w),
            }
        }
        (positional, params)
    };
    let (pa, ka) = split(a);
    let (pb, kb) = split(b);
    if pa.len() != pb.len() || !pa.iter().zip(&pb).all(|(x, y)| same_word(x, y)) {
        return false;
    }
    let find = |list: &[(String, String)], key: &str| -> Option<String> {
        list.iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let keys: std::collections::BTreeSet<&str> =
        ka.iter().chain(&kb).map(|(k, _)| k.as_str()).collect();
    keys.into_iter()
        .all(|k| match (find(&ka, k), find(&kb, k)) {
            (Some(x), Some(y)) => same_word(&x, &y),
            (Some(x), None) | (None, Some(x)) => is_default(k, &x),
            (None, None) => true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlist::parse;

    #[test]
    fn renumbered_nets_are_equivalent() {
        let a = parse("t\nR1 N001 out 1k\nC1 out 0 1\u{b5}\nV1 N001 0 1\n.op\n");
        let b = parse("t\nR1 N007 out 1k\nC1 out 0 1u\nV1 N007 0 1\n.op\n.backanno\n");
        assert_eq!(compare(&a, &b), Ok(()));
    }

    #[test]
    fn different_topology_is_caught() {
        let a = parse("t\nR1 N001 N002 1k\nR2 N002 0 1k\n");
        let b = parse("t\nR1 N001 N002 1k\nR2 N001 0 1k\n");
        assert!(compare(&a, &b).is_err());
    }

    #[test]
    fn section_separator_is_ignored_in_names() {
        let a = parse("t\nR\u{a7}load a 0 1k\n");
        let b = parse("t\nRload a 0 1k\n");
        assert_eq!(compare(&a, &b), Ok(()));
    }

    #[test]
    fn exact_compare_keeps_its_rules() {
        // Without options, GND is a net like any other and xschem's names
        // are labels.
        let a = parse("t\nR1 net1 GND 1k\n");
        let b = parse("t\nR1 N001 0 1k\n");
        assert!(compare(&a, &b).is_err());
        let a = parse("t\nR1 a 0 1k m=1\n");
        let b = parse("t\nR1 a 0 1k\n");
        assert!(compare(&a, &b).is_err());
    }

    #[test]
    fn xschem_ground_and_automatic_names() {
        let opts = CompareOptions::xschem();
        let a = parse(
            "t\nR1 net1 GND 1k m=1\nR2 net1 net2 2k m=1\nC1 net2 gnd 1p m=1\n.GLOBAL GND\n.op\n",
        );
        let b = parse("t\nR1 N001 0 1k\nR2 N001 N002 2k\nC1 N002 0 1p\n.op\n");
        assert_eq!(compare_with(&a, &b, &opts), Ok(()));
        // Swapping two nets is still caught.
        let c = parse("t\nR1 N001 0 1k\nR2 N002 N001 2k\nC1 N002 0 1p\n.op\n");
        assert!(compare_with(&a, &c, &opts).is_err());
        // A drawing's own label for a numbered net counts as automatic when
        // named so.
        let d = parse("t\nR1 n1 0 1k\nR2 n1 N002 2k\nC1 N002 0 1p\n.op\n");
        assert!(compare_with(&a, &d, &opts).is_err());
        let with = CompareOptions {
            auto_names: vec!["n1".into()],
            ..CompareOptions::xschem()
        };
        assert_eq!(compare_with(&a, &d, &with), Ok(()));
        // An xschem netN name is only automatic with xschem names on.
        assert!(
            xschem_auto_named("net12")
                && !xschem_auto_named("netx")
                && !xschem_auto_named("n\u{e9}t1")
                && !xschem_auto_named("ab\u{20ac}1")
        );
    }

    #[test]
    fn parameters_compare_as_a_set_of_values() {
        let opts = CompareOptions::xschem();
        let a = parse("t\nXR1 p m sub! rppd w=0.5e-6 l=0.5e-6 m=1 b=0\n");
        let b = parse("t\nXR1 p m sub! rppd w=0.5u l=0.5u b=0 m=1\n");
        assert_eq!(compare_with(&a, &b, &opts), Ok(()));
        let c = parse("t\nXR1 p m sub! rppd w=0.5u l=0.6u b=0 m=1\n");
        assert!(compare_with(&a, &c, &opts).is_err());
        // A different device is a difference even with the same parameters.
        let d = parse("t\nXR1 p m sub! rsil w=0.5u l=0.5u b=0 m=1\n");
        assert!(compare_with(&a, &d, &opts).is_err());
        // m=1 and area=1 may be left out; m=2 may not.
        let e = parse("t\nD1 a 0 DX area=1\nQ1 c b 0 QX area=1 m=1\nR1 a b 1k m=2\n");
        let f = parse("t\nD1 a 0 DX\nQ1 c b 0 QX\nR1 a b 1k\n");
        let problems = compare_with(&e, &f, &opts).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].0.starts_with("R1:"), "{problems:?}");
        // xschem's bsource writes `V = expr`.
        let g = parse("t\nB1 a 0 V = V(b)*2\n");
        let h = parse("t\nB1 a 0 V=V(b)*2\n");
        assert_eq!(compare_with(&g, &h, &opts), Ok(()));
    }

    #[test]
    fn subcircuit_bodies_are_compared_by_port_position() {
        let opts = CompareOptions::xschem();
        let a =
            parse("t\nx1 in out inv\n.subckt inv A Y\nXM1 Y A GND GND sg13_lv_nmos w=1u\n.ends\n");
        let b =
            parse("t\nx1 in out inv\n.subckt inv a y\nXM1 y a 0 0 sg13_lv_nmos w=1u\n.ends inv\n");
        assert_eq!(compare_with(&a, &b, &opts), Ok(()));
        let c =
            parse("t\nx1 in out inv\n.subckt inv a y\nXM1 a y 0 0 sg13_lv_nmos w=1u\n.ends inv\n");
        let problems = compare_with(&a, &c, &opts).unwrap_err();
        assert!(
            problems.iter().all(|p| p.0.starts_with("in inv: ")),
            "{problems:?}"
        );
        let d = parse("t\nx1 in out inv\n");
        assert!(compare_with(&a, &d, &opts).is_err());
    }
}
