//! Electrical rule checks.
//!
//! Each finding names the rule, the parts and nets involved, and where on the
//! sheet to look, so both a person and the agent can act on it. Rules aim at
//! what makes a simulation fail or lie: no ground, floating nodes, no DC path,
//! shorted sources, missing values or analysis. Readability rules (off-grid,
//! overlapping parts) are reported as information.

use crate::geometry::{GRID, Point};
use crate::netlist::{Connectivity, connect};
use crate::schematic::{Item, Schematic};
use crate::symbol::SymbolLibrary;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// Stable rule id, e.g. `no-ground`.
    pub rule: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Point>,
}

impl Finding {
    fn new(severity: Severity, rule: &str, message: impl Into<String>) -> Self {
        Self {
            severity,
            rule: rule.into(),
            message: message.into(),
            parts: vec![],
            nets: vec![],
            at: None,
        }
    }
    fn parts(mut self, p: &[&str]) -> Self {
        self.parts = p.iter().map(|s| s.to_string()).collect();
        self
    }
    fn nets(mut self, n: &[&str]) -> Self {
        self.nets = n.iter().map(|s| s.to_string()).collect();
        self
    }
    fn at(mut self, p: Option<Point>) -> Self {
        self.at = p;
        self
    }
}

/// Every rule id, for documentation and for turning rules off.
pub const RULES: &[(&str, &str)] = &[
    (
        "no-ground",
        "The circuit has no ground (a flag labelled 0).",
    ),
    (
        "unknown-symbol",
        "A symbol could not be found in any library.",
    ),
    ("missing-name", "A component has no instance name."),
    ("duplicate-name", "Two components share an instance name."),
    (
        "missing-value",
        "A resistor, capacitor or inductor has no value.",
    ),
    ("floating-pin", "A pin connects to nothing."),
    ("dangling-wire", "A wire end touches nothing."),
    ("single-pin-net", "An unlabelled net reaches only one pin."),
    (
        "shorted-part",
        "Both terminals of a two-terminal part are on the same net.",
    ),
    (
        "shorted-source",
        "A voltage source's terminals are on the same net.",
    ),
    (
        "parallel-sources",
        "Two voltage sources are connected directly in parallel.",
    ),
    (
        "no-dc-path",
        "A net reaches ground only through capacitors or current sources.",
    ),
    (
        "no-analysis",
        "There is no analysis directive (.tran, .ac, .dc, .op, .noise, .tf).",
    ),
    (
        "multiple-analyses",
        "More than one analysis directive; LTspice and Xyce run only one per simulation.",
    ),
    (
        "multiple-labels",
        "One net carries several different labels.",
    ),
    (
        "unknown-subckt",
        "A part calls a subcircuit that no .subckt directive, readable library or aispice's own models define.",
    ),
    ("off-grid", "A part or wire is not on the 16-unit grid."),
    ("overlap", "Two parts are drawn on top of each other."),
];

pub struct LintReport {
    pub findings: Vec<Finding>,
    pub connectivity: Connectivity,
}

impl LintReport {
    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .count()
    }
}

