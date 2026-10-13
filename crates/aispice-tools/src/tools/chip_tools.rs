//! `chip`: an analog-on-top chip from macro packages and IHP's pad ring.
//!
//! `plan` reads the chip description and its packages and lays the chip
//! out on paper (floorplan and routes) without running a layout program or
//! writing a file. `assemble` draws it with KLayout, fills it with Magic,
//! runs full-chip DRC and LVS, and writes the chip, its LVS netlist, a
//! report and evidence into the project in one approved, undoable write.
//!
//! Packages are untrusted: each must lie in the project (no link out of
//! it), pass `check_package` against the project's PDK, and is then read
//! by file names made from its validated manifest, never paths it names.

use super::schematic_tools::spec;
use crate::layout;
use crate::project::Project;
use crate::workspace::{FileWrite, Workspace};
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::macropkg::lef::parse_lef;
use aispice_core::macropkg::{
    CheckOptions, Evidence, Manifest, Pdk, PdkLibs, Status, check_package,
};
use aispice_layout::chip::assemble::{self, Assembled, Block, ChipPlan};
use aispice_layout::chip::floorplan::{MAX_DIE, MacroGeom};
use aispice_layout::chip::geom::LayerRect;
use aispice_layout::chip::io::IoLibrary;
use aispice_layout::chip::netlist::{self, BlockNetlist};
use aispice_layout::chip::spec::{ChipSpec, MAX_BYTES};
use aispice_layout::drc::DrcReport;
use aispice_layout::gds;
use aispice_layout::lvs::LvsReport;
use aispice_layout::macro_export as mx;
use aispice_layout::tools as ltools;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A block's LEF and SPICE views are not read past this size.
const MAX_VIEW_BYTES: u64 = 64 << 20;
/// Nor its GDS past this (the macro package limit).
const MAX_GDS_BYTES: u64 = 256 << 20;
/// The cell in which KLayout stores pcell and library context in a GDS.
const CONTEXT_INFO: &str = "$$$CONTEXT_INFO$$$";

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChipAction {
    /// Lay the chip out on paper: floorplan and routes, no files.
    Plan,
    /// Draw, fill and check the chip, and write it into the project.
    Assemble,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChipInput {
    pub action: ChipAction,
    /// The chip description: a `.chip.toml` file of the project.
    pub spec: String,
    /// assemble: the folder the results go to; default `<name>.chip`
    /// beside the description.
    #[serde(default)]
    pub out: Option<String>,
    /// assemble: run full-chip DRC and LVS (default true).
    #[serde(default = "yes")]
    pub check: bool,
}

fn yes() -> bool {
    true
}

const DESCRIPTION: &str = r#"
Assemble an analog-on-top chip on the project's PDK (IHP SG13G2) from macro packages: aispice blocks exported with macro_export, and hardened digital blocks (ChipLeap packages). The chip description is a TOML file in the project: `name`, optional `pad_pitch_um` (default 100), `[[macros]]` with `instance`, `package` (a .macro folder of the project), optional `at_um = [x, y]` in the core and `nets = { pin = "net" }` (pins not listed join the net of their own name), and `[[pads]]` with `net`, `side` (north, east, south, west) and `kind`: analog (sg13g2_IOPadAnalog, its clamps and diodes; `resistor = true` connects through its secondary protection), input (sg13g2_IOPadIn), output (sg13g2_IOPadOut4mA), vdd, vss (core supplies), iovdd, iovss (the IO ring's supplies; at least one pad of each supply kind). Every package is checked with check_macro's rules against the project's PDK before use. `plan` gives the floorplan (die size, pad ring with corners and fillers, bond pads, seal ring, block placement) and the top-level routes (Metal3 and Metal4 on a 0.6 um grid, clean by construction) without running anything. `assemble` draws the chip with KLayout and IHP's pcells (bond pads, seal ring), adds Magic's metal and active fill, runs IHP's DRC decks (main, extra, antenna, density) on the finished chip and IHP's LVS deck on the chip without its pad cells against a netlist of the blocks with the pad nets as ports, then writes <name>.gds, <name>.lvs.spice, <name>.chip.json and DRC and LVS evidence into the output folder. When a block does not fit or a net cannot be routed, move blocks with at_um, change pad sides or pitch, and plan again.
"#;

