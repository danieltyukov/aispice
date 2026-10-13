//! The layout flow on a project: the PDK's layout files, layouts and their
//! records in the project, run folders for reports, the hints that turn a
//! DRC or LVS failure into a generator parameter to change, and the
//! post-layout netlist that check_specs simulates.

use crate::project::Project;
use crate::workspace::Workspace;
use aispice_core::netlist::{Line, Netlist, Policy, resolve_include};
use aispice_layout::drc::DrcReport;
use aispice_layout::extract::{self, Extracted};
use aispice_layout::gds;
use aispice_layout::generator::LayoutMeta;
use aispice_layout::lvs::LvsReport;
use aispice_layout::tools::{PdkLayoutFiles, Programs};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

/// What every layout tool needs: the project, the programs and the PDK's
/// layout files.
pub struct LayoutEnv {
    pub project: Arc<Project>,
    pub programs: Programs,
    pub files: PdkLayoutFiles,
}

/// The registered PDK folders, which layout tools may read from.
pub fn pdk_roots(ws: &Workspace) -> Vec<PathBuf> {
    ws.runner.config().pdks.trusted_roots()
}

/// Open a GDS of the open project (see [`open_gds`]).
pub fn open_project_gds(ws: &Workspace, rel: &str, cell: Option<&str>) -> Result<GdsFile, String> {
    let p = ws.project().map_err(|e| e.to_string())?;
    open_gds(&p, &pdk_roots(ws), rel, cell)
}

/// The layout setup of the open project, or why there is none.
/// The sandbox KLayout and Magic get on this machine, for `aispice doctor`.
pub async fn sandbox_description() -> String {
    aispice_layout::tools::sandbox_description().await
}

/// `text` with a note when KLayout and Magic ran without a sandbox on this
/// machine, for every tool that runs them.
pub async fn noted(mut text: String) -> String {
    if let Some(note) = aispice_layout::tools::unsandboxed().await {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&note);
        text.push('\n');
    }
    text
}

pub fn env(ws: &Workspace) -> Result<LayoutEnv, String> {
    let project = ws.project().map_err(|e| e.to_string())?;
    let pdk = ws
        .runner
        .project_pdk(&project)
        .map_err(|e| e.to_string())?
        .ok_or(
            "this project names no PDK; layout needs one (use the pdk tool's `use` action to set it in aispice.toml)",
        )?;
    let files = PdkLayoutFiles::of(&pdk).map_err(|e| e.to_string())?;
    Ok(LayoutEnv {
        project,
        programs: ws.runner.config().layout_programs,
        files,
    })
}

/// A GDS file of the project, read and checked, with the cell to work on
/// and the generator's record when it has one.
pub struct GdsFile {
    pub path: PathBuf,
    pub rel: String,
    pub cell: String,
    pub meta: Option<LayoutMeta>,
}

/// Open a GDS file a tool was asked to read. It is resolved by the netlist
/// policy's rules (no `~`, drive or `..`; symlinks followed) and must lie
/// inside the project, outside `.aispice/`, or inside a registered PDK
/// folder (`pdk_roots`). It must be a GDS by its header, not too large or
/// deep, with the cell asked for (the one top cell by default).
pub fn open_gds(
    p: &Project,
    pdk_roots: &[PathBuf],
    rel: &str,
    cell: Option<&str>,
) -> Result<GdsFile, String> {
    let policy = Policy {
        base_dir: p.root().to_path_buf(),
        allowed_dirs: Vec::new(),
        allow_control_blocks: false,
        trusted_dirs: pdk_roots.to_vec(),
    };
    let path = resolve_include(rel, p.root(), &policy)
        .map_err(|e| format!("layout {e}"))?
        .ok_or_else(|| format!("{rel} does not exist"))?;
    if p.in_state_dir(&path) {
        return Err(format!("{rel} is inside aispice's own .aispice folder"));
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(ext.as_str(), "gds" | "gds2" | "gdsii") {
        return Err(format!("{rel} is not a .gds file"));
    }
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("{rel} does not exist"))?;
    if !meta.is_file() {
        return Err(format!("{rel} is not a plain file"));
    }
    let lib = gds::read(&path).map_err(|e| format!("{rel}: {e}"))?;
    let cell = match cell {
        Some(c) => {
            if !lib.cells.contains_key(c) {
                return Err(format!(
                    "{rel} has no cell {c}; its top cells are {}",
                    lib.top_cells().join(", ")
                ));
            }
            c.to_string()
        }
        None => lib.single_top()?.to_string(),
    };
    aispice_layout::tools::check_name("cell", &cell).map_err(|e| e.to_string())?;
    let sidecar = LayoutMeta::path_for(&path, &cell);
    let meta = match std::fs::symlink_metadata(&sidecar) {
        Ok(m) if m.is_file() => Some(LayoutMeta::read(&sidecar)?),
        _ => None,
    };
    let rel = if path.starts_with(p.root()) {
        p.relative(&path)
    } else {
        path.display().to_string()
    };
    Ok(GdsFile {
        rel,
        path,
        cell,
        meta,
    })
}

