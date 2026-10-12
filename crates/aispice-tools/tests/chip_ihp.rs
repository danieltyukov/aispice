//! The A4 acceptance chip on IHP SG13G2: the 5T OTA and the StrongARM
//! comparator exported as macro packages, placed in a core with a pad ring
//! of IHP's IO cells, bond pads, a seal ring and fill, and the whole chip
//! DRC clean on IHP's main, extra, antenna and density decks and LVS clean
//! on its LVS deck.
//!
//! Needs `AISPICE_PDK_IHP` (a git checkout with the OSDI models, pcell
//! submodules and IO library), KLayout and Magic; passes with a note when
//! any is missing. With `AISPICE_DIGITAL_MACRO` naming a hardened counter's
//! package (`tests/support/harden_counter.py` makes one), the comparator's
//! output also drives the counter's enable. `AISPICE_KEEP` keeps the
//! project folder.

use aispice_agent::ToolContext;
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Project, Runner, RunnerConfig, Workspace};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVINP inp 0 0.6 AC 1\nVINM inm 0 0.6\nIB vdd vbn 20u\nCL out 0 1p\nXM1 d1 inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 d1 d1 vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out d1 vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\n.ac dec 20 1 10G\n.end\n";
const OTA_SPECS: &str = "gain = gain_db_at(V(out), 10) >= 20\ngbw = unity_gain_freq(V(out)) >= 1Meg\npm = phase_margin(V(out)) >= 45\n";
const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n.end\n";
const COMPARATOR_SPECS: &str = "decision = value_at(V(outp,outn), 2.9n) >= 0.8\n";

fn workspace() -> Option<(tempfile::TempDir, Arc<Workspace>)> {
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
    if opened.commit.is_none() || !pdk.join("libs.ref/sg13g2_io/gds/sg13g2_io.gds").is_file() {
        eprintln!("skipped: the PDK needs to be a git checkout with libs.ref/sg13g2_io");
        return None;
    }
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
    Some((dir, Arc::new(ws)))
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
    assert!(!out.is_error, "{tool}: {}", out.text_content());
    out
}

/// Copy a package folder (and its evidence) into the project.
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

fn pad(net: &str, side: &str, kind: &str, extra: &str) -> String {
    format!("[[pads]]\nnet = \"{net}\"\nside = \"{side}\"\nkind = \"{kind}\"\n{extra}\n")
}

#[tokio::test]
async fn an_analog_on_top_chip_is_drc_and_lvs_clean() {
    let Some((dir, ws)) = workspace() else {
        return;
    };
    for (circuit, template, name, specs) in [
        ("ota.cir", "ota5t", "ota5t", OTA_SPECS),
        (
            "comparator.cir",
            "comparator",
            "strongarm",
            COMPARATOR_SPECS,
        ),
    ] {
        call(
            &ws,
            "layout",
            json!({"action": "generate", "circuit": circuit, "template": template}),
        )
        .await;
        let gds = circuit.replace(".cir", ".gds");
        call(
            &ws,
            "macro_export",
            json!({"circuit": circuit, "layout": gds, "name": name, "specs": specs}),
        )
        .await;
    }
    let digital = std::env::var_os("AISPICE_DIGITAL_MACRO").map(PathBuf::from);
    let mut spec = String::from(
        "name = \"demo\"\n\n[[macros]]\ninstance = \"ota\"\npackage = \"macros/ota5t.macro\"\nnets = { inp = \"ota_inp\", inm = \"ota_inm\", out = \"ota_out\", vbn = \"ota_bias\" }\n\n[[macros]]\ninstance = \"cmp\"\npackage = \"macros/strongarm.macro\"\nnets = { inp = \"cmp_inp\", inm = \"cmp_inm\", outp = \"cmp_outp\", outn = \"cmp_outn\" }\n\n",
    );
    if let Some(pkg) = &digital {
        let name = pkg.file_name().unwrap().to_string_lossy().into_owned();
        copy_package(pkg, &dir.path().join("macros").join(&name));
        spec.push_str(&format!(
            "[[macros]]\ninstance = \"cnt\"\npackage = \"macros/{name}\"\nnets = {{ en = \"cmp_outp\", \"q[0]\" = \"q0\", \"q[1]\" = \"q1\", VDD = \"vdd\", VSS = \"vss\" }}\n\n"
        ));
    }
    for (net, side, kind, extra) in [
        ("ota_inp", "west", "analog", "resistor = true"),
        ("ota_inm", "west", "analog", "resistor = true"),
        ("ota_bias", "west", "analog", ""),
        ("ota_out", "east", "analog", ""),
        ("cmp_outp", "east", "output", ""),
        ("cmp_outn", "east", "output", ""),
        ("cmp_inp", "south", "analog", "resistor = true"),
        ("cmp_inm", "south", "analog", "resistor = true"),
        ("clk", "south", "input", ""),
        ("vdd", "north", "vdd", ""),
        ("vss", "north", "vss", ""),
        ("iovdd", "north", "iovdd", ""),
        ("iovss", "north", "iovss", ""),
    ] {
        spec.push_str(&pad(net, side, kind, extra));
    }
    if digital.is_some() {
        spec.push_str(&pad("rst_n", "south", "input", ""));
        spec.push_str(&pad("q0", "east", "output", ""));
        spec.push_str(&pad("q1", "west", "output", ""));
    }
    std::fs::write(dir.path().join("demo.chip.toml"), &spec).unwrap();

    let out = call(
        &ws,
        "chip",
        json!({"action": "plan", "spec": "demo.chip.toml"}),
    )
    .await;
    let v = &out.data.as_ref().unwrap()["value"];
    assert!(v["routes"].as_array().unwrap().len() >= 11, "{v}");
    assert!(
        !dir.path().join("demo.chip").exists(),
        "plan writes nothing"
    );

    let out = call(
        &ws,
        "chip",
        json!({"action": "assemble", "spec": "demo.chip.toml"}),
    )
    .await;
    let r = &out.data.as_ref().unwrap()["value"]["result"];
    let gds = dir.path().join("demo.chip/demo.gds");
    assert!(gds.is_file());
    assert!(dir.path().join("demo.chip/evidence/drc.json").is_file());
    if std::env::var_os("AISPICE_KEEP").is_some() {
        eprintln!("kept {}", dir.keep().display());
    }
    assert_eq!(r["drc"]["rules"], json!([]), "full-chip DRC clean");
    assert_eq!(r["lvs"]["matched"], json!(true), "full-chip LVS clean");
}
