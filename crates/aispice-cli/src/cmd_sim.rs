//! `aispice sim` and `aispice check`: simulate a circuit, or check it against
//! its specs (at corners, if asked) with an exit code CI can use.

use aispice_agent::{ToolContext, ToolOutput};
use aispice_tools::setup::open_workspace;
use aispice_tools::{Workspace, registry};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

/// Open the circuit's folder as the project; return the workspace and the
/// circuit's name within it.
pub fn workspace_for(file: &Path, symbols: &[PathBuf]) -> Result<(Arc<Workspace>, String)> {
    let file =
        std::fs::canonicalize(file).with_context(|| format!("{} not found", file.display()))?;
    let dir = file
        .parent()
        .context("the file has no folder")?
        .to_path_buf();
    let name = file
        .file_name()
        .context("not a file")?
        .to_string_lossy()
        .into_owned();
    let cfg = aispice_agent::Config::load().unwrap_or_default();
    let ws = open_workspace(&dir, &cfg, symbols)
        .with_context(|| format!("opening {}", dir.display()))?;
    Ok((ws, name))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> Result<ToolOutput> {
    let reg = registry(ws.clone());
    let t = reg.get(tool).context("tool missing")?;
    Ok(t.call(&ToolContext::default(), input).await)
}

fn print(out: &ToolOutput, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&out.data).unwrap_or_default()
        );
    } else {
        print!("{}", crate::cmd_agent::terminal_safe(&out.text_content()));
        if !out.text_content().ends_with('\n') {
            println!();
        }
    }
}

pub fn sim(
    file: &Path,
    symbols: &[PathBuf],
    simulator: Option<String>,
    analysis: Option<String>,
    measure: Vec<String>,
    json_mode: bool,
) -> Result<ExitCode> {
    let (ws, circuit) = workspace_for(file, symbols)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let out = rt.block_on(call(&ws, "simulate", json!({"circuit": circuit, "simulator": simulator, "analysis": analysis, "measurements": measure})))?;
    print(&out, json_mode);
    Ok(if out.is_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// The corner set the command line describes, or `None` without corners.
pub fn corners(
    process: &[String],
    temperatures: &[String],
    supplies: &[String],
) -> Result<Option<Value>> {
    if process.is_empty() && temperatures.is_empty() && supplies.is_empty() {
        return Ok(None);
    }
    // Numbers stay text: the tool reads SPICE numbers such as `1.8` or
    // `900m` and names a bad one.
    let mut supply_list = Vec::new();
    for s in supplies {
        let (target, values) = s
            .split_once('=')
            .with_context(|| format!("--supply `{s}` should look like VDD=1.62,1.8,1.98"))?;
        let values: Vec<&str> = values.split(',').map(str::trim).collect();
        supply_list.push(json!({"target": target.trim(), "values": values}));
    }
    Ok(Some(json!({
        "process": process.iter().map(|p| p.trim()).collect::<Vec<_>>(),
        "temperatures": temperatures.iter().map(|t| t.trim()).collect::<Vec<_>>(),
        "supplies": supply_list,
    })))
}

pub fn check(
    file: &Path,
    symbols: &[PathBuf],
    specs: Option<PathBuf>,
    simulator: Option<String>,
    corners: Option<Value>,
    json_mode: bool,
) -> Result<ExitCode> {
    let (ws, circuit) = workspace_for(file, symbols)?;
    let spec_text = match specs {
        Some(p) => {
            Some(std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)
        }
        None => None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let out = rt.block_on(call(
        &ws,
        "check_specs",
        json!({"circuit": circuit, "specs": spec_text, "save": false, "simulator": simulator, "corners": corners}),
    ))?;
    print(&out, json_mode);
    if out.is_error {
        bail!("{}", out.text_content());
    }
    let pass = out
        .data
        .as_ref()
        .and_then(|d| d["report"]["all_pass"].as_bool())
        .unwrap_or(false);
    Ok(if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
