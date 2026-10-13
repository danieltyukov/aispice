//! Backends against the real simulators. Each test skips when its simulator
//! is missing; LTspice tests also need `AISPICE_LTSPICE_TESTS=1`.

mod common;

use aispice_sim::backend::{SimError, SimId, Simulator, detect_all};
use aispice_sim::dialect::{Target, translate};
use aispice_sim::{AnalysisKind, Quantity};
use common::*;
use std::f64::consts::PI;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn rc_mag(f: f64) -> f64 {
    let fc = 1.0 / (2.0 * PI * 1e3 * 100e-9);
    1.0 / (1.0 + (f / fc).powi(2)).sqrt()
}

/// LTspice tests in this file take turns. All Wine processes in a prefix
/// share one session, and the no-display test deliberately starts one
/// without a display, which makes every process that joins that session in
/// the next moments fail the same way.
static WINE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Wait for the prefix's Wine session to end, so the next run starts a new
/// one. Bounded, in case a user's LTspice window keeps it alive.
async fn wait_for_wine_session_end() {
    let mut cmd = tokio::process::Command::new("wineserver");
    cmd.arg("-w").kill_on_drop(true);
    let _ = tokio::time::timeout(Duration::from_secs(15), cmd.status()).await;
}

#[tokio::test]
async fn detection_finds_installed_simulators() {
    let all = detect_all().await;
    assert_eq!(all.len(), 4);
    for (id, d) in &all {
        eprintln!(
            "{id}: found={} version={:?} path={:?} notes={:?}",
            d.found, d.version, d.path, d.notes
        );
    }
    let spectre = all.iter().find(|(id, _)| *id == SimId::Spectre).unwrap();
    assert!(
        !spectre.1.found,
        "Spectre is never found without configuration"
    );
    if aispice_sim::backend::which("ngspice").is_some() {
        let ng = &all.iter().find(|(id, _)| *id == SimId::Ngspice).unwrap().1;
        assert!(ng.found);
        assert!(ng.version.is_some(), "{ng:?}");
    }
}

