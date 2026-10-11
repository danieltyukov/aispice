//! xschem import and export: `xschem_import` turns an xschem `.sch` into a
//! readable `.asc`, `xschem_export` writes an `.asc` as an xschem `.sch`.
//! Both prove the conversion (see [`crate::xschem`]).

use super::blocking;
use super::schematic_tools::spec;
use crate::workspace::Workspace;
use crate::xschem::{
    ExportOutcome, ImportOutcome, Setup, check_export, check_import, export_schematic, import_file,
    pdk_for, report, report_json,
};
use aispice_agent::tool::parse_input;
use aispice_agent::{Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_core::diff::diff;
use aispice_core::schematic::Schematic;
use aispice_core::summary::summarize;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The PDK whose xschem symbols and models a conversion uses: the
/// project's, else the only registered one.
fn setup(ws: &Workspace, project: &crate::Project) -> Setup {
    let pdk = match ws.runner.project_pdk(project) {
        Ok(Some(p)) => Some(p),
        Ok(None) => pdk_for(None, &ws.runner.config().pdks),
        Err(_) => None,
    };
    Setup::new(pdk)
}

/// `name.<ext>` with another extension.
fn with_extension(path: &str, ext: &str) -> String {
    let p = Path::new(path);
    p.with_extension(ext).to_string_lossy().replace('\\', "/")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct XschemImportInput {
    /// The xschem schematic, relative to the project folder, e.g. `tb.sch`.
    pub schematic: String,
    /// The LTspice schematic to create. Default: the same name with `.asc`.
    #[serde(default)]
    pub circuit: Option<String>,
    /// How many candidate layouts to try: 1 is quick, 3 searches widely.
    #[serde(default)]
    pub effort: Option<u8>,
}

pub struct XschemImport {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for XschemImport {
    fn spec(&self) -> ToolSpec {
        spec::<XschemImportInput>(
            "xschem_import",
            "Import an xschem schematic (.sch, e.g. an IHP SG13G2 test bench) into the project as a readable LTspice .asc, drawn by aispice's layout engine. IHP devices become sg13g2 symbols; ngspice .control blocks and the PDK model header are dropped (aispice runs its own analyses and the project's PDK supplies the models). Returns what could not be carried over and a proof: both schematics are netlisted (by aispice, and by xschem when installed) and compared. Refuses to overwrite.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: XschemImportInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        let planned = tokio::task::spawn_blocking({
            let ws = ws.clone();
            let input_schematic = input.schematic.clone();
            let circuit = input
                .circuit
                .clone()
                .unwrap_or_else(|| with_extension(&input.schematic, "asc"));
            let effort = input.effort.unwrap_or(2).clamp(1, 3);
            move || -> Result<(Setup, PathBuf, Vec<PathBuf>, Option<PathBuf>, String, aispice_core::xschem::import::Imported), String> {
                let p = ws.project().map_err(|e| e.to_string())?;
                if !input_schematic.to_ascii_lowercase().ends_with(".sch") {
                    return Err(format!("{input_schematic} must be an xschem .sch file"));
                }
                if !circuit.to_ascii_lowercase().ends_with(".asc") {
                    return Err(format!("{circuit} must end in .asc"));
                }
                let sch = p.resolve(&input_schematic).map_err(|e| e.to_string())?;
                if !sch.is_file() {
                    return Err(format!("{input_schematic} does not exist"));
                }
                let target = p.resolve(&circuit).map_err(|e| e.to_string())?;
                if target.exists() {
                    return Err(format!("{circuit} already exists; name another circuit"));
                }
                let setup = setup(&ws, &p);
                let roots = vec![p.root().to_path_buf()];
                let out_dir = target.parent().map(Path::to_path_buf);
                let imported = import_file(&setup, &sch, roots.clone(), out_dir.as_deref(), p.library(), effort)?;
                Ok((setup, sch, roots, out_dir, circuit, imported))
            }
        })
        .await;
        let (setup, sch, roots, out_dir, circuit, imported) = match planned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::error(format!("No file created. {e}")),
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        };
        if !imported.proof.equivalent {
            return ToolOutput::error(format!(
                "No file created: the drawing does not netlist to the schematic's circuit: {}",
                imported.proof.differences.join("; ")
            ));
        }
        let xschem = check_import(
            &setup,
            &sch,
            roots,
            &imported,
            out_dir.as_deref(),
            &ctx.cancel,
        )
        .await;
        let outcome = ImportOutcome { imported, xschem };
        let p = match ws.project() {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e.to_string()),
        };
        let d = diff(&Schematic::new(), &outcome.imported.schematic, p.library());
        if !ws
            .approve(
                &circuit,
                &format!("Create {circuit} from {}", input.schematic),
                &d.text,
            )
            .await
        {
            return ToolOutput::text(
                "The user declined creating this file; nothing was written. Ask what they would prefer.",
            );
        }
        let source = input.schematic.clone();
        blocking(move || {
            let i = &outcome.imported;
            p.create_as(&circuit, &i.schematic, &format!("Imported from xschem: {source}"))
                .map_err(|e| e.to_string())?;
            if let Ok(path) = p.resolve(&circuit) {
                ws.after_save(&path);
            }
            let summary = summarize(&i.schematic, p.library());
            let mut text = format!(
                "Created {circuit} from {source} with {} component(s) (layout quality {:.0}/100).\n",
                summary.components.len(),
                i.quality.score
            );
            text.push_str(&report(&i.findings, &i.notes, &i.proof, &outcome.xschem));
            text.push_str(&summary.to_text());
            Ok(ToolOutput::text(text).with_data(json!({
                "kind": "schematic",
                "circuit": circuit,
                "summary": summary,
                "xschem": report_json(&i.findings, &i.notes, &i.proof, &outcome.xschem),
            })))
        })
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct XschemExportInput {
    /// The LTspice schematic to export, relative to the project folder, e.g.
    /// `amp.asc`.
    pub circuit: String,
    /// The xschem schematic to create. Default: the same name with `.sch`.
    #[serde(default)]
    pub schematic: Option<String>,
}

pub struct XschemExport {
    pub ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for XschemExport {
    fn spec(&self) -> ToolSpec {
        spec::<XschemExportInput>(
            "xschem_export",
            "Write a project schematic (.asc) as an xschem schematic (.sch): redrawn with xschem's symbols (IHP's sg13g2_pr symbols for PDK devices, a MODEL block for the PDK header), directives as code blocks. Parts xschem has no symbol for stay SPICE lines in a code block. Returns findings and a proof: the .asc's netlist compared with the .sch's (netlisted by aispice, and by xschem when installed). Refuses to overwrite.",
        )
    }

    async fn call(&self, ctx: &ToolContext, input: Value) -> ToolOutput {
        let input: XschemExportInput = match parse_input(input) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let ws = self.ws.clone();
        let target_name = input
            .schematic
            .clone()
            .unwrap_or_else(|| with_extension(&input.circuit, "sch"));
        let planned = tokio::task::spawn_blocking({
            let ws = ws.clone();
            let circuit = input.circuit.clone();
            let target_name = target_name.clone();
            move || -> Result<(Setup, PathBuf, aispice_core::xschem::export::Exported), String> {
                let p = ws.project().map_err(|e| e.to_string())?;
                if !target_name.to_ascii_lowercase().ends_with(".sch") {
                    return Err(format!("{target_name} must end in .sch"));
                }
                let (sch, _) = p.load(&circuit).map_err(|e| e.to_string())?;
                let out = p.resolve(&target_name).map_err(|e| e.to_string())?;
                if out.exists() {
                    return Err(format!("{target_name} already exists; name another file"));
                }
                let setup = setup(&ws, &p);
                let exported = export_schematic(
                    &setup,
                    &sch,
                    p.library(),
                    &format!("* {circuit}"),
                    &out,
                    vec![p.root().to_path_buf()],
                    2,
                )?;
                Ok((setup, out, exported))
            }
        })
        .await;
        let (setup, out, exported) = match planned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return ToolOutput::error(format!("No file created. {e}")),
            Err(e) => return ToolOutput::error(format!("internal error: {e}")),
        };
        if !exported.proof.equivalent {
            return ToolOutput::error(format!(
                "No file created: the xschem schematic does not netlist to the circuit: {}",
                exported.proof.differences.join("; ")
            ));
        }
        let xschem = check_export(&setup, &target_name, &exported, &ctx.cancel).await;
        let outcome = ExportOutcome { exported, xschem };
        if !ws
            .approve(
                &target_name,
                &format!("Create {target_name} from {}", input.circuit),
                &outcome.exported.text,
            )
            .await
        {
            return ToolOutput::text(
                "The user declined creating this file; nothing was written. Ask what they would prefer.",
            );
        }
        let circuit = input.circuit.clone();
        blocking(move || {
            let e = &outcome.exported;
            if out.exists() {
                return Err(format!("{target_name} already exists; nothing was written"));
            }
            crate::project::atomic_write(&out, e.text.as_bytes()).map_err(|e| e.to_string())?;
            ws.after_save(&out);
            let mut text = format!(
                "Created {target_name} from {circuit} ({} instance(s), {} wire(s)).\n",
                e.file.instances.len(),
                e.file.wires.len()
            );
            text.push_str(&report(&e.findings, &e.notes, &e.proof, &outcome.xschem));
            let mut value = report_json(&e.findings, &e.notes, &e.proof, &outcome.xschem);
            value["schematic"] = json!(target_name);
            value["circuit"] = json!(circuit);
            Ok(ToolOutput::text(text).with_data(json!({"kind": "generic", "value": value})))
        })
        .await
    }
}
