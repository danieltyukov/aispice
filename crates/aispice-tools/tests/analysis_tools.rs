//! Noise, PSRR and CMRR, corners and mismatch Monte Carlo through the tools,
//! on ngspice (and Xyce where a test says so), with small libraries written
//! for the tests. Skipped when the simulator is missing.

use aispice_agent::{ToolContext, ToolOutput};
use aispice_tools::{Project, ProjectOptions, Workspace, registry};
use serde_json::{Value, json};
use std::sync::Arc;

fn have(program: &str, arg: &str) -> bool {
    std::process::Command::new(program)
        .arg(arg)
        .output()
        .is_ok()
}

fn have_ngspice() -> bool {
    let ok = have("ngspice", "--version");
    if !ok {
        eprintln!("skipped: ngspice not installed");
    }
    ok
}

/// A project holding the given files.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let project = Project::open(dir.path(), ProjectOptions::default()).unwrap();
    (dir, Arc::new(Workspace::with_project(project)))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> ToolOutput {
    let reg = registry(ws.clone());
    reg.get(tool)
        .unwrap()
        .call(&ToolContext::default(), input)
        .await
}

fn ok(out: &ToolOutput) -> String {
    let text = out.text_content();
    assert!(!out.is_error, "{text}");
    text
}

/// Was a bug: model resolution dropped an `.include` of a file beside the
/// circuit, because it kept only absolute paths, so the run on ngspice
/// failed for want of the model.
#[tokio::test(flavor = "multi_thread")]
async fn includes_beside_the_circuit_reach_ngspice() {
    if !have_ngspice() {
        return;
    }
    let (_dir, ws) = project(&[
        ("toy.lib", "* toy diode\n.model dtoy D(IS=1e-14)\n"),
        (
            "d.cir",
            "* include test\n.include toy.lib\nI1 0 a 1m\nD1 a 0 dtoy\n.op\n.end\n",
        ),
    ]);
    let out = call(
        &ws,
        "simulate",
        json!({"circuit": "d.cir", "simulator": "ngspice", "measurements": ["v = value_at(V(a), 0)"]}),
    )
    .await;
    let text = ok(&out);
    assert!(text.contains("v = 655"), "{text}");
}
