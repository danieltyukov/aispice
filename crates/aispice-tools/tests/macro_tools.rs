//! The check_macro tool on the shared macro package fixtures, inside a
//! project folder.

use aispice_agent::{ContentBlock, ToolContext, ToolOutput};
use aispice_tools::{Project, ProjectOptions, Workspace, registry};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../aispice-core/tests/fixtures/macro")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

fn workspace() -> (tempfile::TempDir, Arc<Workspace>) {
    let dir = tempfile::tempdir().unwrap();
    let project = Project::open(dir.path(), ProjectOptions::default()).unwrap();
    (dir, Arc::new(Workspace::with_project(project)))
}

async fn call(ws: &Arc<Workspace>, input: serde_json::Value) -> ToolOutput {
    registry(ws.clone())
        .get("check_macro")
        .unwrap()
        .call(&ToolContext::default(), input)
        .await
}

fn text_of(out: &ToolOutput) -> String {
    out.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn check_macro_reports_the_shared_findings() {
    let (dir, ws) = workspace();
    copy_dir(
        &fixtures().join("tiny_ota.macro"),
        &dir.path().join("macros/tiny_ota.macro"),
    );
    copy_dir(
        &fixtures().join("broken/lef-missing-pin/tiny_ota.macro"),
        &dir.path().join("broken/tiny_ota.macro"),
    );

    let out = call(
        &ws,
        json!({"path": "macros/tiny_ota.macro", "pdk": "ihp-sg13g2@22f2a25"}),
    )
    .await;
    let text = text_of(&out);
    assert!(!out.is_error, "{text}");
    assert!(
        text.contains("macros/tiny_ota.macro: pass, 0 errors, 0 warnings"),
        "{text}"
    );
    let data = out.data.unwrap();
    assert_eq!(data["kind"], "generic");
    assert_eq!(data["value"]["pass"], true);

    let out = call(&ws, json!({"path": "broken/tiny_ota.macro"})).await;
    let text = text_of(&out);
    assert!(
        !out.is_error,
        "a failing package is a result, not a tool error"
    );
    assert!(
        text.contains(
            "error   lef.missing_pin tiny_ota.lef: pin \"inn\" from macro.toml is missing"
        ),
        "{text}"
    );
    assert_eq!(
        out.data.unwrap()["value"]["findings"][0]["rule"],
        "lef.missing_pin"
    );

    let out = call(
        &ws,
        json!({"path": "macros/tiny_ota.macro", "pdk": "sky130A"}),
    )
    .await;
    assert!(text_of(&out).contains("pdk.expected"));
}

#[tokio::test(flavor = "multi_thread")]
async fn check_macro_stays_in_the_project() {
    let (_dir, ws) = workspace();
    for path in ["../elsewhere.macro", "missing.macro", ""] {
        let out = call(&ws, json!({"path": path})).await;
        assert!(out.is_error, "{path}: {}", text_of(&out));
    }
    let out = call(&ws, json!({"path": "macros", "pdk": "x@not-hex"})).await;
    assert!(out.is_error);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn check_macro_refuses_links_out_of_the_project() {
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.lef"), "SECRET ;\n").unwrap();
    let pkg = dir.path().join("linked.macro");
    copy_dir(&fixtures().join("tiny_ota.macro"), &pkg);
    std::fs::remove_file(pkg.join("tiny_ota.lef")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.lef"), pkg.join("tiny_ota.lef"))
        .unwrap();
    let out = call(&ws, json!({"path": "linked.macro"})).await;
    let text = text_of(&out);
    assert!(
        out.is_error && text.contains("outside the project"),
        "{text}"
    );
    assert!(!text.contains("SECRET"));
}
