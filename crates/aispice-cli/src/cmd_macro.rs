//! `aispice macro`: macro packages shared with ChipLeap (macro/v1).

use aispice_core::macropkg::{CheckOptions, MacroError, SCHEMA_JSON, check_package};
use anyhow::Result;
use std::path::Path;
use std::process::ExitCode;

/// Check a package. Exit code 0 when it passes, 1 when it does not (or has
/// warnings with `strict`), 2 when the directory or `--pdk` is wrong, the same
/// as `chipleap macro check`.
pub fn check(dir: &Path, pdk: Option<String>, strict: bool, json: bool) -> Result<ExitCode> {
    let report = match check_package(dir, &CheckOptions { pdk }) {
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
