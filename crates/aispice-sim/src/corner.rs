//! Process, voltage and temperature corners.
//!
//! A corner set crosses process corners, temperatures and supply values.
//! Each corner is one rewrite of the netlist, simulated like any other
//! variant, and the spec reports of all corners reduce to the worst case per
//! spec and the corner where it occurred.
//!
//! Process corners follow the convention of corner libraries: one top-level
//! `.lib <file> <family>_<corner>[_mismatch|_stat]` line per model family
//! (`mos_tt`, `res_typ_mismatch`, `dio_tt_stat`). A process corner changes
//! the `<corner>` part and keeps the rest, so it works on any library
//! written that way, a PDK or one of the user's own.
//!
//! ngspice cannot read library sections in the LTspice compatibility mode
//! aispice runs it in (it takes the one-argument `.lib name` section headers
//! for includes), so [`expand_lib_sections`] inlines them for ngspice and
//! LTspice; Xyce and Spectre read sections themselves.

use crate::measure::{human, lenient, plain};
use crate::spec::{Spec, SpecReport, SpecRow, evaluate};
use crate::sweep::{self, SweepError};
use aispice_core::netlist::{Line, Netlist};
use aispice_core::units;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Model families with corner sections.
pub const FAMILIES: &[&str] = &["mos", "res", "cap", "dio", "hbt"];

/// A corner set larger than this is refused: it is almost always a list
/// where fewer values were meant.
pub const MAX_CORNERS: usize = 200;

/// Library files larger than this are not read for their sections.
const MAX_LIBRARY_BYTES: u64 = 16 * 1024 * 1024;

/// Library sections nest at most this deep when expanded.
const MAX_SECTION_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CornerError {
    #[error("the corner set is empty: give process corners, temperatures or supply values")]
    Empty,
    #[error("{count} corners is more than the limit of {limit}; use fewer values")]
    TooMany { count: usize, limit: usize },
    #[error(
        "process corner `{0}` is neither a preset (tt, ss, ff, sf, fs) nor family=corner pairs such as `mos=ss res=wcs` (families: mos, res, cap, dio, hbt)"
    )]
    BadProcess(String),
    #[error(
        "process corner `{0}` needs `.lib <file> <family>_<corner>` lines (such as `.lib models.lib mos_tt`), and the circuit has none"
    )]
    NoLibraries(String),
    #[error(
        "process corner `{process}` sets {wanted}, but the circuit's corner libraries are for {found}"
    )]
    NoFamilies {
        process: String,
        wanted: String,
        found: String,
    },
    #[error("{file} has no section `{section}`; it has {}", .available.join(", "))]
    MissingSection {
        file: String,
        section: String,
        available: Vec<String>,
    },
    #[error("temperature {0} is not a temperature in Celsius")]
    BadTemperature(f64),
    #[error("supply {0}: {1}")]
    BadSupply(String, String),
    #[error(transparent)]
    Netlist(#[from] SweepError),
}

/// Whether a section is the plain corner, its mismatch variant or its
/// statistical variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    Nominal,
    /// `_mismatch`: device mismatch around the corner.
    Mismatch,
    /// `_stat`: global process spread plus mismatch.
    Stat,
}

/// A corner library section name, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibSection {
    pub family: String,
    pub corner: String,
    pub kind: SectionKind,
}

impl LibSection {
    pub fn name(&self) -> String {
        let suffix = match self.kind {
            SectionKind::Nominal => "",
            SectionKind::Mismatch => "_mismatch",
            SectionKind::Stat => "_stat",
        };
        format!("{}_{}{suffix}", self.family, self.corner)
    }
}

/// `mos_ss_mismatch` as family `mos`, corner `ss`, kind mismatch; `None`
/// for a name that is not a corner section of a known family.
pub fn parse_section(name: &str) -> Option<LibSection> {
    let lower = name.to_ascii_lowercase();
    let (family, rest) = lower.split_once('_')?;
    if !FAMILIES.contains(&family) {
        return None;
    }
    let (corner, kind) = if let Some(c) = rest.strip_suffix("_mismatch") {
        (c, SectionKind::Mismatch)
    } else if let Some(c) = rest.strip_suffix("_stat") {
        (c, SectionKind::Stat)
    } else {
        (rest, SectionKind::Nominal)
    };
    if corner.is_empty() || !corner.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(LibSection {
        family: family.into(),
        corner: corner.into(),
        kind,
    })
}

/// The section names a library file defines: its one-argument `.lib name`
/// lines.
pub fn lib_sections(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut w = line.split_whitespace();
        if w.next().is_some_and(|k| k.eq_ignore_ascii_case(".lib"))
            && let (Some(name), None) = (w.next(), w.next())
            && !out.iter().any(|o| o.eq_ignore_ascii_case(name))
        {
            out.push(name.to_string());
        }
    }
    out
}

