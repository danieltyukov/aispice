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
//! generator draws names the generator and which instances play which part,
//! and, where the generator's wiring changes what the block meets, the spec
//! table it meets after layout and why.

use aispice_core::schematic::{self, Schematic};
use aispice_sim::corner::CornerSet;
use serde::{Deserialize, Serialize};
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

/// The layout generator that draws a template's block, and the role map it
/// takes: the generator's own types, so a template that names a generator
/// or a role it does not have fails to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutHint {
    pub generator: aispice_layout::generator::Template,
    pub roles: aispice_layout::generator::Roles,
    /// The spec table the block meets after layout by the generator, at
    /// every corner of the set, where the wiring changes what the
    /// schematic's table holds; `None` when that table holds after layout
    /// too. Same specs, same names, different limits.
    #[serde(default)]
    pub specs: Option<String>,
    /// Why the table after layout differs from the schematic's.
    #[serde(default)]
    pub note: Option<String>,
    /// For a comparator: the largest input-referred offset after layout at
    /// tt and 27 C, in mV, found by sweeping the input difference.
    #[serde(default)]
    pub offset_mv: Option<f64>,
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

    /// A parameter from a PDK device's SpiceLine (`w=11u l=2u ng=2 m=1`).
    fn spice_param(line: &str, key: &str) -> Option<f64> {
        line.split_whitespace()
            .filter_map(|w| w.split_once('='))
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| aispice_core::units::parse(v))
    }

    /// An IC template says which PDK it is drawn in, the corners and
    /// simulators its specs hold on, and uses only that kit's devices, each
    /// inside the models' valid range. Other templates name none of this.
    #[test]
    fn ic_templates_name_their_kit_corners_and_simulators() {
        let mut ic = 0;
        for t in all() {
            if t.category != "ic" {
                assert!(
                    t.pdk.is_none()
                        && t.corners.is_none()
                        && t.simulators.is_empty()
                        && t.layout.is_none(),
                    "{}: only IC templates name a PDK, corners, simulators or a layout",
                    t.id
                );
                continue;
            }
            ic += 1;
            let kind = t
                .pdk
                .as_deref()
                .unwrap_or_else(|| panic!("{}: no pdk", t.id));
            let kit = aispice_core::pdk::kit(kind)
                .unwrap_or_else(|| panic!("{}: unknown PDK kind {kind}", t.id));
            let set = t
                .corners
                .as_ref()
                .unwrap_or_else(|| panic!("{}: no corners", t.id));
            set.corners()
                .unwrap_or_else(|e| panic!("{}: corners: {e}", t.id));
            assert_eq!(
                set.process,
                ["tt", "ss", "ff", "sf", "fs"],
                "{}: every process corner",
                t.id
            );
            assert!(!t.simulators.is_empty(), "{}: no simulators", t.id);
            let mut hbt = false;
            for s in t.schematic().symbols() {
                let Some(device) = aispice_core::pdk::device_for_symbol(&s.name) else {
                    continue;
                };
                assert!(kit.device(device.name).is_some(), "{}: {}", t.id, s.name);
                hbt |= device.family == aispice_core::pdk::Family::Hbt;
                if device.is_mosfet() {
                    // IHP's MOS models hold up to 10 um per finger.
                    let line = s.attr("SpiceLine").unwrap_or_default();
                    let w = spice_param(line, "w").unwrap();
                    let ng = spice_param(line, "ng").unwrap_or(1.0);
                    assert!(
                        w / ng <= 10e-6 + 1e-12,
                        "{}: {} has {} um per finger",
                        t.id,
                        s.inst_name().unwrap_or_default(),
                        w / ng * 1e6
                    );
                }
            }
            for sim in &t.simulators {
                assert!(
                    ["ngspice", "xyce"].contains(&sim.as_str()),
                    "{}: simulator {sim}",
                    t.id
                );
            }
            // Xyce's IHP models have no HBT.
            assert!(
                !hbt || t.simulators == ["ngspice"],
                "{}: a circuit with an HBT runs on ngspice only",
                t.id
            );
        }
        assert_eq!(ic, 6, "six IC templates");
    }

    /// A template that names a layout generator goes straight to layout:
    /// the generator's planner accepts its netlist with its role map and
    /// keeps the roles as given.
    #[test]
    fn ic_role_maps_suit_their_layout_generators() {
        use aispice_layout::generator::{Params, generate};
        let lib = SymbolLibrary::builtin_only();
        let mut drawn = BTreeSet::new();
        for t in all() {
            let Some(layout) = &t.layout else { continue };
            let sch = t.schematic();
            for (role, devices) in &layout.roles {
                for name in devices.names() {
                    assert!(
                        sch.symbol(name).is_some(),
                        "{}: role {role} names {name}, which is not in the schematic",
                        t.id
                    );
                }
            }
            let (built, _) = netlist::build(&sch, &lib, &t.id);
            let planned = generate(
                &built.netlist,
                layout.generator,
                &layout.roles,
                &Params::default(),
                &t.id,
            )
            .unwrap_or_else(|e| panic!("{}: {e}", t.id));
            assert_eq!(planned.meta.roles, layout.roles, "{}", t.id);
            drawn.insert(layout.generator.name());
            // A table after layout has the schematic's specs, by name, and
            // says why it differs.
            if let Some(after) = &layout.specs {
                let names = |text: &str| -> Vec<String> {
                    aispice_sim::spec::parse_specs(text)
                        .unwrap_or_else(|e| panic!("{}: {e}", t.id))
                        .iter()
                        .map(|s| s.name.clone())
                        .collect()
                };
                assert_eq!(names(after), names(&t.specs), "{}", t.id);
                assert!(
                    layout.note.is_some(),
                    "{}: a layout table needs a note",
                    t.id
                );
            }
            assert_eq!(
                layout.offset_mv.is_some(),
                layout.generator.name() == "comparator",
                "{}: an offset belongs to a comparator",
                t.id
            );
        }
        assert_eq!(
            drawn.into_iter().collect::<Vec<_>>(),
            ["comparator", "current_mirror", "ota5t", "two_stage"]
        );
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
