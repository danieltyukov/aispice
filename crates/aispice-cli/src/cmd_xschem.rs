//! `aispice xschem import|export`: convert between xschem's `.sch` and
//! LTspice's `.asc`, with the proof that the circuit did not change.
//!
//! Exit codes: 0 when the conversion is proven, 1 when a proof finds a
//! difference (for CI), 2 on an error. A proof xschem could not run is
//! reported and is not a difference.

use crate::cmd_agent::terminal_safe;
use crate::cmd_files::{Ctx, ltspice_lib};
use aispice_core::schematic;
use aispice_core::symbol::SymbolLibrary;
use aispice_core::xschem::import::Proof;
use aispice_core::xschem::netlist::Finding;
use aispice_tools::pdk::Registry;
use aispice_tools::xschem::{
    Effort, Setup, XschemCheck, check_export, export_schematic, import_file, import_xschem_check,
    pdk_for, report, report_json,
};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tokio_util::sync::CancellationToken;

/// The PDK named by the current folder's `aispice.toml`, else the only
/// registered one.
fn setup() -> Setup {
    let cwd = std::env::current_dir().ok();
    let registry = Registry::load().unwrap_or_default();
    Setup::new(pdk_for(cwd.as_deref(), &registry))
}

fn folder_of(p: &Path) -> PathBuf {
    p.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Folders a schematic may pull files from besides the libraries: its own,
/// and the current one when the schematic is inside it (a project run from
/// its root). A path in the schematic never widens this.
fn roots(file: &Path) -> Vec<PathBuf> {
    let mut out = vec![folder_of(file)];
    if let (Ok(cwd), Ok(f)) = (
        std::env::current_dir().and_then(|d| d.canonicalize()),
        file.canonicalize(),
    ) && f.starts_with(&cwd)
    {
        out.push(cwd);
    }
    out
}

fn print_report(
    ctx: &Ctx,
    header: &str,
    findings: &[Finding],
    notes: &[String],
    aispice: &Proof,
    xschem: &XschemCheck,
) -> Result<()> {
    if ctx.json {
        let mut v = report_json(findings, notes, aispice, xschem);
        v["result"] = serde_json::json!(header);
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("{}", terminal_safe(header));
        print!(
            "{}",
            terminal_safe(&report(findings, notes, aispice, xschem))
        );
    }
    Ok(())
}

fn exit(aispice: &Proof, xschem: &XschemCheck) -> ExitCode {
    if aispice.equivalent && xschem.equivalent() != Some(false) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

/// Import an xschem schematic. xschem itself is never run on it: it
/// evaluates Tcl from the schematic, which aispice did not write.
pub fn import(
    ctx: &Ctx,
    file: &Path,
    output: Option<&Path>,
    force: bool,
    effort: u8,
) -> Result<ExitCode> {
    let sch = file
        .canonicalize()
        .with_context(|| format!("reading {}", file.display()))?;
    let out = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("asc"));
    if out.exists() && !force {
        bail!("{} exists; pass --force to replace it", out.display());
    }
    let out_dir = folder_of(&out);
    let lib = SymbolLibrary::for_project(Some(&out_dir), &ctx.symbols, ltspice_lib().as_deref());
    let setup = setup();
    let imported = import_file(
        &setup,
        &sch,
        roots(&sch),
        Some(&out_dir),
        &lib,
        Effort {
            effort: effort.clamp(1, 3),
            max_elements: None,
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if !imported.proof.equivalent {
        let skipped = XschemCheck::Skipped {
            reason: "the conversion is not proven, so nothing was written".into(),
        };
        print_report(
            ctx,
            &format!("{} was not written", out.display()),
            &imported.findings,
            &imported.notes,
            &imported.proof,
            &skipped,
        )?;
        return Ok(ExitCode::from(1));
    }
    let xschem = import_xschem_check();
    std::fs::write(&out, schematic::write_bytes(&imported.schematic))
        .with_context(|| format!("writing {}", out.display()))?;
    print_report(
        ctx,
        &format!(
            "wrote {} (layout quality {:.0}/100)",
            out.display(),
            imported.quality.score
        ),
        &imported.findings,
        &imported.notes,
        &imported.proof,
        &xschem,
    )?;
    Ok(exit(&imported.proof, &xschem))
}

pub fn export(ctx: &Ctx, file: &Path, output: Option<&Path>, force: bool) -> Result<ExitCode> {
    let bytes = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let (asc, warnings) = schematic::parse_bytes(&bytes);
    for w in warnings {
        eprintln!(
            "{}:{}: {}",
            file.display(),
            w.line,
            terminal_safe(&w.message)
        );
    }
    let out = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.with_extension("sch"));
    if out.exists() && !force {
        bail!("{} exists; pass --force to replace it", out.display());
    }
    let lib = SymbolLibrary::for_project(
        Some(&folder_of(file)),
        &ctx.symbols,
        ltspice_lib().as_deref(),
    );
    let setup = setup();
    let out_abs = folder_of(&out)
        .canonicalize()
        .with_context(|| format!("the folder of {}", out.display()))?
        .join(out.file_name().unwrap_or_default());
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let exported = export_schematic(
        &setup,
        &asc,
        &lib,
        &format!("* {name}"),
        &out_abs,
        roots(file),
        Effort {
            effort: 2,
            max_elements: None,
        },
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    if !exported.proof.equivalent {
        let skipped = XschemCheck::Skipped {
            reason: "the conversion is not proven, so nothing was written".into(),
        };
        print_report(
            ctx,
            &format!("{} was not written", out.display()),
            &exported.findings,
            &exported.notes,
            &exported.proof,
            &skipped,
        )?;
        return Ok(ExitCode::from(1));
    }
    let out_name = out
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let xschem = runtime()?.block_on(check_export(
        &setup,
        &out_name,
        &exported,
        &CancellationToken::new(),
    ));
    std::fs::write(&out, &exported.text).with_context(|| format!("writing {}", out.display()))?;
    print_report(
        ctx,
        &format!(
            "wrote {} ({} instance(s), {} wire(s))",
            out.display(),
            exported.file.instances.len(),
            exported.file.wires.len()
        ),
        &exported.findings,
        &exported.notes,
        &exported.proof,
        &xschem,
    )?;
    Ok(exit(&exported.proof, &xschem))
}