/// Run every rule.
pub fn lint(sch: &Schematic, lib: &SymbolLibrary) -> LintReport {
    let conn = connect(sch, lib);
    let mut out = Vec::new();

    for w in &conn.warnings {
        let rule = if w.message.contains("no symbol named") {
            "unknown-symbol"
        } else if w.message.contains("no InstName") {
            "missing-name"
        } else {
            "multiple-labels"
        };
        let sev = if rule == "multiple-labels" {
            Severity::Warning
        } else {
            Severity::Error
        };
        out.push(Finding::new(sev, rule, w.message.clone()).at(w.at));
    }

    if !conn.nets.iter().any(|n| n.is_ground()) && !conn.nets.is_empty() {
        out.push(Finding::new(
            Severity::Error,
            "no-ground",
            "No ground. Add a flag labelled 0 on the reference node; SPICE needs one.",
        ));
    }

    // Names.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in &conn.placed {
        if !p.inst.is_empty() {
            *seen.entry(p.inst.to_ascii_uppercase()).or_default() += 1;
        }
    }
    let mut dup: Vec<_> = seen.into_iter().filter(|(_, c)| *c > 1).collect();
    dup.sort();
    for (name, count) in dup {
        out.push(
            Finding::new(
                Severity::Error,
                "duplicate-name",
                format!("{count} components are named {name}; instance names must be unique."),
            )
            .parts(&[&name]),
        );
    }

    // Per part checks.
    for p in &conn.placed {
        let Some(def) = &p.def else { continue };
        let Item::Symbol(sym) = &sch.items[p.item] else {
            continue;
        };
        let letter = def
            .prefix()
            .chars()
            .next()
            .unwrap_or('X')
            .to_ascii_uppercase();
        if matches!(letter, 'R' | 'C' | 'L') {
            let value = sym.value().unwrap_or("").trim();
            if value.is_empty() || value.eq_ignore_ascii_case(&letter.to_string()) {
                out.push(
                    Finding::new(
                        Severity::Error,
                        "missing-value",
                        format!("{} has no value.", p.inst),
                    )
                    .parts(&[&p.inst])
                    .at(Some(sym.at)),
                );
            }
        }
        let pins = conn.pin_nets.get(&p.inst.to_ascii_uppercase());
        if let Some(pins) = pins
            && pins.len() == 2
            && pins[0].1 == pins[1].1
        {
            let net = &conn.nets[pins[0].1].name;
            let (rule, sev) = if letter == 'V' {
                ("shorted-source", Severity::Error)
            } else {
                ("shorted-part", Severity::Warning)
            };
            out.push(
                Finding::new(
                    sev,
                    rule,
                    format!("Both terminals of {} are on net {net}.", p.inst),
                )
                .parts(&[&p.inst])
                .nets(&[net])
                .at(Some(sym.at)),
            );
        }
        if !sym.at.on_grid() {
            out.push(
                Finding::new(
                    Severity::Info,
                    "off-grid",
                    format!("{} is at {}, off the 16-unit grid.", p.inst, sym.at),
                )
                .parts(&[&p.inst])
                .at(Some(sym.at)),
            );
        }
    }

    // Floating pins and single-pin nets.
    for net in &conn.nets {
        if net.name.starts_with("NC_") {
            for pin in &net.pins {
                out.push(
                    Finding::new(
                        Severity::Warning,
                        "floating-pin",
                        format!("Pin {} of {} connects to nothing.", pin.pin, pin.inst),
                    )
                    .parts(&[&pin.inst])
                    .at(Some(pin.at)),
                );
            }
        } else if !net.labelled && net.pins.len() == 1 {
            let pin = &net.pins[0];
            out.push(
                Finding::new(
                    Severity::Warning,
                    "single-pin-net",
                    format!(
                        "Pin {} of {} reaches a wire but no other pin.",
                        pin.pin, pin.inst
                    ),
                )
                .parts(&[&pin.inst])
                .nets(&[&net.name])
                .at(Some(pin.at)),
            );
        }
    }

    // Dangling wire ends: an end that touches no other wire, pin or flag.
    let mut touch: HashMap<Point, usize> = HashMap::new();
    for w in sch.wires() {
        *touch.entry(w.a).or_default() += 1;
        *touch.entry(w.b).or_default() += 1;
    }
    let anchors: HashSet<Point> = conn
        .nets
        .iter()
        .flat_map(|n| n.pins.iter().map(|p| p.at))
        .chain(sch.flags().map(|f| f.at))
        .collect();
    let wires: Vec<_> = sch.wires().copied().collect();
    let segs: Vec<(Point, Point)> = wires.iter().map(|w| (w.a, w.b)).collect();
    let (index, _) = crate::geometry::SegmentIndex::new(&segs);
    for w in &wires {
        let horizontal = w.a.y == w.b.y;
        for end in [w.a, w.b] {
            let shared = touch.get(&end).copied().unwrap_or(0) > 1;
            // Another wire covers this end if a perpendicular run passes
            // through it, or if the merged run along this wire extends past it
            // (only another wire can make it extend).
            let c = index.cover(end);
            let (along, across) = if horizontal {
                (c.horizontal, c.vertical)
            } else {
                (c.vertical, c.horizontal)
            };
            let coord = if horizontal { end.x } else { end.y };
            let on_other = across.is_some()
                || along.is_some_and(|s| s.lo < coord && coord < s.hi)
                || c.diagonal.len() > usize::from(w.a.x != w.b.x && w.a.y != w.b.y);
            if !shared && !on_other && !anchors.contains(&end) {
                out.push(
                    Finding::new(
                        Severity::Warning,
                        "dangling-wire",
                        format!("A wire ends at {end} without touching anything."),
                    )
                    .at(Some(end)),
                );
            }
        }
        if !w.a.on_grid() || !w.b.on_grid() {
            out.push(
                Finding::new(
                    Severity::Info,
                    "off-grid",
                    format!("Wire {} to {} is off the 16-unit grid.", w.a, w.b),
                )
                .at(Some(w.a)),
            );
        }
    }

    // Voltage sources in parallel.
    let mut vsrc: HashMap<(usize, usize), Vec<String>> = HashMap::new();
    for p in &conn.placed {
        let Some(def) = &p.def else { continue };
        if def.prefix().to_ascii_uppercase().starts_with('V')
            && let Some(pins) = conn.pin_nets.get(&p.inst.to_ascii_uppercase())
            && pins.len() == 2
            && pins[0].1 != pins[1].1
        {
            let key = (pins[0].1.min(pins[1].1), pins[0].1.max(pins[1].1));
            vsrc.entry(key).or_default().push(p.inst.clone());
        }
    }
    for ((a, b), names) in vsrc {
        if names.len() > 1 {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            out.push(
                Finding::new(Severity::Error, "parallel-sources", format!("{} are voltage sources in parallel across {} and {}; the circuit has no unique solution.", names.join(" and "), conn.nets[a].name, conn.nets[b].name))
                    .parts(&refs)
                    .nets(&[&conn.nets[a].name, &conn.nets[b].name]),
            );
        }
    }

    // DC path to ground: walk from ground through parts that conduct at DC.
    if let Some(gnd) = conn.nets.iter().position(|n| n.is_ground()) {
        let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
        for p in &conn.placed {
            let Some(def) = &p.def else { continue };
            let letter = def
                .prefix()
                .chars()
                .next()
                .unwrap_or('X')
                .to_ascii_uppercase();
            if matches!(letter, 'C' | 'I') {
                continue;
            }
            let Some(pins) = conn.pin_nets.get(&p.inst.to_ascii_uppercase()) else {
                continue;
            };
            // Controlled sources: only the output pair conducts.
            let take = if matches!(letter, 'E' | 'G' | 'S') {
                2
            } else {
                pins.len()
            };
            let nets: Vec<usize> = pins.iter().take(take).map(|(_, n)| *n).collect();
            for &x in &nets {
                for &y in &nets {
                    if x != y {
                        adj.entry(x).or_default().push(y);
                    }
                }
            }
        }
        let mut reached = HashSet::from([gnd]);
        let mut queue = VecDeque::from([gnd]);
        while let Some(n) = queue.pop_front() {
            for &m in adj.get(&n).into_iter().flatten() {
                if reached.insert(m) {
                    queue.push_back(m);
                }
            }
        }
        for (i, net) in conn.nets.iter().enumerate() {
            if !reached.contains(&i) && !net.name.starts_with("NC_") && net.pins.len() > 1 {
                out.push(
                    Finding::new(Severity::Warning, "no-dc-path", format!("Net {} reaches ground only through capacitors or current sources; add a DC path (for example a large resistor) or the operating point is undefined.", net.name))
                        .nets(&[&net.name])
                        .at(net.pins.first().map(|p| p.at)),
                );
            }
        }
    }

    subckt_calls(sch, lib, &conn, &mut out);

    let analyses: Vec<String> = sch
        .directives()
        .flat_map(|t| t.lines())
        .filter(|l| crate::netlist::spice::is_analysis(l))
        .map(|l| l.trim().to_string())
        .collect();
    if analyses.is_empty() {
        out.push(Finding::new(Severity::Warning, "no-analysis", "No analysis directive. Add one of .op, .tran, .ac, .dc, .noise or .tf before simulating."));
    } else if analyses.len() > 1 {
        out.push(Finding::new(
            Severity::Warning,
            "multiple-analyses",
            format!(
                "{} analysis directives ({}). LTspice and Xyce run only one per simulation (aispice runs the first there); ngspice runs them all. Keep one, or turn the others into comments.",
                analyses.len(),
                analyses.join(", ")
            ),
        ));
    }

    // Overlapping parts.
    let boxes: Vec<(String, crate::geometry::Rect)> = conn
        .placed
        .iter()
        .filter_map(|p| {
            let Item::Symbol(sym) = &sch.items[p.item] else {
                return None;
            };
            let r = p.def.as_ref()?.placed_bounds(sym.at, sym.orient)?;
            Some((p.inst.clone(), r.inflate(-GRID / 2)))
        })
        .collect();
    for i in 0..boxes.len() {
        for j in i + 1..boxes.len() {
            if boxes[i].1.intersects(&boxes[j].1) {
                out.push(
                    Finding::new(
                        Severity::Info,
                        "overlap",
                        format!(
                            "{} and {} are drawn on top of each other.",
                            boxes[i].0, boxes[j].0
                        ),
                    )
                    .parts(&[&boxes[i].0, &boxes[j].0]),
                );
            }
        }
    }

    out.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then(a.rule.cmp(&b.rule))
            .then(a.message.cmp(&b.message))
    });
    LintReport {
        findings: out,
        connectivity: conn,
    }
}

