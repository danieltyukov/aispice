//! Generated blocks drawn with IHP's pcells, then checked with IHP's DRC and
//! LVS decks. Needs KLayout, `AISPICE_PDK_IHP` and the PDK's pcell
//! submodules; passes with a note when any is missing.

use aispice_core::netlist::{Netlist, parse};
use aispice_layout::drc::{self, DrcChecks, DrcReport};
use aispice_layout::generator::{self, Generated, Params, Roles, Template};
use aispice_layout::lvs::{self, LvsReport};
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
    keep(dir);
}
