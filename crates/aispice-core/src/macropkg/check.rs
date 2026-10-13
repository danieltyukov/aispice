//! The macro/v1 validator: every rule of `docs/macro-package.md`, in the
//! documented order. ChipLeap runs the same rules (`chipleap.macro.check`),
//! with the same rule ids and messages; `tests/fixtures/macro/expected.json`
//! pins both.

use super::files::{self, Kind, read_package_file, sha256_package_file};
use super::gds::{GdsError, GdsLibrary, name_of, parse_gds_with, structure_digests};
use super::lef::{LefError, LefMacro, parse_lef_with};
use super::liberty::{LibertyError, parse_liberty_with};
use super::limits::{Limits, ReadError, json_depth, toml_depth};
use super::manifest::{
    Direction, MANIFEST, MacroError, MacroKind, Manifest, OPENADA_SCHEMA, Pin, PinKind,
    SCHEMA_VERSION,
};
use super::pdklibs::PdkLibs;
use super::schema::{Val, validate_def};
use super::spice::{spice_subckt_with, spice_subckts_with};
use super::text::{fmt_g, names, plural, quote};
use super::toml10::toml11_syntax;
use super::verilog::{VerilogError, verilog_ports_with};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

pub const REPORT_SCHEMA: &str = "macro.check/v1";
const REQUIRED_EVIDENCE: [&str; 2] = ["drc", "lvs"];
/// Half a database unit of 1 nm: sizes printed to three decimals still
/// compare equal.
const TOLERANCE_UM: f64 = 0.0005;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub rule: String,
    pub severity: Severity,
    /// Relative to the package, `.` for the package itself.
    pub file: String,
    pub message: String,
}

impl Finding {
    pub fn to_text(&self) -> String {
        let place = if self.file == "." {
            String::new()
        } else {
            format!("{}: ", self.file)
        };
        format!(
            "{:<7} {} {place}{}",
            self.severity.as_str(),
            self.rule,
            self.message
        )
    }
}

/// The result of [`check_package`]; serializes to the `macro.check/v1` JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub package: String,
    pub name: Option<String>,
    pub kind: Option<MacroKind>,
    #[serde(rename = "pass")]
    pub passed: bool,
    pub errors: usize,
    pub warnings: usize,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn rules(&self) -> Vec<&str> {
        self.findings.iter().map(|f| f.rule.as_str()).collect()
    }

    pub fn to_text(&self) -> String {
        let mut lines: Vec<String> = self.findings.iter().map(Finding::to_text).collect();
        lines.push(format!(
            "{}: {}, {}, {}",
            self.package,
            if self.passed { "pass" } else { "fail" },
            plural(self.errors, "error", None),
            plural(self.warnings, "warning", None)
        ));
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// The placing chip's PDK: `NAME` or `NAME@COMMIT`.
    pub pdk: Option<String>,
    /// The cells of the PDK's libraries ([`PdkLibs::load`]), when the caller
    /// has them: a package cell named like one is refused unless it is a GDS
    /// cell identical to the PDK's of that name (`pdk.cell_name`). Names are
    /// compared in ASCII lower case.
    pub pdk_libs: Option<PdkLibs>,
    /// For tests; the spec fixes the values of [`Limits::DEFAULT`].
    pub limits: Limits,
}

/// The name spaces of a PDK's own cells, lower case (`pdk.cell_name`): a
/// chip merges layouts and netlists by cell name, so a macro's cell named in
/// one could replace a cell of the PDK.
pub fn pdk_cell_prefixes(pdk: &str) -> &'static [&'static str] {
    match pdk {
        "ihp-sg13g2" => &["sg13g2", "rm_ihpsg13"],
        _ => &[],
    }
}