/// Subcircuits aispice supplies from its own embedded model library, so a
/// call to one needs no definition. Must list exactly the subcircuits in
/// aispice-sim's `models/generic.lib`; a test there checks it.
pub const BUILTIN_SUBCKTS: &[&str] = &["opamp"];

/// Library files aispice resolves by itself, with the subcircuits each one
/// provides.
const BUILTIN_LIBRARIES: &[(&str, &[&str])] = &[("opamp.sub", &["opamp"])];

/// Library files larger than this are not read for the check.
const MAX_LIBRARY_BYTES: u64 = 4 * 1024 * 1024;

/// The file named by `.lib file [section]`, `.include file` or `.inc file`.
fn include_target(directive: &str) -> Option<String> {
    let trimmed = directive.trim();
    let kw = trimmed.split_whitespace().next()?.to_ascii_lowercase();
    if !matches!(kw.as_str(), ".lib" | ".include" | ".inc") {
        return None;
    }
    let rest = trimmed[kw.len()..].trim();
    let file = match rest.strip_prefix('"') {
        Some(q) => q.split('"').next().unwrap_or(""),
        None => rest.split_whitespace().next().unwrap_or(""),
    };
    (!file.is_empty()).then(|| file.to_string())
}

/// Upper-cased names of every subcircuit defined in a netlist, nested ones
/// included.
fn subckt_names(n: &crate::netlist::Netlist) -> HashSet<String> {
    fn walk(items: &[crate::netlist::Line], out: &mut HashSet<String>) {
        for item in items {
            if let crate::netlist::Line::Subckt(s) = item {
                out.insert(s.name.to_ascii_uppercase());
                walk(&s.body, out);
            }
        }
    }
    let mut out = HashSet::new();
    walk(&n.items, &mut out);
    out
}