pub struct ChipTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for ChipTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<ChipInput>("chip", DESCRIPTION)
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: ChipInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let loaded = match load(&self.ws, &input).await {
            Ok(l) => l,
            Err(e) => return ToolOutput::error(e),
        };
        let plan = match assemble::plan(&loaded.spec, &loaded.io, &loaded.geoms) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(format!("{}: {e}", input.spec)),
        };
        let mut text = plan_text(&loaded, &plan);
        let mut data = json!({
            "chip": loaded.spec.name,
            "die_um": [plan.floorplan.die.width() as f64 / 1000.0, plan.floorplan.die.height() as f64 / 1000.0],
            "routes": plan.routed.nets,
            "unconnected": plan.routed.single,
            "warnings": loaded.warnings,
        });
        if matches!(input.action, ChipAction::Plan) {
            text.push_str("Nothing was drawn or written; assemble draws and checks it.\n");
            return ToolOutput::text(text).with_data(json!({"kind": "generic", "value": data}));
        }
        match self.assemble(&loaded, &plan, &input, ctx).await {
            Ok((t, d)) => {
                text.push_str(&t);
                data["result"] = d;
                ToolOutput::text(crate::layout::noted(text).await)
                    .with_data(json!({"kind": "generic", "value": data}))
            }
            Err(e) => ToolOutput::error(format!("{text}{e}")),
        }
    }
}

/// A chip description with what it needs, read and checked.
struct Loaded {
    spec: ChipSpec,
    spec_rel: String,
    io: IoLibrary,
    geoms: Vec<MacroGeom>,
    blocks: Vec<Block>,
    run: PathBuf,
    run_id: String,
    pdk: Pdk,
    warnings: Vec<String>,
}

/// Read a file of a package, refusing one past `limit` or that is not a
/// plain file.
fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a plain file", path.display()));
    }
    if meta.len() > limit {
        return Err(format!(
            "{} is larger than {} MiB",
            path.display(),
            limit >> 20
        ));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// A package folder of the project whose every entry stays inside it.
fn package_dir(p: &Project, rel: &str) -> Result<PathBuf, String> {
    let dir = p.resolve(rel).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err(format!("{rel} is not a folder"));
    }
    for sub in [dir.clone(), dir.join("evidence")] {
        if let Ok(list) = std::fs::read_dir(&sub) {
            for e in list.filter_map(Result::ok) {
                if let Ok(real) = e.path().canonicalize()
                    && p.check_real(&real, rel).is_err()
                {
                    return Err(format!(
                        "{} leads outside the project folder or into .aispice; refusing to read it",
                        e.path().display()
                    ));
                }
            }
        }
    }
    Ok(dir)
}