#[tokio::test]
async fn ngspice_ac_run() {
    let Some(ng) = ngspice().await else { return };
    // Its own run folder, since the kept raw file is checked after the run.
    let dir = tempfile::tempdir().unwrap();
    let out = ng
        .run(
            &job(&deck("rc_ac"), dir.path(), "rc_ac"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let ds = out.dataset(AnalysisKind::Ac).unwrap();
    assert_eq!(ds.len(), 41);
    let f = ds.axis_vector().unwrap().data.real();
    let v = ds.vector("out").unwrap().data.as_complex();
    for (f, v) in f.iter().zip(&v) {
        assert!((v.abs() - rc_mag(*f)).abs() < 1e-9);
    }
    // Only the deck and the raw file are kept, in the run folder.
    let names: Vec<_> = out
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["rc_ac.cir", "rc_ac.raw"]);
    assert!(out.raw_path.unwrap().exists());
    assert!(
        out.log.contains("Compatibility modes selected"),
        "{}",
        out.log
    );
}

#[tokio::test]
async fn ngspice_measurements() {
    let Some(ng) = ngspice().await else { return };
    let out = run(&ng, &deck("ng_meas"), "ng_meas").await.unwrap();
    let get = |n: &str| {
        out.measurements
            .iter()
            .find(|m| m.name == n)
            .unwrap_or_else(|| panic!("{n} in {:?}", out.measurements))
    };
    assert!((get("vend").value.unwrap() - (1.0 - (-2f64).exp())).abs() < 1e-3);
    assert!(get("never").failed());
}

#[tokio::test]
async fn ngspice_errors_fail_the_run() {
    let Some(ng) = ngspice().await else { return };
    match run(&ng, &deck("bad_subckt"), "bad").await {
        Err(SimError::Failed { errors, .. }) => {
            assert!(
                errors.iter().any(|e| e.contains("unknown subckt")),
                "{errors:?}"
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// A `.spiceinit` in the project or the run folder must never execute: the
/// run happens in a private directory with HOME pointing at it.
#[tokio::test]
async fn ngspice_ignores_foreign_spiceinit() {
    let Some(ng) = ngspice().await else { return };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".spiceinit"),
        "echo AISPICE-HOSTILE-SPICEINIT\n",
    )
    .unwrap();
    let out = ng
        .run(
            &job(&deck("div_op"), dir.path(), "div"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        !out.log.contains("AISPICE-HOSTILE-SPICEINIT"),
        "{}",
        out.log
    );
    let ds = out.dataset(AnalysisKind::Op).unwrap();
    assert!((ds.vector("out").unwrap().data.real()[0] - 7.5).abs() < 1e-12);
}

#[tokio::test]
async fn ngspice_timeout_and_cancel() {
    let Some(ng) = ngspice().await else { return };
    let slow = "slow\nV1 in 0 SIN(0 1 1Meg)\nR1 in out 1k\nC1 out 0 1n\n.tran 1n 10 0 1n\n.end\n";
    let dir = tempfile::tempdir().unwrap();
    let mut j = job(slow, dir.path(), "slow");
    j.timeout = Duration::from_millis(300);
    let r = ng.run(&j, &CancellationToken::new()).await;
    assert!(matches!(r, Err(SimError::Timeout(_))), "{r:?}");

    j.timeout = Duration::from_secs(60);
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        t2.cancel();
    });
    let r = ng.run(&j, &token).await;
    assert!(matches!(r, Err(SimError::Cancelled)), "{r:?}");
}

#[tokio::test]
async fn xyce_dc_run_names_the_swept_source() {
    let Some(x) = xyce().await else { return };
    let out = run(&x, &deck("div_dc"), "div_dc").await.unwrap();
    let ds = out.dataset(AnalysisKind::Dc).unwrap();
    let axis = ds.axis_vector().unwrap();
    assert_eq!(axis.name, "V1");
    assert_eq!(axis.quantity, Quantity::Sweep);
    let out_v = ds.vector("out").unwrap().data.real();
    assert!((out_v[10] - 7.5).abs() < 1e-9);
}

#[tokio::test]
async fn xyce_measurements_and_errors() {
    let Some(x) = xyce().await else { return };
    let out = run(&x, &deck("xyce_meas"), "xm").await.unwrap();
    let names: Vec<_> = out.measurements.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["VMAX", "VEND", "TRISE", "NEVER"]);
    assert!(
        out.files
            .iter()
            .any(|f| f.to_string_lossy().ends_with(".mt0"))
    );
    match run(&x, &deck("bad_subckt"), "bad").await {
        Err(SimError::Failed { errors, .. }) => {
            assert!(errors[0].contains("NOSUCHSUB"), "{errors:?}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn xyce_step_runs_merge() {
    let Some(x) = xyce().await else { return };
    let tr = translate(
        &aispice_core::netlist::parse(&deck("div_step_op")),
        Target::Xyce,
    );
    assert!(tr.unsupported.is_empty(), "{:?}", tr.unsupported);
    let out = run(&x, &tr.text, "step").await.unwrap();
    let ds = &out.datasets[0];
    assert_eq!(ds.steps.len(), 2, "{:?}", ds.steps);
    assert_eq!(ds.kind, AnalysisKind::Op);
    let labels: Vec<_> = ds.steps.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, vec!["R=1k", "R=3k"]);
    let v = ds.vector("out").unwrap().data.real();
    assert!(
        (v[0] - 7.5).abs() < 1e-9 && (v[1] - 5.0).abs() < 1e-9,
        "{v:?}"
    );
}

#[tokio::test]
async fn ltspice_runs_and_labels_steps() {
    let Some(lt) = ltspice().await else { return };
    let _wine = WINE.lock().await;
    let tr = translate(
        &aispice_core::netlist::parse(&deck("rc_step")),
        Target::Ltspice,
    );
    let (ac_deck, bad_deck) = (deck("rc_ac"), deck("bad_subckt"));
    let (step, ac, bad) = tokio::join!(
        run(&lt, &tr.text, "rc_step"),
        run(&lt, &ac_deck, "rc_ac"),
        run(&lt, &bad_deck, "bad"),
    );
    let step = step.unwrap();
    let ds = step.dataset(AnalysisKind::Transient).unwrap();
    let labels: Vec<_> = ds.steps.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels, vec!["r=1000", "r=2000", "r=4000"]);
    // plotwinsize=0 keeps every point.
    assert!(ds.len() > 150, "{} points", ds.len());
    assert_eq!(
        step.measurements
            .iter()
            .filter(|m| m.name == "vend")
            .count(),
        3
    );
    assert!(
        step.files
            .iter()
            .any(|f| f.to_string_lossy().ends_with("rc_step.log"))
    );

    let ac = ac.unwrap();
    let d = ac.dataset(AnalysisKind::Ac).unwrap();
    let f = d.axis_vector().unwrap().data.real();
    let v = d.vector("out").unwrap().data.as_complex();
    for (f, v) in f.iter().zip(&v) {
        assert!((v.abs() - rc_mag(*f)).abs() < 1e-6);
    }

    match bad {
        Err(SimError::Failed { errors, .. }) => {
            assert!(errors[0].contains("Unknown subcircuit"), "{errors:?}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// Without a display LTspice under Wine exits 0 having written nothing; the
/// backend must say so instead of returning an empty result.
#[tokio::test]
async fn ltspice_without_display_is_reported() {
    let Some(lt) = ltspice().await else { return };
    let _wine = WINE.lock().await;
    let lt = aispice_sim::backend::Ltspice {
        headless: Some(false),
        env: vec![
            ("DISPLAY".into(), ":987".into()),
            ("WAYLAND_DISPLAY".into(), String::new()),
        ],
        ..lt
    };
    let result = run(&lt, &deck("div_op"), "nodisplay").await;
    wait_for_wine_session_end().await;
    match result {
        Err(SimError::NoOutput(msg)) => assert!(msg.contains("display"), "{msg}"),
        other => panic!("expected NoOutput, got {other:?}"),
    }
}

/// A timeout must stop the Windows process under Wine too, not just the
/// `wine` launcher.
#[tokio::test]
async fn ltspice_timeout_kills_the_process_tree() {
    let Some(lt) = ltspice().await else { return };
    let _wine = WINE.lock().await;
    let slow = "slow\nV1 in 0 SINE(0 1 1Meg)\nR1 in out 1k\nC1 out 0 1n\n.tran 0 100 0 1n\n.end\n";
    let dir = tempfile::tempdir().unwrap();
    let mut j = job(slow, dir.path(), "aispice_timeout_probe");
    j.timeout = Duration::from_secs(6);
    let r = lt.run(&j, &CancellationToken::new()).await;
    assert!(matches!(r, Err(SimError::Timeout(_))), "{r:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let ps = std::process::Command::new("pgrep")
        .args(["-f", "aispice_timeout_probe.net"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
        "LTspice still running: {}",
        String::from_utf8_lossy(&ps.stdout)
    );
}

#[tokio::test]
async fn ltspice_netlists_a_schematic() {
    let Some(lt) = ltspice().await else { return };
    let _wine = WINE.lock().await;
    let net = lt.netlist_asc(&schematic("divider")).await.unwrap();
    assert!(net.starts_with("* "));
    assert!(net.contains("R\u{a7}top N001 out 1k"), "{net}");
    assert!(net.contains(".op"));
}

#[tokio::test]
async fn simulators_are_cheap_to_box() {
    let sims: Vec<Box<dyn Simulator>> = SimId::ALL
        .iter()
        .map(|&id| aispice_sim::backend::simulator(id))
        .collect();
    let ids: Vec<_> = sims.iter().map(|s| s.id()).collect();
    assert_eq!(ids, SimId::ALL.to_vec());
}