static RUNS: AtomicU64 = AtomicU64::new(0);

/// A fresh folder for one layout run's reports: `.aispice/layout/<id>`.
/// It is always made here, never reused: a project can ship a `.aispice`
/// folder, and a run folder it planted could hold links that the reports,
/// written by KLayout and Magic, would follow out of the project.
pub fn run_dir(p: &Project) -> Result<(String, PathBuf), String> {
    let base = p.state_subdir(&["layout"]).map_err(|e| e.to_string())?;
    for _ in 0..64 {
        let id = format!(
            "{}-{}",
            crate::project::now_ms(),
            RUNS.fetch_add(1, Ordering::Relaxed)
        );
        let dir = base.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
    }
    Err(format!(
        "no new run folder could be made in {}",
        base.display()
    ))
}

/// The netlist names of the devices a layout holds: the generator's record,
/// else every PDK device of the circuit.
pub fn layout_devices(netlist: &Netlist, meta: Option<&LayoutMeta>) -> Vec<String> {
    match meta {
        Some(m) => m.devices.clone(),
        None => netlist
            .items
            .iter()
            .filter_map(|l| match l {
                Line::Element(e)
                    if e.letter() == 'X'
                        && e.first_word().and_then(aispice_core::pdk::device).is_some() =>
                {
                    Some(e.name.clone())
                }
                _ => None,
            })
            .collect(),
    }
}

/// What usually fixes each family of DRC rules in a generated layout.
pub fn drc_hints(report: &DrcReport) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |h: &str| {
        if !out.iter().any(|o| o == h) {
            out.push(h.to_string());
        }
    };
    for r in &report.rules {
        let family = r.rule.split('.').next().unwrap_or("");
        match family {
            "M2" | "M3" | "M4" | "M5" | "V2" | "V3" | "V4" => {
                add("metal spacing on the routing layers: raise track_pitch (default 0.6 um)")
            }
            "NW" | "pSD" | "nSD" | "LU" | "PWB" => {
                add("well, implant or latch-up rules: keep guard_ring on and raise row_gap")
            }
            "Ant" => {
                add("antenna: a long route on a gate; lower row_gap or track_pitch to shorten it")
            }
            "Act" | "Gat" | "Cnt" | "M1" | "V1" => add(
                "device-level rules: change fingers (finger width) or pattern; if they persist, the generator has a bug to report",
            ),
            _ => add("change the generator parameters and generate again; never edit the GDS"),
        }
    }
    out
}

/// What to do about an LVS mismatch.
pub fn lvs_hints(report: &LvsReport, generated: bool) -> Vec<String> {
    if report.matched {
        return Vec::new();
    }
    let mut out = Vec::new();
    if generated {
        out.push("the layout came from a generator whose topology checks passed, so a mismatch usually means the circuit changed since it was generated: generate it again, or check the role map".to_string());
    } else {
        out.push(
            "nets in the layout are named by labels on the metal text layers; a pin net the schematic has but the layout does not label shows as not found".to_string(),
        );
    }
    out
}

/// Post-layout input for check_specs.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PostLayout {
    /// The layout, a GDS file in the project (from the layout tool).
    pub layout: String,
    /// The cell; default the file's one top cell.
    #[serde(default)]
    pub cell: Option<String>,
    /// Extract resistor networks as well as capacitance (default false).
    #[serde(default)]
    pub resistance: bool,
}

/// The circuit's netlist with its laid-out devices replaced by the
/// extracted layout. This is the base netlist every post-layout run starts
/// from: check_specs makes its corner variants from it the same way as from
/// the schematic's, since the PDK's model header is left as it is.
pub async fn post_layout_netlist(
    ws: &Workspace,
    circuit: &str,
    input: &PostLayout,
    cancel: &CancellationToken,
) -> Result<(Netlist, Vec<String>, Extracted), String> {
    let gds = open_project_gds(ws, &input.layout, input.cell.as_deref())?;
    let env = env(ws)?;
    let (netlist, std_libs, _) = ws
        .runner
        .netlist_for(&env.project, circuit)
        .map_err(|e| e.to_string())?;
    let (_, dir) = run_dir(&env.project)?;
    let ex = extract::run(
        &env.programs,
        &env.files,
        &gds.path,
        &gds.cell,
        input.resistance,
        &dir,
        cancel,
    )
    .await
    .map_err(|e| e.to_string())?;
    let devices = layout_devices(&netlist, gds.meta.as_ref());
    let post = extract::post_layout(&netlist, &ex, &devices)?;
    Ok((post, std_libs, ex))
}
