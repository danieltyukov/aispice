//! Verified reference circuits to start a design from.
//!
//! Each template is an LTspice schematic (`templates/<id>.asc`) and a TOML
//! file (`templates/<id>.toml`) with what a designer needs to adapt it: the
//! parts usually changed, what each one controls and its design equation,
//! the analysis the schematic runs, and a spec table in the `check_specs`
//! format. Both files are compiled into the binary. The test suite
//! simulates every template and requires it to meet its own specs, so a
//! design that starts from one starts from a circuit known to work.
//!
//! Integrated-circuit templates (category `ic`) are drawn in a process design
//! kit's devices and name it (`pdk = "ihp-sg13g2"`): they simulate only in a
//! project that uses that PDK. Their specs hold at a set of corners the
//! template names, which `check_specs`, `optimize` and `monte_carlo` take as
//! `corners`, on the simulators it lists. A template whose block a layout
//! generator draws names the generator and which instances play which part.

use aispice_core::schematic::{self, Schematic};
use aispice_sim::corner::CornerSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The categories templates are filed under, in display order.
pub const CATEGORIES: &[&str] = &[
    "filters",
    "amplifiers",
    "references",
    "power",
    "oscillators",
    "digital",
    "sensors",
    "ic",
];

/// A part a designer usually changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    /// Instance name in the schematic, e.g. `R1`.
    pub part: String,
    /// What changing it does.
    pub controls: String,
    /// The design equation, in plain text.
    pub equation: String,
}

/// A role's instances: one name or several, as the layout generators read
/// role maps (`{"pair": ["M1", "M2"], "tail": "M5"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RoleDevices {
    One(String),
    Many(Vec<String>),
}

impl RoleDevices {
    pub fn names(&self) -> Vec<&str> {
        match self {
            RoleDevices::One(s) => vec![s.as_str()],
            RoleDevices::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// The layout generator that draws a template's block, and the role map it
/// takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutHint {
    /// `current_mirror`, `ota5t`, `two_stage` or `comparator`.
    pub generator: String,
    pub roles: BTreeMap<String, RoleDevices>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub id: String,
    pub title: String,
    /// One of [`CATEGORIES`].
    pub category: String,
    pub description: String,
    /// The analysis directive the schematic runs.
    pub analysis: String,
    /// One spec per line, as `check_specs` reads them.
    pub specs: String,
    #[serde(default)]
    pub parameters: Vec<Parameter>,
    /// The kind of PDK the circuit is drawn in (`ihp-sg13g2`), for IC
    /// templates.
    #[serde(default)]
    pub pdk: Option<String>,
    /// The corners the spec table holds at.
    #[serde(default)]
    pub corners: Option<CornerSet>,
    /// The simulators the spec table is verified on; empty means any.
    #[serde(default)]
    pub simulators: Vec<String>,
    /// The layout generator for the block, when there is one.
    #[serde(default)]
    pub layout: Option<LayoutHint>,
    /// The `.asc` file.
    #[serde(skip)]
    pub asc: &'static [u8],
}

impl Template {
    /// The schematic, parsed.
    pub fn schematic(&self) -> Schematic {
        schematic::parse_bytes(self.asc).0
    }

    /// The spec table as written beside a circuit made from this template.
    pub fn specs_file_text(&self) -> String {
        format!(
            "# Specs from the aispice template {} ({}).\n# Edit them to your requirements; check_specs reads this file.\n{}",
            self.id,
            self.title,
            self.specs.trim_start()
        )
    }

    /// Whether every word of `query` appears in the id, title, category or
    /// description, ignoring case.
    fn matches(&self, query: &str) -> bool {
        let hay = format!(
            "{} {} {} {} {}",
            self.id.replace('_', " "),
            self.title,
            self.category,
            self.pdk.as_deref().unwrap_or(""),
            self.description
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|w| hay.contains(&w.to_lowercase()))
    }
}

/// `(id, metadata, schematic)` for every template.
macro_rules! embed {
    ($($id:literal),* $(,)?) => {
        &[$((
            $id,
            include_str!(concat!("../templates/", $id, ".toml")),
            include_bytes!(concat!("../templates/", $id, ".asc")),
        )),*]
    };
}

const FILES: &[(&str, &str, &[u8])] = embed![
    "rc_lowpass",
    "rc_highpass",
    "sallen_key_lowpass",
    "mfb_bandpass",
    "integrator",
    "inverting_amplifier",
    "noninverting_amplifier",
    "difference_amplifier",
    "ce_amplifier",
    "cs_amplifier",
    "zener_reference",
    "pass_regulator",
    "halfwave_rectifier",
    "nmos_lowside_switch",
    "wien_bridge_oscillator",
    "level_shifter",
    "transimpedance_amplifier",
    "current_mirror",
    "ota5t",
    "two_stage",
    "comparator",
    "bandgap",
    "ldo",
];

static ALL: LazyLock<Vec<Template>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|(id, meta, asc)| {
            let mut t: Template = toml::from_str(meta)
                .unwrap_or_else(|e| panic!("embedded template {id} is invalid: {e}"));
            t.asc = asc;
            t
        })
        .collect()
});

