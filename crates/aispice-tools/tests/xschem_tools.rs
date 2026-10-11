//! The xschem tools through the registry: import and export write files only
//! after approval, never overwrite, report findings and proofs, and treat a
//! hostile schematic as data.

use aispice_agent::ToolContext;
use aispice_tools::{Approver, Hooks, Project, Workspace, registry};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const RC: &str = r#"v {xschem version=3.4.4 file_version=1.2}
N 100 -200 200 -200 {lab=in}
N 260 -200 300 -200 {lab=out}
N 300 -200 300 -160 {lab=out}
N 100 -200 100 -170 {lab=in}
C {devices/vsource.sym} 100 -140 0 0 {name=V1 value="dc 0 ac 1"}
C {devices/res.sym} 230 -200 1 0 {name=R1 value=1k}
C {devices/capa.sym} 300 -130 0 0 {name=C1 value=1n}
C {devices/gnd.sym} 100 -110 0 0 {name=l1 lab=GND}
C {devices/gnd.sym} 300 -100 0 0 {name=l2 lab=GND}
C {devices/lab_pin.sym} 300 -200 0 0 {name=p1 lab=out}
C {devices/lab_pin.sym} 100 -200 0 0 {name=p2 lab=in}
C {devices/code_shown.sym} 400 -200 0 0 {name=s1 only_toplevel=false value=".ac dec 10 1 1meg"}
"#;

fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("rc.sch"), RC).unwrap();
    let ws = Workspace::new();
    ws.set_project(Project::open(&root, Default::default()).unwrap());
    (dir, Arc::new(ws))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> (String, bool, Value) {
    let reg = registry(ws.clone());
    let out = reg
        .get(tool)
        .unwrap_or_else(|| panic!("{tool}"))
        .call(&ToolContext::default(), input)
        .await;
    (
        out.text_content(),
        out.is_error,
        out.data.unwrap_or(Value::Null),
    )
}

/// The xschem proof ran and agreed, or was skipped with a reason (no
/// xschem on this machine).
fn xschem_proof_ok(data: &Value) {
    let x = &data["proofs"]["xschem"];
    match x["status"].as_str() {
        Some("ran") => assert_eq!(x["proof"]["equivalent"], json!(true), "{x}"),
        Some("skipped") => assert!(x["reason"].as_str().is_some_and(|r| !r.is_empty())),
        _ => panic!("{data}"),
    }
}

