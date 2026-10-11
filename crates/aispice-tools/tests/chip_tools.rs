//! The chip tool's checks before anything runs, on a stand-in PDK: a
//! package must lie in the project, must not lead out of it through a
//! link, and must pass the macro package checks; a chip description is
//! read with limits; `plan` writes nothing; `assemble` never replaces a
//! folder it did not make. No KLayout, Magic or real PDK is needed: every
//! check happens before them.

use aispice_agent::ToolContext;
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Project, Runner, RunnerConfig, Workspace};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const COMMIT: &str = "22f2a25f1734796de3debbbf29cf697cbbc54081";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../aispice-core/tests/fixtures/macro")
}

/// A stand-in IHP folder: the fake models, empty decks, the IO library's
/// LEF in IHP's shape, and git metadata naming the commit.
fn fake_pdk(dir: &Path) -> PathBuf {
    let root = aispice_sim::pdk::fake_ihp(dir);
    for f in [
        "libs.tech/klayout/tech/drc/ihp-sg13g2.drc",
        "libs.tech/klayout/tech/drc/rule_decks/sg13g2_tech_default.json",
        "libs.tech/klayout/tech/lvs/sg13g2.lvs",
        "libs.ref/sg13g2_io/gds/sg13g2_io.gds",
    ] {
        let p = root.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }
    let lef = root.join("libs.ref/sg13g2_io/lef/sg13g2_io.lef");
    std::fs::create_dir_all(lef.parent().unwrap()).unwrap();
    std::fs::write(lef, aispice_layout::chip::io::test_lef()).unwrap();
    let git = dir.join("IHP-Open-PDK/.git");
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), format!("{COMMIT}\n")).unwrap();
    root
}

fn copy_package(from: &Path, to: &Path) {
    std::fs::create_dir_all(to.join("evidence")).unwrap();
    for sub in ["", "evidence"] {
        for e in std::fs::read_dir(from.join(sub))
            .unwrap()
            .map(Result::unwrap)
        {
            if e.path().is_file() {
                std::fs::copy(e.path(), to.join(sub).join(e.file_name())).unwrap();
            }
        }
    }
}

struct Setup {
    _pdk: tempfile::TempDir,
    project: tempfile::TempDir,
    ws: Arc<Workspace>,
}

