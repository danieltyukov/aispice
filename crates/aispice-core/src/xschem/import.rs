//! xschem schematic to LTspice `.asc`.
//!
//! The schematic is netlisted with aispice's own xschem netlister, the
//! netlist is made into a deck aispice can run, and the deck is drawn by the
//! schematic layout engine, which checks that the drawing netlists back to
//! it. Then the `.asc`'s netlist is compared with the xschem netlist: that is
//! the proof the conversion kept the circuit.
//!
//! Making the deck:
//!
//! - `.control ... .endc` blocks are dropped: they are ngspice scripts, which
//!   aispice's netlist policy refuses, and aispice runs its own analyses. The
//!   finding names the analyses the block ran.
//! - The PDK's model header (`.lib cornerMOSlv.lib mos_tt`) is dropped: in
//!   aispice the project's PDK supplies it. A section other than the typical
//!   one is named in the note.
//! - `.GLOBAL` lines go (a flat deck has no use for them), and so does `.end`.
//! - A relative `.include` whose file is not beside the new `.asc` is kept,
//!   with a finding.
//! - Every other line must pass [`crate::netlist::check_lexical`]; a refused
//!   line is dropped with a finding quoting the policy's reason.
//! - `GND` becomes `0`, xschem's numbered nets (`net3`) become LTspice's
//!   (`N001`) so the drawing labels only the nets the designer named, and
//!   `m=1`, SPICE's default, is dropped from R, C and L lines.

use super::file::XFile;
use super::netlist::{self, Finding, XNetlist};
use super::symbol::Resolver;
use crate::layout::{LayoutError, LayoutOptions, Quality};
use crate::netlist::{CompareOptions, Line, Netlist, check_lexical, compare_with};
use crate::schematic::Schematic;
use crate::symbol::SymbolLibrary;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub netlist: netlist::Options,
    pub layout: LayoutOptions,
    /// Where the `.asc` will be written. A relative `.include` whose file is
    /// there needs no finding.
    pub out_dir: Option<PathBuf>,
}

/// Whether two netlists were found to describe the same circuit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proof {
    /// What was compared with what.
    pub compared: String,
    pub equivalent: bool,
    pub differences: Vec<String>,
}