/// A library file in the project folder, read and parsed; `None` when it is
/// not a plain file there that aispice can read.
fn project_library(lib: &SymbolLibrary, file: &str) -> Option<crate::netlist::Netlist> {
    let rel = std::path::Path::new(file);
    if rel.is_absolute() {
        return None;
    }
    for (dir, source) in lib.dirs() {
        if source != crate::symbol::SymbolSource::Project {
            continue;
        }
        let root = std::fs::canonicalize(&dir).ok()?;
        let Ok(path) = std::fs::canonicalize(root.join(rel)) else {
            continue;
        };
        if !path.starts_with(&root)
            || !path.is_file()
            || std::fs::metadata(&path).map_or(true, |m| m.len() > MAX_LIBRARY_BYTES)
        {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let (text, _) = crate::encoding::decode(&bytes);
        return Some(crate::netlist::parse(&format!("* {file}\n{text}")));
    }
    None
}

/// Whether an LTspice library aispice uses has a file for this subcircuit,
/// where the simulation layer would look it up by name.
fn ltspice_provides(lib: &SymbolLibrary, name: &str) -> bool {
    let wanted: Vec<String> = ["sub", "lib", "cir", "mod", "txt"]
        .iter()
        .map(|ext| format!("{name}.{ext}").to_ascii_lowercase())
        .collect();
    lib.dirs()
        .into_iter()
        .filter(|(_, s)| *s == crate::symbol::SymbolSource::Ltspice)
        .filter_map(|(dir, _)| dir.parent().map(std::path::Path::to_path_buf))
        .flat_map(|root| ["sub", "cmp", ""].map(|s| root.join(s)))
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|entries| entries.flatten())
        .any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| wanted.contains(&n.to_ascii_lowercase()))
        })
}

