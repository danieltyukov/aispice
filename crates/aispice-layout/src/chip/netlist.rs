//! The chip's netlist for LVS: one instance per block, the pad nets as the
//! chip's ports, the blocks' own netlists from their packages (reduced to
//! their subcircuits), and IHP's standard-cell netlists when a block uses
//! standard cells.
//!
//! A package's SPICE file is untrusted: only `.subckt` blocks with device
//! lines and subcircuit calls are kept, every node and parameter is
//! checked, and anything else (includes, libraries, control lines,
//! expressions) is refused rather than passed to the LVS reader.

use super::spec::{ChipSpec, MacroUse};
use aispice_core::netlist::{self, Line, Subckt};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// A block's netlist as the chip uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockNetlist {
    pub cell: String,
    /// The subcircuit's ports, in order.
    pub ports: Vec<String>,
    /// The block's subcircuit and those it defines, cleaned.
    pub text: String,
    /// Subcircuits it calls that it does not define (standard cells).
    pub external: BTreeSet<String>,
}

fn node_ok(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 128
        && n.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '_' | '[' | ']' | '<' | '>' | '.' | '$' | '#' | '|' | '-' | '!'
                )
        })
}

fn param_ok(p: &str) -> bool {
    match p.split_once('=') {
        Some((k, v)) => {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !v.is_empty()
                && v.len() <= 64
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
        }
        None => p
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '+' | '!')),
    }
}

fn clean_subckt(s: &Subckt, file: &str) -> Result<(String, BTreeSet<String>), String> {
    let mut t = String::new();
    let mut calls = BTreeSet::new();
    for p in &s.ports {
        if !node_ok(p) {
            return Err(format!(
                "{file}: subcircuit {} has a port `{p}` aispice does not pass on",
                s.name
            ));
        }
    }
    if !s.params.trim().is_empty() && !s.params.split_whitespace().all(param_ok) {
        return Err(format!(
            "{file}: subcircuit {} has parameters aispice does not pass on",
            s.name
        ));
    }
    let _ = writeln!(t, ".subckt {} {}", s.name, s.ports.join(" "));
    for line in &s.body {
        match line {
            Line::Element(e) => {
                if !matches!(e.letter(), 'M' | 'R' | 'C' | 'D' | 'Q' | 'X') {
                    return Err(format!(
                        "{file}: {} is not a device a chip netlist takes (M, R, C, D, Q or X)",
                        e.name
                    ));
                }
                if !node_ok(&e.name) || !e.nodes.iter().all(|n| node_ok(n)) {
                    return Err(format!(
                        "{file}: {} has a name aispice does not pass on",
                        e.name
                    ));
                }
                if !e.rest.split_whitespace().all(param_ok) {
                    return Err(format!(
                        "{file}: {} has a value or parameter aispice does not pass on (only plain names, numbers and name=number)",
                        e.name
                    ));
                }
                if e.letter() == 'X'
                    && let Some(sub) = e.first_word()
                {
                    calls.insert(sub.to_ascii_lowercase());
                }
                let _ = writeln!(t, "{} {} {}", e.name, e.nodes.join(" "), e.rest);
            }
            Line::Comment { .. } => {}
            Line::Directive { text } => {
                return Err(format!(
                    "{file}: `{}` inside subcircuit {}; a block's netlist holds devices only",
                    text.split_whitespace().next().unwrap_or(""),
                    s.name
                ));
            }
            Line::Subckt(inner) => {
                return Err(format!(
                    "{file}: subcircuit {} is nested in {}",
                    inner.name, s.name
                ));
            }
        }
    }
    let _ = writeln!(t, ".ends {}", s.name);
    Ok((t, calls))
}