impl Proof {
    pub fn new(
        compared: impl Into<String>,
        result: Result<(), Vec<crate::netlist::Mismatch>>,
    ) -> Self {
        let differences: Vec<String> = match result {
            Ok(()) => Vec::new(),
            Err(m) => m.into_iter().map(|m| m.0).collect(),
        };
        Self {
            compared: compared.into(),
            equivalent: differences.is_empty(),
            differences,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Imported {
    pub schematic: Schematic,
    /// aispice's xschem netlist of the `.sch`, with what it read.
    pub xschem: XNetlist,
    /// The deck that was drawn.
    pub deck: Netlist,
    /// Things the import could not carry over, or carried over with a
    /// caveat: each one names what it is about.
    pub findings: Vec<Finding>,
    /// What was changed on purpose (dropped headers, renamed nets).
    pub notes: Vec<String>,
    pub quality: Quality,
    /// How a netlist of the `.sch` (aispice's or xschem's own, after
    /// [`clean`]) is compared with the `.asc`'s.
    pub compare: CompareOptions,
    /// The `.asc`'s netlist.
    pub asc_netlist: Netlist,
    /// aispice's xschem netlist of the `.sch` against the `.asc`'s netlist.
    pub proof: Proof,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ImportError {
    #[error(transparent)]
    Netlist(#[from] netlist::NetlistError),
    #[error("the schematic has nothing aispice can draw: {0}")]
    Layout(LayoutError),
}

/// A netlist made into a deck aispice may run, and what that took.
#[derive(Debug, Clone, Default)]
pub struct Cleaned {
    pub text: String,
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

/// Import the schematic at `path`.
pub fn import(
    path: &Path,
    file: &XFile,
    resolver: &Resolver,
    lib: &SymbolLibrary,
    opts: &ImportOptions,
) -> Result<Imported, ImportError> {
    let xn = netlist::netlist(path, file, resolver, &opts.netlist)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned = clean(&xn.text, opts.out_dir.as_deref());
    let mut findings = xn.findings.clone();
    findings.extend(cleaned.findings.iter().cloned());
    let mut notes = cleaned.notes.clone();

    let mut deck = crate::netlist::parse(&cleaned.text);
    deck.title = format!("Imported from xschem: {name}");
    notes.extend(rename_for_ltspice(&mut deck));
    strip_default_cards(&mut deck);

    let laid =
        crate::layout::from_netlist(&deck, lib, &opts.layout).map_err(ImportError::Layout)?;
    for w in &laid.warnings {
        if w.contains("SPICE line") {
            let (subject, message) = w.split_once(": ").unwrap_or(("", w.as_str()));
            findings.push(Finding {
                subject: subject.to_string(),
                message: message.to_string(),
            });
        } else {
            notes.push(w.clone());
        }
    }
    let compare = CompareOptions {
        auto_names: laid.renames.iter().map(|(_, to)| to.clone()).collect(),
        ..CompareOptions::xschem()
    };
    let (built, _) = crate::netlist::build(&laid.schematic, lib, &format!("* {name}"));
    let reference = crate::netlist::parse(&cleaned.text);
    let proof = Proof::new(
        "aispice's xschem netlist of the .sch against the .asc's netlist",
        compare_with(&reference, &built.netlist, &compare),
    );
    Ok(Imported {
        schematic: laid.schematic,
        xschem: xn,
        deck,
        findings,
        notes,
        quality: laid.quality,
        compare,
        asc_netlist: built.netlist,
        proof,
    })
}

/// Make an xschem netlist into a deck aispice may run. The same cleaning is
/// applied to xschem's own netlist before comparing it with the `.asc`.
pub fn clean(text: &str, out_dir: Option<&Path>) -> Cleaned {
    let mut out = Cleaned::default();
    let mut lines = text.lines();
    // The first line is SPICE's title.
    out.text.push_str(lines.next().unwrap_or("*"));
    out.text.push('\n');
    let mut control: Option<Vec<String>> = None;
    let mut globals = false;
    for line in lines {
        let t = line.trim();
        let low = t.to_ascii_lowercase();
        let keyword = low.split_whitespace().next().unwrap_or("");
        if let Some(block) = control.as_mut() {
            block.push(t.to_string());
            if keyword == ".endc" {
                let block = control.take().unwrap_or_default();
                out.findings.push(control_finding(&block));
            }
            continue;
        }
        if keyword == ".control" {
            control = Some(vec![t.to_string()]);
            continue;
        }
        if t.is_empty() || t.starts_with('*') {
            out.text.push_str(line);
            out.text.push('\n');
            continue;
        }
        if keyword == ".end" {
            continue;
        }
        if keyword == ".global" {
            globals = true;
            continue;
        }
        if let Some(note) = pdk_header(t) {
            if !out.notes.contains(&note) {
                out.notes.push(note);
            }
            continue;
        }
        let violations = check_lexical(t);
        if let Some(v) = violations.first() {
            out.findings.push(Finding {
                subject: first_word(t),
                message: format!("`{t}` is dropped: {}", v.reason),
            });
            continue;
        }
        if let Some(f) = include_finding(t, out_dir) {
            out.findings.push(f);
        }
        out.text.push_str(line);
        out.text.push('\n');
    }
    if let Some(block) = control {
        let mut f = control_finding(&block);
        f.message.push_str(" (it had no .endc)");
        out.findings.push(f);
    }
    if globals {
        out.notes
            .push(".GLOBAL lines are dropped: a flat deck has no use for them".to_string());
    }
    out
}

fn first_word(t: &str) -> String {
    t.split_whitespace().next().unwrap_or("").to_string()
}

/// The finding for a dropped control block, naming the analyses it ran so
/// they can be added as directives.
fn control_finding(block: &[String]) -> Finding {
    let analyses: Vec<&str> = block
        .iter()
        .map(String::as_str)
        .filter(|l| {
            let w = l
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            matches!(
                w.as_str(),
                "op" | "dc" | "ac" | "tran" | "noise" | "tf" | "pz" | "sp" | "disto"
            )
        })
        .collect();
    let mut message = format!(
        "a .control block of {} lines is dropped: it is an ngspice script, which aispice's netlist policy refuses, and aispice runs its own analyses",
        block.len()
    );
    if !analyses.is_empty() {
        let shown: Vec<String> = analyses.iter().take(4).map(|a| format!("`.{a}`")).collect();
        message.push_str(&format!(
            "; it ran {}, which can be added as directives",
            shown.join(", ")
        ));
    }
    Finding {
        subject: ".control".to_string(),
        message,
    }
}

/// A note when `line` is a PDK model header: a `.lib` of one of a known
/// kit's corner libraries.
fn pdk_header(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    if !words.next()?.eq_ignore_ascii_case(".lib") {
        return None;
    }
    let file = words.next()?.trim_matches(['"', '\'']);
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    let section = words.next().unwrap_or("");
    let (kit, lib) = crate::pdk::KITS
        .iter()
        .find_map(|k| k.library(base).map(|l| (k, l)))?;
    let mut note = format!(
        "the {} model header `.lib {base} {section}` is dropped: in aispice the project's PDK supplies it",
        kit.title
    );
    let typical = lib.family.default_section();
    if !section.is_empty() && !section.eq_ignore_ascii_case(&typical) {
        note.push_str(&format!(
            "; the schematic selected section {section}, not {typical}, so choose that corner when simulating"
        ));
    }
    Some(note)
}

/// A finding for a relative `.include` or `.lib` whose file is not beside
/// the new `.asc`.
fn include_finding(line: &str, out_dir: Option<&Path>) -> Option<Finding> {
    let mut words = line.split_whitespace();
    let kw = words.next()?.to_ascii_lowercase();
    if !matches!(kw.as_str(), ".include" | ".inc" | ".lib") {
        return None;
    }
    let file = words.next()?.trim_matches(['"', '\'']);
    if Path::new(file).is_absolute() {
        return None;
    }
    if out_dir.is_some_and(|d| d.join(file).is_file()) {
        return None;
    }
    Some(Finding {
        subject: kw,
        message: format!(
            "`{line}` is kept, but {file} is not beside the new schematic; copy it there (xschem's launcher writes such files) or remove the line"
        ),
    })
}

/// Rename nets the way an LTspice deck would have them, and drop `m=1` from
/// R, C and L lines. Returns notes.
fn rename_for_ltspice(deck: &mut Netlist) -> Vec<String> {
    // Nets a directive names keep their names, so the directive still
    // refers to them.
    let mut in_directives: BTreeSet<String> = BTreeSet::new();
    for d in deck.directives() {
        for w in d.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '!')) {
            if !w.is_empty() {
                in_directives.insert(w.to_ascii_lowercase());
            }
        }
    }
    let used: BTreeSet<String> = deck
        .nodes()
        .iter()
        .map(|n| n.to_ascii_uppercase())
        .collect();
    let mut map: HashMap<String, String> = HashMap::new();
    let mut next = 0u32;
    let mut renamed = 0usize;
    let mut ground = false;
    for item in deck.items.iter_mut() {
        let Line::Element(e) = item else { continue };
        for n in e.nodes.iter_mut() {
            if n.eq_ignore_ascii_case("gnd") {
                *n = "0".to_string();
                ground = true;
                continue;
            }
            if !is_xschem_auto(n) || in_directives.contains(&n.to_ascii_lowercase()) {
                continue;
            }
            let to = map.entry(n.clone()).or_insert_with(|| {
                loop {
                    next += 1;
                    let candidate = format!("N{next:03}");
                    if !used.contains(&candidate) {
                        renamed += 1;
                        break candidate;
                    }
                }
            });
            *n = to.clone();
        }
        let letter = e.letter();
        if matches!(letter, 'R' | 'C' | 'L') {
            let words: Vec<&str> = e.rest.split_whitespace().collect();
            let kept: Vec<&str> = words
                .iter()
                .copied()
                .filter(|w| !w.eq_ignore_ascii_case("m=1"))
                .collect();
            if kept.len() != words.len() && !kept.is_empty() {
                e.rest = kept.join(" ");
            }
        }
    }
    let mut notes = Vec::new();
    if ground {
        notes.push("xschem's GND is LTspice's ground, `0`".to_string());
    }
    if renamed > 0 {
        notes.push(format!(
            "{renamed} net(s) xschem numbered (net1, net2, ...) are numbered the LTspice way (N001, ...)"
        ));
    }
    notes
}

/// Drop the generic model cards (`.model NMOS NMOS`) the `.asc`'s netlist
/// adds by itself: as LTspice does, it adds a family's cards whenever a part
/// of that kind is drawn, whatever model the part names. Kept, the drawing
/// would netlist with each card twice.
fn strip_default_cards(deck: &mut Netlist) {
    let family = |name: &str| match name {
        "d" => Some('D'),
        "npn" | "pnp" => Some('Q'),
        "nmos" | "pmos" => Some('M'),
        "njf" | "pjf" => Some('J'),
        _ => None,
    };
    let letters: BTreeSet<char> = deck.elements().map(|e| e.letter()).collect();
    deck.items.retain(|item| {
        let Line::Directive { text } = item else {
            return true;
        };
        let w: Vec<String> = text
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        match w.as_slice() {
            [kw, name, kind] if kw == ".model" && name == kind => {
                !family(name).is_some_and(|l| letters.contains(&l))
            }
            _ => true,
        }
    });
}

fn is_xschem_auto(n: &str) -> bool {
    n.len() > 3
        && n.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("net"))
        && n[3..].bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests;
