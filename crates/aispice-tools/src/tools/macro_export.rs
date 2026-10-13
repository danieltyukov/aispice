//! `macro_export`: a generated block as a macro package (macro/v1) that a
//! chip, aispice's or ChipLeap's, can place.
//!
//! Everything is built and checked in a run folder under `.aispice/layout/`
//! first: the block drawn again with its pins at the edge and ground named,
//! DRC, LVS, extraction, the specs at every corner before and after layout,
//! the views, the evidence, and the package checked with the same rules as
//! `check_macro`. Only a package that passes reaches the project, in one
//! approved, undoable write.

use super::schematic_tools::spec;
use super::sim_tools::{corner_report, load_specs};
use crate::layout::{self, GdsFile};
use crate::project::Project;
use crate::workspace::{FileWrite, Workspace};
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::macropkg::{
    CheckOptions, Corner, Direction, Domain, Evidence, MANIFEST, MacroKind, Manifest, Pdk, PinKind,
    Producer, SCHEMA_VERSION, Size, Status, check_package,
};
use aispice_layout::drc::{self, DrcChecks};
use aispice_layout::extract;
use aispice_layout::generator::{self, Params};
use aispice_layout::lvs;
use aispice_layout::macro_export::{self as mx, SpecEntry};
use aispice_layout::tools as ltools;
use aispice_sim::corner::{CornerReport, CornerSet};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PinDirection {
    Input,
    Output,
    Inout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PinUse {
    Signal,
    Clock,
    Analog,
}

/// A change to what export found for a pin.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct PinOverride {
    #[serde(default)]
    pub direction: Option<PinDirection>,
    /// signal, clock or analog. Supplies are found by name.
    #[serde(default)]
    pub kind: Option<PinUse>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VerilogModel {
    #[default]
    Blackbox,
    Wreal,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MacroExportInput {
    /// The circuit the block comes from, with its test bench: a schematic or
    /// netlist of the project.
    pub circuit: String,
    /// The block's layout from the layout tool: a GDS of the project with its
    /// `.layout.json` record beside it.
    pub layout: String,
    /// The macro's name; default the layout's cell. Letters, digits and
    /// underscores, starting with a letter.
    #[serde(default)]
    pub name: Option<String>,
    /// The package folder; default `macros/<name>.macro`.
    #[serde(default)]
    pub out: Option<String>,
    /// The name of the ground pin, which is net 0 in the circuit; default
    /// `vss`.
    #[serde(default)]
    pub ground: Option<String>,
    /// Changes to the pins found from the circuit, by pin name, e.g.
    /// `{"clk": {"kind": "clock"}, "out": {"description": "Output, 1 pF"}}`.
    #[serde(default)]
    pub pins: Option<BTreeMap<String, PinOverride>>,
    /// Specs to measure, one per line; default the circuit's `.specs` file.
    #[serde(default)]
    pub specs: Option<String>,
    /// The corners to characterize at, as for check_specs; default tt at
    /// 27 C.
    #[serde(default)]
    pub corners: Option<CornerSet>,
    #[serde(default)]
    pub simulator: Option<String>,
    /// The block's supply in volts; default the circuit's supply source,
    /// else 1.2.
    #[serde(default)]
    pub voltage: Option<f64>,
    /// The Verilog model: `blackbox` (default) or `wreal`.
    #[serde(default)]
    pub verilog: Option<VerilogModel>,
    /// A line about the block for `macro.toml`.
    #[serde(default)]
    pub description: Option<String>,
    /// Extract resistor networks as well as capacitance (default false).
    #[serde(default)]
    pub resistance: bool,
}

const DESCRIPTION: &str = r#"
Export a generated block as a macro package (macro/v1), the format chips place blocks from: aispice's chip tool and ChipLeap both read it. The block is drawn again from its layout record with every pin brought to its bottom edge on Metal4 and ground (net 0) named as a pin, then checked with the PDK's DRC and LVS decks, extracted, and its specs measured at every corner before and after layout; every post-layout value must meet its limits. The package holds macro.toml (pins with directions and kinds, supply domain, corners, PDK and commit), the GDS, a LEF abstract from Magic, a liberty view with pin capacitance from extraction, a Verilog model (blackbox or wreal), the LVS netlist, the extracted netlist, the measured spec table, and DRC, LVS, extraction and simulation evidence with the hashes of the files they checked. It is checked with the same rules as check_macro before anything is written, then written into the project in one approved, undoable step. Needs a layout made by the layout tool.
"#;

pub struct MacroExport {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for MacroExport {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<MacroExportInput>("macro_export", DESCRIPTION)
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: MacroExportInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let built = match build(&self.ws, &input, ctx).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(e),
        };
        if let Err(e) = self
            .ws
            .write_files(
                self,
                &built.files,
                &format!("Macro package {}", built.folder),
                &built.text,
            )
            .await
        {
            return ToolOutput::error(e);
        }
        let mut t = format!(
            "Wrote {} ({} files; undo with the history tool).\n",
            built.folder,
            built.files.len()
        );
        t.push_str(&built.text);
        ToolOutput::text(crate::layout::noted(t).await)
            .with_data(json!({"kind": "generic", "value": built.data}))
    }
}

/// A package built and checked in a run folder, ready to write.
struct Built {
    folder: String,
    files: Vec<FileWrite>,
    text: String,
    data: Value,
}

fn version_of(
    name: &str,
    programs: &ltools::Programs,
    magic: bool,
    ctx: &ToolContext,
) -> impl std::future::Future<Output = (String, String)> {
    let name = name.to_string();
    let programs = programs.clone();
    let cancel = ctx.cancel.clone();
    async move {
        let v = ltools::version(&programs, magic, &cancel).await;
        (name, v)
    }
}

/// The corners a package is characterized at, as macro.toml writes them:
/// identifiers, in the corner set's order.
fn package_corners(
    set: &CornerSet,
    domain: &Domain,
    netlist: &aispice_core::netlist::Netlist,
) -> Result<Vec<(String, Corner)>, String> {
    let corners = set.corners().map_err(|e| format!("corners: {e}"))?;
    let mut used: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for c in corners {
        let process = c
            .process
            .as_ref()
            .map_or_else(|| "tt".to_string(), |p| p.name.clone());
        let temperature = c.temperature.unwrap_or(27.0);
        let mut ident = mx::corner_ident(&process, temperature, &c.supplies);
        let base = ident.clone();
        let mut k = 2;
        while used.contains(&ident) {
            ident = format!("{}_{k}", &base[..base.len().min(60)]);
            k += 1;
        }
        used.push(ident.clone());
        // A supply value is the domain's when it sets the source that feeds
        // the domain's power pin.
        let mut voltages = BTreeMap::new();
        for (target, v) in &c.supplies {
            let feeds = netlist.elements().any(|e| {
                e.letter() == 'V'
                    && e.name.eq_ignore_ascii_case(target)
                    && e.nodes
                        .iter()
                        .any(|n| domain.power.iter().any(|p| p.eq_ignore_ascii_case(n)))
            });
            if feeds {
                voltages.insert(domain.name.clone(), *v);
            }
        }
        out.push((
            c.name.clone(),
            Corner {
                name: ident,
                process,
                temperature,
                voltages,
                liberty: None,
            },
        ));
    }
    Ok(out)
}

/// The spec table: each spec after layout with its limits, and before
/// layout as `<spec>_schematic` without limits.
fn spec_entries(
    pre: &CornerReport,
    post: &CornerReport,
    names: &BTreeMap<String, String>,
    definitions: &BTreeMap<String, String>,
) -> Vec<SpecEntry> {
    let values = |r: &CornerReport, i: usize| -> BTreeMap<String, f64> {
        r.corners
            .iter()
            .zip(&r.rows[i].values)
            .filter_map(|(c, v)| Some((names.get(c)?.clone(), (*v)?)))
            .collect()
    };
    let mut out = Vec::new();
    for (i, row) in post.rows.iter().enumerate() {
        out.push(SpecEntry {
            name: row.name.clone(),
            description: Some("After layout (extracted).".into()),
            unit: row.unit.clone(),
            min: row.min,
            max: row.max,
            measured: values(post, i),
            definition: definitions.get(&row.name).cloned(),
        });
    }
    for (i, row) in pre.rows.iter().enumerate() {
        let name = format!("{}_schematic", row.name);
        if !mx::is_identifier(&name) {
            continue;
        }
        out.push(SpecEntry {
            name,
            description: Some(format!(
                "{} before layout (schematic), for comparison; its limits are on {}.",
                row.name, row.name
            )),
            unit: row.unit.clone(),
            min: None,
            max: None,
            measured: values(pre, i),
            definition: definitions.get(&row.name).cloned(),
        });
    }
    out
}

fn write(dir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let p = dir.join(name);
    std::fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(p)
}

/// Every file of a package folder (and its `evidence/`), as project writes
/// under `folder`.
fn package_files(dir: &Path, folder: &str) -> Result<Vec<FileWrite>, String> {
    let mut out = Vec::new();
    for (sub, prefix) in [
        (dir.to_path_buf(), String::new()),
        (dir.join("evidence"), "evidence/".to_string()),
    ] {
        let Ok(list) = std::fs::read_dir(&sub) else {
            continue;
        };
        let mut names: Vec<PathBuf> = list.filter_map(Result::ok).map(|e| e.path()).collect();
        names.sort();
        for p in names {
            if !p.is_file() {
                continue;
            }
            let file = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            out.push(FileWrite {
                rel: format!("{folder}/{prefix}{file}"),
                bytes,
            });
        }
    }
    Ok(out)
}

/// Refuse to write over something that is not an earlier aispice export of
/// the same files: another tool's package, or files this export would not
/// replace (their evidence would no longer describe the package).
/// The folder inspected is the one the output checks resolve, which the
/// files are then written into, not the path as given.
fn may_replace(p: &Project, folder: &str, files: &[FileWrite]) -> Result<(), String> {
    let Some(dir) = p.existing_output_dir(folder).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    match Manifest::load(&dir.join(MANIFEST)) {
        Ok(m) if m.producer.tool == "aispice" => {}
        Ok(m) => {
            return Err(format!(
                "{folder} holds a package made by {}; export to another folder with `out`",
                m.producer.tool
            ));
        }
        Err(_) => {
            if std::fs::read_dir(&dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(true)
            {
                return Err(format!(
                    "{folder} exists and is not an aispice macro package; export to another folder with `out`"
                ));
            }
        }
    }
    let ours: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    for (sub, prefix) in [
        (dir.clone(), format!("{folder}/")),
        (dir.join("evidence"), format!("{folder}/evidence/")),
    ] {
        if let Ok(list) = std::fs::read_dir(&sub) {
            for e in list.filter_map(Result::ok) {
                if e.path().is_file() {
                    let rel = format!("{prefix}{}", e.file_name().to_string_lossy());
                    if !ours.contains(&rel.as_str()) {
                        return Err(format!(
                            "{rel} is not part of the new package; remove it or export to another folder, so the package holds only what was checked"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

async fn build(
    ws: &Workspace,
    input: &MacroExportInput,
    ctx: &ToolContext,
) -> Result<Built, String> {
    let gds = layout::open_project_gds(ws, &input.layout, None)?;
    let GdsFile {
        meta,
        cell,
        rel: layout_rel,
        ..
    } = gds;
    let Some(meta) = meta else {
        return Err(format!(
            "{layout_rel} has no layout record beside it; macro export redraws a block with its pins at the edge, so it needs a layout made by the layout tool"
        ));
    };
    let name = input.name.clone().unwrap_or_else(|| cell.clone());
    if !mx::is_identifier(&name) {
        return Err(format!(
            "`{name}` cannot name a macro: use letters, digits and underscores, starting with a letter"
        ));
    }
    let ground = input.ground.clone().unwrap_or_else(|| "vss".into());
    if !mx::is_identifier(&ground) {
        return Err(format!("`{ground}` cannot name the ground pin"));
    }
    // Trimmed once, so every later use names the same folder.
    let folder = input
        .out
        .as_deref()
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| format!("macros/{name}.macro"));
    if let Some(v) = input.voltage
        && !(v.is_finite() && v > 0.0 && v < 100.0)
    {
        return Err(format!("voltage {v} V is not a supply voltage"));
    }
    let env = layout::env(ws)?;
    // A macro named like a cell of the PDK would clash with it in a chip and
    // in SPICE, which ignores case.
    let pdk_cells = mx::pdk_cell_names(&env.files.root)?;
    if let Some(why) = mx::pdk_name_clash(&name, &pdk_cells) {
        return Err(format!("{why}; choose another name with `name`"));
    }
    let p = env.project.clone();
    p.missing_dirs(&format!("{folder}/{MANIFEST}"))
        .map_err(|e| e.to_string())?;
    let pdk = ws
        .runner
        .project_pdk(&p)
        .map_err(|e| e.to_string())?
        .ok_or("this project names no PDK")?;
    let commit = pdk.commit.clone().ok_or_else(|| {
        format!(
            "the commit of PDK `{}` is unknown (its folder is not a git checkout); a macro package records the PDK commit it was checked with, so register a git checkout of the PDK",
            pdk.name
        )
    })?;
    let pkg_pdk = Pdk {
        name: pdk.name.clone(),
        commit: commit.clone(),
    };
    let (specs, spec_text) = load_specs(ws, &input.circuit, input.specs.as_deref())?;
    let (netlist, std_libs, _) = ws
        .runner
        .netlist_for(&p, &input.circuit)
        .map_err(|e| e.to_string())?;
    let renamed = mx::rename_ground(&netlist, &ground)?;

    // 1. The block again, pins at the edge, ground named, cell named.
    let params = Params {
        pin_edge: true,
        ..meta.params.clone()
    };
    let mut generated = generator::generate(&renamed, meta.generator, &meta.roles, &params, &name)?;
    generated.meta.circuit = Some(input.circuit.clone());
    for port in &generated.meta.ports {
        if !mx::is_identifier(port) {
            return Err(format!(
                "net `{port}` is a pin of the block but cannot name a macro pin; rename it in the circuit (letters, digits and underscores)"
            ));
        }
    }
    let (run_id, run) = layout::run_dir(&p)?;
    let pkg = run.join(format!("{name}.macro"));
    std::fs::create_dir_all(pkg.join("evidence")).map_err(|e| e.to_string())?;
    let gds_path = pkg.join(format!("{name}.gds"));
    generator::draw(
        &env.programs,
        &env.files,
        &generated.plan,
        &gds_path,
        &ctx.cancel,
    )
    .await
    .map_err(|e| e.to_string())?;

    // 2. DRC and LVS on exactly the file that goes into the package.
    let checks = DrcChecks::default();
    let drc = drc::run(
        &env.programs,
        &env.files,
        &gds_path,
        &name,
        checks,
        &run.join("drc"),
        &ctx.cancel,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !drc.clean() {
        return Err(format!(
            "the block drawn for the package is not DRC clean, so nothing was written.\n{}{}",
            drc.to_text(),
            layout::drc_hints(&drc)
                .iter()
                .map(|h| format!("  hint: {h}\n"))
                .collect::<String>()
        ));
    }
    let schematic = lvs::lvs_netlist(
        &renamed,
        &name,
        Some(&generated.meta.devices),
        &generated.meta.dummies,
    )?;
    let lvs_report = lvs::run(
        &env.programs,
        &env.files,
        &gds_path,
        &schematic,
        &run.join("lvs"),
        &ctx.cancel,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !lvs_report.matched {
        return Err(format!(
            "the block drawn for the package does not match its schematic, so nothing was written.\n{}",
            lvs_report.to_text()
        ));
    }

    // 3. Extraction, and the specs before and after layout.
    let ex = extract::run(
        &env.programs,
        &env.files,
        &gds_path,
        &name,
        input.resistance,
        &run.join("ext"),
        &ctx.cancel,
    )
    .await
    .map_err(|e| e.to_string())?;
    let for_bench = extract::rename_net(&ex, &ground, "0")?;
    let post_netlist = extract::post_layout(&netlist, &for_bench, &generated.meta.devices)?;
    let set = input.corners.clone().unwrap_or(CornerSet {
        process: vec!["tt".into()],
        temperatures: vec![27.0],
        supplies: Vec::new(),
    });
    let simulator = input.simulator.as_deref();
    let (pre, sim) = corner_report(
        ws,
        &p,
        &input.circuit,
        netlist.clone(),
        &std_libs,
        &specs,
        &set,
        simulator,
        ctx,
    )
    .await?;
    let (post, _) = corner_report(
        ws,
        &p,
        &input.circuit,
        post_netlist,
        &std_libs,
        &specs,
        &set,
        simulator,
        ctx,
    )
    .await?;
    if !post.failed.is_empty() {
        let (c, why) = &post.failed[0];
        return Err(format!(
            "the post-layout simulation failed at {c}: {why}; nothing was written"
        ));
    }
    if !post.all_pass {
        let failing: Vec<String> = post
            .rows
            .iter()
            .filter(|r| !r.pass)
            .map(|r| r.display.clone())
            .collect();
        return Err(format!(
            "after layout the block does not meet its specs at every corner, and a package never claims a spec it does not meet; nothing was written.\n  {}\nImprove the design or relax the spec, then export again.",
            failing.join("\n  ")
        ));
    }

    // 4. The manifest.
    let mut pins = mx::infer_pins(
        &renamed,
        &generated.meta.devices,
        &generated.meta.ports,
        &ground,
    );
    for (pin, o) in input.pins.iter().flatten() {
        let Some(found) = pins.iter_mut().find(|p| &p.name == pin) else {
            return Err(format!(
                "pins: the block has no pin {pin}; its pins are {}",
                generated.meta.ports.join(", ")
            ));
        };
        if found.is_supply() && (o.direction.is_some() || o.kind.is_some()) {
            return Err(format!(
                "pins: {pin} is a supply; only its description can change"
            ));
        }
        if let Some(d) = o.direction {
            found.direction = match d {
                PinDirection::Input => Direction::Input,
                PinDirection::Output => Direction::Output,
                PinDirection::Inout => Direction::Inout,
            };
        }
        if let Some(k) = o.kind {
            found.kind = match k {
                PinUse::Signal => PinKind::Signal,
                PinUse::Clock => PinKind::Clock,
                PinUse::Analog => PinKind::Analog,
            };
        }
        if let Some(text) = &o.description {
            found.description = Some(text.clone());
        }
    }
    for pin in &mut pins {
        pin.layer = Some("Metal4".into());
    }
    let power: Vec<String> = pins
        .iter()
        .filter(|p| p.kind == PinKind::Power)
        .map(|p| p.name.clone())
        .collect();
    let grounds: Vec<String> = pins
        .iter()
        .filter(|p| p.kind == PinKind::Ground)
        .map(|p| p.name.clone())
        .collect();
    if power.is_empty() || grounds.is_empty() {
        return Err(format!(
            "the block has no {} pin; a macro needs its supplies as pins (pins: {})",
            if power.is_empty() { "power" } else { "ground" },
            generated.meta.ports.join(", ")
        ));
    }
    let voltage = input
        .voltage
        .or_else(|| mx::supply_voltage(&renamed, &power[0], &ground))
        .unwrap_or(1.2);
    let domain = Domain {
        name: "core".into(),
        voltage,
        power,
        ground: grounds,
    };
    let corners = package_corners(&set, &domain, &renamed)?;
    let corner_names: BTreeMap<String, String> = corners
        .iter()
        .map(|(run, c)| (run.clone(), c.name.clone()))
        .collect();
    let mut extra = toml::Table::new();
    let mut ours = toml::Table::new();
    ours.insert("generator".into(), generated.meta.generator.name().into());
    ours.insert("circuit".into(), input.circuit.clone().into());
    ours.insert(
        "params".into(),
        toml::Value::try_from(&generated.meta.params).map_err(|e| e.to_string())?,
    );
    ours.insert(
        "pin_pitch_um".into(),
        (aispice_layout::generator::route::PIN_PITCH as f64 / 1000.0).into(),
    );
    ours.insert("pin_edge".into(), "bottom".into());
    extra.insert("aispice".into(), toml::Value::Table(ours));
    let mut manifest = Manifest {
        schema: SCHEMA_VERSION.into(),
        name: name.clone(),
        kind: MacroKind::Analog,
        description: input.description.clone().or_else(|| {
            Some(format!(
                "{} block from {}, laid out by aispice",
                generated.meta.generator.name(),
                input.circuit
            ))
        }),
        pdk: pkg_pdk.clone(),
        producer: Producer {
            tool: "aispice".into(),
            version: VERSION.into(),
            source: Some(input.circuit.clone()),
        },
        size: Size {
            width_um: 0.0,
            height_um: 0.0,
        },
        domains: vec![domain],
        pins,
        corners: corners.into_iter().map(|(_, c)| c).collect(),
        extra: Some(extra),
    };

    // 5. The views. The LEF's size is the package's.
    let (lef_text, lef) = mx::lef(&env.programs, &env.files, &gds_path, &manifest, &ctx.cancel)
        .await
        .map_err(|e| e.to_string())?;
    let (w, h) = lef.size.ok_or("Magic's LEF has no SIZE")?;
    manifest.size = Size {
        width_um: w,
        height_um: h,
    };
    let pin_names: Vec<String> = manifest.pins.iter().map(|p| p.name.clone()).collect();
    let caps = mx::pin_caps(&ex.text, &pin_names);
    let definitions: BTreeMap<String, String> = spec_text
        .lines()
        .filter_map(|l| {
            let (n, _) = l.split_once('=')?;
            let n = n.trim();
            specs
                .iter()
                .any(|s| s.name == n)
                .then(|| (n.to_string(), l.trim().to_string()))
        })
        .collect();
    let entries = spec_entries(&pre, &post, &corner_names, &definitions);
    write(&pkg, &format!("{name}.lef"), &lef_text)?;
    write(
        &pkg,
        &format!("{name}.lib"),
        &mx::liberty(&manifest, &caps, VERSION),
    )?;
    write(
        &pkg,
        &format!("{name}.v"),
        &mx::verilog(
            &manifest,
            input.verilog == Some(VerilogModel::Wreal),
            VERSION,
        ),
    )?;
    let spice_path = write(
        &pkg,
        &format!("{name}.spice"),
        &mx::spice(&schematic, &manifest, VERSION)?,
    )?;
    let pex_path = write(
        &pkg,
        &format!("{name}.pex.spice"),
        &format!(
            "* {name}: netlist extracted by Magic from {name}.gds ({}), written by aispice {VERSION}\n{}",
            if input.resistance {
                "capacitance and resistance"
            } else {
                "capacitance"
            },
            ex.text
        ),
    )?;
    let specs_path = write(
        &pkg,
        &format!("{name}.specs.toml"),
        &mx::specs_toml(&entries, VERSION)?,
    )?;
    let manifest_text = manifest.to_toml().map_err(|e| e.to_string())?;
    write(
        &pkg,
        MANIFEST,
        &format!(
            "#:schema https://raw.githubusercontent.com/danieltyukov/chipleap/main/src/chipleap/macro/macro-v1.schema.json\n{manifest_text}"
        ),
    )?;

    // 6. Evidence, hashing the package files each check read.
    let (klayout, magic) = futures::join!(
        version_of("klayout", &env.programs, false, ctx),
        version_of("magic", &env.programs, true, ctx)
    );
    let evidence = |op: &str, summary: String, tool: &(String, String), inputs: &[PathBuf]| {
        Evidence::new(
            &pkg,
            op,
            Status::Pass,
            &summary,
            (tool.0.as_str(), tool.1.as_str()),
            &pkg_pdk,
            inputs,
        )
        .map_err(|e| e.to_string())
    };
    evidence(
        "drc",
        format!(
            "IHP KLayout DRC decks {}: 0 violations",
            drc.decks.join(", ")
        ),
        &klayout,
        std::slice::from_ref(&gds_path),
    )?
    .with_data(json!({"decks": drc.decks, "violations": 0}))
    .write(&pkg)
    .map_err(|e| e.to_string())?;
    evidence(
        "lvs",
        format!(
            "IHP KLayout LVS deck: the layout matches {name}.spice ({} devices)",
            schematic.devices.len()
        ),
        &klayout,
        &[gds_path.clone(), spice_path.clone()],
    )?
    .with_data(json!({"devices": schematic.devices, "matched": true}))
    .write(&pkg)
    .map_err(|e| e.to_string())?;
    evidence(
        "pex",
        format!(
            "Magic extraction: {} device fingers, {} capacitors ({:.1} fF), {} resistors",
            ex.devices, ex.capacitors, ex.total_cap_ff, ex.resistors
        ),
        &magic,
        std::slice::from_ref(&gds_path),
    )?
    .with_artifacts(&pkg, std::slice::from_ref(&pex_path))
    .map_err(|e| e.to_string())?
    .write(&pkg)
    .map_err(|e| e.to_string())?;
    let simulator_name = sim.map_or("unknown".to_string(), |s| s.name().to_string());
    evidence(
        "sim",
        format!(
            "{} specs at {} corners after layout, every value within its limits",
            post.rows.len(),
            post.corners.len()
        ),
        &(simulator_name.clone(), String::from("-")),
        std::slice::from_ref(&pex_path),
    )?
    .with_artifacts(&pkg, std::slice::from_ref(&specs_path))
    .map_err(|e| e.to_string())?
    .with_data(json!({"corners": post.corners, "simulator": simulator_name}))
    .write(&pkg)
    .map_err(|e| e.to_string())?;

    // 7. The package must pass the checks a chip will run on it.
    let report = check_package(
        &pkg,
        &CheckOptions {
            pdk: Some(format!("{}@{commit}", pdk.name)),
            pdk_cells,
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    if !report.passed {
        return Err(format!(
            "the package does not pass its own check, so nothing was written; this is a bug in macro export, please report it.\n{}",
            report.to_text()
        ));
    }
    let files = package_files(&pkg, &folder)?;
    may_replace(&p, &folder, &files)?;

    let mut text = format!(
        "{name}: {} pins ({}), {:.2} x {:.2} um, PDK {} at {}.\n",
        manifest.pins.len(),
        manifest
            .pins
            .iter()
            .map(|p| format!("{} {} {}", p.name, p.direction.as_str(), p.kind.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
        w,
        h,
        pdk.name,
        &commit[..commit.len().min(12)]
    );
    text.push_str(&format!(
        "DRC clean ({}), LVS clean, extracted ({:.1} fF), {} specs at {} corners pass after layout:\n",
        drc.decks.join(", "),
        ex.total_cap_ff,
        post.rows.len(),
        post.corners.len()
    ));
    text.push_str(&super::sim_tools::pre_post_text(&pre.worst, &post.worst));
    text.push_str(&format!(
        "check_macro: {} error(s), {} warning(s). Reports in .aispice/layout/{run_id}.\n",
        report.errors, report.warnings
    ));
    for f in report.findings.iter().take(10) {
        text.push_str(&format!("  {}\n", f.to_text()));
    }
    let data = json!({
        "package": folder,
        "manifest": serde_json::to_value(&manifest).unwrap_or_default(),
        "check": report,
        "pre": super::analysis::corner_json(&pre),
        "post": super::analysis::corner_json(&post),
        "layout": layout_rel,
    });
    Ok(Built {
        folder,
        files,
        text,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(names: &[&str], values: Vec<Option<f64>>) -> CornerReport {
        let row = aispice_sim::corner::CornerRow {
            name: "gbw".into(),
            unit: "Hz".into(),
            min: Some(1e6),
            max: None,
            values,
            worst_value: None,
            worst_margin: None,
            worst_corner: None,
            pass: true,
            display: String::new(),
        };
        CornerReport {
            corners: names.iter().map(|s| s.to_string()).collect(),
            failed: Vec::new(),
            rows: vec![row],
            worst: aispice_sim::spec::SpecReport {
                rows: Vec::new(),
                all_pass: true,
                summary: String::new(),
            },
            worst_corner: None,
            all_pass: true,
            summary: String::new(),
        }
    }

    #[test]
    fn spec_entries_keep_limits_after_layout_only() {
        let pre = report(&["tt/27C", "ss/125C"], vec![Some(5e7), Some(4e7)]);
        let post = report(&["tt/27C", "ss/125C"], vec![Some(4.5e7), None]);
        let names: BTreeMap<String, String> = [("tt/27C", "tt_27"), ("ss/125C", "ss_125")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let e = spec_entries(&pre, &post, &names, &BTreeMap::new());
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, "gbw");
        assert_eq!(e[0].min, Some(1e6));
        assert_eq!(
            e[0].measured.len(),
            1,
            "a corner without a value is left out"
        );
        assert_eq!(e[1].name, "gbw_schematic");
        assert_eq!(e[1].min, None);
        assert_eq!(e[1].measured["ss_125"], 4e7);
    }

    #[test]
    fn corners_become_identifiers_with_domain_voltages() {
        let set = CornerSet {
            process: vec!["tt".into(), "ss".into()],
            temperatures: vec![27.0, -40.0],
            supplies: vec![aispice_sim::corner::Supply {
                target: "VDD".into(),
                values: vec![1.08],
            }],
        };
        let n = aispice_core::netlist::parse("t\nVDD vdd vss 1.2\nR1 vdd vss 1k\n.op\n");
        let domain = Domain {
            name: "core".into(),
            voltage: 1.2,
            power: vec!["vdd".into()],
            ground: vec!["vss".into()],
        };
        let c = package_corners(&set, &domain, &n).unwrap();
        let names: Vec<&str> = c.iter().map(|(_, c)| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "tt_27_vdd_1v08",
                "tt_m40_vdd_1v08",
                "ss_27_vdd_1v08",
                "ss_m40_vdd_1v08"
            ]
        );
        assert_eq!(c[0].1.voltages["core"], 1.08);
        assert_eq!(c[3].1.temperature, -40.0);
    }
}