/// A block's netlist from its package's SPICE file: its subcircuit and the
/// ones it defines and calls, nothing else.
pub fn block_netlist(cell: &str, text: &str, file: &str) -> Result<BlockNetlist, String> {
    let parsed = netlist::parse(&format!("* {file}\n{text}"));
    let defined: BTreeMap<String, &Subckt> = parsed
        .subckts()
        .map(|s| (s.name.to_ascii_lowercase(), s))
        .collect();
    let top = defined
        .get(&cell.to_ascii_lowercase())
        .ok_or_else(|| format!("{file} has no .subckt {cell}"))?;
    let mut out = String::new();
    let mut external = BTreeSet::new();
    let mut todo = vec![top.name.to_ascii_lowercase()];
    let mut done = BTreeSet::new();
    while let Some(name) = todo.pop() {
        if !done.insert(name.clone()) {
            continue;
        }
        if done.len() > 10_000 {
            return Err(format!("{file} defines too many subcircuits"));
        }
        match defined.get(&name) {
            Some(s) => {
                let (t, calls) = clean_subckt(s, file)?;
                out.push_str(&t);
                todo.extend(calls);
            }
            None => {
                external.insert(name);
            }
        }
    }
    Ok(BlockNetlist {
        cell: top.name.clone(),
        ports: top.ports.clone(),
        text: out,
        external,
    })
}

