//! Generated blocks drawn with IHP's pcells, then checked with IHP's DRC and
//! LVS decks. Needs KLayout, `AISPICE_PDK_IHP` and the PDK's pcell
//! submodules; passes with a note when any is missing.

use aispice_core::netlist::{Netlist, parse};
use aispice_layout::drc::{self, DrcChecks, DrcReport};
use aispice_layout::extract;
use aispice_layout::generator::{self, Generated, Params, Roles, Template};
use aispice_layout::lvs::{self, LvsReport};
use aispice_layout::render;
use aispice_layout::tools::{PdkLayoutFiles, Programs};
use aispice_sim::pdk::Pdk;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub const OTA: &str = "* 5T OTA\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM2 out inm tail 0 sg13_lv_nmos w=2u l=0.5u ng=1 m=1\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM4 out x vdd vdd sg13_lv_pmos w=4u l=0.5u ng=1 m=1\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\nXM6 vbn vbn 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\n.ac dec 20 1 1G\n";

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
    let pdk = Pdk::open("ihp-sg13g2", Path::new(&path)).unwrap();
    let files = PdkLayoutFiles::of(&pdk).unwrap();
    if let Err(e) = files.check_pcells() {
        eprintln!("skipped: {e}");
        return None;
    }
    Some((programs, files))
}

struct Checked {
    generated: Generated,
    drc: DrcReport,
    lvs: LvsReport,
}

async fn generate_and_check(
    programs: &Programs,
    files: &PdkLayoutFiles,
    netlist: &Netlist,
    template: Template,
    params: &Params,
    dir: &Path,
    cell: &str,
) -> Checked {
    let generated = generator::generate(netlist, template, &Roles::new(), params, cell).unwrap();
    let gds = dir.join(format!("{cell}.gds"));
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    generator::draw(programs, files, &generated.plan, &gds, &cancel)
        .await
        .unwrap();
    eprintln!(
        "{}drawn in {:.1} s",
        generated.summary(),
        started.elapsed().as_secs_f64()
    );
    let started = std::time::Instant::now();
    let drc = drc::run(
        programs,
        files,
        &gds,
        cell,
        DrcChecks::default(),
        &dir.join(format!("{cell}-drc")),
        &cancel,
    )
    .await
    .unwrap();
    eprintln!(
        "DRC in {:.1} s: {}",
        started.elapsed().as_secs_f64(),
        drc.to_text()
    );
    let schematic = lvs::lvs_netlist(
        netlist,
        cell,
        Some(&generated.meta.devices),
        &generated.meta.dummies,
    )
    .unwrap();
    let started = std::time::Instant::now();
    let lvs = lvs::run(
        programs,
        files,
        &gds,
        &schematic,
        &dir.join(format!("{cell}-lvs")),
        &cancel,
    )
    .await
    .unwrap();
    eprintln!(
        "LVS in {:.1} s: {}",
        started.elapsed().as_secs_f64(),
        lvs.to_text()
    );
    Checked {
        generated,
        drc,
        lvs,
    }
}

fn keep(dir: tempfile::TempDir) {
    if std::env::var_os("AISPICE_KEEP").is_some() {
        eprintln!("kept {}", dir.keep().display());
    }
}

#[tokio::test]
async fn a_generated_current_mirror_is_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(
        "* mirror\nIREF vdd ref 10u\nVDD vdd 0 1.2\nVO out 0 0.6\nXM1 ref ref 0 0 sg13_lv_nmos w=2u l=1u ng=1 m=1\nXM2 out ref 0 0 sg13_lv_nmos w=4u l=1u ng=1 m=1\n.op\n",
    );
    let c = generate_and_check(
        &programs,
        &files,
        &n,
        Template::CurrentMirror,
        &Params::default(),
        dir.path(),
        "mirror",
    )
    .await;
    assert!(c.drc.clean(), "{}", c.drc.to_text());
    assert!(c.lvs.matched, "{}", c.lvs.to_text());
    assert_eq!(c.generated.meta.ports, ["0", "out", "ref"]);
    let png = render::png(
        &programs,
        &files,
        &dir.path().join("mirror.gds"),
        "mirror",
        (800, 600),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(png.starts_with(b"\x89PNG"), "not a PNG");
    keep(dir);
}

#[tokio::test]
async fn a_generated_diff_pair_is_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(OTA);
    let params = Params {
        fingers: Some(4),
        pattern: generator::Pattern::Interdigitated,
        ..Default::default()
    };
    let c = generate_and_check(
        &programs,
        &files,
        &n,
        Template::DiffPair,
        &params,
        dir.path(),
        "pair",
    )
    .await;
    assert!(c.drc.clean(), "{}", c.drc.to_text());
    assert!(c.lvs.matched, "{}", c.lvs.to_text());
    keep(dir);
}

