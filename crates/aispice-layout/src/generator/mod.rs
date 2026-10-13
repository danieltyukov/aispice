//! Layout generators: deterministic Rust planners that turn a schematic and
//! a few parameters into a drawing plan, and one KLayout script that draws
//! any plan with IHP's pcells.
//!
//! A generator never edits GDS: when DRC or LVS finds a problem, the fix is
//! a different parameter (pattern, dummies, fingers, track pitch, row gap)
//! or a different role map, and the block is drawn again. Each layout gets
//! a `<cell>.layout.json` beside it recording how it was made, which LVS
//! and post-layout simulation read back.

pub mod cap;
pub mod plan;
pub mod route;
pub mod row;
pub mod rules;
pub mod templates;

use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use aispice_core::netlist::Netlist;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub use plan::Plan;
pub use templates::Template;

const DRAW_SCRIPT: &str = include_str!("../../klayout/draw.py");

/// How matched devices are ordered in a row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Pattern {
    /// `ABAB`: units alternate.
    Interdigitated,
    /// `ABBA`: units mirror about the row's centre.
    #[default]
    CommonCentroid,
}

/// The parameters a generator takes, and the agent may change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Params {
    #[serde(default)]
    pub pattern: Pattern,
    /// Dummy fingers at each end of every island (0 to 4, default 1).
    #[serde(default = "one")]
    pub dummies: u32,
    /// A full guard ring around each row (default); false draws a tap bar
    /// under the row only.
    #[serde(default = "yes")]
    pub guard_ring: bool,
    /// Fingers of the smallest device in each row (even); the others get
    /// the same finger width. By default the fewest that keep fingers at
    /// most 2 um wide.
    #[serde(default)]
    pub fingers: Option<u32>,
    /// Centre-to-centre pitch of the routing tracks, um (default 0.6, at
    /// least 0.51).
    #[serde(default = "track_pitch")]
    pub track_pitch: f64,
    /// Space between the routing channels of two rows, um (default 1).
    #[serde(default = "row_gap")]
    pub row_gap: f64,
    /// Bring every pin to the block's bottom edge on a Metal4 column of its
    /// own, 1.2 um apart on a grid fixed to the block's origin, as a macro
    /// package needs (default false: pins are labels on the Metal3 tracks).
    #[serde(default)]
    pub pin_edge: bool,
}

fn one() -> u32 {
    1
}
fn yes() -> bool {
    true
}
fn track_pitch() -> f64 {
    0.6
}
fn row_gap() -> f64 {
    1.0
}

impl Default for Params {
    fn default() -> Self {
        Self {
            pattern: Pattern::default(),
            dummies: 1,
            guard_ring: true,
            fingers: None,
            track_pitch: track_pitch(),
            row_gap: row_gap(),
            pin_edge: false,
        }
    }
}

