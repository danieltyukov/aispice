//! The `pdk` tool against a fake IHP PDK: listing, describing, devices, and
//! opting a project in through the approval hook.

use aispice_agent::ToolContext;
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Approver, Hooks, Project, RunnerConfig, Workspace, registry};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn workspace(register: bool) -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    aispice_sim::pdk::fake_ihp(dir.path());
    let mut pdks = Registry::default();
    if register {
        pdks.add("ihp-sg13g2", &dir.path().join("IHP-Open-PDK"))
            .unwrap();
    }
    let root = dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    let ws = Workspace::new();
    ws.runner.set_config(RunnerConfig {
        pdks,
        ..RunnerConfig::default()
    });
    ws.set_project(Project::open(&root, Default::default()).unwrap());
    (dir, Arc::new(ws))
}

async fn call(ws: &Arc<Workspace>, input: Value) -> (String, bool) {
    let reg = registry(ws.clone());
    let out = reg
        .get("pdk")
        .expect("pdk tool")
        .call(&ToolContext::default(), input)
        .await;
    (out.text_content(), out.is_error)
}

#[tokio::test]
async fn list_show_and_devices() {
    let (_d, ws) = workspace(true);
    let (text, err) = call(&ws, json!({"action": "list"})).await;
    assert!(
        !err && text.contains("ihp-sg13g2:") && text.contains("uses no PDK"),
        "{text}"
    );
    let (text, err) = call(&ws, json!({"action": "show", "name": "ihp-sg13g2"})).await;
    assert!(!err, "{text}");
    assert!(text.contains("cornerMOSlv.lib [mos]: mos_tt"), "{text}");
    assert!(text.contains("cornerHBT.lib hbt_typ"), "{text}");
    assert!(text.contains("psp103.osdi"), "{text}");
    let (text, err) = call(&ws, json!({"action": "devices"})).await;
    assert!(
        !err && text.contains(
            "symbol sg13g2\\sg13_lv_nmos; pins D G S B; defaults w=0.15u l=0.13u ng=1 m=1"
        ),
        "{text}"
    );
    assert!(text.contains("symbol sg13g2\\rppd; pins P M BN;"), "{text}");
    let (text, err) = call(&ws, json!({"action": "show"})).await;
    assert!(err && text.contains("needs a PDK name"), "{text}");
}

#[tokio::test]
async fn use_writes_the_project_file_after_approval() {
    struct Once(AtomicBool);
    #[async_trait::async_trait]
    impl Approver for Once {
        async fn approve(&self, circuit: &str, _summary: &str, diff: &str) -> bool {
            assert_eq!(circuit, PROJECT_FILE);
            assert!(diff.contains("pdk = \"ihp-sg13g2\""), "{diff}");
            // Decline the first request, accept the next.
            self.0.swap(true, Ordering::SeqCst)
        }
    }
    let (_d, ws) = workspace(true);
    ws.set_hooks(Hooks {
        approver: Some(Arc::new(Once(AtomicBool::new(false)))),
        after_save: None,
    });
    let file = ws.project().unwrap().root().join(PROJECT_FILE);
    std::fs::write(&file, "keep = true\n").unwrap();
    let (text, err) = call(&ws, json!({"action": "use", "name": "ihp-sg13g2"})).await;
    assert!(!err && text.contains("declined"), "{text}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep = true\n");
    let (text, err) = call(&ws, json!({"action": "use", "name": "ihp-sg13g2"})).await;
    assert!(!err && text.contains("now uses PDK ihp-sg13g2"), "{text}");
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("pdk = \"ihp-sg13g2\"") && written.contains("keep = true"),
        "{written}"
    );
    let (text, _) = call(&ws, json!({"action": "list"})).await;
    assert!(text.contains("uses `ihp-sg13g2`"), "{text}");
}

#[tokio::test]
async fn only_registered_pdks_can_be_used() {
    let (_d, ws) = workspace(false);
    let (text, err) = call(&ws, json!({"action": "use", "name": "ihp-sg13g2"})).await;
    assert!(err && text.contains("none are registered"), "{text}");
    let (text, err) = call(&ws, json!({"action": "list"})).await;
    assert!(!err && text.contains("No PDK is registered"), "{text}");
    assert!(!ws.project().unwrap().root().join(PROJECT_FILE).exists());
}

