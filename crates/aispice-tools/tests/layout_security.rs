//! The layout tools follow the security model: each declares whether it
//! writes the user's files, only a tool declared to write can, and no tool
//! argument reaches outside the project (or a registered PDK) to read, or
//! outside it, through `..` or through a symlink to write. None of this
//! needs a PDK, KLayout or Magic: every check happens before them.

use aispice_agent::{Access, Tool, ToolContext, ToolOutput, ToolSpec};
use aispice_layout::gds::testwrite::{TestCell, library};
use aispice_tools::{Approver, FileWrite, Hooks, Project, Workspace};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

fn workspace(dir: &Path) -> Arc<Workspace> {
    Arc::new(Workspace::with_project(
        Project::open(dir, Default::default()).unwrap(),
    ))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> ToolOutput {
    aispice_tools::registry(ws.clone())
        .get(tool)
        .unwrap()
        .call(&ToolContext::default(), input)
        .await
}

const SPECS: &str = "gbw = unity_gain_freq(V(out)) >= 1Meg\n";

async fn generate_to(ws: &Arc<Workspace>, layout: &str) -> ToolOutput {
    call(
        ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.cir", "template": "ota5t", "layout": layout}),
    )
    .await
}

fn gds(path: &Path) {
    let cell = TestCell {
        name: "blk".into(),
        rects: vec![(8, 0, 0, 0, 1000, 1000)],
        ..Default::default()
    };
    std::fs::write(path, library(&[cell])).unwrap();
}

#[test]
fn every_tool_declares_what_it_does_to_files() {
    let dir = tempfile::tempdir().unwrap();
    let reg = aispice_tools::registry(workspace(dir.path()));
    let got: BTreeMap<String, Access> = reg
        .specs_with_access()
        .into_iter()
        .map(|(s, a)| (s.name, a))
        .collect();
    use Access::*;
    let expected: BTreeMap<String, Access> = [
        // The layout tools: only generate writes the user's files; DRC, LVS
        // and extraction keep their reports in aispice's run folders.
        ("layout", Edit),
        ("drc", ReadOnly),
        ("lvs", ReadOnly),
        ("extract", ReadOnly),
        ("list_circuits", ReadOnly),
        ("read_schematic", ReadOnly),
        ("edit_schematic", Edit),
        ("create_schematic", Edit),
        ("lint", ReadOnly),
        ("netlist", ReadOnly),
        ("history", Edit),
        ("render_schematic", ReadOnly),
        ("symbols", ReadOnly),
        ("simulators", ReadOnly),
        ("simulate", ReadOnly),
        ("measure", ReadOnly),
        ("check_specs", Edit),
        ("plot", ReadOnly),
        ("read_waveform", ReadOnly),
        ("poles_zeros", ReadOnly),
        ("operating_point", ReadOnly),
        ("sweep", ReadOnly),
        ("monte_carlo", ReadOnly),
        ("optimize", Edit),
        ("templates", Edit),
        ("pdk", Edit),
        ("gm_id", ReadOnly),
        ("check_macro", ReadOnly),
        // xschem import and export each write one new file of the user's.
        ("xschem_import", Edit),
        ("xschem_export", Edit),
    ]
    .into_iter()
    .map(|(n, a)| (n.to_string(), a))
    .collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn a_gds_outside_the_project_is_refused() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("blk.gds");
    gds(&outside);
    let ws = workspace(project.path());
    let abs = outside.display().to_string();
    for (tool, input) in [
        ("drc", json!({"layout": abs})),
        ("extract", json!({"layout": abs})),
        ("lvs", json!({"layout": abs, "circuit": "x.cir"})),
        ("layout", json!({"action": "render", "layout": abs})),
        (
            "check_specs",
            json!({"circuit": "x.cir", "specs": SPECS, "save": false,
                   "post_layout": {"layout": abs}}),
        ),
    ] {
        let out = call(&ws, tool, input).await;
        assert!(out.is_error, "{tool}: {}", out.text_content());
        assert!(
            out.text_content().contains("outside the project"),
            "{tool}: {}",
            out.text_content()
        );
    }
    // A link inside the project to it is followed and refused the same way.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, project.path().join("link.gds")).unwrap();
        let out = call(&ws, "drc", json!({"layout": "link.gds"})).await;
        assert!(
            out.is_error && out.text_content().contains("outside the project"),
            "{}",
            out.text_content()
        );
    }
    // So is `..`, and aispice's own folder.
    let out = call(&ws, "drc", json!({"layout": "../blk.gds"})).await;
    assert!(
        out.is_error && out.text_content().contains(".."),
        "{}",
        out.text_content()
    );
    std::fs::create_dir_all(project.path().join(".aispice")).unwrap();
    gds(&project.path().join(".aispice/blk.gds"));
    let out = call(&ws, "drc", json!({"layout": ".aispice/blk.gds"})).await;
    assert!(out.is_error, "{}", out.text_content());
}