async fn load(ws: &Workspace, input: &ChipInput) -> Result<Loaded, String> {
    let env = layout::env(ws)?;
    let p = env.project.clone();
    let path = p.resolve(&input.spec).map_err(|e| e.to_string())?;
    let text = String::from_utf8(read_capped(&path, MAX_BYTES)?)
        .map_err(|_| format!("{} is not UTF-8 text", input.spec))?;
    let spec = ChipSpec::parse(&text).map_err(|e| format!("{}: {e}", input.spec))?;
    let pdk = ws
        .runner
        .project_pdk(&p)
        .map_err(|e| e.to_string())?
        .ok_or("this project names no PDK")?;
    let commit = pdk
        .commit
        .clone()
        .ok_or("the PDK's commit is unknown (its folder is not a git checkout); a chip checks every package against it")?;
    let io = IoLibrary::load(&env.files.root)?;
    let (run_id, run) = layout::run_dir(&p)?;
    let blocks_dir = run.join("blocks");
    std::fs::create_dir_all(&blocks_dir).map_err(|e| e.to_string())?;
    let mut geoms = Vec::new();
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    // Names compare without case, as SPICE compares them.
    let pdk_libs = PdkLibs::load(&env.files.root)?;
    let pdk_cells = pdk_libs.names.clone();
    if let Some(why) = mx::pdk_name_clash(&spec.name, &pdk_cells) {
        return Err(format!("the chip's name: {why}"));
    }
    let mut cells: BTreeMap<String, String> = BTreeMap::new();
    let opts = CheckOptions {
        pdk: Some(format!("{}@{commit}", pdk.name)),
        pdk_libs: Some(pdk_libs),
        ..Default::default()
    };
    for m in &spec.macros {
        let dir = package_dir(&p, &m.package)?;
        let report = check_package(&dir, &opts).map_err(|e| format!("{}: {e}", m.package))?;
        if !report.passed {
            return Err(format!(
                "{} does not pass the macro package checks, so the chip does not use it:\n{}",
                m.package,
                report.to_text()
            ));
        }
        for f in &report.findings {
            warnings.push(format!("{}: {}", m.package, f.to_text()));
        }
        let manifest = Manifest::load(&dir).map_err(|e| e.to_string())?;
        let name = manifest.name.clone();
        if name.eq_ignore_ascii_case(&spec.name) {
            return Err(format!(
                "{}: the block's name {name} is the chip's (names compare without case, as in SPICE)",
                m.package
            ));
        }
        if let Some(why) = mx::pdk_name_clash(&name, &pdk_cells) {
            return Err(format!("{}: {why}", m.package));
        }
        if let Some(other) = cells.insert(name.to_ascii_lowercase(), m.package.clone())
            && other != m.package
        {
            return Err(format!(
                "{other} and {} are both called {name} (names compare without case, as in SPICE); a chip cannot hold two different cells of one name",
                m.package
            ));
        }
        let lef_text = String::from_utf8(read_capped(
            &dir.join(manifest.file("lef")),
            MAX_VIEW_BYTES,
        )?)
        .map_err(|_| format!("{}: the LEF is not text", m.package))?;
        let lef = parse_lef(&lef_text)
            .map_err(|e| format!("{}: {e}", m.package))?
            .remove(&name)
            .ok_or_else(|| format!("{}: the LEF has no macro {name}", m.package))?;
        let (w, h) = lef
            .size
            .ok_or_else(|| format!("{}: the LEF has no SIZE", m.package))?;
        // Numbers no block on a die aispice lays out has are refused before
        // anything is sized from them (or they saturate on the way to nm).
        let limit = MAX_DIE as f64 / 1000.0;
        let within = |v: f64| v.is_finite() && v.abs() <= limit;
        if !(within(w) && within(h) && w > 0.0 && h > 0.0) {
            return Err(format!(
                "{}: the LEF's SIZE {w} BY {h} um is not a block's (at most {} mm a side)",
                m.package,
                MAX_DIE / 1_000_000
            ));
        }
        let pins_within = lef
            .pins
            .values()
            .flat_map(|p| &p.rects)
            .all(|r| r.rect.iter().all(|v| within(*v)));
        if !(within(lef.origin.0) && within(lef.origin.1) && pins_within) {
            return Err(format!(
                "{}: the LEF's ORIGIN or pins lie more than {} mm away",
                m.package,
                MAX_DIE / 1_000_000
            ));
        }
        let nm = |v: f64| (v * 1000.0).round() as i64;
        let mut pins = BTreeMap::new();
        for (pin, lp) in &lef.pins {
            pins.insert(
                pin.clone(),
                lp.rects
                    .iter()
                    .filter_map(|r| LayerRect::from_lef(r, (0.0, 0.0)))
                    .collect::<Vec<_>>(),
            );
        }
        let geom = MacroGeom {
            instance: m.instance.clone(),
            cell: name.clone(),
            size: (nm(w), nm(h)),
            origin: (nm(lef.origin.0), nm(lef.origin.1)),
            pins,
            at: m.at_um.map(|[x, y]| (nm(x), nm(y))),
        };
        // The layout KLayout reads is a copy aispice made, not the package's
        // file, which could change in between.
        let gds = blocks_dir.join(format!("{name}.gds"));
        if !gds.exists() {
            let bytes = read_capped(&dir.join(manifest.file("gds")), MAX_GDS_BYTES)?;
            // KLayout rebuilds pcells and library cells from a GDS's context
            // information, with parameters the file chooses, after the PDK's
            // pcell library is loaded: the chip would hold geometry the
            // package checks never saw.
            let lib = gds::parse(&bytes).map_err(|e| format!("{}: {e}", m.package))?;
            if lib.cells.keys().any(|c| c.starts_with(CONTEXT_INFO)) {
                return Err(format!(
                    "{}: the GDS holds KLayout context information ({CONTEXT_INFO}), from which KLayout would rebuild pcells; write it without (KLayout's write_context_info = false)",
                    m.package
                ));
            }
            std::fs::write(&gds, bytes).map_err(|e| e.to_string())?;
        }
        let spice_name = manifest.file("spice");
        let spice = String::from_utf8(read_capped(&dir.join(&spice_name), MAX_VIEW_BYTES)?)
            .map_err(|_| format!("{}: the SPICE netlist is not text", m.package))?;
        let block_netlist: BlockNetlist = netlist::block_netlist(&name, &spice, &spice_name)?;
        geom.check().map_err(|e| format!("{}: {e}", m.package))?;
        geoms.push(geom.clone());
        blocks.push(Block {
            geom,
            gds,
            netlist: block_netlist,
        });
    }
    Ok(Loaded {
        spec,
        spec_rel: input.spec.clone(),
        io,
        geoms,
        blocks,
        run,
        run_id,
        pdk: Pdk {
            name: pdk.name.clone(),
            commit,
        },
        warnings,
    })
}

