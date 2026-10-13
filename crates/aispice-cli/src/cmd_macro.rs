//! `aispice macro`: macro packages shared with ChipLeap (macro/v1).

use crate::cmd_layout::{done, in_project, run_tool};
use crate::cmd_sim::workspace_for;
use aispice_core::macropkg::{CheckOptions, MacroError, SCHEMA_JSON, check_package};
use aispice_layout::macro_export::pdk_cell_names;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Check a package. Exit code 0 when it passes, 1 when it does not (or has
/// warnings with `strict`), 2 when the directory, `--pdk` or `--pdk-libs` is
/// wrong, the same as `chipleap macro check`.
pub fn check(
    dir: &Path,
    pdk: Option<String>,
    pdk_libs: Option<&Path>,
    strict: bool,
    json: bool,
) -> Result<ExitCode> {
    let mut pdk_cells = BTreeSet::new();
    if let Some(libs) = pdk_libs {
        if !libs.join("libs.ref").is_dir() {
            eprintln!(
                "aispice: --pdk-libs: {} has no libs.ref directory",
                libs.display()
            );
            return Ok(ExitCode::from(2));
        }
        match pdk_cell_names(libs) {
            Ok(cells) => pdk_cells = cells,
            Err(e) => {
                eprintln!("aispice: --pdk-libs: {e}");
                return Ok(ExitCode::from(2));
            }
        }
    }
    let report = match check_package(
        dir,
        &CheckOptions {
            pdk,
            pdk_cells,
            ..Default::default()
        },
    ) {
        Ok(r) => r,
        Err(e @ (MacroError::NotADirectory(_) | MacroError::Invalid(_))) => {
            eprintln!("aispice: {e}");
            return Ok(ExitCode::from(2));
        }
        Err(e) => return Err(e.into()),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        // Messages quote names from the package's files.
        println!("{}", crate::cmd_agent::terminal_safe(&report.to_text()));
    }
    let failed = !report.passed || (strict && report.warnings > 0);
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

pub fn schema() -> Result<ExitCode> {
    print!("{SCHEMA_JSON}");
    Ok(ExitCode::SUCCESS)
}

pub struct ExportArgs<'a> {
    pub circuit: &'a Path,
    pub layout: &'a Path,
    /// The tool's other inputs.
    pub input: Value,
    pub symbols: &'a [PathBuf],
    pub json_mode: bool,
}

/// `aispice macro export`: the `macro_export` tool on the circuit's
/// project. Exit code 1 when the export stops (nothing is written then).
pub fn export(a: ExportArgs) -> Result<ExitCode> {
    let (ws, circuit) = workspace_for(a.circuit, a.symbols)?;
    let layout = in_project(&ws, a.layout)?;
    let mut input = a.input;
    input["circuit"] = Value::String(circuit);
    input["layout"] = Value::String(layout);
    let out = run_tool(&ws, "macro_export", input)?;
    if out.is_error {
        println!("{}", crate::cmd_agent::terminal_safe(&out.text_content()));
        return Ok(ExitCode::from(1));
    }
    done(&out, a.json_mode, true)
}