/// The chip's LVS netlist: the top subcircuit with the pad nets as ports
/// and an instance per block, then the blocks' netlists, then the PDK's
/// standard cells (`stdcells`, the CDL text) when a block needs them.
pub fn chip_netlist(
    spec: &ChipSpec,
    blocks: &BTreeMap<String, BlockNetlist>,
    stdcells: Option<&str>,
    version: &str,
) -> Result<String, String> {
    let mut ports: BTreeSet<&str> = BTreeSet::new();
    for p in spec.pads.iter().filter(|p| p.kind.in_core()) {
        ports.insert(&p.net);
    }
    let mut t = format!(
        "* {}: chip netlist for LVS written by aispice {version}. The pad cells are left\n* out (IHP's LVS deck cannot compare them); their nets are the ports.\n",
        spec.name
    );
    let _ = writeln!(
        t,
        ".subckt {} {}",
        spec.name,
        ports.iter().copied().collect::<Vec<_>>().join(" ")
    );
    for m in &spec.macros {
        let b = block_of(blocks, m)?;
        let nets: Vec<&str> = b.ports.iter().map(|p| ChipSpec::net_of(m, p)).collect();
        let _ = writeln!(t, "X{} {} {}", m.instance, nets.join(" "), b.cell);
    }
    let _ = writeln!(t, ".ends {}", spec.name);
    let mut external = BTreeSet::new();
    let mut written = BTreeSet::new();
    for m in &spec.macros {
        let b = block_of(blocks, m)?;
        if written.insert(b.cell.to_ascii_lowercase()) {
            t.push_str(&b.text);
            external.extend(b.external.iter().cloned());
        }
    }
    if !external.is_empty() {
        let Some(cdl) = stdcells else {
            return Err(format!(
                "the blocks call subcircuits they do not define ({}), and the PDK has no standard-cell netlist",
                external.into_iter().collect::<Vec<_>>().join(", ")
            ));
        };
        let lib = netlist::parse(&format!("* stdcells\n{cdl}"));
        let have: BTreeSet<String> = lib.subckts().map(|s| s.name.to_ascii_lowercase()).collect();
        let missing: Vec<&String> = external.iter().filter(|e| !have.contains(*e)).collect();
        if !missing.is_empty() {
            return Err(format!(
                "the blocks call subcircuits that neither they nor the PDK's standard cells define: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        t.push_str("* The PDK's standard cells.\n");
        t.push_str(cdl);
        if !cdl.ends_with('\n') {
            t.push('\n');
        }
    }
    Ok(t)
}

fn block_of<'a>(
    blocks: &'a BTreeMap<String, BlockNetlist>,
    m: &MacroUse,
) -> Result<&'a BlockNetlist, String> {
    blocks
        .get(&m.instance)
        .ok_or_else(|| format!("no netlist for block {}", m.instance))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNTER: &str = include_str!(
        "../../../aispice-core/tests/fixtures/macro/tiny_counter.macro/tiny_counter.spice"
    );

    #[test]
    fn a_block_netlist_keeps_its_subcircuits_and_names_what_it_calls() {
        let text = "* ota\n.include /etc/passwd\n.subckt ota inp out vdd vss\nMM1 out inp vss vss sg13_lv_nmos w=1u l=0.5u ng=1 m=1\nXinner out vdd helper\n.ends ota\n.subckt helper a b\nRR1 a b sub! rppd w=1u l=2u\n.ends helper\n.subckt unused x\n.ends\n";
        let b = block_netlist("ota", text, "ota.spice").unwrap();
        assert_eq!(b.ports, ["inp", "out", "vdd", "vss"]);
        assert!(
            b.text.contains(".subckt ota inp out vdd vss\n"),
            "{}",
            b.text
        );
        assert!(b.text.contains(".subckt helper a b\n"), "{}", b.text);
        assert!(!b.text.contains("unused"), "{}", b.text);
        assert!(!b.text.contains("include"), "{}", b.text);
        assert!(b.external.is_empty());
        let c = block_netlist("tiny_counter", COUNTER, "tiny_counter.spice").unwrap();
        assert!(c.external.contains("sg13g2_dfrbp_1"), "{:?}", c.external);
        assert_eq!(c.ports.len(), 7);
    }

    #[test]
    fn what_a_block_netlist_may_not_hold_is_refused() {
        for (bad, why) in [
            (".subckt b a\n.include x.sp\n.ends\n", "inside subcircuit"),
            (".subckt b a\nV1 a 0 1\n.ends\n", "not a device"),
            (
                ".subckt b a\nMM1 a a a a nmos w={1/0}\n.ends\n",
                "value or parameter",
            ),
            (".subckt b a{x}\n.ends\n", "port"),
            (".subckt b a\nRR1 a \"x\" 1k\n.ends\n", "name"),
        ] {
            let e = block_netlist("b", bad, "b.spice").unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(
            block_netlist("nope", ".subckt b a\n.ends\n", "b.spice")
                .unwrap_err()
                .contains("no .subckt nope")
        );
    }

    #[test]
    fn the_chip_netlist_has_pad_ports_and_block_instances() {
        let spec = ChipSpec::parse(
            "name = \"demo\"\n[[macros]]\ninstance = \"u1\"\npackage = \"m/ota.macro\"\nnets = { inp = \"a_in\" }\n[[pads]]\nnet = \"a_in\"\nside = \"west\"\nkind = \"analog\"\n[[pads]]\nnet = \"vdd\"\nside = \"north\"\nkind = \"vdd\"\n[[pads]]\nnet = \"vss\"\nside = \"south\"\nkind = \"vss\"\n[[pads]]\nnet = \"iovdd\"\nside = \"east\"\nkind = \"iovdd\"\n[[pads]]\nnet = \"iovss\"\nside = \"east\"\nkind = \"iovss\"\n",
        )
        .unwrap();
        let mut blocks = BTreeMap::new();
        blocks.insert(
            "u1".to_string(),
            block_netlist(
                "ota",
                ".subckt ota inp out vdd vss\nMM1 out inp vss vss sg13_lv_nmos w=1u l=1u\n.ends\n",
                "ota.spice",
            )
            .unwrap(),
        );
        let t = chip_netlist(&spec, &blocks, None, "0.2.0").unwrap();
        assert!(t.contains(".subckt demo a_in vdd vss\n"), "{t}");
        assert!(t.contains("Xu1 a_in out vdd vss ota\n"), "{t}");
        assert!(t.contains(".subckt ota inp out vdd vss\n"), "{t}");
        let mut digital = blocks.clone();
        digital.insert(
            "u1".into(),
            block_netlist("tiny_counter", COUNTER, "c.spice").unwrap(),
        );
        assert!(
            chip_netlist(&spec, &digital, None, "x")
                .unwrap_err()
                .contains("sg13g2_dfrbp_1")
        );
        let cdl = ".SUBCKT sg13g2_dfrbp_1 Q Q_N CLK D RESET_B VDD VSS\n.ENDS\n.SUBCKT sg13g2_xor2_1 X A B VDD VSS\n.ENDS\n.SUBCKT sg13g2_and2_1 X A B VDD VSS\n.ENDS\n";
        let t = chip_netlist(&spec, &digital, Some(cdl), "x").unwrap();
        assert!(t.contains("sg13g2_xor2_1"), "{t}");
    }
}
