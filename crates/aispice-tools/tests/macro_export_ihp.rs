//! Macro export on IHP SG13G2: the 5T OTA and the StrongARM comparator laid
//! out, exported as macro/v1 packages, and the packages checked by aispice
//! and, when it is installed, by ChipLeap.
//!
//! Needs `AISPICE_PDK_IHP` (a git checkout, so the commit is known, with the
//! OSDI models and pcell submodules), KLayout and Magic; passes with a note
//! when any is missing. `AISPICE_CHIPLEAP` names a `chipleap` executable to
//! run ChipLeap's validator on the packages too.

use aispice_agent::ToolContext;
use aispice_core::macropkg::{CheckOptions, Manifest, check_package};
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Approver, Hooks, Project, Runner, RunnerConfig, Workspace};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVINP inp 0 0.6 AC 1\nVINM inm 0 0.6\nIB vdd vbn 20u\nCL out 0 1p\nXM1 d1 inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 d1 d1 vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out d1 vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\n.ac dec 20 1 10G\n.end\n";

const OTA_SPECS: &str = "gain = gain_db_at(V(out), 10) >= 20\ngbw = unity_gain_freq(V(out)) >= 1Meg\npm = phase_margin(V(out)) >= 45\n";

pub const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n.end\n";

/// The comparator decides within the clock's high half: the side with the
/// higher input stays high, the other falls.
const COMPARATOR_SPECS: &str =
    "decision = value_at(V(outp,outn), 2.9n) >= 0.8\noutn_low = value_at(V(outn), 2.9n) <= 0.3\n";