#[tokio::test]
async fn a_generated_ota_is_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(OTA);
    let c = generate_and_check(
        &programs,
        &files,
        &n,
        Template::Ota5t,
        &Params::default(),
        dir.path(),
        "ota5t",
    )
    .await;
    assert!(c.drc.clean(), "{}", c.drc.to_text());
    assert!(c.lvs.matched, "{}", c.lvs.to_text());
    assert_eq!(
        c.generated.meta.ports,
        ["0", "inm", "inp", "out", "vbn", "vdd"]
    );
    if programs.magic().is_err() {
        eprintln!("extraction skipped: Magic is not installed");
        keep(dir);
        return;
    }
    let gds = dir.path().join("ota5t.gds");
    let cancel = CancellationToken::new();
    for resistance in [false, true] {
        let started = std::time::Instant::now();
        let ex = extract::run(
            &programs,
            &files,
            &gds,
            "ota5t",
            resistance,
            &dir.path().join("ext"),
            &cancel,
        )
        .await
        .unwrap();
        eprintln!(
            "extracted in {:.1} s: {}",
            started.elapsed().as_secs_f64(),
            ex.to_text()
        );
        assert_eq!(ex.ports, ["0", "inm", "inp", "out", "vbn", "vdd"]);
        // 14 fingers and 6 dummy fingers.
        assert_eq!(ex.devices, 20);
        assert!(ex.capacitors > 5);
        assert_eq!(resistance, ex.resistors > 0, "{}", ex.text);
        let post = extract::post_layout(&n, &ex, &c.generated.meta.devices).unwrap();
        let text = aispice_core::netlist::write(&post);
        assert!(
            text.contains("Xota5t_layout 0 inm inp out vbn vdd ota5t"),
            "{text}"
        );
        assert!(!text.contains("XM1 "), "{text}");
    }
    keep(dir);
}

/// The checks catch what a generator could get wrong: a contact left out
/// fails LVS on its net, a Metal1 sliver too close to a pad fails DRC with
/// the rule and where it is.
#[tokio::test]
async fn a_missing_contact_fails_lvs_and_a_sliver_fails_drc() {
    use aispice_layout::generator::plan::{Layer, Rect};
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(OTA);
    let cancel = CancellationToken::new();
    let generated = generator::generate(
        &n,
        Template::Ota5t,
        &Roles::new(),
        &Params::default(),
        "ota5t",
    )
    .unwrap();

    // One gate contact of M2 (net inm) left out.
    let mut plan = generated.plan.clone();
    let at = plan
        .shapes
        .iter()
        .position(|s| s.layer == Layer::Cont && s.net.as_deref() == Some("inm"))
        .unwrap();
    plan.shapes.remove(at);
    let gds = dir.path().join("open.gds");
    generator::draw(&programs, &files, &plan, &gds, &cancel)
        .await
        .unwrap();
    let schematic = lvs::lvs_netlist(
        &n,
        "ota5t",
        Some(&generated.meta.devices),
        &generated.meta.dummies,
    )
    .unwrap();
    let report = lvs::run(
        &programs,
        &files,
        &gds,
        &schematic,
        &dir.path().join("lvs"),
        &cancel,
    )
    .await
    .unwrap();
    eprintln!("{}", report.to_text());
    assert!(!report.matched);
    assert!(
        report
            .bad_nets()
            .iter()
            .any(|n| n.eq_ignore_ascii_case("inm")),
        "{}",
        report.to_text()
    );

    // A Metal1 sliver 0.1 um to the right of an inp gate pad.
    let mut plan = generated.plan.clone();
    let pad = plan
        .shapes
        .iter()
        .find(|s| s.layer == Layer::Metal1 && s.net.as_deref() == Some("inp"))
        .unwrap()
        .rect;
    let sliver = Rect::new(pad.x2 + 100, pad.y1, pad.x2 + 260, pad.y2);
    plan.add(Layer::Metal1, sliver, None);
    let gds = dir.path().join("sliver.gds");
    generator::draw(&programs, &files, &plan, &gds, &cancel)
        .await
        .unwrap();
    let checks = DrcChecks {
        extra: false,
        antenna: false,
        density: false,
    };
    let report = drc::run(
        &programs,
        &files,
        &gds,
        "ota5t",
        checks,
        &dir.path().join("drc"),
        &cancel,
    )
    .await
    .unwrap();
    eprintln!("{}", report.to_text());
    let m1b: Vec<_> = report
        .violations
        .iter()
        .filter(|v| v.rule == "M1.b")
        .collect();
    assert!(!m1b.is_empty(), "{}", report.to_text());
    let [x1, _, x2, _] = sliver.um();
    let near = m1b
        .iter()
        .any(|v| v.bbox[0] >= x1 - 0.2 && v.bbox[2] <= x2 && v.bbox[2] >= x1 - 0.1);
    assert!(
        near,
        "no M1.b marker at the sliver {:?}: {}",
        sliver.um(),
        report.to_text()
    );
    keep(dir);
}

