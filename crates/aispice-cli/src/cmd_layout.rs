//! `aispice layout`, `drc`, `lvs` and `extract`: the layout tools from the
//! command line, on the project folder of the file given (whose
//! `aispice.toml` names the PDK). Exit code 1 when a check fails, for CI.

use crate::cmd_sim::workspace_for;
use aispice_agent::{ContentBlock, ToolContext, ToolOutput};
use aispice_tools::{Workspace, registry};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> Result<ToolOutput> {
    let reg = registry(ws.clone());
    let t = reg.get(tool).context("tool missing")?;
    Ok(t.call(&ToolContext::default(), input).await)
}

pub fn run_tool(ws: &Arc<Workspace>, tool: &str, input: Value) -> Result<ToolOutput> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(call(ws, tool, input))
}

fn print(out: &ToolOutput, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&out.data).unwrap_or_default()
        );
    } else {
        let text = out.text_content();
        print!("{}", crate::cmd_agent::terminal_safe(&text));
        if !text.ends_with('\n') {
            println!();
        }
    }
}

/// A file's path relative to the project folder `ws` was opened on.
pub fn in_project(ws: &Workspace, file: &Path) -> Result<String> {
    let p = ws.project()?;
    let abs =
        std::fs::canonicalize(file).with_context(|| format!("{} not found", file.display()))?;
    let rel = abs.strip_prefix(p.root()).with_context(|| {
        format!(
            "{} is not inside the project folder {}",
            file.display(),
            p.root().display()
        )
    })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

pub fn done(out: &ToolOutput, json_mode: bool, ok: bool) -> Result<ExitCode> {
    print(out, json_mode);
    if out.is_error {
        bail!("{}", out.text_content());
    }
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

pub struct DrcArgs {
    pub gds: PathBuf,
    pub cell: Option<String>,
    pub no_extra: bool,
    pub no_antenna: bool,
    pub density: bool,
}

pub fn drc(a: DrcArgs, symbols: &[PathBuf], json_mode: bool) -> Result<ExitCode> {
    let (ws, gds) = workspace_for(&a.gds, symbols)?;
    let out = run_tool(
        &ws,
        "drc",
        json!({"layout": gds, "cell": a.cell, "extra": !a.no_extra,
               "antenna": !a.no_antenna, "density": a.density}),
    )?;
    let clean = out
        .data
        .as_ref()
        .and_then(|d| d["value"]["drc"]["rules"].as_array().map(Vec::is_empty))
        .unwrap_or(false);
    done(&out, json_mode, clean)
}

pub fn lvs(
    gds: &Path,
    circuit: &Path,
    cell: Option<String>,
    symbols: &[PathBuf],
    json_mode: bool,
) -> Result<ExitCode> {
    let (ws, name) = workspace_for(circuit, symbols)?;
    let layout = in_project(&ws, gds)?;
    let out = run_tool(
        &ws,
        "lvs",
        json!({"layout": layout, "circuit": name, "cell": cell}),
    )?;
    let matched = out
        .data
        .as_ref()
        .and_then(|d| d["value"]["lvs"]["matched"].as_bool())
        .unwrap_or(false);
    done(&out, json_mode, matched)
}

pub fn extract(
    gds: &Path,
    cell: Option<String>,
    resistance: bool,
    symbols: &[PathBuf],
    json_mode: bool,
) -> Result<ExitCode> {
    let (ws, layout) = workspace_for(gds, symbols)?;
    let out = run_tool(
        &ws,
        "extract",
        json!({"layout": layout, "cell": cell, "resistance": resistance}),
    )?;
    done(&out, json_mode, true)
}

pub struct GenerateArgs {
    pub circuit: PathBuf,
    pub template: String,
    pub roles: Option<String>,
    pub params: Option<String>,
    pub cell: Option<String>,
    pub output: Option<PathBuf>,
    pub no_check: bool,
}

pub fn json_arg(what: &str, text: Option<&str>) -> Result<Value> {
    match text {
        Some(t) => serde_json::from_str(t).with_context(|| format!("--{what} is not JSON")),
        None => Ok(Value::Null),
    }
}

pub fn generate(a: GenerateArgs, symbols: &[PathBuf], json_mode: bool) -> Result<ExitCode> {
    let (ws, circuit) = workspace_for(&a.circuit, symbols)?;
    let layout = match &a.output {
        Some(o) => {
            // The output need not exist yet; its folder must be in the project.
            let dir = o.parent().filter(|d| !d.as_os_str().is_empty());
            let base = match dir {
                Some(d) => in_project(&ws, d)?,
                None => String::new(),
            };
            let name = o
                .file_name()
                .context("--output names no file")?
                .to_string_lossy()
                .into_owned();
            Some(if base.is_empty() {
                name
            } else {
                format!("{base}/{name}")
            })
        }
        None => None,
    };
    let out = run_tool(
        &ws,
        "layout",
        json!({
            "action": "generate", "circuit": circuit, "template": a.template,
            "roles": json_arg("roles", a.roles.as_deref())?,
            "params": json_arg("params", a.params.as_deref())?,
            "cell": a.cell, "layout": layout, "check": !a.no_check,
        }),
    )?;
    let ok = out.data.as_ref().is_some_and(|d| {
        let v = &d["value"];
        a.no_check
            || (v["drc"]["rules"].as_array().is_some_and(Vec::is_empty)
                && v["lvs"]["matched"].as_bool() == Some(true))
    });
    done(&out, json_mode, ok)
}

pub fn render(
    gds: &Path,
    output: &Path,
    cell: Option<String>,
    size: (u32, u32),
    symbols: &[PathBuf],
) -> Result<ExitCode> {
    let (ws, layout) = workspace_for(gds, symbols)?;
    let out = run_tool(
        &ws,
        "layout",
        json!({"action": "render", "layout": layout, "cell": cell, "size": [size.0, size.1]}),
    )?;
    if out.is_error {
        bail!("{}", out.text_content());
    }
    let png = out
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image { data_base64, .. } => Some(data_base64.clone()),
            _ => None,
        })
        .context("the renderer returned no image")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
    std::fs::write(output, bytes).with_context(|| format!("writing {}", output.display()))?;
    println!("wrote {}", output.display());
    Ok(ExitCode::SUCCESS)
}

pub struct PostLayoutArgs<'a> {
    pub file: &'a Path,
    pub layout: &'a Path,
    pub resistance: bool,
    pub specs: Option<String>,
    pub simulator: Option<String>,
    /// A corner set from `--process`, `--temp` and `--supply`.
    pub corners: Option<Value>,
    pub symbols: &'a [PathBuf],
    pub json_mode: bool,
}

/// `aispice check --post-layout`: specs before and after layout (at every
/// corner when a corner set is given). Exit code 1 when a spec fails after
/// layout.
pub fn check_post_layout(a: PostLayoutArgs) -> Result<ExitCode> {
    let (ws, circuit) = workspace_for(a.file, a.symbols)?;
    let gds = in_project(&ws, a.layout)?;
    let out = run_tool(
        &ws,
        "check_specs",
        json!({"circuit": circuit, "specs": a.specs, "save": false, "simulator": a.simulator,
               "corners": a.corners,
               "post_layout": {"layout": gds, "resistance": a.resistance}}),
    )?;
    let pass = out
        .data
        .as_ref()
        .and_then(|d| d["value"]["post"]["all_pass"].as_bool())
        .unwrap_or(false);
    done(&out, a.json_mode, pass)
}
