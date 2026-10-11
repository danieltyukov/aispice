//! Putting a chip together: the plan (floorplan, terminals, routes), the
//! drawing (one KLayout run), fill (Magic) and the checks (IHP's DRC decks
//! on the finished chip, its LVS deck on the chip without the pad cells).

use super::floorplan::{self, BOND_SIZE, Floorplan, MacroGeom, MacroPlaced, SEAL_EDGE};
use super::geom::{Facing, METAL1, METAL2, METAL3, METAL4, Rect, TEXT};
use super::io::IoLibrary;
use super::netlist::BlockNetlist;
use super::route::{self, Problem, Routed, Terminal};
use super::spec::{ChipSpec, MacroUse};
use crate::drc::{self, DrcChecks, DrcReport};
use crate::lvs::{self, LvsNetlist, LvsReport};
use crate::tools::{self, LayoutError, PdkLayoutFiles, Programs, RunDir};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

const CHIP_SCRIPT: &str = include_str!("../../klayout/chip.py");
const MERGE_SCRIPT: &str = include_str!("../../klayout/merge.py");
const FILL_SCRIPT: &str = include_str!("../../magic/fill.tcl");
/// Datatype of pin shapes on metal layers.
const PIN: i16 = 2;

/// A block for the chip: where it is drawn from and how LVS sees it.
#[derive(Debug, Clone)]
pub struct Block {
    pub geom: MacroGeom,
    /// Its layout, copied into the run folder.
    pub gds: PathBuf,
    pub netlist: BlockNetlist,
}

/// Everything decided before anything is drawn.
#[derive(Debug, Clone, Serialize)]
pub struct ChipPlan {
    pub floorplan: Floorplan,
    pub terminals: Vec<Terminal>,
    pub routed: Routed,
}

/// Which way a pin of a placed block leaves it: the side of the block's
/// outline its shape touches.
fn edge_facing(r: &Rect, outline: &Rect) -> Option<Facing> {
    if r.y1 == outline.y1 {
        Some(Facing::South)
    } else if r.y2 == outline.y2 {
        Some(Facing::North)
    } else if r.x1 == outline.x1 {
        Some(Facing::West)
    } else if r.x2 == outline.x2 {
        Some(Facing::East)
    } else {
        None
    }
}

/// The terminals of a placed block: for each pin, a shape on Metal2 to
/// Metal4 on the block's edge.
pub fn block_terminals(m: &MacroPlaced, mu: &MacroUse) -> Result<Vec<Terminal>, String> {
    let mut out = Vec::new();
    for (pin, rects) in &m.pins {
        let found = [METAL4, METAL3, METAL2, METAL1].iter().find_map(|l| {
            rects
                .iter()
                .filter(|r| r.layer == *l)
                .find_map(|r| edge_facing(&r.rect, &m.bbox).map(|f| (r.clone(), f)))
        });
        let Some((shape, facing)) = found else {
            return Err(format!(
                "pin {pin} of block {} has no shape on Metal1 to Metal4 at the block's edge, where the chip router reaches pins (aispice blocks: export them with macro_export, which puts every pin there)",
                m.instance
            ));
        };
        out.push(Terminal {
            net: ChipSpec::net_of(mu, pin).to_string(),
            name: format!("{}.{pin}", m.instance),
            pin: shape,
            facing,
            owner: m.bbox,
        });
    }
    Ok(out)
}

/// Every terminal: the core pins and supply straps of the pads, and the
/// blocks' pins.
pub fn terminals(spec: &ChipSpec, fp: &Floorplan) -> Result<Vec<Terminal>, String> {
    let mut out = Vec::new();
    for p in &fp.pads {
        if let Some(core) = &p.core {
            out.push(Terminal {
                net: p.net.clone(),
                name: format!("pad {} ({})", p.net, p.cell),
                pin: core.clone(),
                facing: p.inward,
                owner: p.bbox,
            });
        }
    }
    for (m, mu) in fp.macros.iter().zip(&spec.macros) {
        out.extend(block_terminals(m, mu)?);
    }
    Ok(out)
}

/// Plan a chip: floorplan, terminals and routes. Pure: no tool runs.
pub fn plan(spec: &ChipSpec, io: &IoLibrary, blocks: &[MacroGeom]) -> Result<ChipPlan, String> {
    let fp = floorplan::plan(spec, io, blocks)?;
    let terminals = terminals(spec, &fp)?;
    let problem = Problem {
        area: fp.core,
        blocks: fp.macros.iter().map(|m| m.bbox).collect(),
        terminals: terminals.clone(),
    };
    let routed = route::route(&problem)?;
    let problems = route::check(&routed, &terminals);
    if !problems.is_empty() {
        return Err(format!(
            "the router's result fails its own check; this is a bug, please report it: {}",
            problems.join("; ")
        ));
    }
    Ok(ChipPlan {
        floorplan: fp,
        terminals,
        routed,
    })
}