fn pdk_name_ok(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && cs.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

fn commit_ok(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `ihp-sg13g2` or `ihp-sg13g2@22f2a25` as (name, commit).
pub fn parse_pdk_option(text: &str) -> Result<(String, Option<String>), MacroError> {
    let (name, commit) = match text.split_once('@') {
        Some((n, c)) => (n, Some(c)),
        None => (text, None),
    };
    if !pdk_name_ok(name) {
        return Err(MacroError::Invalid(format!(
            "--pdk {text:?}: give NAME or NAME@COMMIT, e.g. ihp-sg13g2@22f2a25"
        )));
    }
    match commit {
        Some(c) if !c.is_empty() && !commit_ok(c) => Err(MacroError::Invalid(format!(
            "--pdk {text:?}: the commit must be 7 to 40 lowercase hex digits"
        ))),
        Some(c) if !c.is_empty() => Ok((name.to_string(), Some(c.to_string()))),
        _ => Ok((name.to_string(), None)),
    }
}

fn commits_match(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Files the package must hold beside `macro.toml` and `evidence/`, in check
/// order.
pub fn required_files(m: &Manifest) -> Vec<String> {
    let mut files: Vec<String> = ["gds", "lef", "lib", "v", "spice"]
        .iter()
        .map(|e| m.file(e))
        .collect();
    if m.kind == MacroKind::Analog {
        files.push(m.file("specs.toml"));
    }
    for c in &m.corners {
        if let Some(l) = &c.liberty
            && !files.contains(l)
        {
            files.push(l.clone());
        }
    }
    files
}

/// The first line of a parser's message.
fn first_line(msg: &str) -> String {
    msg.lines().next().unwrap_or("").to_string()
}

/// A TOML parser error with its position, as Python's tomllib words it.
fn toml_error(text: &str, e: &toml::de::Error) -> String {
    match e.span() {
        Some(span) => {
            // The span is a byte offset; back off to a character boundary.
            let mut end = span.start.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let before = &text[..end];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            format!("{} (at line {line}, column {column})", e.message())
        }
        None => e.message().to_string(),
    }
}

fn parse_toml(text: &str) -> Result<toml::Value, String> {
    if let Some(e) = toml11_syntax(text) {
        return Err(e);
    }
    toml::from_str::<toml::Table>(text)
        .map(toml::Value::Table)
        .map_err(|e| toml_error(text, &e))
}

fn nesting(limit: usize, what: &str) -> String {
    format!("{what} nest more than {limit} deep")
}

fn ascii_lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Why the LEF could not be used, reported at the LEF's place in the order.
enum LefProblem {
    Read(ReadError),
    Parse(LefError),
}

struct Checker<'a> {
    root: &'a Path,
    limits: Limits,
    findings: Vec<Finding>,
    /// Findings past `Limits::findings` are counted, not listed; a
    /// `check.limit` warning at the end of the list says how many.
    unlisted_errors: usize,
    unlisted_warnings: usize,
    /// Files reported as too large, once each.
    too_large: HashSet<String>,
    /// Files reported as unreadable, once each.
    unreadable: HashSet<String>,
    /// The cells a chip would merge, seen while the views are read: (file,
    /// what it is, name, and for a GDS cell named exactly like a GDS cell of
    /// the PDK's, its digest).
    cells: Vec<(String, &'static str, String, String)>,
    /// From [`CheckOptions::pdk_libs`], names in lower case.
    pdk_libs: Option<PdkLibs>,
    /// Package files hashed for evidence.
    digests: HashMap<String, Option<String>>,
    hash_limit_reported: bool,
    /// From macro.toml, once it follows the schema.
    name: Option<String>,
    kind: Option<MacroKind>,
}

impl Checker<'_> {
    fn add(&mut self, rule: &str, file: &str, message: String) {
        self.push(rule, file, message, Severity::Error);
    }

    fn warn(&mut self, rule: &str, file: &str, message: String) {
        self.push(rule, file, message, Severity::Warning);
    }

    fn push(&mut self, rule: &str, file: &str, message: String, severity: Severity) {
        if self.findings.len() < self.limits.findings {
            self.findings.push(Finding {
                rule: rule.into(),
                severity,
                file: file.into(),
                message,
            });
        } else if severity == Severity::Error {
            self.unlisted_errors += 1;
        } else {
            self.unlisted_warnings += 1;
        }
    }

    /// Whether the package holds `rel` as anything but a directory: a
    /// symbolic link is there, and reading it is refused.
    fn present(&self, rel: &str) -> bool {
        !matches!(files::kind(self.root, rel), Kind::Missing | Kind::Dir)
    }

    /// A package file's bytes, or None after reporting why it cannot be read.
    fn read(&mut self, rel: &str, limit: u64) -> Option<Vec<u8>> {
        match read_package_file(self.root, rel, limit) {
            Ok(data) => Some(data),
            Err(e) => {
                self.unreadable(rel, e);
                None
            }
        }
    }

    fn unreadable(&mut self, rel: &str, e: ReadError) {
        match e {
            ReadError::TooLarge(m) => self.report_too_large(rel, m),
            ReadError::Io(e) => self.report_unreadable(
                rel,
                format!("cannot read the file: {}", first_line(&e.to_string())),
            ),
        }
    }

    fn report_unreadable(&mut self, rel: &str, message: String) {
        if self.unreadable.insert(rel.to_string()) {
            self.add("file.unreadable", rel, message);
        }
    }

    fn report_too_large(&mut self, rel: &str, message: String) {
        if self.too_large.insert(rel.to_string()) {
            self.add("file.too_large", rel, message);
        }
    }

    fn manifest(&mut self) -> Option<Manifest> {
        if !self.present(MANIFEST) {
            self.add("file.missing", MANIFEST, "required file is missing".into());
            return None;
        }
        let bytes = self.read(MANIFEST, self.limits.manifest_bytes)?;
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => {
                self.add(
                    "manifest.syntax",
                    MANIFEST,
                    format!("not valid TOML: {}", first_line(&e.to_string())),
                );
                return None;
            }
        };
        if toml_depth(&text) > self.limits.nesting {
            let message = nesting(self.limits.nesting, "tables and arrays");
            self.add("manifest.limit", MANIFEST, message);
            return None;
        }
        let value = match parse_toml(&text) {
            Ok(v) => v,
            Err(e) => {
                self.add(
                    "manifest.syntax",
                    MANIFEST,
                    format!("not valid TOML: {}", first_line(&e)),
                );
                return None;
            }
        };
        let data = Val::from(&value);
        if let Some(Val::Str(version)) = data.get("schema")
            && version.starts_with("macro/")
            && version != SCHEMA_VERSION
        {
            self.add(
                "manifest.version",
                MANIFEST,
                format!(
                    "schema is {}; this validator reads {SCHEMA_VERSION}",
                    quote(version)
                ),
            );
            return None;
        }
        let errors = validate_def("", &data);
        if !errors.is_empty() {
            for e in errors {
                self.add("manifest.schema", MANIFEST, e);
            }
            return None;
        }
        let m = match value.try_into::<Manifest>() {
            Ok(m) => m,
            Err(e) => {
                self.add("manifest.schema", MANIFEST, e.message().to_string());
                return None;
            }
        };
        self.name = Some(m.name.clone());
        self.kind = Some(m.kind);
        // Every view check walks every pin bit, so a huge bus would stall them all.
        let mut too_big = false;
        if m.pin_bits() > self.limits.pins as u64 {
            let message = format!("the pins have more than {} bits", self.limits.pins);
            self.add("manifest.limit", MANIFEST, message);
            too_big = true;
        }
        if m.corners.len() > self.limits.corners {
            let message = format!("more than {} corners", self.limits.corners);
            self.add("manifest.limit", MANIFEST, message);
            too_big = true;
        }
        (!too_big).then_some(m)
    }

    fn semantics(&mut self, m: &Manifest) {
        let mut seen = HashSet::new();
        for p in &m.pins {
            if !seen.insert(p.name.as_str()) {
                self.add(
                    "manifest.duplicate_pin",
                    MANIFEST,
                    format!("pin {} is declared twice", quote(&p.name)),
                );
            }
        }
        // Lookups by name: a long pin or domain list must not cost its square.
        let mut by_name: HashMap<&str, &Pin> = HashMap::new();
        for p in &m.pins {
            by_name.entry(p.name.as_str()).or_insert(p);
        }
        let mut power: HashSet<&str> = HashSet::new();
        let mut ground: HashSet<&str> = HashSet::new();
        let mut domains = HashSet::new();
        for d in &m.domains {
            if !domains.insert(d.name.as_str()) {
                self.add(
                    "manifest.domain",
                    MANIFEST,
                    format!("domain {} is declared twice", quote(&d.name)),
                );
            }
            power.extend(d.power.iter().map(String::as_str));
            ground.extend(d.ground.iter().map(String::as_str));
            for (role, pin_names) in [(PinKind::Power, &d.power), (PinKind::Ground, &d.ground)] {
                for n in pin_names {
                    let message = match by_name.get(n.as_str()) {
                        None => format!(
                            "domain {} lists {} as {}, but there is no such pin",
                            quote(&d.name),
                            quote(n),
                            role.as_str()
                        ),
                        Some(pin) if pin.kind != role => format!(
                            "domain {} lists {} as {}, but its kind is {}",
                            quote(&d.name),
                            quote(n),
                            role.as_str(),
                            pin.kind.as_str()
                        ),
                        Some(_) => continue,
                    };
                    self.add("manifest.domain", MANIFEST, message);
                }
            }
        }
        for p in &m.pins {
            if p.is_supply() {
                let listed = if p.kind == PinKind::Power {
                    &power
                } else {
                    &ground
                };
                if !listed.contains(p.name.as_str()) {
                    self.add(
                        "manifest.domain",
                        MANIFEST,
                        format!("{} pin {} is in no domain", p.kind.as_str(), quote(&p.name)),
                    );
                }
            } else {
                let domain = p.domain.as_deref().unwrap_or("");
                if !domains.contains(domain) {
                    self.add(
                        "manifest.domain",
                        MANIFEST,
                        format!(
                            "pin {} names domain {}, which is not declared",
                            quote(&p.name),
                            quote(domain)
                        ),
                    );
                }
            }
        }
        let mut corners = HashSet::new();
        for c in &m.corners {
            if !corners.insert(c.name.as_str()) {
                self.add(
                    "manifest.corner",
                    MANIFEST,
                    format!("corner {} is declared twice", quote(&c.name)),
                );
            }
            // BTreeMap keys are sorted, as Python's sorted() sorts them.
            for k in c.voltages.keys() {
                if !domains.contains(k.as_str()) {
                    self.add(
                        "manifest.corner",
                        MANIFEST,
                        format!(
                            "corner {} sets the voltage of domain {}, which is not declared",
                            quote(&c.name),
                            quote(k)
                        ),
                    );
                }
            }
        }
    }

    fn dirname(&mut self, m: &Manifest) {
        let actual = self
            .root
            .canonicalize()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let want = format!("{}.macro", m.name);
        if actual != want {
            self.warn(
                "package.dirname",
                ".",
                format!(
                    "the directory is {}; name it {}",
                    quote(&actual),
                    quote(&want)
                ),
            );
        }
    }

    fn required(&mut self, m: &Manifest) {
        for f in required_files(m) {
            if !self.present(&f) {
                self.add("file.missing", &f, "required file is missing".into());
            }
        }
    }

    fn gds_error(&mut self, f: &str, e: GdsError) {
        match e {
            GdsError::Syntax(m) => {
                self.add(
                    "gds.parse",
                    f,
                    format!("cannot read the GDS: {}", first_line(&m)),
                );
            }
            GdsError::Cycle(m) => self.add("gds.cycle", f, m),
            GdsError::Limit(m) => self.add("gds.limit", f, m.0),
        }
    }

    fn gds(&mut self, m: &Manifest, lef: Option<&LefMacro>) {
        let f = m.file("gds");
        if !self.present(&f) {
            return;
        }
        let Some(data) = self.read(&f, self.limits.gds_bytes) else {
            return;
        };
        let lib = match parse_gds_with(&data, &self.limits) {
            Ok(lib) => lib,
            Err(e) => {
                self.gds_error(&f, e);
                return;
            }
        };
        // The digests of the cells named like the PDK's GDS cells, to compare
        // with theirs.
        let named: HashSet<&str> = match &self.pdk_libs {
            Some(libs) => lib
                .cells()
                .iter()
                .filter(|c| **c != m.name && libs.gds.contains_key(*c))
                .map(String::as_str)
                .collect(),
            None => HashSet::new(),
        };
        let mut digests: HashMap<String, String> = HashMap::new();
        if !named.is_empty() {
            for (raw, digest) in structure_digests(&data[..]).unwrap_or_default() {
                let name = name_of(&raw);
                if named.contains(name.as_str()) {
                    digests.insert(name, digest);
                }
            }
        }
        for c in lib.cells().iter().filter(|c| **c != m.name) {
            let digest = digests.remove(c).unwrap_or_default();
            self.cells.push((f.clone(), "GDS cell", c.clone(), digest));
        }
        let tops = lib.top_cells();
        if !tops.contains(&m.name) {
            let message = if lib.has_cell(&m.name) {
                format!("cell {} is placed inside another cell", quote(&m.name))
            } else {
                format!(
                    "no top cell is named {}; top cells: {}",
                    quote(&m.name),
                    names(&tops)
                )
            };
            self.add("gds.top_cell", &f, message);
            return;
        }
        for t in tops.iter().filter(|t| **t != m.name) {
            self.warn(
                "gds.extra_top_cell",
                &f,
                format!(
                    "top cell {} would be merged into the chip with the macro",
                    quote(t)
                ),
            );
        }
        self.gds_bbox(m, &lib, &f, lef);
        let labels: HashSet<String> = lib.labels(&m.name).into_iter().collect();
        for p in &m.pins {
            for bit in p.bits() {
                if !labels.contains(&bit) {
                    self.warn(
                        "gds.label",
                        &f,
                        format!("pin {} has no text label in the top cell", quote(&bit)),
                    );
                }
            }
        }
    }

    fn gds_bbox(&mut self, m: &Manifest, lib: &GdsLibrary, f: &str, lef: Option<&LefMacro>) {
        let bx = match lib.bbox(&m.name) {
            Ok(Some(b)) => b,
            Ok(None) => {
                self.add("gds.bbox", f, "the top cell has no geometry".into());
                return;
            }
            Err(e) => {
                self.gds_error(f, e);
                return;
            }
        };
        let (ox, oy) = lef.map_or((0.0, 0.0), |l| l.origin);
        let (bx0, by0) = (-ox, -oy);
        let (bx1, by1) = (bx0 + m.size.width_um, by0 + m.size.height_um);
        // Adding zero turns -0 into 0, as in ChipLeap.
        let (x0, y0, x1, y1) = (bx.0 + 0.0, bx.1 + 0.0, bx.2 + 0.0, bx.3 + 0.0);
        let t = TOLERANCE_UM;
        if x0 < bx0 - t || y0 < by0 - t || x1 > bx1 + t || y1 > by1 + t {
            self.add(
                "gds.bbox",
                f,
                format!(
                    "the top cell spans ({}, {}) to ({}, {}) um, outside the {} x {} um boundary",
                    fmt_g(x0),
                    fmt_g(y0),
                    fmt_g(x1),
                    fmt_g(y1),
                    fmt_g(m.size.width_um),
                    fmt_g(m.size.height_um)
                ),
            );
        }
    }

    /// (macro, problem, macro names), reporting nothing: the GDS check needs
    /// the LEF origin, and the LEF's own findings come after the GDS's.
    fn read_lef(&self, m: &Manifest) -> (Option<LefMacro>, Option<LefProblem>, Vec<String>) {
        let f = m.file("lef");
        if !self.present(&f) {
            return (None, None, Vec::new());
        }
        let data = match read_package_file(self.root, &f, self.limits.view_bytes) {
            Ok(d) => d,
            Err(e) => return (None, Some(LefProblem::Read(e)), Vec::new()),
        };
        match parse_lef_with(&String::from_utf8_lossy(&data), &self.limits) {
            Ok(mut macros) => {
                let names = macros.keys().cloned().collect();
                (macros.remove(&m.name), None, names)
            }
            Err(e) => (None, Some(LefProblem::Parse(e)), Vec::new()),
        }
    }

    fn lef(
        &mut self,
        m: &Manifest,
        lef: Option<&LefMacro>,
        problem: Option<LefProblem>,
        macro_names: &[String],
    ) {
        let f = m.file("lef");
        if !self.present(&f) {
            return;
        }
        match problem {
            Some(LefProblem::Read(e)) => return self.unreadable(&f, e),
            Some(LefProblem::Parse(LefError::Limit(e))) => return self.add("lef.limit", &f, e.0),
            Some(LefProblem::Parse(LefError::Syntax(e))) => {
                let message = format!("cannot read the LEF: {}", first_line(&e));
                return self.add("lef.parse", &f, message);
            }
            None => {}
        }
        let Some(lef) = lef else {
            self.add(
                "lef.macro",
                &f,
                format!(
                    "no MACRO {}; macros in the file: {}",
                    quote(&m.name),
                    names(macro_names)
                ),
            );
            return;
        };
        if lef.class.split(' ').next() != Some("BLOCK") {
            let what = if lef.class.is_empty() {
                "the macro has no CLASS".to_string()
            } else {
                format!("CLASS is {}", quote(&lef.class))
            };
            self.warn("lef.class", &f, format!("{what}, expected BLOCK"));
        }
        match lef.size {
            None => self.add("lef.size", &f, "the macro has no SIZE".into()),
            Some((w, h))
                if (w - m.size.width_um).abs() > TOLERANCE_UM
                    || (h - m.size.height_um).abs() > TOLERANCE_UM =>
            {
                self.add(
                    "lef.size",
                    &f,
                    format!(
                        "SIZE is {} x {} um, macro.toml says {} x {} um",
                        fmt_g(w),
                        fmt_g(h),
                        fmt_g(m.size.width_um),
                        fmt_g(m.size.height_um)
                    ),
                );
            }
            Some(_) => {}
        }
        let mut bits = HashSet::new();
        for p in &m.pins {
            for bit in p.bits() {
                bits.insert(bit.clone());
                let Some(lp) = lef.pins.get(&bit) else {
                    self.add(
                        "lef.missing_pin",
                        &f,
                        format!("pin {} from macro.toml is missing", quote(&bit)),
                    );
                    continue;
                };
                let use_ = if lp.use_.is_empty() {
                    "SIGNAL"
                } else {
                    lp.use_.as_str()
                };
                let kind = p.kind.as_str().to_ascii_uppercase();
                if p.is_supply() {
                    if use_ != kind {
                        self.add(
                            "lef.use",
                            &f,
                            format!("pin {} is USE {use_}, expected USE {kind}", quote(&bit)),
                        );
                    }
                } else {
                    let want = p.direction.as_str().to_ascii_uppercase();
                    if lp.direction != want {
                        let got = if lp.direction.is_empty() {
                            "no DIRECTION".to_string()
                        } else {
                            format!("DIRECTION {}", lp.direction)
                        };
                        self.add(
                            "lef.direction",
                            &f,
                            format!(
                                "pin {} has {got}, macro.toml says {}",
                                quote(&bit),
                                p.direction.as_str()
                            ),
                        );
                    }
                    if use_ == "POWER" || use_ == "GROUND" {
                        self.add(
                            "lef.use",
                            &f,
                            format!(
                                "pin {} is USE {use_}, but its kind is {}",
                                quote(&bit),
                                p.kind.as_str()
                            ),
                        );
                    } else if use_ != kind {
                        self.warn(
                            "lef.use",
                            &f,
                            format!("pin {} is USE {use_}, expected USE {kind}", quote(&bit)),
                        );
                    }
                }
                if lp.shapes == 0 {
                    self.add(
                        "lef.port",
                        &f,
                        format!("pin {} has no PORT shapes", quote(&bit)),
                    );
                }
                if let Some(layer) = &p.layer
                    && !lp.layers.contains(layer)
                {
                    self.add(
                        "lef.pin_layer",
                        &f,
                        format!("pin {} has no shape on {}", quote(&bit), quote(layer)),
                    );
                }
            }
        }
        // BTreeMap keys are sorted.
        for name in lef.pins.keys().filter(|n| !bits.contains(*n)) {
            self.add(
                "lef.extra_pin",
                &f,
                format!("pin {} is not in macro.toml", quote(name)),
            );
        }
    }

    fn liberty(&mut self, m: &Manifest, f: &str) {
        if !self.present(f) {
            return;
        }
        let Some(data) = self.read(f, self.limits.liberty_bytes) else {
            return;
        };
        let text = String::from_utf8_lossy(&data);
        let cells = match parse_liberty_with(&text, &self.limits, Some(&m.name)) {
            Ok(c) => c,
            Err(LibertyError::Limit(e)) => {
                self.add("lib.limit", f, e.0);
                return;
            }
            Err(LibertyError::Syntax(e)) => {
                self.add(
                    "lib.parse",
                    f,
                    format!("cannot read the liberty file: {}", first_line(&e)),
                );
                return;
            }
        };
        let Some(pins) = cells.get(&m.name) else {
            self.add("lib.cell", f, format!("no cell {}", quote(&m.name)));
            return;
        };
        let mut bits = HashSet::new();
        for p in &m.pins {
            for bit in p.bits() {
                bits.insert(bit.clone());
                let Some(lp) = pins.get(&bit) else {
                    self.add(
                        "lib.missing_pin",
                        f,
                        format!("pin {} from macro.toml is missing", quote(&bit)),
                    );
                    continue;
                };
                if p.is_supply() {
                    let kind = p.kind.as_str();
                    if !lp.pg {
                        self.add(
                            "lib.pg_pin",
                            f,
                            format!("pin {} must be a pg_pin", quote(&bit)),
                        );
                    } else if !lp.pg_type.ends_with(&format!("_{kind}")) {
                        let got = if lp.pg_type.is_empty() {
                            "no pg_type".to_string()
                        } else {
                            format!("pg_type {}", quote(&lp.pg_type))
                        };
                        self.add(
                            "lib.pg_pin",
                            f,
                            format!("pg_pin {} has {got}, expected a *_{kind} type", quote(&bit)),
                        );
                    }
                } else if lp.pg {
                    self.add(
                        "lib.pg_pin",
                        f,
                        format!(
                            "pin {} is a pg_pin, but its kind is {}",
                            quote(&bit),
                            p.kind.as_str()
                        ),
                    );
                } else {
                    if lp.direction != p.direction.as_str() {
                        let got = if lp.direction.is_empty() {
                            "no direction".to_string()
                        } else {
                            format!("direction {}", quote(&lp.direction))
                        };
                        self.add(
                            "lib.direction",
                            f,
                            format!(
                                "pin {} has {got}, macro.toml says {}",
                                quote(&bit),
                                p.direction.as_str()
                            ),
                        );
                    }
                    if m.kind == MacroKind::Digital
                        && p.direction == Direction::Output
                        && matches!(p.kind, PinKind::Signal | PinKind::Clock)
                        && lp.timing == 0
                    {
                        self.add(
                            "lib.timing",
                            f,
                            format!("output pin {} has no timing arc", quote(&bit)),
                        );
                    }
                }
            }
        }
        for name in pins.keys().filter(|n| !bits.contains(*n)) {
            self.add(
                "lib.extra_pin",
                f,
                format!("pin {} is not in macro.toml", quote(name)),
            );
        }
    }

    fn verilog(&mut self, m: &Manifest) {
        let f = m.file("v");
        if !self.present(&f) {
            return;
        }
        let Some(data) = self.read(&f, self.limits.view_bytes) else {
            return;
        };
        let ports = match verilog_ports_with(&String::from_utf8_lossy(&data), &m.name, &self.limits)
        {
            Ok(Some(ports)) => ports,
            Ok(None) => {
                self.add(
                    "verilog.module",
                    &f,
                    format!("no module {}", quote(&m.name)),
                );
                return;
            }
            Err(VerilogError::Limit(e)) => {
                self.add("verilog.limit", &f, e.0);
                return;
            }
            Err(VerilogError::Duplicate(message)) => {
                self.add("verilog.duplicate", &f, message);
                return;
            }
        };
        // A port declared twice (on both sides of an ifdef, say): a tool reads
        // one of the declarations, and which one depends on its defines.
        let mut count: HashMap<&str, usize> = HashMap::new();
        for p in &ports {
            *count.entry(p.name.as_str()).or_default() += p.declared.max(1);
        }
        let twice: BTreeSet<&str> = count
            .into_iter()
            .filter(|&(_, k)| k > 1)
            .map(|(name, _)| name)
            .collect();
        for name in &twice {
            self.add(
                "verilog.duplicate",
                &f,
                format!("port {} is declared more than once", quote(name)),
            );
        }
        if !twice.is_empty() {
            return;
        }
        // A later port of the same name wins, as in a Python dict.
        let by_name: HashMap<&str, _> = ports.iter().map(|p| (p.name.as_str(), p)).collect();
        for p in &m.pins {
            let Some(vp) = by_name.get(p.name.as_str()) else {
                if !p.is_supply() {
                    self.add(
                        "verilog.missing_pin",
                        &f,
                        format!("port {} from macro.toml is missing", quote(&p.name)),
                    );
                }
                continue;
            };
            if vp.direction.is_empty() {
                self.add(
                    "verilog.direction",
                    &f,
                    format!(
                        "port {} is never declared input, output or inout",
                        quote(&p.name)
                    ),
                );
            } else if p.is_supply() {
                if vp.direction != "inout" && vp.direction != "input" {
                    self.add(
                        "verilog.direction",
                        &f,
                        format!(
                            "{} port {} is {}, expected inout or input",
                            p.kind.as_str(),
                            quote(&p.name),
                            vp.direction
                        ),
                    );
                }
            } else if vp.direction != p.direction.as_str() {
                self.add(
                    "verilog.direction",
                    &f,
                    format!(
                        "port {} is {}, macro.toml says {}",
                        quote(&p.name),
                        vp.direction,
                        p.direction.as_str()
                    ),
                );
            }
            if p.is_supply() || vp.width == Some(0) {
                continue;
            }
            let message = match (p.width.map(u64::from), vp.width) {
                (None, Some(w)) => format!(
                    "port {} is a bus of width {w}, macro.toml says a scalar",
                    quote(&p.name)
                ),
                (Some(w), None) => format!(
                    "port {} is a scalar, macro.toml says a bus of width {w}",
                    quote(&p.name)
                ),
                (Some(w), Some(got)) if w != got => format!(
                    "port {} has width {got}, macro.toml says {w}",
                    quote(&p.name)
                ),
                _ => continue,
            };
            self.add("verilog.width", &f, message);
        }
        let names: HashSet<&str> = m.pins.iter().map(|p| p.name.as_str()).collect();
        let extra: BTreeSet<&str> = by_name
            .keys()
            .copied()
            .filter(|n| !names.contains(n))
            .collect();
        for name in extra {
            self.add(
                "verilog.extra_pin",
                &f,
                format!("port {} is not in macro.toml", quote(name)),
            );
        }
    }

    fn spice(&mut self, m: &Manifest) {
        let f = m.file("spice");
        if !self.present(&f) {
            return;
        }
        let Some(data) = self.read(&f, self.limits.view_bytes) else {
            return;
        };
        let text = String::from_utf8_lossy(&data);
        let found = spice_subckt_with(&text, &m.name, &self.limits)
            .and_then(|sub| Ok((sub, spice_subckts_with(&text, &self.limits)?)));
        let sub = match found {
            Ok((sub, all)) => {
                for c in all.into_iter().filter(|c| !c.eq_ignore_ascii_case(&m.name)) {
                    self.cells.push((f.clone(), "subcircuit", c, String::new()));
                }
                sub
            }
            Err(e) => {
                self.add("spice.limit", &f, e.0);
                return;
            }
        };
        let Some(sub) = sub else {
            self.add("spice.subckt", &f, format!("no .subckt {}", quote(&m.name)));
            return;
        };
        let ports: HashSet<String> = sub.ports.iter().map(|p| ascii_lower(p)).collect();
        let mut bits = HashSet::new();
        for p in &m.pins {
            for bit in p.bits() {
                let low = ascii_lower(&bit);
                bits.insert(low.clone());
                if !ports.contains(&low) {
                    self.add(
                        "spice.missing_pin",
                        &f,
                        format!("port {} from macro.toml is missing", quote(&bit)),
                    );
                    continue;
                }
                let want = match p.direction {
                    Direction::Input => "I",
                    Direction::Output => "O",
                    Direction::Inout => "B",
                };
                if let Some(got) = sub.pininfo.get(&low)
                    && !p.is_supply()
                    && got != want
                {
                    self.add(
                        "spice.direction",
                        &f,
                        format!(
                            "*.PININFO gives {} direction {got}, macro.toml says {}",
                            quote(&bit),
                            p.direction.as_str()
                        ),
                    );
                }
            }
        }
        let extra: BTreeSet<&str> = sub
            .ports
            .iter()
            .filter(|p| !bits.contains(&ascii_lower(p)))
            .map(String::as_str)
            .collect();
        for name in extra {
            self.add(
                "spice.extra_pin",
                &f,
                format!("port {} is not in macro.toml", quote(name)),
            );
        }
    }

    fn specs(&mut self, m: &Manifest) {
        let f = m.file("specs.toml");
        if !self.present(&f) {
            return;
        }
        let Some(bytes) = self.read(&f, self.limits.specs_bytes) else {
            return;
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => {
                self.add(
                    "specs.parse",
                    &f,
                    format!("not valid TOML: {}", first_line(&e.to_string())),
                );
                return;
            }
        };
        if toml_depth(&text) > self.limits.nesting {
            let message = nesting(self.limits.nesting, "tables and arrays");
            self.add("specs.limit", &f, message);
            return;
        }
        let data = match parse_toml(&text) {
            Ok(v) => Val::from(&v),
            Err(e) => {
                self.add(
                    "specs.parse",
                    &f,
                    format!("not valid TOML: {}", first_line(&e)),
                );
                return;
            }
        };
        let errors = validate_def("specs_file", &data);
        if !errors.is_empty() {
            for e in errors {
                self.add("specs.schema", &f, e);
            }
            return;
        }
        let Some(Val::Arr(specs)) = data.get("specs") else {
            return;
        };
        let corners: Vec<&str> = m.corners.iter().map(|c| c.name.as_str()).collect();
        let name_of = |s: &Val| {
            s.get("name")
                .and_then(Val::as_str)
                .unwrap_or("")
                .to_string()
        };
        let mut seen = HashSet::new();
        for s in specs {
            let name = name_of(s);
            if !seen.insert(name.clone()) {
                self.add(
                    "specs.duplicate",
                    &f,
                    format!("spec {} is declared twice", quote(&name)),
                );
            }
        }
        for s in specs {
            let name = name_of(s);
            let empty = BTreeMap::new();
            let measured = match s.get("measured") {
                Some(Val::Obj(o)) => o,
                _ => &empty,
            };
            for c in &corners {
                if !measured.contains_key(*c) {
                    self.warn(
                        "specs.corner",
                        &f,
                        format!("spec {} has no value at corner {}", quote(&name), quote(c)),
                    );
                }
            }
            let unknown: Vec<&String> = measured
                .keys()
                .filter(|k| !corners.contains(&k.as_str()))
                .collect();
            for k in &unknown {
                self.add(
                    "specs.corner",
                    &f,
                    format!(
                        "spec {} has a value at corner {}, which macro.toml does not declare",
                        quote(&name),
                        quote(k)
                    ),
                );
            }
            let unit = match s.get("unit").and_then(Val::as_str) {
                Some(u) if !u.is_empty() => format!(" {u}"),
                _ => String::new(),
            };
            let min = s.get("min").and_then(Val::as_f64);
            let max = s.get("max").and_then(Val::as_f64);
            let order = corners
                .iter()
                .copied()
                .filter(|c| measured.contains_key(*c))
                .chain(unknown.iter().map(|k| k.as_str()));
            for k in order {
                let Some(v) = measured.get(k).and_then(Val::as_f64) else {
                    continue;
                };
                if let Some(lo) = min
                    && v < lo
                {
                    self.add(
                        "specs.fail",
                        &f,
                        format!(
                            "spec {} at corner {} is {}{unit}, below the minimum {}{unit}",
                            quote(&name),
                            quote(k),
                            fmt_g(v),
                            fmt_g(lo)
                        ),
                    );
                }
                if let Some(hi) = max
                    && v > hi
                {
                    self.add(
                        "specs.fail",
                        &f,
                        format!(
                            "spec {} at corner {} is {}{unit}, above the maximum {}{unit}",
                            quote(&name),
                            quote(k),
                            fmt_g(v),
                            fmt_g(hi)
                        ),
                    );
                }
            }
        }
    }

    /// Names of the summaries in evidence/, sorted; None when there are too
    /// many. Names that are not UTF-8 are skipped. Any `*.json` but a
    /// directory is a summary: one that is a symbolic link is listed, and
    /// reported when it is read. `evidence` itself must be a directory, not a
    /// link to one.
    fn summaries(&mut self) -> Option<Vec<String>> {
        match files::kind(self.root, "evidence") {
            Kind::Dir => {}
            Kind::Link => {
                let message = "cannot read the folder: it is a symbolic link".into();
                self.report_unreadable("evidence", message);
                return Some(Vec::new());
            }
            Kind::Other => {
                let message = "cannot read the folder: it is not a directory".into();
                self.report_unreadable("evidence", message);
                return Some(Vec::new());
            }
            Kind::Missing | Kind::File => return Some(Vec::new()),
        }
        let Ok(entries) = std::fs::read_dir(self.root.join("evidence")) else {
            return Some(Vec::new());
        };
        let mut found = Vec::new();
        for entry in entries.filter_map(Result::ok) {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // The entry's own type: a link to a directory is listed.
            if !name.ends_with(".json") || entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            found.push(name);
            if found.len() > self.limits.evidence_files {
                return None;
            }
        }
        found.sort();
        Some(found)
    }

    /// The SHA-256 of package file `base`, hashed once per check. A file
    /// already found too large or unreadable for its own check is not read
    /// here either.
    fn digest(&mut self, f: &str, base: &str) -> Option<String> {
        if let Some(d) = self.digests.get(base) {
            return d.clone();
        }
        if self.too_large.contains(base) || self.unreadable.contains(base) {
            return None;
        }
        if self.digests.len() >= self.limits.hashed_files {
            if !self.hash_limit_reported {
                self.hash_limit_reported = true;
                let message = format!(
                    "the summaries name more than {} package files as inputs",
                    self.limits.hashed_files
                );
                self.add("evidence.limit", f, message);
            }
            return None;
        }
        let digest = match sha256_package_file(self.root, base, self.limits.gds_bytes) {
            Ok(d) => Some(d),
            Err(e) => {
                self.unreadable(base, e);
                None
            }
        };
        self.digests.insert(base.to_string(), digest.clone());
        digest
    }

    fn evidence(&mut self, m: &Manifest) {
        let Some(files) = self.summaries() else {
            let message = format!("more than {} summaries", self.limits.evidence_files);
            self.add("evidence.limit", "evidence", message);
            return;
        };
        let mut seen = HashSet::new();
        for name in files {
            let f = format!("evidence/{name}");
            let Some(bytes) = self.read(&f, self.limits.evidence_bytes) else {
                continue;
            };
            let text = match String::from_utf8(bytes) {
                Ok(t) => t,
                Err(e) => {
                    self.add(
                        "evidence.parse",
                        &f,
                        format!("not valid JSON: {}", first_line(&e.to_string())),
                    );
                    continue;
                }
            };
            if json_depth(&text) > self.limits.nesting {
                let message = nesting(self.limits.nesting, "objects and arrays");
                self.add("evidence.limit", &f, message);
                continue;
            }
            let parsed =
                serde_json::from_str::<serde_json::Value>(&text).map_err(|e| e.to_string());
            let doc = match parsed {
                Ok(v) => Val::from(&v),
                Err(e) => {
                    self.add(
                        "evidence.parse",
                        &f,
                        format!("not valid JSON: {}", first_line(&e)),
                    );
                    continue;
                }
            };
            let openada = doc.get("schema").and_then(Val::as_str) == Some(OPENADA_SCHEMA);
            let errors = validate_def(
                if openada {
                    "openada_result"
                } else {
                    "evidence"
                },
                &doc,
            );
            if !errors.is_empty() {
                for e in errors {
                    self.add("evidence.schema", &f, e);
                }
                continue;
            }
            let op = doc.get("operation").and_then(Val::as_str).unwrap_or("");
            let status = doc
                .get("engineering")
                .and_then(|e| e.get("status"))
                .and_then(Val::as_str)
                .unwrap_or("");
            seen.insert(op.to_string());
            if REQUIRED_EVIDENCE.contains(&op) && status != "pass" {
                self.add(
                    "evidence.status",
                    &f,
                    format!("{op} status is {}, expected pass", quote(status)),
                );
            } else if status != "pass" && status != "not_applicable" {
                self.add(
                    "evidence.status",
                    &f,
                    format!(
                        "{op} status is {}, expected pass or not_applicable",
                        quote(status)
                    ),
                );
            }
            if let Some(Val::Arr(inputs)) = doc.get("inputs") {
                for item in inputs {
                    let (Some(path), Some(digest)) = (
                        item.get("path").and_then(Val::as_str),
                        item.get("sha256").and_then(Val::as_str),
                    ) else {
                        continue;
                    };
                    let normal = path.replace('\\', "/");
                    let base = normal.rsplit('/').next().unwrap_or("");
                    if base.is_empty() || base == "." || base == ".." || !self.present(base) {
                        continue;
                    }
                    let Some(actual) = self.digest(&f, base) else {
                        continue;
                    };
                    if actual != digest {
                        self.add(
                            "evidence.stale",
                            &f,
                            format!(
                                "input {} has sha256 {}, the package file has {}",
                                quote(base),
                                digest.chars().take(12).collect::<String>(),
                                actual.chars().take(12).collect::<String>()
                            ),
                        );
                    }
                }
            }
            if !openada {
                let pdk = doc.get("pdk");
                let name = pdk.and_then(|p| p.get("name")).and_then(Val::as_str);
                let commit = pdk.and_then(|p| p.get("commit")).and_then(Val::as_str);
                if let (Some(name), Some(commit)) = (name, commit) {
                    if name != m.pdk.name {
                        self.add(
                            "pdk.mismatch",
                            &f,
                            format!(
                                "ran on PDK {}, macro.toml says {}",
                                quote(name),
                                quote(&m.pdk.name)
                            ),
                        );
                    } else if !commits_match(commit, &m.pdk.commit) {
                        self.add(
                            "pdk.mismatch",
                            &f,
                            format!(
                                "ran on PDK commit {}, macro.toml says {}",
                                quote(commit),
                                quote(&m.pdk.commit)
                            ),
                        );
                    }
                }
            }
        }
        for op in REQUIRED_EVIDENCE {
            if !seen.contains(op) {
                self.add(
                    "evidence.missing",
                    "evidence",
                    format!("no summary for {op}"),
                );
            }
        }
    }

    fn expected_pdk(&mut self, m: &Manifest, name: &str, commit: Option<&str>) {
        if m.pdk.name != name {
            self.add(
                "pdk.expected",
                MANIFEST,
                format!(
                    "the package is built for {}, expected {}",
                    quote(&m.pdk.name),
                    quote(name)
                ),
            );
        } else if let Some(c) = commit
            && !commits_match(c, &m.pdk.commit)
        {
            self.warn(
                "pdk.expected",
                MANIFEST,
                format!(
                    "the package is built on PDK commit {}, expected {}",
                    quote(&m.pdk.commit),
                    quote(c)
                ),
            );
        }
    }

    /// `pdk.cell_name`: the package's name and every cell a chip would
    /// merge, ASCII case folded, outside its PDK's cell name spaces and, when
    /// the validator has the PDK's libraries, not one of their cells.
    fn cell_names(&mut self, m: &Manifest) {
        let prefixes = pdk_cell_prefixes(&m.pdk.name);
        let cells = std::mem::take(&mut self.cells);
        let libs = self.pdk_libs.take();
        let pdk = quote(&m.pdk.name);
        let all = std::iter::once((MANIFEST.to_string(), "name", m.name.clone(), String::new()))
            .chain(cells);
        for (f, what, name, digest) in all {
            let low = ascii_lower(&name);
            let hit = prefixes.iter().find(|p| low.starts_with(*p));
            if hit.is_none() && !libs.as_ref().is_some_and(|l| l.names.contains(&low)) {
                continue;
            }
            let pdk_gds = libs.as_ref().and_then(|l| l.gds.get(&name));
            let (message, severity) = match (what, pdk_gds, hit) {
                // The PDK's own cell, as a hardened block holds its standard cells.
                ("GDS cell", Some(theirs), _) if theirs.contains(&digest) => continue,
                ("GDS cell", Some(_), _) => (
                    format!(
                        "{what} {} differs from the cell of that name in PDK {pdk}",
                        quote(&name)
                    ),
                    Severity::Error,
                ),
                (_, _, None) => (
                    format!("{what} {} is a cell of PDK {pdk}", quote(&name)),
                    Severity::Error,
                ),
                ("GDS cell", None, Some(hit)) if libs.is_none() => (
                    format!(
                        "{what} {} begins with {}, as the cells of PDK {pdk} do; without the \
                         PDK's libraries, the validator cannot compare it with the PDK's own",
                        quote(&name),
                        quote(hit)
                    ),
                    Severity::Warning,
                ),
                (_, _, Some(hit)) => (
                    format!(
                        "{what} {} begins with {}, as the cells of PDK {pdk} do",
                        quote(&name),
                        quote(hit)
                    ),
                    Severity::Error,
                ),
            };
            self.push("pdk.cell_name", &f, message, severity);
        }
        self.pdk_libs = libs;
    }
}

/// Check a macro package directory against macro/v1.
///
/// Fails only when `dir` is not a directory or `opts.pdk` is malformed;
/// everything wrong with the package itself is a finding in the report.
pub fn check_package(dir: &Path, opts: &CheckOptions) -> Result<Report, MacroError> {
    let expected = opts.pdk.as_deref().map(parse_pdk_option).transpose()?;
    if !dir.is_dir() {
        return Err(MacroError::NotADirectory(dir.display().to_string()));
    }
    let mut c = Checker {
        root: dir,
        limits: opts.limits,
        findings: Vec::new(),
        unlisted_errors: 0,
        unlisted_warnings: 0,
        too_large: HashSet::new(),
        unreadable: HashSet::new(),
        cells: Vec::new(),
        pdk_libs: opts.pdk_libs.as_ref().map(|l| PdkLibs {
            names: l.names.iter().map(|n| ascii_lower(n)).collect(),
            gds: l.gds.clone(),
        }),
        digests: HashMap::new(),
        hash_limit_reported: false,
        name: None,
        kind: None,
    };
    let manifest = c.manifest();
    if let Some(m) = &manifest {
        c.semantics(m);
        c.dirname(m);
        c.required(m);
        let (lef, lef_error, lef_names) = c.read_lef(m);
        c.gds(m, lef.as_ref());
        c.lef(m, lef.as_ref(), lef_error, &lef_names);
        let mut libs = vec![m.file("lib")];
        for corner in &m.corners {
            if let Some(l) = &corner.liberty
                && !libs.contains(l)
            {
                libs.push(l.clone());
            }
        }
        for f in &libs {
            c.liberty(m, f);
        }
        c.verilog(m);
        c.spice(m);
        c.specs(m);
        c.evidence(m);
        if let Some((name, commit)) = &expected {
            c.expected_pdk(m, name, commit.as_deref());
        }
        c.cell_names(m);
    }
    let unlisted = c.unlisted_errors + c.unlisted_warnings;
    if unlisted > 0 {
        c.findings.push(Finding {
            rule: "check.limit".into(),
            severity: Severity::Warning,
            file: ".".into(),
            message: format!(
                "{} not listed: {}, {}",
                plural(unlisted, "more finding", None),
                plural(c.unlisted_errors, "error", None),
                plural(c.unlisted_warnings, "warning", None)
            ),
        });
    }
    let listed_errors = c
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let errors = listed_errors + c.unlisted_errors;
    let warnings = c.findings.len() - listed_errors + c.unlisted_warnings;
    Ok(Report {
        schema: REPORT_SCHEMA,
        package: dir.display().to_string(),
        name: c.name,
        kind: c.kind,
        passed: errors == 0,
        errors,
        warnings,
        findings: c.findings,
    })
}
