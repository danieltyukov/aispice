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
//! `.lib` lines are read with the netlist policy's own parser, and the
//! libraries they name are read only through [`crate::library`], which asks
//! the policy first.

use crate::library::LibraryInfo;
use crate::measure::{human, lenient, plain};
use crate::spec::{Spec, SpecReport, SpecRow, evaluate};
use crate::sweep::{self, SweepError};
use aispice_core::netlist::{Line, Netlist, parse_include};
use aispice_core::units;
use schemars::JsonSchema;
use serde::de::{Error as _, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::marker::PhantomData;

/// Model families with corner sections.
pub const FAMILIES: &[&str] = &["mos", "res", "cap", "dio", "hbt"];

/// A corner set larger than this is refused: it is almost always a list
/// where fewer values were meant.
pub const MAX_CORNERS: usize = 200;

/// The most process corners, temperatures, or values of one supply a set
/// may list. Longer lists are refused while they are read.
pub const MAX_LIST: usize = 64;

/// The most supplies a set may vary.
pub const MAX_SUPPLIES: usize = 8;

/// A process corner's text is at most this long.
const MAX_PROCESS_TEXT: usize = 200;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CornerError {
    #[error("the corner set is empty: give process corners, temperatures or supply values")]
    Empty,
    #[error("{what}: at most {limit}, found {count}")]
    ListTooLong {
        what: &'static str,
        count: usize,
        limit: usize,
    },
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
    #[error("{file} has no section `{section}`{}", listed(.available))]
    MissingSection {
        file: String,
        section: String,
        /// The library's sections, only for a library inside the project.
        available: Vec<String>,
    },
    #[error("`{file}`: {reason}")]
    Refused { file: String, reason: String },
    #[error("temperature {0} is not a temperature in Celsius")]
    BadTemperature(f64),
    #[error("supply {0}: {1}")]
    BadSupply(String, String),
    #[error(transparent)]
    Netlist(#[from] SweepError),
}

fn listed(available: &[String]) -> String {
    if available.is_empty() {
        String::new()
    } else {
        format!("; it has {}", available.join(", "))
    }
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
            let inc = parse_include(text)?.ok()?;
            if !inc.keyword.eq_ignore_ascii_case(".lib") {
                return None;
            }
            let written_section = inc.section?;
            let section = parse_section(&written_section)?;
            Some(SectionUse {
                index,
                file: inc.file,
                section,
                keyword: inc.keyword,
                written_file: inc.written_file,
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

/// What a library a netlist names defines, as
/// [`crate::library::LibraryFiles::sections`] answers: `Ok(None)` when that
/// cannot be told (the simulator then has the last word), the policy's
/// reason when it refuses the file.
pub type SectionLookup<'a> = &'a dyn Fn(&str) -> Result<Option<LibraryInfo>, String>;

/// A lookup that reads nothing.
pub fn no_lookup(_: &str) -> Result<Option<LibraryInfo>, String> {
    Ok(None)
}

/// Whether a library has a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Yes,
    /// It does not; the names it has, when they may be shown.
    No(Vec<String>),
    /// The library could not be read.
    Unknown,
}

/// Whether the library a line names has a section; the policy's reason when
/// it refuses the file.
pub fn find_section(lookup: SectionLookup, file: &str, section: &str) -> Result<Found, String> {
    Ok(match lookup(file)? {
        None => Found::Unknown,
        Some(info)
            if info
                .sections
                .iter()
                .any(|s| s.eq_ignore_ascii_case(section)) =>
        {
            Found::Yes
        }
        Some(info) if info.listable => Found::No(info.sections),
        Some(_) => Found::No(Vec::new()),
    })
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
        if text.len() > MAX_PROCESS_TEXT {
            return Err(CornerError::BadProcess(format!(
                "{}...",
                text.chars().take(40).collect::<String>()
            )));
        }
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
    /// Its values, as numbers or SPICE text (`"1.8"`), at most 64.
    #[serde(deserialize_with = "numbers")]
    #[schemars(length(max = 64))]
    pub values: Vec<f64>,
}

/// A number as JSON or SPICE text.
struct Number(f64);

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        lenient::num(d).map(Number)
    }
}

/// A list read element by element that stops with an error at its limit,
/// so an oversized request fails before it is held in full.
struct Capped<T> {
    limit: usize,
    what: &'static str,
    _t: PhantomData<T>,
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for Capped<T> {
    type Value = Vec<T>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "a list of at most {} {}", self.limit, self.what)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element::<T>()? {
            if out.len() == self.limit {
                return Err(A::Error::custom(format!(
                    "at most {} {} in a corner set",
                    self.limit, self.what
                )));
            }
            out.push(v);
        }
        Ok(out)
    }
}

fn capped<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
    limit: usize,
    what: &'static str,
) -> Result<Vec<T>, D::Error> {
    d.deserialize_seq(Capped {
        limit,
        what,
        _t: PhantomData,
    })
}