/// A two-argument `.lib`: its keyword as written, the file (unquoted and as
/// written) and the section.
fn lib_line(text: &str) -> Option<(String, String, String, String)> {
    let t = text.trim();
    let kw_end = t.find(char::is_whitespace)?;
    let kw = &t[..kw_end];
    if !kw.eq_ignore_ascii_case(".lib") {
        return None;
    }
    let rest = t[kw_end..].trim_start();
    let (file, written, tail) = match rest.chars().next()? {
        q @ ('"' | '\'') => {
            let end = rest[1..].find(q)? + 1;
            (&rest[1..end], &rest[..=end], &rest[end + 1..])
        }
        _ => {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], &rest[..end], &rest[end..])
        }
    };
    let mut words = tail.split_whitespace();
    let section = words.next()?;
    if words.next().is_some() || file.is_empty() {
        return None;
    }
    Some((
        kw.to_string(),
        file.to_string(),
        written.to_string(),
        section.to_string(),
    ))
}

/// A top-level corner-library line of a netlist.
#[derive(Debug, Clone, PartialEq)]
pub struct SectionUse {
    /// Index into the netlist's items.
    pub index: usize,
    /// The library file, as the line names it.
    pub file: String,
    pub section: LibSection,
    keyword: String,
    written_file: String,
    written_section: String,
}

/// Every top-level `.lib <file> <family>_<corner>...` line.
pub fn section_uses(netlist: &Netlist) -> Vec<SectionUse> {
    netlist
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let Line::Directive { text } = item else {
                return None;
            };
            let (keyword, file, written_file, written_section) = lib_line(text)?;
            let section = parse_section(&written_section)?;
            Some(SectionUse {
                index,
                file,
                section,
                keyword,
                written_file,
                written_section,
            })
        })
        .collect()
}

/// Put another section on a corner-library line, in the case the line used.
pub fn set_section(netlist: &mut Netlist, at: &SectionUse, section: &LibSection) {
    let mut name = section.name();
    if !at.written_section.chars().any(|c| c.is_ascii_lowercase()) {
        name = name.to_ascii_uppercase();
    }
    netlist.items[at.index] = Line::Directive {
        text: format!("{} {} {name}", at.keyword, at.written_file),
    };
}

/// Lists the sections of a library file named in a netlist, or `None` when
/// the file cannot be read (the simulator then has the last word).
pub type SectionLookup<'a> = &'a dyn Fn(&str) -> Option<Vec<String>>;

/// A lookup that reads nothing.
pub fn no_lookup(_: &str) -> Option<Vec<String>> {
    None
}

/// Whether a library defines a section, when that can be told.
pub fn defines(lookup: SectionLookup, file: &str, section: &str) -> Option<bool> {
    lookup(file).map(|list| list.iter().any(|s| s.eq_ignore_ascii_case(section)))
}

/// A process corner: the corner each model family is put at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Process {
    pub name: String,
    /// Family and corner, such as `("mos", "ss")`.
    pub sections: Vec<(String, String)>,
}

/// The process presets: MOS at its corner, passives and HBTs at the
/// matching worst (`wcs`) or best (`bcs`) case, diodes with the MOS corner.
/// The skewed MOS corners leave everything else typical.
pub fn preset(name: &str) -> Option<Process> {
    let (mos, passive, dio) = match name.trim().to_ascii_lowercase().as_str() {
        "tt" => ("tt", "typ", "tt"),
        "ss" => ("ss", "wcs", "ss"),
        "ff" => ("ff", "bcs", "ff"),
        "sf" => ("sf", "typ", "tt"),
        "fs" => ("fs", "typ", "tt"),
        _ => return None,
    };
    Some(Process {
        name: mos.to_string(),
        sections: vec![
            ("mos".into(), mos.into()),
            ("res".into(), passive.into()),
            ("cap".into(), passive.into()),
            ("hbt".into(), passive.into()),
            ("dio".into(), dio.into()),
        ],
    })
}

impl Process {
    /// A preset name, or `family=corner` pairs separated by spaces or
    /// commas: `mos=ss res=wcs`.
    pub fn parse(text: &str) -> Result<Process, CornerError> {
        if let Some(p) = preset(text) {
            return Ok(p);
        }
        let bad = || CornerError::BadProcess(text.trim().to_string());
        let mut sections: Vec<(String, String)> = Vec::new();
        for pair in text
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
        {
            let (family, corner) = pair.split_once('=').ok_or_else(bad)?;
            let family = family.trim().to_ascii_lowercase();
            let corner = corner.trim().to_ascii_lowercase();
            if !FAMILIES.contains(&family.as_str())
                || corner.is_empty()
                || !corner.chars().all(|c| c.is_ascii_alphanumeric())
                || sections.iter().any(|(f, _)| *f == family)
            {
                return Err(bad());
            }
            sections.push((family, corner));
        }
        if sections.is_empty() {
            return Err(bad());
        }
        let name = sections
            .iter()
            .map(|(f, c)| format!("{f}={c}"))
            .collect::<Vec<_>>()
            .join(",");
        Ok(Process { name, sections })
    }