#[tokio::test]
async fn the_netlist_tool_shows_the_model_header() {
    let (_d, ws) = workspace(true);
    let root = ws.project().unwrap().root().to_path_buf();
    std::fs::write(root.join(PROJECT_FILE), "pdk = \"ihp-sg13g2\"\n").unwrap();
    std::fs::write(
        root.join("m.asc"),
        "Version 4\nSHEET 1 880 680\nFLAG 48 0 d\nFLAG 0 48 g\nFLAG 48 96 0\nFLAG 48 48 0\nSYMBOL sg13g2\\sg13_lv_nmos 0 0 R0\nSYMATTR InstName M1\nTEXT 0 160 Left 2 !.op\n",
    )
    .unwrap();
    let reg = registry(ws.clone());
    let out = reg
        .get("netlist")
        .unwrap()
        .call(&ToolContext::default(), json!({"circuit": "m.asc"}))
        .await;
    let text = out.text_content();
    assert!(
        text.contains("XM1 d g 0 0 sg13_lv_nmos w=0.15u l=0.13u ng=1 m=1"),
        "{text}"
    );
    assert!(text.contains("cornerMOSlv.lib mos_tt"), "{text}");
    std::fs::write(root.join(PROJECT_FILE), "pdk = \"missing\"\n").unwrap();
    let out = reg
        .get("netlist")
        .unwrap()
        .call(&ToolContext::default(), json!({"circuit": "m.asc"}))
        .await;
    let text = out.text_content();
    assert!(
        !out.is_error && text.contains("* warning: this project uses PDK `missing`"),
        "{text}"
    );
}

/// A project's `aispice.toml` shipped as a link to a file elsewhere is not
/// read: no tool quotes it, and `use` does not write through it.
#[cfg(unix)]
#[tokio::test]
async fn a_linked_project_file_is_not_read_or_written() {
    let (d, ws) = workspace(true);
    let root = ws.project().unwrap().root().to_path_buf();
    let secret = d.path().join("secret.env");
    std::fs::write(&secret, "API_KEY=sk-live-0123456789\n").unwrap();
    std::os::unix::fs::symlink(&secret, root.join(PROJECT_FILE)).unwrap();
    std::fs::write(
        root.join("r.asc"),
        "Version 4\nSHEET 1 880 680\nFLAG 0 0 a\nSYMBOL res 0 0 R0\nSYMATTR InstName R1\nSYMATTR Value 1k\nTEXT 0 160 Left 2 !.op\n",
    )
    .unwrap();
    let reg = registry(ws.clone());
    let out = reg
        .get("netlist")
        .unwrap()
        .call(&ToolContext::default(), json!({"circuit": "r.asc"}))
        .await;
    let text = out.text_content();
    assert!(!text.contains("sk-live") && text.contains("link"), "{text}");

    // A link to another project file: `use` must not overwrite that file.
    std::fs::remove_file(root.join(PROJECT_FILE)).unwrap();
    std::fs::write(root.join("notes.toml"), "keep = 1\n").unwrap();
    std::os::unix::fs::symlink(root.join("notes.toml"), root.join(PROJECT_FILE)).unwrap();
    let (text, err) = call(&ws, json!({"action": "use", "name": "ihp-sg13g2"})).await;
    assert!(err && text.contains("link"), "{text}");
    assert_eq!(
        std::fs::read_to_string(root.join("notes.toml")).unwrap(),
        "keep = 1\n"
    );
}

/// Registering a PDK trusts its compiled code, so no tool can do it: the
/// pdk tool only lists, shows, lists devices and opts a project in.
#[tokio::test]
async fn no_tool_can_register_a_pdk() {
    let (d, ws) = workspace(false);
    let reg = registry(ws.clone());
    let spec = reg.get("pdk").unwrap().spec();
    let actions = spec.input_schema.to_string();
    for allowed in ["list", "show", "devices", "use"] {
        assert!(actions.contains(&format!("\"{allowed}\"")), "{actions}");
    }
    for attempt in [
        json!({"action": "add", "name": "evil", "path": d.path()}),
        json!({"action": "register", "name": "evil", "path": d.path()}),
        json!({"action": "use", "name": "evil", "path": d.path().join("IHP-Open-PDK")}),
    ] {
        let (text, err) = call(&ws, attempt.clone()).await;
        assert!(err, "{attempt}: {text}");
    }
    assert!(ws.runner.config().pdks.pdks.is_empty());
    for t in reg.specs() {
        let name = t.name.to_ascii_lowercase();
        assert!(
            !(name.contains("register") || name.contains("pdk_add")),
            "{name}"
        );
    }
}