fn workspace() -> Option<(tempfile::TempDir, Arc<Workspace>, String)> {
    let Some(pdk) = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from) else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return None;
    };
    let programs = aispice_layout::tools::Programs::default();
    if programs.klayout().is_err() || programs.magic().is_err() {
        eprintln!("skipped: KLayout and Magic are needed");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", &pdk).unwrap();
    let opened = pdks.open("ihp-sg13g2").unwrap();
    let files = aispice_layout::tools::PdkLayoutFiles::of(&opened).unwrap();
    if let Err(e) = files.check_pcells() {
        eprintln!("skipped: {e}");
        return None;
    }
    let Some(commit) = opened.commit.clone() else {
        eprintln!("skipped: the PDK folder is not a git checkout, so its commit is unknown");
        return None;
    };
    std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"ihp-sg13g2\"\n").unwrap();
    std::fs::write(dir.path().join("ota.cir"), OTA).unwrap();
    std::fs::write(dir.path().join("comparator.cir"), STRONGARM).unwrap();
    let runner = Runner::new(RunnerConfig {
        pdks,
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let ws = Workspace::new();
    ws.runner.set_config(runner.config());
    ws.set_project(Project::open(dir.path(), Default::default()).unwrap());
    Some((dir, Arc::new(ws), commit))
}

async fn call(ws: &Arc<Workspace>, tool: &str, input: Value) -> aispice_agent::ToolOutput {
    let started = std::time::Instant::now();
    let out = aispice_tools::registry(ws.clone())
        .get(tool)
        .unwrap_or_else(|| panic!("no tool {tool}"))
        .call(&ToolContext::default(), input)
        .await;
    eprintln!(
        "== {tool} ({:.1} s)\n{}",
        started.elapsed().as_secs_f64(),
        out.text_content()
    );
    out
}

/// ChipLeap's validator on a package, when `AISPICE_CHIPLEAP` names it.
fn chipleap_check(dir: &Path, pdk: &str) {
    let Some(exe) = std::env::var_os("AISPICE_CHIPLEAP") else {
        eprintln!("ChipLeap's validator skipped: AISPICE_CHIPLEAP is not set");
        return;
    };
    let out = std::process::Command::new(exe)
        .args(["macro", "check", "--json", "--pdk", pdk])
        .arg(dir)
        .output()
        .expect("chipleap runs");
    let text = String::from_utf8_lossy(&out.stdout);
    eprintln!("chipleap macro check {}:\n{text}", dir.display());
    assert!(
        out.status.success(),
        "chipleap rejects {}: {text}{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(report["pass"], json!(true));
}

fn check(dir: &Path, pdk: &str) -> Manifest {
    let report = check_package(
        dir,
        &CheckOptions {
            pdk: Some(pdk.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.passed, "{}", report.to_text());
    assert_eq!(report.warnings, 0, "{}", report.to_text());
    Manifest::load(dir).unwrap()
}

#[tokio::test]
async fn the_ota_and_the_comparator_export_as_packages_both_tools_accept() {
    let Some((dir, ws, commit)) = workspace() else {
        return;
    };
    let pdk = format!("ihp-sg13g2@{commit}");

    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.cir", "template": "ota5t"}),
    )
    .await;
    assert!(!out.is_error);
    let out = call(
        &ws,
        "macro_export",
        json!({"circuit": "ota.cir", "layout": "ota.gds", "name": "ota5t",
               "specs": OTA_SPECS, "corners": {"process": ["tt", "ss", "ff"]},
               "pins": {"vbn": {"description": "Bias current in, 20 uA"}}}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text_content());
    let pkg = dir.path().join("macros/ota5t.macro");
    let m = check(&pkg, &pdk);
    let pins: Vec<(&str, &str, &str)> = m
        .pins
        .iter()
        .map(|p| (p.name.as_str(), p.direction.as_str(), p.kind.as_str()))
        .collect();
    assert_eq!(
        pins,
        [
            ("inm", "input", "analog"),
            ("inp", "input", "analog"),
            ("out", "output", "analog"),
            ("vbn", "input", "analog"),
            ("vdd", "inout", "power"),
            ("vss", "inout", "ground"),
        ]
    );
    assert_eq!(m.corners.len(), 3);
    assert_eq!(m.corners[0].name, "tt_27");
    assert_eq!(m.domains[0].voltage, 1.2);
    for f in [
        "ota5t.gds",
        "ota5t.lef",
        "ota5t.lib",
        "ota5t.v",
        "ota5t.spice",
        "ota5t.pex.spice",
        "ota5t.specs.toml",
        "evidence/drc.json",
        "evidence/lvs.json",
        "evidence/pex.json",
        "evidence/sim.json",
    ] {
        assert!(pkg.join(f).is_file(), "{f}");
    }
    let specs = std::fs::read_to_string(pkg.join("ota5t.specs.toml")).unwrap();
    assert!(specs.contains("gbw_schematic"), "{specs}");
    chipleap_check(&pkg, &pdk);

    // The comparator, its clock marked as one and its outputs digital.
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "comparator.cir", "template": "comparator"}),
    )
    .await;
    assert!(!out.is_error);
    let out = call(
        &ws,
        "macro_export",
        json!({"circuit": "comparator.cir", "layout": "comparator.gds", "name": "strongarm",
               "specs": COMPARATOR_SPECS,
               "pins": {"clk": {"kind": "clock"}, "outp": {"kind": "signal"}, "outn": {"kind": "signal"}}}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text_content());
    let pkg = dir.path().join("macros/strongarm.macro");
    let m = check(&pkg, &pdk);
    let clk = m.pin("clk").unwrap();
    assert_eq!(
        (clk.direction.as_str(), clk.kind.as_str()),
        ("input", "clock")
    );
    assert_eq!(m.pin("outp").unwrap().direction.as_str(), "output");
    chipleap_check(&pkg, &pdk);

    // Exporting again replaces the package, which is still valid.
    let out = call(
        &ws,
        "macro_export",
        json!({"circuit": "comparator.cir", "layout": "comparator.gds", "name": "strongarm",
               "specs": COMPARATOR_SPECS}),
    )
    .await;
    assert!(!out.is_error, "{}", out.text_content());
    check(&pkg, &pdk);

    // A spec the block cannot meet stops the export before anything is
    // written.
    let out = call(
        &ws,
        "macro_export",
        json!({"circuit": "ota.cir", "layout": "ota.gds", "name": "fast_ota",
               "specs": "gbw = unity_gain_freq(V(out)) >= 100G\n"}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("does not meet its specs"),
        "{}",
        out.text_content()
    );
    assert!(!dir.path().join("macros/fast_ota.macro").exists());
}

struct Decline;

#[async_trait]
impl Approver for Decline {
    async fn approve(&self, _circuit: &str, _summary: &str, _diff: &str) -> bool {
        false
    }
}

/// A declined export writes nothing of the user's.
#[tokio::test]
async fn a_declined_export_writes_nothing() {
    let Some((dir, ws, _)) = workspace() else {
        return;
    };
    let out = call(
        &ws,
        "layout",
        json!({"action": "generate", "circuit": "ota.cir", "template": "ota5t", "check": false}),
    )
    .await;
    assert!(!out.is_error);
    ws.set_hooks(Hooks {
        approver: Some(Arc::new(Decline)),
        after_save: None,
    });
    let out = call(
        &ws,
        "macro_export",
        json!({"circuit": "ota.cir", "layout": "ota.gds", "specs": OTA_SPECS}),
    )
    .await;
    assert!(out.is_error);
    assert!(
        out.text_content().contains("declined"),
        "{}",
        out.text_content()
    );
    assert!(!dir.path().join("macros").exists());
}