    pub fn corner_for(&self, family: &str) -> Option<&str> {
        self.sections
            .iter()
            .find(|(f, _)| f.eq_ignore_ascii_case(family))
            .map(|(_, c)| c.as_str())
    }
}

/// Values for one supply: a voltage source or a `.param`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Supply {
    /// A source (`VDD`) or `.param` name.
    pub target: String,
    /// Its values, as numbers or SPICE text (`"1.8"`).
    #[serde(deserialize_with = "lenient::vec")]
    pub values: Vec<f64>,
}

/// Process corners times temperatures times supply values. Every part is
/// optional; an empty part leaves the netlist as it is in that respect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CornerSet {
    /// Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or
    /// family=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,
    /// dio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.
    #[serde(default)]
    pub process: Vec<String>,
    /// Temperatures in degrees Celsius.
    #[serde(default, deserialize_with = "lenient::vec")]
    pub temperatures: Vec<f64>,
    /// Supply values, for example `[{"target": "VDD", "values": [1.62,
    /// 1.8, 1.98]}]`.
    #[serde(default)]
    pub supplies: Vec<Supply>,
}

/// One corner of a set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Corner {
    /// `ss/125C/VDD=1.62`.
    pub name: String,
    pub process: Option<Process>,
    pub temperature: Option<f64>,
    pub supplies: Vec<(String, f64)>,
}

impl CornerSet {
    pub fn is_empty(&self) -> bool {
        self.process.is_empty() && self.temperatures.is_empty() && self.supplies.is_empty()
    }

    /// Every corner, process first and the last supply varying fastest.
    pub fn corners(&self) -> Result<Vec<Corner>, CornerError> {
        if self.is_empty() {
            return Err(CornerError::Empty);
        }
        let processes: Vec<Option<Process>> = if self.process.is_empty() {
            vec![None]
        } else {
            self.process
                .iter()
                .map(|p| Process::parse(p).map(Some))
                .collect::<Result<_, _>>()?
        };
        if let Some(t) = self
            .temperatures
            .iter()
            .find(|t| !t.is_finite() || **t < -273.15)
        {
            return Err(CornerError::BadTemperature(*t));
        }
        let temps: Vec<Option<f64>> = if self.temperatures.is_empty() {
            vec![None]
        } else {
            self.temperatures.iter().copied().map(Some).collect()
        };
        for (i, s) in self.supplies.iter().enumerate() {
            let bad = |why: &str| CornerError::BadSupply(s.target.clone(), why.to_string());
            if s.values.is_empty() {
                return Err(bad("no values"));
            }
            if s.values.iter().any(|v| !v.is_finite()) {
                return Err(bad("values must be finite numbers"));
            }
            if self.supplies[..i]
                .iter()
                .any(|o| o.target.eq_ignore_ascii_case(&s.target))
            {
                return Err(bad("given twice"));
            }
        }
        let combos = self
            .supplies
            .iter()
            .try_fold(1usize, |acc, s| acc.checked_mul(s.values.len()))
            .unwrap_or(usize::MAX);
        let count = processes
            .len()
            .saturating_mul(temps.len())
            .saturating_mul(combos);
        if count > MAX_CORNERS {
            return Err(CornerError::TooMany {
                count,
                limit: MAX_CORNERS,
            });
        }
        let mut supply_sets: Vec<Vec<(String, f64)>> = vec![Vec::new()];
        for s in &self.supplies {
            supply_sets = supply_sets
                .into_iter()
                .flat_map(|prefix| {
                    s.values.iter().map(move |v| {
                        let mut next = prefix.clone();
                        next.push((s.target.clone(), *v));
                        next
                    })
                })
                .collect();
        }
        let mut out = Vec::with_capacity(count);
        for p in &processes {
            for t in &temps {
                for supplies in &supply_sets {
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(p) = p {
                        parts.push(p.name.clone());
                    }
                    if let Some(t) = t {
                        parts.push(format!("{}C", plain(*t)));
                    }
                    for (target, v) in supplies {
                        parts.push(format!("{target}={}", units::format(*v)));
                    }
                    out.push(Corner {
                        name: parts.join("/"),
                        process: p.clone(),
                        temperature: *t,
                        supplies: supplies.clone(),
                    });
                }
            }
        }
        Ok(out)
    }
}

