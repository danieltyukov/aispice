//! Analog layout as tools: generate a layout from a schematic, check it
//! (DRC, LVS), extract it, and look at it. Layouts come only from the
//! generators; no tool edits GDS.

use super::schematic_tools::spec;
use crate::layout::{self, GdsFile, LayoutEnv};
use crate::workspace::{FileWrite, Workspace};
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_layout::drc::{self, DrcChecks, DrcReport};
use aispice_layout::extract;
use aispice_layout::generator::{self, LayoutMeta, Params, Roles, Template};
use aispice_layout::lvs::{self, LvsReport};
use aispice_layout::render;
use async_trait::async_trait;
use base64::Engine as _;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

macro_rules! input {
    ($v:expr) => {
        match parse_input($v) {
            Ok(i) => i,
            Err(e) => return e,
        }
    };
}

macro_rules! tri {
    ($v:expr) => {
        match $v {
            Ok(v) => v,
            Err(e) => return ToolOutput::error(e.to_string()),
        }
    };
}

fn yes() -> bool {
    true
}

fn hints_text(hints: &[String]) -> String {
    hints.iter().map(|h| format!("  hint: {h}\n")).collect()
}

async fn run_drc(
    env: &LayoutEnv,
    gds: &GdsFile,
    checks: DrcChecks,
    cancel: &CancellationToken,
) -> Result<DrcReport, String> {
    let (_, dir) = layout::run_dir(&env.project)?;
    drc::run(
        &env.programs,
        &env.files,
        &gds.path,
        &gds.cell,
        checks,
        &dir.join("drc"),
        cancel,
    )
    .await
    .map_err(|e| e.to_string())
}

async fn run_lvs(
    ws: &Workspace,
    env: &LayoutEnv,
    gds: &GdsFile,
    circuit: &str,
    cancel: &CancellationToken,
) -> Result<LvsReport, String> {
    let (netlist, _, _) = ws
        .runner
        .netlist_for(&env.project, circuit)
        .map_err(|e| e.to_string())?;
    let (only, extra) = match &gds.meta {
        Some(m) => (Some(m.devices.as_slice()), m.dummies.as_slice()),
        None => (None, &[][..]),
    };
    let schematic = lvs::lvs_netlist(&netlist, &gds.cell, only, extra)?;
    let (_, dir) = layout::run_dir(&env.project)?;
    lvs::run(
        &env.programs,
        &env.files,
        &gds.path,
        &schematic,
        &dir.join("lvs"),
        cancel,
    )
    .await
    .map_err(|e| e.to_string())
}

