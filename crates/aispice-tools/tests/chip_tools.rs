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
    pdk_root: PathBuf,
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
        pdk_root: root,
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

/// The counter's package claiming another size, in its LEF and its
/// manifest alike, as a package made to mislead would.
fn with_size(s: &Setup, w: &str, h: &str) {
    let pkg = s.project.path().join("macros/tiny_counter.macro");
    let from = fixtures().join("tiny_counter.macro");
    let lef = std::fs::read_to_string(from.join("tiny_counter.lef")).unwrap();
    assert!(lef.contains("SIZE 20 BY 10"));
    std::fs::write(
        pkg.join("tiny_counter.lef"),
        lef.replace("SIZE 20 BY 10", &format!("SIZE {w} BY {h}")),
    )
    .unwrap();
    let manifest = std::fs::read_to_string(from.join("macro.toml")).unwrap();
    assert!(manifest.contains("width_um = 20.0\nheight_um = 10.0"));
    std::fs::write(
        pkg.join("macro.toml"),
        manifest.replace(
            "width_um = 20.0\nheight_um = 10.0",
            &format!("width_um = {w}\nheight_um = {h}"),
        ),
    )
    .unwrap();
}

/// A package's LEF can claim any size. Past the die limit it is refused
/// before anything is sized from it: a ring around 1e10 um would be 2e8
/// filler cells.
#[tokio::test]
async fn a_block_larger_than_any_die_is_refused_at_once() {
    let s = setup();
    std::fs::write(
        s.project.path().join("t.chip.toml"),
        chip("macros/tiny_counter.macro"),
    )
    .unwrap();
    for (w, h) in [("1e10", "10.0"), ("20.0", "1e300"), ("20000.001", "10.0")] {
        with_size(&s, w, h);
        let started = std::time::Instant::now();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(
            out.is_error && out.text_content().contains("at most 20 mm"),
            "{w} x {h}: {}",
            out.text_content()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}

/// The counter's GDS with a `$$$CONTEXT_INFO$$$` cell added before ENDLIB,
/// and its evidence brought up to date so the package still checks.
fn add_context_info(pkg: &Path) {
    let gds = pkg.join("tiny_counter.gds");
    let old = aispice_core::macropkg::sha256_file(&gds).unwrap();
    let bytes = std::fs::read(&gds).unwrap();
    let end = bytes
        .windows(4)
        .rposition(|w| w == [0, 4, 4, 0])
        .expect("ENDLIB");
    let mut out = bytes[..end].to_vec();
    let mut record = |kind: u8, dt: u8, data: &[u8]| {
        out.extend(((data.len() + 4) as u16).to_be_bytes());
        out.extend([kind, dt]);
        out.extend(data);
    };
    record(0x05, 0x02, &[0; 24]);
    record(0x06, 0x06, b"$$$CONTEXT_INFO$$$");
    record(0x07, 0x00, &[]);
    out.extend(&bytes[end..]);
    std::fs::write(&gds, out).unwrap();
    let new = aispice_core::macropkg::sha256_file(&gds).unwrap();
    for e in ["drc.json", "lvs.json"] {
        let path = pkg.join("evidence").join(e);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace(&old, &new)).unwrap();
    }
}

/// KLayout rebuilds pcells from a GDS's context information with the
/// parameters stored there, after IHP's pcell library is loaded, so a block
/// carrying it could put geometry into the chip that no check saw.
#[tokio::test]
async fn a_block_gds_with_klayout_context_information_is_refused() {
    let s = setup();
    add_context_info(&s.project.path().join("macros/tiny_counter.macro"));
    std::fs::write(
        s.project.path().join("t.chip.toml"),
        chip("macros/tiny_counter.macro"),
    )
    .unwrap();
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(
        out.is_error && out.text_content().contains("context information"),
        "{}",
        out.text_content()
    );
}

/// SPICE ignores case, so `TINY_COUNTER` and `tiny_counter` are one
/// subcircuit; a block or chip named like a cell of the PDK is refused
/// whatever its case, the block by macro/v1's `pdk.cell_name` itself.
#[tokio::test]
async fn block_and_chip_names_of_pdk_cells_are_refused_in_any_case() {
    let s = setup();
    let lef = s
        .pdk_root
        .join("libs.ref/sg13g2_stdcell/lef/sg13g2_stdcell.lef");
    std::fs::create_dir_all(lef.parent().unwrap()).unwrap();
    std::fs::write(
        &lef,
        "MACRO TINY_COUNTER\n  CLASS CORE ;\nEND TINY_COUNTER\n",
    )
    .unwrap();
    let text = chip("macros/tiny_counter.macro");
    std::fs::write(s.project.path().join("t.chip.toml"), &text).unwrap();
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(
        out.is_error
            && out
                .text_content()
                .contains(r#"pdk.cell_name macro.toml: name "tiny_counter" is a cell of PDK"#),
        "{}",
        out.text_content()
    );
    std::fs::remove_file(&lef).unwrap();
    for (name, why) in [
        ("SG13G2_top", "begins with sg13g2,"),
        ("sg13g2_inv_1", "begins with sg13g2,"),
        ("rm_ihpsg13_top", "begins with rm_ihpsg13,"),
        ("TINY_COUNTER", "is the chip's"),
    ] {
        std::fs::write(
            s.project.path().join("t.chip.toml"),
            text.replace("name = \"t\"", &format!("name = \"{name}\"")),
        )
        .unwrap();
        let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
        assert!(
            out.is_error && out.text_content().contains(why),
            "{name}: {}",
            out.text_content()
        );
    }
    // The fixture as it is plans.
    std::fs::write(s.project.path().join("t.chip.toml"), &text).unwrap();
    let out = call(&s.ws, json!({"action": "plan", "spec": "t.chip.toml"})).await;
    assert!(!out.is_error, "{}", out.text_content());
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
    // An `out` with leading space names the same folder as without: the
    // guard looks at the folder that would be written.
    let out = call(
        &s.ws,
        json!({"action": "assemble", "spec": "t.chip.toml", "out": "  t.chip"}),
    )
    .await;
    assert!(
        out.is_error && out.text_content().contains("was not made by the chip tool"),
        "{}",
        out.text_content()
    );
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
