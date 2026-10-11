//! DRC with IHP's decks on synthetic layouts. Needs KLayout and
//! `AISPICE_PDK_IHP`; passes with a note when either is missing.

use aispice_layout::drc::{self, DrcChecks};
use aispice_layout::gds::testwrite::{TestCell, library};
use aispice_layout::tools::{PdkLayoutFiles, Programs};
use aispice_sim::pdk::Pdk;
use tokio_util::sync::CancellationToken;

fn setup() -> Option<(Programs, PdkLayoutFiles)> {
    let Some(path) = std::env::var_os("AISPICE_PDK_IHP") else {
        eprintln!("skipped: AISPICE_PDK_IHP is not set");
        return None;
    };
    let programs = Programs::default();
    if programs.klayout().is_err() {
        eprintln!("skipped: KLayout is not installed");
        return None;
    }
    let pdk = Pdk::open("ihp-sg13g2", std::path::Path::new(&path)).unwrap();
    Some((programs, PdkLayoutFiles::of(&pdk).unwrap()))
}

/// Two Metal1 squares 0.1 um apart: IHP's rule M1.b wants 0.18 um.
#[tokio::test]
async fn a_metal1_spacing_violation_is_reported_with_rule_and_place() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let gds = dir.path().join("spacing.gds");
    let cell = TestCell {
        name: "spacing".into(),
        rects: vec![(8, 0, 0, 0, 1000, 1000), (8, 0, 1100, 0, 2100, 1000)],
        ..Default::default()
    };
    std::fs::write(&gds, library(&[cell])).unwrap();
    let checks = DrcChecks {
        extra: false,
        antenna: false,
        density: false,
    };
    let started = std::time::Instant::now();
    let report = drc::run(
        &programs,
        &files,
        &gds,
        "spacing",
        checks,
        &dir.path().join("drc"),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    eprintln!(
        "main deck: {:.1} s\n{}",
        started.elapsed().as_secs_f64(),
        report.to_text()
    );
    let rule = report
        .rules
        .iter()
        .find(|r| r.rule == "M1.b")
        .expect("M1.b reported");
    assert_eq!(rule.count, 1);
    assert_eq!(rule.layer.as_deref(), Some("Metal1"));
    let v = report.violations.iter().find(|v| v.rule == "M1.b").unwrap();
    // The marker sits in the gap between the squares.
    assert!(v.bbox[0] >= 0.99 && v.bbox[2] <= 1.11, "{:?}", v.bbox);
    assert!(v.bbox[1] >= -0.01 && v.bbox[3] <= 1.01, "{:?}", v.bbox);
    assert!(report.to_text().contains("M1.b"));

    // Pulled apart to 0.2 um, the rule passes.
    let ok = TestCell {
        name: "spacing".into(),
        rects: vec![(8, 0, 0, 0, 1000, 1000), (8, 0, 1200, 0, 2200, 1000)],
        ..Default::default()
    };
    std::fs::write(&gds, library(&[ok])).unwrap();
    let report = drc::run(
        &programs,
        &files,
        &gds,
        "spacing",
        checks,
        &dir.path().join("drc2"),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        !report.rules.iter().any(|r| r.rule == "M1.b"),
        "{}",
        report.to_text()
    );
}