/// Every template, in library order.
pub fn all() -> &'static [Template] {
    &ALL
}

/// A template by id, ignoring case.
pub fn find(id: &str) -> Option<&'static Template> {
    let id = id.trim();
    all().iter().find(|t| t.id.eq_ignore_ascii_case(id))
}

/// Templates in a category (ignoring case) whose text contains every word of
/// `query`. Either filter may be empty.
pub fn search(category: Option<&str>, query: Option<&str>) -> Vec<&'static Template> {
    let category = category.map(str::trim).filter(|c| !c.is_empty());
    let query = query.map(str::trim).filter(|q| !q.is_empty());
    all()
        .iter()
        .filter(|t| category.is_none_or(|c| t.category.eq_ignore_ascii_case(c)))
        .filter(|t| query.is_none_or(|q| t.matches(q)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aispice_core::lint::lint;
    use aispice_core::netlist;
    use aispice_core::symbol::SymbolLibrary;
    use std::collections::BTreeSet;

    #[test]
    fn every_file_in_the_folder_is_embedded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        let mut on_disk = BTreeSet::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let ext = path.extension().unwrap().to_string_lossy().into_owned();
            assert!(ext == "asc" || ext == "toml", "unexpected file {path:?}");
            on_disk.insert(stem);
        }
        let embedded: BTreeSet<String> = FILES.iter().map(|(id, ..)| id.to_string()).collect();
        assert_eq!(
            on_disk, embedded,
            "templates/ and FILES must list the same ids"
        );
        assert!(all().len() >= 12);
    }

    #[test]
    fn metadata_is_complete_and_consistent() {
        let mut categories = BTreeSet::new();
        for t in all() {
            let (id, meta, asc) = FILES.iter().find(|f| f.0 == t.id).unwrap();
            assert_eq!(&t.id, id);
            assert!(
                CATEGORIES.contains(&t.category.as_str()),
                "{id}: {}",
                t.category
            );
            categories.insert(t.category.as_str());
            assert!(!t.title.is_empty() && !t.description.is_empty(), "{id}");
            // Plain ASCII keeps long dashes and other decoration out.
            assert!(meta.is_ascii() && asc.is_ascii(), "{id}: non-ASCII text");
            let specs = aispice_sim::spec::parse_specs(&t.specs)
                .unwrap_or_else(|e| panic!("{id}: specs: {e}"));
            assert!(specs.len() >= 2, "{id}: at least two specs");
            let written = aispice_sim::spec::parse_specs(&t.specs_file_text()).unwrap();
            assert_eq!(written, specs, "{id}: the specs file reads back the same");
            assert!(!t.parameters.is_empty(), "{id}: no parameters");
        }
        assert_eq!(
            categories.len(),
            CATEGORIES.len(),
            "every category has a template"
        );
    }

    /// Every template parses cleanly, has no rule-check findings at all (not
    /// even the informational ones about layout), netlists without warnings,
    /// runs the analysis its metadata names, and names real parts.
    #[test]
    fn schematics_are_clean() {
        let lib = SymbolLibrary::builtin_only();
        for t in all() {
            let (sch, warnings) = schematic::parse_bytes(t.asc);
            assert!(warnings.is_empty(), "{}: {warnings:?}", t.id);
            let findings = lint(&sch, &lib).findings;
            assert!(findings.is_empty(), "{}: {findings:?}", t.id);
            let (built, _) = netlist::build(&sch, &lib, &t.id);
            assert!(built.warnings.is_empty(), "{}: {:?}", t.id, built.warnings);
            let directives: Vec<String> = sch.directives().flat_map(|d| d.lines()).collect();
            assert!(
                directives.iter().any(|d| d.trim() == t.analysis),
                "{}: analysis `{}` not in {directives:?}",
                t.id,
                t.analysis
            );
            for p in &t.parameters {
                assert!(
                    sch.symbol(&p.part).is_some(),
                    "{}: no part {}",
                    t.id,
                    p.part
                );
            }
        }
    }

    #[test]
    fn search_filters_by_category_and_words() {
        assert_eq!(search(None, None).len(), all().len());
        let filters = search(Some("Filters"), None);
        assert!(!filters.is_empty() && filters.iter().all(|t| t.category == "filters"));
        let hits: Vec<&str> = search(None, Some("band-pass"))
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        assert_eq!(hits, ["mfb_bandpass"]);
        assert!(search(Some("filters"), Some("photodiode")).is_empty());
        assert_eq!(find("RC_LOWPASS").unwrap().id, "rc_lowpass");
        assert!(find("nope").is_none());
    }
}