impl Params {
    pub fn check(&self) -> Result<(), String> {
        if self.dummies > 4 {
            return Err(format!("dummies is {}; use 0 to 4", self.dummies));
        }
        match rules::um_to_nm(self.track_pitch) {
            Some(nm) if (510..=5000).contains(&nm) => {}
            _ => {
                return Err(format!(
                    "track_pitch {} um must be between 0.51 and 5 um on the 5 nm grid",
                    self.track_pitch
                ));
            }
        }
        match rules::um_to_nm(self.row_gap) {
            Some(nm) if (500..=20_000).contains(&nm) => {}
            _ => {
                return Err(format!(
                    "row_gap {} um must be between 0.5 and 20 um on the 5 nm grid",
                    self.row_gap
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn track_pitch_nm(&self) -> i64 {
        rules::um_to_nm(self.track_pitch).unwrap_or(600)
    }

    pub(crate) fn row_gap_nm(&self) -> i64 {
        rules::um_to_nm(self.row_gap).unwrap_or(1000)
    }
}

/// A device the layout has that the schematic does not: a dummy. LVS adds
/// these to the schematic side, and the extracted netlist has them too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraDevice {
    pub name: String,
    pub model: String,
    /// Total width and length, nm.
    pub w: i64,
    pub l: i64,
    pub ng: i64,
    pub d: String,
    pub g: String,
    pub s: String,
    pub b: String,
}

/// A role's devices: one instance name or several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// Which instances play which part: `{"pair": ["M1", "M2"], "tail": "M5"}`.
pub type Roles = BTreeMap<String, RoleDevices>;

/// What `<cell>.layout.json` records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutMeta {
    pub generator: Template,
    pub cell: String,
    /// The circuit file the layout was generated from.
    #[serde(default)]
    pub circuit: Option<String>,
    pub roles: Roles,
    pub params: Params,
    /// Netlist names of the schematic devices drawn (`XM1`, ...).
    pub devices: Vec<String>,
    /// Nets labelled as pins.
    pub ports: Vec<String>,
    pub dummies: Vec<ExtraDevice>,
    pub rows: Vec<serde_json::Value>,
    /// Nets joined across rows on Metal4.
    pub spine: Vec<String>,
    /// Width and height, um.
    pub size_um: [f64; 2],
}

impl LayoutMeta {
    /// The sidecar file of a GDS: `<cell>.layout.json` in the same folder.
    pub fn path_for(gds: &Path, cell: &str) -> std::path::PathBuf {
        gds.with_file_name(format!("{cell}.layout.json"))
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > 4 << 20 {
            return Err(format!("{} is too large", path.display()));
        }
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// A generated block: the plan to draw and its record.
#[derive(Debug, Clone)]
pub struct Generated {
    pub plan: Plan,
    pub meta: LayoutMeta,
}

impl Generated {
    /// What was built, in words.
    pub fn summary(&self) -> String {
        let m = &self.meta;
        let mut t = format!(
            "{} layout of {} ({} devices, {} dummies), {:.2} x {:.2} um.\n",
            serde_json::to_value(m.generator)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            m.cell,
            m.devices.len(),
            m.dummies.len(),
            m.size_um[0],
            m.size_um[1]
        );
        for r in &m.rows {
            if r["kind"] == "mim" {
                t.push_str(&format!(
                    "  capacitor {}: MIM {} x {} um\n",
                    r["name"].as_str().unwrap_or(""),
                    r["finger_width_um"],
                    r["length_um"]
                ));
                continue;
            }
            t.push_str(&format!(
                "  row {}: {} fingers {} um wide, L {} um; order {}\n",
                r["name"].as_str().unwrap_or(""),
                r["kind"].as_str().unwrap_or(""),
                r["finger_width_um"],
                r["length_um"],
                r["order"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" | "))
                    .unwrap_or_default()
            ));
        }
        t.push_str(&format!("  pins: {}\n", m.ports.join(", ")));
        t
    }
}

/// The parameter change that usually fixes the DRC rules a generated block
/// broke, and why, when the parameters have one: a wider track pitch for
/// spacing on the routing layers, a guard ring and a larger row gap for
/// well, implant and latch-up rules. `None` when no parameter addresses the
/// rules (or the parameter is at its limit).
pub fn adjust(params: &Params, rules: &[String]) -> Option<(Params, String)> {
    let family = |r: &String| r.split('.').next().unwrap_or("").to_string();
    let families: Vec<String> = rules.iter().map(family).collect();
    let has = |names: &[&str]| families.iter().any(|f| names.contains(&f.as_str()));
    let mut next = params.clone();
    if has(&["M2", "M3", "M4", "V2", "V3"]) && params.track_pitch < 2.0 {
        next.track_pitch = ((params.track_pitch + 0.2) * 1000.0).round() / 1000.0;
        let why = format!("track_pitch {} um", next.track_pitch);
        return Some((next, why));
    }
    if has(&["NW", "pSD", "nSD", "LU", "PWB"]) {
        if !params.guard_ring {
            next.guard_ring = true;
            return Some((next, "guard_ring on".into()));
        }
        if params.row_gap < 5.0 {
            next.row_gap = ((params.row_gap + 1.0) * 1000.0).round() / 1000.0;
            let why = format!("row_gap {} um", next.row_gap);
            return Some((next, why));
        }
    }
    None
}

/// Plan a block from a circuit's netlist.
pub fn generate(
    netlist: &Netlist,
    template: Template,
    roles: &Roles,
    params: &Params,
    cell: &str,
) -> Result<Generated, String> {
    params.check()?;
    tools::check_name("cell", cell).map_err(|e| e.to_string())?;
    let mos = templates::mosfets(netlist)?;
    let caps = templates::caps(netlist)?;
    let block = templates::block(template, &mos, &caps, roles)?;
    let mut rows = block
        .rows
        .iter()
        .map(|r| row::build(r, params))
        .collect::<Result<Vec<_>, _>>()?;
    for c in &block.caps {
        rows.push(cap::build(c)?);
    }
    let ports = crate::lvs::block_ports(netlist, Some(&block.elements));
    let (plan, spine) = route::assemble(cell, &rows, &ports, params, &block.mirror);
    let bbox = plan.bbox().unwrap_or(plan::Rect::new(0, 0, 0, 0));
    let mut dummies: Vec<ExtraDevice> = Vec::new();
    for r in &rows {
        dummies.extend(r.dummies.iter().cloned());
    }
    let meta = LayoutMeta {
        generator: template,
        cell: cell.to_string(),
        circuit: None,
        roles: block.roles,
        params: params.clone(),
        devices: block.elements,
        ports,
        dummies,
        rows: rows
            .iter()
            .map(|r| serde_json::to_value(&r.summary).unwrap_or_default())
            .collect(),
        spine,
        size_um: [bbox.width() as f64 / 1000.0, bbox.height() as f64 / 1000.0],
    };
    Ok(Generated { plan, meta })
}

/// Draw a plan with the PDK's pcells and write it to `gds`.
pub async fn draw(
    programs: &Programs,
    files: &PdkLayoutFiles,
    plan: &Plan,
    gds: &Path,
    cancel: &CancellationToken,
) -> Result<(), LayoutError> {
    files.check_pcells()?;
    tools::check_path(gds)?;
    tools::check_name("cell", &plan.cell)?;
    for l in &plan.labels {
        tools::check_name("label", &l.text)?;
    }
    let run = RunDir::new()?;
    let script = run.script("draw.py", DRAW_SCRIPT)?;
    let json = run.file("plan.json", plan.to_draw_json().to_string().as_bytes())?;
    let out = tools::klayout(
        programs,
        &run,
        &script,
        &[
            ("pdk_python", files.python.display().to_string()),
            ("plan", json.display().to_string()),
            ("out", gds.display().to_string()),
        ],
        &tools::Access::new().read(&files.root).write_file(gds),
        cancel,
    )
    .await?;
    if !out.success() || !tools::is_output(gds) {
        return Err(LayoutError::Failed {
            program: "KLayout (drawing the layout)".into(),
            message: tools::tail(&out.combined(), 12),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_rules_map_to_parameters() {
        let p = Params::default();
        let (n, why) = adjust(&p, &["M3.b".into()]).unwrap();
        assert_eq!(n.track_pitch, 0.8);
        assert_eq!(why, "track_pitch 0.8 um");
        let off = Params {
            guard_ring: false,
            ..Default::default()
        };
        assert!(adjust(&off, &["LU.b".into()]).unwrap().0.guard_ring);
        assert_eq!(adjust(&p, &["NW.b".into()]).unwrap().0.row_gap, 2.0);
        assert!(adjust(&p, &["Gat.a".into()]).is_none());
        let wide = Params {
            track_pitch: 2.0,
            ..Default::default()
        };
        assert!(adjust(&wide, &["M2.b".into()]).is_none());
    }

    const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\n.ac dec 20 1 1G\n";
    const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n";

    #[test]
    fn the_comparator_is_drawn_symmetric() {
        let n = aispice_core::netlist::parse(STRONGARM);
        for pin_edge in [false, true] {
            let p = Params {
                pin_edge,
                ..Default::default()
            };
            let g = generate(&n, Template::Comparator, &Roles::new(), &p, "cmp").unwrap();
            let order: Vec<String> = g.meta.rows.iter().map(|r| r["order"].to_string()).collect();
            assert_eq!(
                order,
                [
                    r#"["d M0 d"]"#,
                    r#"["d M1 M2 d"]"#,
                    r#"["M3","M4"]"#,
                    r#"["d M5 M6 d"]"#,
                    r#"["d M9 M7 M8 M10 d"]"#,
                ],
                "each row reads the same reflected, with the sides swapped"
            );
            // Per routing layer, each net's metal area and via count.
            let tally = |net: &str| {
                let mut t: BTreeMap<plan::Layer, (i64, usize)> = BTreeMap::new();
                for s in g
                    .plan
                    .shapes
                    .iter()
                    .filter(|s| s.net.as_deref() == Some(net))
                {
                    let e = t.entry(s.layer).or_insert((0, 0));
                    e.0 += s.rect.width() * s.rect.height();
                    e.1 += 1;
                }
                t
            };
            for (a, b) in [("xp", "xn"), ("outn", "outp"), ("inp", "inm")] {
                let (ta, tb) = (tally(a), tally(b));
                assert_eq!(
                    ta.keys().collect::<Vec<_>>(),
                    tb.keys().collect::<Vec<_>>(),
                    "{a} and {b} use the same layers"
                );
                for (layer, (area_a, n_a)) in &ta {
                    let (area_b, n_b) = tb[layer];
                    assert_eq!(*n_a, n_b, "{a} and {b}: shapes on {layer:?}");
                    let diff = (*area_a - area_b).abs() as f64 / *area_a.max(&area_b) as f64;
                    eprintln!(
                        "{a} {b} {layer:?}: {area_a} {area_b} nm2 ({:.1} %)",
                        100.0 * diff
                    );
                    // Where a pair takes two tracks, the outer one's stubs
                    // are a pitch longer (Metal2); pins stand on a grid
                    // (Metal3 and Metal4 with pin_edge).
                    let limit = if *layer == plan::Layer::Metal2 || pin_edge {
                        0.1
                    } else {
                        0.03
                    };
                    assert!(
                        diff < limit,
                        "{a} and {b} on {layer:?}: {area_a} and {area_b} nm2 (pin_edge {pin_edge})"
                    );
                }
            }
            // A net that is its own mirror image has as much routing metal
            // on each side of the axis, its pin column included.
            let bbox = g.plan.bbox().unwrap();
            let axis = (bbox.x1 + bbox.x2) / 2;
            for net in ["clk", "vdd", "0"] {
                let (mut left, mut right) = (0i64, 0i64);
                for s in g.plan.shapes.iter().filter(|s| {
                    s.net.as_deref() == Some(net)
                        && matches!(s.layer, plan::Layer::Metal3 | plan::Layer::Metal4)
                }) {
                    let r = s.rect;
                    let area = |a: i64, b: i64| (b - a).max(0) * r.height();
                    left += area(r.x1, r.x2.min(axis));
                    right += area(r.x1.max(axis), r.x2);
                }
                let diff = (left - right).abs() as f64 / left.max(right) as f64;
                assert!(
                    diff < 0.05,
                    "{net}: {left} nm2 left of the axis, {right} right (pin_edge {pin_edge})"
                );
            }
            let errors = g.plan.spacing_errors(&BTreeMap::new());
            assert!(errors.is_empty(), "{errors:?}");
        }
    }

    #[test]
    fn edge_pins_reach_the_bottom_edge_on_columns_of_their_own() {
        for (text, template) in [(OTA, Template::Ota5t), (STRONGARM, Template::Comparator)] {
            let n = aispice_core::netlist::parse(text);
            let roles = Roles::new();
            let plain = generate(&n, template, &roles, &Params::default(), "blk").unwrap();
            assert!(
                plain
                    .plan
                    .labels
                    .iter()
                    .all(|l| l.layer == plan::Layer::Metal3)
            );
            let p = Params {
                pin_edge: true,
                ..Default::default()
            };
            let g = generate(&n, template, &roles, &p, "blk").unwrap();
            let bbox = g.plan.bbox().unwrap();
            let pins: Vec<&plan::Label> = g.plan.labels.iter().filter(|l| l.pin).collect();
            assert_eq!(pins.len(), g.meta.ports.len(), "every port gets a pin");
            let mut xs = Vec::new();
            for l in &pins {
                assert_eq!(l.layer, plan::Layer::Metal4);
                assert_eq!(l.y, bbox.y1 + 100, "{} sits at the bottom edge", l.text);
                assert!(bbox.y1 < plain.plan.bbox().unwrap().y1, "below the rest");
                assert_eq!(
                    l.x.rem_euclid(route::PIN_PITCH),
                    0,
                    "{} on the grid",
                    l.text
                );
                xs.push(l.x);
            }
            xs.sort();
            xs.dedup();
            assert_eq!(xs.len(), pins.len(), "one column per pin");
            // Left of the block, or for the comparator (drawn symmetric) the
            // right net of each mirror pair right of it.
            let inner = plain.plan.bbox().unwrap();
            assert!(xs.iter().all(|x| *x < inner.x1 || *x > inner.x2));
            if template == Template::Ota5t {
                assert!(xs.iter().all(|x| *x < inner.x1));
            }
            // The routes inside are the same, and what was added keeps every
            // net clear of the others.
            for s in &plain.plan.shapes {
                assert!(g.plan.shapes.contains(s), "{s:?}");
            }
            let errors = g.plan.spacing_errors(&BTreeMap::new());
            assert!(errors.is_empty(), "{errors:?}");
            assert!(g.meta.params.pin_edge);
        }
    }
}