/// The pad nets that reach the core, which are the chip's LVS ports.
pub fn ports(spec: &ChipSpec) -> Vec<String> {
    let set: BTreeSet<&str> = spec
        .pads
        .iter()
        .filter(|p| p.kind.in_core())
        .map(|p| p.net.as_str())
        .collect();
    set.into_iter().map(str::to_string).collect()
}

/// The plan as `chip.py` reads it.
pub fn draw_plan(
    spec: &ChipSpec,
    plan: &ChipPlan,
    io: &IoLibrary,
    blocks: &[Block],
    out: &Path,
    lvs_out: &Path,
) -> Value {
    let fp = &plan.floorplan;
    let io_cells: BTreeSet<&str> = fp.ring.iter().map(|c| c.cell.as_str()).collect();
    let mut libraries = vec![json!({"file": io.gds, "cells": io_cells})];
    for b in blocks {
        libraries.push(json!({"file": b.gds, "cells": [b.geom.cell]}));
    }
    let place = |cell: &str, t: &super::geom::Trans| json!({"cell": cell, "rot": t.rot, "mirror": t.mirror, "x": t.dx, "y": t.dy});
    let mut instances: Vec<Value> = fp.ring.iter().map(|c| place(&c.cell, &c.trans)).collect();
    for m in &fp.macros {
        instances.push(place(&m.cell, &m.trans));
    }
    let um = |nm: i64| format!("{}u", nm as f64 / 1000.0);
    let mut pcells: Vec<Value> = fp
        .pads
        .iter()
        .map(|p| {
            json!({"pcell": "bondpad", "params": {"shape": "square", "diameter": um(BOND_SIZE)},
                   "x": p.bond.0, "y": p.bond.1})
        })
        .collect();
    pcells.push(json!({"pcell": "sealring",
        "params": {"l": um(fp.die.width() - 2 * SEAL_EDGE), "w": um(fp.die.height() - 2 * SEAL_EDGE)},
        "x": 0, "y": 0}));
    let mut shapes: Vec<Value> = plan
        .routed
        .shapes
        .iter()
        .map(|(_, s)| json!([s.layer, 0, s.rect.x1, s.rect.y1, s.rect.x2, s.rect.y2]))
        .collect();
    let mut texts = Vec::new();
    // Each pad's net is named where its stub meets the pad, at the top
    // level: LVS finds the chip's ports by these labels.
    for (net, name, layer, (x, y)) in &plan.routed.pin_points {
        if !name.starts_with("pad ") {
            continue;
        }
        texts.push(json!([layer, TEXT, net, x, y]));
        texts.push(json!([layer, PIN, net, x, y]));
        let r = Rect::new(x - 100, y - 100, x + 100, y + 100);
        shapes.push(json!([layer, PIN, r.x1, r.y1, r.x2, r.y2]));
    }
    let _ = spec;
    json!({
        "top": spec.name,
        "libraries": libraries,
        "instances": instances,
        "pcells": pcells,
        "shapes": shapes,
        "texts": texts,
        "out": out,
        "lvs_out": lvs_out,
        "lvs_drop": io_cells,
    })
}

/// The chip's files.
#[derive(Debug, Clone, Serialize)]
pub struct Assembled {
    /// The finished chip: seal ring and fill included.
    pub gds: PathBuf,
    /// The chip without its pad cells, bond pads, seal ring and fill.
    pub lvs_view: PathBuf,
}

fn failed(what: &str, out: &aispice_sim::backend::process::ProcessOutput) -> LayoutError {
    LayoutError::Failed {
        program: what.into(),
        message: tools::tail(&out.combined(), 12),
    }
}