fn um(nm: i64) -> f64 {
    nm as f64 / 1000.0
}

fn plan_text(l: &Loaded, plan: &ChipPlan) -> String {
    let fp = &plan.floorplan;
    let mut t = format!(
        "{}: die {:.0} x {:.0} um, core {:.0} x {:.0} um, {} pads, {} IO ring cells, {} blocks.\n",
        l.spec.name,
        um(fp.die.width()),
        um(fp.die.height()),
        um(fp.core.width()),
        um(fp.core.height()),
        fp.pads.len(),
        fp.ring.len(),
        fp.macros.len()
    );
    for m in &fp.macros {
        t.push_str(&format!(
            "  block {} ({}) at ({:.1}, {:.1}) um in the core, {:.1} x {:.1} um\n",
            m.instance,
            m.cell,
            um(m.bbox.x1 - fp.core.x1),
            um(m.bbox.y1 - fp.core.y1),
            um(m.bbox.width()),
            um(m.bbox.height())
        ));
    }
    for r in &plan.routed.nets {
        t.push_str(&format!(
            "  net {}: {} ({:.1} um, {} vias)\n",
            r.net,
            r.terminals.join(", "),
            r.length_um,
            r.vias
        ));
    }
    for n in &plan.routed.single {
        t.push_str(&format!("  net {n}: one pin only, not routed\n"));
    }
    for w in &l.warnings {
        t.push_str(&format!("  warning: {w}\n"));
    }
    t
}