fn kept(env: &LayoutEnv, path: &Option<std::path::PathBuf>) -> String {
    path.as_ref()
        .and_then(|p| p.parent())
        .map(|p| env.project.relative(p))
        .unwrap_or_default()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DrcInput {
    /// The layout: a GDS file in the project.
    pub layout: String,
    /// The cell to check; default the file's one top cell.
    #[serde(default)]
    pub cell: Option<String>,
    /// IHP's extra rules beyond the main set (default true).
    #[serde(default = "yes")]
    pub extra: bool,
    /// Antenna rules (default true).
    #[serde(default = "yes")]
    pub antenna: bool,
    /// Density rules, which apply to a whole chip (default false).
    #[serde(default)]
    pub density: bool,
}

pub struct DrcTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for DrcTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<DrcInput>(
            "drc",
            "Design rule check of a layout (GDS) with the PDK's own KLayout rule decks: the main rules, IHP's extra rules and antenna rules by default. Each violation comes with its rule, the layer, a description and where it is (box in um). For a layout made by the layout tool, the report says which generator parameter usually fixes each rule family; change it and generate again. Never edit GDS by hand.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: DrcInput = input!(input);
        let gds = tri!(layout::open_project_gds(
            &self.ws,
            &input.layout,
            input.cell.as_deref()
        ));
        let env = tri!(layout::env(&self.ws));
        let checks = DrcChecks {
            extra: input.extra,
            antenna: input.antenna,
            density: input.density,
            recommended: true,
        };
        let report = tri!(run_drc(&env, &gds, checks, &ctx.cancel).await);
        let mut t = format!("{}: {}", gds.rel, report.to_text());
        if gds.meta.is_some() {
            t.push_str(&hints_text(&layout::drc_hints(&report)));
        }
        let folder = kept(&env, &report.reports.first().cloned());
        if !folder.is_empty() {
            t.push_str(&format!("Reports kept in {folder}.\n"));
        }
        ToolOutput::text(layout::noted(t).await)
            .with_data(json!({"kind": "generic", "value": {"drc": report}}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LvsInput {
    /// The layout: a GDS file in the project.
    pub layout: String,
    /// The circuit it should match: a schematic or netlist of the project.
    pub circuit: String,
    /// The cell; default the file's one top cell.
    #[serde(default)]
    pub cell: Option<String>,
}

pub struct LvsTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for LvsTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<LvsInput>(
            "lvs",
            "Layout versus schematic with the PDK's KLayout LVS deck: compares the devices and nets extracted from a layout (GDS) with the circuit's PDK devices. For a layout the layout tool made, only the devices it drew are compared, with its dummy transistors added to the schematic side. Reports every net, device and pin that does not match, by its name in the schematic and in the layout. Pin nets must be labelled in the layout; the generators do that.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: LvsInput = input!(input);
        let gds = tri!(layout::open_project_gds(
            &self.ws,
            &input.layout,
            input.cell.as_deref()
        ));
        let env = tri!(layout::env(&self.ws));
        let report = tri!(run_lvs(&self.ws, &env, &gds, &input.circuit, &ctx.cancel).await);
        let mut t = format!(
            "{} against {}: {}",
            gds.rel,
            input.circuit,
            report.to_text()
        );
        t.push_str(&hints_text(&layout::lvs_hints(&report, gds.meta.is_some())));
        if let Some(m) = &gds.meta
            && m.circuit.as_deref().is_some_and(|c| c != input.circuit)
        {
            t.push_str(&format!(
                "  note: this layout was generated from {}, not {}\n",
                m.circuit.as_deref().unwrap_or(""),
                input.circuit
            ));
        }
        ToolOutput::text(layout::noted(t).await)
            .with_data(json!({"kind": "generic", "value": {"lvs": report}}))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractInput {
    /// The layout: a GDS file in the project.
    pub layout: String,
    #[serde(default)]
    pub cell: Option<String>,
    /// Resistor networks as well as capacitance (default false).
    #[serde(default)]
    pub resistance: bool,
}

pub struct ExtractTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for ExtractTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::ReadOnly
    }

    fn spec(&self) -> ToolSpec {
        spec::<ExtractInput>(
            "extract",
            "Extract a layout's parasitics with Magic and the PDK's tech file: every transistor finger, coupling and ground capacitance and, with resistance, a resistor network per net. Returns the extracted subcircuit. To simulate it, run check_specs with post_layout, which puts the extracted layout in place of the schematic's devices and reports each spec before and after layout.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: ExtractInput = input!(input);
        let gds = tri!(layout::open_project_gds(
            &self.ws,
            &input.layout,
            input.cell.as_deref()
        ));
        let env = tri!(layout::env(&self.ws));
        let (_, dir) = tri!(layout::run_dir(&env.project));
        let ex = tri!(
            extract::run(
                &env.programs,
                &env.files,
                &gds.path,
                &gds.cell,
                input.resistance,
                &dir,
                &ctx.cancel,
            )
            .await
        );
        let mut t = format!("{}: {}", gds.rel, ex.to_text());
        if let Some(p) = &ex.path {
            t.push_str(&format!("Kept as {}.\n", env.project.relative(p)));
        }
        let lines: Vec<&str> = ex.text.lines().collect();
        if lines.len() <= 120 {
            t.push_str(&ex.text);
        } else {
            t.push_str(&lines[..120].join("\n"));
            t.push_str(&format!("\n... {} more lines\n", lines.len() - 120));
        }
        ToolOutput::text(layout::noted(t).await).with_data(
            json!({"kind": "generic", "value": {"extracted": {
                "cell": ex.cell, "ports": ex.ports, "devices": ex.devices,
                "capacitors": ex.capacitors, "resistors": ex.resistors,
                "total_cap_ff": ex.total_cap_ff,
            }}}),
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayoutAction {
    /// Lay out a circuit's transistors with a generator, then check it.
    Generate,
    /// Draw a layout as an image.
    Render,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LayoutInput {
    pub action: LayoutAction,
    /// generate: the circuit to lay out.
    #[serde(default)]
    pub circuit: Option<String>,
    /// generate: current_mirror, diff_pair, ota5t, two_stage or comparator.
    #[serde(default)]
    pub template: Option<Template>,
    /// generate: which instances play which part, e.g. {"pair": ["M1",
    /// "M2"], "load": ["M3", "M4"], "tail": "M5", "bias": "M6"}. Found from
    /// the topology when omitted.
    #[serde(default)]
    pub roles: Option<Roles>,
    /// generate: pattern, dummies, guard_ring, fingers, track_pitch, row_gap.
    #[serde(default)]
    pub params: Option<Params>,
    /// The cell name; default the circuit's file name.
    #[serde(default)]
    pub cell: Option<String>,
    /// generate: the GDS to write, default `<cell>.gds` beside the circuit.
    /// render: the GDS to draw.
    #[serde(default)]
    pub layout: Option<String>,
    /// generate: run DRC and LVS on the result (default true).
    #[serde(default = "yes")]
    pub check: bool,
    /// generate: how many times to draw it, changing the parameter that
    /// usually fixes a failed DRC between tries (default 3, at most 5).
    #[serde(default)]
    pub attempts: Option<u32>,
    /// render: image width and height in pixels (default 1200 x 900).
    #[serde(default)]
    pub size: Option<[u32; 2]>,
}

const LAYOUT_DESCRIPTION: &str = r#"
Analog layout on the project's PDK (IHP SG13G2), drawn with the PDK's own pcells. `generate` lays out a circuit's transistors with a generator: current_mirror (roles ref, out), diff_pair (pair), ota5t (pair, load, tail, bias), two_stage, a Miller op-amp (the ota5t roles plus cs, sink and cc, its cap_cmim capacitor, drawn as a MIM pcell with both plates brought down to the routing), or comparator, a StrongARM latch (tail, pair, latch_n, latch_p, reset); roles are found from the topology when not given. Each matched group becomes a row of two-finger units in a common-centroid or interdigitated pattern with dummy fingers at both ends and a guard ring; nets run on Metal3 tracks, joined across rows on Metal4, and pin nets are labelled. A comparator is drawn mirror-symmetric about its axis, each pair's devices on their own sides and its nets wired as mirror images, so the laid-out latch keeps its offset small. It draws the block, runs DRC and LVS, and when DRC fails on a rule a parameter addresses (routing spacing, wells, latch-up) it changes that parameter and draws again, up to `attempts` times; then it writes <cell>.gds and <cell>.layout.json (how it was made) and returns every remaining violation with a hint on which parameter to change. Parameters: pattern (common_centroid, interdigitated), dummies (0 to 4 per row end), guard_ring, fingers (of the smallest device in a row, even), track_pitch and row_gap in um. GDS is never edited by hand: when a check fails, change parameters or roles and generate again. `render` draws a layout as a PNG.
"#;

pub struct LayoutTool {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for LayoutTool {
    fn access(&self) -> aispice_agent::Access {
        aispice_agent::Access::Edit
    }

    fn spec(&self) -> ToolSpec {
        spec::<LayoutInput>("layout", LAYOUT_DESCRIPTION)
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: LayoutInput = input!(input);
        match input.action {
            LayoutAction::Generate => self.generate(input, &ctx.cancel).await,
            LayoutAction::Render => self.render(input, &ctx.cancel).await,
        }
    }
}

/// A cell name from a file name: its stem if that is a name layout tools
/// take.
fn cell_from(circuit: &str) -> Option<String> {
    let stem = Path::new(circuit)
        .file_stem()?
        .to_string_lossy()
        .into_owned();
    aispice_layout::tools::safe_name(&stem).then_some(stem)
}

impl LayoutTool {
    async fn generate(&self, input: LayoutInput, cancel: &CancellationToken) -> ToolOutput {
        let Some(circuit) = input.circuit.clone() else {
            return ToolOutput::error("generate needs the circuit to lay out");
        };
        let Some(template) = input.template else {
            return ToolOutput::error(
                "generate needs a template: current_mirror, diff_pair, ota5t, two_stage or comparator",
            );
        };
        let cell = input
            .cell
            .clone()
            .or_else(|| cell_from(&circuit))
            .unwrap_or_else(|| template.name().to_string());
        if let Err(e) = aispice_layout::tools::check_name("cell", &cell) {
            return ToolOutput::error(e.to_string());
        }
        let p = tri!(self.ws.project());

        // Where it goes: beside the circuit unless named. Both files go
        // through the project's output resolver now, before any work.
        let out_rel = input.layout.clone().unwrap_or_else(|| {
            let dir = Path::new(&circuit)
                .parent()
                .map(|d| d.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if dir.is_empty() {
                format!("{cell}.gds")
            } else {
                format!("{dir}/{cell}.gds")
            }
        });
        let out = tri!(p.resolve_output(&out_rel));
        if !out
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gds"))
        {
            return ToolOutput::error(format!("{out_rel} must be a .gds file"));
        }
        let rel = p.relative(&out);
        let sidecar_rel = p.relative(&LayoutMeta::path_for(&out, &cell));
        let sidecar = tri!(p.resolve_output(&sidecar_rel));
        // A file of the user's under that name is never replaced: only a
        // layout this tool made (it has its record beside it).
        if out.exists() && !std::fs::symlink_metadata(&sidecar).is_ok_and(|m| m.is_file()) {
            return ToolOutput::error(format!(
                "{rel} exists and was not made by the layout tool; name another file with `layout`"
            ));
        }
        let env = tri!(layout::env(&self.ws));
        let (netlist, _, _) = tri!(self.ws.runner.netlist_for(&env.project, &circuit));
        let roles = input.roles.clone().unwrap_or_default();
        let mut params = input.params.clone().unwrap_or_default();

        // Draw and check in a run folder; only the result goes into the
        // project. A failed DRC that a parameter addresses is drawn again.
        let (_, dir) = tri!(layout::run_dir(&env.project));
        let attempts = input.attempts.unwrap_or(3).clamp(1, 5);
        let mut log = String::new();
        let mut k = 1;
        let (generated, tmp, checks) = loop {
            let mut generated = tri!(generator::generate(
                &netlist, template, &roles, &params, &cell
            ));
            generated.meta.circuit = Some(circuit.clone());
            let tmp = dir.join(format!("try{k}.gds"));
            tri!(generator::draw(&env.programs, &env.files, &generated.plan, &tmp, cancel).await);
            if !input.check {
                break (generated, tmp, None);
            }
            let gds = GdsFile {
                path: tmp.clone(),
                rel: String::new(),
                cell: cell.clone(),
                meta: Some(generated.meta.clone()),
            };
            let drc = tri!(run_drc(&env, &gds, DrcChecks::default(), cancel).await);
            let lvs = tri!(run_lvs(&self.ws, &env, &gds, &circuit, cancel).await);
            let rules: Vec<String> = drc.rules.iter().map(|r| r.rule.clone()).collect();
            let next = (!drc.clean() && k < attempts)
                .then(|| generator::adjust(&params, &rules))
                .flatten();
            match next {
                Some((p, why)) => {
                    log.push_str(&format!(
                        "Try {k}: DRC failed ({}); drawing again with {why}.\n",
                        rules.join(", ")
                    ));
                    params = p;
                    k += 1;
                }
                None => break (generated, tmp, Some((drc, lvs))),
            }
        };

        let summary = generated.summary();
        let bytes = tri!(aispice_layout::tools::read_output(
            &tmp,
            aispice_layout::gds::MAX_BYTES
        ));
        let meta_json = serde_json::to_string_pretty(&generated.meta).unwrap_or_default();
        let files = [
            FileWrite {
                rel: rel.clone(),
                bytes,
            },
            FileWrite {
                rel: sidecar_rel.clone(),
                bytes: meta_json.into_bytes(),
            },
        ];
        tri!(
            self.ws
                .write_files(self, &files, &format!("Layout {rel}"), &summary)
                .await
        );

        let mut t = format!(
            "{log}Wrote {rel} and {sidecar_rel} (undo either with the history tool).\n{summary}"
        );
        let mut data = json!({"layout": rel, "meta": generated.meta, "attempts": k});
        if let Some((drc, lvs)) = checks {
            t.push_str(&drc.to_text());
            t.push_str(&hints_text(&layout::drc_hints(&drc)));
            t.push_str(&lvs.to_text());
            t.push_str(&hints_text(&layout::lvs_hints(&lvs, true)));
            if drc.clean() && lvs.matched {
                t.push_str(&format!(
                    "Next: look at it with layout render, and compare specs before and after layout with check_specs and post_layout {{\"layout\": \"{rel}\"}}.\n"
                ));
            }
            data["drc"] = json!(drc);
            data["lvs"] = json!(lvs);
        }
        ToolOutput::text(layout::noted(t).await)
            .with_data(json!({"kind": "generic", "value": data}))
    }

    async fn render(&self, input: LayoutInput, cancel: &CancellationToken) -> ToolOutput {
        let Some(rel) = input.layout.clone() else {
            return ToolOutput::error("render needs the layout (a GDS file) to draw");
        };
        let gds = tri!(layout::open_project_gds(
            &self.ws,
            &rel,
            input.cell.as_deref()
        ));
        let env = tri!(layout::env(&self.ws));
        let [w, h] = input.size.unwrap_or([1200, 900]);
        let png = tri!(
            render::png(
                &env.programs,
                &env.files,
                &gds.path,
                &gds.cell,
                (w, h),
                cancel
            )
            .await
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        ToolOutput::text(layout::noted(format!("Layout of {} in {}", gds.cell, gds.rel)).await)
            .with_image("image/png", b64)
            .with_data(json!({"kind": "generic", "value": {"layout": gds.rel, "cell": gds.cell}}))
    }
}
