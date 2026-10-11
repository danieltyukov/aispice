//! SPICE, far enough to find a subcircuit's ports and their `*.PININFO`
//! directions. ChipLeap's `chipleap.macro.views.spice_subckt` reads it the
//! same way. SPICE is case-insensitive; names are compared in ASCII lower case.

use super::{is_ws, words};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub struct SpiceSubckt {
    pub name: String,
    pub ports: Vec<String>,
    /// Lower-cased port to `I`, `O` or `B`.
    pub pininfo: BTreeMap<String, String>,
}

fn code(line: &str) -> &str {
    if line.starts_with('*') {
        return "";
    }
    let mut line = line;
    for mark in [" $", "\t$", ";"] {
        if let Some(k) = line.find(mark) {
            line = &line[..k];
        }
    }
    line
}

/// `.subckt name` with its ports and `*.PININFO` directions, or None if absent.
pub fn spice_subckt(text: &str, name: &str) -> Option<SpiceSubckt> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let s = raw.trim_matches(is_ws);
        match (s.strip_prefix('+'), lines.last_mut()) {
            (Some(rest), Some(last)) => {
                last.push(' ');
                last.push_str(rest);
            }
            _ => lines.push(s.to_string()),
        }
    }
    for (k, line) in lines.iter().enumerate() {
        let ws = words(code(line));
        if ws.len() < 2
            || !ws[0].eq_ignore_ascii_case(".subckt")
            || !ws[1].eq_ignore_ascii_case(name)
        {
            continue;
        }
        let ports = ws[2..]
            .iter()
            .take_while(|w| !w.eq_ignore_ascii_case("params:") && !w.contains('='))
            .map(|w| w.to_string())
            .collect();
        let mut pininfo = BTreeMap::new();
        for later in &lines[k + 1..] {
            let lower = later.to_ascii_lowercase();
            if lower.starts_with(".ends") {
                break;
            }
            if lower.starts_with("*.pininfo") {
                for item in words(later).into_iter().skip(1) {
                    if let Some((pin, d)) = item.rsplit_once(':')
                        && !pin.is_empty()
                    {
                        pininfo.insert(pin.to_ascii_lowercase(), d.to_ascii_uppercase());
                    }
                }
            }
        }
        return Some(SpiceSubckt {
            name: ws[1].to_string(),
            ports,
            pininfo,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_chipleap_reads() {
        // The same input as ChipLeap's test_spice_reader.
        let text = "* header\n.SUBCKT other a b\n.ends\n.subckt BLK in out\n+ vdd vss params: w=1u\n*.pininfo in:I out:O\n+ vdd:B\nX1 out in vdd vss inv ; inline comment\n.ends BLK\n";
        let sub = spice_subckt(text, "blk").unwrap();
        assert_eq!(sub.ports, ["in", "out", "vdd", "vss"]);
        let info: Vec<(&str, &str)> = sub
            .pininfo
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(info, [("in", "I"), ("out", "O"), ("vdd", "B")]);
        assert!(
            spice_subckt(".subckt x a=1\n.ends\n", "x")
                .unwrap()
                .ports
                .is_empty()
        );
        assert!(spice_subckt(text, "missing").is_none());
    }
}