/// Put the netlist's corner libraries at a process corner.
fn set_process(
    netlist: &mut Netlist,
    process: &Process,
    lookup: SectionLookup,
) -> Result<(), CornerError> {
    let uses = section_uses(netlist);
    if uses.is_empty() {
        return Err(CornerError::NoLibraries(process.name.clone()));
    }
    let mut changed = 0;
    for u in &uses {
        let Some(corner) = process.corner_for(&u.section.family) else {
            continue;
        };
        let next = LibSection {
            corner: corner.to_string(),
            ..u.section.clone()
        };
        if defines(lookup, &u.file, &next.name()) == Some(false) {
            return Err(CornerError::MissingSection {
                file: u.file.clone(),
                section: next.name(),
                available: lookup(&u.file).unwrap_or_default(),
            });
        }
        set_section(netlist, u, &next);
        changed += 1;
    }
    if changed == 0 {
        let mut found: Vec<&str> = uses.iter().map(|u| u.section.family.as_str()).collect();
        found.dedup();
        return Err(CornerError::NoFamilies {
            process: process.name.clone(),
            wanted: process
                .sections
                .iter()
                .map(|(f, _)| f.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            found: found.join(", "),
        });
    }
    Ok(())
}

/// Replace the netlist's temperature: every `.temp` line and `temp=` option
/// goes, and one `.temp` line comes in.
pub fn set_temperature(netlist: &mut Netlist, celsius: f64) {
    let kw = |t: &str| {
        t.split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    netlist.items.retain(|item| match item {
        Line::Directive { text } => kw(text) != ".temp",
        _ => true,
    });
    netlist.items.retain_mut(|item| {
        let Line::Directive { text } = item else {
            return true;
        };
        if !matches!(kw(text).as_str(), ".options" | ".option" | ".opt" | ".opts") {
            return true;
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut kept: Vec<&str> = vec![words[0]];
        let mut i = 1;
        while i < words.len() {
            let lower = words[i].to_ascii_lowercase();
            if lower == "temp" && words.get(i + 1) == Some(&"=") {
                i += 3;
            } else if lower == "temp=" {
                i += 2;
            } else if lower.starts_with("temp=") {
                i += 1;
            } else {
                kept.push(words[i]);
                i += 1;
            }
        }
        if kept.len() == words.len() {
            return true;
        }
        let keep = kept.len() > 1;
        let joined = kept.join(" ");
        *text = joined;
        keep
    });
    netlist.items.push(Line::Directive {
        text: format!(".temp {}", crate::expr::format_number(celsius)),
    });
}

/// The netlist at one corner. `lookup` lists a library's sections so a
/// missing one is reported before anything runs.
pub fn apply(
    netlist: &Netlist,
    corner: &Corner,
    lookup: SectionLookup,
) -> Result<Netlist, CornerError> {
    let mut n = netlist.clone();
    if let Some(p) = &corner.process {
        set_process(&mut n, p, lookup)?;
    }
    if let Some(t) = corner.temperature {
        set_temperature(&mut n, t);
    }
    for (target, v) in &corner.supplies {
        sweep::set_value(&mut n, target, *v)?;
    }
    Ok(n)
}

/// One netlist per corner of the set, labelled with the corner's name.
pub fn variants(
    netlist: &Netlist,
    set: &CornerSet,
    lookup: SectionLookup,
) -> Result<Vec<(String, Netlist)>, CornerError> {
    set.corners()?
        .iter()
        .map(|c| apply(netlist, c, lookup).map(|n| (c.name.clone(), n)))
        .collect()
}

/// One spec across the corners.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CornerRow {
    pub name: String,
    pub unit: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// The value at each corner, in the order of `CornerReport::corners`;
    /// `None` where there is no value or the corner failed to simulate.
    pub values: Vec<Option<f64>>,
    /// The value at the worst corner: the smallest margin, or a corner with
    /// no value.
    pub worst_value: Option<f64>,
    pub worst_margin: Option<f64>,
    pub worst_corner: Option<String>,
    /// Passes at every corner.
    pub pass: bool,
    /// One line for people: `bw: 0.9MHz worst at ss/125C (>= 1MHz) FAIL,
    /// margin -10.0%`.
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CornerReport {
    /// Corner names, in run order.
    pub corners: Vec<String>,
    /// Corners whose simulation failed, with the reason.
    pub failed: Vec<(String, String)>,
    pub rows: Vec<CornerRow>,
    /// The worst case over every corner as one spec report, each row from
    /// its worst corner (named in `worst_step`).
    pub worst: SpecReport,
    /// The corner closest to failing, or failing worst.
    pub worst_corner: Option<String>,
    pub all_pass: bool,
    pub summary: String,
}

/// How bad a row is: lower is worse. A missing value is the worst there
/// is; rows with limits compare by margin, rows with only a target by their
/// distance from it.
fn badness(row: &SpecRow) -> f64 {
    let limited = row.min.is_some() || row.max.is_some();
    match (row.value, limited, row.target) {
        (None, true, _) | (None, false, Some(_)) => f64::NEG_INFINITY,
        (Some(_), true, _) => row.margin.unwrap_or(f64::NEG_INFINITY),
        (Some(v), false, Some(t)) => {
            let scale = if t.abs() > 1e-300 { t.abs() } else { 1.0 };
            -(v - t).abs() / scale
        }
        _ => f64::INFINITY,
    }
}

/// The worst case of several labelled spec reports of the same specs: each
/// row taken from the report where it is worst, labelled in `worst_step`,
/// and passing only if it passes in every report.
pub fn worst_case(reports: &[(String, SpecReport)]) -> SpecReport {
    let Some((_, first)) = reports.first() else {
        return SpecReport {
            rows: Vec::new(),
            all_pass: false,
            summary: "no corners".into(),
        };
    };
    let rows: Vec<SpecRow> = first
        .rows
        .iter()
        .map(|r0| {
            let mut worst: Option<(f64, &str, &SpecRow)> = None;
            for (label, report) in reports {
                let Some(row) = report.row(&r0.name) else {
                    continue;
                };
                let b = badness(row);
                if worst.is_none_or(|(w, _, _)| b < w) {
                    worst = Some((b, label, row));
                }
            }
            let (_, label, row) = worst.unwrap_or((0.0, &reports[0].0, r0));
            let mut w = row.clone();
            w.pass = reports
                .iter()
                .all(|(_, r)| r.row(&r0.name).is_some_and(|x| x.pass));
            w.worst_step = Some(match &row.worst_step {
                Some(step) if !step.is_empty() => format!("{label}, {step}"),
                _ => label.to_string(),
            });
            w.display = crate::spec::row_display(&w, true);
            w
        })
        .collect();
    let all_pass = rows.iter().all(|r| r.pass);
    let summary = crate::spec::summarize(&rows);
    SpecReport {
        rows,
        all_pass,
        summary,
    }
}

/// A report for a corner that failed to simulate: every spec without a
/// value, the failure as the note.
fn failed_report(specs: &[Spec], why: &str) -> SpecReport {
    let mut r = evaluate(specs, &[]);
    for row in &mut r.rows {
        row.note = Some(format!("the simulation failed: {why}"));
        row.display = crate::spec::row_display(row, false);
    }
    r
}

/// Reduce the spec reports of every corner, in corner order, to the worst
/// case per spec and the full table.
pub fn summarize(
    specs: &[Spec],
    corners: &[String],
    results: &[Result<SpecReport, String>],
) -> CornerReport {
    let mut failed = Vec::new();
    let reports: Vec<(String, SpecReport)> = corners
        .iter()
        .zip(results)
        .map(|(c, r)| match r {
            Ok(r) => (c.clone(), r.clone()),
            Err(e) => {
                failed.push((c.clone(), e.clone()));
                (c.clone(), failed_report(specs, e))
            }
        })
        .collect();
    let worst = worst_case(&reports);
    let rows: Vec<CornerRow> = worst
        .rows
        .iter()
        .map(|w| CornerRow {
            name: w.name.clone(),
            unit: w.unit.clone(),
            min: w.min,
            max: w.max,
            values: reports
                .iter()
                .map(|(_, r)| r.row(&w.name).and_then(|x| x.value))
                .collect(),
            worst_value: w.value,
            worst_margin: w.margin,
            worst_corner: w.worst_step.clone(),
            pass: w.pass,
            display: w.display.clone(),
        })
        .collect();
    let worst_corner = reports
        .iter()
        .map(|(c, r)| (c, r.worst_margin().unwrap_or(f64::NEG_INFINITY)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, m)| m.is_finite() || *m == f64::NEG_INFINITY)
        .map(|(c, _)| c.clone());
    let all_pass = worst.all_pass && failed.is_empty() && !reports.is_empty();
    let n = corners.len();
    let mut summary = if all_pass {
        match rows.len() {
            1 => format!("the spec passes at all {n} corners"),
            k => format!("all {k} specs pass at all {n} corners"),
        }
    } else {
        let failing: Vec<String> = rows
            .iter()
            .filter(|r| !r.pass)
            .map(|r| {
                let at = r.worst_corner.as_deref().unwrap_or("?");
                match r.worst_value {
                    Some(v) => format!("{} ({} at {at})", r.name, human(v, &r.unit)),
                    None => format!("{} (no value at {at})", r.name),
                }
            })
            .collect();
        format!(
            "{} of {} specs pass at all {n} corners; failing: {}",
            rows.len() - failing.len(),
            rows.len(),
            failing.join(", ")
        )
    };
    if !failed.is_empty() {
        summary.push_str(&format!("; {} corner(s) failed to simulate", failed.len()));
    }
    CornerReport {
        corners: corners.to_vec(),
        failed,
        rows,
        worst,
        worst_corner,
        all_pass,
        summary,
    }
}

/// Replace every two-argument `.lib <file> <section>` line of a deck with
/// that section of the file, for simulators that cannot select a section
/// themselves. Relative paths inside a section are made absolute against
/// the library's own folder, and sections it selects in turn are expanded
/// too. `base` is the folder relative library paths in the deck start from.
///
/// Call it only on a deck the netlist policy has accepted (the policy reads
/// every library the deck names), and check the result again.
pub fn expand_lib_sections(deck: &str, base: &Path) -> Result<String, String> {
    let mut out = String::with_capacity(deck.len());
    expand(deck, base, 0, &mut out)?;
    Ok(out)
}

fn expand(text: &str, base: &Path, depth: usize, out: &mut String) -> Result<(), String> {
    for line in text.lines() {
        if let Some((_, file, _, section)) = lib_line(line) {
            if depth >= MAX_SECTION_DEPTH {
                return Err(format!(
                    "library sections nest deeper than {MAX_SECTION_DEPTH} levels"
                ));
            }
            let path = if Path::new(&file).is_absolute() {
                PathBuf::from(&file)
            } else {
                base.join(&file)
            };
            let body = read_section(&path, &section)?;
            let dir = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| base.to_path_buf());
            out.push_str(&format!(
                "* aispice: section {section} of {}\n",
                path.display()
            ));
            expand(&body, &dir, depth + 1, out)?;
            out.push_str(&format!("* aispice: end of section {section}\n"));
            continue;
        }
        if depth > 0 {
            out.push_str(&absolute_include(line, base));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    Ok(())
}

/// A one-argument `.include` or `.lib` inside a library section, with a
/// relative file that exists made absolute.
fn absolute_include(line: &str, base: &Path) -> String {
    let t = line.trim_start();
    let mut words = t.split_whitespace();
    let kw = words.next().unwrap_or("");
    if !matches!(
        kw.to_ascii_lowercase().as_str(),
        ".include" | ".inc" | ".lib"
    ) {
        return line.to_string();
    }
    let rest = t[kw.len()..].trim();
    let arg = rest.trim_matches(|c| c == '"' || c == '\'');
    if arg.is_empty() || arg.contains(char::is_whitespace) && !rest.starts_with(['"', '\'']) {
        return line.to_string();
    }
    let p = Path::new(arg);
    if p.is_absolute() {
        return line.to_string();
    }
    match std::fs::canonicalize(base.join(p)) {
        Ok(real) => format!("{kw} \"{}\"", real.display()),
        Err(_) => line.to_string(),
    }
}

fn read_section(path: &Path, section: &str) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_LIBRARY_BYTES {
        return Err(format!("{} is larger than 16 MiB", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (text, _) = aispice_core::encoding::decode(&bytes);
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        let mut w = line.split_whitespace();
        let kw = w.next().unwrap_or("").to_ascii_lowercase();
        if !inside {
            if kw == ".lib"
                && let (Some(name), None) = (w.next(), w.next())
                && name.eq_ignore_ascii_case(section)
            {
                inside = true;
            }
            continue;
        }
        if kw == ".endl" {
            return Ok(body);
        }
        body.push_str(line);
        body.push('\n');
    }
    if inside {
        return Err(format!(
            "{}: section `{section}` has no `.endl`",
            path.display()
        ));
    }
    Err(format!(
        "{} has no section `{section}`; it has {}",
        path.display(),
        lib_sections(&text).join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::test_support::*;
    use crate::spec::parse_specs;
    use aispice_core::netlist::{parse, write};

    const DECK: &str = "* corners\n.lib /pdk/corner.lib mos_tt\n.LIB \"/pdk/res lib.lib\" RES_TYP_MISMATCH\n.lib /pdk/dio.lib dio_tt\n.lib opamp.sub\nVDD vdd 0 1.8\nR1 vdd out {rload}\n.param rload=1k vref=0.9\n.options reltol=1m temp=50\n.temp 85\n.ac dec 10 1 1Meg\n.end\n";

    fn sections(file: &str) -> Option<Vec<String>> {
        let list: &[&str] = match file {
            "/pdk/corner.lib" => &["mos_tt", "mos_ss", "mos_ff", "mos_sf", "mos_fs"],
            "/pdk/res lib.lib" => &[
                "res_typ",
                "res_wcs",
                "res_bcs",
                "res_typ_mismatch",
                "res_wcs_mismatch",
                "res_bcs_mismatch",
            ],
            _ => return None,
        };
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn section_names_parse() {
        let s = parse_section("mos_ss_mismatch").unwrap();
        assert_eq!((s.family.as_str(), s.corner.as_str()), ("mos", "ss"));
        assert_eq!(s.kind, SectionKind::Mismatch);
        assert_eq!(
            parse_section("RES_TYP_STAT").unwrap().name(),
            "res_typ_stat"
        );
        assert_eq!(parse_section("dio_tt").unwrap().kind, SectionKind::Nominal);
        assert!(parse_section("tt").is_none());
        assert!(parse_section("my_amp").is_none());
        assert!(parse_section("mos_").is_none());
        assert_eq!(
            lib_sections(
                ".LIB mos_tt\n.param a=1\n.ENDL mos_tt\n.lib mos_ss\n.endl\n.lib x.lib y\n"
            ),
            vec!["mos_tt", "mos_ss"]
        );
    }

    #[test]
    fn corner_sets_cross_and_name_their_corners() {
        let set: CornerSet = serde_json::from_value(serde_json::json!({
            "process": ["tt", "ss"],
            "temperatures": [-40, "27"],
            "supplies": [{"target": "VDD", "values": ["1.62", 1.98]}]
        }))
        .unwrap();
        let names: Vec<String> = set.corners().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "tt/-40C/VDD=1.62",
                "tt/-40C/VDD=1.98",
                "tt/27C/VDD=1.62",
                "tt/27C/VDD=1.98",
                "ss/-40C/VDD=1.62",
                "ss/-40C/VDD=1.98",
                "ss/27C/VDD=1.62",
                "ss/27C/VDD=1.98"
            ]
        );
        assert_eq!(CornerSet::default().corners(), Err(CornerError::Empty));
        let many = CornerSet {
            temperatures: (0..201).map(f64::from).collect(),
            ..Default::default()
        };
        assert!(matches!(many.corners(), Err(CornerError::TooMany { .. })));
        let bad = CornerSet {
            process: vec!["slow".into()],
            ..Default::default()
        };
        assert!(matches!(bad.corners(), Err(CornerError::BadProcess(_))));
        let custom = Process::parse("mos=ss, res=WCS").unwrap();
        assert_eq!(custom.name, "mos=ss,res=wcs");
        assert_eq!(custom.corner_for("res"), Some("wcs"));
        assert!(Process::parse("gate=ss").is_err());
        assert!(Process::parse("mos=ss mos=ff").is_err());
        let cold = CornerSet {
            temperatures: vec![-300.0],
            ..Default::default()
        };
        assert_eq!(cold.corners(), Err(CornerError::BadTemperature(-300.0)));
    }

    #[test]
    fn a_corner_rewrites_sections_temperature_and_supplies() {
        let n = parse(DECK);
        let set = CornerSet {
            process: vec!["ss".into()],
            temperatures: vec![125.0],
            supplies: vec![
                Supply {
                    target: "VDD".into(),
                    values: vec![1.62],
                },
                Supply {
                    target: "vref".into(),
                    values: vec![0.8],
                },
            ],
        };
        let v = variants(&n, &set, &sections).unwrap();
        assert_eq!(v[0].0, "ss/125C/VDD=1.62/vref=800m");
        let out = write(&v[0].1);
        assert!(out.contains(".lib /pdk/corner.lib mos_ss\n"), "{out}");
        // The suffix and the case of the line are kept.
        assert!(
            out.contains(".LIB \"/pdk/res lib.lib\" RES_WCS_MISMATCH\n"),
            "{out}"
        );
        // Not readable: rewritten, and the simulator has the last word.
        assert!(out.contains(".lib /pdk/dio.lib dio_ss\n"), "{out}");
        assert!(out.contains(".lib opamp.sub\n"), "{out}");
        assert!(out.contains("VDD vdd 0 1.62\n"), "{out}");
        assert!(out.contains(".param rload=1k vref=800m"), "{out}");
        assert!(out.contains(".options reltol=1m\n"), "{out}");
        assert_eq!(out.matches(".temp").count(), 1, "{out}");
        assert!(out.contains(".temp 125\n"), "{out}");
    }

    #[test]
    fn process_corners_need_libraries_that_have_them() {
        let n = parse(DECK);
        let at = |p: &str| Corner {
            name: p.into(),
            process: Some(Process::parse(p).unwrap()),
            temperature: None,
            supplies: Vec::new(),
        };
        // A section the library does not define is named before any run.
        let err = apply(&n, &at("mos=xx"), &sections).unwrap_err();
        assert_eq!(
            err.to_string(),
            "/pdk/corner.lib has no section `mos_xx`; it has mos_tt, mos_ss, mos_ff, mos_sf, mos_fs"
        );
        let err = apply(&n, &at("hbt=wcs"), &sections).unwrap_err();
        assert!(matches!(err, CornerError::NoFamilies { .. }), "{err}");
        let plain = parse("* x\nR1 a 0 1k\n.op\n.end\n");
        let err = apply(&plain, &at("tt"), &no_lookup).unwrap_err();
        assert!(matches!(err, CornerError::NoLibraries(_)), "{err}");
        // Temperatures and supplies need no library.
        let warm = Corner {
            name: "85C".into(),
            process: None,
            temperature: Some(85.0),
            supplies: Vec::new(),
        };
        assert!(write(&apply(&plain, &warm, &no_lookup).unwrap()).contains(".temp 85\n"));
    }

    fn rc_reports(fcs: &[(&str, f64)]) -> Vec<(String, SpecReport)> {
        let specs =
            parse_specs("bw = bandwidth_3db(V(out)) >= 1k\ng = gain_db_at(V(out), 10) = 0 +- 1")
                .unwrap();
        let f = logspace(1.0, 1e6, 121);
        fcs.iter()
            .map(|(name, fc)| {
                let h = f.iter().map(|&x| poles(x, &[*fc], 1.0)).collect();
                (
                    name.to_string(),
                    evaluate(&specs, &[ac(f.clone(), vec![("V(out)", h)])]),
                )
            })
            .collect()
    }

    #[test]
    fn the_worst_corner_is_reported_per_spec() {
        let reports = rc_reports(&[("tt", 1.6e3), ("ss", 0.9e3), ("ff", 2.5e3)]);
        let worst = worst_case(&reports);
        let bw = worst.row("bw").unwrap();
        assert_eq!(bw.worst_step.as_deref(), Some("ss"));
        assert!((bw.value.unwrap() - 900.0).abs() < 5.0, "{:?}", bw.value);
        assert!(!bw.pass);
        assert!(bw.display.contains("worst at ss"), "{}", bw.display);
        assert!(!worst.all_pass);

        let specs =
            parse_specs("bw = bandwidth_3db(V(out)) >= 1k\ng = gain_db_at(V(out), 10) = 0 +- 1")
                .unwrap();
        let names: Vec<String> = ["tt", "ss", "ff", "sf"].map(String::from).to_vec();
        let mut results: Vec<Result<SpecReport, String>> =
            reports.into_iter().map(|(_, r)| Ok(r)).collect();
        results.push(Err("no convergence".into()));
        let r = summarize(&specs, &names, &results);
        assert_eq!(r.corners, names);
        assert_eq!(
            r.failed,
            vec![("sf".to_string(), "no convergence".to_string())]
        );
        assert!(!r.all_pass);
        let bw = &r.rows[0];
        assert_eq!(bw.values.len(), 4);
        assert!(bw.values[3].is_none());
        // A corner with no value is the worst there is.
        assert_eq!(bw.worst_corner.as_deref(), Some("sf"));
        assert_eq!(r.worst_corner.as_deref(), Some("sf"));
        assert!(
            r.summary.ends_with("1 corner(s) failed to simulate"),
            "{}",
            r.summary
        );

        let good = rc_reports(&[("tt", 1.6e3), ("ff", 2.5e3)]);
        let names: Vec<String> = good.iter().map(|(n, _)| n.clone()).collect();
        let results: Vec<_> = good.into_iter().map(|(_, r)| Ok(r)).collect();
        let r = summarize(&specs, &names, &results);
        assert!(r.all_pass);
        assert_eq!(r.summary, "all 2 specs pass at all 2 corners");
        assert_eq!(r.rows[0].worst_corner.as_deref(), Some("tt"));
        assert_eq!(r.worst_corner.as_deref(), Some("tt"));
    }

    #[test]
    fn sections_expand_inline_with_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("models.inc"), ".model d1 D(IS=1e-14)\n").unwrap();
        std::fs::write(
            lib.join("corner.lib"),
            "* corners\n.LIB res_typ\n.param rsh=1k\n.include models.inc\n.lib inner.lib extra\n.ENDL res_typ\n.LIB res_wcs\n.param rsh=1.2k\n.ENDL res_wcs\n",
        )
        .unwrap();
        std::fs::write(lib.join("inner.lib"), ".lib extra\n.param x=2\n.endl\n").unwrap();
        let deck = "* t\n.lib lib/corner.lib res_typ\nR1 a 0 {rsh}\n.lib opamp.sub\n.end\n";
        let out = expand_lib_sections(deck, dir.path()).unwrap();
        let models = std::fs::canonicalize(lib.join("models.inc")).unwrap();
        assert!(out.contains(".param rsh=1k\n"), "{out}");
        assert!(!out.contains("rsh=1.2k"), "{out}");
        assert!(
            out.contains(&format!(".include \"{}\"\n", models.display())),
            "{out}"
        );
        assert!(out.contains(".param x=2\n"), "{out}");
        assert!(out.contains("R1 a 0 {rsh}\n.lib opamp.sub\n"), "{out}");
        assert!(!out.contains(".lib lib/corner.lib"), "{out}");
        let err =
            expand_lib_sections("* t\n.lib lib/corner.lib res_bcs\n", dir.path()).unwrap_err();
        assert!(
            err.ends_with("has no section `res_bcs`; it has res_typ, res_wcs"),
            "{err}"
        );
        assert!(expand_lib_sections("* t\n.lib lib/none.lib x\n", dir.path()).is_err());
        // A section that selects itself stops at the depth limit.
        std::fs::write(lib.join("loop.lib"), ".lib s\n.lib loop.lib s\n.endl\n").unwrap();
        let err = expand_lib_sections("* t\n.lib lib/loop.lib s\n", dir.path()).unwrap_err();
        assert!(err.contains("nest deeper"), "{err}");
    }
}