/// Refuse to write over something the chip tool did not make. The folder
/// inspected is the one the output checks resolve, which the files are then
/// written into, not the path as given.
fn may_replace(p: &Project, folder: &str, name: &str) -> Result<(), String> {
    let Some(dir) = p.existing_output_dir(folder).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    let ours = dir.join(format!("{name}.chip.json"));
    let empty = std::fs::read_dir(&dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if empty || std::fs::symlink_metadata(&ours).is_ok_and(|m| m.is_file()) {
        Ok(())
    } else {
        Err(format!(
            "{folder} exists and was not made by the chip tool; choose another folder with `out`"
        ))
    }
}

impl ChipTool {
    async fn assemble(
        &self,
        l: &Loaded,
        plan: &ChipPlan,
        input: &ChipInput,
        ctx: &ToolContext,
    ) -> Result<(String, Value), String> {
        let env = layout::env(&self.ws)?;
        let p = env.project.clone();
        // Trimmed once, so every later use names the same folder.
        let folder = input
            .out
            .as_deref()
            .map(str::trim)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let dir = Path::new(&l.spec_rel)
                    .parent()
                    .map(|d| d.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                if dir.is_empty() {
                    format!("{}.chip", l.spec.name)
                } else {
                    format!("{dir}/{}.chip", l.spec.name)
                }
            });
        p.missing_dirs(&format!("{folder}/x"))
            .map_err(|e| e.to_string())?;
        may_replace(&p, &folder, &l.spec.name)?;
        let out = l.run.join("chip");
        let started = std::time::Instant::now();
        let chip: Assembled = assemble::assemble(
            &env.programs,
            &env.files,
            &l.spec,
            plan,
            &l.io,
            &l.blocks,
            &out,
            &ctx.cancel,
        )
        .await
        .map_err(|e| e.to_string())?;
        let drawn = started.elapsed().as_secs_f64();
        let blocks: BTreeMap<String, BlockNetlist> = l
            .blocks
            .iter()
            .map(|b| (b.geom.instance.clone(), b.netlist.clone()))
            .collect();
        let needs_cells = blocks.values().any(|b| !b.external.is_empty());
        let stdcells = if needs_cells {
            Some(
                String::from_utf8(read_capped(&env.files.stdcell_cdl, MAX_VIEW_BYTES)?)
                    .map_err(|_| "the PDK's standard-cell netlist is not text".to_string())?,
            )
        } else {
            None
        };
        let lvs_text = netlist::chip_netlist(&l.spec, &blocks, stdcells.as_deref(), VERSION)?;
        let lvs_path = out.join(format!("{}.lvs.spice", l.spec.name));
        std::fs::write(&lvs_path, &lvs_text).map_err(|e| e.to_string())?;
        let mut t = format!("Drawn and filled in {drawn:.0} s.\n");
        let mut result = json!({"gds": format!("{folder}/{}.gds", l.spec.name)});
        let mut checks: Option<(DrcReport, LvsReport)> = None;
        if input.check {
            let started = std::time::Instant::now();
            let (drc, lvs) = assemble::check(
                &env.programs,
                &env.files,
                &l.spec,
                &chip,
                &lvs_text,
                &l.run,
                &ctx.cancel,
            )
            .await
            .map_err(|e| e.to_string())?;
            t.push_str(&format!(
                "Checked in {:.0} s.\n{}{}",
                started.elapsed().as_secs_f64(),
                drc.to_text(),
                lvs.to_text()
            ));
            result["drc"] = json!(drc);
            result["lvs"] = json!(lvs);
            checks = Some((drc, lvs));
        }

        // Evidence about exactly the files written.
        let klayout = ltools::version(&env.programs, false, &ctx.cancel).await;
        let mut files = vec![
            (format!("{}.gds", l.spec.name), chip.gds.clone()),
            (format!("{}.lvs.spice", l.spec.name), lvs_path.clone()),
        ];
        if let Some((drc, lvs)) = &checks {
            let status = |ok: bool| if ok { Status::Pass } else { Status::Fail };
            let drc_ev = Evidence::new(
                &out,
                "drc",
                status(drc.clean()),
                &format!(
                    "IHP KLayout DRC decks {}: {} violations",
                    drc.decks.join(", "),
                    drc.total()
                ),
                ("klayout", &klayout),
                &l.pdk,
                std::slice::from_ref(&chip.gds),
            )
            .map_err(|e| e.to_string())?
            .with_data(json!({"rules": drc.rules}));
            let lvs_ev = Evidence::new(
                &out,
                "lvs",
                status(lvs.matched),
                &if lvs.matched {
                    "IHP KLayout LVS deck: the chip without its pad cells matches its netlist"
                        .to_string()
                } else {
                    format!("IHP KLayout LVS deck: {} mismatches", lvs.problems().len())
                },
                ("klayout", &klayout),
                &l.pdk,
                &[chip.gds.clone(), lvs_path.clone()],
            )
            .map_err(|e| e.to_string())?;
            for ev in [drc_ev, lvs_ev] {
                let path = ev.write(&out).map_err(|e| e.to_string())?;
                files.push((format!("evidence/{}.json", ev.operation), path));
            }
        }
        let report = json!({
            "chip": l.spec.name,
            "producer": {"tool": "aispice", "version": VERSION},
            "pdk": l.pdk,
            "spec": l.spec_rel,
            "floorplan": plan.floorplan,
            "routes": plan.routed.nets,
            "unconnected": plan.routed.single,
            "result": result,
        });
        let report_path = out.join(format!("{}.chip.json", l.spec.name));
        std::fs::write(
            &report_path,
            serde_json::to_string_pretty(&report).unwrap_or_default() + "\n",
        )
        .map_err(|e| e.to_string())?;
        files.push((format!("{}.chip.json", l.spec.name), report_path));
        let mut writes = Vec::new();
        for (name, path) in &files {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            writes.push(FileWrite {
                rel: format!("{folder}/{name}"),
                bytes,
            });
        }
        self.ws
            .write_files(
                self,
                &writes,
                &format!("Chip {}", l.spec.name),
                &format!("{} files into {folder}", writes.len()),
            )
            .await?;
        t.push_str(&format!(
            "Wrote {folder}: {} (undo with the history tool). Reports in .aispice/layout/{}.\n",
            files
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            l.run_id
        ));
        Ok((t, result))
    }
}