#[tokio::test]
async fn an_output_path_with_dotdot_is_refused() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("ota.cir"), "* ota\n.op\n").unwrap();
    std::fs::create_dir(project.join("sub")).unwrap();
    let ws = workspace(&project);
    for bad in [
        "../evil.gds",
        "sub/../../evil.gds",
        "sub/../ota.gds",
        "~/evil.gds",
    ] {
        let out = generate_to(&ws, bad).await;
        assert!(out.is_error, "{bad}: {}", out.text_content());
        assert!(
            out.text_content().contains("outside"),
            "{bad}: {}",
            out.text_content()
        );
    }
    assert!(!parent.path().join("evil.gds").exists());
    assert!(!project.join("ota.gds").exists());
    // A cell name cannot carry a path either.
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.cir", "template": "ota5t", "cell": "../evil"}),
    )
    .await;
    assert!(out.is_error, "{}", out.text_content());
    assert!(!parent.path().join("evil.gds").exists());
}

#[tokio::test]
async fn an_absolute_output_path_outside_the_project_is_refused() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("ota.cir"), "* ota\n.op\n").unwrap();
    let ws = workspace(&project);
    let beside = parent.path().join("evil.gds").display().to_string();
    let root = if cfg!(windows) {
        "C:\\evil.gds"
    } else {
        "/evil.gds"
    };
    // On Windows `/etc/evil.gds` has a root but no drive: the current
    // drive's, not the project's.
    for bad in [beside.as_str(), root, "/etc/evil.gds"] {
        let out = generate_to(&ws, bad).await;
        assert!(out.is_error, "{bad}: {}", out.text_content());
        assert!(
            out.text_content().contains("outside"),
            "{bad}: {}",
            out.text_content()
        );
    }
    assert!(!parent.path().join("evil.gds").exists());
}

#[tokio::test]
async fn an_output_inside_aispice_state_folder_is_refused() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("ota.cir"), "* ota\n.op\n").unwrap();
    std::fs::create_dir_all(project.path().join(".aispice/layout")).unwrap();
    let ws = workspace(project.path());
    for bad in [".aispice/ota.gds", ".aispice/layout/ota.gds"] {
        let out = generate_to(&ws, bad).await;
        assert!(out.is_error, "{bad}: {}", out.text_content());
        assert!(
            out.text_content().contains("outside"),
            "{bad}: {}",
            out.text_content()
        );
    }
    let err = ws
        .write_files(&Writer, &[file(".aispice/blk.gds", b"x")], "test", "")
        .await
        .unwrap_err();
    assert!(err.contains("outside"), "{err}");
    assert!(!project.path().join(".aispice/blk.gds").exists());
}