/// Parts that call a subcircuit nothing defines: the netlist would fail in
/// the simulator with "unknown subckt". A library aispice cannot read (in
/// LTspice's folders, or outside the project) may define anything, so with
/// one of those included only calls that cannot be a name are reported.
fn subckt_calls(sch: &Schematic, lib: &SymbolLibrary, conn: &Connectivity, out: &mut Vec<Finding>) {
    use crate::netlist::build::{effective_attr, is_library_file, subckt_call};
    let mut calls = Vec::new();
    let mut includes: Vec<String> = Vec::new();
    for p in &conn.placed {
        let Some(def) = &p.def else { continue };
        let Item::Symbol(sym) = &sch.items[p.item] else {
            continue;
        };
        if crate::netlist::connect::is_jumper(&sym.name) {
            continue;
        }
        let model = effective_attr(sym, def, "SpiceModel").filter(|m| is_library_file(m));
        for file in [model, effective_attr(sym, def, "ModelFile")]
            .into_iter()
            .flatten()
        {
            includes.push(file.to_string());
        }
        // LTspice finds a part from its own library by searching its
        // library files, which aispice does not read: leave those alone.
        let from_ltspice = lib
            .resolve(&sym.name)
            .is_ok_and(|(_, s)| s == crate::symbol::SymbolSource::Ltspice);
        if let Some(call) = subckt_call(sym, def) {
            calls.push((
                p.inst.clone(),
                sym.at,
                def.attr("Value").map(str::to_string),
                call,
                from_ltspice,
            ));
        }
    }
    if calls.is_empty() {
        return;
    }
    let mut deck = String::from("* directives\n");
    for t in sch.directives() {
        for line in t.lines() {
            deck.push_str(&line);
            deck.push('\n');
        }
    }
    let parsed = crate::netlist::parse(&deck);
    let mut defined = subckt_names(&parsed);
    includes.extend(parsed.directives().filter_map(include_target));
    let mut unreadable = false;
    for file in &includes {
        let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
        if let Some((_, names)) = BUILTIN_LIBRARIES
            .iter()
            .find(|(l, _)| l.eq_ignore_ascii_case(base))
        {
            defined.extend(names.iter().map(|n| n.to_ascii_uppercase()));
            continue;
        }
        match project_library(lib, file) {
            // A library that includes further files may define anything.
            Some(n) if n.directives().any(|d| include_target(d).is_some()) => unreadable = true,
            Some(n) => defined.extend(subckt_names(&n)),
            None => unreadable = true,
        }
    }
    // A PDK device's model comes from the PDK the project names, which the
    // runner includes; without one the run stops with that explanation.
    let builtin = |n: &str| {
        BUILTIN_SUBCKTS.iter().any(|b| b.eq_ignore_ascii_case(n)) || crate::pdk::device(n).is_some()
    };
    for (inst, at, default, call, from_ltspice) in calls {
        let own = default
            .filter(|d| builtin(d))
            .map(|d| format!("`{d}`"))
            .unwrap_or_else(|| "the subcircuit's name".into());
        let message = match &call.name {
            None => format!("{inst} names no subcircuit: its Value is empty. Set Value to {own}."),
            Some(n) if n.contains('=') => format!(
                "{inst}'s {} `{n}` is a parameter, not a subcircuit name, so the netlist calls a subcircuit named `{n}` and the simulation fails with \"unknown subckt\". Set Value to {own} and put parameters in SpiceLine or SpiceLine2 with set_attr.",
                call.from
            ),
            Some(n) => {
                if unreadable
                    || from_ltspice
                    || defined.contains(&n.to_ascii_uppercase())
                    || builtin(n)
                    || ltspice_provides(lib, n)
                {
                    continue;
                }
                format!(
                    "{inst} calls subcircuit `{n}`, which no .subckt directive, readable library or aispice's own models define, so the simulation fails with \"unknown subckt\". {}",
                    if own.starts_with('`') {
                        format!(
                            "This symbol's own subcircuit is {own}; set Value back to it, or add a .lib or .include line for `{n}`'s model."
                        )
                    } else {
                        format!(
                            "Add a .lib or .include line for `{n}`'s model, or define it with .subckt."
                        )
                    }
                )
            }
        };
        out.push(
            Finding::new(Severity::Warning, "unknown-subckt", message)
                .parts(&[&inst])
                .at(Some(at)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schematic::parse;

    fn rules(src: &str) -> Vec<String> {
        let (sch, _) = parse(src);
        lint(&sch, &SymbolLibrary::builtin_only())
            .findings
            .into_iter()
            .map(|f| f.rule)
            .collect()
    }

    const RC: &str = "Version 4\nSHEET 1 880 680\nWIRE 96 96 32 96\nWIRE 240 96 176 96\nWIRE 240 128 240 96\nFLAG 32 176 0\nFLAG 240 192 0\nFLAG 240 96 out\nSYMBOL voltage 32 80 R0\nSYMATTR InstName V1\nSYMATTR Value SINE(0 1 1k)\nSYMBOL res 192 80 R90\nSYMATTR InstName R1\nSYMATTR Value 1k\nSYMBOL cap 224 128 R0\nSYMATTR InstName C1\nSYMATTR Value 100n\nTEXT 0 232 Left 2 !.tran 5m\n";

    #[test]
    fn clean_circuit_has_no_findings() {
        assert_eq!(rules(RC), Vec::<String>::new());
    }

    #[test]
    fn missing_ground_and_analysis() {
        let src = RC
            .replace("FLAG 32 176 0\n", "FLAG 32 176 gnd2\n")
            .replace("FLAG 240 192 0\n", "FLAG 240 192 gnd2\n")
            .replace("TEXT 0 232 Left 2 !.tran 5m\n", "");
        let r = rules(&src);
        assert!(r.contains(&"no-ground".to_string()), "{r:?}");
        assert!(r.contains(&"no-analysis".to_string()), "{r:?}");
    }

    #[test]
    fn two_analyses_are_flagged() {
        let src = RC.replace(
            "TEXT 0 232 Left 2 !.tran 5m\n",
            "TEXT 0 232 Left 2 !.tran 5m\nTEXT 0 264 Left 2 !.ac dec 20 10 100k\n",
        );
        assert!(rules(&src).contains(&"multiple-analyses".to_string()));
        assert!(!rules(RC).contains(&"multiple-analyses".to_string()));
    }

    #[test]
    fn floating_pin_and_missing_value() {
        let src = format!("{RC}SYMBOL res 480 80 R0\nSYMATTR InstName R2\n");
        let r = rules(&src);
        assert_eq!(
            r.iter().filter(|x| *x == "floating-pin").count(),
            2,
            "{r:?}"
        );
        assert!(r.contains(&"missing-value".to_string()));
    }

    #[test]
    fn capacitor_only_node_has_no_dc_path() {
        // C2 in series with C1's top node: the middle node floats at DC.
        let src = "Version 4\nSHEET 1 880 680\nWIRE 16 64 16 96\nFLAG 16 0 in\nFLAG 16 160 0\nSYMBOL cap 0 0 R0\nSYMATTR InstName C1\nSYMATTR Value 1n\nSYMBOL cap 0 96 R0\nSYMATTR InstName C2\nSYMATTR Value 1n\nSYMBOL voltage -128 0 R0\nSYMATTR InstName V1\nSYMATTR Value 1\nFLAG -128 16 in\nFLAG -128 96 0\nTEXT 0 300 Left 2 !.op\n";
        let r = rules(src);
        assert!(r.contains(&"no-dc-path".to_string()), "{r:?}");
    }

    #[test]
    fn duplicate_names() {
        let src = format!("{RC}SYMBOL res 480 80 R0\nSYMATTR InstName R1\nSYMATTR Value 1k\n");
        assert!(rules(&src).contains(&"duplicate-name".to_string()));
    }

    #[test]
    fn parallel_sources() {
        let src = "Version 4\nSHEET 1 880 680\nFLAG 0 16 a\nFLAG 0 96 0\nFLAG 128 16 a\nFLAG 128 96 0\nSYMBOL voltage 0 0 R0\nSYMATTR InstName V1\nSYMATTR Value 1\nSYMBOL voltage 128 0 R0\nSYMATTR InstName V2\nSYMATTR Value 2\nTEXT 0 300 Left 2 !.op\n";
        let r = rules(src);
        assert!(r.contains(&"parallel-sources".to_string()), "{r:?}");
    }

    const OPAMP: &str = "Version 4\nSHEET 1 880 680\nFLAG 192 96 fb\nFLAG 192 128 0\nFLAG 256 112 fb\nSYMBOL OpAmps/opamp 224 48 R0\nSYMATTR InstName U1\nTEXT 0 300 Left 2 !.op\n";

    fn unknown_subckt(src: &str, lib: &SymbolLibrary) -> Vec<String> {
        let (sch, _) = parse(src);
        lint(&sch, lib)
            .findings
            .into_iter()
            .filter(|f| f.rule == "unknown-subckt")
            .map(|f| {
                assert_eq!(f.severity, Severity::Warning);
                f.message
            })
            .collect()
    }

    /// Was a trap: an op-amp whose Value had been replaced by `GBW=1Meg`
    /// linted clean and then failed in ngspice with "unknown subckt".
    #[test]
    fn subcircuit_calls_need_a_definition() {
        let lib = SymbolLibrary::builtin_only();
        // The built-in op-amp's subcircuit is aispice's own.
        assert!(unknown_subckt(OPAMP, &lib).is_empty());
        let with_value = |v: &str| {
            OPAMP.replace(
                "SYMATTR InstName U1\n",
                &format!("SYMATTR InstName U1\nSYMATTR Value {v}\n"),
            )
        };
        let found = unknown_subckt(&with_value("GBW=1Meg"), &lib);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`GBW=1Meg`") && found[0].contains("SpiceLine"),
            "{found:?}"
        );
        let found = unknown_subckt(&with_value("LM741"), &lib);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("`LM741`") && found[0].contains("`opamp`"),
            "{found:?}"
        );
        // Defined in the schematic: fine.
        let defined = format!(
            "{}TEXT 0 340 Left 2 !.subckt LM741 a b c\\nE1 c 0 b a 1e5\\n.ends LM741\n",
            with_value("LM741")
        );
        assert!(unknown_subckt(&defined, &lib).is_empty());
        // A library aispice cannot read may define it: not flagged.
        let included = format!(
            "{}TEXT 0 340 Left 2 !.lib vendor.lib\n",
            with_value("LM741")
        );
        assert!(unknown_subckt(&included, &lib).is_empty());
        // opamp.sub is aispice's own.
        let builtin = format!("{}TEXT 0 340 Left 2 !.lib opamp.sub\n", with_value("opamp"));
        assert!(unknown_subckt(&builtin, &lib).is_empty());
    }

    /// A library in the project folder is read, so a name it does not define
    /// is flagged and one it defines is not.
    #[test]
    fn project_libraries_are_read() {
        let dir = std::env::temp_dir().join(format!("aispice-lint-sub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("amps.lib"),
            "* amps\n.subckt myamp a b c\nE1 c 0 b a 1e5\n.ends myamp\n",
        )
        .unwrap();
        let lib =
            SymbolLibrary::builtin_only().with_dir(&dir, crate::symbol::SymbolSource::Project);
        let src = |v: &str| {
            format!(
                "{}TEXT 0 340 Left 2 !.lib amps.lib\n",
                OPAMP.replace(
                    "SYMATTR InstName U1\n",
                    &format!("SYMATTR InstName U1\nSYMATTR Value {v}\n")
                )
            )
        };
        assert!(unknown_subckt(&src("myamp"), &lib).is_empty());
        assert_eq!(unknown_subckt(&src("youramp"), &lib).len(), 1);
        // A library that includes another file may define anything.
        std::fs::write(
            dir.join("amps.lib"),
            "* amps\n.include more.lib\n.subckt myamp a b c\n.ends myamp\n",
        )
        .unwrap();
        assert!(unknown_subckt(&src("youramp"), &lib).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// LTspice finds a vendor part's subcircuit by searching its library
    /// files, which aispice does not read; a part from that library is not
    /// flagged.
    #[test]
    fn ltspice_parts_are_not_second_guessed() {
        let dir = std::env::temp_dir().join(format!("aispice-lint-lt-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sym")).unwrap();
        std::fs::write(
            dir.join("sym/vendorop.asy"),
            "Version 4\nSymbolType CELL\nSYMATTR Value VENDOR1\nSYMATTR Prefix X\nPIN -32 48 NONE 8\nPINATTR PinName In-\nPINATTR SpiceOrder 1\nPIN -32 80 NONE 8\nPINATTR PinName In+\nPINATTR SpiceOrder 2\nPIN 32 64 NONE 8\nPINATTR PinName OUT\nPINATTR SpiceOrder 3\n",
        )
        .unwrap();
        let lib = SymbolLibrary::builtin_only()
            .with_dir(&dir.join("sym"), crate::symbol::SymbolSource::Ltspice);
        let src = OPAMP.replace("SYMBOL OpAmps/opamp", "SYMBOL vendorop");
        assert!(unknown_subckt(&src, &lib).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dangling_wire_is_reported() {
        let src = format!("{RC}WIRE 400 400 480 400\n");
        let r = rules(&src);
        assert_eq!(
            r.iter().filter(|x| *x == "dangling-wire").count(),
            2,
            "{r:?}"
        );
    }
}
