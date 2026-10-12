//! The proof behind the IC templates: each one, copied into a project that
//! uses IHP SG13G2 the way an agent would, meets every spec in its table at
//! every corner of the set it names, with room to spare, on ngspice and on
//! Xyce where the template lists it.
//!
//! Needs the PDK: `AISPICE_PDK_IHP=<path to IHP-Open-PDK/ihp-sg13g2>` with
//! its OSDI files compiled. The Xyce half also needs the PDK's Xyce plugins
//! and a Xyce built with plugin support (`AISPICE_XYCE`, else `Xyce` on
//! PATH). Every test passes, with a note on stderr, when the PDK is not
//! there.

use aispice_agent::{Tool, ToolContext, ToolOutput};
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::tools::{CheckSpecs, Templates};
use aispice_tools::{Project, RunnerConfig, Workspace, templates};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

fn ihp() -> Option<PathBuf> {
    let p = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from);
    if p.is_none() {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
    }
    p
}

fn xyce_exe() -> Option<PathBuf> {
    std::env::var_os("AISPICE_XYCE")
        .map(PathBuf::from)
        .or_else(|| aispice_sim::backend::which("Xyce"))
}

/// A project that uses the PDK, registered as `ihp`, with aispice's own
/// symbols and models only.
fn workspace(pdk: &std::path::Path) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp", pdk).unwrap();
    std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"ihp\"\n").unwrap();
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        pdks,
        xyce_exe: xyce_exe(),
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    ws.set_project(Project::open(dir.path(), Default::default()).unwrap());
    (dir, Arc::new(ws))
}

/// Whether Xyce can run the PDK here; says why not when it cannot.
fn xyce_ready(ws: &Workspace) -> bool {
    let pdk = ws.runner.config().pdks.open("ihp").unwrap();
    if pdk.xyce_plugins.is_empty() {
        eprintln!("Xyce half skipped: the PDK has no Xyce plugins (libs.tech/xyce/plugins)");
        return false;
    }
    if xyce_exe().is_none() {
        eprintln!("Xyce half skipped: no Xyce");
        return false;
    }
    true
}

async fn call(tool: &dyn Tool, input: Value) -> ToolOutput {
    tool.call(&ToolContext::default(), input).await
}

/// How close a spec's worst value may sit to its limits, as for the other
/// templates: a two-sided spec inside the middle 70% of its window, a
/// one-sided one passing by at least 5% of its limit.
fn comfortable(row: &Value) -> bool {
    let (Some(v), min, max) = (
        row["value"].as_f64(),
        row["min"].as_f64(),
        row["max"].as_f64(),
    ) else {
        return false;
    };
    match (min, max) {
        (Some(lo), Some(hi)) => {
            let edge = 0.15 * (hi - lo);
            v >= lo + edge && v <= hi - edge
        }
        _ => aispice_sim::spec::margin(v, min, max).is_some_and(|m| m >= 0.05),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_ic_template_meets_its_specs_at_every_corner() {
    let Some(pdk) = ihp() else { return };
    let (_dir, ws) = workspace(&pdk);
    let xyce = xyce_ready(&ws);
    let use_tool = Templates { ws: ws.clone() };
    let check = CheckSpecs { ws: ws.clone() };
    let mut failures = Vec::new();
    let mut checked = 0;
    for t in templates::all().iter().filter(|t| t.pdk.is_some()) {
        let circuit = format!("{}.asc", t.id);
        let out = call(
            &use_tool,
            json!({"action": "use", "id": t.id, "circuit": circuit}),
        )
        .await;
        assert!(!out.is_error, "{}: {}", t.id, out.text_content());
        for sim in &t.simulators {
            if sim == "xyce" && !xyce {
                continue;
            }
            // The corner set exactly as `templates show` hands it out.
            let out = call(
                &check,
                json!({"circuit": circuit, "simulator": sim, "corners": t.corners}),
            )
            .await;
            let text = out.text_content();
            assert!(!out.is_error, "{} on {sim}: {text}", t.id);
            println!("{} on {sim}:\n{text}", t.id);
            let data = out.data.unwrap();
            let corners = t.corners.as_ref().unwrap().count().unwrap();
            // The simulator asked for ran every corner (`on Xyce`).
            assert!(
                text.to_lowercase()
                    .starts_with(&format!("{circuit} at {corners} corners on {sim}")),
                "{text}"
            );
            for row in data["report"]["rows"].as_array().unwrap() {
                if !(row["pass"].as_bool() == Some(true) && comfortable(row)) {
                    failures.push(format!(
                        "{} on {sim}: {} = {} (min {}, max {})",
                        t.id, row["name"], row["display"], row["min"], row["max"]
                    ));
                }
            }
            checked += 1;
        }
    }
    assert!(checked >= 6, "{checked} template and simulator pairs");
    assert!(
        failures.is_empty(),
        "specs failing or too close to a limit at their worst corner:\n{}",
        failures.join("\n")
    );
}
