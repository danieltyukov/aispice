//! The export tool asks the user before xschem runs on the file, and a
//! declined export runs nothing. Gated on an installed xschem library and a
//! sandbox that starts here; elsewhere xschem would not run either way.
//!
//! The one test in this file points `AISPICE_XSCHEM` at a stand-in that
//! spins until the run's timeout, so if xschem ran before the question the
//! approval would come about 30 seconds late. It is alone in its own binary
//! because it sets the environment, before any thread starts.
#![cfg(unix)]

use aispice_agent::ToolContext;
use aispice_sim::xschem::{Sandbox, Xschem};
use aispice_tools::{Approver, Hooks, Project, Workspace, registry};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

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
"#;

/// Approves the import, declines the export, and notes when it was asked.
struct DeclineExport {
    asked: Mutex<Option<Instant>>,
}

#[async_trait::async_trait]
impl Approver for DeclineExport {
    async fn approve(&self, _circuit: &str, summary: &str, _diff: &str) -> bool {
        if summary.starts_with("Create out.sch") {
            *self.asked.lock().unwrap() = Some(Instant::now());
            false
        } else {
            true
        }
    }
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> (String, bool) {
    let out = registry(ws.clone())
        .get(tool)
        .unwrap_or_else(|| panic!("{tool}"))
        .call(&ToolContext::default(), input)
        .await;
    (out.text_content(), out.is_error)
}

#[test]
#[allow(unsafe_code)]
fn the_export_is_approved_before_xschem_runs() {
    use std::os::unix::fs::PermissionsExt;
    let Some(share) = Xschem::find().and_then(|x| x.share) else {
        eprintln!("skipped: xschem's library is not installed");
        return;
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if let Err(e) = rt.block_on(Sandbox::select(&CancellationToken::new())) {
        eprintln!("skipped: no sandbox here: {e}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let stand_in = dir.join("xschem");
    std::fs::write(&stand_in, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
    std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: the only test in this binary, on the main thread, and the
    // runtime above is single-threaded and idle, so nothing reads the
    // environment while it changes.
    unsafe {
        std::env::set_var("AISPICE_XSCHEM", &stand_in);
        std::env::set_var("AISPICE_XSCHEM_SHAREDIR", &share);
    }
    let root = dir.join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("rc.sch"), RC).unwrap();
    let ws = Workspace::new();
    ws.set_project(Project::open(&root, Default::default()).unwrap());
    let ws = Arc::new(ws);
    let approver = Arc::new(DeclineExport {
        asked: Mutex::new(None),
    });
    ws.set_hooks(Hooks {
        approver: Some(approver.clone()),
        after_save: None,
    });
    rt.block_on(async {
        let (text, err) = call(
            &ws,
            "xschem_import",
            json!({"schematic": "rc.sch", "circuit": "rc.asc", "effort": 1}),
        )
        .await;
        assert!(!err, "{text}");
        let start = Instant::now();
        let (text, err) = call(
            &ws,
            "xschem_export",
            json!({"circuit": "rc.asc", "schematic": "out.sch"}),
        )
        .await;
        let asked = approver
            .asked
            .lock()
            .unwrap()
            .expect("the export was not approved");
        assert!(!err && text.contains("declined"), "{text}");
        // Asked before any xschem run, and nothing ran after the refusal.
        assert!(
            asked - start < Duration::from_secs(10),
            "asked after {:?}",
            asked - start
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        assert!(!root.join("out.sch").exists());
    });
}