/// Draw, fill and merge a planned chip into `dir`.
#[allow(clippy::too_many_arguments)]
pub async fn assemble(
    programs: &Programs,
    files: &PdkLayoutFiles,
    spec: &ChipSpec,
    plan: &ChipPlan,
    io: &IoLibrary,
    blocks: &[Block],
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<Assembled, LayoutError> {
    tools::check_path(dir)?;
    tools::check_name("cell", &spec.name)?;
    files.check_pcells()?;
    if !files.magic_gds_tech.is_file() {
        return Err(LayoutError::Input(format!(
            "the PDK has no Magic GDS tech file ({}), which fill needs",
            files.magic_gds_tech.display()
        )));
    }
    for b in blocks {
        tools::check_path(&b.gds)?;
    }
    std::fs::create_dir_all(dir).map_err(|e| LayoutError::Io(e.to_string()))?;
    let nofill = dir.join("chip_nofill.gds");
    let lvs_view = dir.join("lvs_view.gds");
    let json = dir.join("plan.json");
    let data = draw_plan(spec, plan, io, blocks, &nofill, &lvs_view);
    std::fs::write(&json, data.to_string()).map_err(|e| LayoutError::Io(e.to_string()))?;

    let run = RunDir::new()?;
    let script = run.script("chip.py", CHIP_SCRIPT)?;
    let out = tools::klayout(
        programs,
        run.path(),
        &script,
        &[
            ("pdk_python", files.python.display().to_string()),
            ("plan", json.display().to_string()),
        ],
        cancel,
    )
    .await?;
    if !out.success() || !nofill.is_file() || !lvs_view.is_file() {
        return Err(failed("KLayout (assembling the chip)", &out));
    }

    // Fill runs in a folder of its own: Magic writes a file per tile there.
    let fill_dir = RunDir::new()?;
    let fill_script = fill_dir.script("fill.tcl", FILL_SCRIPT)?;
    let fill = fill_dir.path().join("fill.gds");
    let out = tools::magic_tech(
        programs,
        fill_dir.path(),
        &files.magic_gds_tech,
        &fill_script,
        &[
            ("AISPICE_GDS", nofill.display().to_string()),
            ("AISPICE_CELL", spec.name.clone()),
            ("AISPICE_OUT", fill.display().to_string()),
        ],
        cancel,
    )
    .await?;
    if !fill.is_file() {
        return Err(failed("Magic (fill)", &out));
    }
    let gds = dir.join(format!("{}.gds", spec.name));
    let merge = run.script("merge.py", MERGE_SCRIPT)?;
    let out = tools::klayout(
        programs,
        run.path(),
        &merge,
        &[
            ("chip", nofill.display().to_string()),
            ("fill", fill.display().to_string()),
            ("top", spec.name.clone()),
            ("fill_top", format!("{}_fill_pattern", spec.name)),
            ("out", gds.display().to_string()),
        ],
        cancel,
    )
    .await?;
    if !out.success() || !gds.is_file() {
        return Err(failed("KLayout (merging the fill)", &out));
    }
    Ok(Assembled { gds, lvs_view })
}

/// Full-chip DRC (main, extra, antenna and density decks) on the finished
/// chip, and LVS of the chip without its pad cells against `netlist`.
pub async fn check(
    programs: &Programs,
    files: &PdkLayoutFiles,
    spec: &ChipSpec,
    chip: &Assembled,
    netlist: &str,
    dir: &Path,
    cancel: &CancellationToken,
) -> Result<(DrcReport, LvsReport), LayoutError> {
    let checks = DrcChecks {
        extra: true,
        antenna: true,
        density: true,
    };
    let schematic = LvsNetlist {
        text: netlist.to_string(),
        cell: spec.name.clone(),
        ports: ports(spec),
        devices: Vec::new(),
    };
    let (drc_dir, lvs_dir) = (dir.join("drc"), dir.join("lvs"));
    let (drc, lvs) = futures::join!(
        drc::run(
            programs, files, &chip.gds, &spec.name, checks, &drc_dir, cancel
        ),
        lvs::run(
            programs,
            files,
            &chip.lvs_view,
            &schematic,
            &lvs_dir,
            cancel
        )
    );
    Ok((drc?, lvs?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chip::floorplan::MacroGeom;
    use crate::chip::geom::LayerRect;
    use crate::chip::io::test_lef;
    use aispice_core::macropkg::lef::parse_lef;
    use std::collections::BTreeMap;

    const COUNTER_LEF: &str = include_str!(
        "../../../aispice-core/tests/fixtures/macro/tiny_counter.macro/tiny_counter.lef"
    );

    /// A block of the shared digital fixture, which has its pins on the
    /// edge, and a block in aispice's edge-pinned shape.
    fn blocks() -> Vec<MacroGeom> {
        let m = parse_lef(COUNTER_LEF)
            .unwrap()
            .remove("tiny_counter")
            .unwrap();
        let mut pins = BTreeMap::new();
        for (name, p) in &m.pins {
            pins.insert(
                name.clone(),
                p.rects
                    .iter()
                    .filter_map(|r| LayerRect::from_lef(r, (0.0, 0.0)))
                    .collect(),
            );
        }
        let counter = MacroGeom {
            instance: "cnt".into(),
            cell: "tiny_counter".into(),
            size: (20_000, 10_000),
            origin: (0, 0),
            pins,
            at: None,
        };
        let mut ota_pins = BTreeMap::new();
        for (k, name) in ["inp", "out", "vdd", "vss"].iter().enumerate() {
            let x = -1_200 * (k as i64 + 1);
            ota_pins.insert(
                name.to_string(),
                vec![LayerRect {
                    layer: METAL4,
                    rect: Rect::new(x - 150, -600, x + 150, 10_000),
                }],
            );
        }
        let ota = MacroGeom {
            instance: "ota".into(),
            cell: "ota5t".into(),
            size: (24_000, 25_000),
            origin: (6_000, 600),
            pins: ota_pins,
            at: None,
        };
        vec![ota, counter]
    }

    fn spec() -> ChipSpec {
        ChipSpec::parse(
            r#"
name = "demo"
[[macros]]
instance = "ota"
package = "macros/ota5t.macro"
nets = { inp = "ota_in", out = "en" }
[[macros]]
instance = "cnt"
package = "macros/tiny_counter.macro"
nets = { VDD = "vdd", VSS = "vss", "q[0]" = "q0", "q[1]" = "q1" }
[[pads]]
net = "ota_in"
side = "west"
kind = "analog"
[[pads]]
net = "clk"
side = "south"
kind = "input"
[[pads]]
net = "rst_n"
side = "south"
kind = "input"
[[pads]]
net = "q0"
side = "east"
kind = "output"
[[pads]]
net = "q1"
side = "east"
kind = "output"
[[pads]]
net = "vdd"
side = "north"
kind = "vdd"
[[pads]]
net = "vss"
side = "north"
kind = "vss"
[[pads]]
net = "iovdd"
side = "west"
kind = "iovdd"
[[pads]]
net = "iovss"
side = "west"
kind = "iovss"
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_chip_with_an_analog_and_a_digital_block_is_planned_and_routed() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let p = plan(&spec(), &io, &blocks()).unwrap();
        let routed: Vec<&str> = p.routed.nets.iter().map(|n| n.net.as_str()).collect();
        // The OTA's output drives the counter's enable; supplies reach both
        // blocks from the supply pads.
        for net in ["clk", "en", "ota_in", "q0", "q1", "rst_n", "vdd", "vss"] {
            assert!(routed.contains(&net), "{net} in {routed:?}");
        }
        let vdd = p.routed.nets.iter().find(|n| n.net == "vdd").unwrap();
        assert_eq!(vdd.terminals.len(), 3, "{:?}", vdd.terminals);
        assert!(route::check(&p.routed, &p.terminals).is_empty());
        assert_eq!(
            ports(&spec()),
            ["clk", "ota_in", "q0", "q1", "rst_n", "vdd", "vss"]
        );
    }

    #[test]
    fn the_drawing_plan_names_every_cell_and_labels_the_pads() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let p = plan(&spec(), &io, &blocks()).unwrap();
        let blocks: Vec<Block> = blocks()
            .into_iter()
            .map(|g| Block {
                gds: PathBuf::from(format!("/run/{}.gds", g.cell)),
                netlist: BlockNetlist {
                    cell: g.cell.clone(),
                    ports: Vec::new(),
                    text: String::new(),
                    external: BTreeSet::new(),
                },
                geom: g,
            })
            .collect();
        let d = draw_plan(
            &spec(),
            &p,
            &io,
            &blocks,
            Path::new("/run/a.gds"),
            Path::new("/run/b.gds"),
        );
        assert_eq!(d["top"], "demo");
        assert_eq!(d["libraries"].as_array().unwrap().len(), 3);
        let labels: Vec<&str> = d["texts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t[1] == TEXT)
            .map(|t| t[2].as_str().unwrap())
            .collect();
        for port in ports(&spec()) {
            assert!(labels.contains(&port.as_str()), "{port} in {labels:?}");
        }
        let pcells = d["pcells"].as_array().unwrap();
        assert_eq!(
            pcells.len(),
            spec().pads.len() + 1,
            "a bond pad per pad and the seal ring"
        );
        let seal = pcells.last().unwrap();
        assert_eq!(seal["pcell"], "sealring");
        assert!(
            d["lvs_drop"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "sg13g2_Corner")
        );
    }

    #[test]
    fn a_block_pin_away_from_the_edge_is_named() {
        let io = IoLibrary::from_lef(&test_lef()).unwrap();
        let mut b = blocks();
        b[0].pins.get_mut("inp").unwrap()[0].rect = Rect::new(-1_350, 2_000, -1_050, 10_000);
        let e = plan(&spec(), &io, &b).unwrap_err();
        assert!(e.contains("pin inp of block ota"), "{e}");
    }
}
