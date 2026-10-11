//! IHP SG13G2 circuits drawn with aispice's own device symbols, simulated on
//! ngspice and Xyce through the normal runner, with the results compared.
//!
//! Needs the PDK: `AISPICE_PDK_IHP=<path to IHP-Open-PDK/ihp-sg13g2>` with
//! its OSDI files compiled. The Xyce half also needs the PDK's Xyce plugins
//! and a Xyce built with plugin support (`AISPICE_XYCE`, else `Xyce` on
//! PATH); without them it is skipped. Every test passes, with a note on
//! stderr, when the PDK is not there.

use aispice_agent::ToolContext;
use aispice_sim::backend::SimId;
use aispice_sim::dataset::{AnalysisKind, Dataset};
use aispice_tools::pdk::{PROJECT_FILE, Registry};
use aispice_tools::{Project, RunMods, Runner, RunnerConfig, StoredRun, Workspace};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn ihp() -> Option<PathBuf> {
    let p = std::env::var_os("AISPICE_PDK_IHP").map(PathBuf::from);
    if p.is_none() {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
    }
    p
}

fn xyce_exe() -> Option<PathBuf> {
    std::env::var_os("AISPICE_XYCE")
        .map(PathBuf::from)
        .or_else(|| aispice_sim::backend::which("Xyce"))
}