#[tokio::test]
async fn import_creates_an_asc_with_proofs() {
    let (_d, ws) = workspace();
    let (text, err, data) = call(
        &ws,
        "xschem_import",
        json!({"schematic": "rc.sch", "effort": 1}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(text.contains("Created rc.asc from rc.sch"), "{text}");
    assert!(text.contains("Proof, aispice: same circuit"), "{text}");
    assert_eq!(data["kind"], "schematic");
    assert_eq!(data["circuit"], "rc.asc");
    assert_eq!(
        data["xschem"]["proofs"]["aispice"]["equivalent"],
        json!(true)
    );
    xschem_proof_ok(&data["xschem"]);
    let root = ws.project().unwrap().root().to_path_buf();
    let asc = std::fs::read_to_string(root.join("rc.asc")).unwrap();
    assert!(asc.contains("SYMBOL res"), "{asc}");
    // Never overwrites.
    let (text, err, _) = call(&ws, "xschem_import", json!({"schematic": "rc.sch"})).await;
    assert!(err && text.contains("already exists"), "{text}");
    // Only .sch files, inside the project.
    let (text, err, _) = call(&ws, "xschem_import", json!({"schematic": "../x.sch"})).await;
    assert!(err, "{text}");
    let (text, err, _) = call(&ws, "xschem_import", json!({"schematic": "rc.asc"})).await;
    assert!(err && text.contains(".sch"), "{text}");
}

#[tokio::test]
async fn export_writes_a_sch_with_proofs() {
    let (_d, ws) = workspace();
    let (text, err, _) = call(
        &ws,
        "xschem_import",
        json!({"schematic": "rc.sch", "circuit": "rc_in.asc", "effort": 1}),
    )
    .await;
    assert!(!err, "{text}");
    let (text, err, data) = call(
        &ws,
        "xschem_export",
        json!({"circuit": "rc_in.asc", "schematic": "rc_out.sch"}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(text.contains("Created rc_out.sch from rc_in.asc"), "{text}");
    assert_eq!(data["kind"], "generic");
    let value = &data["value"];
    assert_eq!(
        value["proofs"]["aispice"]["equivalent"],
        json!(true),
        "{value}"
    );
    xschem_proof_ok(value);
    let root = ws.project().unwrap().root().to_path_buf();
    let sch = std::fs::read_to_string(root.join("rc_out.sch")).unwrap();
    assert!(sch.contains("C {devices/res.sym}"), "{sch}");
    let (text, err, _) = call(
        &ws,
        "xschem_export",
        json!({"circuit": "rc_in.asc", "schematic": "rc_out.sch"}),
    )
    .await;
    assert!(err && text.contains("already exists"), "{text}");
}

#[tokio::test]
async fn nothing_is_written_when_the_user_declines() {
    struct No(AtomicBool);
    #[async_trait::async_trait]
    impl Approver for No {
        async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
            self.0.store(true, Ordering::SeqCst);
            false
        }
    }
    let (_d, ws) = workspace();
    let asked = Arc::new(No(AtomicBool::new(false)));
    ws.set_hooks(Hooks {
        approver: Some(asked.clone()),
        after_save: None,
    });
    let (text, err, _) = call(
        &ws,
        "xschem_import",
        json!({"schematic": "rc.sch", "effort": 1}),
    )
    .await;
    assert!(!err && text.contains("declined"), "{text}");
    assert!(asked.0.load(Ordering::SeqCst));
    assert!(!ws.project().unwrap().root().join("rc.asc").exists());
}

#[tokio::test]
async fn hostile_schematics_are_data() {
    let (_d, ws) = workspace();
    let root = ws.project().unwrap().root().to_path_buf();
    let marker = root.join("pwned");
    let hostile = format!(
        "v {{xschem version=3.4.4 file_version=1.2}}\nC {{devices/res.sym}} 0 0 0 0 {{name=R1 value=\"tcleval([exec touch {m}])\"}}\nC {{devices/vsource.sym}} 100 0 0 0 {{name=V1 value=\"[exec touch {m}]\"}}\nC {{devices/res.sym}} 200 0 0 0 {{name=R2 value=1k}}\nC {{devices/gnd.sym}} 200 30 0 0 {{name=l1 lab=GND}}\n",
        m = marker.display()
    );
    std::fs::write(root.join("evil.sch"), hostile).unwrap();
    let (text, err, data) = call(
        &ws,
        "xschem_import",
        json!({"schematic": "evil.sch", "effort": 1}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(text.contains("R1: ") && text.contains("Tcl"), "{text}");
    // xschem is never run on it.
    let x = &data["xschem"]["proofs"]["xschem"];
    assert_eq!(x["status"], "skipped", "{x}");
    assert!(!marker.exists());
}

#[tokio::test]
async fn import_refuses_what_the_layout_cannot_draw() {
    let (_d, ws) = workspace();
    let root = ws.project().unwrap().root().to_path_buf();
    let big: String = std::iter::once("v {xschem version=3.4.4 file_version=1.2}\n".to_string())
        .chain((0..200).map(|i| {
            format!(
                "C {{devices/res.sym}} {} 0 0 0 {{name=R{i} value=1k}}\n",
                i * 100
            )
        }))
        .collect();
    std::fs::write(root.join("big.sch"), big).unwrap();
    let (text, err, _) = call(&ws, "xschem_import", json!({"schematic": "big.sch"})).await;
    assert!(err && text.contains("200 elements"), "{text}");
    assert!(!root.join("big.asc").exists());
}