#[tokio::test]
async fn a_file_the_layout_tool_did_not_make_is_never_replaced() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("ota.cir"), "* ota\n.op\n").unwrap();
    std::fs::write(project.path().join("ota.gds"), b"the user's layout").unwrap();
    let ws = workspace(project.path());
    // Refused before any PDK, program or approval is involved.
    let out = generate_to(&ws, "ota.gds").await;
    assert!(out.is_error, "{}", out.text_content());
    assert!(
        out.text_content()
            .contains("was not made by the layout tool"),
        "{}",
        out.text_content()
    );
    assert_eq!(
        std::fs::read(project.path().join("ota.gds")).unwrap(),
        b"the user's layout"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_output_folder_that_links_outside_is_refused() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("ota.cir"), "* ota\n.op\n").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), project.path().join("out")).unwrap();
    let ws = workspace(project.path());
    let out = generate_to(&ws, "out/ota.gds").await;
    assert!(out.is_error, "{}", out.text_content());
    assert!(
        out.text_content().contains("outside"),
        "{}",
        out.text_content()
    );
    let err = ws
        .write_files(&Writer, &[file("out/blk.gds", b"x")], "test", "")
        .await
        .unwrap_err();
    assert!(err.contains("outside"), "{err}");
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn check_specs_does_not_save_specs_through_a_link() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("ota.cir"), "* ota\n.op\n").unwrap();
    std::fs::write(project.path().join("notes.txt"), b"keep").unwrap();
    std::os::unix::fs::symlink(
        project.path().join("notes.txt"),
        project.path().join("ota.specs"),
    )
    .unwrap();
    let ws = workspace(project.path());
    // The simulation that follows may fail without a simulator; the save
    // comes first either way.
    call(
        &ws,
        "check_specs",
        json!({"circuit": "ota.cir", "specs": SPECS, "save": true}),
    )
    .await;
    assert_eq!(
        std::fs::read(project.path().join("notes.txt")).unwrap(),
        b"keep"
    );
    assert!(
        std::fs::symlink_metadata(project.path().join("ota.specs"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_output_target_is_refused() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("victim.gds");
    std::fs::write(&target, b"the user's file").unwrap();
    std::fs::write(project.path().join("ota.cir"), "* ota\n.op\n").unwrap();
    // The default output, ota.gds, is a link out of the project.
    std::os::unix::fs::symlink(&target, project.path().join("ota.gds")).unwrap();
    let ws = workspace(project.path());
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.cir", "template": "ota5t"}),
    )
    .await;
    assert!(out.is_error, "{}", out.text_content());
    assert!(
        out.text_content().contains("symbolic link"),
        "{}",
        out.text_content()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"the user's file");
    // The write path itself refuses a link even inside the project.
    std::fs::write(project.path().join("real.gds"), b"keep").unwrap();
    std::os::unix::fs::symlink(
        project.path().join("real.gds"),
        project.path().join("alias.gds"),
    )
    .unwrap();
    let err = ws
        .write_files(&Writer, &[file("alias.gds", b"new")], "test", "")
        .await
        .unwrap_err();
    assert!(err.contains("symbolic link"), "{err}");
    assert_eq!(
        std::fs::read(project.path().join("real.gds")).unwrap(),
        b"keep"
    );
}

fn file(rel: &str, bytes: &[u8]) -> FileWrite {
    FileWrite {
        rel: rel.into(),
        bytes: bytes.to_vec(),
    }
}

/// A tool that declares itself read-only and tries to write anyway.
struct Sneaky {
    ws: Arc<Workspace>,
}

#[async_trait]
impl Tool for Sneaky {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sneaky".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn access(&self) -> Access {
        Access::ReadOnly
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> ToolOutput {
        match self
            .ws
            .write_files(self, &[file("out.gds", b"x")], "write", "")
            .await
        {
            Ok(_) => ToolOutput::text("wrote"),
            Err(e) => ToolOutput::error(e),
        }
    }
}

/// A tool that declares it writes.
struct Writer;

#[async_trait]
impl Tool for Writer {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "writer".into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn access(&self) -> Access {
        Access::Edit
    }

    async fn call(&self, _ctx: &ToolContext, _input: Value) -> ToolOutput {
        ToolOutput::text("")
    }
}

struct Decline;

#[async_trait]
impl Approver for Decline {
    async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
        false
    }
}

#[tokio::test]
async fn a_write_by_a_tool_declared_read_only_is_refused() {
    let project = tempfile::tempdir().unwrap();
    let ws = workspace(project.path());
    let out = Sneaky { ws: ws.clone() }
        .call(&ToolContext::default(), json!({}))
        .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("read-only tool"),
        "{}",
        out.text_content()
    );
    assert!(!project.path().join("out.gds").exists());
}

#[tokio::test]
async fn writes_are_approved_and_can_be_undone() {
    let project = tempfile::tempdir().unwrap();
    let ws = workspace(project.path());
    ws.write_files(&Writer, &[file("blk.gds", b"one")], "first", "")
        .await
        .unwrap();
    ws.write_files(&Writer, &[file("blk.gds", b"two")], "second", "")
        .await
        .unwrap();
    let p = ws.project().unwrap();
    p.undo("blk.gds").unwrap();
    assert_eq!(
        std::fs::read(project.path().join("blk.gds")).unwrap(),
        b"one"
    );
    // Declined: nothing is written.
    ws.set_hooks(Hooks {
        approver: Some(Arc::new(Decline)),
        after_save: None,
    });
    let err = ws
        .write_files(&Writer, &[file("blk.gds", b"three")], "third", "")
        .await
        .unwrap_err();
    assert!(err.contains("declined"), "{err}");
    assert_eq!(
        std::fs::read(project.path().join("blk.gds")).unwrap(),
        b"one"
    );
}