/// A project folder with `aispice.toml` naming the PDK, and a runner with the
/// PDK registered, using aispice's own symbols and models only.
fn setup(pdk: &Path, files: &[(&str, &str)]) -> (tempfile::TempDir, Runner, Project) {
    let dir = tempfile::tempdir().unwrap();
    let mut pdks = Registry::default();
    pdks.add("ihp-sg13g2", pdk).unwrap();
    std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"ihp-sg13g2\"\n").unwrap();
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    let runner = Runner::new(RunnerConfig {
        pdks,
        xyce_exe: xyce_exe(),
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let project = Project::open(dir.path(), Default::default()).unwrap();
    (dir, runner, project)
}

async fn run(runner: &Runner, project: &Project, circuit: &str, sim: SimId) -> Arc<StoredRun> {
    runner
        .run_circuit(
            project,
            circuit,
            Some(sim.name()),
            &RunMods::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_or_else(|e| panic!("{circuit} on {}: {e}", sim.name()))
}

/// Whether Xyce can run the PDK here; says why not when it cannot.
fn xyce_ready(runner: &Runner) -> bool {
    let pdk = runner.config().pdks.open("ihp-sg13g2").unwrap();
    if pdk.xyce_plugins.is_empty() {
        eprintln!("Xyce half skipped: the PDK has no Xyce plugins (libs.tech/xyce/plugins)");
        return false;
    }
    if xyce_exe().is_none() {
        eprintln!("Xyce half skipped: no Xyce");
        return false;
    }
    true
}

fn real(ds: &Dataset, name: &str) -> Vec<f64> {
    ds.vector(name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", ds.names()))
        .data
        .real()
}

/// A CMOS inverter, VDD 1.2 V, input swept 0 to 1.2 V in 10 mV steps.
const INVERTER: &str = "Version 4
SHEET 1 880 680
FLAG 48 0 out
FLAG 0 48 in
FLAG 48 96 0
FLAG 48 48 0
FLAG 208 0 vdd
FLAG 160 48 in
FLAG 208 96 out
FLAG 208 48 vdd
FLAG 320 16 vdd
FLAG 320 96 0
FLAG 400 16 in
FLAG 400 96 0
SYMBOL sg13g2\\sg13_lv_nmos 0 0 R0
SYMATTR InstName M1
SYMATTR SpiceLine w=1u l=0.13u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_pmos 160 0 R0
SYMATTR InstName M2
SYMATTR SpiceLine w=2u l=0.13u ng=1 m=1
SYMBOL voltage 320 0 R0
SYMATTR InstName VDD
SYMATTR Value 1.2
SYMBOL voltage 400 0 R0
SYMATTR InstName VIN
SYMATTR Value 0
TEXT 0 160 Left 2 !.dc VIN 0 1.2 10m
";

/// A five-transistor OTA: NMOS pair, PMOS mirror load, NMOS tail biased from
/// a 20 uA diode, a MIM load capacitor. Open loop, inputs at 0.6 V, AC on
/// the non-inverting input.
const OTA: &str = "Version 4
SHEET 1 1200 900
FLAG 48 0 d1
FLAG 0 48 inp
FLAG 48 96 tail
FLAG 48 48 0
FLAG 208 0 out
FLAG 160 48 inm
FLAG 208 96 tail
FLAG 208 48 0
FLAG 48 -200 vdd
FLAG 0 -152 d1
FLAG 48 -104 d1
FLAG 48 -152 vdd
FLAG 208 -200 vdd
FLAG 160 -152 d1
FLAG 208 -104 out
FLAG 208 -152 vdd
FLAG 128 200 tail
FLAG 80 248 vbn
FLAG 128 296 0
FLAG 128 248 0
FLAG 348 200 vbn
FLAG 300 248 vbn
FLAG 348 296 0
FLAG 348 248 0
FLAG 480 0 vdd
FLAG 480 80 vbn
FLAG 576 0 out
FLAG 576 64 0
FLAG 700 16 vdd
FLAG 700 96 0
FLAG 800 16 inp
FLAG 800 96 0
FLAG 900 16 inm
FLAG 900 96 0
SYMBOL sg13g2\\sg13_lv_nmos 0 0 R0
SYMATTR InstName M1
SYMATTR SpiceLine w=2u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 160 0 R0
SYMATTR InstName M2
SYMATTR SpiceLine w=2u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_pmos 0 -200 R0
SYMATTR InstName M3
SYMATTR SpiceLine w=4u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_pmos 160 -200 R0
SYMATTR InstName M4
SYMATTR SpiceLine w=4u l=0.5u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 80 200 R0
SYMATTR InstName M5
SYMATTR SpiceLine w=4u l=1u ng=1 m=1
SYMBOL sg13g2\\sg13_lv_nmos 300 200 R0
SYMATTR InstName M6
SYMATTR SpiceLine w=4u l=1u ng=1 m=1
SYMBOL current 480 0 R0
SYMATTR InstName IB
SYMATTR Value 20u
SYMBOL sg13g2\\cap_cmim 560 0 R0
SYMATTR InstName C1
SYMATTR SpiceLine w=10u l=10u m=1
SYMBOL voltage 700 0 R0
SYMATTR InstName VDD
SYMATTR Value 1.2
SYMBOL voltage 800 0 R0
SYMATTR InstName VINP
SYMATTR Value 0.6
SYMATTR SpiceLine AC 1
SYMBOL voltage 900 0 R0
SYMATTR InstName VINM
SYMATTR Value 0.6
TEXT 0 400 Left 2 !.ac dec 20 1 10G
";

/// The input where the output crosses half the supply, interpolated.
fn threshold(vin: &[f64], vout: &[f64], half: f64) -> f64 {
    for i in 1..vin.len() {
        if (vout[i - 1] - half) * (vout[i] - half) <= 0.0 {
            let t = (half - vout[i - 1]) / (vout[i] - vout[i - 1]);
            return vin[i - 1] + t * (vin[i] - vin[i - 1]);
        }
    }
    panic!("the output never crosses {half} V");
}

#[tokio::test]
async fn inverter_dc_transfer_on_ngspice_and_xyce() {
    let Some(pdk) = ihp() else { return };
    let (_dir, runner, project) = setup(&pdk, &[("inv.asc", INVERTER)]);
    let ng = run(&runner, &project, "inv.asc", SimId::Ngspice).await;
    assert!(
        ng.deck.contains("cornerMOSlv.lib mos_tt"),
        "header missing: {}",
        ng.deck
    );
    assert!(
        ng.notes
            .iter()
            .any(|n| n.starts_with("PDK ihp-sg13g2") && n.contains("native mode")),
        "{:?}",
        ng.notes
    );
    let ds = ng.output.dataset(AnalysisKind::Dc).expect("a DC sweep");
    let (vin, vout) = (real(ds, "V(in)"), real(ds, "V(out)"));
    assert_eq!(vin.len(), 121);
    assert!(
        vout[0] > 1.19 && vout[120] < 0.01,
        "not an inverter: {vout:?}"
    );
    let vm_ng = threshold(&vin, &vout, 0.6);
    assert!((0.45..0.75).contains(&vm_ng), "threshold {vm_ng}");

    if !xyce_ready(&runner) {
        return;
    }
    let xy = run(&runner, &project, "inv.asc", SimId::Xyce).await;
    assert_eq!(xy.simulator, SimId::Xyce);
    assert!(
        xy.deck
            .contains("libs.tech/xyce/models/cornerMOSlv.lib mos_tt")
    );
    assert!(
        xy.notes
            .iter()
            .any(|n| n.contains("Xyce with the PDK's plugins")),
        "{:?}",
        xy.notes
    );
    let ds = xy.output.dataset(AnalysisKind::Dc).expect("a DC sweep");
    let (vin_x, vout_x) = (real(ds, "V(in)"), real(ds, "V(out)"));
    assert_eq!(vin_x.len(), vin.len());
    let worst = vout
        .iter()
        .zip(&vout_x)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f64::max);
    let vm_x = threshold(&vin_x, &vout_x, 0.6);
    eprintln!(
        "inverter: largest V(out) difference {:.3} mV, threshold ngspice {:.4} V, Xyce {:.4} V",
        worst * 1e3,
        vm_ng,
        vm_x
    );
    assert!(worst < 5e-3, "V(out) differs by {worst} V");
    assert!((vm_ng - vm_x).abs() < 1e-3, "{vm_ng} vs {vm_x}");
}

/// Size a device with the gm_id tool, then simulate the sized device at the
/// bias the tool reported and check it carries the current and gm/Id asked
/// for. A second call must come from the cache.
async fn sized_device_meets_its_target(device: &str, sign: f64) {
    let Some(pdk) = ihp() else { return };
    let (dir, runner, project) = setup(&pdk, &[]);
    let mut cfg = runner.config();
    cfg.gmid_cache_dir = Some(dir.path().join("gmid-cache"));
    let ws = Workspace::new();
    ws.runner.set_config(cfg);
    ws.set_project(project);
    let ws = Arc::new(ws);
    let tools = aispice_tools::registry(ws.clone());
    let gm_id = tools.get("gm_id").unwrap();
    let ask = json!({"device": device, "gm_id": 12, "l": "0.5u", "id": "50u"});
    let out = gm_id.call(&ToolContext::default(), ask.clone()).await;
    assert!(!out.is_error, "{}", out.text_content());
    assert!(
        out.text_content().contains("Table built"),
        "{}",
        out.text_content()
    );
    let s = out.data.as_ref().unwrap()["value"]["sizing"].clone();
    let (w, vgs) = (s["w"].as_f64().unwrap(), s["vgs"].as_f64().unwrap());
    let ng = s["ng"].as_u64().expect("fingers chosen");
    assert_eq!(s["verified"], json!(true));
    assert!(w / ng as f64 <= 10e-6, "fingers within the model's range");

    let inner = device.to_ascii_lowercase();
    let check = format!(
        "* sized {device}\nVG g 0 {vg}\nVD d 0 {vd}\nXM1 d g 0 0 {device} w={w:e} l=0.5u ng={ng} m=1\n.save @n.xm1.n{inner}[ids] @n.xm1.n{inner}[gm]\n.dc VG {vg} {vg} 1\n.end\n",
        vg = sign * vgs,
        vd = sign * 0.6,
    );
    std::fs::write(ws.project().unwrap().root().join("check.cir"), check).unwrap();
    let run = run(
        &ws.runner,
        &ws.project().unwrap(),
        "check.cir",
        SimId::Ngspice,
    )
    .await;
    let ds = run.output.dataset(AnalysisKind::Dc).unwrap();
    let find = |q: &str| {
        ds.vectors
            .iter()
            .find(|v| v.name.to_ascii_lowercase().contains(&format!("[{q}]")))
            .unwrap_or_else(|| panic!("no {q} in {:?}", ds.names()))
            .data
            .real()[0]
            .abs()
    };
    let (id, gm) = (find("ids"), find("gm"));
    eprintln!(
        "{device}: W {:.3} um in {ng} fingers, VGS {:.4} V; simulated Id {:.3} uA, gm/Id {:.3}",
        w * 1e6,
        vgs,
        id * 1e6,
        gm / id
    );
    assert!((id / 50e-6 - 1.0).abs() < 0.01, "Id {id}");
    assert!((gm / id / 12.0 - 1.0).abs() < 0.01, "gm/Id {}", gm / id);

    let again = gm_id.call(&ToolContext::default(), ask).await;
    assert!(again.text_content().contains("read from the cache"));
    assert_eq!(again.data.as_ref().unwrap()["value"]["sizing"], s);
}

#[tokio::test]
async fn gm_id_sizes_an_nmos() {
    sized_device_meets_its_target("sg13_lv_nmos", 1.0).await;
}

#[tokio::test]
async fn gm_id_sizes_a_pmos() {
    sized_device_meets_its_target("sg13_lv_pmos", -1.0).await;
}

/// DC gain in dB and the unity-gain frequency of an AC result.
fn gain_and_ugf(ds: &Dataset) -> (f64, f64) {
    let f = ds.axis_vector().expect("frequency axis").data.real();
    let v = ds.vector("V(out)").unwrap().data.as_complex();
    let mag: Vec<f64> = v.iter().map(|c| c.abs()).collect();
    let dc_db = 20.0 * mag[0].log10();
    let i = mag
        .windows(2)
        .position(|w| w[0] >= 1.0 && w[1] < 1.0)
        .expect("gain crosses 0 dB");
    // Interpolate in log-log between the two points around the crossing.
    let (f0, f1) = (f[i].ln(), f[i + 1].ln());
    let (m0, m1) = (mag[i].ln(), mag[i + 1].ln());
    let ugf = (f0 + (0.0 - m0) * (f1 - f0) / (m1 - m0)).exp();
    (dc_db, ugf)
}

#[tokio::test]
async fn five_transistor_ota_ac_on_ngspice_and_xyce() {
    let Some(pdk) = ihp() else { return };
    let (_dir, runner, project) = setup(&pdk, &[("ota.asc", OTA)]);
    let ng = run(&runner, &project, "ota.asc", SimId::Ngspice).await;
    assert!(ng.deck.contains("cornerCAP.lib cap_typ"), "{}", ng.deck);
    let (gain_ng, ugf_ng) = gain_and_ugf(ng.output.dataset(AnalysisKind::Ac).unwrap());
    assert!(gain_ng > 20.0, "gain {gain_ng} dB");
    assert!((1e6..1e10).contains(&ugf_ng), "unity gain at {ugf_ng} Hz");

    if !xyce_ready(&runner) {
        return;
    }
    let xy = run(&runner, &project, "ota.asc", SimId::Xyce).await;
    assert_eq!(xy.simulator, SimId::Xyce);
    assert!(
        xy.deck
            .contains("libs.tech/xyce/models/cornerCAP.lib cap_typ")
    );
    let (gain_x, ugf_x) = gain_and_ugf(xy.output.dataset(AnalysisKind::Ac).unwrap());
    eprintln!(
        "OTA: DC gain ngspice {gain_ng:.3} dB, Xyce {gain_x:.3} dB; unity gain ngspice {:.4} MHz, Xyce {:.4} MHz",
        ugf_ng / 1e6,
        ugf_x / 1e6
    );
    assert!((gain_ng - gain_x).abs() < 0.1, "{gain_ng} vs {gain_x} dB");
    assert!(
        (ugf_ng / ugf_x - 1.0).abs() < 0.01,
        "{ugf_ng} vs {ugf_x} Hz"
    );
}

/// A workspace for the tools, with the PDK registered.
fn tool_workspace(runner: &Runner, project: Project) -> Arc<Workspace> {
    let ws = Workspace::new();
    ws.runner.set_config(runner.config());
    ws.set_project(project);
    Arc::new(ws)
}

/// The inverter's switching threshold at every process corner, through
/// check_specs: a slow NMOS with a fast PMOS (sf) moves it up, the
/// opposite skew (fs) moves it down, and ss and ff differ from tt, so each
/// corner really selected its own section of IHP's corner library.
#[tokio::test]
async fn inverter_threshold_moves_with_the_process_corner() {
    let Some(pdk) = ihp() else { return };
    let (_dir, runner, project) = setup(&pdk, &[("inv.asc", INVERTER)]);
    let ws = tool_workspace(&runner, project);
    let tools = aispice_tools::registry(ws.clone());
    let check = tools.get("check_specs").unwrap();
    let mut sims = vec!["ngspice"];
    if xyce_ready(&runner) {
        sims.push("xyce");
    }
    let mut by_sim = Vec::new();
    for sim in sims {
        let out = check
            .call(
                &ToolContext::default(),
                json!({"circuit": "inv.asc", "simulator": sim, "save": false,
                       "specs": "vm = crossing(V(out), 0.6) in 0.4..0.8",
                       "corners": {"process": ["tt", "ss", "ff", "sf", "fs"]}}),
            )
            .await;
        let text = out.text_content();
        assert!(!out.is_error, "{sim}: {text}");
        let row = &out.data.as_ref().unwrap()["corners"]["rows"][0];
        let vm: Vec<f64> = row["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap_or_else(|| panic!("{sim}: {text}")))
            .collect();
        eprintln!(
            "{sim}: threshold tt {:.4} ss {:.4} ff {:.4} sf {:.4} fs {:.4} V",
            vm[0], vm[1], vm[2], vm[3], vm[4]
        );
        let (tt, ss, ff, sf, fs) = (vm[0], vm[1], vm[2], vm[3], vm[4]);
        assert!(sf > tt + 0.005 && fs < tt - 0.005, "{sim}: {vm:?}");
        assert!(
            (ss - tt).abs() > 1e-3 && (ff - tt).abs() > 1e-3,
            "{sim}: {vm:?}"
        );
        by_sim.push(vm);
    }
    if let [ng, xy] = by_sim.as_slice() {
        for (a, b) in ng.iter().zip(xy) {
            assert!((a - b).abs() < 2e-3, "ngspice {ng:?} vs Xyce {xy:?}");
        }
    }
}

/// Mismatch Monte Carlo on IHP's `mos_tt_mismatch` and `mos_tt_stat`
/// sections: the threshold spreads, the same seed gives the same runs, and
/// global variation spreads it more than local mismatch alone.
#[tokio::test]
async fn inverter_threshold_spreads_with_mismatch() {
    let Some(pdk) = ihp() else { return };
    let (_dir, runner, project) = setup(&pdk, &[("inv.asc", INVERTER)]);
    let ws = tool_workspace(&runner, project);
    let tools = aispice_tools::registry(ws.clone());
    let mc = tools.get("monte_carlo").unwrap();
    let spread = |sim: &'static str, mode: &'static str, seed: u64| {
        let mc = &mc;
        async move {
            let out = mc
                .call(
                    &ToolContext::default(),
                    json!({"circuit": "inv.asc", "simulator": sim, "mismatch": mode,
                           "runs": 40, "seed": seed, "specs": "vm = crossing(V(out), 0.6) in 0.4..0.8"}),
                )
                .await;
            let text = out.text_content();
            assert!(!out.is_error, "{sim} {mode}: {text}");
            let s = &out.data.as_ref().unwrap()["specs"][0];
            let get = |k: &str| {
                s[k].as_f64()
                    .unwrap_or_else(|| panic!("{sim} {mode}: no {k}\n{text}"))
            };
            (get("mean"), get("std"))
        }
    };
    let (mean, local) = spread("ngspice", "local", 1).await;
    let again = spread("ngspice", "local", 1).await;
    let (_, global) = spread("ngspice", "global", 1).await;
    eprintln!(
        "ngspice: threshold mean {mean:.4} V, std local {:.2} mV, global {:.2} mV",
        local * 1e3,
        global * 1e3
    );
    assert_eq!(again, (mean, local));
    assert!(local > 0.2e-3, "no mismatch spread: {local}");
    assert!(global > local, "global {global} vs local {local}");
    if xyce_ready(&runner) {
        // IHP's Xyce models have `_stat` sections but no `_mismatch` ones,
        // and the sections are checked in the library Xyce reads. Xyce
        // draws from its own generator, so only the size of the spread
        // compares.
        let (xmean, xglobal) = spread("xyce", "global", 1).await;
        eprintln!(
            "Xyce: threshold mean {xmean:.4} V, std global {:.2} mV",
            xglobal * 1e3
        );
        assert!((xmean - mean).abs() < 10e-3, "{xmean} vs {mean}");
        assert!(
            xglobal > 0.5 * global && xglobal < 2.0 * global,
            "{xglobal} vs {global}"
        );
        let out = mc
            .call(
                &ToolContext::default(),
                json!({"circuit": "inv.asc", "simulator": "xyce", "mismatch": "local",
                       "runs": 4, "specs": "vm = crossing(V(out), 0.6) in 0.4..0.8"}),
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.text_content().contains("_mismatch section"),
            "{}",
            out.text_content()
        );
    }
}