const STRONGARM: &str = "* StrongARM comparator\nVDD vdd 0 1.2\nVCLK clk 0 PULSE(0 1.2 1n 50p 50p 2n 4n)\nVP inp 0 0.61\nVM inm 0 0.59\nXM0 s clk 0 0 sg13_lv_nmos w=4u l=0.13u\nXM1 xp inp s 0 sg13_lv_nmos w=4u l=0.13u\nXM2 xn inm s 0 sg13_lv_nmos w=4u l=0.13u\nXM3 outn outp xp 0 sg13_lv_nmos w=2u l=0.13u\nXM4 outp outn xn 0 sg13_lv_nmos w=2u l=0.13u\nXM5 outn outp vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM6 outp outn vdd vdd sg13_lv_pmos w=2u l=0.13u\nXM7 outn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM8 outp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM9 xp clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nXM10 xn clk vdd vdd sg13_lv_pmos w=1u l=0.13u\nCP outp 0 5f\nCN outn 0 5f\n.tran 10p 8n\n";

#[tokio::test]
async fn a_generated_comparator_is_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(STRONGARM);
    let c = generate_and_check(
        &programs,
        &files,
        &n,
        Template::Comparator,
        &Params::default(),
        dir.path(),
        "comparator",
    )
    .await;
    assert!(c.drc.clean(), "{}", c.drc.to_text());
    assert!(c.lvs.matched, "{}", c.lvs.to_text());
    assert_eq!(
        c.generated.meta.ports,
        ["0", "clk", "inm", "inp", "outn", "outp", "vdd"]
    );
    keep(dir);
}

const TWO_STAGE: &str = "* two-stage Miller op-amp\nVDD vdd 0 1.2\nVP inp 0 0.6 AC 0.5\nVM inm 0 0.6 AC -0.5\nIB vdd vbn 20u\nCL out 0 1p\nXM1 x inp tail 0 sg13_lv_nmos w=2u l=0.5u\nXM2 out1 inm tail 0 sg13_lv_nmos w=2u l=0.5u\nXM3 x x vdd vdd sg13_lv_pmos w=4u l=0.5u\nXM4 out1 x vdd vdd sg13_lv_pmos w=4u l=0.5u\nXM5 tail vbn 0 0 sg13_lv_nmos w=4u l=1u\nXM8 vbn vbn 0 0 sg13_lv_nmos w=4u l=1u\nXM6 out out1 vdd vdd sg13_lv_pmos w=16u l=0.5u\nXM7 out vbn 0 0 sg13_lv_nmos w=8u l=1u\nXC1 out1 out cap_cmim w=10u l=10u m=1\n.ac dec 20 1 1G\n";

#[tokio::test]
async fn a_generated_two_stage_op_amp_is_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let n = parse(TWO_STAGE);
    let c = generate_and_check(
        &programs,
        &files,
        &n,
        Template::TwoStage,
        &Params::default(),
        dir.path(),
        "two_stage",
    )
    .await;
    assert!(c.drc.clean(), "{}", c.drc.to_text());
    assert!(c.lvs.matched, "{}", c.lvs.to_text());
    assert_eq!(
        c.generated.meta.ports,
        ["0", "inm", "inp", "out", "vbn", "vdd"]
    );
    if programs.magic().is_ok() {
        let ex = extract::run(
            &programs,
            &files,
            &dir.path().join("two_stage.gds"),
            "two_stage",
            false,
            &dir.path().join("ext"),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        eprintln!("{}", ex.to_text());
        assert!(ex.text.contains("cap_cmim"), "{}", ex.text);
        extract::post_layout(&n, &ex, &c.generated.meta.devices).unwrap();
    }
    keep(dir);
}

/// Blocks with their pins brought to the bottom edge, as a macro package
/// has them, stay DRC and LVS clean: the pins are on Metal4 now.
#[tokio::test]
async fn edge_pinned_blocks_are_drc_and_lvs_clean() {
    let Some((programs, files)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let params = Params {
        pin_edge: true,
        ..Default::default()
    };
    for (text, template, cell) in [
        (OTA, Template::Ota5t, "ota_pins"),
        (STRONGARM, Template::Comparator, "comparator_pins"),
        (TWO_STAGE, Template::TwoStage, "two_stage_pins"),
    ] {
        let n = parse(text);
        let c =
            generate_and_check(&programs, &files, &n, template, &params, dir.path(), cell).await;
        assert!(c.drc.clean(), "{cell}: {}", c.drc.to_text());
        assert!(c.lvs.matched, "{cell}: {}", c.lvs.to_text());
    }
    keep(dir);
}