fn numbers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<f64>, D::Error> {
    let v: Vec<Number> = capped(d, MAX_LIST, "values")?;
    Ok(v.into_iter().map(|n| n.0).collect())
}

fn processes<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    capped(d, MAX_LIST, "process corners")
}

fn supplies<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Supply>, D::Error> {
    capped(d, MAX_SUPPLIES, "supplies")
}

/// Process corners times temperatures times supply values. Every part is
/// optional; an empty part leaves the netlist as it is in that respect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CornerSet {
    /// Process corners: presets `tt`, `ss`, `ff`, `sf`, `fs`, or
    /// family=corner pairs such as `mos=ss res=wcs` (families mos, res, cap,
    /// dio, hbt). Needs `.lib <file> <family>_<corner>` lines in the circuit.
    #[serde(default, deserialize_with = "processes")]
    #[schemars(length(max = 64))]
    pub process: Vec<String>,
    /// Temperatures in degrees Celsius.
    #[serde(default, deserialize_with = "numbers")]
    #[schemars(length(max = 64))]
    pub temperatures: Vec<f64>,
    /// Supply values, for example `[{"target": "VDD", "values": [1.62,
    /// 1.8, 1.98]}]`. At most 200 corners in all.
    #[serde(default, deserialize_with = "supplies")]
    #[schemars(length(max = 8))]
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

    /// How many corners the set has, checked before any is built: every
    /// list within its limit, every value sensible, at most
    /// [`MAX_CORNERS`] in all.
    pub fn count(&self) -> Result<usize, CornerError> {
        if self.is_empty() {
            return Err(CornerError::Empty);
        }
        let long = |what: &'static str, count: usize, limit: usize| {
            (count > limit).then_some(CornerError::ListTooLong { what, count, limit })
        };
        if let Some(e) = long("process corners", self.process.len(), MAX_LIST)
            .or_else(|| long("temperatures", self.temperatures.len(), MAX_LIST))
            .or_else(|| long("supplies", self.supplies.len(), MAX_SUPPLIES))
            .or_else(|| {
                self.supplies
                    .iter()
                    .find_map(|s| long("values of one supply", s.values.len(), MAX_LIST))
            })
        {
            return Err(e);
        }
        if let Some(t) = self
            .temperatures
            .iter()
            .find(|t| !t.is_finite() || **t < -273.15)
        {
            return Err(CornerError::BadTemperature(*t));
        }
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
        let count = self
            .supplies
            .iter()
            .map(|s| s.values.len())
            .chain([self.process.len().max(1), self.temperatures.len().max(1)])
            .try_fold(1usize, |acc, n| acc.checked_mul(n))
            .unwrap_or(usize::MAX);
        if count > MAX_CORNERS {
            return Err(CornerError::TooMany {
                count,
                limit: MAX_CORNERS,
            });
        }
        Ok(count)
    }

    /// Every corner, process first and the last supply varying fastest.
    pub fn corners(&self) -> Result<Vec<Corner>, CornerError> {
        let count = self.count()?;
        let processes: Vec<Option<Process>> = if self.process.is_empty() {
            vec![None]
        } else {
            self.process
                .iter()
                .map(|p| Process::parse(p).map(Some))
                .collect::<Result<_, _>>()?
        };
        let temps: Vec<Option<f64>> = if self.temperatures.is_empty() {
            vec![None]
        } else {
            self.temperatures.iter().copied().map(Some).collect()
        };
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
        match find_section(lookup, &u.file, &next.name()) {
            Err(reason) => {
                return Err(CornerError::Refused {
                    file: u.file.clone(),
                    reason,
                });
            }
            Ok(Found::No(available)) => {
                return Err(CornerError::MissingSection {
                    file: u.file.clone(),
                    section: next.name(),
                    available,
                });
            }
            Ok(Found::Yes | Found::Unknown) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::test_support::*;
    use crate::spec::parse_specs;
    use aispice_core::netlist::{parse, write};

    const DECK: &str = "* corners\n.lib /pdk/corner.lib mos_tt\n.LIB \"/pdk/res-lib.lib\" RES_TYP_MISMATCH\n.lib /pdk/dio.lib dio_tt\n.lib opamp.sub\nVDD vdd 0 1.8\nR1 vdd out {rload}\n.param rload=1k vref=0.9\n.options reltol=1m temp=50\n.temp 85\n.ac dec 10 1 1Meg\n.end\n";

    fn sections(file: &str) -> Result<Option<LibraryInfo>, String> {
        let list: &[&str] = match file {
            "/pdk/corner.lib" => &["mos_tt", "mos_ss", "mos_ff", "mos_sf", "mos_fs"],
            "/pdk/res-lib.lib" => &[
                "res_typ",
                "res_wcs",
                "res_bcs",
                "res_typ_mismatch",
                "res_wcs_mismatch",
                "res_bcs_mismatch",
            ],
            "/etc/passwd" => return Err("`/etc/passwd` is outside the project".into()),
            _ => return Ok(None),
        };
        Ok(Some(LibraryInfo {
            sections: list.iter().map(|s| s.to_string()).collect(),
            listable: file != "/pdk/corner.lib",
        }))
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
    }

    /// Oversized requests fail fast and say why: lists are cut off while
    /// they are read, and the corner count is known before any corner is
    /// built.
    #[test]
    fn oversized_corner_sets_fail_fast() {
        let temps: Vec<i64> = (0..100_000).collect();
        let err = serde_json::from_value::<CornerSet>(serde_json::json!({"temperatures": temps}))
            .unwrap_err();
        assert!(err.to_string().contains("at most 64 values"), "{err}");
        let many: Vec<serde_json::Value> = (0..20)
            .map(|i| serde_json::json!({"target": format!("V{i}"), "values": [1, 2]}))
            .collect();
        let err =
            serde_json::from_value::<CornerSet>(serde_json::json!({"supplies": many})).unwrap_err();
        assert!(err.to_string().contains("at most 8 supplies"), "{err}");
        // 64^10 corners: refused from the counts alone.
        let huge = CornerSet {
            process: vec!["tt".into(); 64],
            temperatures: vec![27.0; 64],
            supplies: (0..8)
                .map(|i| Supply {
                    target: format!("V{i}"),
                    values: vec![1.0; 64],
                })
                .collect(),
        };
        let started = std::time::Instant::now();
        assert_eq!(
            huge.corners(),
            Err(CornerError::TooMany {
                count: 1 << 60,
                limit: MAX_CORNERS
            })
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        let long = CornerSet {
            temperatures: vec![27.0; 65],
            ..Default::default()
        };
        assert!(matches!(long.count(), Err(CornerError::ListTooLong { .. })));
        assert!(matches!(
            Process::parse(&"mos=ss ".repeat(100)),
            Err(CornerError::BadProcess(_))
        ));
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
            process: vec!["tt".into(); 8],
            temperatures: (0..30).map(f64::from).collect(),
            ..Default::default()
        };
        assert_eq!(
            many.corners().unwrap_err().to_string(),
            "240 corners is more than the limit of 200; use fewer values"
        );
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
            out.contains(".LIB \"/pdk/res-lib.lib\" RES_WCS_MISMATCH\n"),
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
        // A section the library does not define is named before any run;
        // a library outside the project keeps its other sections to itself.
        let err = apply(&n, &at("mos=xx"), &sections).unwrap_err();
        assert_eq!(err.to_string(), "/pdk/corner.lib has no section `mos_xx`");
        let err = apply(&n, &at("res=xx"), &sections).unwrap_err();
        assert!(
            err.to_string().ends_with("it has res_typ, res_wcs, res_bcs, res_typ_mismatch, res_wcs_mismatch, res_bcs_mismatch"),
            "{err}"
        );
        // A library the policy refuses stops the corner, with its reason.
        let evil = parse("* x\n.lib /etc/passwd mos_tt\n.op\n.end\n");
        let err = apply(&evil, &at("ss"), &sections).unwrap_err();
        assert_eq!(
            err,
            CornerError::Refused {
                file: "/etc/passwd".into(),
                reason: "`/etc/passwd` is outside the project".into()
            }
        );
        // The policy's reading of `.lib` lines: keyword case, quotes and
        // continuation lines.
        let odd = parse("* x\n.LiB \"/pdk/corner.lib\"\n+ MOS_TT\n.op\n.end\n");
        let out = write(&apply(&odd, &at("ff"), &sections).unwrap());
        assert!(out.contains(".LiB \"/pdk/corner.lib\" MOS_FF\n"), "{out}");
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
}