fn setup() -> Setup {
    let pdk_dir = tempfile::tempdir().unwrap();
    let root = fake_pdk(pdk_dir.path());
    let project = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", &root).unwrap();
    std::fs::write(project.path().join(PROJECT_FILE), "pdk = \"ihp-sg13g2\"\n").unwrap();
    copy_package(
        &fixtures().join("tiny_counter.macro"),
        &project.path().join("macros/tiny_counter.macro"),
    );
    let runner = Runner::new(RunnerConfig {
        pdks,
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let ws = Workspace::new();
    ws.runner.set_config(runner.config());
    ws.set_project(Project::open(project.path(), Default::default()).unwrap());
    Setup {
        _pdk: pdk_dir,
        project,
        ws: Arc::new(ws),
    }
}

const PADS: &str = "[[pads]]\nnet = \"clk\"\nside = \"south\"\nkind = \"input\"\n[[pads]]\nnet = \"rst_n\"\nside = \"south\"\nkind = \"input\"\n[[pads]]\nnet = \"en\"\nside = \"west\"\nkind = \"input\"\n[[pads]]\nnet = \"q0\"\nside = \"east\"\nkind = \"output\"\n[[pads]]\nnet = \"q1\"\nside = \"east\"\nkind = \"output\"\n[[pads]]\nnet = \"vdd\"\nside = \"north\"\nkind = \"vdd\"\n[[pads]]\nnet = \"vss\"\nside = \"north\"\nkind = \"vss\"\n[[pads]]\nnet = \"iovdd\"\nside = \"north\"\nkind = \"iovdd\"\n[[pads]]\nnet = \"iovss\"\nside = \"north\"\nkind = \"iovss\"\n";

/// A chip of the counter. The package path is a TOML literal string, so a
/// Windows path's backslashes stay as they are.
fn chip(package: &str) -> String {
    format!(
        "name = \"t\"\n[[macros]]\ninstance = \"cnt\"\npackage = '{package}'\nnets = {{ VDD = \"vdd\", VSS = \"vss\", \"q[0]\" = \"q0\", \"q[1]\" = \"q1\" }}\n{PADS}"
    )
}

async fn call(ws: &Arc<Workspace>, input: Value) -> aispice_agent::ToolOutput {
    aispice_tools::registry(ws.clone())
        .get("chip")
        .unwrap()
        .call(&ToolContext::default(), input)
        .await
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in walk(dir) {
        if !e.starts_with(dir.join(".aispice")) {
            out.push(e);
        }
    }
    out.sort();
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn a_chip_of_a_checked_package_is_planned_and_nothing_is_written() {
    let s = setup();
    std::fs::write(
        s.project.path().join("t.chip.toml"),
        chip("macros/tiny_counter.macro"),
    )
    .unwrap();
    let before = files(s.project.path());
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(!out.is_error, "{}", out.text_content());
    let text = out.text_content();
    assert!(text.contains("block cnt (tiny_counter)"), "{text}");
    for net in ["clk", "rst_n", "en", "q0", "q1", "vdd", "vss"] {
        assert!(text.contains(&format!("net {net}:")), "{net}: {text}");
    }
    assert_eq!(files(s.project.path()), before, "plan writes nothing");
}

#[tokio::test]
async fn packages_outside_the_project_are_refused() {
    let s = setup();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("tiny_counter.macro");
    copy_package(&fixtures().join("tiny_counter.macro"), &outside);
    for (package, why) in [
        (outside.display().to_string(), "outside"),
        ("../tiny_counter.macro".to_string(), "outside"),
    ] {
        std::fs::write(s.project.path().join("t.chip.toml"), chip(&package)).unwrap();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(out.is_error, "{package}: {}", out.text_content());
        assert!(
            out.text_content().contains(why),
            "{package}: {}",
            out.text_content()
        );
    }
    #[cfg(unix)]
    {
        // A package folder that links out, and a file in one that does.
        std::os::unix::fs::symlink(&outside, s.project.path().join("macros/linked.macro")).unwrap();
        std::fs::write(
            s.project.path().join("t.chip.toml"),
            chip("macros/linked.macro"),
        )
        .unwrap();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(
            out.is_error && out.text_content().contains("outside"),
            "{}",
            out.text_content()
        );
        let inner = s
            .project
            .path()
            .join("macros/tiny_counter.macro/tiny_counter.lef");
        std::fs::remove_file(&inner).unwrap();
        std::os::unix::fs::symlink(outside.join("tiny_counter.lef"), &inner).unwrap();
        std::fs::write(
            s.project.path().join("t.chip.toml"),
            chip("macros/tiny_counter.macro"),
        )
        .unwrap();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(
            out.is_error && out.text_content().contains("leads outside"),
            "{}",
            out.text_content()
        );
    }
}

#[tokio::test]
async fn a_package_that_fails_its_checks_is_not_used() {
    let s = setup();
    let broken = fixtures().join("broken/lvs-fail/tiny_counter.macro");
    if !broken.is_dir() {
        // The shared fixtures name their cases; take any broken counter.
        panic!("expected the shared fixture {}", broken.display());
    }
    copy_package(
        &broken,
        &s.project
            .path()
            .join("macros/broken.macro/tiny_counter.macro"),
    );
    std::fs::write(
        s.project.path().join("t.chip.toml"),
        chip("macros/broken.macro/tiny_counter.macro"),
    )
    .unwrap();
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(out.is_error);
    let text = out.text_content();
    assert!(
        text.contains("does not pass the macro package checks"),
        "{text}"
    );
    assert!(text.contains("evidence.status"), "{text}");
}

#[tokio::test]
async fn an_analog_fixture_with_pins_inside_is_named() {
    let s = setup();
    copy_package(
        &fixtures().join("tiny_ota.macro"),
        &s.project.path().join("macros/tiny_ota.macro"),
    );
    let spec = format!(
        "name = \"t\"\n[[macros]]\ninstance = \"ota\"\npackage = \"macros/tiny_ota.macro\"\n{PADS}"
    );
    std::fs::write(s.project.path().join("t.chip.toml"), spec).unwrap();
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("of block ota has no shape"),
        "{}",
        out.text_content()
    );
}

#[tokio::test]
async fn bad_chip_descriptions_and_outputs_are_refused() {
    let s = setup();
    for (text, why) in [
        ("name = \"a b\"\n".to_string(), "chip name"),
        (
            chip("macros/tiny_counter.macro").replace("instance = \"cnt\"", "instance = \"c;rm\""),
            "macro instance",
        ),
        (
            format!("{}\n", "x = [".repeat(70) + &"]".repeat(70)),
            "nests",
        ),
    ] {
        std::fs::write(s.project.path().join("t.chip.toml"), &text).unwrap();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(
            out.is_error && out.text_content().contains(why),
            "{why}: {}",
            out.text_content()
        );
    }
    let out = call(&s.ws, json!({"action": "plan", "spec": "../t.chip.toml"})).await;
    assert!(
        out.is_error && out.text_content().contains("outside"),
        "{}",
        out.text_content()
    );
    // A folder of the user's under the output name is never replaced.
    std::fs::write(
        s.project.path().join("t.chip.toml"),
        chip("macros/tiny_counter.macro"),
    )
    .unwrap();
    std::fs::create_dir(s.project.path().join("t.chip")).unwrap();
    std::fs::write(s.project.path().join("t.chip/notes.txt"), "mine").unwrap();
    let out = call(&s.ws, json!({"action": "assemble", "spec": "t.chip.toml"})).await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("was not made by the chip tool"),
        "{}",
        out.text_content()
    );
    for bad in ["../out", ".aispice/out", "/etc/out"] {
        let out = call(
            &s.ws,
            json!({"action": "assemble", "spec": "t.chip.toml", "out": bad}),
        )
        .await;
        assert!(
            out.is_error && out.text_content().contains("outside"),
            "{bad}: {}",
            out.text_content()
        );
    }
    // Names Windows opens as another file: `.aispice.` is `.aispice`.
    for bad in [".aispice./out", "AISPIC~1/out", "out:stream"] {
        let out = call(
            &s.ws,
            json!({"action": "assemble", "spec": "t.chip.toml", "out": bad}),
        )
        .await;
        assert!(
            out.is_error && out.text_content().contains("does not write"),
            "{bad}: {}",
            out.text_content()
        );
    }
    assert_eq!(
        std::fs::read_to_string(s.project.path().join("t.chip/notes.txt")).unwrap(),
        "mine"
    );
}
